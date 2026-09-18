//! Path normalization and output-path generation helpers.

use std::path::{Path, PathBuf};

use crate::types::VideoFormat;

/// Canonicalize a path; fall back to the input verbatim on error.
///
/// Used when we want absolute paths in logs/UI even if the user passed a
/// relative one.
pub fn normalize(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

/// Compute the output path for a transcoded file.
///
/// # Naming convention (v1.2 follow-up #3)
///
/// Default: `<stem>.<target_ext>` where `<target_ext>` is the
/// format's container:
/// - X264 / HEVC / AV1 → `.mkv`
/// - VP9 → `.webm`
///
/// When the resulting path would equal the input path (e.g.
/// `foo.mkv` + AV1 would yield `foo.mkv` — overwriting the input,
/// and cancel cleanup would then remove the input), append a
/// disambiguating `_encoded.<target_ext>` suffix. Examples:
///
/// | Format | input ext | output |
/// |---|---|---|
/// | X264 / HEVC / AV1 | `.mp4` / `.avi` / `.webm` | `<stem>.mkv` |
/// | X264 / HEVC / AV1 | `.mkv` | `<stem>_encoded.mkv` |
/// | VP9 | `.mp4` / `.mkv` / `.avi` | `<stem>.webm` |
/// | VP9 | `.webm` | `<stem>_encoded.webm` |
///
/// The `_<ext>`-suffix is the **second line of defense** — the
/// primary guarantee is that the default extension differs from
/// the input extension in the common case (mp4/avi/webm inputs).
/// The suffix only triggers when a user explicitly targets a
/// container that matches their input's container.
///
/// # Why this matters
///
/// - ffmpeg can read `foo.mkv` and write to `foo.mkv` only via a
///   platform-specific "write to same path you read from"
///   workaround; on macOS / Linux this often fails. ffmpeg on
///   cancel may leave the input partially overwritten AND fail
///   to delete the partial. Cleanup that targets the wrong
///   file (the input instead of the partial) is structural
///   failure.
/// - cancel cleanup relies on `argv.last()` being the partial
///   output path; if output == input, the cleanup deletes the
///   input instead of the partial — silently corrupting the
///   user's source media.
pub fn output_path_for(input: &Path, format: VideoFormat) -> PathBuf {
    let stem = input
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("output");
    let parent = input.parent().unwrap_or_else(|| Path::new(""));
    let ext = format.default_extension();
    let naive = parent.join(format!("{stem}.{ext}"));
    if naive == input {
        // Collision: default extension == input extension.
        // Append `_encoded` so the output is always distinct.
        parent.join(format!("{stem}_encoded.{ext}"))
    } else {
        naive
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_falls_back_for_nonexistent_path() {
        let p = normalize(Path::new("/nonexistent/path/never/here"));
        assert_eq!(p, PathBuf::from("/nonexistent/path/never/here"));
    }

    #[test]
    fn normalize_returns_absolute_for_existing() {
        let p = normalize(Path::new("/tmp"));
        // canonicalize may resolve /tmp to /private/tmp on macOS; just
        // assert the path is non-empty and absolute.
        assert!(p.is_absolute());
        assert!(!p.as_os_str().is_empty());
    }

    // v1.2 follow-up #3: X264/HEVC/AV1 → mkv, VP9 → webm.
    // Existing tests updated to reflect the new defaults.

    #[test]
    fn output_path_for_av1_mp4_input_produces_mkv() {
        let p = output_path_for(Path::new("/dir/movie.mp4"), VideoFormat::Av1);
        assert_eq!(p, PathBuf::from("/dir/movie.mkv"));
    }

    #[test]
    fn output_path_for_x264_avi_input_produces_mkv() {
        // v1.2 follow-up #3: X264 now produces .mkv (was .mp4).
        let p = output_path_for(Path::new("movie.avi"), VideoFormat::X264);
        assert_eq!(p, PathBuf::from("movie.mkv"));
    }

    #[test]
    fn output_path_for_no_extension_input() {
        let p = output_path_for(Path::new("/dir/movie"), VideoFormat::Av1);
        assert_eq!(p, PathBuf::from("/dir/movie.mkv"));
    }

    #[test]
    fn output_path_for_vp9_mp4_input_produces_webm() {
        let p = output_path_for(Path::new("/d/clip.mp4"), VideoFormat::Vp9);
        assert_eq!(p, PathBuf::from("/d/clip.webm"));
    }

    #[test]
    fn output_path_for_hevc_mkv_input_collides_with_encoded_suffix() {
        // v1.2 follow-up #3: HEVC now produces .mkv; input is
        // already .mkv → collision guard fires → `_encoded` suffix.
        let p = output_path_for(Path::new("/d/clip.mkv"), VideoFormat::Hevc);
        assert_eq!(p, PathBuf::from("/d/clip_encoded.mkv"));
    }

    // v1.2 follow-up #3: collision guard matrix — every
    // (format, input ext) pair where the format's default
    // extension matches the input's extension MUST trip the
    // `_encoded` suffix so output != input structurally.
    #[test]
    fn output_path_for_collision_guard_always_distinguishes() {
        let cases = [
            // X264 / HEVC / AV1 all produce .mkv; .mkv inputs collide.
            ("/x/foo.mkv", VideoFormat::X264, "/x/foo_encoded.mkv"),
            ("/x/foo.mkv", VideoFormat::Hevc, "/x/foo_encoded.mkv"),
            ("/x/foo.mkv", VideoFormat::Av1, "/x/foo_encoded.mkv"),
            // VP9 produces .webm; .webm input collides.
            ("/x/foo.webm", VideoFormat::Vp9, "/x/foo_encoded.webm"),
            // Sanity: non-colliding inputs do NOT trip the suffix.
            ("/x/foo.mp4", VideoFormat::Av1, "/x/foo.mkv"),
            ("/x/foo.mp4", VideoFormat::X264, "/x/foo.mkv"),
            ("/x/foo.mp4", VideoFormat::Hevc, "/x/foo.mkv"),
            ("/x/foo.avi", VideoFormat::Av1, "/x/foo.mkv"),
            ("/x/foo.mp4", VideoFormat::Vp9, "/x/foo.webm"),
            ("/x/foo.mkv", VideoFormat::Vp9, "/x/foo.webm"),
        ];
        for (input, format, expected) in cases {
            let input_p = Path::new(input);
            let out = output_path_for(input_p, format);
            assert_eq!(
                out,
                PathBuf::from(expected),
                "output_path_for({input:?}, {format:?}) = {out:?}, expected {expected:?}"
            );
            assert_ne!(
                out, input_p,
                "output must NEVER equal input (avoids cancel cleanup deleting the source)"
            );
        }
    }
}
