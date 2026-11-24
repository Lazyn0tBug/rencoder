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
//!
//! # One check, shared
//!
//! The result is **one whole**, not "the answer to one item". The
//! machine's facts (can ffmpeg run? can NVENC run?) are gathered
//! **once** -- by the NVENC resolution -- and handed to the check
//! ([`MachineFacts`]). The check adopts them; it does not probe the
//! machine a second time. What the check does itself is the part
//! that changes with the directory: the filesystem.

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::config::paths::Paths;
use crate::encoder::nvenc;
use crate::error::EncodeError;

/// This run's machine facts: what the machine can do.
///
/// The facts are gathered **once** (design doc §3.10.7): whoever
/// needed them gathered them and hands them on -- to the check, to
/// the encoder cache, to the UI. Nobody asks a second time: asking
/// again is slow, and the second answer can disagree with the first
/// (the report says "no capability record" while the run encodes with
/// the GPU).
#[derive(Clone, Debug, Default)]
pub struct MachineFacts {
    /// Can ffmpeg run? `Some` = known, `None` = nobody asked yet.
    ///
    /// `None` means "this run did not need it" (an analyze run never
    /// touches an encoder) -- not "it cannot run".
    pub ffmpeg: Option<bool>,
    /// This machine's NVENC capability (`None` = nobody resolved it).
    pub nvenc: Option<nvenc::Capability>,
}

impl MachineFacts {
    /// The facts a resolved NVENC capability implies.
    ///
    /// The resolution ran `ffmpeg -encoders`, so ffmpeg works -- the
    /// two facts come out of the same call.
    pub fn from_resolution(resolution: &nvenc::Resolution) -> Self {
        Self {
            ffmpeg: Some(true),
            nvenc: Some(resolution.capability()),
        }
    }

    /// The facts when ffmpeg does not work: the resolution failed
    /// because ffmpeg did not run.
    pub fn ffmpeg_broken() -> Self {
        Self {
            ffmpeg: Some(false),
            nvenc: None,
        }
    }
}

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
///
/// The **whole** result, not "the answer to one item": the checks
/// and the machine's capability together (design doc §3.10.7). The
/// report file is this struct on disk -- a frontend reads it, it
/// does not recompute anything.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Capabilities {
    /// The checks, in the order they ran.
    pub checks: Vec<Check>,
    /// This machine's NVENC capability -- part of the result, and the
    /// part the frontends show next to the encoder switch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nvenc: Option<nvenc::Capability>,
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
/// `paths` decides where everything lives (design doc §3.6): the
/// application directory holds the config, the records and the machine
/// capability record; the operation directory holds the cancel request
/// and the report.
///
/// `facts` are the machine facts somebody already gathered (design doc
/// §3.10.7: the facts are gathered once and shared). The check adopts
/// them; only the filesystem -- the part that changes with the
/// directory -- is checked here.
pub fn check_run_capabilities(paths: &Paths, facts: &MachineFacts) -> Capabilities {
    let mut capabilities = Capabilities::default();

    // 1. Filesystem: the directories this run writes into.
    capabilities
        .checks
        .push(writable("app directory", &paths.app_dir));
    capabilities
        .checks
        .push(writable("operation directory", &paths.op_dir));

    // 2. ffmpeg: without it there is no encoding. The fact is known
    //    when the run resolved the machine's capability; when nobody
    //    needed it (an analyze run), the check asks now -- this is
    //    the one place that runs the probe.
    let ffmpeg = facts.ffmpeg.unwrap_or_else(probe_ffmpeg);
    capabilities.checks.push(if ffmpeg {
        Check::ok("ffmpeg")
    } else {
        Check::failed("ffmpeg", "ffmpeg is not available")
    });

    // 3. NVENC: advisory only -- a machine without it still encodes,
    //    in software. The capability the run resolved; if nobody
    //    resolved it, the stored record is the next best thing
    //    (reading it costs nothing -- probing would).
    let nvenc = facts
        .nvenc
        .clone()
        .or_else(|| stored_capability(&paths.capability_record));
    capabilities.checks.push(nvenc_check(nvenc.as_ref()));
    capabilities.nvenc = nvenc;

    // The report goes to a file: every frontend reads this one.
    if let Err(e) = write_report(&capabilities, &paths.capability_report) {
        tracing::warn!(error = %e, path = %paths.capability_report.display(), "could not write the capability report");
    }
    capabilities
}

/// Can a file be created in this directory?
fn writable(name: &str, dir: &Path) -> Check {
    match ensure_dir_writable(dir) {
        Ok(()) => Check::ok(name),
        Err(e) => Check::failed(name, e.to_string()),
    }
}

/// Check that a file can be created in `dir`, leaving nothing behind.
///
/// The **filesystem** part of the run capability check (design doc
/// §3.10.7): the app directory holds the config, the records and the
/// cancel request, so a directory that cannot be written means the run
/// cannot finish. That is a precondition, checked before the work
/// starts -- not something to discover when the run is over, or when a
/// signal arrives.
///
/// The probe file's name starts with a dash so the input collector
/// skips it even if this process dies between the write and the
/// delete.
///
/// This is plain `std` -- it is not an OS difference, so it does not
/// belong in the platform layer (design doc §1.3).
fn ensure_dir_writable(dir: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    let probe = dir.join(format!("-rencoder-write-probe.{}", std::process::id()));
    std::fs::write(&probe, b"")?;
    let _ = std::fs::remove_file(&probe);
    Ok(())
}

/// Can ffmpeg run? The probe the check runs when nobody asked before.
fn probe_ffmpeg() -> bool {
    crate::encoder::nvenc::run_ffmpeg(&["-encoders"]).is_ok()
}

/// The capability the stored record describes.
fn stored_capability(record_path: &Path) -> Option<nvenc::Capability> {
    let record = nvenc::load(record_path)?;
    Some(nvenc::Capability::from_check(
        record.encoders.clone(),
        Some(record.probed_at),
        false,
    ))
}

/// The NVENC line: advisory -- a machine without it still encodes, in
/// software.
fn nvenc_check(capability: Option<&nvenc::Capability>) -> Check {
    match capability {
        None => Check::advisory("nvenc", "no capability record yet"),
        Some(capability) => {
            if capability.any_usable() {
                Check::ok("nvenc")
            } else {
                Check::advisory("nvenc", "no NVENC encoder is usable here")
            }
        }
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
    use crate::config::app::AppConfig;
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

    /// The paths of a run: the application directory holds the app's
    /// own files, the operation directory holds this run's state.
    fn paths(app: &std::path::Path, op: &std::path::Path) -> Paths {
        Paths::resolve(
            app.join("rencoder.toml"),
            app.to_path_buf(),
            op.to_path_buf(),
            &AppConfig::default(),
        )
    }

    /// The facts a machine with a working NVENC encoder produces.
    fn facts_with_nvenc() -> MachineFacts {
        MachineFacts {
            ffmpeg: Some(true),
            nvenc: Some(nvenc::Capability::from_check(
                vec![crate::types::NvencSupport {
                    encoder: "h264_nvenc".to_string(),
                    usable: true,
                    reason: None,
                }],
                None,
                false,
            )),
        }
    }

    #[test]
    fn the_report_is_written_and_read_back() {
        let dir = scratch("report");
        let app = dir.join("app");
        let op = dir.join("op");
        let paths = paths(&app, &op);

        let capabilities = check_run_capabilities(&paths, &facts_with_nvenc());

        assert!(
            paths.capability_report.exists(),
            "the report must be a file"
        );
        let read = read_report(&paths.capability_report).expect("the report must be readable");
        assert_eq!(read.checks.len(), capabilities.checks.len());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_report_carries_the_machine_capability() {
        let dir = scratch("capability_in_report");
        let app = dir.join("app");
        let op = dir.join("op");
        let paths = paths(&app, &op);

        let capabilities = check_run_capabilities(&paths, &facts_with_nvenc());

        // The whole result: the checks *and* the machine's capability.
        let capability = capabilities
            .nvenc
            .as_ref()
            .expect("the capability must be part of the result");
        assert!(capability.any_usable());
        // ... and it survives the round trip through the file.
        let read = read_report(&paths.capability_report).expect("the report must be readable");
        assert!(read
            .nvenc
            .expect("the capability must be in the file")
            .any_usable());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_unwritable_directory_is_a_failure_not_a_refusal() {
        let dir = scratch("unwritable");
        let app = dir.join("app");
        std::fs::create_dir_all(&app).unwrap();
        // A file where the directory should be: the check cannot write
        // there.
        let blocked = app.join("blocked");
        std::fs::write(&blocked, b"not a directory").unwrap();
        let op = dir.join("op");
        let paths = paths(&blocked, &op);

        let capabilities = check_run_capabilities(&paths, &facts_with_nvenc());

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
        assert!(paths.capability_report.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_machine_without_nvenc_is_advisory() {
        let dir = scratch("no_nvenc");
        let app = dir.join("app");
        std::fs::create_dir_all(&app).unwrap();
        let op = dir.join("op");
        let paths = paths(&app, &op);

        // The machine has no usable NVENC encoder.
        let facts = MachineFacts {
            ffmpeg: Some(true),
            nvenc: Some(nvenc::Capability::default()),
        };
        let capabilities = check_run_capabilities(&paths, &facts);

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

    #[test]
    fn the_facts_are_adopted_not_recomputed() {
        let dir = scratch("adopted_facts");
        let app = dir.join("app");
        std::fs::create_dir_all(&app).unwrap();
        let op = dir.join("op");
        let paths = paths(&app, &op);

        // The facts say ffmpeg does not work. The check adopts them:
        // it does not run ffmpeg to find out a second time (and the
        // answer it reports is the one the run is going with).
        let facts = MachineFacts::ffmpeg_broken();
        let capabilities = check_run_capabilities(&paths, &facts);

        let ffmpeg = capabilities
            .checks
            .iter()
            .find(|c| c.name == "ffmpeg")
            .expect("the ffmpeg check must be there");
        assert_eq!(ffmpeg.status, CheckStatus::Failed);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
