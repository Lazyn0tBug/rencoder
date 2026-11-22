//! Single-instance guard: one instance per target tree (design doc
//! §3.11.1.1).
//!
//! **The mutex is the kernel's file lock.** The target directory holds a
//! marker file (`.rencoder-owner`); whoever holds the exclusive lock on
//! it holds the target. The kernel releases the lock when the process
//! dies (SIGKILL included), so there is no "is that pid still alive?"
//! question to answer -- and taking the lock is atomic, so two
//! instances can never both believe they own the target.
//!
//! **The scope is a tree, not a directory.** Collection is recursive: a
//! run on a parent directory picks up the inputs *and* the outputs of a
//! run on a child directory. Two runs overlapping that way write the
//! same files at the same time, and the products are garbage. So a
//! target is taken when
//!
//! - the target directory itself is held,
//! - an **ancestor** of it is held (that run collects this subtree), or
//! - a **descendant** of it is held (this run would collect that
//!   subtree).
//!
//! Ancestors are cheap to check (a few levels up, one non-blocking lock
//! attempt each). Descendants cost a tree walk -- it rides on the walk
//! the collection does anyway.
//!
//! **The marker is deleted when the target is released** (the name goes
//! first, the lock is released after): the application leaves no litter
//! in the user's directory.

use std::fs::{File, OpenOptions, TryLockError};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use walkdir::WalkDir;

/// The marker file's name inside the target directory.
pub const OWNER_FILE_NAME: &str = ".rencoder-owner";

/// Who holds a target.
///
/// The record is **information, not the lock**: the lock is the
/// kernel's. These two fields exist so the refusal can say *who* is
/// working there.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Holder {
    /// The holding process's pid.
    pub pid: u32,
    /// When it took the target (ISO 8601, wall clock).
    pub since: String,
}

/// Is a directory held by another instance?
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Occupancy {
    /// Nobody holds it: no marker, or the marker is not locked.
    Free,
    /// Another instance holds it. The record is best-effort: it may be
    /// unreadable, or still name the previous holder.
    Held(Option<Holder>),
}

/// Why the guard refused.
#[derive(Debug)]
pub enum GuardError {
    /// Another instance holds this target -- or an ancestor of it, or a
    /// directory inside it.
    Busy {
        /// The directory that is held.
        path: PathBuf,
        /// Who holds it, when the record could be read.
        holder: Option<Holder>,
    },
    /// The filesystem does not support the file lock this guard needs.
    Unsupported { path: PathBuf, source: io::Error },
    /// The target directory could not be used at all.
    Io { path: PathBuf, source: io::Error },
}

impl std::fmt::Display for GuardError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            GuardError::Busy { path, holder } => {
                write!(f, "another instance is working on {}", path.display())?;
                if let Some(holder) = holder {
                    write!(f, " (pid {}, since {})", holder.pid, holder.since)?;
                }
                Ok(())
            }
            GuardError::Unsupported { path, source } => write!(
                f,
                "cannot lock {} ({}): this filesystem may not support file locking",
                path.display(),
                source
            ),
            GuardError::Io { path, source } => {
                write!(f, "{}: {}", path.display(), source)
            }
        }
    }
}

impl std::error::Error for GuardError {}

/// The marker file's path in `dir`.
pub fn marker_path(dir: &Path) -> PathBuf {
    dir.join(OWNER_FILE_NAME)
}

/// The deepest directory that contains all of `paths`.
///
/// The scope of a batch that got a file list instead of a directory
/// (the GUI): the batch writes next to its inputs, so the scope is
/// the directory that covers them all.
pub fn common_ancestor(paths: &[PathBuf]) -> Option<PathBuf> {
    use std::path::Component;

    let mut iter = paths
        .iter()
        .map(|path| path.parent().unwrap_or(path).to_path_buf())
        .filter(|path| !path.as_os_str().is_empty());
    let first = iter.next()?;
    let mut shared: Vec<Component> = first.components().collect();
    for path in iter {
        let components: Vec<Component> = path.components().collect();
        let common = shared
            .iter()
            .zip(components.iter())
            .take_while(|(a, b)| a == b)
            .count();
        shared.truncate(common);
    }
    let mut out = PathBuf::new();
    for component in shared {
        out.push(component.as_os_str());
    }
    Some(out)
}

/// Is `dir` held by another instance?
///
/// The lock answers it: locking it succeeds when nobody holds it. A
/// marker we cannot open is treated as *held* -- refusing is safe,
/// racing is not.
pub fn occupancy(dir: &Path) -> Result<Occupancy, GuardError> {
    let path = marker_path(dir);
    let file = match OpenOptions::new().read(true).write(true).open(&path) {
        Ok(file) => file,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Occupancy::Free),
        Err(_) => return Ok(Occupancy::Held(None)),
    };
    match file.try_lock() {
        Ok(()) => {
            // We got the lock: nobody holds it. Give it back.
            let _ = file.unlock();
            Ok(Occupancy::Free)
        }
        Err(TryLockError::WouldBlock) => Ok(Occupancy::Held(read_holder(&path))),
        Err(TryLockError::Error(e)) => Err(GuardError::Unsupported { path, source: e }),
    }
}

/// Is the target held? (cheap: the ancestors and the target itself)
///
/// The check without the taking: the frontends use it to refuse a
/// target early, before anything is written. The real acquisition
/// re-checks atomically.
pub fn probe(dir: &Path) -> Result<(), GuardError> {
    let target = canonicalize(dir)?;
    for ancestor in target.ancestors().skip(1) {
        if let Occupancy::Held(holder) = occupancy(ancestor)? {
            return Err(GuardError::Busy {
                path: ancestor.to_path_buf(),
                holder,
            });
        }
    }
    match occupancy(&target)? {
        Occupancy::Free => Ok(()),
        Occupancy::Held(holder) => Err(GuardError::Busy {
            path: target,
            holder,
        }),
    }
}

/// A held target: this object owns the lock.
#[derive(Debug)]
pub struct TargetGuard {
    /// The locked marker file. Dropping it releases the lock -- it is
    /// held here for exactly that reason.
    #[allow(dead_code)]
    file: File,
    /// The marker path -- removed on release.
    marker: Option<PathBuf>,
    /// The directory this guard holds.
    target: PathBuf,
}

impl TargetGuard {
    /// Take the target.
    ///
    /// The checks in order: the ancestors (cheap), the target itself
    /// (the atomic part), then the descendants (a tree walk). A
    /// conflict anywhere gives the target back.
    pub fn acquire(dir: &Path) -> Result<Self, GuardError> {
        let target = canonicalize(dir)?;

        // Ancestors: a run on an ancestor collects this subtree.
        for ancestor in target.ancestors().skip(1) {
            if let Occupancy::Held(holder) = occupancy(ancestor)? {
                return Err(GuardError::Busy {
                    path: ancestor.to_path_buf(),
                    holder,
                });
            }
        }

        // The target itself: the atomic part.
        let marker = marker_path(&target);
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&marker)
            .map_err(|source| GuardError::Io {
                path: marker.clone(),
                source,
            })?;
        match file.try_lock() {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => {
                return Err(GuardError::Busy {
                    path: target,
                    holder: read_holder(&marker),
                });
            }
            Err(TryLockError::Error(e)) => {
                return Err(GuardError::Unsupported {
                    path: target,
                    source: e,
                })
            }
        }

        // The record: who holds it, since when. For the refusal
        // message.
        let record = Holder {
            pid: std::process::id(),
            since: chrono::Utc::now().to_rfc3339(),
        };
        let _ = write_holder(&mut file, &record);

        // Descendants: this run collects them. The same walk finds the
        // temp files of dead attempts -- we hold the target, so they
        // are all leftovers (design doc §3.11.2).
        let scan = scan_subtree(&target)?;
        if let Some((dir, holder)) = scan.conflict {
            // Give the target back before refusing.
            let _ = std::fs::remove_file(&marker);
            drop(file);
            return Err(GuardError::Busy { path: dir, holder });
        }
        for leftover in scan.temp_files {
            if let Err(e) = std::fs::remove_file(&leftover) {
                if e.kind() != io::ErrorKind::NotFound {
                    tracing::warn!(
                        error = %e,
                        path = %leftover.display(),
                        "could not remove a leftover temp file"
                    );
                }
            }
        }

        Ok(TargetGuard {
            file,
            marker: Some(marker),
            target,
        })
    }

    /// The directory this guard holds.
    pub fn target(&self) -> &Path {
        &self.target
    }

    /// Release the target.
    pub fn release(mut self) {
        self.release_inner();
    }

    fn release_inner(&mut self) {
        if let Some(marker) = self.marker.take() {
            // The name goes first, the lock is released when the file
            // is closed (design doc §3.11.1.1): while the name is gone
            // we still hold the lock, so nobody can take the target
            // believing it is free.
            if let Err(e) = std::fs::remove_file(&marker) {
                if e.kind() != io::ErrorKind::NotFound {
                    tracing::warn!(
                        error = %e,
                        path = %marker.display(),
                        "could not remove the target marker"
                    );
                }
            }
        }
    }
}

impl Drop for TargetGuard {
    fn drop(&mut self) {
        self.release_inner();
    }
}

/// What one walk over the target's tree finds.
struct SubtreeScan {
    /// A directory inside the tree that another instance holds.
    conflict: Option<(PathBuf, Option<Holder>)>,
    /// The temp files of dead attempts (design doc §3.11.2).
    temp_files: Vec<PathBuf>,
}

/// Walk the target's tree once: who holds a directory inside it, and
/// which temp files are left over.
///
/// The root is ours -- we just locked it -- so it is not a conflict.
fn scan_subtree(root: &Path) -> Result<SubtreeScan, GuardError> {
    let mut scan = SubtreeScan {
        conflict: None,
        temp_files: Vec::new(),
    };
    for entry in WalkDir::new(root).follow_links(false).into_iter() {
        let entry = entry.map_err(|e| GuardError::Io {
            path: root.to_path_buf(),
            source: e.into(),
        })?;
        let path = entry.path();
        if entry.file_type().is_dir() {
            // The root is ours -- we just locked it.
            if path == root {
                continue;
            }
            if scan.conflict.is_none() {
                if let Occupancy::Held(holder) = occupancy(path)? {
                    scan.conflict = Some((path.to_path_buf(), holder));
                }
            }
        } else if entry.file_type().is_file() && crate::utils::fs::is_temp_file(path) {
            scan.temp_files.push(path.to_path_buf());
        }
    }
    Ok(scan)
}

/// The directory as the filesystem sees it: symlinks resolved, so two
/// paths to one directory share one marker.
fn canonicalize(dir: &Path) -> Result<PathBuf, GuardError> {
    dir.canonicalize().map_err(|source| GuardError::Io {
        path: dir.to_path_buf(),
        source,
    })
}

/// The holder record, when it can be read.
fn read_holder(path: &Path) -> Option<Holder> {
    let raw = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&raw).ok()
}

/// Write the holder record into the marker.
fn write_holder(file: &mut File, holder: &Holder) -> io::Result<()> {
    let json =
        serde_json::to_vec(holder).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    file.set_len(0)?;
    file.write_all(&json)?;
    file.flush()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A scratch directory tree: `<dir>/app`, `<dir>/app/sub`.
    fn scratch(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "rencoder_guard_{label}_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or_default()
        ));
        std::fs::create_dir_all(dir.join("app/sub")).expect("the scratch dir must exist");
        dir
    }

    #[test]
    fn taking_a_free_target_works_and_writes_the_record() {
        let dir = scratch("take");
        let app = dir.join("app");

        let guard = TargetGuard::acquire(&app).expect("the target is free");

        // The marker is there, and it says who holds it.
        let marker = marker_path(&app);
        assert!(marker.exists(), "the marker must exist while held");
        let holder = read_holder(&marker).expect("the record must be readable");
        assert_eq!(holder.pid, std::process::id());

        guard.release();

        // Released: the marker is gone, the target is free again.
        assert!(!marker.exists(), "the marker must be removed on release");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_held_target_refuses_the_second_taker() {
        let dir = scratch("busy");
        let app = dir.join("app");

        let _guard = TargetGuard::acquire(&app).expect("the target is free");

        // A second lock attempt on the same file (a different
        // descriptor) is refused -- that is what the kernel's lock
        // guarantees.
        match TargetGuard::acquire(&app) {
            Err(GuardError::Busy { path, .. }) => {
                assert_eq!(path, app.canonicalize().unwrap());
            }
            other => panic!("the target must be refused: {other:?}"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_held_ancestor_refuses_the_child() {
        let dir = scratch("ancestor");
        let app = dir.join("app");
        let sub = app.join("sub");

        let _guard = TargetGuard::acquire(&app).expect("the target is free");

        // The child: its ancestor is held.
        match TargetGuard::acquire(&sub) {
            Err(GuardError::Busy { path, .. }) => {
                assert_eq!(path, app.canonicalize().unwrap());
            }
            other => panic!("the child must be refused: {other:?}"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_held_descendant_refuses_the_parent() {
        let dir = scratch("descendant");
        let app = dir.join("app");
        let sub = app.join("sub");

        let _guard = TargetGuard::acquire(&sub).expect("the target is free");

        // The parent: this run would collect the child's files.
        match TargetGuard::acquire(&app) {
            Err(GuardError::Busy { path, .. }) => {
                assert_eq!(path, sub.canonicalize().unwrap());
            }
            other => panic!("the parent must be refused: {other:?}"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_free_tree_is_free() {
        let dir = scratch("free");
        let app = dir.join("app");

        probe(&app).expect("nothing holds this tree");
        assert!(TargetGuard::acquire(&app).is_ok());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn taking_the_target_removes_the_leftover_temp_files() {
        // We hold the target, so every temp file in it belongs to a
        // dead attempt (design doc §3.11.2).
        let dir = scratch("leftovers");
        let app = dir.join("app");
        let leftover = app.join(".rencoder-tmp.clip.mkv");
        std::fs::write(&leftover, b"partial").unwrap();

        let _guard = TargetGuard::acquire(&app).expect("the target is free");

        assert!(
            !leftover.exists(),
            "the leftover temp file must be gone: {}",
            leftover.display()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_marker_is_not_collected_as_an_input() {
        // The marker lives in the target directory; the collector must
        // not pick it up (design doc §3.6). It has no video extension,
        // but an empty extension list matches everything -- so the
        // name-based skip is what keeps it out.
        let dir = scratch("collected");
        let app = dir.join("app");
        let _guard = TargetGuard::acquire(&app).expect("the target is free");
        assert!(marker_path(&app).exists());

        let files = crate::collect::collect_files(&crate::types::Target::Dir(app.clone()), &[], 10)
            .expect("the collection must work");
        assert!(
            files.is_empty(),
            "the marker must not be an input: {files:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
