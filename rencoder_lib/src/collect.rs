//! File collection for batch transcode.
//!
//! Walks a directory (or accepts a single file) and returns paths matching
//! the given extension allowlist. Symlinks are never followed (R10) so a
//! loop in the filesystem doesn't recurse forever.

use std::path::Path;
#[cfg(test)]
use std::path::PathBuf;

use crate::error::EncodeError;
use crate::pipeline::cancel::CancelToken;
use crate::types::{InputPath, Target};

/// How often the collection checks the cancel token.
///
/// The check is a file's existence (the cancel request, design doc
/// §3.11.2) -- cheap, but not free. Every 64 entries keeps the
/// reaction time around a millisecond on a big tree without paying a
/// stat per file.
const CANCEL_CHECK_EVERY: usize = 64;

/// Collect input files for a transcode batch, wrapped in [`InputPath`]
/// (review #109 type-level boundary — dash-prefixed filenames are
/// rejected at construction so ffmpeg can never parse them as options).
///
/// - `Target::File(p)` returns `[InputPath::new(p)]` if `p` exists as a
///   file and matches the extension filter (empty extensions = match
///   all). Bad paths are skipped with a warning.
/// - `Target::Dir(root)` walks the tree without following symlinks,
///   returns every regular file matching the filter, wrapped in
///   `InputPath`, **in path order** (the readdir order is not a
///   contract -- design doc §3.11.1). Dash-prefixed files are
///   skipped with a warning.
///
/// `extensions` is a case-insensitive allowlist; an empty slice means
/// "all files".
///
/// v1.2 follow-up #5: `max_batch_size` caps the result vec length.
/// `collect_dir` short-circuits once `max + 1` files are seen so a
/// misclicked root (`/`, `~`) doesn't allocate a giant Vec before
/// bailing. `collect_single_file` (target = a single file) ignores
/// the cap — passing a single path through the same limit would
/// break the CLI `encode -f foo.mp4` UX.
/// `cancel` stops the walk: the startup phase must be cancellable
/// too (design doc §3.11.2) -- Ctrl-C during a long walk must not
/// wait for the walk to finish.
pub fn collect_files(
    target: &Target,
    extensions: &[String],
    max_batch_size: u32,
    cancel: &CancelToken,
) -> Result<Vec<InputPath>, EncodeError> {
    match target {
        Target::File(p) => Ok(collect_single_file(p, extensions)),
        Target::Dir(root) => collect_dir(root, extensions, max_batch_size, cancel),
    }
}

fn collect_single_file(p: &Path, extensions: &[String]) -> Vec<InputPath> {
    if p.is_file() && matches_extension(p, extensions) {
        match InputPath::new(p) {
            Ok(ip) => vec![ip],
            Err(e) => {
                tracing::warn!(path = %p.display(), error = %e, "skipping file");
                vec![]
            }
        }
    } else {
        vec![]
    }
}

fn collect_dir(
    root: &Path,
    extensions: &[String],
    max_batch_size: u32,
    cancel: &CancelToken,
) -> Result<Vec<InputPath>, EncodeError> {
    let mut results = Vec::new();
    let mut since_check = 0usize;
    let walker = walkdir::WalkDir::new(root).follow_links(false);
    for entry in walker {
        // The cancel check (design doc §3.11.2): the walk is part of
        // the startup phase, and Ctrl-C must not wait for it.
        since_check += 1;
        if since_check >= CANCEL_CHECK_EVERY {
            since_check = 0;
            if cancel.is_cancelled() {
                return Err(EncodeError::UserCancel);
            }
        }
        let entry = entry.map_err(|e| EncodeError::Probe(format!("walkdir: {e}")))?;
        let path = entry.path();
        // Use walkdir's reported file_type(), NOT `Path::is_file()`. The
        // latter calls `std::fs::metadata` which follows symlinks, so a
        // symlink to a file would slip past `follow_links(false)`. With
        // follow_links(false) walkdir reports symlinks with is_file()=false,
        // is_symlink()=true -- exactly what we want.
        // The app's own files in the operation directory (the cancel
        // request, the capability report -- design doc §3.6) are this
        // run's state, not the user's videos.
        if is_app_owned_file(path) {
            continue;
        }
        if entry.file_type().is_file() && matches_extension(path, extensions) {
            // Review fix #109 (type-level boundary): the runtime
            // dash-prefix check that used to live here is gone. Each
            // entry goes through `InputPath::new`, which rejects
            // dash-prefixed filenames at construction time. The
            // rejection surfaces as a warn-and-skip, matching the
            // v1 collect_dir behavior.
            match InputPath::new(path) {
                Ok(ip) => {
                    // v1.2 follow-up #5: cap. Check BEFORE pushing so
                    // we never allocate beyond `max_batch_size + 1`
                    // InputPath wrappers. The +1 lets `count` in the
                    // error carry "you had at least `max + 1`" — the
                    // user knows they overshot by 1+.
                    //
                    // v1.2 follow-up #5 fix: compare in `usize` (the
                    // count's natural type), not `as u32`. A 64-bit
                    // `walkdir` walk over a pathological root could
                    // push `results.len() > u32::MAX`; the cast
                    // would truncate, the comparison would misfire,
                    // and the cap would never trip. Promoting
                    // `max_batch_size` to usize is safe (u32 always
                    // fits) and avoids the silent truncation.
                    if results.len() >= max_batch_size as usize {
                        return Err(EncodeError::BatchTooLarge {
                            count: results.len() + 1,
                            max: max_batch_size,
                        });
                    }
                    results.push(ip);
                }
                Err(e) => tracing::warn!(path = %path.display(), error = %e, "skipping file"),
            }
        }
    }
    Ok(sort_by_path(results))
}

/// The collected files in **path order**.
///
/// The filesystem's readdir order is not a contract: ext4 hands out
/// hash order, APFS hands out creation order, so the same directory
/// yields a different list on another machine (or after a rebuild).
/// The batch would then run its files in a different order every
/// time -- and the output numbering follows the *sorted* paths
/// (design doc §3.11.1), so without this the collection order and
/// the numbering order disagree.
///
/// The order itself is [`crate::utils::fs::path_order`]: by path
/// **component**, one rule for the whole repo (the numbering uses
/// the same one).
///
/// Sorting once, here, is the single place the order is decided:
/// the batch, the plan and the dry run all read this list.
fn sort_by_path(mut results: Vec<InputPath>) -> Vec<InputPath> {
    results.sort_by(|a, b| crate::utils::fs::path_order(a.as_path(), b.as_path()));
    results
}

/// Is this file one of the app's own (design doc §3.6)?
fn is_app_owned_file(path: &Path) -> bool {
    path.file_name().is_some_and(|name| {
        crate::config::paths::APP_OWNED_FILE_NAMES
            .iter()
            .any(|owned| name == std::ffi::OsStr::new(owned))
    })
}

fn matches_extension(path: &Path, extensions: &[String]) -> bool {
    if extensions.is_empty() {
        return true;
    }
    match path.extension().and_then(|e| e.to_str()) {
        Some(ext) => extensions
            .iter()
            .any(|allowed| crate::platform::fs_semantics::same_name(allowed, ext)),
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pipeline::cancel::CancelToken;
    use std::fs;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Per-test isolated subdir under tempdir; auto-suffixed with the
    /// test's pid + an atomic counter so parallel runs don't collide.
    static COUNTER: AtomicUsize = AtomicUsize::new(0);

    fn fixture_dir(label: &str) -> PathBuf {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!(
            "rencoder_collect_{}_{}_{}",
            label,
            std::process::id(),
            n
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// The app's own files in the operation directory are not inputs
    /// (design doc §3.6): they are this run's state, not the user's
    /// videos.
    #[test]
    fn a_cancelled_collection_stops() {
        // The startup phase must be cancellable too (design doc
        // §3.11.2): Ctrl-C during a long walk must not wait for it.
        let dir = fixture_dir("cancelled");
        for i in 0..200 {
            touch(&dir, &format!("movie_{i}.mp4"));
        }
        let cancel = CancelToken::new();
        cancel.cancel();

        let result = collect_files(
            &Target::Dir(dir.clone()),
            &["mp4".into()],
            u32::MAX,
            &cancel,
        );

        assert!(
            matches!(result, Err(EncodeError::UserCancel)),
            "the collection must stop: {:?}",
            result.err()
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_apps_own_files_are_not_collected_as_inputs() {
        let dir = fixture_dir("app_owned");
        fs::write(dir.join("movie.mp4"), b"video").unwrap();
        fs::write(dir.join("cancel"), b"").unwrap();
        fs::write(dir.join("capabilities.json"), b"{}").unwrap();

        // No extension filter: everything in the directory is a candidate.
        let files = collect_files(
            &crate::types::Target::Dir(dir.clone()),
            &[],
            100,
            &CancelToken::new(),
        )
        .expect("collection must succeed");

        let names: Vec<String> = files
            .iter()
            .map(|f| {
                f.as_path()
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        assert_eq!(
            names,
            vec!["movie.mp4".to_string()],
            "only the user's video is an input"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    fn touch(dir: &Path, name: &str) -> PathBuf {
        let p = dir.join(name);
        fs::write(&p, b"x").unwrap();
        p
    }

    // ---- the order is a contract ----

    /// The collected files come back in **path order**.
    ///
    /// The filesystem's readdir order is not a contract (ext4 hands
    /// out hash order, APFS creation order), so without this the
    /// batch runs its files in a different order on another machine
    /// -- and the "second file" stops being a thing you can point at
    /// (design doc §3.11.1).
    #[test]
    fn the_collection_is_sorted_by_path() {
        let dir = fixture_dir("sorted");
        // Created in an order that is *not* the sorted one.
        for name in ["c.mp4", "a.mp4", "b.mp4"] {
            touch(&dir, name);
        }
        let files = collect_files(
            &Target::Dir(dir.clone()),
            &["mp4".into()],
            u32::MAX,
            &CancelToken::new(),
        )
        .expect("collection must succeed");

        let names: Vec<String> = files
            .iter()
            .map(|f| {
                f.as_path()
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        assert_eq!(
            names,
            vec![
                "a.mp4".to_string(),
                "b.mp4".to_string(),
                "c.mp4".to_string()
            ],
            "the collection is sorted by path"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    /// The sort is not only within one directory: a nested file sorts
    /// by its **full** path.
    ///
    /// The rule is *by path component*, not by the bytes of the path
    /// string -- and the fixture is chosen so the two rules
    /// disagree. `a/x.mp4` comes **before** `a-b/y.mp4` under the
    /// component rule ("a" < "a-b"), while the byte-wise order of
    /// the same strings is the other way round ('-' = 0x2D < '/' =
    /// 0x2F). A fixture without such a pair cannot tell the two
    /// rules apart: the sort could be changed to the other one and
    /// the test would still pass.
    #[test]
    fn the_collection_is_sorted_across_directories() {
        let dir = fixture_dir("sorted_nested");
        let sub = dir.join("a");
        fs::create_dir_all(&sub).unwrap();
        let sibling = dir.join("a-b");
        fs::create_dir_all(&sibling).unwrap();
        touch(&sub, "x.mp4");
        touch(&sibling, "y.mp4");
        let files = collect_files(
            &Target::Dir(dir.clone()),
            &["mp4".into()],
            u32::MAX,
            &CancelToken::new(),
        )
        .expect("collection must succeed");

        // The oracle is the rule itself (`Path`'s component-wise
        // `Ord`), not a proxy for it.
        let paths: Vec<&Path> = files.iter().map(|f| f.as_path()).collect();
        let mut sorted = paths.clone();
        sorted.sort();
        assert_eq!(paths, sorted, "the collection is sorted by path component");
        assert_eq!(
            files
                .iter()
                .map(|f| f
                    .as_path()
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
                    .into_owned())
                .collect::<Vec<_>>(),
            vec!["x.mp4".to_string(), "y.mp4".to_string()],
            "a/x.mp4 sorts before a-b/y.mp4 -- the component rule, not the byte rule"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    // ---- single-file target ----

    #[test]
    fn file_target_with_matching_extension_is_collected() {
        let dir = fixture_dir("file_match");
        let f = touch(&dir, "movie.mp4");
        let files = collect_files(
            &Target::File(f.clone()),
            &["mp4".into()],
            u32::MAX,
            &CancelToken::new(),
        )
        .unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].as_path(), f);
    }

    #[test]
    fn file_target_with_non_matching_extension_is_skipped() {
        let dir = fixture_dir("file_miss");
        let f = touch(&dir, "movie.txt");
        let files = collect_files(
            &Target::File(f.clone()),
            &["mp4".into()],
            u32::MAX,
            &CancelToken::new(),
        )
        .unwrap();
        assert!(files.is_empty());
    }

    #[test]
    fn file_target_with_empty_extensions_always_matches() {
        let dir = fixture_dir("file_empty");
        let f = touch(&dir, "anything.xyz");
        let files =
            collect_files(&Target::File(f.clone()), &[], u32::MAX, &CancelToken::new()).unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].as_path(), f);
    }

    #[test]
    fn dir_target_skips_files_with_dash_prefixed_names() {
        // Review fix #109 (type-level boundary): filenames starting
        // with `-` are rejected by `InputPath::new` at construction
        // time. collect_dir sees the rejection and skips with a
        // warning; the runtime dash-prefix check that used to live
        // here is gone.
        let dir = fixture_dir("dir_dash");
        let good = touch(&dir, "movie.mp4");
        let bad = touch(&dir, "-vf_negate.mp4");
        let files = collect_files(
            &Target::Dir(dir),
            &["mp4".into()],
            u32::MAX,
            &CancelToken::new(),
        )
        .unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].as_path(), good);
        assert!(!files.iter().any(|ip| ip.as_path() == bad));
    }

    #[test]
    fn file_target_missing_returns_empty() {
        let files = collect_files(
            &Target::File(PathBuf::from("/nonexistent/never.mp4")),
            &["mp4".into()],
            u32::MAX,
            &CancelToken::new(),
        )
        .unwrap();
        assert!(files.is_empty());
    }

    #[test]
    fn file_target_extension_match_is_case_insensitive() {
        let dir = fixture_dir("file_case");
        let f = touch(&dir, "movie.MP4");
        let files = collect_files(
            &Target::File(f.clone()),
            &["mp4".into()],
            u32::MAX,
            &CancelToken::new(),
        )
        .unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].as_path(), f);
    }

    // ---- directory target ----

    #[test]
    fn dir_filters_by_extension() {
        let dir = fixture_dir("dir_filter");
        touch(&dir, "a.mp4");
        touch(&dir, "b.txt");
        touch(&dir, "c.mkv");
        let files = collect_files(
            &Target::Dir(dir.clone()),
            &["mp4".into(), "mkv".into()],
            u32::MAX,
            &CancelToken::new(),
        )
        .unwrap();
        assert_eq!(files.len(), 2);
        assert!(files.iter().all(|ip| {
            ip.as_path()
                .extension()
                .and_then(|e| e.to_str())
                .is_some_and(|e| e == "mp4" || e == "mkv")
        }));
    }

    #[test]
    fn dir_with_no_extension_filter_returns_all_files() {
        let dir = fixture_dir("dir_all");
        touch(&dir, "a.mp4");
        touch(&dir, "b.txt");
        touch(&dir, "c.bin");
        let files = collect_files(
            &Target::Dir(dir.clone()),
            &[],
            u32::MAX,
            &CancelToken::new(),
        )
        .unwrap();
        assert_eq!(files.len(), 3);
    }

    #[test]
    fn dir_empty_returns_empty() {
        let dir = fixture_dir("dir_empty");
        let files = collect_files(
            &Target::Dir(dir.clone()),
            &["mp4".into()],
            u32::MAX,
            &CancelToken::new(),
        )
        .unwrap();
        assert!(files.is_empty());
    }

    #[test]
    fn dir_walks_subdirectories() {
        let dir = fixture_dir("dir_walk");
        let sub = dir.join("sub");
        fs::create_dir_all(&sub).unwrap();
        touch(&dir, "a.mp4");
        touch(&sub, "b.mp4");
        let files = collect_files(
            &Target::Dir(dir.clone()),
            &["mp4".into()],
            u32::MAX,
            &CancelToken::new(),
        )
        .unwrap();
        assert_eq!(files.len(), 2);
    }

    // Unix only: creating a symlink needs privileges the Windows CI
    // runner doesn't have. The production code path is the same
    // there -- walkdir never follows links on any platform.
    #[cfg(unix)]
    #[test]
    fn dir_skips_symlinks_to_files() {
        // Symlinks are never followed (follow_links(false)). Even if a
        // symlink points to a regular file inside the tree, it is reported
        // as a symlink and is_file() returns false -> excluded.
        let dir = fixture_dir("dir_symlink");
        let real = touch(&dir, "real.mp4");
        let link = dir.join("alias.mp4");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let files = collect_files(
            &Target::Dir(dir.clone()),
            &["mp4".into()],
            u32::MAX,
            &CancelToken::new(),
        )
        .unwrap();
        assert_eq!(files.len(), 1);
        assert!(files.iter().any(|ip| ip.as_path() == real));
        assert!(!files.iter().any(|ip| ip.as_path() == link));
    }

    // Unix only: same reason as `dir_skips_symlinks_to_files` --
    // creating a symlink needs privileges the Windows CI runner
    // doesn't have.
    #[cfg(unix)]
    #[test]
    fn dir_does_not_recurse_through_loop_symlink() {
        // Create dir/sub/ with a symlink sub/loop pointing back to dir.
        // With follow_links(false) the symlink is reported and skipped,
        // so the walk doesn't bounce forever.
        let dir = fixture_dir("dir_loop");
        let sub = dir.join("sub");
        fs::create_dir_all(&sub).unwrap();
        touch(&dir, "outer.mp4");
        touch(&sub, "inner.mp4");
        let link = sub.join("loop");
        std::os::unix::fs::symlink(&dir, &link).unwrap();
        let files = collect_files(
            &Target::Dir(dir.clone()),
            &["mp4".into()],
            u32::MAX,
            &CancelToken::new(),
        )
        .unwrap();
        // outer.mp4 + inner.mp4; the symlink should NOT cause the walk to
        // revisit outer.mp4 a second time.
        assert_eq!(files.len(), 2);
    }

    #[test]
    fn dir_extension_match_is_case_insensitive() {
        let dir = fixture_dir("dir_case");
        touch(&dir, "a.MP4");
        touch(&dir, "b.mp4");
        touch(&dir, "c.Mp4");
        let files = collect_files(
            &Target::Dir(dir.clone()),
            &["mp4".into()],
            u32::MAX,
            &CancelToken::new(),
        )
        .unwrap();
        assert_eq!(files.len(), 3);
    }

    // ---- v1.2 follow-up #5: batch size cap ----

    #[test]
    fn dir_cap_short_circuits_at_max_plus_one() {
        // Create `max_batch_size + 5` files but cap at `max_batch_size`.
        // collect_dir must return Err(BatchTooLarge { count, max })
        // with count = max_batch_size + 1 (the first one past the cap).
        let dir = fixture_dir("dir_cap");
        let max = 5_u32;
        for i in 0..(max + 5) {
            touch(&dir, &format!("movie_{i}.mp4"));
        }
        let err = collect_files(
            &Target::Dir(dir.clone()),
            &["mp4".into()],
            max,
            &CancelToken::new(),
        )
        .expect_err("expected BatchTooLarge");
        match err {
            EncodeError::BatchTooLarge { count, max: m } => {
                assert_eq!(m, max);
                // count is max + 1 — the loop trips on the max+1-th
                // entry. (We never allocate beyond max+1 because the
                // check runs before the push.)
                assert_eq!(count as u32, max + 1);
            }
            _ => panic!("expected BatchTooLarge, got {err:?}"),
        }
    }

    #[test]
    fn dir_cap_at_exact_boundary_succeeds() {
        // Exactly `max_batch_size` files should pass through; the
        // check is `results.len() >= max` so `== max` is still OK.
        let dir = fixture_dir("dir_cap_exact");
        let max = 3_u32;
        for i in 0..max {
            touch(&dir, &format!("m_{i}.mp4"));
        }
        let files = collect_files(&Target::Dir(dir), &["mp4".into()], max, &CancelToken::new())
            .expect("at boundary");
        assert_eq!(files.len() as u32, max);
    }

    #[test]
    fn dir_cap_one_over_boundary_fails() {
        // `max + 1` files should trip the cap.
        let dir = fixture_dir("dir_cap_over");
        let max = 3_u32;
        for i in 0..(max + 1) {
            touch(&dir, &format!("m_{i}.mp4"));
        }
        let err = collect_files(&Target::Dir(dir), &["mp4".into()], max, &CancelToken::new())
            .expect_err("expected BatchTooLarge");
        assert!(matches!(err, EncodeError::BatchTooLarge { .. }));
    }

    #[test]
    fn file_target_ignores_cap() {
        // Single-file target should pass even if `max = 0` — `encode
        // -f foo.mp4` is a deliberate user choice. (Avoids breaking
        // the CLI UX for one-off encodes.)
        let dir = fixture_dir("file_ignore_cap");
        let f = touch(&dir, "movie.mp4");
        let files = collect_files(
            &Target::File(f.clone()),
            &["mp4".into()],
            0,
            &CancelToken::new(),
        )
        .expect("single file bypasses cap");
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].as_path(), f);
    }
}
