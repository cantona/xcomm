//! Shared helpers for integration tests. Not every test file uses every helper.
#![allow(dead_code)]

use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use rustix::event::{PollFd, PollFlags, poll};
use rustix::fs::{Mode, OFlags, open};
use rustix::pty::{OpenptFlags, grantpt, openpt, ptsname, unlockpt};

pub struct Pty {
    pub master: OwnedFd,
    pub slave: OwnedFd,
    pub slave_path: PathBuf,
}

pub fn pty() -> Pty {
    #[cfg(target_os = "linux")]
    let flags = OpenptFlags::RDWR | OpenptFlags::NOCTTY | OpenptFlags::CLOEXEC;
    #[cfg(not(target_os = "linux"))]
    let flags = OpenptFlags::RDWR | OpenptFlags::NOCTTY;
    let master = openpt(flags).unwrap();
    // macOS has no O_CLOEXEC for posix_openpt; set it so a child spawned by
    // another test thread cannot hold this master open.
    #[cfg(not(target_os = "linux"))]
    rustix::io::fcntl_setfd(&master, rustix::io::FdFlags::CLOEXEC).unwrap();
    grantpt(&master).unwrap();
    unlockpt(&master).unwrap();
    let name = ptsname(&master, Vec::new()).unwrap();
    let slave_path = PathBuf::from(name.to_str().unwrap());
    let slave = open(
        &slave_path,
        OFlags::RDWR | OFlags::NOCTTY | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .unwrap();
    Pty {
        master,
        slave,
        slave_path,
    }
}

/// Reads until `needle` has been seen; panics with the bytes read so far on
/// timeout.
pub fn read_until(fd: BorrowedFd<'_>, needle: &[u8], timeout: Duration) -> Vec<u8> {
    assert!(!needle.is_empty(), "read_until needs a non-empty needle");
    let deadline = Instant::now() + timeout;
    let mut got = Vec::new();
    let mut buf = [0u8; 4096];
    while !got.windows(needle.len()).any(|w| w == needle) {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            panic!("timeout waiting for {needle:?}; got {got:?}");
        }
        let ts = rustix::event::Timespec::try_from(left).unwrap();
        let mut fds = [PollFd::new(&fd, PollFlags::IN)];
        if poll(&mut fds, Some(&ts)).unwrap() == 0 {
            continue;
        }
        // A pty master reads EIO, not 0, once the slave side is closed.
        let n = match rustix::io::read(fd, &mut buf) {
            Ok(n) => n,
            Err(rustix::io::Errno::IO) => 0,
            Err(e) => panic!("read failed: {e}; got {got:?}"),
        };
        assert!(n > 0, "EOF waiting for {needle:?}; got {got:?}");
        got.extend_from_slice(&buf[..n]);
    }
    got
}

pub fn write_all(fd: BorrowedFd<'_>, mut bytes: &[u8]) {
    while !bytes.is_empty() {
        let n = rustix::io::write(fd, bytes).unwrap();
        bytes = &bytes[n..];
    }
}

pub fn borrow(fd: &OwnedFd) -> BorrowedFd<'_> {
    fd.as_fd()
}
