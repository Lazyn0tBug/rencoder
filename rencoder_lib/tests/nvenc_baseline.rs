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

/// The verdict for `encoder` in a support list.
///
/// Every NVENC candidate is listed, whatever the machine looks like --
/// an encoder the build lacks is there too, marked unusable with the
/// build's absence as the reason (design doc §3.10.5).
fn entry<'a>(support: &'a [NvencSupport], encoder: &str) -> &'a NvencSupport {
    support
        .iter()
        .find(|s| s.encoder == encoder)
        .unwrap_or_else(|| panic!("{encoder} must be listed"))
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
    // One entry per candidate: the probed one, and the two this build
    // does not have.
    assert_eq!(resolution.support.len(), 3);
    let av1 = entry(&resolution.support, "av1_nvenc");
    assert!(av1.usable);
    assert!(entry(&resolution.support, "h264_nvenc").is_build_absent());
    assert!(entry(&resolution.support, "hevc_nvenc").is_build_absent());
    assert!(!resolution.review_due, "a fresh probe is never due");

    // The record is on disk, bound to this ffmpeg build.
    let stored = nvenc::load(&path).expect("the record must be stored");
    assert_eq!(stored.ffmpeg_version, FFMPEG_VERSION);
    assert_eq!(stored.encoders.len(), 3);
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
    assert_eq!(resolution.support.len(), 3);
    assert!(entry(&resolution.support, "av1_nvenc").usable);
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
fn a_build_without_nvenc_is_answered_without_any_hardware_check() {
    // Level 1 (does this ffmpeg build have NVENC at all?) decides it.
    // A build without the encoders can never run them, so no 1-frame
    // probe may happen -- on this machine the answer ends here.
    let dir = TempDir::new("nvenc_build_without_nvenc");
    let path = dir.file(nvenc::CAPABILITY_FILE_NAME);
    let clock = Clock::starting_now();
    let ffmpeg = ScriptedFfmpeg::new()
        .encoders("Encoders:\n V..... libsvtav1  libsvtav1 SVT-AV1\n")
        .version(FFMPEG_VERSION);

    let resolution =
        nvenc::resolve_and_store_with_clock(Some(&path), false, ffmpeg.runner(), clock.now())
            .expect("a build without NVENC is not an error");

    // Every candidate is listed, and every one of them failed the test
    // for the same reason: the build does not have it.
    assert_eq!(resolution.support.len(), 3);
    for encoder in ["h264_nvenc", "hevc_nvenc", "av1_nvenc"] {
        let verdict = entry(&resolution.support, encoder);
        assert!(!verdict.usable, "{encoder}");
        assert!(verdict.is_build_absent(), "{encoder}: {verdict:?}");
    }
    assert_eq!(
        ffmpeg.calls_containing("-c:v").len(),
        0,
        "level 1 decides it -- no hardware probe may run: {:?}",
        ffmpeg.calls()
    );
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

    assert_eq!(resolution.support.len(), 3);
    // The probed encoder failed the test -- with ffmpeg's reason.
    let av1 = entry(&resolution.support, "av1_nvenc");
    assert!(!av1.usable);
    assert!(!av1.is_build_absent());
    assert!(
        av1.reason
            .as_deref()
            .unwrap_or_default()
            .contains("No NVENC capable devices found"),
        "the reason must survive into the record: {av1:?}"
    );
    // The encoders the build does not have failed for a different
    // reason: the build, not the machine.
    assert!(entry(&resolution.support, "h264_nvenc").is_build_absent());
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

#[test]
fn the_lib_owns_the_review_cycle() {
    // The review cycle is the lib's: resolving a due record pushes its
    // review date and writes it back, so the advisory is shown once
    // and the next run is quiet. The frontends only display it -- there
    // is no state machine in them (design doc §2).
    let dir = TempDir::new("nvenc_review_cycle");
    let path = dir.file(nvenc::CAPABILITY_FILE_NAME);
    let clock = Clock::starting_now();

    let first = working_machine();
    nvenc::resolve_and_store_with_clock(Some(&path), false, first.runner(), clock.now())
        .expect("the first run must resolve");

    // Thirty-one days later: the record is due.
    clock.advance(chrono::Duration::days(31));
    let due_at = nvenc::load(&path)
        .expect("the record must be there")
        .next_review_at;

    // The next run reports the advisory and pushes the date.
    let second = working_machine();
    let resolution =
        nvenc::resolve_and_store_with_clock(Some(&path), false, second.runner(), clock.now())
            .expect("the second run must resolve");
    assert!(resolution.review_due, "the record is due");
    assert!(
        nvenc::load(&path)
            .expect("the record must be there")
            .next_review_at
            > clock.now() + chrono::Duration::days(29),
        "the review date must be pushed ~30 days out"
    );
    assert_ne!(
        nvenc::load(&path)
            .expect("the record must be there")
            .next_review_at,
        due_at,
        "the date must have moved"
    );

    // And the run after that is quiet again -- the push bought 30
    // days from the moment it happened (which was 31 days ahead of
    // the first run's clock), so 29 more days is still inside it.
    clock.advance(chrono::Duration::days(29));
    let third = working_machine();
    let resolution =
        nvenc::resolve_and_store_with_clock(Some(&path), false, third.runner(), clock.now())
            .expect("the third run must resolve");
    assert!(
        !resolution.review_due,
        "the advisory must not fire twice in a row"
    );
}

// ---- encoder selection ----

fn cache() -> EncoderCache {
    EncoderCache {
        encoders_output: ENCODERS_WITH_NVENC.to_string(),
    }
}

/// The capability of a machine that can (`usable`) or cannot run
/// av1_nvenc.
fn capability_with(usable: bool) -> nvenc::Capability {
    nvenc::Capability::from_support(vec![NvencSupport {
        encoder: "av1_nvenc".to_string(),
        usable,
        reason: if usable {
            None
        } else {
            Some("No NVENC capable devices found".to_string())
        },
    }])
}

#[test]
fn the_software_encoder_is_chosen_when_nv_is_off() {
    // The machine could do hardware -- but this run did not ask for it.
    let cache = cache();
    let params = EncodeParams {
        nv: false,
        ..EncodeParams::default()
    };

    let chosen = select_encoder(VideoFormat::Av1, &cache, &params, &capability_with(true))
        .expect("an encoder must be chosen");

    assert_eq!(chosen, "libsvtav1", "nv off -> software encoding");
}

#[test]
fn the_software_encoder_is_chosen_when_the_nvenc_encoder_is_unusable() {
    // The run asked for hardware; the machine cannot deliver it.
    let cache = cache();
    let params = EncodeParams {
        nv: true,
        ..EncodeParams::default()
    };

    let chosen = select_encoder(VideoFormat::Av1, &cache, &params, &capability_with(false))
        .expect("an encoder must be chosen");

    assert_eq!(
        chosen, "libsvtav1",
        "an unusable NVENC encoder falls back to software"
    );
}

#[test]
fn the_nvenc_encoder_is_chosen_when_nv_is_on_and_the_machine_can_run_it() {
    let cache = cache();
    let params = EncodeParams {
        nv: true,
        ..EncodeParams::default()
    };

    let chosen = select_encoder(VideoFormat::Av1, &cache, &params, &capability_with(true))
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
fn the_real_build_answers_level_one() {
    if !ffmpeg_available() {
        eprintln!("skipping: ffmpeg not available");
        return;
    }

    let encoders_output = real_ffmpeg(&["-encoders"]);
    let support = nvenc::probe(&encoders_output);

    // Every candidate is listed. The verdict must agree with the
    // build: an encoder the build lacks is unusable with the build's
    // absence as the reason; one the build has was really probed.
    assert_eq!(support.len(), 3);
    for encoder in ["h264_nvenc", "hevc_nvenc", "av1_nvenc"] {
        let in_build = encoders_output.contains(encoder);
        let verdict = entry(&support, encoder);
        if in_build {
            assert!(
                !verdict.is_build_absent(),
                "{encoder}: the build has it, so it was probed, not skipped"
            );
            if !verdict.usable {
                assert!(
                    verdict
                        .reason
                        .as_deref()
                        .is_some_and(|r| !r.trim().is_empty()),
                    "{encoder}: an unusable encoder must carry ffmpeg's reason: {verdict:?}"
                );
            }
        } else {
            assert!(
                verdict.is_build_absent(),
                "{encoder}: the build does not have it, so the verdict must say so: {verdict:?}"
            );
        }
    }
}

#[test]
fn the_real_machine_answers_level_two() {
    if !ffmpeg_available() {
        eprintln!("skipping: ffmpeg not available");
        return;
    }

    let encoders_output = real_ffmpeg(&["-encoders"]);
    let support = nvenc::probe(&encoders_output);
    let cache = EncoderCache {
        encoders_output: encoders_output.clone(),
    };
    let capability = nvenc::Capability::from_support(support);
    let params = EncodeParams {
        nv: true,
        ..EncodeParams::default()
    };
    // The premise of this test is "**an** AV1 encoder can be chosen" --
    // the hardware one when it runs here, the software one otherwise.
    //
    // **Listed is not usable**: the CI hosts' ffmpeg builds *list*
    // av1_nvenc (the encoder is compiled in) but the machine has no
    // NVIDIA GPU, so the probe marks it unusable; and the package
    // builds have no libsvtav1 either. Then there is nothing to
    // choose and the run would fail with "no AV1 encoder available in
    // this ffmpeg build" (design doc §8 row 79 -- a missing encoder
    // is a per-file failure, not a panic). Skip rather than assert on
    // a premise that does not hold -- and the premise is exactly what
    // `select_encoder` decides below.
    let can_choose = capability.usable("av1_nvenc") || cache.has_encoder("libsvtav1");
    if !can_choose {
        eprintln!("skipping: this machine has no usable AV1 encoder");
        return;
    }

    let chosen = select_encoder(VideoFormat::Av1, &cache, &params, &capability)
        .expect("an encoder must be chosen");

    // The machine either can run AV1 in hardware, or it cannot -- and
    // then the software encoder is what the run has to use. What it
    // must never be is an NVENC encoder the probe rejected.
    if capability.usable("av1_nvenc") {
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
/// The two-level model says where the device belongs: level 1 (does
/// the build have NVENC?) is a property of the build alone, and the
/// key already covers it; level 2 (can this machine run it?) is a
/// property of build **and** device, so the key needs the device --
/// but only when level 1 passed. A build without NVENC needs no device
/// information at all: its answer can never change.
///
/// The production side does not exist yet -- `nvenc::fingerprint` hashes
/// the version string and the encoder list, nothing about the device.
/// The test is kept (and ignored) so the expectation is on record and
/// the step-3 work has a target to satisfy.
///
/// TODO(step 3): let the fixture script the device/driver information
/// the probe reads, store a record for device A, then resolve again
/// with device B scripted -- and assert that the record is re-probed
/// instead of reused. The counterpart -- a build without NVENC keeps
/// reusing its record even when the device information changes -- is
/// already covered by
/// `a_build_without_nvenc_is_answered_without_any_hardware_check`.
#[test]
#[ignore = "needs the device-aware capability cache key from §3.10.6 step 3"]
fn the_capability_cache_key_changes_with_the_device() {
    todo!("implement together with the device-aware cache key");
}
