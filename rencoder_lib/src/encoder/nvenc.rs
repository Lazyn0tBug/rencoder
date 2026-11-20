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

use std::collections::HashMap;
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

/// NVENC verdicts for one run: which encoders are known to be unusable.
///
/// The scope is "one run" — the CLI's process, or the GUI session. The
/// owner is explicit: whoever runs the batch holds the value and
/// passes it down (`select_encoder` reads it, the NVENC fallback
/// writes it). No lock, no process-global table: the write happens in
/// exactly one place (the moment an NVENC encode fails) and the read
/// in exactly one (encoder selection).
///
/// The GUI's actor keeps the authoritative copy: each batch worker
/// gets a snapshot, and downgrades the worker finds are reported back
/// so the next batch inherits them.
#[derive(Clone, Debug, Default)]
pub struct CapabilityVerdicts {
    /// Encoder name → usable?
    map: HashMap<String, bool>,
}

impl CapabilityVerdicts {
    /// The verdict we already have for this encoder, if any.
    pub fn known(&self, encoder: &str) -> Option<bool> {
        self.map.get(encoder).copied()
    }

    /// Record a verdict for this encoder.
    pub fn set(&mut self, encoder: &str, usable: bool) {
        self.map.insert(encoder.to_string(), usable);
    }

    /// Is this encoder known to be unusable? (`select_encoder` skips it.)
    pub fn is_disabled(&self, encoder: &str) -> bool {
        self.known(encoder) == Some(false)
    }

    /// Fold another set of verdicts into this one.
    ///
    /// Used by the actor to merge a worker's findings back into the
    /// authoritative copy.
    pub fn merge(&mut self, other: &CapabilityVerdicts) {
        for (encoder, usable) in &other.map {
            self.map.insert(encoder.clone(), *usable);
        }
    }

    /// The verdicts that differ from `base`.
    ///
    /// A batch worker starts from a snapshot of the authoritative copy
    /// and reports what it changed; this is how the difference is
    /// computed.
    pub fn changes_since(&self, base: &CapabilityVerdicts) -> Vec<(String, bool)> {
        self.map
            .iter()
            .filter(|(encoder, usable)| base.map.get(*encoder) != Some(*usable))
            .map(|(encoder, usable)| (encoder.clone(), *usable))
            .collect()
    }
}

/// Can this machine run the NVENC encoder *right now*?
///
/// A 1-frame test encode -- the same probe the capability record is
/// built from. Used when an NVENC encode fails: a failure alone does
/// not prove the encoder is unusable (a full disk or a bad passthrough
/// argument fails it too), and the CLI cannot tell them apart because
/// ffmpeg's stderr is inherited rather than captured.
///
/// The verdict is recorded in `verdicts`: the second failure of the
/// same encoder does not probe again.
pub fn verify_capability(verdicts: &mut CapabilityVerdicts, encoder: &str) -> bool {
    if let Some(known) = verdicts.known(encoder) {
        return known;
    }
    let usable = probe_one(encoder).is_ok();
    verdicts.set(encoder, usable);
    usable
}

/// Is this an NVENC encoder? (`h264_nvenc`, `hevc_nvenc`, `av1_nvenc`)
pub fn is_nvenc_encoder(encoder: &str) -> bool {
    encoder.ends_with("_nvenc")
}

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
    /// The record describing this result -- freshly built when we probed,
    /// or the stored one we reused.
    pub record: CapabilityRecord,
    /// Was the record freshly built? Then it is worth storing.
    pub probed: bool,
    /// The stored record is past its review date: advise the user.
    pub review_due: bool,
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
    resolve_with(path, force, run_ffmpeg)
}

/// Resolve with an injectable runner (test seam).
///
/// The runner answers every ffmpeg invocation: `["-encoders"]`,
/// `["-version"]` and the 1-frame capability probes.
pub fn resolve_with<F>(
    path: Option<&Path>,
    force: bool,
    runner: F,
) -> Result<Resolution, EncodeError>
where
    F: Fn(&[&str]) -> Result<String, EncodeError>,
{
    resolve_with_clock(path, force, runner, Utc::now())
}

/// Resolve with an injectable runner **and** an injectable clock.
///
/// The clock is what makes the 30-day review window testable: a test
/// hands in a time it chooses, advances it past the review date, and
/// watches the record go stale -- without waiting a month and without
/// the flow reading the wall clock behind the test's back.
///
/// `now` is used for every timestamp this call produces: the review
/// check, the record's `probed_at` and `next_review_at`, and the
/// deferred review date when the record is stored back.
pub fn resolve_with_clock<F>(
    path: Option<&Path>,
    force: bool,
    runner: F,
    now: DateTime<Utc>,
) -> Result<Resolution, EncodeError>
where
    F: Fn(&[&str]) -> Result<String, EncodeError>,
{
    let encoders_output = runner(&["-encoders"])?;
    let version = runner(&["-version"])?;
    let ffmpeg_version = version.lines().next().unwrap_or("").trim().to_string();
    let fingerprint = fingerprint(&ffmpeg_version, &encoders_output);

    let stored = path.and_then(load);
    let usable_stored = stored
        .as_ref()
        .filter(|record| record.matches_fingerprint(&fingerprint));

    if !force {
        if let Some(record) = usable_stored {
            let review_due = record.review_due(now);
            return Ok(Resolution {
                encoders_output,
                support: record.support().to_vec(),
                record: record.clone(),
                probed: false,
                review_due,
            });
        }
    }

    // Probe now.
    let support = probe_with(&encoders_output, &runner);
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
        record,
        probed: true,
        review_due: false,
    })
}

/// Resolve the capability and store the result when it is new.
///
/// Two write cases:
///
/// * a fresh probe -> store the new record;
/// * the stored record was past its review date -> push the review
///   date 30 days out, so the advisory ("this result may be stale")
///   is shown **once per 30 days** rather than on every start. This
///   is the GUI's equivalent of the CLI's "declined the refresh"
///   answer: the CLI asks, the GUI cannot ask (no stdin), so both
///   end up with "advise once, then leave it alone until the user
///   asks for a refresh".
///
/// A store failure is not fatal: the capability is still usable for
/// this run, it just will not be reused next time.
pub fn resolve_and_store(path: Option<&Path>, force: bool) -> Result<Resolution, EncodeError> {
    resolve_and_store_with(path, force, run_ffmpeg)
}

/// Resolve **and store** with an injectable runner (test seam).
///
/// Same as [`resolve_and_store`], but the ffmpeg invocations go through
/// `runner` — the same seam [`resolve_with`] offers. The GUI actor
/// injects it so its tests never spawn ffmpeg; the CLI keeps the real
/// one.
pub fn resolve_and_store_with<F>(
    path: Option<&Path>,
    force: bool,
    runner: F,
) -> Result<Resolution, EncodeError>
where
    F: Fn(&[&str]) -> Result<String, EncodeError>,
{
    resolve_and_store_with_clock(path, force, runner, Utc::now())
}

/// Resolve **and store** with an injectable runner and clock.
///
/// Same as [`resolve_and_store_with`], but the call chooses "now" -- so
/// the deferred review date is the test's, not the wall clock's.
pub fn resolve_and_store_with_clock<F>(
    path: Option<&Path>,
    force: bool,
    runner: F,
    now: DateTime<Utc>,
) -> Result<Resolution, EncodeError>
where
    F: Fn(&[&str]) -> Result<String, EncodeError>,
{
    let mut resolution = resolve_with_clock(path, force, runner, now)?;
    if let Some(record) = record_to_store(&resolution, now) {
        resolution.record = record.clone();
        if let Some(path) = path {
            if let Err(e) = save(path, &record) {
                tracing::warn!(
                    error = %e,
                    path = %path.display(),
                    "could not store the NVENC capability record"
                );
            }
        }
    }
    Ok(resolution)
}

/// What a resolution wants stored, if anything.
///
/// * a fresh probe -> the new record;
/// * an expired record -> the same record with its review date pushed
///   30 days out (the advisory has been shown; do not repeat it);
/// * a fresh stored record -> nothing to write.
///
/// `now` is the caller's time: the deferred review date is measured
/// from it, so a test can watch the advisory fire exactly once.
fn record_to_store(resolution: &Resolution, now: DateTime<Utc>) -> Option<CapabilityRecord> {
    if resolution.probed {
        return Some(resolution.record.clone());
    }
    if resolution.review_due {
        let mut record = resolution.record.clone();
        record.defer_review(now);
        return Some(record);
    }
    None
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
pub fn probe_one(encoder: &str) -> Result<(), String> {
    probe_encoder(&run_ffmpeg, encoder)
}

/// Same, with an injectable runner (test seam).
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
    // Same wording as the UI's capability list -- both render
    // `NvencCapability::lines()`.
    for line in capability_of(record).lines() {
        out.push_str(&format!("  {line}\n"));
    }
    out
}

/// The capability of this machine, in the shape the frontends render.
pub fn capability_of(record: &CapabilityRecord) -> crate::types::NvencCapability {
    crate::types::NvencCapability {
        support: record.encoders.clone(),
        probed_at: Some(record.probed_at),
        review_due: record.review_due(Utc::now()),
    }
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
    use std::sync::{Arc, Mutex};

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

    // ---- capability verdicts ----

    #[test]
    fn capability_verdict_round_trips() {
        let encoder = "registry_test_nvenc";
        let mut verdicts = CapabilityVerdicts::default();
        assert_eq!(verdicts.known(encoder), None);
        assert!(!verdicts.is_disabled(encoder));

        verdicts.set(encoder, false);
        assert_eq!(verdicts.known(encoder), Some(false));
        assert!(verdicts.is_disabled(encoder));

        verdicts.set(encoder, true);
        assert_eq!(verdicts.known(encoder), Some(true));
        assert!(!verdicts.is_disabled(encoder));
    }

    #[test]
    fn verdicts_merge_folds_the_other_side_in() {
        // The actor merges a worker's findings back into the
        // authoritative copy.
        let mut authoritative = CapabilityVerdicts::default();
        authoritative.set("a_nvenc", false);

        let mut worker = authoritative.clone();
        worker.set("b_nvenc", false);

        authoritative.merge(&worker);
        assert!(authoritative.is_disabled("a_nvenc"));
        assert!(authoritative.is_disabled("b_nvenc"));
    }

    #[test]
    fn changes_since_reports_only_the_differences() {
        // The batch worker starts from a snapshot and reports what it
        // changed — the actor merges those into its copy.
        let base = CapabilityVerdicts::default();

        let mut worker = base.clone();
        worker.set("a_nvenc", false);
        assert_eq!(
            worker.changes_since(&base),
            vec![("a_nvenc".to_string(), false)]
        );

        // Nothing changed since the base: no report.
        assert!(worker.changes_since(&worker).is_empty());

        // A verdict that came back (the encoder works after all) is a
        // change too.
        let mut worker = base.clone();
        worker.set("b_nvenc", false);
        worker.set("b_nvenc", true);
        assert_eq!(
            worker.changes_since(&base),
            vec![("b_nvenc".to_string(), true)]
        );
    }

    #[test]
    fn verify_capability_uses_the_cached_verdict() {
        // A cached verdict must not trigger another probe -- the probe
        // spawns ffmpeg, and the second failure of the same encoder
        // must not cost another spawn.
        let encoder = "cached_test_nvenc";
        let mut verdicts = CapabilityVerdicts::default();
        verdicts.set(encoder, false);
        assert!(!verify_capability(&mut verdicts, encoder));
        assert_eq!(verdicts.known(encoder), Some(false));
    }

    #[test]
    fn is_nvenc_encoder_recognises_the_suffix() {
        assert!(is_nvenc_encoder("h264_nvenc"));
        assert!(is_nvenc_encoder("hevc_nvenc"));
        assert!(is_nvenc_encoder("av1_nvenc"));
        assert!(!is_nvenc_encoder("libx264"));
        assert!(!is_nvenc_encoder("libsvtav1"));
    }

    #[test]
    fn expired_record_pushes_the_review_date_when_stored() {
        // The advisory is shown once per 30 days: reporting it pushes
        // the date, so the next start stays quiet.
        let mut record = sample_record();
        record.next_review_at = Utc::now() - Duration::days(1);
        let resolution = Resolution {
            encoders_output: String::new(),
            support: record.support().to_vec(),
            record: record.clone(),
            probed: false,
            review_due: true,
        };
        let stored =
            record_to_store(&resolution, Utc::now()).expect("an expired record must be re-stored");
        assert!(
            stored.next_review_at > Utc::now() + Duration::days(29),
            "the review date must be pushed ~30 days out: {:?}",
            stored.next_review_at
        );
    }

    #[test]
    fn fresh_record_is_not_rewritten() {
        let record = sample_record();
        let resolution = Resolution {
            encoders_output: String::new(),
            support: record.support().to_vec(),
            record: record.clone(),
            probed: false,
            review_due: false,
        };
        assert!(
            record_to_store(&resolution, Utc::now()).is_none(),
            "a fresh record needs no write"
        );
    }

    #[test]
    fn probed_record_is_stored_as_is() {
        let record = sample_record();
        let resolution = Resolution {
            encoders_output: String::new(),
            support: record.support().to_vec(),
            record: record.clone(),
            probed: true,
            review_due: false,
        };
        let stored =
            record_to_store(&resolution, Utc::now()).expect("a fresh probe must be stored");
        assert_eq!(stored.probed_at, record.probed_at);
    }

    // ---- resolve (with an injected runner) ----

    /// Fake ffmpeg: answers `-encoders`, `-version`, and the capability
    /// probes. Records every invocation so the tests can assert what
    /// was (not) probed.
    fn fake_ffmpeg(
        encoders: &'static str,
        version: &'static str,
        probe_ok: bool,
        calls: Arc<Mutex<Vec<String>>>,
    ) -> impl Fn(&[&str]) -> Result<String, EncodeError> {
        move |args: &[&str]| {
            calls.lock().unwrap().push(args.join(" "));
            if args == ["-encoders"] {
                return Ok(encoders.to_string());
            }
            if args == ["-version"] {
                return Ok(version.to_string());
            }
            // A capability probe.
            if probe_ok {
                Ok(String::new())
            } else {
                Err(EncodeError::Probe(
                    "ffmpeg exited 1: No NVENC capable devices found".into(),
                ))
            }
        }
    }

    const ENCODERS_WITH_NVENC: &str = "\
Encoders:
 V..... libsvtav1            libsvtav1 SVT-AV1
 V..... av1_nvenc            NVIDIA NVENC AV1 encoder
";

    const FFMPEG_VERSION: &str = "ffmpeg version 9.0.1 Copyright (c) 2000-2026";

    fn store_path(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "rencoder_nvenc_resolve_{}_{}",
            label,
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.join(CAPABILITY_FILE_NAME)
    }

    #[test]
    fn resolve_without_a_record_probes_and_builds_one() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let runner = fake_ffmpeg(ENCODERS_WITH_NVENC, FFMPEG_VERSION, true, calls.clone());
        let path = store_path("no_record");

        let resolution = resolve_with(Some(&path), false, runner).unwrap();
        assert!(resolution.probed, "a missing record must be probed");
        assert_eq!(resolution.support.len(), 1);
        assert!(resolution.support[0].usable);
        // The probe ran (one call per NVENC encoder in the build).
        assert!(calls
            .lock()
            .unwrap()
            .iter()
            .any(|c| c.contains("-c:v av1_nvenc")));
    }

    #[test]
    fn resolve_reuses_a_fresh_record_without_probing() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let runner = fake_ffmpeg(ENCODERS_WITH_NVENC, FFMPEG_VERSION, true, calls.clone());
        let path = store_path("fresh");

        // Store a record that matches the fake build's fingerprint.
        let mut record = sample_record();
        record.ffmpeg_fingerprint = fingerprint(FFMPEG_VERSION, ENCODERS_WITH_NVENC);
        save(&path, &record).unwrap();

        let resolution = resolve_with(Some(&path), false, runner).unwrap();
        assert!(!resolution.probed, "a fresh record must be reused");
        assert!(!resolution.review_due);
        // No probe ran.
        assert!(
            !calls.lock().unwrap().iter().any(|c| c.contains("-c:v")),
            "a fresh record must not trigger a probe: {:?}",
            calls.lock().unwrap()
        );
    }

    #[test]
    fn resolve_reprobes_when_the_record_belongs_to_another_build() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let runner = fake_ffmpeg(ENCODERS_WITH_NVENC, FFMPEG_VERSION, true, calls.clone());
        let path = store_path("other_build");

        // A record from a different ffmpeg build (different fingerprint).
        let mut record = sample_record();
        record.ffmpeg_fingerprint = "0000000000000000".to_string();
        save(&path, &record).unwrap();

        let resolution = resolve_with(Some(&path), false, runner).unwrap();
        assert!(
            resolution.probed,
            "a record from another build must be re-probed"
        );
    }

    #[test]
    fn resolve_forces_a_probe_even_with_a_fresh_record() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let runner = fake_ffmpeg(ENCODERS_WITH_NVENC, FFMPEG_VERSION, true, calls.clone());
        let path = store_path("forced");

        let mut record = sample_record();
        record.ffmpeg_fingerprint = fingerprint(FFMPEG_VERSION, ENCODERS_WITH_NVENC);
        save(&path, &record).unwrap();

        let resolution = resolve_with(Some(&path), true, runner).unwrap();
        assert!(resolution.probed, "force must re-probe");
    }

    #[test]
    fn resolve_reports_an_expired_record_as_due_for_review() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let runner = fake_ffmpeg(ENCODERS_WITH_NVENC, FFMPEG_VERSION, true, calls.clone());
        let path = store_path("expired");

        let mut record = sample_record();
        record.ffmpeg_fingerprint = fingerprint(FFMPEG_VERSION, ENCODERS_WITH_NVENC);
        record.next_review_at = Utc::now() - Duration::days(1);
        save(&path, &record).unwrap();

        let resolution = resolve_with(Some(&path), false, runner).unwrap();
        assert!(!resolution.probed, "an expired record is still reused");
        assert!(resolution.review_due, "an expired record must be flagged");
    }

    #[test]
    fn resolve_marks_a_failed_probe_as_unusable() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let runner = fake_ffmpeg(ENCODERS_WITH_NVENC, FFMPEG_VERSION, false, calls.clone());
        let path = store_path("probe_failed");

        let resolution = resolve_with(Some(&path), false, runner).unwrap();
        assert_eq!(resolution.support.len(), 1);
        assert!(!resolution.support[0].usable);
        assert!(resolution.support[0]
            .reason
            .as_deref()
            .unwrap_or_default()
            .contains("No NVENC capable devices found"));
    }

    #[test]
    fn report_lists_usable_and_unusable_encoders() {
        // The wording is the shared one (`NvencCapability::lines`) --
        // the CLI report and the UI list must not drift apart.
        let text = report(&sample_record());
        assert!(text.contains("h264_nvenc: usable"), "{text}");
        assert!(text.contains("av1_nvenc: unusable — "), "{text}");
        assert!(text.contains("No NVENC capable devices found"));
    }
}
