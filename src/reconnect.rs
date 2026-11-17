//! Waiting for a device that went away to come back.

#[cfg(target_os = "linux")]
use std::ffi::OsString;
use std::io;
use std::os::fd::BorrowedFd;
#[cfg(target_os = "linux")]
use std::os::fd::{AsFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const BY_PATH: &str = "/dev/serial/by-path";
/// How often the device is looked for where no event says it is back
/// (macOS, or a watched directory that is missing).
const POLL: Duration = Duration::from_secs(1);
/// udev gives a new node its owner and mode a moment after the kernel makes
/// it; an open in between fails with EACCES.
const RETRY: Duration = Duration::from_millis(100);
const RETRY_FOR: Duration = Duration::from_secs(5);

/// The name to reopen `path` by when the device comes back. A name the
/// user chose (any symbolic link: by-id, by-path, a udev rule's) stays.
/// For a kernel name (`/dev/ttyUSB1`), the `/dev/serial/by-path` link that
/// resolves to it is taken: the USB port it is plugged into. Not by-id:
/// identical adapters, and clones that share a serial number, share one
/// by-id name, which udev points at whichever came last, so it could name
/// a different adapter (another board's console). Else `path` itself.
pub fn stable_path(path: &Path) -> PathBuf {
    stable_path_in(Path::new(BY_PATH), path)
}

/// [`stable_path`] with the by-path directory given (tests).
pub fn stable_path_in(by_path: &Path, path: &Path) -> PathBuf {
    let chosen =
        std::fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_symlink());
    let real = match std::fs::canonicalize(path) {
        Ok(real) if !chosen => real,
        _ => return path.to_path_buf(),
    };
    let Ok(entries) = std::fs::read_dir(by_path) else {
        return path.to_path_buf();
    };
    let mut links: Vec<PathBuf> = entries
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| std::fs::canonicalize(p).is_ok_and(|r| r == real))
        .collect();
    links.sort();
    links
        .into_iter()
        .next()
        .unwrap_or_else(|| path.to_path_buf())
}

/// Which device node a path named, as far as reopening cares: a node made
/// again (a replug) has another inode, and udev setting its owner or mode
/// changes its ctime.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NodeId {
    dev: u64,
    ino: u64,
    ctime: (i64, i64),
}

impl NodeId {
    /// The node `fd` was opened from.
    pub fn of(fd: BorrowedFd<'_>) -> io::Result<NodeId> {
        Ok(NodeId::from(rustix::fs::fstat(fd)?))
    }

    /// The node `path` names now, following links.
    pub fn at(path: &Path) -> io::Result<NodeId> {
        Ok(NodeId::from(rustix::fs::stat(path)?))
    }

    /// Whether both name the same node, whatever changed on it since.
    pub fn same_node(&self, other: &NodeId) -> bool {
        self.dev == other.dev && self.ino == other.ino
    }

    /// The entry `path` itself, not following a link: a link put in its
    /// place is another one.
    pub fn link_at(path: &Path) -> io::Result<NodeId> {
        Ok(NodeId::from(rustix::fs::lstat(path)?))
    }
}

/// What tells that the device is back.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Track {
    /// Another node at the path (a replug).
    Node,
    /// Another link at the path, naming a node that exists. For a
    /// pseudo-terminal reached through a link (pty numbers are reused at
    /// once, so a node coming back under the old name is someone else's
    /// terminal), and for a udev link (see [`udev_link`]).
    Link,
}

/// Whether `path` is a link udev keeps under `/dev/serial`. udev removes it
/// with the device and makes it again when the device comes back; until
/// it is removed it may lead to another device that took the old tty name.
pub fn udev_link(path: &Path) -> bool {
    path.starts_with("/dev/serial/")
}

/// Whether `real` (a resolved path) is a pseudo-terminal's slave. Its other
/// end closing is final: no device comes back under its name.
pub fn is_pty(real: &Path) -> bool {
    if cfg!(target_os = "linux") {
        return real.parent() == Some(Path::new("/dev/pts"));
    }
    real.parent() == Some(Path::new("/dev"))
        && real
            .file_name()
            .and_then(|n| n.to_str())
            .and_then(|n| n.strip_prefix("ttys"))
            .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
}

impl From<rustix::fs::Stat> for NodeId {
    // The field types differ between platforms.
    #[allow(clippy::unnecessary_cast)]
    fn from(s: rustix::fs::Stat) -> NodeId {
        NodeId {
            dev: s.st_dev as u64,
            ino: s.st_ino as u64,
            ctime: (s.st_ctime as i64, s.st_ctime_nsec as i64),
        }
    }
}

/// `path` and each link it leads through, to the first name that is not a
/// link (which may be missing). Links in the directories above are not
/// followed.
#[cfg(target_os = "linux")]
fn chain(path: &Path) -> Vec<PathBuf> {
    // The kernel's own limit on links followed in one lookup.
    const MAX_LINKS: usize = 40;
    let mut names = vec![path.to_path_buf()];
    while names.len() <= MAX_LINKS {
        let last = names.last().expect("starts with the path");
        let Ok(target) = std::fs::read_link(last) else {
            break;
        };
        let next = match last.parent() {
            Some(dir) if target.is_relative() => dir.join(target),
            _ => target,
        };
        if names.contains(&next) {
            break;
        }
        names.push(next);
    }
    names
}

/// Watches for the device at a path to come back: the directory entries it
/// resolves through are watched with inotify on Linux, and looked at once
/// per second on macOS.
pub struct DeviceWatch {
    path: PathBuf,
    track: Track,
    /// Directory and name of each entry whose change can bring the device
    /// back.
    #[cfg(target_os = "linux")]
    entries: Vec<(PathBuf, OsString)>,
    /// The node last reported (or the one that went away).
    last: Option<NodeId>,
    /// Look at the path on the next check without waiting for an event.
    pending: bool,
    poll_at: Option<Instant>,
    retry_at: Option<Instant>,
    /// When the retries for `last` began.
    retry_since: Option<Instant>,
    /// None when no inotify instance can be had (the per-user limit): the
    /// path is polled then.
    #[cfg(target_os = "linux")]
    inotify: Option<OwnedFd>,
    /// The watch of each entry's directory; None while it cannot be watched
    /// (missing: udev removes `/dev/serial/by-id` with its last link).
    #[cfg(target_os = "linux")]
    wds: Vec<Option<i32>>,
}

impl DeviceWatch {
    /// Watches `path`, which resolved to `resolved` and whose identity (as
    /// `track` takes it) was `gone`. The first check looks at the path at
    /// once: the device may be back already.
    pub fn new(
        path: &Path,
        resolved: &Path,
        gone: Option<NodeId>,
        track: Track,
    ) -> DeviceWatch {
        #[cfg(target_os = "linux")]
        let entries = {
            let mut entries = Vec::new();
            let names = match track {
                Track::Node => {
                    let mut names = chain(path);
                    names.push(resolved.to_path_buf());
                    names
                }
                Track::Link => vec![path.to_path_buf()],
            };
            for p in &names {
                if let (Some(dir), Some(name)) = (p.parent(), p.file_name()) {
                    let dir = if dir.as_os_str().is_empty() {
                        Path::new(".")
                    } else {
                        dir
                    };
                    let entry = (dir.to_path_buf(), name.to_os_string());
                    if !entries.contains(&entry) {
                        entries.push(entry);
                    }
                }
            }
            entries
        };
        // Polling looks at the path itself.
        #[cfg(not(target_os = "linux"))]
        let _ = resolved;
        #[cfg(target_os = "linux")]
        let inotify = rustix::fs::inotify::init(
            rustix::fs::inotify::CreateFlags::CLOEXEC
                | rustix::fs::inotify::CreateFlags::NONBLOCK,
        )
        .ok();
        let mut watch = DeviceWatch {
            path: path.to_path_buf(),
            track,
            #[cfg(target_os = "linux")]
            wds: vec![None; entries.len()],
            #[cfg(target_os = "linux")]
            entries,
            last: gone,
            pending: true,
            poll_at: None,
            retry_at: None,
            retry_since: None,
            #[cfg(target_os = "linux")]
            inotify,
        };
        watch.arm(Instant::now());
        watch
    }

    /// Readable when an event may mean the device is back. None on macOS,
    /// or without inotify.
    pub fn fd(&self) -> Option<BorrowedFd<'_>> {
        #[cfg(target_os = "linux")]
        return self.inotify.as_ref().map(|fd| fd.as_fd());
        #[cfg(not(target_os = "linux"))]
        None
    }

    /// When [`check`](Self::check) should be called without an event.
    pub fn deadline(&self) -> Option<Instant> {
        [self.poll_at, self.retry_at].into_iter().flatten().min()
    }

    /// Whether the path names a node (or, tracking the link, is a link)
    /// other than the last one reported, or, when a [`retry`](Self::retry)
    /// is due, any node: reopen it then.
    pub fn check(&mut self) -> io::Result<bool> {
        let now = Instant::now();
        let mut look = std::mem::take(&mut self.pending);
        let retry = self.retry_at.is_some_and(|t| now >= t);
        if retry {
            self.retry_at = None;
            look = true;
        }
        if self.poll_at.is_some_and(|t| now >= t) {
            self.poll_at = None;
            look = true;
            self.arm(now);
        }
        #[cfg(target_os = "linux")]
        {
            look |= self.read_events(now)?;
        }
        if !look {
            return Ok(false);
        }
        let id = match self.track {
            Track::Node => NodeId::at(&self.path),
            Track::Link => {
                NodeId::at(&self.path).and_then(|_| NodeId::link_at(&self.path))
            }
        };
        match id {
            Ok(id) if Some(id) != self.last => {
                self.last = Some(id);
                self.retry_since = None;
                Ok(true)
            }
            Ok(_) => Ok(retry),
            Err(_) => Ok(false),
        }
    }

    /// Asks for another look at the same node in a moment, after an open
    /// that failed with EACCES or ENOENT. False once the node has had
    /// RETRY_FOR of retries; a change to it starts them again.
    pub fn retry(&mut self) -> bool {
        let now = Instant::now();
        let since = *self.retry_since.get_or_insert(now);
        if now.duration_since(since) >= RETRY_FOR {
            return false;
        }
        self.retry_at = Some(now + RETRY);
        true
    }

    /// Asks for another look at the same node after `delay`, with no limit:
    /// for a device another program holds, which may let go of it.
    pub fn retry_in(&mut self, delay: Duration) {
        self.retry_at = Some(Instant::now() + delay);
    }

    /// Watches what can be watched, and schedules a poll for the rest.
    #[cfg(target_os = "linux")]
    fn arm(&mut self, now: Instant) {
        use rustix::fs::inotify::WatchFlags;
        let mut missing = false;
        for (wd, (dir, _)) in self.wds.iter_mut().zip(&self.entries) {
            if let (None, Some(inotify)) = (*wd, &self.inotify) {
                *wd = rustix::fs::inotify::add_watch(
                    inotify,
                    dir.as_path(),
                    WatchFlags::CREATE
                        | WatchFlags::MOVED_TO
                        | WatchFlags::ATTRIB
                        | WatchFlags::ONLYDIR,
                )
                .ok();
                // Made since the last look: what it holds came unseen.
                self.pending |= wd.is_some();
            }
            missing |= wd.is_none();
        }
        if missing {
            self.poll_at = Some(now + POLL);
        }
    }

    #[cfg(not(target_os = "linux"))]
    fn arm(&mut self, now: Instant) {
        self.poll_at = Some(now + POLL);
    }

    /// Takes the queued events; true if one is about a watched entry.
    #[cfg(target_os = "linux")]
    fn read_events(&mut self, now: Instant) -> io::Result<bool> {
        use rustix::fs::inotify::{ReadFlags, Reader};
        let mut buf = [std::mem::MaybeUninit::<u8>::uninit(); 4096];
        let Some(inotify) = &self.inotify else {
            return Ok(std::mem::take(&mut self.pending));
        };
        let mut reader = Reader::new(inotify, &mut buf);
        let mut look = false;
        let mut lost = false;
        loop {
            let event = match reader.next() {
                Ok(event) => event,
                Err(rustix::io::Errno::AGAIN) => break,
                Err(rustix::io::Errno::INTR) => continue,
                Err(e) => return Err(io::Error::new(e.kind(), format!("inotify: {e}"))),
            };
            if event.events().contains(ReadFlags::QUEUE_OVERFLOW) {
                look = true;
                continue;
            }
            for (i, (_, name)) in self.entries.iter().enumerate() {
                if self.wds[i] != Some(event.wd()) {
                    continue;
                }
                if event.events().contains(ReadFlags::IGNORED) {
                    // The directory went away.
                    self.wds[i] = None;
                    lost = true;
                } else if event
                    .file_name()
                    .is_some_and(|n| n.to_bytes() == name.as_encoded_bytes())
                {
                    look = true;
                }
            }
        }
        if lost {
            self.arm(now);
        }
        Ok(look || std::mem::take(&mut self.pending))
    }
}
