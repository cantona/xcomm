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

pub struct Spawned {
    pub child: std::process::Child,
    /// xcomm's terminal: write to `host.master` to type, read it to see.
    pub host: Pty,
    pub lock_dir: PathBuf,
}

impl Drop for Spawned {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.lock_dir);
    }
}

/// A fresh, empty lock directory for one test.
pub fn lock_dir(tag: &str) -> PathBuf {
    use std::sync::atomic::{AtomicUsize, Ordering};
    static N: AtomicUsize = AtomicUsize::new(0);
    let d = std::env::temp_dir().join(format!(
        "xcomm-test-{tag}-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::SeqCst)
    ));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// Runs xcomm with a pty as its controlling terminal (stdin, stdout and
/// stderr), a private lock directory and no prefix-related environment.
pub fn spawn_xcomm(args: &[&str], lock_dir: &std::path::Path) -> Spawned {
    spawn_in_pty(env!("CARGO_BIN_EXE_xcomm"), args, lock_dir)
}

/// Like spawn_xcomm, for any program (for example a shell that starts xcomm).
pub fn spawn_in_pty(program: &str, args: &[&str], lock_dir: &std::path::Path) -> Spawned {
    spawn_with_env(program, args, lock_dir, &[])
}

/// Like spawn_in_pty, with `env` set after the prefix variables are removed.
pub fn spawn_with_env(
    program: &str,
    args: &[&str],
    lock_dir: &std::path::Path,
    env: &[(&str, &str)],
) -> Spawned {
    spawn_with(program, args, lock_dir, env, None)
}

/// Like spawn_with_env, with stdout sent to `stdout` instead of the pty.
pub fn spawn_with(
    program: &str,
    args: &[&str],
    lock_dir: &std::path::Path,
    env: &[(&str, &str)],
    stdout: Option<std::process::Stdio>,
) -> Spawned {
    use std::os::unix::process::CommandExt;
    let host = pty();
    let slave = host.slave.try_clone().unwrap();
    let mut cmd = std::process::Command::new(program);
    cmd.args(args)
        .env("XCOMM_LOCK_DIR", lock_dir)
        .stdin(std::process::Stdio::from(slave.try_clone().unwrap()))
        .stdout(stdout.unwrap_or_else(|| std::process::Stdio::from(slave.try_clone().unwrap())))
        .stderr(std::process::Stdio::from(slave));
    for v in [
        "XCOMM_PREFIX",
        "HERDR_ENV",
        "HC_TERMINAL_ID",
        "TMUX",
        "STY",
        "ZELLIJ",
    ] {
        cmd.env_remove(v);
    }
    cmd.envs(env.iter().copied());
    // SAFETY: setsid and ioctl are async-signal-safe.
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() < 0 || libc::ioctl(0, libc::TIOCSCTTY as _, 0) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let child = cmd.spawn().unwrap();
    Spawned {
        child,
        host,
        lock_dir: lock_dir.to_path_buf(),
    }
}

/// Waits for the child to exit, killing it after `timeout`.
pub fn wait_exit(child: &mut std::process::Child, timeout: Duration) -> std::process::ExitStatus {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(s) = child.try_wait().unwrap() {
            return s;
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            panic!("child still running after {timeout:?}");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}
