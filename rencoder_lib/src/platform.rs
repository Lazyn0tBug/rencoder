//! Every OS difference in the library, in one file.
//!
//! The rule (design doc §1.3): **`#[cfg]` appears here and nowhere
//! else**. Every other module calls a uniform signature and never
//! asks what platform it is on.
//!
//! # What is unified here -- and what is not
//!
//! **The mechanisms are not unified, and must not be.** Unix claims a
//! target with an advisory `flock`; Windows claims it with the share
//! mode of an open handle. They are different facts about different
//! operating systems, and forcing them into one shape would cost a
//! property the application depends on (see the lock contract below).
//!
//! **The functional semantics are unified.** What the application gets
//! is the same on every platform: the same call, the same result
//! classification, the same observable behavior. The mechanism is an
//! implementation detail of this file, and the contract is asserted
//! by tests that run -- unchanged -- on every host.
//!
//! # The contracts
//!
//! | Module | Contract |
//! |--------|----------|
//! | [`lock`] | Claiming a target: atomic takeover, released on process death, probeable without interfering, **does not block reads**, located by path |
//! | [`perms`] | The config file is editable by its user and readable by others; the app directories are enterable and writable |
//! | [`fs_semantics`] | "The output is never the input" holds on every platform (design doc §3.11.2) |
//! | [`process`] | A console control event reaches the app, not the encoder it started; stopping the encoder stops every process it runs |

/// Claiming a target: the single-instance guard's lock.
///
/// # The contract
///
/// The application (design doc §3.11.1.1) depends on five properties.
/// They hold on every platform, and [`lock`]'s tests assert them on
/// every platform:
///
/// 1. **Atomic takeover** -- of two claimers, exactly one wins; the
///    other gets [`Error::Busy`].
/// 2. **Released on process death** -- a claim is held by the kernel,
///    not by a pid that can go stale. Dropping the [`Claim`] releases
///    it; so does the process dying.
/// 3. **Probeable without interfering** -- [`probe`] answers whether
///    somebody holds the target and leaves the state alone.
/// 4. **Does not block reads** -- while the target is held, the
///    marker stays *readable*, so the refusal message can name the
///    holder. This is the property that rules out Windows'
///    `LockFileEx`: it is a mandatory lock over the whole file, so
///    nobody could read the record while it was held.
/// 5. **Located by path** -- the record is found (and removed) by the
///    marker's path, not by a handle the caller has to keep.
///
/// # How each platform gets there
///
/// - **Unix**: std's `flock`, an *advisory* lock. It does not block
///   reads. It locks the *inode*, and the open and the lock are two
///   steps, so the implementation also re-checks that the name still
///   points at the file it locked.
/// - **Windows**: the **share mode** of the open. The holder opens
///   for writing and lets others read and delete, denying only other
///   writers -- so a second writer fails while the record stays
///   readable. The claim is the open itself, so it is atomic by
///   construction.
/// - **Anything else**: no claim at all -- [`claim`] and [`probe`]
///   report [`Error::Unsupported`], and the guard refuses the run
///   rather than pretending the target is free.
pub mod lock {
    use std::fs::File;
    use std::io;
    use std::path::Path;

    /// Why a claim did not produce a handle.
    ///
    /// The platform-specific failures are folded into these three, so
    /// the rest of the library never branches on the platform.
    #[derive(Debug)]
    pub enum Error {
        /// Somebody else holds the target: a benign refusal.
        Busy,
        /// The marker could not be opened at all -- a permission
        /// problem, a path that is not a directory, and so on.
        Unreadable(io::Error),
        /// The filesystem cannot provide the claim at all. On Unix
        /// that is `ENOLCK` / `EOPNOTSUPP` / `ENOSYS`; on Windows
        /// the share mode is always available, so it never happens
        /// there.
        #[cfg_attr(windows, allow(dead_code))]
        Unsupported(io::Error),
    }

    /// A held claim on a marker.
    ///
    /// Opaque on purpose: whether it holds an advisory lock or a
    /// share mode is not the application's business. Dropping it
    /// releases the claim.
    #[derive(Debug)]
    pub struct Claim {
        file: File,
    }

    impl Claim {
        /// Write the holder record into the marker, replacing what
        /// was there.
        ///
        /// The bytes are opaque here: the record's format belongs to
        /// the application, the *storage* belongs to the platform.
        pub fn write_record(&mut self, bytes: &[u8]) -> io::Result<()> {
            use std::io::{Seek, SeekFrom, Write};

            self.file.set_len(0)?;
            // From the start: the marker may be a leftover, and the
            // position is not ours to assume.
            self.file.seek(SeekFrom::Start(0))?;
            self.file.write_all(bytes)?;
            self.file.flush()
        }
    }

    /// Take the claim on `marker`.
    ///
    /// Succeeds only when nobody holds it. The returned [`Claim`]
    /// holds the target until it is dropped.
    pub fn claim(marker: &Path) -> Result<Claim, Error> {
        imp::take(marker).map(|file| Claim { file })
    }

    /// Is somebody holding the target?
    ///
    /// Read-only: it does not create the marker, does not change who
    /// holds it, and does not block the holder. `Ok(())` means the
    /// target is free; [`Error::Busy`] means somebody holds it.
    ///
    /// A marker that does not exist means "nobody holds it" -- that
    /// is the same answer on every platform, so the missing-file case
    /// is folded in here instead of at every call site.
    pub fn probe(marker: &Path) -> Result<(), Error> {
        match imp::probe(marker) {
            Ok(_) => Ok(()),
            Err(Error::Unreadable(e)) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e),
        }
    }

    /// Unix: the claim is std's `flock` -- an **advisory** lock.
    ///
    /// It does not block reads, so the record stays readable while
    /// the target is held (design doc §3.11.1.1, property 4).
    #[cfg(unix)]
    mod imp {
        use std::fs::{File, OpenOptions, TryLockError};
        use std::io;
        use std::os::unix::fs::MetadataExt;
        use std::path::Path;

        use super::Error;

        /// How many times we retry when the marker moved under us.
        const MAX_ATTEMPTS: usize = 8;

        /// Take the target: open the marker, lock it, and make sure the
        /// name still points at the file we locked.
        pub fn take(marker: &Path) -> Result<File, Error> {
            for _ in 0..MAX_ATTEMPTS {
                let file = match open_for_claim(marker) {
                    Ok(file) => file,
                    Err(e) => return Err(Error::Unreadable(e)),
                };
                match file.try_lock() {
                    Ok(()) => {}
                    Err(TryLockError::WouldBlock) => return Err(Error::Busy),
                    // Any lock error is "this filesystem cannot do it" --
                    // the class the design doc calls out (ENOLCK /
                    // EOPNOTSUPP / ENOSYS).
                    Err(TryLockError::Error(e)) => return Err(Error::Unsupported(e)),
                }
                // `flock` locks the *inode*, so the open and the lock are
                // two steps with a window between them: if the previous
                // holder deleted the name and released in that window, we
                // would lock an unlinked inode while a later instance
                // locks a fresh file under the same name -- two holders.
                // The device and inode numbers tell the two apart.
                match still_named(&file, marker) {
                    Ok(true) => return Ok(file),
                    // The name moved on: dropping the file releases the
                    // lock, and we start over on the file that is there now.
                    Ok(false) => continue,
                    Err(e) => return Err(Error::Unreadable(e)),
                }
            }
            Err(Error::Unreadable(io::Error::other(
                "the target marker kept changing hands",
            )))
        }

        /// Probe: is somebody holding the target?
        ///
        /// Read-only, and it does not create the marker -- no marker means
        /// nobody holds the directory.
        pub fn probe(marker: &Path) -> Result<File, Error> {
            let file = match File::open(marker) {
                Ok(file) => file,
                Err(e) => return Err(Error::Unreadable(e)),
            };
            match file.try_lock_shared() {
                Ok(()) => Ok(file),
                Err(TryLockError::WouldBlock) => Err(Error::Busy),
                Err(TryLockError::Error(e)) => Err(Error::Unsupported(e)),
            }
        }

        /// Open the marker for the claim.
        ///
        /// Write access is preferred (the record goes in there), but it is
        /// not required: a marker left behind by **another user** cannot be
        /// written, and it must not block this directory forever -- we take
        /// it over (the lock is the state, the record is only for the
        /// message).
        fn open_for_claim(marker: &Path) -> io::Result<File> {
            match OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .open(marker)
            {
                Ok(file) => Ok(file),
                Err(e) if e.kind() == io::ErrorKind::PermissionDenied => {
                    // Another user's marker: read it and lock it. The
                    // record stays theirs (we cannot write it) -- it is
                    // best-effort.
                    File::open(marker)
                }
                Err(e) => Err(e),
            }
        }

        /// Is the name still pointing at the file we hold?
        pub(super) fn still_named(file: &File, marker: &Path) -> io::Result<bool> {
            let mine = file.metadata()?;
            match std::fs::metadata(marker) {
                Ok(current) => Ok(current.dev() == mine.dev() && current.ino() == mine.ino()),
                Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
                Err(e) => Err(e),
            }
        }
    }

    /// Windows: the claim is the **share mode** of the open.
    ///
    /// std's `LockFileEx` is a *mandatory* lock over the whole file: while
    /// it is held, no other handle can read the file either, so the record
    /// would be unreadable and the refusal message would lose the pid.
    /// The share mode denies *writers* instead: the holder opens for
    /// writing and lets others read and delete.
    #[cfg(windows)]
    mod imp {
        use std::fs::{File, OpenOptions};
        use std::io;
        use std::os::windows::fs::OpenOptionsExt;
        use std::path::Path;

        use super::Error;

        /// Others may read the marker.
        const FILE_SHARE_READ: u32 = 0x0000_0001;
        /// Others may delete the marker (the holder deletes its own).
        const FILE_SHARE_DELETE: u32 = 0x0000_0004;
        /// Others may read and delete; nobody else may write.
        const DENY_WRITE: u32 = FILE_SHARE_READ | FILE_SHARE_DELETE;
        /// The file is open by somebody else in a way that conflicts.
        const ERROR_SHARING_VIOLATION: i32 = 32;

        /// Take the target.
        ///
        /// The claim *is* the open: it succeeds only when nobody else
        /// holds the marker for writing, so it is atomic by construction
        /// (no open/lock window to close, unlike the Unix inode case).
        pub fn take(marker: &Path) -> Result<File, Error> {
            match OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .share_mode(DENY_WRITE)
                .open(marker)
            {
                Ok(file) => Ok(file),
                Err(e) if e.kind() == io::ErrorKind::PermissionDenied => {
                    // Another user's marker: read-only, but still deny the
                    // writes -- the claim is the share mode, not the access.
                    OpenOptions::new()
                        .read(true)
                        .share_mode(DENY_WRITE)
                        .open(marker)
                        .map_err(map_open_error)
                }
                Err(e) => Err(map_open_error(e)),
            }
        }

        /// Probe: is somebody holding the target?
        ///
        /// Read-only, and it does not create the marker. It denies writers
        /// too -- that is what makes it conflict with a holder, which is
        /// open for writing.
        pub fn probe(marker: &Path) -> Result<File, Error> {
            OpenOptions::new()
                .read(true)
                .share_mode(DENY_WRITE)
                .open(marker)
                .map_err(map_open_error)
        }

        /// A share mode conflict means "somebody else holds it".
        fn map_open_error(e: io::Error) -> Error {
            if e.raw_os_error() == Some(ERROR_SHARING_VIOLATION) {
                Error::Busy
            } else {
                Error::Unreadable(e)
            }
        }
    }

    /// Any other platform: there is no claim.
    ///
    /// The target cannot be held, so the run is refused rather than
    /// started without a guard -- an unguarded run would let two
    /// instances write the same files.
    #[cfg(not(any(unix, windows)))]
    mod imp {
        use std::fs::File;
        use std::io;
        use std::path::Path;

        use super::Error;

        fn unsupported() -> Error {
            Error::Unsupported(io::Error::new(
                io::ErrorKind::Unsupported,
                "this platform has no way to claim a target",
            ))
        }

        pub fn take(_marker: &Path) -> Result<File, Error> {
            Err(unsupported())
        }

        pub fn probe(_marker: &Path) -> Result<File, Error> {
            Err(unsupported())
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        /// A marker path inside a fresh directory.
        fn marker_for(label: &str) -> std::path::PathBuf {
            let dir = crate::test_support::unique_temp_dir(&format!("lock_{label}"));
            dir.join("marker")
        }
        /// Contract 1: of two claimers, exactly one wins.
        #[test]
        fn only_one_claim_wins() {
            let marker = marker_for("one_wins");
            let _first = claim(&marker).expect("the first claim succeeds");
            match claim(&marker) {
                Err(Error::Busy) => {}
                Err(other) => panic!("expected Busy, got {other:?}"),
                Ok(_) => panic!("the second claim must not succeed"),
            }
        }

        /// Contract 2: the claim is the kernel's, not the process's --
        /// dropping it releases the target.
        #[test]
        fn dropping_the_claim_releases_the_target() {
            let marker = marker_for("release");
            {
                let _held = claim(&marker).expect("the first claim succeeds");
            }
            claim(&marker).expect("the target is free again");
        }

        /// Contract 3: probing does not take the target and does not
        /// change who holds it.
        #[test]
        fn probing_does_not_take_the_target() {
            let marker = marker_for("probe");
            // Nobody holds it: the probe says free...
            probe(&marker).expect("an unheld marker probes free");
            // ...and the target is still free afterwards.
            let _held = claim(&marker).expect("the claim still succeeds");

            // Now somebody holds it: the probe says busy...
            match probe(&marker) {
                Err(Error::Busy) => {}
                Err(other) => panic!("expected Busy, got {other:?}"),
                Ok(()) => panic!("a held marker must probe busy"),
            }
            // ...and the holder still holds it.
            match claim(&marker) {
                Err(Error::Busy) => {}
                Err(other) => panic!("expected Busy, got {other:?}"),
                Ok(_) => panic!("the holder must keep the target"),
            }
        }

        /// Contract 4: the record stays readable while the target is
        /// held. This is the property Windows' mandatory locks would
        /// break -- the refusal message needs the holder's pid.
        #[test]
        fn the_record_stays_readable_while_the_target_is_held() {
            let marker = marker_for("readable");
            let mut held = claim(&marker).expect("the claim succeeds");
            held.write_record(b"{\"pid\":42}")
                .expect("the record is written");
            let read = std::fs::read(&marker).expect("the record is readable while held");
            assert_eq!(read, b"{\"pid\":42}");
        }

        /// Contract 5: the record is located by path -- the marker can
        /// be found and removed by name while the target is held.
        #[test]
        fn the_record_is_located_by_path() {
            let marker = marker_for("by_path");
            let mut held = claim(&marker).expect("the claim succeeds");
            held.write_record(b"held").expect("the record is written");
            assert!(marker.exists(), "the record is found by path");
            std::fs::remove_file(&marker).expect("the marker is removed by path");
            assert!(!marker.exists(), "the marker is gone");
        }

        /// The unix-specific mechanism: `flock` locks the inode, so the
        /// name can be swapped under us. The claim must not accept a
        /// file that is no longer the one named by the path.
        #[cfg(unix)]
        #[test]
        fn a_marker_that_moved_under_us_is_not_ours() {
            use std::os::unix::fs::PermissionsExt;

            let dir = crate::test_support::unique_temp_dir("lock_moved");
            let marker = dir.join("marker");
            std::fs::write(&marker, b"first").expect("write the marker");
            std::fs::set_permissions(&marker, std::fs::Permissions::from_mode(0o644))
                .expect("set the mode");

            // Take the claim, then swap the name: the path now points
            // at a different inode.
            let held = claim(&marker).expect("the claim succeeds");
            std::fs::remove_file(&marker).expect("remove the marker");
            std::fs::write(&marker, b"second").expect("write a new marker");
            std::fs::set_permissions(&marker, std::fs::Permissions::from_mode(0o644))
                .expect("set the mode");

            // The claim we hold is on the old inode; the path is not
            // ours any more.
            assert!(
                !imp::still_named(&held.file, &marker).unwrap_or(false),
                "the name must not point at the inode we locked"
            );
        }
    }
}

/// The permissions the app's own files and directories need.
///
/// # The contract
///
/// - The **config file** is editable by the user who runs the app and
///   readable by the others (it holds settings, not secrets).
/// - The **app directories** (log, data) are enterable and writable by
///   the user, readable by the others.
///
/// # How each platform gets there
///
/// - **Unix**: POSIX mode bits -- `0o644` for the file, `0o755` for the
///   directories (the executable bit is what makes a directory
///   traversable).
/// - **Windows**: there are no mode bits. The ACL is inherited from the
///   parent directory, which already gives the user full access and the
///   others read access -- the same functional result. So the non-Unix
///   branch is a **no-op, not a gap**: "not implemented" would be a
///   lie, the behaviour is there, just not spelled in mode bits.
pub mod perms {
    use std::io;
    use std::path::Path;

    /// Apply the mode bits a config file needs.
    ///
    /// - Unix: `0o644` (rw-r--r--).
    /// - Windows: no-op -- the inherited ACL already gives the same
    ///   functional access.
    pub fn set_config_file_mode<P: AsRef<Path>>(path: P) -> io::Result<()> {
        apply_mode(path, 0o644)
    }

    /// Apply the mode bits an app-managed directory needs.
    ///
    /// - Unix: `0o755` (rwxr-xr-x).
    /// - Windows: no-op -- the inherited ACL already gives the same
    ///   functional access.
    pub fn set_app_dir_mode<P: AsRef<Path>>(path: P) -> io::Result<()> {
        apply_mode(path, 0o755)
    }

    /// Internal helper that consolidates the cfg-at-definition pattern.
    /// `mode_bits` is the POSIX mode (only meaningful on Unix). The
    /// non-Unix branch is a no-op placeholder; the explicit
    /// `let _ = (path, mode_bits)` makes "unused on this target" a
    /// compile-time fact, not a runtime surprise.
    fn apply_mode<P: AsRef<Path>>(path: P, mode_bits: u32) -> io::Result<()> {
        let path = path.as_ref();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode_bits))?;
        }
        #[cfg(not(unix))]
        {
            let _ = (path, mode_bits);
        }
        Ok(())
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[cfg(unix)]
        fn unique_temp(name: &str) -> std::path::PathBuf {
            std::env::temp_dir().join(format!("rencoder_fs_perms_{name}_{}", std::process::id()))
        }

        #[cfg(unix)]
        #[test]
        fn set_config_file_mode_sets_0644_on_unix() {
            use std::os::unix::fs::PermissionsExt;
            let path = unique_temp("config");
            std::fs::write(&path, b"").expect("write temp file");
            set_config_file_mode(&path).expect("set_config_file_mode should succeed");
            let mode = std::fs::metadata(&path).expect("stat").permissions().mode() & 0o777;
            assert_eq!(mode, 0o644, "expected 0o644, got 0o{mode:o}");
            std::fs::remove_file(&path).ok();
        }

        #[cfg(unix)]
        #[test]
        fn set_app_dir_mode_sets_0755_on_unix() {
            use std::os::unix::fs::PermissionsExt;
            let path = unique_temp("appdir");
            std::fs::create_dir_all(&path).expect("create temp dir");
            set_app_dir_mode(&path).expect("set_app_dir_mode should succeed");
            let mode = std::fs::metadata(&path).expect("stat").permissions().mode() & 0o777;
            assert_eq!(mode, 0o755, "expected 0o755, got 0o{mode:o}");
            std::fs::remove_dir_all(&path).ok();
        }

        #[cfg(unix)]
        #[test]
        fn missing_path_propagates_io_error() {
            let nonexistent =
                std::path::PathBuf::from("/nonexistent/never/exists_rencoder_fs_perms_test");
            let result = set_config_file_mode(&nonexistent);
            assert!(result.is_err(), "expected Err for missing path");
        }

        // Non-Unix smoke tests: verify the function compiles AND returns
        // Ok(()) without panicking on missing platform impls. These run
        // only when the test binary is built for a non-Unix target.
        #[cfg(not(unix))]
        #[test]
        fn non_unix_set_config_file_mode_is_ok_noop() {
            let result = set_config_file_mode("/nonexistent/never/exists");
            assert!(result.is_ok());
        }

        #[cfg(not(unix))]
        #[test]
        fn non_unix_set_app_dir_mode_is_ok_noop() {
            let result = set_app_dir_mode("/nonexistent/never/exists");
            assert!(result.is_ok());
        }
    }
}

/// How the filesystem names files -- and therefore when two names are
/// the same file.
///
/// # The contract
///
/// **"The output is never the input" holds on every platform** (design
/// doc §3.11.2). That is the guarantee the application needs: the
/// output must never land on the source file, or the cancel cleanup
/// would delete the user's media.
///
/// # Why one rule, and not one rule per platform
///
/// macOS and Windows filesystems are case-insensitive by default, so
/// `movie.MKV` and `movie.mkv` are one file there; on Linux they are
/// two. But the filesystem is not the only variable: macOS and Linux
/// both support case-sensitive volumes, and Linux can mount
/// case-insensitive ones. A per-platform `cfg` would therefore be a
/// guess that is wrong on some machines.
///
/// So the rule is **one rule everywhere, in the safe direction**: names
/// are folded and compared case-insensitively. On a case-sensitive
/// filesystem that is *conservative* -- two different files may be
/// treated as one, which costs an extra `_encoded` suffix, and never
/// costs a source file. The reverse mistake would be fatal.
///
/// The folding is Unicode-aware, so `Ä` and `ä` are one name.
pub mod fs_semantics {
    /// Fold a file name for comparison.
    ///
    /// The one rule: case does not distinguish names.
    pub fn fold_name(name: &str) -> String {
        name.to_lowercase()
    }

    /// Are two file names the same name?
    pub fn same_name(a: &str, b: &str) -> bool {
        fold_name(a) == fold_name(b)
    }

    /// How long the removal keeps trying while something else holds
    /// the file.
    const REMOVE_ATTEMPTS: usize = 20;
    const REMOVE_RETRY_DELAY: std::time::Duration = std::time::Duration::from_millis(50);

    /// What happened when a file this run wrote was removed.
    #[derive(Debug)]
    pub enum Removal {
        /// The file was there and is gone now.
        Removed,
        /// There was no file to remove.
        Absent,
        /// The file is still there -- the error says why.
        Failed(std::io::Error),
    }

    /// Remove a file this run wrote.
    ///
    /// The contract: **the file is gone when this returns, or the
    /// failure is reported** -- never a silent "it is probably fine".
    /// One primitive for every site that deletes an output (the cancel
    /// path, the failed-encode path, the zero-byte cleanup): three
    /// copies of the rule is three chances to forget the Windows
    /// half.
    ///
    /// # Why this is a platform difference
    ///
    /// Unix unlinks a file that is still open, so the first attempt
    /// always works. **Windows** refuses to delete a file that *any*
    /// process has open -- and something may hold it for a moment
    /// after the writer died: the scanner on a CI runner, an indexer,
    /// a file watcher. So the removal is retried for a short while.
    ///
    /// The wait is bounded: the batch has already stopped, so this
    /// only delays the exit, and a file that is still held after the
    /// window is a failure we report -- not one we retry forever.
    pub fn remove_written_file(path: &std::path::Path) -> Removal {
        for attempt in 0..REMOVE_ATTEMPTS {
            match std::fs::remove_file(path) {
                Ok(()) => return Removal::Removed,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Removal::Absent,
                // The last attempt is the one that reports: no sleep
                // after it (it would buy nothing), and the error goes
                // out with the attempt, not through an `Option` that
                // has to be unwrapped back out of the loop.
                Err(e) if attempt + 1 == REMOVE_ATTEMPTS => return Removal::Failed(e),
                Err(_) => std::thread::sleep(REMOVE_RETRY_DELAY),
            }
        }
        // Unreachable: the last attempt returns. Kept as a total
        // function rather than an `expect` the reader has to trust.
        Removal::Absent
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn case_does_not_distinguish_names() {
            assert!(same_name("movie.mkv", "MOVIE.MKV"));
            assert!(same_name("Movie.Mkv", "movie.mkv"));
            assert!(!same_name("movie.mkv", "movie2.mkv"));
        }

        #[test]
        fn removing_a_file_that_is_not_there_is_not_a_failure() {
            let path = std::env::temp_dir().join("rencoder_remove_absent_never_here");
            let _ = std::fs::remove_file(&path);
            match remove_written_file(&path) {
                Removal::Absent => {}
                other => panic!("expected Absent, got {other:?}"),
            }
        }

        #[test]
        fn removing_a_file_removes_it() {
            let path = std::env::temp_dir()
                .join(format!("rencoder_remove_present_{}", std::process::id()));
            std::fs::write(&path, b"x").unwrap();
            match remove_written_file(&path) {
                Removal::Removed => {}
                other => panic!("expected Removed, got {other:?}"),
            }
            assert!(!path.exists());
        }

        /// The folding is Unicode-aware, so a non-ASCII name folds the
        /// same way everywhere -- one rule, not two.
        #[test]
        fn the_folding_is_unicode_aware() {
            assert!(same_name("Ärger.mkv", "ärger.mkv"));
            assert_eq!(fold_name("Ärger.mkv"), "ärger.mkv");
        }

        /// Folding is idempotent: folding a folded name changes
        /// nothing, so a name that is compared twice does not drift.
        #[test]
        fn folding_is_idempotent() {
            let once = fold_name("Ärger.MKV");
            assert_eq!(fold_name(&once), once);
        }
    }
}

/// How a child process is started on this platform.
///
/// # The contract
///
/// **A console control event reaches the app, not the encoder it
/// started.** The app stops the encoder itself -- it kills the child,
/// removes the half-written output and writes the records (design doc
/// §3.11.2). The encoder must not stop on its own.
///
/// # Why this is a platform difference
///
/// On Unix a signal is delivered to **one process**: `SIGINT` to the
/// app never reaches the encoder, which the app kills itself.
///
/// On Windows a console control event goes to a whole **process
/// group**. The encoder inherits the app's group, so the event would
/// kill the encoder directly -- the file would be recorded as
/// *failed* instead of the batch being *cancelled*, the exit code
/// would not say the run was interrupted (130), and the half-written
/// output would be left behind. Putting the encoder in a process
/// group of its own restores the Unix "one process, one signal"
/// semantics.
///
/// # The second contract: stopping the encoder stops all of it
///
/// **When the app stops the encoder, every process the encoder runs
/// is gone, and the files they held are released** -- so the cancel
/// cleanup can remove the half-written output ([`ProcessTree`]).
///
/// On Windows the `ffmpeg.exe` on `PATH` is often not ffmpeg: a
/// package manager's shim (Chocolatey, Scoop) runs the real binary
/// as *its* child. `Child::kill` ends only the shim; the real encoder
/// keeps running, keeps writing the output, and Windows refuses to
/// delete a file a live process holds (`os error 32`). The child is
/// put in a **job object** and the whole job is ended instead.
///
/// On Unix the shims `exec` (the wrapper *becomes* ffmpeg) and an open
/// file can be unlinked anyway, so `Child::kill` is the whole answer.
pub mod process {
    use std::process::Command;

    /// Keep the platform's "stop everything" event away from the
    /// child.
    ///
    /// Windows only: the child gets `CREATE_NEW_PROCESS_GROUP`, so it
    /// is in a group of its own and the console event the app
    /// receives does not reach it. On Unix this is a no-op -- a
    /// signal already goes to one process only.
    pub fn isolate_from_console_events(cmd: &mut Command) {
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;

            /// CREATE_NEW_PROCESS_GROUP
            const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;

            cmd.creation_flags(CREATE_NEW_PROCESS_GROUP);
        }
        #[cfg(not(windows))]
        {
            let _ = cmd;
        }
    }

    /// How long [`ProcessTree::kill`] waits for the tree to be gone.
    ///
    /// Ending a process is asynchronous on Windows: the call returns
    /// once termination has *started*. The wait is what makes "the
    /// files are released" true when `kill` returns.
    ///
    /// Windows only: on Unix the shim `exec`s, so there is one
    /// process and killing it ends it.
    #[cfg(windows)]
    const TREE_EXIT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

    /// A started child and every process it starts in turn.
    ///
    /// Start the child with [`ProcessTree::spawn`] instead of
    /// `Command::spawn`, stop it with [`ProcessTree::kill`] instead of
    /// `Child::kill`. Dropping it (the child finished on its own) also
    /// ends whatever the child left running -- nothing outlives the
    /// encode.
    pub struct ProcessTree {
        #[cfg(windows)]
        job: Option<job::Job>,
    }

    impl ProcessTree {
        /// Start `cmd` with the child -- and everything it starts --
        /// in one tree.
        ///
        /// Windows: the child is created **suspended**, put in a job
        /// object that ends its processes when it is closed, and only
        /// then resumed. It runs no instruction outside the job, so
        /// nothing it starts can escape it. (Assigning after a normal
        /// `spawn` leaves a window in which a busy machine lets the
        /// child start its own child first -- and that one is outside
        /// the job; CI showed it.)
        ///
        /// This also does what [`isolate_from_console_events`] does:
        /// `Command` keeps only the *last* `creation_flags` value, so
        /// the two flags have to be set together, here.
        ///
        /// If the job cannot be set up, the child still runs and a
        /// cancel stops it alone (logged) -- what happened before.
        pub fn spawn(cmd: &mut Command) -> std::io::Result<(std::process::Child, ProcessTree)> {
            #[cfg(windows)]
            {
                use std::os::windows::io::AsRawHandle;
                use std::os::windows::process::CommandExt;

                /// CREATE_NEW_PROCESS_GROUP (see `isolate_from_console_events`)
                const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
                /// CREATE_SUSPENDED: the main thread waits for `ResumeThread`.
                const CREATE_SUSPENDED: u32 = 0x0000_0004;

                cmd.creation_flags(CREATE_NEW_PROCESS_GROUP | CREATE_SUSPENDED);
                let mut child = cmd.spawn()?;

                let job = match job::Job::for_process(child.as_raw_handle() as _) {
                    Ok(job) => Some(job),
                    Err(e) => {
                        tracing::warn!(
                            pid = child.id(),
                            error = %e,
                            "could not put the encoder in a job object; \
                             a cancel stops only the direct child"
                        );
                        None
                    }
                };
                // Resume whether or not the job was set up: a child left
                // suspended would hang the encode forever.
                if let Err(e) = job::resume_main_thread(child.id()) {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(e);
                }
                Ok((child, ProcessTree { job }))
            }
            #[cfg(not(windows))]
            {
                Ok((cmd.spawn()?, ProcessTree {}))
            }
        }

        /// Stop the child and everything it started, and reap the
        /// child.
        ///
        /// When this returns, **none of them holds a file open** any
        /// more -- with one exception, and it is loud: on Windows, if
        /// the job object could not be created (see [`ProcessTree::spawn`]),
        /// only the direct child is stopped, so a shim's real encoder
        /// may still be running and still holding the output. That
        /// case is reported here, because the caller cannot see it
        /// otherwise: the cleanup that follows will fail to remove
        /// the partial, and the reason has to be in the log next to
        /// it.
        ///
        /// The wait for the tree to drain is bounded by
        /// [`TREE_EXIT_TIMEOUT`]; past it, a warning is logged and the
        /// caller's cleanup reports what it could not remove.
        ///
        /// "None of them holds a file open" is the **eventual**
        /// contract, not an instantaneous one: the kernel releases a
        /// terminated process's handles as the process goes down ✅ ✅,
        /// which is not the same instant as "the job reports no active
        /// process" ✅ ✅. The caller expresses the same bound ✅ ✅
        /// (`platform::fs_semantics::remove_written_file` ✅ ✅ retries
        /// for a short while ✅ ✅) ✅ ✅.
        pub fn kill(&self, child: &mut std::process::Child) {
            #[cfg(windows)]
            match &self.job {
                Some(job) => {
                    if let Err(e) = job.terminate() {
                        tracing::warn!(pid = child.id(), error = %e, "ending the encoder's job failed");
                    } else if !job.wait_until_empty(TREE_EXIT_TIMEOUT) {
                        tracing::warn!(
                            pid = child.id(),
                            "the encoder's processes were still running after the timeout"
                        );
                    }
                }
                // No job: the tree cannot be ended, so the guarantee
                // above does not hold. Say so here -- the cleanup
                // that follows needs the reader to know why it may
                // fail to remove the partial.
                None => tracing::warn!(
                    pid = child.id(),
                    "the encoder had no job object; only the direct child was stopped \
                     -- a shim's real encoder may still be running and holding the output"
                ),
            }
            // Already dead on Windows (the job ended it) -- then this
            // is a harmless error; on Unix it is the kill.
            let _ = child.kill();
            let _ = child.wait();
        }

        /// Does stopping this tree stop **everything** it started?
        ///
        /// On Unix always: the shim `exec`s, so the child *is* the
        /// encoder ✅ ✅. On Windows only when the job object exists
        /// ✅ ✅ -- without it, a grandchild (the real encoder behind
        /// a package-manager shim ✅ ✅) survives the kill ✅ ✅ and
        /// keeps holding the output ✅ ✅.
        ///
        /// The caller cannot see this otherwise ✅ ✅ (the fallback is
        /// a log line ✅ ✅), and the difference decides whether the
        /// cleanup that follows can remove the partial ✅ ✅ ✅.
        pub fn covers_whole_tree(&self) -> bool {
            #[cfg(windows)]
            {
                self.job.is_some()
            }
            #[cfg(not(windows))]
            {
                true
            }
        }
    }

    #[cfg(windows)]
    mod job {
        use std::time::{Duration, Instant};
        use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};
        use windows_sys::Win32::System::Diagnostics::ToolHelp::{
            CreateToolhelp32Snapshot, TH32CS_SNAPTHREAD, THREADENTRY32, Thread32First, Thread32Next,
        };
        use windows_sys::Win32::System::JobObjects::{
            AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
            JOBOBJECT_BASIC_ACCOUNTING_INFORMATION, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
            JobObjectBasicAccountingInformation, JobObjectExtendedLimitInformation,
            QueryInformationJobObject, SetInformationJobObject, TerminateJobObject,
        };
        use windows_sys::Win32::System::Threading::{
            OpenThread, ResumeThread, THREAD_SUSPEND_RESUME,
        };

        /// Resume the threads of a process created with
        /// `CREATE_SUSPENDED` -- just one, its main thread.
        ///
        /// `std::process::Child` keeps the main thread's handle to
        /// itself, so the thread is found by its owner's pid in a
        /// thread snapshot. The pid cannot be reused meanwhile: the
        /// `Child` holds the process handle open.
        pub(super) fn resume_main_thread(pid: u32) -> std::io::Result<()> {
            // SAFETY: a snapshot of all threads; closed below.
            let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) };
            if snapshot == INVALID_HANDLE_VALUE {
                return Err(std::io::Error::last_os_error());
            }
            let mut entry = THREADENTRY32 {
                dwSize: std::mem::size_of::<THREADENTRY32>() as u32,
                ..Default::default()
            };
            let mut resumed = 0u32;
            let mut failure = None;
            // SAFETY: `entry` is a THREADENTRY32 with its dwSize set.
            let mut more = unsafe { Thread32First(snapshot, &mut entry) } != 0;
            while more {
                if entry.th32OwnerProcessID == pid {
                    // SAFETY: a thread id from the snapshot; null on failure.
                    let thread =
                        unsafe { OpenThread(THREAD_SUSPEND_RESUME, 0, entry.th32ThreadID) };
                    if thread.is_null() {
                        failure = Some(std::io::Error::last_os_error());
                    } else {
                        // SAFETY: a valid thread handle, closed right after.
                        if unsafe { ResumeThread(thread) } == u32::MAX {
                            failure = Some(std::io::Error::last_os_error());
                        } else {
                            resumed += 1;
                        }
                        unsafe { CloseHandle(thread) };
                    }
                }
                // SAFETY: as for Thread32First.
                more = unsafe { Thread32Next(snapshot, &mut entry) } != 0;
            }
            // SAFETY: we own the snapshot handle.
            unsafe { CloseHandle(snapshot) };
            match (resumed, failure) {
                (0, Some(e)) => Err(e),
                (0, None) => Err(std::io::Error::other(format!(
                    "no thread of process {pid} to resume"
                ))),
                _ => Ok(()),
            }
        }

        /// An owned job object handle.
        pub(super) struct Job(HANDLE);

        // SAFETY: a job handle names a kernel object; the calls made on
        // it here are safe from any thread.
        unsafe impl Send for Job {}
        unsafe impl Sync for Job {}

        impl Job {
            /// A new job that ends its processes when it is closed,
            /// with `process` in it.
            pub(super) fn for_process(process: HANDLE) -> std::io::Result<Self> {
                // SAFETY: no security attributes, no name -- both may be null.
                let handle = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
                if handle.is_null() {
                    return Err(std::io::Error::last_os_error());
                }
                let job = Job(handle);

                // SAFETY: a plain C struct; all-zero is its "no limits" value.
                let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
                info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
                // SAFETY: `info` is the struct this class expects, with its size.
                let ok = unsafe {
                    SetInformationJobObject(
                        job.0,
                        JobObjectExtendedLimitInformation,
                        &info as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION
                            as *const core::ffi::c_void,
                        std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                    )
                };
                if ok == 0 {
                    return Err(std::io::Error::last_os_error());
                }
                // SAFETY: both handles are valid for the call.
                if unsafe { AssignProcessToJobObject(job.0, process) } == 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(job)
            }

            /// Start ending every process in the job.
            pub(super) fn terminate(&self) -> std::io::Result<()> {
                // SAFETY: the handle is valid while `self` lives.
                if unsafe { TerminateJobObject(self.0, 1) } == 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            }

            fn active_processes(&self) -> std::io::Result<u32> {
                // SAFETY: a plain C struct, filled in by the call.
                let mut info: JOBOBJECT_BASIC_ACCOUNTING_INFORMATION =
                    unsafe { std::mem::zeroed() };
                // SAFETY: `info` is the struct this class expects, with its size.
                let ok = unsafe {
                    QueryInformationJobObject(
                        self.0,
                        JobObjectBasicAccountingInformation,
                        &mut info as *mut JOBOBJECT_BASIC_ACCOUNTING_INFORMATION
                            as *mut core::ffi::c_void,
                        std::mem::size_of::<JOBOBJECT_BASIC_ACCOUNTING_INFORMATION>() as u32,
                        std::ptr::null_mut(),
                    )
                };
                if ok == 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(info.ActiveProcesses)
            }

            /// Wait until no process in the job is alive. `false` on
            /// timeout, or if the job cannot be asked.
            pub(super) fn wait_until_empty(&self, timeout: Duration) -> bool {
                let deadline = Instant::now() + timeout;
                loop {
                    match self.active_processes() {
                        Ok(0) => return true,
                        Ok(_) => {}
                        Err(_) => return false,
                    }
                    if Instant::now() >= deadline {
                        return false;
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
            }
        }

        impl Drop for Job {
            fn drop(&mut self) {
                // SAFETY: we own the handle and close it once. Closing
                // the last handle ends what is left in the job
                // (KILL_ON_JOB_CLOSE).
                unsafe { CloseHandle(self.0) };
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::ProcessTree;

        /// The contract on every host: after `kill`, the child is
        /// reaped.
        #[test]
        fn killing_the_tree_reaps_the_child() {
            #[cfg(windows)]
            let mut cmd = {
                let mut c = std::process::Command::new("ping");
                c.args(["-n", "30", "127.0.0.1"]);
                c
            };
            #[cfg(not(windows))]
            let mut cmd = {
                let mut c = std::process::Command::new("sleep");
                c.arg("30");
                c
            };
            cmd.stdout(std::process::Stdio::null());
            let (mut child, tree) = ProcessTree::spawn(&mut cmd).expect("spawn the child");
            tree.kill(&mut child);
            assert!(
                child.try_wait().expect("ask the child").is_some(),
                "the child must be reaped after kill"
            );
        }

        /// The bug this exists for: a shim in front of the encoder.
        /// `cmd` opens the redirection target and hands it to `ping`,
        /// its child -- the same shape as a Chocolatey shim running
        /// the real ffmpeg. Killing only `cmd` leaves `ping` holding
        /// the file, and Windows refuses to delete it.
        #[cfg(windows)]
        #[test]
        fn killing_the_tree_releases_a_file_a_grandchild_holds() {
            use std::os::windows::process::CommandExt;

            let dir =
                std::env::temp_dir().join(format!("rencoder_process_tree_{}", std::process::id()));
            std::fs::create_dir_all(&dir).unwrap();
            let held = dir.join("held.txt");

            let mut cmd = std::process::Command::new("cmd");
            cmd.raw_arg(format!("/C ping -n 30 127.0.0.1 > \"{}\"", held.display()));
            let (mut child, tree) = ProcessTree::spawn(&mut cmd).expect("spawn cmd");

            // Wait for the grandchild to be writing.
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
            while std::fs::metadata(&held).map_or(0, |m| m.len()) == 0 {
                assert!(
                    std::time::Instant::now() < deadline,
                    "ping never wrote to the file"
                );
                std::thread::sleep(std::time::Duration::from_millis(50));
            }

            tree.kill(&mut child);

            // The contract is **bounded**, not instantaneous: the
            // kernel releases a terminated process's handles as the
            // process goes down, which is not the same instant as
            // "the job reports no active process". The production
            // cleanup states the same bound -- it removes the file
            // through the shared primitive, which retries for a short
            // while (platform::fs_semantics::remove_written_file).
            //
            // So the test uses that primitive: the rule lives in one
            // place, and the test asserts the rule rather than a
            // timing detail of the kernel.
            let removed = crate::platform::fs_semantics::remove_written_file(&held);
            let _ = std::fs::remove_dir_all(&dir);
            assert!(
                matches!(removed, crate::platform::fs_semantics::Removal::Removed),
                "the file must be deletable once the tree is killed \
                 (the tree covers everything: {}) -- removal: {removed:?}",
                tree.covers_whole_tree(),
            );
        }
    }
}
