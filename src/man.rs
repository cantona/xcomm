// SPDX-FileCopyrightText: 2026 Su Kang Yin <paradyse@gmail.com>
// SPDX-License-Identifier: GPL-2.0-only

//! The manual page, built in: `xcomm man` shows it wherever xcomm was
//! installed from, as cargo installs no man page.

use std::io::{IsTerminal, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};

pub const PAGE: &str = include_str!("../xcomm.1");

/// What a Ctrl-C in the pager, or the terminal closing, sends the group,
/// and what ends xcomm alone.
const CAUGHT: [libc::c_int; 4] =
    [libc::SIGINT, libc::SIGQUIT, libc::SIGHUP, libc::SIGTERM];

/// The man being waited for; the last of CAUGHT that came (0: none); and
/// whether a SIGTERM did, which is passed on to man and ends xcomm once the
/// file is gone.
static MAN: AtomicI32 = AtomicI32::new(0);
static CAME: AtomicI32 = AtomicI32::new(0);
static TERMED: AtomicBool = AtomicBool::new(false);

extern "C" fn on_signal(sig: libc::c_int) {
    CAME.store(sig, Ordering::SeqCst);
    if sig == libc::SIGTERM {
        TERMED.store(true, Ordering::SeqCst);
        let pid = MAN.load(Ordering::SeqCst);
        if pid > 0 {
            // SAFETY: kill is async-signal-safe.
            unsafe { libc::kill(pid, libc::SIGTERM) };
        }
    }
}

/// Ends xcomm as `sig` ended man, the way a shell expects of what it ran.
fn end_by(sig: libc::c_int) -> ! {
    // SIGQUIT's default action would dump xcomm's core (and on macOS file
    // a crash report in its name): the status a shell shows for it instead.
    if sig != libc::SIGQUIT {
        // SAFETY: the default action back, the signal unblocked (a parent
        // may have blocked it, and man unblocks its own), then raised.
        unsafe {
            libc::signal(sig, libc::SIG_DFL);
            let mut set: libc::sigset_t = std::mem::zeroed();
            libc::sigemptyset(&mut set);
            libc::sigaddset(&mut set, sig);
            libc::pthread_sigmask(libc::SIG_UNBLOCK, &set, std::ptr::null_mut());
            libc::raise(sig);
        }
    }
    std::process::exit(128 + sig)
}

/// CAUGHT taken by on_signal while xcomm shows the page, from before the
/// file is made, and put back when dropped: no signal can leave the file.
/// A handler, unlike an ignored signal, is not inherited, so man starts
/// with them as they were; one the parent ignored stays ignored, for xcomm
/// and man alike.
/// SIGCHLD too, if the parent ignored it, is set back to the default (and
/// so man starts with that): ignored, man would be reaped by the kernel,
/// and there would be no status to wait for.
struct Caught(Vec<(libc::c_int, libc::sighandler_t)>);

impl Caught {
    fn new() -> Caught {
        let mut was = Vec::new();
        // SAFETY: reading SIGCHLD's action, and setting the default one.
        unsafe {
            let mut now: libc::sigaction = std::mem::zeroed();
            libc::sigaction(libc::SIGCHLD, std::ptr::null(), &mut now);
            if now.sa_sigaction == libc::SIG_IGN {
                was.push((libc::SIGCHLD, libc::signal(libc::SIGCHLD, libc::SIG_DFL)));
            }
        }
        for sig in CAUGHT {
            // SAFETY: reading the signal's action, then setting a handler
            // that only stores and calls kill.
            unsafe {
                let mut now: libc::sigaction = std::mem::zeroed();
                libc::sigaction(sig, std::ptr::null(), &mut now);
                if now.sa_sigaction != libc::SIG_IGN {
                    let handler = on_signal as *const () as libc::sighandler_t;
                    was.push((sig, libc::signal(sig, handler)));
                }
            }
        }
        Caught(was)
    }
}

impl Drop for Caught {
    fn drop(&mut self) {
        for &(sig, was) in &self.0 {
            // SAFETY: putting back what signal() returned for it.
            unsafe { libc::signal(sig, was) };
        }
    }
}

/// Runs man on `path`, which is gone by the time it returns.
fn run_man(path: &std::path::Path) -> std::io::Result<std::process::ExitStatus> {
    let man = std::process::Command::new("man").arg(path).spawn();
    let mut man = match man {
        Ok(man) => man,
        Err(e) => {
            let _ = std::fs::remove_file(path);
            return Err(e);
        }
    };
    let pid = man.id() as i32;
    MAN.store(pid, Ordering::SeqCst);
    // One that came while man was starting missed it: it gets it now, and
    // a SIGTERM whatever came after it.
    let came = match CAME.load(Ordering::SeqCst) {
        _ if TERMED.load(Ordering::SeqCst) => libc::SIGTERM,
        came => came,
    };
    if came != 0 {
        // SAFETY: kill on the child just spawned, not yet reaped.
        unsafe { libc::kill(pid, came) };
    }
    // While man runs, a Ctrl-C in the pager or the terminal closing reaches
    // the whole group: man and its pager deal with it, and xcomm, only
    // noting it, waits. Waited for without reaping first: while its pid is
    // still man's, a SIGTERM passed on cannot reach another process that
    // got the pid.
    loop {
        // SAFETY: waitid fills `info`; WNOWAIT leaves man to be reaped.
        let waited = unsafe {
            let mut info: libc::siginfo_t = std::mem::zeroed();
            libc::waitid(
                libc::P_PID,
                pid as libc::id_t,
                &mut info,
                libc::WEXITED | libc::WNOWAIT,
            )
        };
        if waited == 0
            || std::io::Error::last_os_error().raw_os_error() != Some(libc::EINTR)
        {
            break;
        }
    }
    MAN.store(0, Ordering::SeqCst);
    let status = man.wait();
    let _ = std::fs::remove_file(path);
    status
}

/// On a terminal, opens the page in man; otherwise writes its source to
/// stdout (`xcomm man > xcomm.1`).
pub fn show() -> Result<(), String> {
    if !std::io::stdout().is_terminal() {
        return crate::out::stdout(PAGE.as_bytes())
            .map_err(|e| format!("cannot write to stdout: {e}"));
    }
    let caught = Caught::new();
    // man takes an argument with a '/' in it as a file, on Linux (man-db)
    // and macOS alike. A file of our own, made new, so nobody else's is
    // opened or swapped in; one already there under the name (in a shared
    // /tmp, anyone can make it) means the next name.
    // An empty TMPDIR gives no directory at all, and a name without a '/'
    // is looked up as a page.
    let dir = Some(std::env::temp_dir())
        .filter(|d| d.is_absolute())
        .unwrap_or_else(|| "/tmp".into());
    let mut tries = 0;
    let (path, mut file) = loop {
        let path = dir.join(format!("xcomm-{}-{tries}.1", std::process::id()));
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
        {
            Ok(f) => break (path, f),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists && tries < 100 => {
                tries += 1
            }
            Err(e) => return Err(format!("{}: {e}", path.display())),
        }
    };
    if let Err(e) = file.write_all(PAGE.as_bytes()) {
        let _ = std::fs::remove_file(&path);
        return Err(format!("{}: {e}", path.display()));
    }
    drop(file);
    // One that came before man would start: xcomm ends by it, as man would
    // (by a SIGTERM, whatever came after it).
    let came = match CAME.load(Ordering::SeqCst) {
        _ if TERMED.load(Ordering::SeqCst) => libc::SIGTERM,
        came => came,
    };
    if came != 0 {
        let _ = std::fs::remove_file(&path);
        drop(caught);
        end_by(came);
    }
    let shown = run_man(&path);
    drop(caught);
    // man ended by one of them (macOS man does, on a Ctrl-C in its pager),
    // or xcomm was told to end: so does xcomm, not with an error.
    use std::os::unix::process::ExitStatusExt;
    if TERMED.load(Ordering::SeqCst) {
        end_by(libc::SIGTERM);
    }
    let instead = "xcomm man > xcomm.1 writes the page out instead";
    match shown {
        Ok(s) if s.success() => Ok(()),
        Ok(s) => match s.signal() {
            Some(sig) if CAUGHT.contains(&sig) => end_by(sig),
            _ => Err(format!("man: {s} ({instead})")),
        },
        Err(e) => Err(format!("man: {e} ({instead})")),
    }
}
