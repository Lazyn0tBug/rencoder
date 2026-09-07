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
/// Stub for Phase 2: appends the format's default extension. Phase 4 will
/// align this with `encode.zsh`'s naming convention (e.g. `<stem>.<tag>.<ext>`).
pub fn output_path_for(input: &Path, format: VideoFormat) -> PathBuf {
    let stem = input
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("output");
    let parent = input.parent().unwrap_or_else(|| Path::new(""));
    parent.join(format!("{}.{}", stem, format.default_extension()))
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

    #[test]
    fn output_path_for_strips_input_extension() {
        let p = output_path_for(Path::new("/dir/movie.mp4"), VideoFormat::Av1);
        assert_eq!(p, PathBuf::from("/dir/movie.mkv"));
    }

    #[test]
    fn output_path_for_relative_input() {
        let p = output_path_for(Path::new("movie.avi"), VideoFormat::X264);
        assert_eq!(p, PathBuf::from("movie.mp4"));
    }

    #[test]
    fn output_path_for_no_extension_input() {
        let p = output_path_for(Path::new("/dir/movie"), VideoFormat::Av1);
        assert_eq!(p, PathBuf::from("/dir/movie.mkv"));
    }

    #[test]
    fn output_path_for_vp9_uses_webm() {
        let p = output_path_for(Path::new("/d/clip.mp4"), VideoFormat::Vp9);
        assert_eq!(p, PathBuf::from("/d/clip.webm"));
    }

    #[test]
    fn output_path_for_hevc_uses_mp4() {
        let p = output_path_for(Path::new("/d/clip.mkv"), VideoFormat::Hevc);
        assert_eq!(p, PathBuf::from("/d/clip.mp4"));
    }
}
