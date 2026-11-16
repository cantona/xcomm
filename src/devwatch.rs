//! Serial device nodes coming and going: inotify on `<root>/dev`,
//! `<root>/dev/serial` and `<root>/dev/serial/by-id` on Linux, watched again
//! after each event, as udev removes the by-id directory with the last
//! adapter and makes it again with the next. Elsewhere there is nothing to
//! watch and callers look again on a timer.

use std::os::fd::BorrowedFd;
use std::path::Path;
#[cfg(target_os = "linux")]
use std::path::PathBuf;
use std::time::Duration;

pub struct DevWatch {
    #[cfg(target_os = "linux")]
    root: PathBuf,
    /// Why nothing is watched, when /dev could not be.
    problem: Option<String>,
    #[cfg(target_os = "linux")]
    fd: Option<std::os::fd::OwnedFd>,
}

impl DevWatch {
    /// Watches below `root` ("/" but in tests).
    pub fn new(root: &Path) -> DevWatch {
        #[cfg(target_os = "linux")]
        {
            use rustix::fs::inotify::{CreateFlags, init};
            let mut w = DevWatch {
                root: root.to_path_buf(),
                problem: None,
                fd: None,
            };
            match init(CreateFlags::CLOEXEC | CreateFlags::NONBLOCK) {
                Ok(fd) => w.fd = Some(fd),
                Err(e) => w.problem = Some(format!("inotify: {e}")),
            }
            // Without /dev itself (the user's watch limit reached, say),
            // nothing is watched: callers look on a timer then.
            if let Err(e) = w.add() {
                w.problem = Some(format!("watching {}: {e}", w.root.join("dev").display()));
                w.fd = None;
            }
            w
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = root;
            DevWatch {
                problem: Some("no inotify here".into()),
            }
        }
    }

    /// What to poll for events; None when there is none to watch.
    pub fn fd(&self) -> Option<BorrowedFd<'_>> {
        #[cfg(target_os = "linux")]
        return self.fd.as_ref().map(std::os::fd::AsFd::as_fd);
        #[cfg(not(target_os = "linux"))]
        None
    }

    /// Why nothing is watched, if so.
    pub fn problem(&self) -> Option<&str> {
        self.problem.as_deref()
    }

    /// Watches the directories; the by-id ones need not exist yet. An
    /// error watching /dev itself is returned.
    #[cfg(target_os = "linux")]
    fn add(&self) -> rustix::io::Result<()> {
        use rustix::fs::inotify::{WatchFlags, add_watch};
        let Some(fd) = &self.fd else {
            return Ok(());
        };
        let flags =
            WatchFlags::CREATE | WatchFlags::DELETE | WatchFlags::MOVED_TO | WatchFlags::MOVED_FROM;
        add_watch(fd, self.root.join("dev"), flags)?;
        for dir in ["dev/serial", "dev/serial/by-id"] {
            let _ = add_watch(fd, self.root.join(dir), flags);
        }
        Ok(())
    }

    /// Reads the events there are, and watches again what came back;
    /// whether there were any.
    pub fn take_events(&self) -> bool {
        let Some(fd) = self.fd() else {
            return false;
        };
        let mut any = false;
        let mut buf = [0u8; 4096];
        while matches!(rustix::io::read(fd, &mut buf), Ok(n) if n > 0) {
            any = true;
        }
        #[cfg(target_os = "linux")]
        if any {
            let _ = self.add();
        }
        any
    }

    /// Waits for an event or `most`; after an event, a little longer for
    /// udev to make the links.
    pub fn wait(&self, most: Duration) {
        use rustix::event::{PollFd, PollFlags, Timespec, poll};
        let Some(fd) = self.fd() else {
            std::thread::sleep(most);
            return;
        };
        let mut fds = [PollFd::from_borrowed_fd(fd, PollFlags::IN)];
        let timeout = Timespec::try_from(most).unwrap_or_default();
        if matches!(poll(&mut fds, Some(&timeout)), Ok(n) if n > 0) {
            std::thread::sleep(Duration::from_millis(100));
            self.take_events();
        }
    }
}
