//! File-based tracing sink with daily + size rotation.
//!
//! Naming: `rencoder.YYYY-MM-DD.NN.log` where NN increments when the
//! current file exceeds `max_size`. At midnight the date rolls over
//! and NN resets to 0.
//!
//! Retention: files older than `max_age_days` are deleted at startup.
//!
//! Thread-safety: `FileSink` holds `Mutex<Inner>`. Each fmt event
//! produces a `FileSinkHandle` that holds `&Mutex<Inner>`, so writes
//! serialize on the mutex without needing `&mut FileSink`.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use chrono::{Duration, Local, NaiveDate};
use tracing_subscriber::fmt::MakeWriter;

pub struct FileSink {
    inner: Mutex<Inner>,
}

struct Inner {
    log_dir: PathBuf,
    current_date: NaiveDate,
    current_suffix: u32,
    current_size: u64,
    writer: File,
    max_size: u64,
}

impl FileSink {
    /// Create a sink that writes to `log_dir`, rotating when the current
    /// file exceeds `max_size` bytes or the date rolls over. Files older
    /// than `max_age_days` are deleted on construction (0 = no cap).
    pub fn new<P: Into<PathBuf>>(
        log_dir: P,
        max_size: u64,
        max_age_days: u32,
    ) -> io::Result<Self> {
        let log_dir = log_dir.into();
        fs::create_dir_all(&log_dir)?;
        Self::cleanup_old(&log_dir, max_age_days)?;
        let inner = Inner::open(&log_dir, max_size)?;
        Ok(Self {
            inner: Mutex::new(inner),
        })
    }

    fn cleanup_old(log_dir: &Path, max_age_days: u32) -> io::Result<()> {
        if max_age_days == 0 {
            return Ok(());
        }
        let cutoff = Local::now().date_naive() - Duration::days(max_age_days as i64);
        for entry in fs::read_dir(log_dir)? {
            let entry = entry?;
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if let Some(date) = parse_date_from_filename(&name) {
                if date < cutoff {
                    let _ = fs::remove_file(entry.path());
                }
            }
        }
        Ok(())
    }
}

/// Per-event handle into the shared `FileSink`. Holds a reference to
/// the inner Mutex so writes don't need `&mut FileSink`.
pub struct FileSinkHandle<'a> {
    inner: &'a Mutex<Inner>,
}

impl Write for FileSinkHandle<'_> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let mut inner = self.inner.lock().unwrap();
        inner.rotate_for_date()?;
        let n = inner.writer.write(buf)?;
        inner.current_size += n as u64;
        inner.rotate_for_size()?;
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.lock().unwrap().writer.flush()
    }
}

impl<'a> MakeWriter<'a> for FileSink {
    type Writer = FileSinkHandle<'a>;
    fn make_writer(&'a self) -> Self::Writer {
        FileSinkHandle {
            inner: &self.inner,
        }
    }
}

impl Inner {
    fn open(log_dir: &Path, max_size: u64) -> io::Result<Self> {
        let today = Local::now().date_naive();
        let writer = Self::open_writer(log_dir, today, 0)?;
        Ok(Self {
            log_dir: log_dir.to_path_buf(),
            current_date: today,
            current_suffix: 0,
            current_size: 0,
            writer,
            max_size,
        })
    }

    fn open_writer(log_dir: &Path, date: NaiveDate, suffix: u32) -> io::Result<File> {
        let filename = format!(
            "rencoder.{}.{:02}.log",
            date.format("%Y-%m-%d"),
            suffix
        );
        let path = log_dir.join(filename);
        OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
    }

    /// Rotate when the date has changed (midnight). Called at the start
    /// of each write.
    fn rotate_for_date(&mut self) -> io::Result<()> {
        let today = Local::now().date_naive();
        if today != self.current_date {
            self.current_date = today;
            self.current_suffix = 0;
            self.current_size = 0;
            self.writer = Self::open_writer(&self.log_dir, today, 0)?;
        }
        Ok(())
    }

    /// Rotate when the current file size has reached `max_size`. Called
    /// after each write so the rotation happens on the write that pushes
    /// us over, not the next one.
    fn rotate_for_size(&mut self) -> io::Result<()> {
        if self.current_size >= self.max_size {
            self.current_suffix += 1;
            self.current_size = 0;
            self.writer = Self::open_writer(&self.log_dir, self.current_date, self.current_suffix)?;
        }
        Ok(())
    }
}

fn parse_date_from_filename(name: &str) -> Option<NaiveDate> {
    let rest = name.strip_prefix("rencoder.")?.strip_suffix(".log")?;
    let (date_part, _suffix) = rest.split_once('.')?;
    NaiveDate::parse_from_str(date_part, "%Y-%m-%d").ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static COUNTER: AtomicUsize = AtomicUsize::new(0);

    fn fixture_dir() -> PathBuf {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!(
            "rencoder_file_sink_{}_{}",
            std::process::id(),
            n
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn list_log_files(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.starts_with("rencoder."))
            .collect();
        names.sort();
        names
    }

    #[test]
    fn creates_log_dir_and_initial_file() {
        let dir = fixture_dir();
        let sink = FileSink::new(&dir, 1024, 7).unwrap();

        let mut w = sink.make_writer();
        writeln!(w, "hello").unwrap();

        let files = list_log_files(&dir);
        assert_eq!(files.len(), 1);
        assert!(files[0].starts_with("rencoder."));
        assert!(files[0].ends_with(".00.log"));
    }

    #[test]
    fn rotates_when_max_size_exceeded() {
        let dir = fixture_dir();
        let sink = FileSink::new(&dir, 100, 7).unwrap();

        {
            let mut w = sink.make_writer();
            w.write_all(&[b'a'; 50]).unwrap();
        }

        {
            let mut w = sink.make_writer();
            w.write_all(&[b'b'; 60]).unwrap();
        }

        let files = list_log_files(&dir);
        assert_eq!(files.len(), 2, "expected rotation, got {files:?}");
        assert!(files[0].ends_with(".00.log"));
        assert!(files[1].ends_with(".01.log"));
    }

    #[test]
    fn filename_format_matches_date_and_suffix() {
        let dir = fixture_dir();
        let sink = FileSink::new(&dir, 1024, 7).unwrap();
        let mut w = sink.make_writer();
        writeln!(w, "x").unwrap();

        let file = list_log_files(&dir).into_iter().next().unwrap();
        let today = Local::now().format("%Y-%m-%d").to_string();
        assert!(
            file.contains(&today),
            "filename {file} should contain today's date {today}"
        );
        assert!(file.ends_with(".00.log"));
    }

    #[test]
    fn parse_date_from_filename_round_trip() {
        let today = Local::now().date_naive();
        let name = format!("rencoder.{}.00.log", today.format("%Y-%m-%d"));
        assert_eq!(parse_date_from_filename(&name), Some(today));
    }

    #[test]
    fn parse_date_rejects_unrelated_files() {
        assert!(parse_date_from_filename("other.2026-01-01.log").is_none());
        assert!(parse_date_from_filename("rencoder.log").is_none());
        assert!(parse_date_from_filename("rencoder.2026-13-01.00.log").is_none());
    }

    #[test]
    fn cleanup_old_drops_files_outside_retention_window() {
        let dir = fixture_dir();
        let today = Local::now().date_naive();
        let old = today - Duration::days(10);

        let old_name = format!("rencoder.{}.00.log", old.format("%Y-%m-%d"));
        let fresh_name = format!("rencoder.{}.00.log", today.format("%Y-%m-%d"));
        fs::write(dir.join(&old_name), b"stale").unwrap();
        fs::write(dir.join(&fresh_name), b"fresh").unwrap();

        let _sink = FileSink::new(&dir, 1024, 7).unwrap();

        let files = list_log_files(&dir);
        assert_eq!(files, vec![fresh_name.clone()]);
    }

    #[test]
    fn cleanup_zero_means_no_retention() {
        let dir = fixture_dir();
        let today = Local::now().date_naive();
        let old = today - Duration::days(30);
        let old_name = format!("rencoder.{}.00.log", old.format("%Y-%m-%d"));
        fs::write(dir.join(&old_name), b"old").unwrap();

        let _sink = FileSink::new(&dir, 1024, 0).unwrap();
        let files = list_log_files(&dir);
        assert_eq!(files.len(), 2, "max_age_days=0 should not delete anything");
    }

    #[test]
    fn append_preserves_existing_content() {
        let dir = fixture_dir();
        let sink = FileSink::new(&dir, 1024, 7).unwrap();

        {
            let mut w = sink.make_writer();
            writeln!(w, "first").unwrap();
        }

        {
            let mut w = sink.make_writer();
            writeln!(w, "second").unwrap();
        }

        let path = dir.join(list_log_files(&dir).into_iter().next().unwrap());
        let content = fs::read_to_string(&path).unwrap();
        assert!(content.contains("first"));
        assert!(content.contains("second"));
    }
}
