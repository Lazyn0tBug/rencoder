//! Path normalization and output-path generation helpers.

use std::path::{Path, PathBuf};

use crate::platform::fs_semantics;
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

/// The final output paths for a batch of inputs.
///
/// Two inputs can want the same output: `movie.mp4` and `movie.avi`
/// in one directory both want `movie.mkv`. The colliding group is
/// **renamed** instead of failing the batch: each member gets
/// `<stem>_<source extension>_<n>.<target extension>`
/// (`movie_mp4_1.mkv`, `movie_avi_2.mkv`) -- the source format and a
/// number, so the names cannot repeat.
///
/// The order inside a group is by path, so the names do not depend on
/// the order the files were collected in: a run that named them
/// differently would leave the previous files behind.
///
/// An output never lands on a source file (the inputs are taken too),
/// so the same-file guard stays a backstop rather than the rule.
pub fn plan_outputs(inputs: &[PathBuf], format: VideoFormat) -> Vec<PathBuf> {
    let naive: Vec<PathBuf> = inputs
        .iter()
        .map(|input| output_path_for(input, format))
        .collect();

    // The groups: the inputs that want the same output.
    let mut groups: Vec<Vec<usize>> = Vec::new();
    let mut group_keys: Vec<String> = Vec::new();
    for (index, output) in naive.iter().enumerate() {
        let key = file_key(output);
        match group_keys.iter().position(|known| *known == key) {
            Some(position) => groups[position].push(index),
            None => {
                group_keys.push(key);
                groups.push(vec![index]);
            }
        }
    }

    // The names already taken: every input and every naive output. A
    // renamed output must not land on any of them.
    let mut taken: Vec<String> = inputs.iter().map(|input| file_key(input)).collect();
    taken.extend(naive.iter().map(|output| file_key(output)));

    let mut planned = naive.clone();
    for mut members in groups {
        if members.len() < 2 {
            continue;
        }
        // Sorted by path: the numbers must not depend on the order the
        // files were collected in.
        members.sort_by(|a, b| inputs[*a].cmp(&inputs[*b]));
        for (position, index) in members.iter().enumerate() {
            let mut number = position + 1;
            let candidate = loop {
                let candidate = disambiguated_output(&naive[*index], &inputs[*index], number);
                if !taken.contains(&file_key(&candidate)) {
                    break candidate;
                }
                number += 1;
            };
            taken.push(file_key(&candidate));
            planned[*index] = candidate;
        }
    }
    planned
}

/// The renamed output: `<stem>_<source extension>_<n>.<target extension>`.
fn disambiguated_output(output: &Path, input: &Path, number: usize) -> PathBuf {
    let stem = output
        .file_stem()
        .map(|stem| stem.to_string_lossy().into_owned())
        .unwrap_or_else(|| "output".to_string());
    let target_ext = output
        .extension()
        .map(|ext| ext.to_string_lossy().into_owned())
        .unwrap_or_default();
    let source_ext = input
        .extension()
        .map(|ext| ext.to_string_lossy().into_owned())
        .unwrap_or_default();

    let mut name = stem;
    if !source_ext.is_empty() {
        name.push('_');
        name.push_str(&source_ext);
    }
    name.push('_');
    name.push_str(&number.to_string());
    if !target_ext.is_empty() {
        name.push('.');
        name.push_str(&target_ext);
    }
    output.parent().unwrap_or_else(|| Path::new("")).join(name)
}

/// The identity of a file: the same directory (canonicalized) and the
/// same name (folded -- one rule everywhere, see
/// [`crate::platform::fs_semantics`]).
fn file_key(path: &Path) -> String {
    let parent = path.parent().unwrap_or_else(|| Path::new(""));
    let dir = normalize(parent);
    let name = path
        .file_name()
        .map(|name| fs_semantics::fold_name(&name.to_string_lossy()))
        .unwrap_or_default();
    format!("{}|{}", dir.display(), name)
}

/// Do two paths point at the same file?
///
/// Compares the **canonicalized parent directories** and the **file
/// names folded** (one rule on every platform, see
/// [`crate::platform::fs_semantics`]):
///
/// - canonicalizing the parents resolves `.` / `..` segments and
///   symlinked directories, so `/d/./movie.mkv` and `/d/movie.mkv`
///   compare equal;
/// - the file name comparison ignores case, because macOS and Windows
///   filesystems are case-insensitive by default. `movie.MKV` and
///   `movie.mkv` are the same file there, and ffmpeg's own
///   "input and output are the same file" check compares the strings,
///   so it does not catch this either. On a case-sensitive filesystem
///   the comparison is conservative -- it may treat two different
///   files as one, which costs a rename, never a source file.
///
/// The file name itself is not canonicalized: the output usually does
/// not exist yet, and canonicalizing it would fail anyway.
pub fn is_same_file(input: &Path, other: &Path) -> bool {
    let (Some(input_name), Some(other_name)) = (input.file_name(), other.file_name()) else {
        // No file name (root, trailing separator): only an exact match
        // can be the same file.
        return input == other;
    };
    if !fs_semantics::same_name(&input_name.to_string_lossy(), &other_name.to_string_lossy()) {
        return false;
    }
    let empty = Path::new("");
    let input_dir = normalize(input.parent().unwrap_or(empty));
    let other_dir = normalize(other.parent().unwrap_or(empty));
    input_dir == other_dir
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
        // The temp dir, not a literal "/tmp": on Windows "/tmp" is
        // neither absolute nor existing, so the path would come back
        // unchanged and the assertion would be about Unix.
        let p = normalize(&std::env::temp_dir());
        // canonicalize may resolve the path further (symlinks, the
        // Windows UNC prefix); just assert the path is non-empty and
        // absolute.
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

    // ---- plan_outputs (the batch's output naming) ----

    #[test]
    fn a_colliding_group_is_renamed_with_the_format_and_a_number() {
        // `movie.mp4` and `movie.avi` both want `movie.mkv` -- they
        // must not overwrite each other.
        let dir = PathBuf::from("/d");
        let planned = plan_outputs(
            &[dir.join("movie.mp4"), dir.join("movie.avi")],
            VideoFormat::Av1,
        );

        // The vector follows the inputs (movie.mp4 first); the
        // numbers follow the sorted paths, so they are the same on
        // every run.
        assert_eq!(
            planned,
            vec![
                PathBuf::from("/d/movie_mp4_2.mkv"),
                PathBuf::from("/d/movie_avi_1.mkv"),
            ],
            "the colliding group gets the format and a number"
        );
    }

    #[test]
    fn the_names_do_not_depend_on_the_order_the_files_came_in() {
        let dir = PathBuf::from("/d");
        let inputs = [dir.join("movie.mp4"), dir.join("movie.avi")];
        let reversed = [inputs[1].clone(), inputs[0].clone()];

        let forward = plan_outputs(&inputs, VideoFormat::Av1);
        let backward = plan_outputs(&reversed, VideoFormat::Av1);

        // The vector follows the inputs; the *mapping* (input ->
        // output) must not depend on the order they came in.
        let mut forward_pairs: Vec<(PathBuf, PathBuf)> =
            inputs.iter().cloned().zip(forward).collect();
        forward_pairs.sort();
        let mut backward_pairs: Vec<(PathBuf, PathBuf)> =
            reversed.iter().cloned().zip(backward).collect();
        backward_pairs.sort();
        assert_eq!(
            forward_pairs, backward_pairs,
            "the same inputs must get the same names, whatever the order"
        );
    }

    #[test]
    fn a_batch_without_collisions_keeps_the_plain_names() {
        let dir = PathBuf::from("/d");
        let planned = plan_outputs(&[dir.join("a.mp4"), dir.join("b.mp4")], VideoFormat::Av1);

        assert_eq!(
            planned,
            vec![PathBuf::from("/d/a.mkv"), PathBuf::from("/d/b.mkv")],
            "only the colliding group is renamed"
        );
    }

    #[test]
    fn a_planned_output_never_lands_on_a_source_file() {
        // The inputs are taken too: `movie.mp4` and `movie_mp4_1.mp4`
        // would both want a name the other could take.
        let dir = PathBuf::from("/d");
        let planned = plan_outputs(
            &[
                dir.join("movie.mp4"),
                dir.join("movie.avi"),
                dir.join("movie_mp4_1.mp4"),
            ],
            VideoFormat::Av1,
        );

        let sources = [
            dir.join("movie.mp4"),
            dir.join("movie.avi"),
            dir.join("movie_mp4_1.mp4"),
        ];
        for output in &planned {
            assert!(
                !sources.iter().any(|source| is_same_file(source, output)),
                "an output must not land on a source file: {output:?}"
            );
        }
        // And the names are unique.
        for (index, output) in planned.iter().enumerate() {
            for other in planned.iter().skip(index + 1) {
                assert!(!is_same_file(output, other), "the names must differ");
            }
        }
    }

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

    // v1.2 follow-up #3: collision guard matrix — every
    // (format, input ext) pair where the format's default
    // extension matches the input's extension MUST trip the
    // `_encoded` suffix so output != input structurally.
    //
    // Platform-independent on purpose: the naming rule is the same
    // everywhere, so the matrix runs on every host.
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
