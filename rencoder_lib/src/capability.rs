//! Run capability check: before a run starts, find out whether this
//! machine can do the job.
//!
//! The check is a **set**, not a one-off: [`check_run_capabilities`]
//! runs every item in priority order (most fundamental first) and
//! produces a **report** -- what passed, what did not, and why.
//!
//! # The report is a file
//!
//! The report is written to disk (like the cancel request), so every
//! frontend reads the same one instead of computing its own. A future
//! Tauri UI in another process reads the file; it does not re-run the
//! check.
//!
//! # A failed check does not refuse the start
//!
//! The report is **information**, not a gate. A desktop app that
//! refuses to open because its app directory is not writable would be
//! hostile; the user is told "you cannot write here" and decides.
//!
//! # The check is re-run when the inputs change
//!
//! The GUI can switch its working directory. A different target
//! means different answers (is it writable? do the paths alias?), so
//! the check runs again on every switch.

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::error::EncodeError;

/// How one check came out.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckStatus {
    /// The capability is there.
    Ok,
    /// The capability is missing -- the run cannot do this part.
    Failed,
    /// Not there, but the run can work around it (NVENC: software
    /// encoding instead). Not a failure.
    Advisory,
}

/// One item of the check.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Check {
    /// What was checked.
    pub name: String,
    /// How it came out.
    pub status: CheckStatus,
    /// Why, when it did not pass.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

impl Check {
    fn ok(name: &str) -> Self {
        Self {
            name: name.to_string(),
            status: CheckStatus::Ok,
            detail: None,
        }
    }

    fn failed(name: &str, detail: impl Into<String>) -> Self {
        Self {
            name: name.to_string(),
            status: CheckStatus::Failed,
            detail: Some(detail.into()),
        }
    }

    fn advisory(name: &str, detail: impl Into<String>) -> Self {
        Self {
            name: name.to_string(),
            status: CheckStatus::Advisory,
            detail: Some(detail.into()),
        }
    }
}

/// The report: every check and how it came out.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Capabilities {
    /// The checks, in the order they ran.
    pub checks: Vec<Check>,
}

impl Capabilities {
    /// Did every check pass?
    ///
    /// Advisory items do not count: a machine without NVENC still
    /// encodes, in software.
    pub fn all_ok(&self) -> bool {
        self.checks.iter().all(|c| c.status != CheckStatus::Failed)
    }

    /// The checks that did not pass.
    pub fn failures(&self) -> impl Iterator<Item = &Check> {
        self.checks
            .iter()
            .filter(|c| c.status == CheckStatus::Failed)
    }

    /// The report as one line per check -- for a log line.
    pub fn lines(&self) -> Vec<String> {
        self.checks
            .iter()
            .map(|c| match (&c.status, &c.detail) {
                (CheckStatus::Ok, _) => format!("{}: ok", c.name),
                (status, Some(detail)) => format!("{}: {:?} ({detail})", c.name, status),
                (status, None) => format!("{}: {:?}", c.name, status),
            })
            .collect()
    }
}

/// Run every capability check, write the report, and return it.
///
/// The order is the priority order (design doc §3.10.7): the most
/// fundamental first, because a later check writes into what an earlier
/// one just validated.
///
/// - `app_dir` -- where the config, the records and the capability
///   record live;
/// - `target_dir` -- the directory the run writes into (the GUI's
///   working directory; `None` when the run has no target yet);
/// - `cancel_file` -- the cancel request's file (its directory has to
///   be writable too -- the request is how a run gets stopped);
/// - `report_path` -- where the report is written.
pub fn check_run_capabilities(
    app_dir: &Path,
    target_dir: Option<&Path>,
    cancel_file: &Path,
    report_path: &Path,
) -> Capabilities {
    let mut capabilities = Capabilities::default();

    // 1. Filesystem: the directories this run writes into.
    capabilities.checks.push(writable("app directory", app_dir));
    if let Some(target) = target_dir {
        capabilities
            .checks
            .push(writable("target directory", target));
    }
    if let Some(parent) = cancel_file.parent() {
        capabilities
            .checks
            .push(writable("cancel request directory", parent));
    }

    // 2. ffmpeg: without it there is no encoding.
    capabilities
        .checks
        .push(match crate::encoder::nvenc::run_ffmpeg(&["-encoders"]) {
            Ok(_) => Check::ok("ffmpeg"),
            Err(e) => Check::failed("ffmpeg", e.to_string()),
        });

    // 3. NVENC: advisory only -- a machine without it still encodes,
    //    in software.
    capabilities.checks.push(nvenc_capability(app_dir));

    // The report goes to a file: every frontend reads this one.
    if let Err(e) = write_report(&capabilities, report_path) {
        tracing::warn!(error = %e, path = %report_path.display(), "could not write the capability report");
    }
    capabilities
}

/// Can a file be created in this directory?
fn writable(name: &str, dir: &Path) -> Check {
    match crate::config::fs_perms::ensure_dir_writable(dir) {
        Ok(()) => Check::ok(name),
        Err(e) => Check::failed(name, e.to_string()),
    }
}

/// Is there a usable NVENC encoder according to the stored record?
fn nvenc_capability(app_dir: &Path) -> Check {
    let path = app_dir.join(crate::encoder::nvenc::CAPABILITY_FILE_NAME);
    match crate::encoder::nvenc::load(&path) {
        Some(record) => {
            let usable: Vec<&str> = record
                .encoders
                .iter()
                .filter(|e| e.usable)
                .map(|e| e.encoder.as_str())
                .collect();
            if usable.is_empty() {
                Check::advisory("nvenc", "no NVENC encoder is usable here")
            } else {
                Check::ok("nvenc")
            }
        }
        None => Check::advisory("nvenc", "no capability record yet"),
    }
}

/// Write the report where the frontends read it.
fn write_report(capabilities: &Capabilities, path: &Path) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }
    let json = serde_json::to_string_pretty(capabilities)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    std::fs::write(path, json)
}

/// The report's default location: next to the capability record.
pub fn default_report_path(app_dir: &Path) -> std::path::PathBuf {
    app_dir.join("capabilities.json")
}

/// The report the last run left behind (`None` when there is none).
pub fn read_report(path: &Path) -> Option<Capabilities> {
    let raw = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&raw).ok()
}

/// The report as a lib error -- for callers that do want to refuse.
impl From<&Capabilities> for EncodeError {
    fn from(capabilities: &Capabilities) -> Self {
        let detail = capabilities
            .failures()
            .map(|c| match &c.detail {
                Some(detail) => format!("{}: {detail}", c.name),
                None => c.name.to_string(),
            })
            .collect::<Vec<_>>()
            .join("; ");
        EncodeError::InvalidParam(format!("the run cannot proceed -- {detail}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn scratch(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "rencoder_capability_{label}_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or_default()
        ));
        std::fs::create_dir_all(&dir).expect("the temp dir must be created");
        dir
    }

    #[test]
    fn the_report_is_written_and_read_back() {
        let dir = scratch("report");
        let app = dir.join("app");
        let report = default_report_path(&app);
        let cancel = app.join("cancel");

        let capabilities = check_run_capabilities(&app, None, &cancel, &report);

        assert!(report.exists(), "the report must be a file");
        let read = read_report(&report).expect("the report must be readable");
        assert_eq!(read.checks.len(), capabilities.checks.len());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_unwritable_directory_is_a_failure_not_a_refusal() {
        let dir = scratch("unwritable");
        let app = dir.join("app");
        std::fs::create_dir_all(&app).unwrap();
        let report = default_report_path(&app);
        let cancel = app.join("cancel");

        // A file where the directory should be: the check cannot write
        // there.
        let blocked = app.join("blocked");
        std::fs::write(&blocked, b"not a directory").unwrap();

        let capabilities = check_run_capabilities(&blocked, None, &cancel, &report);

        assert!(
            !capabilities.all_ok(),
            "an unwritable directory must be reported as a failure"
        );
        assert!(
            capabilities.failures().count() >= 1,
            "the failure must be visible: {:?}",
            capabilities.lines()
        );
        // ... and the report was still written (the run is not
        // refused; the user is told).
        assert!(report.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_machine_without_nvenc_is_advisory() {
        let dir = scratch("no_nvenc");
        let app = dir.join("app");
        std::fs::create_dir_all(&app).unwrap();
        let report = default_report_path(&app);
        let cancel = app.join("cancel");

        let capabilities = check_run_capabilities(&app, None, &cancel, &report);

        // No NVENC is not a failure -- the run encodes in software.
        let nvenc = capabilities
            .checks
            .iter()
            .find(|c| c.name == "nvenc")
            .expect("the nvenc check must be there");
        assert_eq!(nvenc.status, CheckStatus::Advisory);
        assert!(
            capabilities.all_ok(),
            "advisory items do not make the run impossible"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
