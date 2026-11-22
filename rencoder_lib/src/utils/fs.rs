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
    if is_same_file(&naive, input) {
        // Collision: default extension == input extension (or differs
        // only in case — macOS / Windows filesystems are case-insensitive
        // by default, so `movie.MKV` and `movie.mkv` are one file).
        // Append `_encoded` so the output is always distinct.
        parent.join(format!("{stem}_encoded.{ext}"))
    } else {
        naive
    }
}

/// Do two paths point at the same file?
///
/// Compares the **canonicalized parent directories** and the **file
/// names case-insensitively**:
///
/// - canonicalizing the parents resolves `.` / `..` segments and
///   symlinked directories, so `/d/./movie.mkv` and `/d/movie.mkv`
///   compare equal;
/// - the file name comparison ignores ASCII case, because macOS and
///   Windows filesystems are case-insensitive by default. `movie.MKV`
///   and `movie.mkv` are the same file there, and ffmpeg's own
///   "input and output are the same file" check compares the strings,
///   so it does not catch this either.
///
/// The file name itself is not canonicalized: the output usually does
/// not exist yet, and canonicalizing it would fail anyway.
pub fn is_same_file(input: &Path, other: &Path) -> bool {
    let (Some(input_name), Some(other_name)) = (input.file_name(), other.file_name()) else {
        // No file name (root, trailing separator): only an exact match
        // can be the same file.
        return input == other;
    };
    if !input_name
        .to_string_lossy()
        .eq_ignore_ascii_case(&other_name.to_string_lossy())
    {
        return false;
    }
    let empty = Path::new("");
    let input_dir = normalize(input.parent().unwrap_or(empty));
    let other_dir = normalize(other.parent().unwrap_or(empty));
    input_dir == other_dir
}

/// The temp file's prefix: the project owns these files.
///
/// The collector skips them, and they are cleaned up with the
/// leftovers (design doc §3.11.2).
pub const TEMP_PREFIX: &str = ".rencoder-tmp.";

/// The file an attempt writes to, next to the final output.
///
/// Same directory (the commit is a same-volume replace), the container
/// extension kept (ffmpeg picks the muxer by extension), and the
/// project's prefix (the collector skips it).
pub fn temp_output_path(output: &Path) -> PathBuf {
    let dir = output.parent().unwrap_or_else(|| Path::new(""));
    let name = output
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "output".to_string());
    dir.join(format!("{TEMP_PREFIX}{name}"))
}

/// Is this one of our temp files?
pub fn is_temp_file(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.starts_with(TEMP_PREFIX))
}

/// Replace `to` with `from` -- atomically, on the same volume.
///
/// The old result **stays in place** when the replace fails (design
/// doc §3.11.2): the target is untouched, and the caller records the
/// failure.
///
/// Unix: `rename` replaces the target atomically.
/// Windows: `rename` refuses an existing target, so the platform call
/// with "replace existing" is used.
pub fn replace_file(from: &Path, to: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        std::fs::rename(from, to)
    }
    #[cfg(windows)]
    {
        replace_file_windows(from, to)
    }
    #[cfg(not(any(unix, windows)))]
    {
        // No platform guarantee available: the plain rename is the
        // best this platform offers.
        std::fs::rename(from, to)
    }
}

/// The Windows replace: `MoveFileEx` with "replace existing".
///
/// Windows' `rename` refuses an existing target, and deleting the old
/// target first is forbidden (design doc §3.11.2: a failed replace must
/// keep the old result). `MoveFileEx` does the replace in one step.
///
/// **Not compiler-verified on this machine** (no Windows std here) --
/// verify it on a Windows host, like the CLI's signal handler.
#[cfg(windows)]
fn replace_file_windows(from: &Path, to: &Path) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{
        MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
    };

    let from_wide: Vec<u16> = from.as_os_str().encode_wide().chain(Some(0)).collect();
    let to_wide: Vec<u16> = to.as_os_str().encode_wide().chain(Some(0)).collect();
    let flags = MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH;
    // SAFETY: both paths are NUL-terminated wide strings that outlive
    // the call.
    let moved = unsafe { MoveFileExW(from_wide.as_ptr(), to_wide.as_ptr(), flags) };
    if moved == 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
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

    // ---- is_same_file ----

    #[test]
    fn is_same_file_exact_match() {
        assert!(is_same_file(
            Path::new("/dir/movie.mkv"),
            Path::new("/dir/movie.mkv")
        ));
    }

    #[test]
    fn is_same_file_ignores_ascii_case() {
        // macOS / Windows filesystems are case-insensitive by default:
        // these two are the same file.
        assert!(is_same_file(
            Path::new("/dir/movie.MKV"),
            Path::new("/dir/movie.mkv")
        ));
    }

    #[test]
    fn is_same_file_ignores_dot_segments() {
        assert!(is_same_file(
            Path::new("/dir/./movie.mkv"),
            Path::new("/dir/movie.mkv")
        ));
    }

    #[test]
    fn is_not_same_file_for_different_names() {
        assert!(!is_same_file(
            Path::new("/dir/movie.mkv"),
            Path::new("/dir/other.mkv")
        ));
    }

    #[test]
    fn is_not_same_file_for_different_directories() {
        assert!(!is_same_file(
            Path::new("/dir1/movie.mkv"),
            Path::new("/dir2/movie.mkv")
        ));
    }

    #[test]
    fn output_path_for_avoids_case_insensitive_collision() {
        // `movie.MKV` + mkv target would yield `movie.mkv` -- the same
        // file on a case-insensitive filesystem. The `_encoded` suffix
        // has to kick in.
        let p = output_path_for(Path::new("/dir/movie.MKV"), VideoFormat::Av1);
        assert_eq!(p, PathBuf::from("/dir/movie_encoded.mkv"));
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

    // ---- the temp file and the commit (design doc §3.11.2) ----

    fn scratch(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "rencoder_fs_{label}_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or_default()
        ));
        std::fs::create_dir_all(&dir).expect("the scratch dir must exist");
        dir
    }

    #[test]
    fn the_temp_file_sits_next_to_the_output_and_keeps_the_extension() {
        let temp = temp_output_path(Path::new("/dir/clip.mkv"));
        assert_eq!(temp, PathBuf::from("/dir/.rencoder-tmp.clip.mkv"));
        assert!(is_temp_file(&temp));
        assert!(!is_temp_file(Path::new("/dir/clip.mkv")));
    }

    #[test]
    fn replace_file_replaces_an_existing_target() {
        let dir = scratch("replace");
        let from = dir.join("from");
        let to = dir.join("to");
        std::fs::write(&from, b"new").unwrap();
        std::fs::write(&to, b"old").unwrap();

        replace_file(&from, &to).expect("the replace must work");

        assert_eq!(std::fs::read_to_string(&to).unwrap(), "new");
        assert!(!from.exists(), "the source is moved, not copied");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_failed_replace_keeps_the_old_result() {
        // The temp is missing -> the replace fails -> the old result
        // is untouched (design doc §3.11.2).
        let dir = scratch("failed_replace");
        let to = dir.join("to");
        std::fs::write(&to, b"old").unwrap();

        let result = replace_file(&dir.join("missing"), &to);

        assert!(result.is_err(), "the replace must fail");
        assert_eq!(
            std::fs::read_to_string(&to).unwrap(),
            "old",
            "the old result must survive a failed replace"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn replacing_an_open_target_is_allowed_on_unix() {
        // The platform's own semantics (design doc §3.11.2): on Unix
        // an open target does not block the replace -- the reader
        // keeps the old inode, the path points at the new one. On
        // Windows the replace of an occupied target fails, and the
        // old result stays.
        let dir = scratch("open_target");
        let from = dir.join("from");
        let to = dir.join("to");
        std::fs::write(&from, b"new").unwrap();
        std::fs::write(&to, b"old").unwrap();

        let open = std::fs::File::open(&to).unwrap();
        replace_file(&from, &to).expect("unix replaces an open target");

        assert_eq!(std::fs::read_to_string(&to).unwrap(), "new");
        // The reader still sees the old content (the old inode).
        use std::io::Read;
        let mut seen = String::new();
        let mut open = open;
        open.read_to_string(&mut seen).unwrap();
        assert_eq!(seen, "old");
        let _ = std::fs::remove_dir_all(&dir);
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
