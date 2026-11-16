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
    // Concurrent opens of /dev/ptmx on macOS now and then fail with -6, the
    // kernel's private EREDRIVEOPEN; a plain C loop of posix_openpt on 16
    // threads shows it too. Another try succeeds.
    let mut tries = 0;
    let master = loop {
        match openpt(flags) {
            Err(e) if e.raw_os_error() == -6 && tries < 10 => tries += 1,
            r => break r.unwrap(),
        }
    };
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
    read_until_after(fd, needle, timeout, Vec::new())
}

/// Like read_until, with `got` already read.
fn read_until_after(
    fd: BorrowedFd<'_>,
    needle: &[u8],
    timeout: Duration,
    mut got: Vec<u8>,
) -> Vec<u8> {
    assert!(!needle.is_empty(), "read_until needs a non-empty needle");
    let deadline = Instant::now() + timeout;
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
    /// Read its modes through `host.master` too: macOS revokes `host.slave`
    /// when xcomm, its session leader, exits.
    pub host: Pty,
    pub lock_dir: PathBuf,
    /// Terminal output read by `free_exiting`; `read_after_exit` hands it
    /// out first.
    tail: Vec<u8>,
}

impl Spawned {
    /// Waits for the child to exit, killing it after `timeout`.
    pub fn wait_exit(&mut self, timeout: Duration) -> std::process::ExitStatus {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(s) = self.child.try_wait().unwrap() {
                return s;
            }
            if Instant::now() > deadline {
                let _ = self.child.kill();
                panic!("child still running after {timeout:?}");
            }
            self.free_exiting();
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// Reads the terminal until `needle`, after the child has exited.
    pub fn read_after_exit(&mut self, needle: &[u8], timeout: Duration) -> Vec<u8> {
        let got = std::mem::take(&mut self.tail);
        read_until_after(self.host.master.as_fd(), needle, timeout, got)
    }

    /// On macOS a session leader cannot finish exiting while output it
    /// wrote to its terminal is unread and another descriptor on that
    /// terminal is open, as `host.slave` is. Once the child is past its own
    /// code, output is restarted (a test may have stopped it) and read into
    /// `tail`. Linux does not wait.
    #[cfg(target_os = "macos")]
    fn free_exiting(&mut self) {
        if !in_exit(self.child.id()) {
            return;
        }
        let _ = rustix::termios::tcflow(&self.host.slave, rustix::termios::Action::OOn);
        let mut buf = [0u8; 4096];
        loop {
            let mut fds = [PollFd::new(&self.host.master, PollFlags::IN)];
            match poll(
                &mut fds,
                Some(&rustix::event::Timespec {
                    tv_sec: 0,
                    tv_nsec: 0,
                }),
            ) {
                Ok(n) if n > 0 && fds[0].revents().contains(PollFlags::IN) => {}
                _ => return,
            }
            match rustix::io::read(&self.host.master, &mut buf) {
                Ok(n) if n > 0 => self.tail.extend_from_slice(&buf[..n]),
                _ => return,
            }
        }
    }

    #[cfg(not(target_os = "macos"))]
    fn free_exiting(&mut self) {}
}

/// Whether process `pid` is working its way through exit (the E in ps).
/// proc_pidinfo cannot tell: it fails with ESRCH from then on.
#[cfg(target_os = "macos")]
fn in_exit(pid: u32) -> bool {
    // <sys/sysctl.h> struct kinfo_proc (not in the libc crate) on 64-bit
    // macOS: 648 bytes, kp_proc.p_flag at byte 32. P_WEXIT from <sys/proc.h>.
    const KINFO_PROC_SIZE: usize = 648;
    const P_FLAG_AT: usize = 32;
    const P_WEXIT: u32 = 0x2000;
    // Room for a larger struct too, so a layout change shows as a size and
    // not as ENOMEM.
    let mut info = [0u8; 4096];
    let mut size = info.len();
    let mut mib = [
        libc::CTL_KERN,
        libc::KERN_PROC,
        libc::KERN_PROC_PID,
        pid as libc::c_int,
    ];
    // SAFETY: `info` is `size` bytes; sysctl writes at most that and stores
    // the length it wrote in `size`.
    let r = unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            mib.len() as libc::c_uint,
            info.as_mut_ptr().cast(),
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    if r != 0 {
        in_exit_failure(&format!(
            "in_exit: sysctl KERN_PROC_PID {pid}: {}",
            std::io::Error::last_os_error()
        ));
        return false;
    }
    // No such process (already reaped).
    if size == 0 {
        return false;
    }
    if size != KINFO_PROC_SIZE {
        in_exit_failure(&format!(
            "in_exit: kinfo_proc is {size} bytes, not {KINFO_PROC_SIZE}"
        ));
        return false;
    }
    let p_flag = u32::from_ne_bytes(info[P_FLAG_AT..P_FLAG_AT + 4].try_into().unwrap());
    p_flag & P_WEXIT != 0
}

/// Reports an in_exit failure the first time only: Drop asks every 10 ms.
#[cfg(target_os = "macos")]
fn in_exit_failure(msg: &str) {
    static REPORTED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    if !REPORTED.swap(true, std::sync::atomic::Ordering::Relaxed) {
        harness_failure(msg);
    }
}

/// Fails the test, or only reports `msg` when the thread is already
/// unwinding: a second panic in a Drop would abort the whole test binary.
fn harness_failure(msg: &str) {
    if std::thread::panicking() {
        eprintln!("{msg}");
    } else {
        panic!("{msg}");
    }
}

impl Drop for Spawned {
    fn drop(&mut self) {
        let _ = self.child.kill();
        // Not wait(): a child that cannot finish exiting would hang the test
        // for good. One left over is let go when `host` closes below.
        let deadline = Instant::now() + Duration::from_secs(10);
        while matches!(self.child.try_wait(), Ok(None)) && Instant::now() < deadline {
            self.free_exiting();
            std::thread::sleep(Duration::from_millis(10));
        }
        if matches!(self.child.try_wait(), Ok(None)) {
            // Its lock directory stays: the child may still hold a lock there.
            harness_failure(&format!(
                "pid {} still running 10 s after SIGKILL; left behind",
                self.child.id()
            ));
            return;
        }
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
/// stderr), a private lock directory, no prefix-related environment,
/// NO_COLOR unset and TERM=xterm.
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
        // Control sockets go under the test's own directory.
        .env("XDG_RUNTIME_DIR", lock_dir)
        .stdin(std::process::Stdio::from(slave.try_clone().unwrap()))
        .stdout(stdout.unwrap_or_else(|| std::process::Stdio::from(slave.try_clone().unwrap())))
        .stderr(std::process::Stdio::from(slave));
    for v in [
        "XCOMM_PREFIX",
        "HERDR_ENV",
        // A session would rename the pane the tests run in.
        "HERDR_SOCKET_PATH",
        "HERDR_PANE_ID",
        "HC_TERMINAL_ID",
        "TMUX",
        "STY",
        "ZELLIJ",
    ] {
        cmd.env_remove(v);
    }
    // Color depends on these; tests that care set them through `env`.
    cmd.env_remove("NO_COLOR").env("TERM", "xterm");
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
        tail: Vec::new(),
    }
}

/// Waits for a child not started by `spawn_with` to exit, killing it after
/// `timeout`.
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

/// Waits until process `pid` is stopped (state T in /proc).
#[cfg(target_os = "linux")]
pub fn wait_stopped(pid: i32, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    loop {
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap();
        if stat.rsplit(") ").next().unwrap().starts_with('T') {
            return;
        }
        assert!(Instant::now() < deadline, "process {pid} not stopped");
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Waits until process `pid` is inside a write(2) to its stdout. Reading
/// /proc/<pid>/syscall needs ptrace access; where that is denied
/// (kernel.yama.ptrace_scope 2 or 3) it waits a fixed time instead, which
/// is usually enough.
#[cfg(target_os = "linux")]
pub fn wait_blocked_writing_stdout(pid: i32, timeout: Duration) {
    let want = format!("{} 0x1 ", libc::SYS_write);
    let deadline = Instant::now() + timeout;
    loop {
        let call = match std::fs::read_to_string(format!("/proc/{pid}/syscall")) {
            Ok(call) => call,
            Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
                eprintln!("/proc/{pid}/syscall: {e}; waiting 200 ms instead");
                std::thread::sleep(Duration::from_millis(200));
                return;
            }
            Err(e) => panic!("/proc/{pid}/syscall: {e}"),
        };
        if call.starts_with(&want) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "process {pid} not blocked writing stdout: {call}"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}
