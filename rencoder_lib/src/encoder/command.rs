//! FFmpeg command construction.
//!
//! Two responsibilities:
//! 1. `should_skip_encoding` -- decide whether a source codec matches the
//!    target format (and whether we should still transcode for fps reduction).
//! 2. `build_ffmpeg_command` -- assemble argv, including the `-r <fps>`
//!    output option placement.
//!
//! `-r` is an FFmpeg **output option**: it must appear AFTER `-i <input>`
//! and BEFORE the output path. Placing it before `-i` would force the input
//! frame rate (origin §3.3 + R4). When `src_fps = None`, no `-r` is injected
//! even if capping is enabled (R21): we'd rather not transcode with a
//! guessed frame rate than corrupt the output.

use crate::config::app::FfmpegArgsConfig;
use crate::types::{EncodeParams, TranscodeTask, VideoFormat};

/// Decide whether the source already matches the target encoding and
/// no fps reduction is needed, in which case the file can be skipped.
///
/// Decision table (origin §3.3):
/// - codec mismatch: transcode (returns false)
/// - codec match, no fps reduction needed: skip (returns true)
/// - codec match, fps reduction needed: transcode (returns false)
pub fn should_skip_encoding(codec: &str, format: VideoFormat, need_reduce: bool) -> bool {
    if need_reduce {
        return false;
    }
    codec_matches_format(codec, format)
}

/// Substring match between a probe-style codec name and a target format.
/// Handles both short names ("h264", "av1") and encoder names
/// ("libx264", "libsvtav1").
fn codec_matches_format(codec: &str, format: VideoFormat) -> bool {
    let c = codec.to_ascii_lowercase();
    match format {
        VideoFormat::X264 => c.contains("h264") || c.contains("x264"),
        VideoFormat::Hevc => c.contains("hevc") || c.contains("h265") || c.contains("x265"),
        VideoFormat::Vp9 => c.contains("vp9"),
        VideoFormat::Av1 => c.contains("av1"),
    }
}

/// Check that an `extra_args` entry is safe to pass verbatim to ffmpeg
/// argv. The user-controlled value (from `rencoder.toml` `[encode]
/// extra_args` or future CLI passthrough) is interpolated by
/// `build_ffmpeg_command` into the argv AFTER our own hardcoded
/// flags, so a malicious value could re-inject an ffmpeg option
/// (`-i /etc/passwd`, `-vf scale=720:foo bar` with whitespace,
/// `; rm -rf /` for a shell-style argument, etc.).
///
/// Each entry must be one of:
///   1. A pure flag — `-` + ASCII alphanumeric (e.g., `-y`, `-vf`,
///      `-metadata`, `-ss`).
///   2. A flag with `=value` — flag + `=` + value where value is
///      ASCII alphanumeric + `. : / - _ @` (rules out shell
///      metachars, whitespace, quotes, semicolons, etc.).
///   3. A bare value passed to a preceding flag. Same value
///      charset as (2), but no `-` prefix, AND containing `=`
///      (the canonical `key=value` form ffmpeg uses for metadata
///      and filter options). Covers entries like the value half
///      of `-metadata title=foo`. Bare values without `=` such
///      as `preset` or `5` are rejected because they could be
///      parsed by ffmpeg as input paths or filter args depending
///      on context.
///
/// Rejected:
///   - long options like `--foo` (too varied for a simple allowlist)
///   - flag-without-`=` followed by path-like chars (e.g.,
///     `-vf_negate.mp4` could be parsed by lenient ffmpeg as
///     `-vf negate.mp4` — ambiguous; reject)
///   - flag on the explicit deny-list: `-i` (re-inject input),
///     `-c:v` / `-c:a` (duplicate our hardcoded codec), `-o`
///     (re-inject output), `-y` (our hardcoded overwrite)
pub fn is_safe_extra_arg(arg: &str) -> bool {
    if let Some(rest) = arg.strip_prefix('-') {
        if rest.is_empty() || rest.starts_with('-') {
            return false; // "--foo" disallowed; we don't allow long opts
        }
        let mut chars = rest.chars();
        let first = chars.next().expect("non-empty by guard above");
        if !first.is_ascii_alphanumeric() {
            return false; // "-;" "-)" etc.
        }
        // Split on the first `=`: flag-name (must be alphanumeric) and
        // value (must be safe if present). This rejects `-vf_negate.mp4`
        // (no `=`, so the path chars become part of the value-less form
        // which the allowlist forbids) while still allowing `-vf` and
        // `-vf=scale=720:-1`.
        let (flag, value) = match rest.find('=') {
            Some(i) => (&rest[..i], &rest[i + 1..]),
            None => (rest, ""),
        };
        if !flag.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
            return false;
        }
        if !value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | ':' | '/' | '-' | '_' | '@'))
        {
            return false;
        }
        // Deny-list: these flags would re-inject paths or duplicate
        // hardcoded wrappers. A user wanting to override them should
        // edit the lib's source (which is the design intent — the
        // `extra_args` field is for *additional* ffmpeg knobs, not
        // for re-overriding our wrapper-level decisions).
        return !matches!(flag, "i" | "y" | "o" | "c" | "vn" | "hide_banner");
    }
    // Bare value: the value half of a preceding flag like
    // `-metadata title=foo` or `-vf scale=720:-1`. Must contain
    // `=` (the canonical `key=value` form ffmpeg uses for
    // metadata + filter options) and otherwise stick to a safe
    // charset. Bare values without `=` (`preset`, `5`) are
    // rejected: they could be parsed by ffmpeg as input paths or
    // filter args depending on context, and the user's intent
    // is ambiguous at the wrapper level.
    if arg.is_empty() || !arg.contains('=') {
        return false;
    }
    arg.chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | ':' | '/' | '-' | '_' | '@' | '='))
}

/// Validate a full `extra_args` slice. Returns the index of the
/// first invalid entry, or `None` if all are safe.
pub fn first_unsafe_extra_arg(args: &[String]) -> Option<usize> {
    args.iter().position(|a| !is_safe_extra_arg(a))
}

/// Build the argv for an `ffmpeg` invocation.
///
/// Layout:
/// ```text
/// ffmpeg <pre_input...> -i <input>
///   <post_input...>
///   -c:v <encoder> [-preset <default_preset>] [-crf ... | -b:v ...]
///   -c:a <default_audio_codec> -b:a <default_audio_bitrate>
///   [-r <MAX_FPS>]              # only when src_fps is Some AND need_reduce
///   [... extra_args]
///   <output>
/// ```
///
/// `pre_input` / `post_input` / `default_preset` / `default_audio_codec`
/// / `default_audio_bitrate` come from `EncodeConfig.ffmpeg_args`
/// (review fix #1: most flags moved out of Rust concatenation so the
/// only Rust-built strings in the argv are `ffmpeg`, `-y`, `-i`, the
/// input path, and the output path). `-y` is the one exception: it's
/// always present (never want to prompt) and is the safest flag to
/// keep Rust-side since it's the most important behavior we don't want
/// users to accidentally disable.
///
/// The `-r` block is the single most error-prone position; tests assert
/// argv ordering by index, not just presence.
pub fn build_ffmpeg_command(
    task: &TranscodeTask,
    params: &EncodeParams,
    encoder: &str,
    src_fps: Option<f64>,
    ffmpeg_args: &FfmpegArgsConfig,
) -> Vec<String> {
    let mut argv: Vec<String> = Vec::with_capacity(16);

    argv.push("ffmpeg".to_string());
    // `-y` is hardcoded (never user-overridable) so a user setting
    // a bad config can't accidentally get ffmpeg to prompt.
    argv.push("-y".to_string());
    argv.extend(ffmpeg_args.pre_input.iter().cloned());
    argv.push("-i".to_string());
    argv.push(task.input.to_string_lossy().into_owned());
    argv.extend(ffmpeg_args.post_input.iter().cloned());

    argv.push("-c:v".to_string());
    argv.push(encoder.to_string());

    // Rate control
    if params.is_crf {
        // `-preset` is libsvtav1-specific. libaom-av1 uses `-cpu-used`;
        // av1_nvenc ignores `-preset` (and rejects it on some ffmpeg
        // builds). Gate on the encoder name so non-svt AV1 overrides
        // produce a clean command.
        if matches!(params.format, VideoFormat::Av1) && encoder.contains("svt") {
            let preset = ffmpeg_args.default_preset.as_deref().unwrap_or("6");
            argv.push("-preset".to_string());
            argv.push(preset.to_string());
        }
        argv.push("-crf".to_string());
        argv.push(params.bitrate_or_crf.to_string());
    } else {
        argv.push("-b:v".to_string());
        argv.push(format!("{}k", params.bitrate_or_crf));
    }

    // Audio: passthrough encode as libopus by default; users
    // override via `ffmpeg_args.default_audio_codec` / `default_audio_bitrate`
    // or via `params.extra_args` if they need a specific codec.
    if let Some(codec) = &ffmpeg_args.default_audio_codec {
        argv.push("-c:a".to_string());
        argv.push(codec.clone());
    }
    if let Some(bitrate) = &ffmpeg_args.default_audio_bitrate {
        argv.push("-b:a".to_string());
        argv.push(bitrate.clone());
    }

    // Frame-rate cap (R4 + R21): only injected when we have a real source
    // fps AND the cap applies to it.
    if let Some(src) = src_fps {
        if params.max_fps.enabled() && params.max_fps.need_reduce(src) {
            argv.push("-r".to_string());
            argv.push(params.max_fps.max_fps.to_string());
        }
    }

    // User passthrough args.
    argv.extend(params.extra_args.iter().cloned());

    // Output path always last.
    argv.push(
        task.output
            .as_ref()
            .expect(
                "encoder::command::build_ffmpeg_command called with TranscodeTask.output = None",
            )
            .to_string_lossy()
            .into_owned(),
    );

    argv
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::MaxFpsConfig;
    use std::path::PathBuf;

    fn task(input: &str, output: &str) -> TranscodeTask {
        TranscodeTask {
            input: PathBuf::from(input),
            output: Some(PathBuf::from(output)),
        }
    }

    fn params_disabled() -> EncodeParams {
        EncodeParams {
            is_crf: true,
            bitrate_or_crf: 28,
            max_fps: MaxFpsConfig::default(), // disabled
            ..Default::default()
        }
    }

    // ---- should_skip_encoding ----

    #[test]
    fn skip_when_codec_matches_and_no_reduce() {
        assert!(should_skip_encoding("h264", VideoFormat::X264, false));
        assert!(should_skip_encoding("hevc", VideoFormat::Hevc, false));
        assert!(should_skip_encoding("vp9", VideoFormat::Vp9, false));
        assert!(should_skip_encoding("av1", VideoFormat::Av1, false));
    }

    #[test]
    fn do_not_skip_when_codec_doesnt_match() {
        assert!(!should_skip_encoding("h264", VideoFormat::Av1, false));
        assert!(!should_skip_encoding("vp9", VideoFormat::X264, false));
        assert!(!should_skip_encoding("av1", VideoFormat::Vp9, false));
        assert!(!should_skip_encoding("hevc", VideoFormat::X264, false));
    }

    #[test]
    fn do_not_skip_when_need_reduce_even_if_codec_matches() {
        // h264 source at 60fps with X264 target and cap=30 -> transcode to reduce.
        assert!(!should_skip_encoding("h264", VideoFormat::X264, true));
        assert!(!should_skip_encoding("av1", VideoFormat::Av1, true));
    }

    #[test]
    fn empty_codec_does_not_match_anything() {
        assert!(!should_skip_encoding("", VideoFormat::X264, false));
        assert!(!should_skip_encoding("", VideoFormat::Av1, false));
    }

    #[test]
    fn encoder_name_aliases_match() {
        // ffmpeg sometimes reports the encoder name rather than the codec.
        assert!(should_skip_encoding("libx264", VideoFormat::X264, false));
        assert!(should_skip_encoding("libx265", VideoFormat::Hevc, false));
        assert!(should_skip_encoding("libvpx-vp9", VideoFormat::Vp9, false));
        assert!(should_skip_encoding("libsvtav1", VideoFormat::Av1, false));
    }

    #[test]
    fn hevc_aliases_h265_and_x265() {
        assert!(should_skip_encoding("h265", VideoFormat::Hevc, false));
        assert!(should_skip_encoding("hevc", VideoFormat::Hevc, false));
        assert!(should_skip_encoding("libx265", VideoFormat::Hevc, false));
    }

    #[test]
    fn vp8_does_not_match_vp9() {
        // Guard against over-broad substring matching.
        assert!(!should_skip_encoding("vp8", VideoFormat::Vp9, false));
        assert!(!should_skip_encoding("libvpx-vp8", VideoFormat::Vp9, false));
    }

    // ---- build_ffmpeg_command ----

    #[test]
    fn command_starts_with_ffmpeg_y_i_input() {
        let argv = build_ffmpeg_command(
            &task("/in.mp4", "/out.mkv"),
            &params_disabled(),
            "libx264",
            Some(30.0),
            &FfmpegArgsConfig::default(),
        );
        // argv[0] = "ffmpeg", argv[1] = "-y" (hardcoded). Then
        // FfmpegArgsConfig::default().pre_input (["-hide_banner",
        // "-loglevel", "info", "-nostats"]) sits between `-y` and `-i`.
        // Position-based lookups so a future pre_input change doesn't
        // break this assertion.
        assert_eq!(argv[0], "ffmpeg");
        assert_eq!(argv[1], "-y");
        let i_pos = argv.iter().position(|s| s == "-i").expect("-i present");
        assert_eq!(argv[i_pos + 1], "/in.mp4");
        let cv_pos = argv.iter().position(|s| s == "-c:v").expect("-c:v present");
        assert_eq!(argv[cv_pos + 1], "libx264");
    }

    #[test]
    fn output_path_is_last_arg() {
        let argv = build_ffmpeg_command(
            &task("/in.mp4", "/out.mkv"),
            &params_disabled(),
            "libx264",
            Some(30.0),
            &FfmpegArgsConfig::default(),
        );
        assert_eq!(argv.last().unwrap(), "/out.mkv");
    }

    #[test]
    fn r_placed_after_i_and_before_output() {
        // Critical assertion (R4): -r must be an output option.
        let mut params = params_disabled();
        params.max_fps = MaxFpsConfig {
            max_fps: 30.0,
            eps: 0.05,
        };
        let argv = build_ffmpeg_command(
            &task("/in.mp4", "/out.mkv"),
            &params,
            "libx264",
            Some(60.0),
            &FfmpegArgsConfig::default(),
        );
        let i_pos = argv.iter().position(|s| s == "-i").expect("-i present");
        let r_pos = argv.iter().position(|s| s == "-r").expect("-r present");
        let out_pos = argv.len() - 1;
        assert!(
            i_pos < r_pos,
            "-r must be after -i (got i_pos={i_pos}, r_pos={r_pos})"
        );
        assert!(
            r_pos < out_pos,
            "-r must be before output (got r_pos={r_pos}, out_pos={out_pos})"
        );
        assert_eq!(argv[r_pos + 1], "30");
    }

    #[test]
    fn no_r_when_src_fps_none() {
        // R21: src_fps=None means no -r even if cap is enabled.
        let mut params = params_disabled();
        params.max_fps = MaxFpsConfig {
            max_fps: 30.0,
            eps: 0.05,
        };
        let argv = build_ffmpeg_command(
            &task("/in.mp4", "/out.mkv"),
            &params,
            "libx264",
            None,
            &FfmpegArgsConfig::default(),
        );
        assert!(
            !argv.contains(&"-r".to_string()),
            "argv must not contain -r when src_fps is None"
        );
    }

    #[test]
    fn no_r_when_max_fps_disabled() {
        let params = params_disabled(); // max_fps = 0.0 (disabled)
        let argv = build_ffmpeg_command(
            &task("/in.mp4", "/out.mkv"),
            &params,
            "libx264",
            Some(60.0),
            &FfmpegArgsConfig::default(),
        );
        assert!(!argv.contains(&"-r".to_string()));
    }

    #[test]
    fn no_r_when_source_fps_within_cap() {
        let mut params = params_disabled();
        params.max_fps = MaxFpsConfig {
            max_fps: 30.0,
            eps: 0.05,
        };
        let argv = build_ffmpeg_command(
            &task("/in.mp4", "/out.mkv"),
            &params,
            "libx264",
            Some(25.0), // below cap
            &FfmpegArgsConfig::default(),
        );
        assert!(!argv.contains(&"-r".to_string()));
    }

    #[test]
    fn crf_mode_emits_crf_flag() {
        let mut params = params_disabled();
        params.bitrate_or_crf = 45;
        let argv = build_ffmpeg_command(
            &task("/in.mp4", "/out.mkv"),
            &params,
            "libsvtav1",
            None,
            &FfmpegArgsConfig::default(),
        );
        let crf_pos = argv.iter().position(|s| s == "-crf").expect("-crf present");
        assert_eq!(argv[crf_pos + 1], "45");
    }

    #[test]
    fn av1_crf_includes_preset_6() {
        let mut params = params_disabled();
        params.format = VideoFormat::Av1;
        params.bitrate_or_crf = 45;
        let argv = build_ffmpeg_command(
            &task("/in.mp4", "/out.mkv"),
            &params,
            "libsvtav1",
            None,
            &FfmpegArgsConfig::default(),
        );
        let preset_pos = argv
            .iter()
            .position(|s| s == "-preset")
            .expect("-preset present");
        assert_eq!(argv[preset_pos + 1], "6");
    }

    #[test]
    fn av1_crf_with_libaom_skips_preset() {
        // libaom-av1 expects -cpu-used, not -preset. build_ffmpeg_command
        // must not inject -preset when the encoder isn't libsvtav1.
        let mut params = params_disabled();
        params.format = VideoFormat::Av1;
        params.bitrate_or_crf = 45;
        let argv = build_ffmpeg_command(
            &task("/in.mp4", "/out.mkv"),
            &params,
            "libaom-av1",
            None,
            &FfmpegArgsConfig::default(),
        );
        assert!(!argv.contains(&"-preset".to_string()));
        assert!(argv.contains(&"-crf".to_string()));
    }

    #[test]
    fn av1_crf_with_av1_nvenc_skips_preset() {
        // NVENC doesn't accept -preset.
        let mut params = params_disabled();
        params.format = VideoFormat::Av1;
        params.bitrate_or_crf = 45;
        let argv = build_ffmpeg_command(
            &task("/in.mp4", "/out.mkv"),
            &params,
            "av1_nvenc",
            None,
            &FfmpegArgsConfig::default(),
        );
        assert!(!argv.contains(&"-preset".to_string()));
        assert!(argv.contains(&"-crf".to_string()));
    }

    #[test]
    fn bitrate_mode_emits_bv_flag_with_k_suffix() {
        let mut params = params_disabled();
        params.is_crf = false;
        params.bitrate_or_crf = 5000;
        let argv = build_ffmpeg_command(
            &task("/in.mp4", "/out.mkv"),
            &params,
            "libx264",
            None,
            &FfmpegArgsConfig::default(),
        );
        let bv_pos = argv.iter().position(|s| s == "-b:v").expect("-b:v present");
        assert_eq!(argv[bv_pos + 1], "5000k");
        assert!(!argv.contains(&"-crf".to_string()));
    }

    #[test]
    fn audio_defaults_to_opus_32k() {
        // Audio defaults to libopus @ 32k (transcode, not copy).
        // Users override via params.extra_args if they need a
        // different codec.
        let argv = build_ffmpeg_command(
            &task("/in.mp4", "/out.mkv"),
            &params_disabled(),
            "libx264",
            None,
            &FfmpegArgsConfig::default(),
        );
        let ca_pos = argv.iter().position(|s| s == "-c:a").expect("-c:a present");
        assert_eq!(argv[ca_pos + 1], "libopus");
        let ba_pos = argv.iter().position(|s| s == "-b:a").expect("-b:a present");
        assert_eq!(argv[ba_pos + 1], "32k");
    }

    #[test]
    fn extra_args_passed_through_before_output() {
        let mut params = params_disabled();
        params.extra_args = vec!["-ss".to_string(), "10".to_string(), "-metadata".to_string()];
        let argv = build_ffmpeg_command(
            &task("/in.mp4", "/out.mkv"),
            &params,
            "libx264",
            None,
            &FfmpegArgsConfig::default(),
        );
        let ss_pos = argv.iter().position(|s| s == "-ss").expect("-ss present");
        assert_eq!(argv[ss_pos + 1], "10");
        let out_pos = argv.len() - 1;
        // -ss should be before output (passed through near the end)
        assert!(ss_pos < out_pos);
    }

    #[test]
    fn extra_args_appear_after_r_block() {
        // -r is an output option; user extras should come AFTER it but BEFORE the
        // output path. (ffmpeg interprets positionally.)
        let mut params = params_disabled();
        params.max_fps = MaxFpsConfig {
            max_fps: 30.0,
            eps: 0.05,
        };
        params.extra_args = vec!["-metadata".to_string(), "title=foo".to_string()];
        let argv = build_ffmpeg_command(
            &task("/in.mp4", "/out.mkv"),
            &params,
            "libx264",
            Some(60.0),
            &FfmpegArgsConfig::default(),
        );
        let r_pos = argv.iter().position(|s| s == "-r").unwrap();
        let meta_pos = argv.iter().position(|s| s == "-metadata").unwrap();
        let out_pos = argv.len() - 1;
        assert!(r_pos < meta_pos);
        assert!(meta_pos < out_pos);
    }

    // ---- is_safe_extra_arg (review fix #15) ----

    #[test]
    fn safe_extra_arg_accepts_standard_ffmpeg_flags() {
        // The allowlist covers typical user tweaks. Note: -y is
        // hardcoded by build_ffmpeg_command and is on the deny
        // list, so it's NOT safe to override here.
        for arg in [
            "-n",        // no-overwrite
            "-ss",       // start time
            "-ss",       // (re-listed; the validator checks each entry)
            "-metadata", // metadata key=val
            "-metadata",
            "-vf",          // video filter
            "-vf",          // (re-listed; same as above)
            "-af",          // audio filter
            "-t",           // duration limit
            "-to",          // stop time
            "-pass",        // two-pass encoding
            "-passlogfile", // two-pass log path
        ] {
            assert!(is_safe_extra_arg(arg), "expected safe: {arg:?}");
        }
    }

    #[test]
    fn safe_extra_arg_rejects_dash_prefixed_values() {
        // Filename starting with `-` would be parsed by ffmpeg
        // as another option. The `is_file` in the wrapper is the
        // user-controlled side; `extra_args` is also user-controlled
        // (config / future CLI passthrough). Both need the same
        // allowlist discipline.
        for arg in [
            "-i",                    // bare -i on the deny list (re-injects input)
            "-c:v",                  // redundant with our hardcoded -c:v; deny-listed
            "-c:a",                  // would override our audio codec; deny-listed
            "-o",                    // would re-inject output; deny-listed
            "-y",                    // -y is hardcoded; deny-listed
            "-i /etc/passwd",        // shell-style value with whitespace
            "-vf scale=720:foo bar", // whitespace inside value
            "-vf scale=720:foo;rm",  // shell metachar
            "--preset",              // long option (ambiguous; reject for safety)
            "preset",                // missing leading dash
            "",                      // empty
            "-",                     // bare dash
            "-; rm -rf /",           // shell metachar prefix
        ] {
            assert!(!is_safe_extra_arg(arg), "expected UNSAFE: {arg:?}");
        }
    }

    #[test]
    fn first_unsafe_extra_arg_returns_position() {
        // Bare positional ("10") has no leading dash, so the
        // validator rejects it at index 2.
        let args = vec!["-vf".to_string(), "-ss".to_string(), "10".to_string()];
        assert_eq!(first_unsafe_extra_arg(&args), Some(2));
    }
}
