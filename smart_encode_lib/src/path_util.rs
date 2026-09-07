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
