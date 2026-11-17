//! NVENC capability detection, and the on-disk record of its result.
//!
//! The probe answers one question: "can **this ffmpeg** run `av1_nvenc`
//! **on this machine**". `ffmpeg -encoders` says the encoder was
//! compiled in, `nvidia-smi -L` says a GPU exists -- neither says the
//! encoder will actually run here (compute capability, driver version,
//! NVENC session quota). The probe answers it with a 1-frame test
//! encode.
//!
//! The answer is stable for months (hardware does not change often), so
//! it is stored in `<app_dir>/nvenc-capability.json` and reused across
//! processes. The record is bound to the ffmpeg build it was made with:
//! swapping the ffmpeg binary can change the answer, so a mismatching
//! fingerprint invalidates the record.
//!
//! Freshness is advisory, not an expiry: after 30 days the tool *advises*
//! the user to re-probe, but the stored result stays in use until the
//! user says otherwise. Nobody knows the state of a machine better than
//! its owner.

use std::path::{Path, PathBuf};

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};

use crate::error::EncodeError;
use crate::types::NvencSupport;

/// File name of the capability record inside the app dir.
pub const CAPABILITY_FILE_NAME: &str = "nvenc-capability.json";

/// How long a stored record is considered fresh.
pub const REVIEW_INTERVAL_DAYS: i64 = 30;

/// Version of the on-disk format. A record with a different version is
/// ignored (and re-probed) rather than misread.
const RECORD_VERSION: u32 = 1;

/// NVENC encoders that may exist in an ffmpeg build. Only the ones
/// actually present in `ffmpeg -encoders` get probed.
const NVENC_CANDIDATES: [&str; 3] = ["h264_nvenc", "hevc_nvenc", "av1_nvenc"];

/// The stored result of the last capability probe.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CapabilityRecord {
    /// On-disk format version.
    pub version: u32,
    /// `ffmpeg -version` first line.
    pub ffmpeg_version: String,
    /// Fingerprint of the ffmpeg build this record belongs to.
    pub ffmpeg_fingerprint: String,
    /// When the probe ran.
    pub probed_at: DateTime<Utc>,
    /// When to *advise* the user to re-probe. Not an expiry: the record
    /// stays usable until the user declines a refresh.
    pub next_review_at: DateTime<Utc>,
    /// Per-encoder probe results.
    pub encoders: Vec<NvencSupport>,
}

impl CapabilityRecord {
    /// Is it time to advise the user to re-probe?
    pub fn review_due(&self, now: DateTime<Utc>) -> bool {
        now >= self.next_review_at
    }

    /// Does this record belong to the given ffmpeg build?
    pub fn matches_fingerprint(&self, fingerprint: &str) -> bool {
        self.ffmpeg_fingerprint == fingerprint
    }

    /// The probe results.
    pub fn support(&self) -> &[NvencSupport] {
        &self.encoders
    }

    /// Push the review date 30 days out -- called when the user was
    /// advised and declined. We must not nag on every run.
    pub fn defer_review(&mut self, now: DateTime<Utc>) {
        self.next_review_at = now + Duration::days(REVIEW_INTERVAL_DAYS);
    }
}

/// Outcome of resolving the capability for this run.
#[derive(Clone, Debug)]
pub struct Resolution {
    /// `ffmpeg -encoders` output (shared with the encoder cache).
    pub encoders_output: String,
    /// The probe results to use for this run.
    pub support: Vec<NvencSupport>,
    /// The record to persist, when the result is new (probed now) or
    /// when the review date was pushed back. `None` when the stored
    /// record is used unchanged.
    pub to_save: Option<CapabilityRecord>,
    /// The stored record is past its review date: advise the user.
    pub review_due: bool,
    /// The stored record did not match the current ffmpeg build (or was
    /// missing / unreadable), so a fresh probe ran.
    pub rebuilt: bool,
}

/// Resolve the NVENC capability for this run.
///
/// * `force` -- probe now, whatever is stored.
/// * else if a stored record exists, matches the current ffmpeg build and
///   is not due for review -- use it (no probe, no extra subprocesses
///   beyond identifying the build).
/// * else -- probe now.
///
/// The caller decides whether to persist [`Resolution::to_save`] and
/// whether to act on [`Resolution::review_due`].
///
/// `path` is the record's location; `None` disables the store entirely
/// (every call probes).
pub fn resolve(path: Option<&Path>, force: bool) -> Result<Resolution, EncodeError> {
    let encoders_output = run_ffmpeg(&["-encoders"])?;
    let ffmpeg_version = ffmpeg_version()?;
    let fingerprint = fingerprint(&ffmpeg_version, &encoders_output);

    let stored = path.and_then(load);
    let usable_stored = stored
        .as_ref()
        .filter(|record| record.matches_fingerprint(&fingerprint));

    if !force {
        if let Some(record) = usable_stored {
            let review_due = record.review_due(Utc::now());
            return Ok(Resolution {
                encoders_output,
                support: record.support().to_vec(),
                to_save: None,
                review_due,
                rebuilt: false,
            });
        }
    }

    // Probe now.
    let support = probe(&encoders_output);
    let now = Utc::now();
    let record = CapabilityRecord {
        version: RECORD_VERSION,
        ffmpeg_version,
        ffmpeg_fingerprint: fingerprint,
        probed_at: now,
        next_review_at: now + Duration::days(REVIEW_INTERVAL_DAYS),
        encoders: support.clone(),
    };
    Ok(Resolution {
        encoders_output,
        support,
        to_save: Some(record),
        review_due: false,
        rebuilt: true,
    })
}

/// Probe every NVENC encoder this ffmpeg build has.
///
/// The probe is a 1-frame test encode through lavfi: it exercises the
/// driver, the ffmpeg build, the NVENC session quota and the GPU's
/// compute capability in one go -- the only combination that actually
/// proves the encoder can run here.
pub fn probe(encoders_output: &str) -> Vec<NvencSupport> {
    probe_with(encoders_output, run_ffmpeg)
}

/// Probe with an injectable runner (test seam).
fn probe_with<F>(encoders_output: &str, runner: F) -> Vec<NvencSupport>
where
    F: Fn(&[&str]) -> Result<String, EncodeError>,
{
    let mut support = Vec::new();
    for encoder in NVENC_CANDIDATES {
        if !encoders_output.contains(encoder) {
            // Not compiled into this ffmpeg -- nothing to probe.
            continue;
        }
        match probe_encoder(&runner, encoder) {
            Ok(()) => {
                tracing::info!(encoder, "NVENC capability probe passed");
                support.push(NvencSupport {
                    encoder: encoder.to_string(),
                    usable: true,
                    reason: None,
                });
            }
            Err(reason) => {
                tracing::warn!(
                    encoder,
                    reason = %reason,
                    "NVENC capability probe failed; encoder unavailable on this machine"
                );
                support.push(NvencSupport {
                    encoder: encoder.to_string(),
                    usable: false,
                    reason: Some(reason),
                });
            }
        }
    }
    support
}

/// Can this NVENC encoder run here? Encode one frame of a synthetic
/// source with it.
fn probe_encoder<F>(runner: &F, encoder: &str) -> Result<(), String>
where
    F: Fn(&[&str]) -> Result<String, EncodeError>,
{
    runner(&[
        "-hide_banner",
        "-loglevel",
        "error",
        "-f",
        "lavfi",
        "-i",
        "color=s=64x64:d=0.1",
        "-frames:v",
        "1",
        "-c:v",
        encoder,
        "-f",
        "null",
        "-",
    ])
    .map(|_| ())
    .map_err(|e| failure_reason(&e))
}

/// `ffmpeg -version`, first line only.
fn ffmpeg_version() -> Result<String, EncodeError> {
    let out = run_ffmpeg(&["-version"])?;
    Ok(out.lines().next().unwrap_or("").trim().to_string())
}

/// Fingerprint of an ffmpeg build.
///
/// Both the version string and the encoder list matter: the version
/// catches a different ffmpeg, the encoder list catches a rebuild of the
/// same version with different compile flags (nvenc in or out).
pub fn fingerprint(ffmpeg_version: &str, encoders_output: &str) -> String {
    let mut data = Vec::new();
    data.extend_from_slice(ffmpeg_version.as_bytes());
    data.push(0);
    data.extend_from_slice(encoders_output.as_bytes());
    format!("{:016x}", fnv1a(&data))
}

/// FNV-1a 64-bit. Small and dependency-free; `std`'s `DefaultHasher`
/// is explicitly not stable across releases, and this value has to
/// survive in a file.
fn fnv1a(data: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in data {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// Load a stored record. `None` when it is missing, unreadable, or was
/// written by an incompatible version.
pub fn load(path: &Path) -> Option<CapabilityRecord> {
    let raw = std::fs::read_to_string(path).ok()?;
    let record: CapabilityRecord = serde_json::from_str(&raw).ok()?;
    if record.version != RECORD_VERSION {
        return None;
    }
    Some(record)
}

/// Store a record. Writes to a temp file and renames it into place, so
/// a crash mid-write cannot leave a half-written record behind.
///
/// A failure here is not fatal: the capability is still usable for this
/// run, it just will not be reused next time.
pub fn save(path: &Path, record: &CapabilityRecord) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }
    let json = serde_json::to_string_pretty(record)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;

    // Temp name carries the pid: several processes may write the same
    // record concurrently (the CLI and the GUI can share an app dir).
    let tmp = tmp_path_for(path);
    let result = (|| -> std::io::Result<()> {
        std::fs::write(&tmp, json.as_bytes())?;
        std::fs::rename(&tmp, path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

fn tmp_path_for(path: &Path) -> PathBuf {
    let mut tmp = path.to_path_buf();
    let name = match path.file_name() {
        Some(n) => format!("{}.{}.tmp", n.to_string_lossy(), std::process::id()),
        None => format!("nvenc-capability.{}.tmp", std::process::id()),
    };
    tmp.set_file_name(name);
    tmp
}

/// A human-readable report of the stored capability.
pub fn report(record: &CapabilityRecord) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "NVENC capability (probed {})\n",
        record.probed_at.to_rfc3339()
    ));
    out.push_str(&format!("  ffmpeg      : {}\n", record.ffmpeg_version));
    if record.encoders.is_empty() {
        out.push_str("  (this ffmpeg build has no NVENC encoders)\n");
        return out;
    }
    for support in &record.encoders {
        match (&support.usable, &support.reason) {
            (true, _) => out.push_str(&format!("  {:<12} : usable\n", support.encoder)),
            (false, Some(reason)) => out.push_str(&format!(
                "  {:<12} : UNUSABLE — {}\n",
                support.encoder, reason
            )),
            (false, None) => out.push_str(&format!("  {:<12} : UNUSABLE\n", support.encoder)),
        }
    }
    out
}

/// Run ffmpeg and return its stdout.
pub(crate) fn run_ffmpeg(args: &[&str]) -> Result<String, EncodeError> {
    let output = std::process::Command::new("ffmpeg")
        .args(args)
        .output()
        .map_err(|e| EncodeError::Probe(format!("ffmpeg spawn: {e}")))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(EncodeError::Probe(format!(
            "ffmpeg exited {}: {}",
            output.status,
            stderr.trim()
        )));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Turn a probe error into a log-friendly reason.
///
/// The failure format is `ffmpeg exited <code>: <stderr>`; the stderr
/// part is what actually explains the failure, so keep it and drop the
/// wrapper.
fn failure_reason(err: &EncodeError) -> String {
    let owned;
    let text = match err {
        EncodeError::Probe(msg) => msg.as_str(),
        other => {
            owned = other.to_string();
            owned.as_str()
        }
    };
    let detail = match text.split_once(" exited ") {
        Some((_, rest)) => match rest.split_once(": ") {
            Some((_, stderr)) => stderr,
            None => rest,
        },
        None => text,
    };
    let detail = detail.trim();
    const MAX: usize = 200;
    if detail.chars().count() > MAX {
        let mut trimmed: String = detail.chars().take(MAX).collect();
        trimmed.push('…');
        trimmed
    } else {
        detail.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_path(label: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("rencoder_nvenc_{}_{}", label, std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.join(CAPABILITY_FILE_NAME)
    }

    fn sample_record() -> CapabilityRecord {
        let now = Utc::now();
        CapabilityRecord {
            version: RECORD_VERSION,
            ffmpeg_version: "ffmpeg version 9.0.1".to_string(),
            ffmpeg_fingerprint: "abc123".to_string(),
            probed_at: now,
            next_review_at: now + Duration::days(REVIEW_INTERVAL_DAYS),
            encoders: vec![
                NvencSupport {
                    encoder: "h264_nvenc".to_string(),
                    usable: true,
                    reason: None,
                },
                NvencSupport {
                    encoder: "av1_nvenc".to_string(),
                    usable: false,
                    reason: Some("No NVENC capable devices found".to_string()),
                },
            ],
        }
    }

    #[test]
    fn save_then_load_round_trips() {
        let path = temp_path("roundtrip");
        let record = sample_record();
        save(&path, &record).unwrap();
        let loaded = load(&path).expect("record should load");
        assert_eq!(loaded.ffmpeg_version, record.ffmpeg_version);
        assert_eq!(loaded.encoders.len(), 2);
        assert_eq!(
            loaded.encoders[1].reason.as_deref(),
            Some("No NVENC capable devices found")
        );
    }

    #[test]
    fn load_missing_file_is_none() {
        let path = temp_path("missing");
        assert!(load(&path).is_none());
    }

    #[test]
    fn load_corrupt_file_is_none() {
        let path = temp_path("corrupt");
        std::fs::write(&path, b"{not json").unwrap();
        assert!(load(&path).is_none());
    }

    #[test]
    fn load_unknown_version_is_none() {
        let path = temp_path("version");
        let mut record = sample_record();
        record.version = RECORD_VERSION + 1;
        save(&path, &record).unwrap();
        assert!(load(&path).is_none());
    }

    #[test]
    fn review_due_only_after_the_interval() {
        let record = sample_record();
        assert!(!record.review_due(record.next_review_at - Duration::days(1)));
        assert!(record.review_due(record.next_review_at));
        assert!(record.review_due(record.next_review_at + Duration::days(1)));
    }

    #[test]
    fn defer_review_pushes_the_date_out() {
        let mut record = sample_record();
        let now = record.next_review_at + Duration::days(10);
        record.defer_review(now);
        assert!(!record.review_due(now));
        assert!(record.review_due(now + Duration::days(REVIEW_INTERVAL_DAYS)));
    }

    #[test]
    fn fingerprint_changes_with_the_build() {
        let a = fingerprint("ffmpeg version 9.0.1", "encoders");
        let b = fingerprint("ffmpeg version 9.0.2", "encoders");
        let c = fingerprint("ffmpeg version 9.0.1", "encoders2");
        assert_ne!(a, b);
        assert_ne!(a, c);
        assert_eq!(a, fingerprint("ffmpeg version 9.0.1", "encoders"));
    }

    #[test]
    fn report_lists_usable_and_unusable_encoders() {
        let text = report(&sample_record());
        assert!(text.contains("h264_nvenc   : usable"), "{text}");
        assert!(text.contains("av1_nvenc    : UNUSABLE"), "{text}");
        assert!(text.contains("No NVENC capable devices found"));
    }
}
