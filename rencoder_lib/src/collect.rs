//! File collection for batch transcode.
//!
//! Walks a directory (or accepts a single file) and returns paths matching
//! the given extension allowlist. Symlinks are never followed (R10) so a
//! loop in the filesystem doesn't recurse forever.

use std::path::{Path, PathBuf};

use crate::error::EncodeError;
use crate::types::Target;

/// Collect input files for a transcode batch.
///
/// - `Target::File(p)` returns `[p]` if `p` exists as a file and matches
///   the extension filter (empty extensions = match all).
/// - `Target::Dir(root)` walks the tree without following symlinks, returns
///   every regular file that matches the filter.
///
/// `extensions` is a case-insensitive allowlist; an empty slice means "all
/// files".
pub fn collect_files(target: &Target, extensions: &[String]) -> Result<Vec<PathBuf>, EncodeError> {
    match target {
        Target::File(p) => Ok(collect_single_file(p, extensions)),
        Target::Dir(root) => collect_dir(root, extensions),
    }
}

fn collect_single_file(p: &Path, extensions: &[String]) -> Vec<PathBuf> {
    if p.is_file() && matches_extension(p, extensions) {
        vec![p.to_path_buf()]
    } else {
        vec![]
    }
}

fn collect_dir(root: &Path, extensions: &[String]) -> Result<Vec<PathBuf>, EncodeError> {
    let mut results = Vec::new();
    let walker = walkdir::WalkDir::new(root).follow_links(false);
    for entry in walker {
        let entry = entry.map_err(|e| EncodeError::Probe(format!("walkdir: {e}")))?;
        let path = entry.path();
        // Use walkdir's reported file_type(), NOT `Path::is_file()`. The
        // latter calls `std::fs::metadata` which follows symlinks, so a
        // symlink to a file would slip past `follow_links(false)`. With
        // follow_links(false) walkdir reports symlinks with is_file()=false,
        // is_symlink()=true -- exactly what we want.
        if entry.file_type().is_file() && matches_extension(path, extensions) {
            results.push(path.to_path_buf());
        }
    }
    Ok(results)
}

fn matches_extension(path: &Path, extensions: &[String]) -> bool {
    if extensions.is_empty() {
        return true;
    }
    match path.extension().and_then(|e| e.to_str()) {
        Some(ext) => extensions
            .iter()
            .any(|allowed| allowed.eq_ignore_ascii_case(ext)),
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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

    fn touch(dir: &Path, name: &str) -> PathBuf {
        let p = dir.join(name);
        fs::write(&p, b"x").unwrap();
        p
    }

    // ---- single-file target ----

    #[test]
    fn file_target_with_matching_extension_is_collected() {
        let dir = fixture_dir("file_match");
        let f = touch(&dir, "movie.mp4");
        let files = collect_files(&Target::File(f.clone()), &["mp4".into()]).unwrap();
        assert_eq!(files, vec![f]);
    }

    #[test]
    fn file_target_with_non_matching_extension_is_skipped() {
        let dir = fixture_dir("file_miss");
        let f = touch(&dir, "movie.txt");
        let files = collect_files(&Target::File(f.clone()), &["mp4".into()]).unwrap();
        assert!(files.is_empty());
    }

    #[test]
    fn file_target_with_empty_extensions_always_matches() {
        let dir = fixture_dir("file_empty");
        let f = touch(&dir, "anything.xyz");
        let files = collect_files(&Target::File(f.clone()), &[]).unwrap();
        assert_eq!(files, vec![f]);
    }

    #[test]
    fn file_target_missing_returns_empty() {
        let files = collect_files(
            &Target::File(PathBuf::from("/nonexistent/never.mp4")),
            &["mp4".into()],
        )
        .unwrap();
        assert!(files.is_empty());
    }

    #[test]
    fn file_target_extension_match_is_case_insensitive() {
        let dir = fixture_dir("file_case");
        let f = touch(&dir, "movie.MP4");
        let files = collect_files(&Target::File(f.clone()), &["mp4".into()]).unwrap();
        assert_eq!(files, vec![f]);
    }

    // ---- directory target ----

    #[test]
    fn dir_filters_by_extension() {
        let dir = fixture_dir("dir_filter");
        touch(&dir, "a.mp4");
        touch(&dir, "b.txt");
        touch(&dir, "c.mkv");
        let files =
            collect_files(&Target::Dir(dir.clone()), &["mp4".into(), "mkv".into()]).unwrap();
        assert_eq!(files.len(), 2);
        assert!(files.iter().all(|p| {
            p.extension()
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
        let files = collect_files(&Target::Dir(dir.clone()), &[]).unwrap();
        assert_eq!(files.len(), 3);
    }

    #[test]
    fn dir_empty_returns_empty() {
        let dir = fixture_dir("dir_empty");
        let files = collect_files(&Target::Dir(dir.clone()), &["mp4".into()]).unwrap();
        assert!(files.is_empty());
    }

    #[test]
    fn dir_walks_subdirectories() {
        let dir = fixture_dir("dir_walk");
        let sub = dir.join("sub");
        fs::create_dir_all(&sub).unwrap();
        touch(&dir, "a.mp4");
        touch(&sub, "b.mp4");
        let files = collect_files(&Target::Dir(dir.clone()), &["mp4".into()]).unwrap();
        assert_eq!(files.len(), 2);
    }

    #[test]
    fn dir_skips_symlinks_to_files() {
        // Symlinks are never followed (follow_links(false)). Even if a
        // symlink points to a regular file inside the tree, it is reported
        // as a symlink and is_file() returns false -> excluded.
        let dir = fixture_dir("dir_symlink");
        let real = touch(&dir, "real.mp4");
        let link = dir.join("alias.mp4");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&real, &link).unwrap();
        #[cfg(not(unix))]
        {
            // Skip on non-Unix; the production code path is the same.
            let _ = link;
            return;
        }
        let files = collect_files(&Target::Dir(dir.clone()), &["mp4".into()]).unwrap();
        assert_eq!(files.len(), 1);
        assert!(files.contains(&real));
        assert!(!files.contains(&link));
    }

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
        #[cfg(unix)]
        std::os::unix::fs::symlink(&dir, &link).unwrap();
        #[cfg(not(unix))]
        {
            let _ = link;
            return;
        }
        let files = collect_files(&Target::Dir(dir.clone()), &["mp4".into()]).unwrap();
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
        let files = collect_files(&Target::Dir(dir.clone()), &["mp4".into()]).unwrap();
        assert_eq!(files.len(), 3);
    }
}
