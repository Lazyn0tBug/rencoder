//! Baseline behaviours of the NVENC capability flow
//! (design doc §3.10.6, step 1).
//!
//! These tests pin down what the flow does **today**, driven through
//! the production entry points with the ffmpeg answers scripted
//! (`rencoder_lib::test_support`). They are the baseline the later
//! steps -- config resolution, capability refresh, probe generations --
//! change against: a behaviour change has to move one of these
//! assertions on purpose, not by accident.
//!
//! Nothing here re-implements the flow. The fixture only decides what
//! ffmpeg answers; the resolution, the record and the encoder choice
//! are all the production code's (design doc §3.10.4 rule 2).

use rencoder_lib::encoder::nvenc;
use rencoder_lib::encoder::nvenc::CapabilityVerdicts;
use rencoder_lib::encoder::select::select_encoder;
use rencoder_lib::test_support::{Clock, ScriptedFfmpeg, TempDir};
use rencoder_lib::types::{EncodeParams, EncoderCache, NvencSupport, VideoFormat};

/// An ffmpeg build with AV1 (software + NVENC) compiled in.
const ENCODERS_WITH_NVENC: &str = "\
Encoders:
 V..... libsvtav1            libsvtav1 SVT-AV1
 V..... av1_nvenc            NVIDIA NVENC AV1 encoder
";

const FFMPEG_VERSION: &str = "ffmpeg version 9.0.1 Copyright (c) 2000-2026";

/// The scripted ffmpeg of a machine where everything works.
fn working_machine() -> ScriptedFfmpeg {
    ScriptedFfmpeg::new()
        .encoders(ENCODERS_WITH_NVENC)
        .version(FFMPEG_VERSION)
        .probe_ok("av1_nvenc")
}

/// The scripted ffmpeg of a machine without a usable NVENC device.
fn machine_without_nvenc() -> ScriptedFfmpeg {
    ScriptedFfmpeg::new()
        .encoders(ENCODERS_WITH_NVENC)
        .version(FFMPEG_VERSION)
        .probe_fail("av1_nvenc", "No NVENC capable devices found")
}

// ---- the capability record ----

#[test]
fn the_first_run_probes_the_machine_and_stores_the_record() {
    let dir = TempDir::new("nvenc_first_run");
    let path = dir.file(nvenc::CAPABILITY_FILE_NAME);
    let clock = Clock::starting_now();
    let ffmpeg = working_machine();

    let resolution =
        nvenc::resolve_and_store_with_clock(Some(&path), false, ffmpeg.runner(), clock.now())
            .expect("the first run must resolve");

    assert!(
        resolution.probed,
        "no record yet -> the machine must be probed"
    );
    assert_eq!(resolution.support.len(), 1);
    assert_eq!(resolution.support[0].encoder, "av1_nvenc");
    assert!(resolution.support[0].usable);
    assert!(!resolution.review_due, "a fresh probe is never due");

    // The record is on disk, bound to this ffmpeg build.
    let stored = nvenc::load(&path).expect("the record must be stored");
    assert_eq!(stored.ffmpeg_version, FFMPEG_VERSION);
    assert_eq!(stored.encoders.len(), 1);
    assert_eq!(
        stored.ffmpeg_fingerprint,
        nvenc::fingerprint(FFMPEG_VERSION, ENCODERS_WITH_NVENC),
        "the record must be bound to the build it was made with"
    );
}

#[test]
fn the_second_run_reuses_the_record_without_probing() {
    let dir = TempDir::new("nvenc_second_run");
    let path = dir.file(nvenc::CAPABILITY_FILE_NAME);
    let clock = Clock::starting_now();

    let first = working_machine();
    nvenc::resolve_and_store_with_clock(Some(&path), false, first.runner(), clock.now())
        .expect("the first run must resolve");

    // Same ffmpeg build, same machine: the stored answer is reused.
    let second = working_machine();
    let resolution =
        nvenc::resolve_and_store_with_clock(Some(&path), false, second.runner(), clock.now())
            .expect("the second run must resolve");

    assert!(!resolution.probed, "a fresh record must be reused");
    assert_eq!(
        second.calls_containing("-c:v").len(),
        0,
        "a reused record must not trigger a probe: {:?}",
        second.calls()
    );
    // The stored answer comes back unchanged.
    assert_eq!(resolution.support.len(), 1);
    assert!(resolution.support[0].usable);
}

#[test]
fn a_record_from_another_ffmpeg_build_is_not_reused() {
    let dir = TempDir::new("nvenc_other_build");
    let path = dir.file(nvenc::CAPABILITY_FILE_NAME);
    let clock = Clock::starting_now();

    // A record made with a different ffmpeg build.
    let other_build = ScriptedFfmpeg::new()
        .encoders(ENCODERS_WITH_NVENC)
        .version("ffmpeg version 8.0.0")
        .probe_ok("av1_nvenc");
    nvenc::resolve_and_store_with_clock(Some(&path), false, other_build.runner(), clock.now())
        .expect("the first run must resolve");

    // This machine runs a different ffmpeg: the record does not apply.
    let current = working_machine();
    let resolution =
        nvenc::resolve_and_store_with_clock(Some(&path), false, current.runner(), clock.now())
            .expect("the second run must resolve");

    assert!(
        resolution.probed,
        "a record from another build must be re-probed"
    );
    assert_eq!(current.calls_containing("-c:v").len(), 1);
}

#[test]
fn a_failed_probe_marks_the_encoder_unusable_with_the_reason() {
    let dir = TempDir::new("nvenc_probe_failed");
    let path = dir.file(nvenc::CAPABILITY_FILE_NAME);
    let clock = Clock::starting_now();
    let ffmpeg = machine_without_nvenc();

    let resolution =
        nvenc::resolve_and_store_with_clock(Some(&path), false, ffmpeg.runner(), clock.now())
            .expect("a failing probe is not an error");

    assert_eq!(resolution.support.len(), 1);
    assert!(!resolution.support[0].usable);
    assert!(
        resolution.support[0]
            .reason
            .as_deref()
            .unwrap_or_default()
            .contains("No NVENC capable devices found"),
        "the reason must survive into the record: {:?}",
        resolution.support[0]
    );
}

#[test]
fn an_expired_record_is_reused_but_flagged_for_review() {
    let dir = TempDir::new("nvenc_expired");
    let path = dir.file(nvenc::CAPABILITY_FILE_NAME);
    let clock = Clock::starting_now();

    let first = working_machine();
    nvenc::resolve_and_store_with_clock(Some(&path), false, first.runner(), clock.now())
        .expect("the first run must resolve");

    // Thirty-one days later: the record is stale but still in use.
    clock.advance(chrono::Duration::days(31));

    let second = working_machine();
    let resolution =
        nvenc::resolve_and_store_with_clock(Some(&path), false, second.runner(), clock.now())
            .expect("an expired record is still usable");

    assert!(
        !resolution.probed,
        "an expired record is reused -- the advisory is not an expiry"
    );
    assert!(resolution.review_due, "an expired record must be flagged");
    assert_eq!(
        second.calls_containing("-c:v").len(),
        0,
        "an expired record must not trigger a probe"
    );
}

// ---- encoder selection ----

fn cache_with(usable: bool) -> EncoderCache {
    EncoderCache {
        encoders_output: ENCODERS_WITH_NVENC.to_string(),
        nvenc: vec![NvencSupport {
            encoder: "av1_nvenc".to_string(),
            usable,
            reason: if usable {
                None
            } else {
                Some("No NVENC capable devices found".to_string())
            },
        }],
    }
}

#[test]
fn the_software_encoder_is_chosen_when_nv_is_off() {
    // The machine could do hardware -- but this run did not ask for it.
    let cache = cache_with(true);
    let params = EncodeParams {
        nv: false,
        ..EncodeParams::default()
    };

    let chosen = select_encoder(
        VideoFormat::Av1,
        &cache,
        &params,
        &CapabilityVerdicts::default(),
    )
    .expect("an encoder must be chosen");

    assert_eq!(chosen, "libsvtav1", "nv off -> software encoding");
}

#[test]
fn the_software_encoder_is_chosen_when_the_nvenc_encoder_is_unusable() {
    // The run asked for hardware; the machine cannot deliver it.
    let cache = cache_with(false);
    let params = EncodeParams {
        nv: true,
        ..EncodeParams::default()
    };

    let chosen = select_encoder(
        VideoFormat::Av1,
        &cache,
        &params,
        &CapabilityVerdicts::default(),
    )
    .expect("an encoder must be chosen");

    assert_eq!(
        chosen, "libsvtav1",
        "an unusable NVENC encoder falls back to software"
    );
}

#[test]
fn the_nvenc_encoder_is_chosen_when_nv_is_on_and_the_machine_can_run_it() {
    let cache = cache_with(true);
    let params = EncodeParams {
        nv: true,
        ..EncodeParams::default()
    };

    let chosen = select_encoder(
        VideoFormat::Av1,
        &cache,
        &params,
        &CapabilityVerdicts::default(),
    )
    .expect("an encoder must be chosen");

    assert_eq!(chosen, "av1_nvenc");
}

// ---- the real machine ----

// The real ffmpeg, no scripted answers.
//
// These are the only tests that exercise the production subprocess
// path end to end: the probe's exit-code handling, the stderr
// extraction, and the rule "an encoder the build does not have is not
// probed at all".
//
// The expectation is derived from the build, so the tests hold on
// every machine -- a laptop without an NVIDIA GPU and a workstation
// with one both have to agree with their own ffmpeg. That is what
// makes them usable as the cross-platform check: run them on the
// Windows host and on the development machine, and both must pass.

fn ffmpeg_available() -> bool {
    std::process::Command::new("ffmpeg")
        .arg("-version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn real_ffmpeg(args: &[&str]) -> String {
    let output = std::process::Command::new("ffmpeg")
        .args(args)
        .output()
        .expect("ffmpeg spawn failed");
    assert!(
        output.status.success(),
        "ffmpeg {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}

#[test]
fn the_real_probe_agrees_with_the_build() {
    if !ffmpeg_available() {
        eprintln!("skipping: ffmpeg not available");
        return;
    }

    let encoders_output = real_ffmpeg(&["-encoders"]);
    let support = nvenc::probe(&encoders_output);

    for encoder in ["h264_nvenc", "hevc_nvenc", "av1_nvenc"] {
        let in_build = encoders_output.contains(encoder);
        let entry = support.iter().find(|s| s.encoder == encoder);
        assert_eq!(
            entry.is_some(),
            in_build,
            "{encoder}: the probe must report exactly the encoders this build has \
             (build says {in_build})"
        );
        if let Some(entry) = entry {
            if !entry.usable {
                assert!(
                    entry
                        .reason
                        .as_deref()
                        .is_some_and(|r| !r.trim().is_empty()),
                    "{encoder}: an unusable encoder must carry ffmpeg's reason: {:?}",
                    entry
                );
            }
        }
    }
}

#[test]
fn the_real_run_picks_an_encoder_the_machine_can_actually_run() {
    if !ffmpeg_available() {
        eprintln!("skipping: ffmpeg not available");
        return;
    }

    let encoders_output = real_ffmpeg(&["-encoders"]);
    let support = nvenc::probe(&encoders_output);
    let cache = EncoderCache {
        encoders_output: encoders_output.clone(),
        nvenc: support,
    };
    let params = EncodeParams {
        nv: true,
        ..EncodeParams::default()
    };

    let chosen = select_encoder(
        VideoFormat::Av1,
        &cache,
        &params,
        &CapabilityVerdicts::default(),
    )
    .expect("an encoder must be chosen");

    // The machine either can run AV1 in hardware, or it cannot -- and
    // then the software encoder is what the run has to use. What it
    // must never be is an NVENC encoder the probe rejected.
    if cache.nvenc_usable("av1_nvenc") {
        assert_eq!(chosen, "av1_nvenc");
    } else {
        assert_eq!(
            chosen, "libsvtav1",
            "a machine that cannot run av1_nvenc must fall back to software"
        );
    }
}

// ---- scenarios the later steps will satisfy ----

/// Regression scenario for §3.10.6 step 3: the capability cache key
/// must bind to the **device**, not only to the ffmpeg build. Swapping
/// the GPU has to invalidate the stored record, otherwise a machine
/// that changed hardware keeps answering with the old machine's
/// capabilities.
///
/// The production side does not exist yet -- `nvenc::fingerprint` hashes
/// the version string and the encoder list, nothing about the device.
/// The test is kept (and ignored) so the expectation is on record and
/// the step-3 work has a target to satisfy.
///
/// TODO(step 3): let the fixture script the device/driver information
/// the probe reads, store a record for device A, then resolve again
/// with device B scripted -- and assert that the record is re-probed
/// instead of reused.
#[test]
#[ignore = "needs the device-aware capability cache key from §3.10.6 step 3"]
fn the_capability_cache_key_changes_with_the_device() {
    todo!("implement together with the device-aware cache key");
}
