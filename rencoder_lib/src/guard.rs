//! Single-instance guard: one instance per target tree (design doc
//! §3.11.1.1).
//!
//! **The mutex is the kernel's.** The target directory holds a marker
//! file (`.rencoder-owner`); whoever holds the kernel's claim on it
//! holds the target. The kernel releases the claim when the process
//! dies (SIGKILL included), so there is no "is that pid still alive?"
//! question to answer -- and taking it is atomic, so two instances can
//! never both believe they own the target.
//!
//! **The claim is not the same mechanism on every platform.** The
//! primitive has to satisfy five properties (design doc §3.11.1.1) --
//! atomic takeover, release on process death, probeable without
//! interfering, **does not block reads**, located by path -- and the
//! two platforms reach them differently. The mechanism lives in
//! [`crate::platform::lock`]; this module only sees the contract:
//! `claim` / `probe` and `Busy` / `Unreadable` / `Unsupported`:
//!
//! - Unix: std's `flock`, an *advisory* lock. It does not block
//!   reads, so the record stays readable while the target is held.
//! - Windows: the **share mode** of the open. std's `LockFileEx` is a
//!   *mandatory* lock over the whole file: while it is held no other
//!   handle can even read the file, so the record would be
//!   unreachable and the refusal message would lose the pid. The
//!   share mode denies *writers* instead -- the holder opens for
//!   writing and lets others read and delete -- so a second writer
//!   fails with `ERROR_SHARING_VIOLATION` while the record stays
//!   readable.
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

use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use walkdir::WalkDir;

use crate::platform::lock::{self, Claim};

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

    // The parents: the batch writes next to its inputs. A relative
    // name has no parent -- it means the current directory, and
    // dropping it would make the scope too narrow (the batch writes
    // there too).
    let cwd = std::env::current_dir().ok();
    let parents: Vec<PathBuf> = paths
        .iter()
        .map(|path| {
            path.parent()
                .filter(|parent| !parent.as_os_str().is_empty())
                .map(Path::to_path_buf)
                .or_else(|| cwd.clone())
                .unwrap_or_else(|| path.clone())
        })
        .collect();

    let first = parents.first()?;
    let mut shared: Vec<Component> = first.components().collect();
    for parent in &parents[1..] {
        let components: Vec<Component> = parent.components().collect();
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
/// The lock answers it: a **shared** lock can only be taken when
/// nobody holds the exclusive one ✓. The file is opened **read-only**
/// ✓: reading is enough to ask ✓, and a marker owned by another user
/// must not look "held" just because we cannot open it for writing
/// ✓ (that would block the directory for everyone, forever ✓).
pub fn occupancy(dir: &Path) -> Result<Occupancy, GuardError> {
    let path = marker_path(dir);
    match lock::probe(&path) {
        // Nobody holds it: no marker, or the marker is not claimed.
        Ok(()) => Ok(Occupancy::Free),
        // We cannot even read it: we cannot tell, and refusing is
        // safer than racing.
        Err(lock::Error::Unreadable(_)) => Ok(Occupancy::Held(None)),
        Err(lock::Error::Busy) => Ok(Occupancy::Held(read_holder(&path))),
        Err(lock::Error::Unsupported(e)) => Err(GuardError::Unsupported { path, source: e }),
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
    /// The claim on the marker. Dropping it releases the claim -- it
    /// is held here for exactly that reason.
    #[allow(dead_code)]
    claim: Claim,
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
        let mut claim = match lock::claim(&marker) {
            Ok(claim) => claim,
            Err(lock::Error::Busy) => {
                return Err(GuardError::Busy {
                    path: target,
                    holder: read_holder(&marker),
                });
            }
            Err(lock::Error::Unreadable(e)) => {
                return Err(GuardError::Io {
                    path: marker,
                    source: e,
                })
            }
            Err(lock::Error::Unsupported(e)) => {
                return Err(GuardError::Unsupported {
                    path: marker,
                    source: e,
                })
            }
        };

        // The record: who holds it, since when. For the refusal
        // message.
        let record = Holder {
            pid: std::process::id(),
            since: chrono::Utc::now().to_rfc3339(),
        };
        let _ = write_holder(&mut claim, &record);

        // Descendants: this run collects them.
        if let Some((dir, holder)) = held_descendant(&target)? {
            // Give the target back before refusing.
            let _ = std::fs::remove_file(&marker);
            drop(claim);
            return Err(GuardError::Busy { path: dir, holder });
        }

        Ok(TargetGuard {
            claim,
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

/// Is a directory inside `root` held by another instance?
///
/// The root is ours -- we just locked it -- so it is not a conflict.
fn held_descendant(root: &Path) -> Result<Option<(PathBuf, Option<Holder>)>, GuardError> {
    for entry in WalkDir::new(root)
        .follow_links(false)
        .into_iter()
        .filter_entry(|entry| entry.file_type().is_dir())
    {
        let entry = entry.map_err(|e| GuardError::Io {
            path: root.to_path_buf(),
            source: e.into(),
        })?;
        let dir = entry.path();
        // The root is ours -- we just locked it.
        if dir == root {
            continue;
        }
        if let Occupancy::Held(holder) = occupancy(dir)? {
            return Ok(Some((dir.to_path_buf(), holder)));
        }
    }
    Ok(None)
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
fn write_holder(claim: &mut Claim, holder: &Holder) -> io::Result<()> {
    let json =
        serde_json::to_vec(holder).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    claim.write_record(&json)
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
    fn the_refusal_carries_the_holder_pid() {
        // The promise §3.11.1.1 makes: "another instance is working on
        // <dir> (pid 1234, since 15:00)". Nothing guarded it -- the
        // refusal tests only looked at the path -- so the record could
        // stop being readable and nobody would notice. On Windows it
        // *did* stop being readable: the std lock is a mandatory lock
        // over the whole file, so no other handle could read it while
        // the target was held.
        //
        // A second handle in the same process is enough: Windows locks
        // per handle and Unix's flock is per open file description, so
        // both platforms really collide -- no second process needed.
        let dir = scratch("holder_pid");
        let app = dir.join("app");

        let guard = TargetGuard::acquire(&app).expect("the target is free");

        match TargetGuard::acquire(&app) {
            Err(GuardError::Busy { holder, .. }) => {
                let holder = holder.expect("the refusal must carry the holder");
                assert_eq!(holder.pid, std::process::id());
            }
            other => panic!("the second taker must be refused: {other:?}"),
        }

        drop(guard);
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

    #[cfg(unix)]
    #[test]
    fn a_marker_we_cannot_write_does_not_block_the_directory() {
        // Another user's marker: readable, not writable. It must not
        // look "held" -- that would block the directory for everyone,
        // forever (design doc §3.11.1.1).
        use std::os::unix::fs::PermissionsExt;
        let dir = scratch("read_only_marker");
        let app = dir.join("app");
        std::fs::create_dir_all(&app).unwrap();
        let marker = marker_path(&app);
        std::fs::write(&marker, b"{\"pid\": 1}").unwrap();
        std::fs::set_permissions(&marker, std::fs::Permissions::from_mode(0o444)).unwrap();

        // The probe asks with a read-only handle: nobody holds it.
        assert_eq!(
            occupancy(&app).expect("the probe must work"),
            Occupancy::Free,
            "a marker we cannot write must not read as held"
        );

        // And we can take the target over: the lock is the state, the
        // record is best-effort.
        let guard = TargetGuard::acquire(&app).expect("the target must be takeable");
        drop(guard);
        // Released: the marker is gone (we may unlink it -- the
        // directory is ours).
        assert!(!marker.exists(), "the marker must be removed on release");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_scope_covers_a_relative_path() {
        // A relative name means the current directory: dropping it
        // would make the scope too narrow (the batch writes there too).
        let dir = scratch("relative_scope");
        let scope = common_ancestor(&[PathBuf::from("clip.mp4")]).expect("a scope");
        assert_eq!(
            scope,
            std::env::current_dir().expect("the current dir"),
            "a relative name means the current directory"
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

        let files = crate::collect::collect_files(
            &crate::types::Target::Dir(app.clone()),
            &[],
            10,
            &crate::pipeline::cancel::CancelToken::new(),
        )
        .expect("the collection must work");
        assert!(
            files.is_empty(),
            "the marker must not be an input: {files:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
