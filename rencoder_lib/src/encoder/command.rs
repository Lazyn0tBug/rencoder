//! FFmpeg command construction.
//!
//! `build_ffmpeg_command` -- assemble argv, including the `-r <fps>`
//! and `-fps_mode` output option placement.
//!
//! `-r` and `-fps_mode` are FFmpeg **output options**: they must appear
//! AFTER `-i <input>` and BEFORE the output path. Placing them before
//! `-i` would force the input frame rate (origin §3.3 + R4). When
//! `src_fps = None`, neither is injected even if capping is enabled
//! (R21): we'd rather not transcode with a guessed frame rate than
//! corrupt the output.

use crate::config::app::FfmpegArgsConfig;
use crate::types::{EncodeParams, EncodeTask, FpsDecision, VideoFormat};

#[cfg(test)]
use crate::encoder::ffmpeg_arg::ExtraArg;

/// Is this an NVIDIA hardware encoder?
///
/// NVENC's rate control is constant quality (`-cq`), not the CRF scale
/// the software encoders use -- the two are not interchangeable.
fn is_nvenc(encoder: &str) -> bool {
    encoder.ends_with("_nvenc")
}

/// Build the argv for an `ffmpeg` invocation.
///
/// Layout:
/// ```text
/// ffmpeg <pre_input...> -i <input>
///   <post_input...>
///   -c:v <encoder> [-preset <default_preset>] [-crf ... | -b:v ...]
///   -c:a <default_audio_codec> -b:a <default_audio_bitrate>
///   [-r <MAX_FPS> -fps_mode cfr]   # source exceeds the cap
///   [-fps_mode passthrough]        # cap enabled, source within the cap
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
    task: &EncodeTask,
    params: &EncodeParams,
    encoder: &str,
    src_fps: Option<f64>,
) -> Vec<String> {
    let ffmpeg_args: &FfmpegArgsConfig = &params.ffmpeg_args;
    let mut argv: Vec<String> = Vec::with_capacity(18);

    argv.push("ffmpeg".to_string());
    // `-y` is hardcoded (never user-overridable) so a user setting
    // a bad config can't accidentally get ffmpeg to prompt.
    argv.push("-y".to_string());
    argv.extend(ffmpeg_args.pre_input.iter().cloned());
    argv.push("-i".to_string());
    // `task.input` is an `InputPath`; the type system guarantees it
    // doesn't start with `-` (review #109), so the lossy round-trip
    // is safe to push verbatim.
    argv.push(task.input.to_string());
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
        if is_nvenc(encoder) {
            // NVENC is constant quality (`-cq`), not the x264/x265 CRF
            // scale -- same numbers, different meaning. Matches
            // `encode.zsh`'s ENC_SPEC, where the nvenc rows are "cq"
            // mode (`-cq N -b:v 0`).
            argv.push("-cq".to_string());
            argv.push(params.bitrate_or_crf.to_string());
            // `-b:v 0` pairs with `-cq`: constant quality, no rate cap.
            argv.push("-b:v".to_string());
            argv.push("0".to_string());
        } else {
            argv.push("-crf".to_string());
            argv.push(params.bitrate_or_crf.to_string());
        }
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

    // Frame-rate handling (R4 + R21 + `encode.zsh` v10.2.0).
    //
    // The decision comes from `MaxFpsConfig::decide` so the skip
    // predicate, this argv and the result record can never disagree
    // about whether the source was reduced.
    //
    //   Reduce      → `-r <cap> -fps_mode cfr`
    //   Passthrough → `-fps_mode passthrough`
    //   Disabled / Unknown → nothing
    //
    // `-fps_mode` is not optional: without it ffmpeg applies its own
    // vsync behaviour and may duplicate or drop frames to reach the
    // container's nominal frame rate — lossy for VFR sources.
    match params.max_fps.decide(src_fps) {
        FpsDecision::Reduce { target } => {
            argv.push("-r".to_string());
            argv.push(target.to_string());
            argv.push("-fps_mode".to_string());
            argv.push("cfr".to_string());
        }
        FpsDecision::Passthrough => {
            argv.push("-fps_mode".to_string());
            argv.push("passthrough".to_string());
        }
        FpsDecision::Disabled | FpsDecision::Unknown => {}
    }

    // User passthrough args.
    // User-supplied extra flags. Each `ExtraArg` has already been
    // validated by `ExtraArg::parse` at the boundary (CLI parse or
    // future TOML deserialize) — no runtime check needed here.
    for arg in &params.extra_args {
        argv.push(arg.as_argv());
    }

    // Output path always last.
    argv.push(task.output.to_string_lossy().into_owned());

    argv
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::MaxFpsConfig;
    use std::path::PathBuf;

    fn task(input: &str, output: &str) -> EncodeTask {
        EncodeTask {
            input: crate::types::InputPath::new(&PathBuf::from(input)).unwrap(),
            output: PathBuf::from(output),
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

    // ---- build_ffmpeg_command ----

    #[test]
    fn command_starts_with_ffmpeg_y_i_input() {
        let argv = build_ffmpeg_command(
            &task("/in.mp4", "/out.mkv"),
            &params_disabled(),
            "libx264",
            Some(30.0),
        );
        // argv[0] = "ffmpeg", argv[1] = "-y" (hardcoded). Then
        // FfmpegArgsConfig::default().pre_input (["-hide_banner",
        // "-loglevel", "info"]) sits between `-y` and `-i`.
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
        );
        assert_eq!(argv.last().unwrap(), "/out.mkv");
    }

    #[test]
    fn r_placed_after_i_and_before_output() {
        // Critical assertion (R4): -r must be an output option.
        let mut params = params_disabled();
        params.max_fps = MaxFpsConfig {
            max_fps: 30.0,
            eps: 0.01,
        };
        let argv =
            build_ffmpeg_command(&task("/in.mp4", "/out.mkv"), &params, "libx264", Some(60.0));
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
    fn fps_mode_placed_after_i_and_before_output() {
        // `-fps_mode` is an output option too (encode.zsh v10.2.0).
        let mut params = params_disabled();
        params.max_fps = MaxFpsConfig {
            max_fps: 30.0,
            eps: 0.01,
        };
        let argv =
            build_ffmpeg_command(&task("/in.mp4", "/out.mkv"), &params, "libx264", Some(60.0));
        let i_pos = argv.iter().position(|s| s == "-i").expect("-i present");
        let f_pos = argv
            .iter()
            .position(|s| s == "-fps_mode")
            .expect("-fps_mode present");
        let out_pos = argv.len() - 1;
        assert!(
            i_pos < f_pos && f_pos < out_pos,
            "-fps_mode must sit between -i and the output (got i_pos={i_pos}, f_pos={f_pos}, out_pos={out_pos})"
        );
    }

    #[test]
    fn cfr_when_source_exceeds_cap() {
        let mut params = params_disabled();
        params.max_fps = MaxFpsConfig {
            max_fps: 30.0,
            eps: 0.01,
        };
        let argv =
            build_ffmpeg_command(&task("/in.mp4", "/out.mkv"), &params, "libx264", Some(60.0));
        let f_pos = argv
            .iter()
            .position(|s| s == "-fps_mode")
            .expect("-fps_mode present");
        assert_eq!(argv[f_pos + 1], "cfr");
    }

    #[test]
    fn passthrough_when_source_within_cap() {
        let mut params = params_disabled();
        params.max_fps = MaxFpsConfig {
            max_fps: 30.0,
            eps: 0.01,
        };
        let argv =
            build_ffmpeg_command(&task("/in.mp4", "/out.mkv"), &params, "libx264", Some(25.0));
        let f_pos = argv
            .iter()
            .position(|s| s == "-fps_mode")
            .expect("-fps_mode present");
        assert_eq!(argv[f_pos + 1], "passthrough");
    }

    #[test]
    fn no_r_when_src_fps_none() {
        // R21: src_fps=None means no -r even if cap is enabled.
        let mut params = params_disabled();
        params.max_fps = MaxFpsConfig {
            max_fps: 30.0,
            eps: 0.01,
        };
        let argv = build_ffmpeg_command(&task("/in.mp4", "/out.mkv"), &params, "libx264", None);
        assert!(
            !argv.contains(&"-r".to_string()),
            "argv must not contain -r when src_fps is None"
        );
        assert!(
            !argv.contains(&"-fps_mode".to_string()),
            "argv must not contain -fps_mode when src_fps is None"
        );
    }

    #[test]
    fn no_r_when_max_fps_disabled() {
        let params = params_disabled(); // max_fps = 0.0 (disabled)
        let argv =
            build_ffmpeg_command(&task("/in.mp4", "/out.mkv"), &params, "libx264", Some(60.0));
        assert!(!argv.contains(&"-r".to_string()));
        assert!(
            !argv.contains(&"-fps_mode".to_string()),
            "argv must not contain -fps_mode when the cap is disabled"
        );
    }

    #[test]
    fn no_r_when_source_fps_within_cap() {
        let mut params = params_disabled();
        params.max_fps = MaxFpsConfig {
            max_fps: 30.0,
            eps: 0.01,
        };
        let argv = build_ffmpeg_command(
            &task("/in.mp4", "/out.mkv"),
            &params,
            "libx264",
            Some(25.0), // below cap
        );
        assert!(!argv.contains(&"-r".to_string()));
    }

    #[test]
    fn crf_mode_emits_crf_flag() {
        let mut params = params_disabled();
        params.bitrate_or_crf = 45;
        let argv = build_ffmpeg_command(&task("/in.mp4", "/out.mkv"), &params, "libsvtav1", None);
        let crf_pos = argv.iter().position(|s| s == "-crf").expect("-crf present");
        assert_eq!(argv[crf_pos + 1], "45");
    }

    #[test]
    fn av1_crf_includes_preset_6() {
        let mut params = params_disabled();
        params.format = VideoFormat::Av1;
        params.bitrate_or_crf = 45;
        let argv = build_ffmpeg_command(&task("/in.mp4", "/out.mkv"), &params, "libsvtav1", None);
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
        let argv = build_ffmpeg_command(&task("/in.mp4", "/out.mkv"), &params, "libaom-av1", None);
        assert!(!argv.contains(&"-preset".to_string()));
        assert!(argv.contains(&"-crf".to_string()));
    }

    #[test]
    fn av1_crf_with_av1_nvenc_skips_preset() {
        // NVENC doesn't accept -preset.
        let mut params = params_disabled();
        params.format = VideoFormat::Av1;
        params.bitrate_or_crf = 45;
        let argv = build_ffmpeg_command(&task("/in.mp4", "/out.mkv"), &params, "av1_nvenc", None);
        assert!(!argv.contains(&"-preset".to_string()));
    }

    #[test]
    fn nvenc_uses_constant_quality_instead_of_crf() {
        // NVENC's rate control is `-cq` (constant quality), not the
        // x264/x265 CRF scale -- the two numbers mean different things.
        // Matches encode.zsh's ENC_SPEC, where the nvenc rows are "cq"
        // mode: `-cq N -b:v 0`.
        let mut params = params_disabled();
        params.format = VideoFormat::Av1;
        params.bitrate_or_crf = 45;
        let argv = build_ffmpeg_command(&task("/in.mp4", "/out.mkv"), &params, "av1_nvenc", None);
        assert!(!argv.contains(&"-crf".to_string()));
        let cq_pos = argv.iter().position(|s| s == "-cq").expect("-cq present");
        assert_eq!(argv[cq_pos + 1], "45");
        let bv_pos = argv.iter().position(|s| s == "-b:v").expect("-b:v present");
        assert_eq!(argv[bv_pos + 1], "0");
    }

    #[test]
    fn software_encoder_keeps_crf() {
        let mut params = params_disabled();
        params.format = VideoFormat::Av1;
        params.bitrate_or_crf = 45;
        let argv = build_ffmpeg_command(&task("/in.mp4", "/out.mkv"), &params, "libsvtav1", None);
        assert!(argv.contains(&"-crf".to_string()));
        assert!(!argv.contains(&"-cq".to_string()));
    }

    #[test]
    fn bitrate_mode_emits_bv_flag_with_k_suffix() {
        let mut params = params_disabled();
        params.is_crf = false;
        params.bitrate_or_crf = 5000;
        let argv = build_ffmpeg_command(&task("/in.mp4", "/out.mkv"), &params, "libx264", None);
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
        );
        let ca_pos = argv.iter().position(|s| s == "-c:a").expect("-c:a present");
        assert_eq!(argv[ca_pos + 1], "libopus");
        let ba_pos = argv.iter().position(|s| s == "-b:a").expect("-b:a present");
        assert_eq!(argv[ba_pos + 1], "32k");
    }

    #[test]
    fn extra_args_passed_through_before_output() {
        let mut params = params_disabled();
        // Build via ExtraArg::parse so the test exercises the
        // type-level boundary (Vec<ExtraArg>, not Vec<String>).
        params.extra_args = vec![
            ExtraArg::parse("-ss").unwrap(),
            ExtraArg::parse("scale=720:-1").unwrap(), // bare value form
            ExtraArg::parse("-metadata").unwrap(),
        ];
        let argv = build_ffmpeg_command(&task("/in.mp4", "/out.mkv"), &params, "libx264", None);
        let ss_pos = argv.iter().position(|s| s == "-ss").expect("-ss present");
        assert_eq!(argv[ss_pos + 1], "scale=720:-1");
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
        params.extra_args = vec![
            ExtraArg::parse("-metadata").unwrap(),
            ExtraArg::parse("title=foo").unwrap(),
        ];
        let argv =
            build_ffmpeg_command(&task("/in.mp4", "/out.mkv"), &params, "libx264", Some(60.0));
        let r_pos = argv.iter().position(|s| s == "-r").unwrap();
        let meta_pos = argv.iter().position(|s| s == "-metadata").unwrap();
        let out_pos = argv.len() - 1;
        assert!(r_pos < meta_pos);
        assert!(meta_pos < out_pos);
    }
}
