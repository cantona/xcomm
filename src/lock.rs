//! UUCP lock files (`LCK..ttyUSB0`), the convention minicom and picocom use
//! to keep two programs off one serial port.
//!
//! The lock file holds the owner's pid in HDB format (ten columns, then a
//! newline). Taking it is atomic: the content is written to a private file
//! that is then hard-linked to the lock name. While a session runs it also
//! holds an flock on the lock file, so two xcomm processes clearing the same
//! stale lock cannot delete each other's fresh one: a lock is removed as
//! stale only if its pid is dead, nobody holds its flock, and the name still
//! refers to the file that was checked.

use std::fmt;
use std::fs::File;
use std::io::{self, Read};
use std::os::fd::AsFd;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use rustix::fs::{FlockOperation, flock};
use rustix::process::{Pid, test_kill_process};

#[derive(Debug)]
pub enum LockError {
    /// A live process holds the lock.
    Held {
        path: PathBuf,
        pid: i32,
    },
    /// The lock is changing hands, or held by a process not visible here.
    Contended {
        path: PathBuf,
        why: String,
    },
    NoLockDir {
        tried: Vec<PathBuf>,
    },
    Io {
        path: PathBuf,
        source: io::Error,
    },
}

impl fmt::Display for LockError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LockError::Held { path, pid } => {
                write!(f, "{} is held by process {pid}", path.display())
            }
            LockError::Contended { path, why } => write!(f, "{}: {why}", path.display()),
            LockError::NoLockDir { tried } => {
                let list: Vec<String> =
                    tried.iter().map(|p| p.display().to_string()).collect();
                write!(
                    f,
                    "no writable lock directory (tried {}); use --no-lock to run without one",
                    list.join(", ")
                )
            }
            LockError::Io { path, source } => write!(f, "{}: {source}", path.display()),
        }
    }
}

impl std::error::Error for LockError {}

/// lockdev (Fedora and relatives) keeps the shared locks one level down;
/// tried first so xcomm and minicom lock the same files there. Not verified
/// on such a system.
#[cfg(not(target_os = "macos"))]
const LOCK_DIRS: [&str; 5] = [
    "/run/lock/lockdev",
    "/var/lock/lockdev",
    "/run/lock",
    "/var/lock",
    "/var/spool/lock",
];
/// Stock macOS has no user-writable UUCP lock directory, so locks fall back
/// to /tmp there: they keep xcomm sessions apart, not other programs.
/// Not verified on a Mac.
#[cfg(target_os = "macos")]
const LOCK_DIRS: [&str; 3] = ["/var/spool/lock", "/var/lock", "/tmp"];

/// `XCOMM_LOCK_DIR` (test-only; used as given, no fallback, so a test
/// never touches the real lock directory) or the first writable standard
/// directory.
pub fn default_lock_dir(
    env: &dyn Fn(&str) -> Option<String>,
) -> Result<PathBuf, LockError> {
    if let Some(dir) = env("XCOMM_LOCK_DIR").filter(|d| !d.is_empty()) {
        let dir = PathBuf::from(dir);
        return match writable(&dir) {
            true => Ok(dir),
            false => Err(LockError::NoLockDir { tried: vec![dir] }),
        };
    }
    LOCK_DIRS
        .iter()
        .map(PathBuf::from)
        .find(|d| writable(d))
        .ok_or_else(|| LockError::NoLockDir {
            tried: LOCK_DIRS.iter().map(PathBuf::from).collect(),
        })
}

/// Where a lock may be, to read it: `XCOMM_LOCK_DIR` (test-only) or every
/// standard directory there is. Reading needs no write access, and the
/// holder may have used a directory this user cannot write.
pub fn lock_dirs_to_read(env: &dyn Fn(&str) -> Option<String>) -> Vec<PathBuf> {
    if let Some(dir) = env("XCOMM_LOCK_DIR").filter(|d| !d.is_empty()) {
        return vec![PathBuf::from(dir)];
    }
    LOCK_DIRS
        .iter()
        .map(PathBuf::from)
        .filter(|d| d.is_dir())
        .collect()
}

fn writable(dir: &Path) -> bool {
    dir.is_dir() && rustix::fs::access(dir, rustix::fs::Access::WRITE_OK).is_ok()
}

/// A held lock; dropping it removes the lock file if it is still ours.
#[derive(Debug)]
pub struct UucpLock {
    path: PathBuf,
    /// The lock file, kept open with an flock for the life of the lock.
    file: File,
}

impl UucpLock {
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Removes the lock file now and reports a failure, which Drop cannot.
    /// A lock file left behind would make the next run report a stale lock,
    /// and in a sticky directory other users could not clear it.
    pub fn release(self) -> Result<(), LockError> {
        let result = remove_if_ours(&self.path, &self.file);
        // Done here; Drop must not try again.
        let mut this = std::mem::ManuallyDrop::new(self);
        // SAFETY: `this` is not used again and its Drop never runs, so each
        // field is dropped exactly once here.
        unsafe {
            std::ptr::drop_in_place(&mut this.path);
            std::ptr::drop_in_place(&mut this.file);
        }
        result
    }
}

/// Removes `path` if it still names `file` (same dev and inode).
fn remove_if_ours(path: &Path, file: &File) -> Result<(), LockError> {
    let err = |what: &str, e: io::Error| LockError::Io {
        path: path.to_path_buf(),
        source: io::Error::new(e.kind(), format!("{what}: {e}")),
    };
    let named = match std::fs::metadata(path) {
        Ok(m) => m,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(err("stat", e)),
    };
    let ours = file.metadata().map_err(|e| err("fstat", e))?;
    if ours.ino() != named.ino() || ours.dev() != named.dev() {
        return Ok(());
    }
    std::fs::remove_file(path).map_err(|e| err("remove", e))
}

impl Drop for UucpLock {
    fn drop(&mut self) {
        // Someone may have removed the file by hand and another program
        // taken the port since: only our own file is removed. This narrows
        // that case; a lock created between the check and the removal can
        // still be removed. Errors cannot be reported here; use release().
        let _ = remove_if_ours(&self.path, &self.file);
    }
}

fn lock_path(dir: &Path, tty_name: &str) -> Result<PathBuf, LockError> {
    if tty_name.is_empty()
        || tty_name == "."
        || tty_name == ".."
        || tty_name.contains('/')
    {
        return Err(LockError::Io {
            path: dir.to_path_buf(),
            source: io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("invalid tty name '{tty_name}'"),
            ),
        });
    }
    Ok(dir.join(format!("LCK..{tty_name}")))
}

fn alive(pid: i32) -> bool {
    match Pid::from_raw(pid) {
        // EPERM means it exists but belongs to someone else.
        Some(p) => !matches!(test_kill_process(p), Err(rustix::io::Errno::SRCH)),
        None => false,
    }
}

/// The lock name for a device, as minicom forms it: the path below `/dev/`
/// with `/` turned into `_` (`/dev/ttyUSB0` -> `ttyUSB0`, `/dev/pts/5` ->
/// `pts_5`), so both programs lock the same file. Outside `/dev/`, the file
/// name. `device` should be the resolved path, not a by-id link.
pub fn lock_name(device: &Path) -> Option<String> {
    let s = device.to_str()?;
    let name = match s.strip_prefix("/dev/") {
        Some(rest) => rest.replace('/', "_"),
        None => device.file_name()?.to_str()?.to_string(),
    };
    (!name.is_empty()).then_some(name)
}

/// What a lock file says.
enum Content {
    Pid(i32),
    /// Empty: another program has created it and not written it yet.
    Empty,
}

/// Lock files are not read past this; a pid line is a dozen bytes.
const MAX_LOCK_SIZE: u64 = 128;

/// Opens an existing lock file without blocking (a FIFO planted in the
/// world-writable lock directory would otherwise hang the open) and without
/// following a symlink, and checks it is a small regular file.
fn open_existing(path: &Path) -> io::Result<File> {
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK | libc::O_NOFOLLOW)
        .open(path)?;
    let meta = file.metadata()?;
    if !meta.file_type().is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "not a regular file; remove it by hand",
        ));
    }
    if meta.len() > MAX_LOCK_SIZE {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "{} bytes, too large for a lock file; remove it by hand",
                meta.len()
            ),
        ));
    }
    Ok(file)
}

/// The pid in a lock file: HDB (padded ASCII), plain ASCII digits, or
/// digits followed by other words, as some programs append their name.
fn read_content(file: &mut File, path: &Path) -> Result<Content, LockError> {
    let mut bytes = Vec::new();
    file.take(MAX_LOCK_SIZE)
        .read_to_end(&mut bytes)
        .map_err(|e| LockError::Io {
            path: path.to_path_buf(),
            source: io::Error::new(e.kind(), format!("read: {e}")),
        })?;
    if bytes.is_empty() {
        return Ok(Content::Empty);
    }
    let text = String::from_utf8_lossy(&bytes);
    text.split_whitespace()
        .next()
        .and_then(|t| t.parse::<i32>().ok())
        .filter(|p| *p > 0)
        .map(Content::Pid)
        .ok_or_else(|| LockError::Io {
            path: path.to_path_buf(),
            source: io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "not a lock file (content {text:?}); remove it by hand if no program uses the port"
                ),
            ),
        })
}

/// What `acquire` cleared before taking the lock.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cleared {
    /// A lock naming a process that no longer exists.
    DeadPid(i32),
    /// An empty lock older than EMPTY_GRACE: its writer died before writing
    /// its pid.
    Empty,
}

/// An empty lock file younger than this is still being written.
const EMPTY_GRACE: std::time::Duration = std::time::Duration::from_secs(5);

/// More than EMPTY_GRACE old, or dated in the future (the clock was stepped
/// back since it was written), which no file being written now can be.
fn older_than_grace(file: &File) -> bool {
    match file.metadata().and_then(|m| m.modified()) {
        Ok(t) => match t.elapsed() {
            Ok(age) => age > EMPTY_GRACE,
            Err(ahead) => ahead.duration() > EMPTY_GRACE,
        },
        Err(_) => false,
    }
}

// mode_t is u16 on macOS, u32 on Linux.
#[allow(clippy::unnecessary_cast)]
const STICKY: u32 = libc::S_ISVTX as u32;

/// A stale lock owned by someone else in a sticky directory: only that
/// user (or root) may remove it, so the port cannot be taken.
fn stuck(path: &Path, file: &File) -> bool {
    let dir_sticky = path
        .parent()
        .and_then(|d| std::fs::metadata(d).ok())
        .is_some_and(|m| m.mode() & STICKY != 0);
    let euid = rustix::process::geteuid();
    let dir_owner = path
        .parent()
        .and_then(|d| std::fs::metadata(d).ok())
        .map(|m| m.uid());
    // Root, the directory's owner and the file's owner may remove it.
    // (A non-root process with CAP_FOWNER also may; that is not checked.)
    dir_sticky
        && !euid.is_root()
        && dir_owner != Some(euid.as_raw())
        && file.metadata().is_ok_and(|m| m.uid() != euid.as_raw())
}

fn flock_held_elsewhere(file: &File) -> Result<bool, rustix::io::Errno> {
    match flock(file.as_fd(), FlockOperation::NonBlockingLockShared) {
        Ok(()) => {
            let _ = flock(file.as_fd(), FlockOperation::Unlock);
            Ok(false)
        }
        Err(rustix::io::Errno::WOULDBLOCK) => Ok(true),
        Err(e) => Err(e),
    }
}

fn flock_held_why(owner: Option<i32>) -> String {
    let who = match owner {
        Some(pid) => format!("process {pid} is not visible here, but "),
        None => String::new(),
    };
    format!(
        "{who}its flock is held: another xcomm is taking it over or checking it, or holds it from another pid namespace"
    )
}

/// The live process holding the lock for `tty_name`. `Ok(None)` means no
/// live owner: no lock file, a dead pid, or an abandoned empty file (a stale
/// file may still need removing, which `acquire` does if it may). A file
/// that cannot be read or understood, or whose flock is held by a process
/// not visible here, is an error, as it is for `acquire`.
pub fn holder(dir: &Path, tty_name: &str) -> Result<Option<i32>, LockError> {
    let path = lock_path(dir, tty_name)?;
    let mut file = match open_existing(&path) {
        Ok(f) => f,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => {
            return Err(LockError::Io {
                path,
                source: io::Error::new(e.kind(), format!("open: {e}")),
            });
        }
    };
    let owner = match read_content(&mut file, &path)? {
        Content::Pid(pid) if alive(pid) => return Ok(Some(pid)),
        Content::Pid(pid) => Some(pid),
        Content::Empty if !older_than_grace(&file) => {
            return Err(LockError::Contended {
                path,
                why: "the lock file is still being written".into(),
            });
        }
        Content::Empty => None,
    };
    // The flock first, as acquire does: a held flock means a live owner
    // (perhaps in another pid namespace), and calling that lock stale would
    // invite deleting it.
    match flock_held_elsewhere(&file) {
        Ok(false) => {}
        Ok(true) => {
            return Err(LockError::Contended {
                path,
                why: flock_held_why(owner),
            });
        }
        Err(e) => {
            let e = io::Error::from(e);
            return Err(LockError::Io {
                path,
                source: io::Error::new(e.kind(), format!("flock: {e}")),
            });
        }
    }
    if stuck(&path, &file) {
        return Err(LockError::Io {
            path,
            source: io::Error::new(
                io::ErrorKind::PermissionDenied,
                "stale lock owned by another user in a sticky directory; only that user or root can remove it",
            ),
        });
    }
    Ok(None)
}

/// Retries after finding the lock changing hands, 20 ms apart. Generous,
/// because holder()'s brief shared-flock probe also looks like this.
const CONTENTION_RETRIES: usize = 10;
/// Bounds the loop even if stale locks keep reappearing.
const MAX_ROUNDS: usize = 20;

/// Creates `LCK..<name>.<pid>.<n>.tmp` with a name no one else uses: `n`
/// counts up per process, and a name that already exists (left by a killed
/// process that had the same pid) is skipped.
fn create_private(dir: &Path, tty_name: &str, pid: u32) -> io::Result<(PathBuf, File)> {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    for _ in 0..100 {
        let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let tmp = dir.join(format!("LCK..{tty_name}.{pid}.{n}.tmp"));
        match std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .mode(0o644)
            .open(&tmp)
        {
            Ok(file) => return Ok((tmp, file)),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => {
                return Err(io::Error::new(
                    e.kind(),
                    format!("create {}: {e}", tmp.display()),
                ));
            }
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        format!("no free temporary lock name in {}", dir.display()),
    ))
}

/// Takes the lock for `tty_name` in `dir`. Returns the lock and what was
/// cleared first, if anything.
///
/// A program that clears stale locks without flock (minicom, picocom) is
/// not excluded: if it decided a lock was stale before this takes over, its
/// later unlink by name can remove the fresh lock. TIOCEXCL on the port is
/// the second line of defence. Lock directories on NFS (where flock is
/// emulated with fcntl locks) are not supported.
pub fn acquire(
    dir: &Path,
    tty_name: &str,
) -> Result<(UucpLock, Option<Cleared>), LockError> {
    let path = lock_path(dir, tty_name)?;
    let io_err = |p: &Path| {
        let p = p.to_path_buf();
        move |source: io::Error| LockError::Io { path: p, source }
    };
    let pid = std::process::id();
    let mut cleared = None;
    let mut contended = 0;
    let mut why = String::new();
    for _ in 0..MAX_ROUNDS {
        let (tmp, mut file) =
            create_private(dir, tty_name, pid).map_err(|source| LockError::Io {
                path: path.clone(),
                source,
            })?;
        // The umask may have cut the mode down; other users must be able
        // to read the pid.
        let step = |what: &'static str| {
            move |e: io::Error| io::Error::new(e.kind(), format!("{what}: {e}"))
        };
        let written = rustix::fs::fchmod(&file, rustix::fs::Mode::from_raw_mode(0o644))
            .map_err(|e| step("fchmod")(e.into()))
            .and_then(|()| {
                io::Write::write_all(&mut file, format!("{pid:>10}\n").as_bytes())
                    .map_err(step("write"))
            })
            .and_then(|()| {
                flock(file.as_fd(), FlockOperation::NonBlockingLockExclusive)
                    .map_err(|e| step("flock")(e.into()))
            })
            .map_err(io_err(&tmp));
        let linked = written.and_then(|()| {
            std::fs::hard_link(&tmp, &path)
                .map_err(|e| {
                    if e.kind() == io::ErrorKind::AlreadyExists {
                        e
                    } else {
                        step("link")(e)
                    }
                })
                .map_err(io_err(&path))
        });
        let _ = std::fs::remove_file(&tmp);
        match linked {
            Ok(()) => return Ok((UucpLock { path, file }, cleared)),
            Err(LockError::Io { source, .. })
                if source.kind() == io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e),
        }

        let mut existing = match open_existing(&path) {
            Ok(f) => f,
            // Removed since the link failed: try again.
            Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
            Err(e) => {
                return Err(io_err(&path)(io::Error::new(
                    e.kind(),
                    format!("open: {e}"),
                )));
            }
        };
        let stale = match read_content(&mut existing, &path)? {
            Content::Pid(owner) if owner == pid as i32 => {
                // Either this process already holds it, or an earlier
                // process with the same pid left it (a restarted container).
                // Deleting it could also remove a live lock of another pid
                // namespace sharing this directory, so it is not cleared.
                return Err(LockError::Io {
                    path,
                    source: io::Error::new(
                        io::ErrorKind::AlreadyExists,
                        format!(
                            "the lock names this process's own pid {owner}: it is already held here, or was left by an earlier process with the same pid; remove it by hand if no program uses the port"
                        ),
                    ),
                });
            }
            Content::Pid(owner) if alive(owner) => {
                return Err(LockError::Held { path, pid: owner });
            }
            Content::Pid(owner) => Cleared::DeadPid(owner),
            Content::Empty if older_than_grace(&existing) => Cleared::Empty,
            Content::Empty => {
                contended += 1;
                why = "the lock file is still being written".into();
                if contended >= CONTENTION_RETRIES {
                    return Err(LockError::Contended { path, why });
                }
                std::thread::sleep(std::time::Duration::from_millis(20));
                continue;
            }
        };
        // No live owner. Another xcomm may be taking it over at this
        // moment: it holds the flock while it does.
        match flock(existing.as_fd(), FlockOperation::NonBlockingLockExclusive) {
            Ok(()) => {}
            Err(rustix::io::Errno::WOULDBLOCK) => {
                contended += 1;
                why = flock_held_why(match stale {
                    Cleared::DeadPid(p) => Some(p),
                    Cleared::Empty => None,
                });
                if contended >= CONTENTION_RETRIES {
                    return Err(LockError::Contended { path, why });
                }
                std::thread::sleep(std::time::Duration::from_millis(20));
                continue;
            }
            Err(e) => {
                let e = io::Error::from(e);
                return Err(io_err(&path)(io::Error::new(
                    e.kind(),
                    format!("flock: {e}"),
                )));
            }
        }
        let same = match (existing.metadata(), std::fs::metadata(&path)) {
            (Ok(a), Ok(b)) => a.ino() == b.ino() && a.dev() == b.dev(),
            _ => false,
        };
        if same {
            if let Err(e) = std::fs::remove_file(&path) {
                let what = match stale {
                    Cleared::DeadPid(p) => format!("stale lock of dead process {p}"),
                    Cleared::Empty => "abandoned empty lock".to_string(),
                };
                let hint = if e.kind() == io::ErrorKind::PermissionDenied {
                    " (the lock directory is sticky: only the file's owner or root can remove it)"
                } else {
                    ""
                };
                return Err(LockError::Io {
                    path,
                    source: io::Error::new(
                        e.kind(),
                        format!("cannot remove {what}: {e}{hint}"),
                    ),
                });
            }
            cleared = Some(stale);
        }
    }
    if why.is_empty() {
        why = "the lock kept changing hands".into();
    }
    Err(LockError::Contended { path, why })
}
