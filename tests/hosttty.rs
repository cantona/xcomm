mod support;

use std::os::fd::AsFd;
use std::process::{Command, Stdio};

use rustix::termios::{
    ControlModes, InputModes, LocalModes, OutputModes, SpecialCodeIndex, Termios, tcgetattr,
};
use xcomm::hosttty::{RawMode, SignalPipe};

/// RawMode is process-wide (one at a time) and tests run on parallel threads.
static RAW_MODE: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn serial() -> std::sync::MutexGuard<'static, ()> {
    RAW_MODE.lock().unwrap_or_else(|e| e.into_inner())
}

fn same_modes(a: &Termios, b: &Termios) -> bool {
    a.input_modes == b.input_modes
        && a.output_modes == b.output_modes
        && a.control_modes == b.control_modes
        && a.local_modes == b.local_modes
}

fn assert_raw(t: &Termios) {
    assert!(
        !t.local_modes.intersects(
            LocalModes::ECHO | LocalModes::ICANON | LocalModes::ISIG | LocalModes::IEXTEN
        )
    );
    assert!(!t.input_modes.intersects(
        InputModes::ICRNL
            | InputModes::IXON
            | InputModes::INLCR
            | InputModes::IGNCR
            | InputModes::ISTRIP
    ));
    assert!(!t.output_modes.contains(OutputModes::OPOST));
    assert!(t.control_modes.contains(ControlModes::CS8));
    assert!(!t.control_modes.contains(ControlModes::PARENB));
    assert_eq!(t.special_codes[SpecialCodeIndex::VMIN], 1);
    assert_eq!(t.special_codes[SpecialCodeIndex::VTIME], 0);
}

#[test]
fn raw_mode_sets_raw_attributes_then_restores_all_modes() {
    let _serial = serial();
    let p = support::pty();
    let before = tcgetattr(p.slave.as_fd()).unwrap();
    assert!(before.local_modes.contains(LocalModes::ICANON));
    {
        let _raw = RawMode::enter(p.slave.as_fd()).unwrap();
        assert_raw(&tcgetattr(p.slave.as_fd()).unwrap());
    }
    assert!(same_modes(&tcgetattr(p.slave.as_fd()).unwrap(), &before));
}

#[test]
fn raw_mode_clears_input_flow_control() {
    let _serial = serial();
    let p = support::pty();
    let mut t = tcgetattr(p.slave.as_fd()).unwrap();
    t.input_modes.insert(InputModes::IXOFF | InputModes::IXANY);
    rustix::termios::tcsetattr(p.slave.as_fd(), rustix::termios::OptionalActions::Now, &t).unwrap();
    let _raw = RawMode::enter(p.slave.as_fd()).unwrap();
    let now = tcgetattr(p.slave.as_fd()).unwrap();
    assert!(
        !now.input_modes
            .intersects(InputModes::IXOFF | InputModes::IXANY)
    );
}

#[test]
fn raw_mode_on_a_non_tty_fails_naming_the_call() {
    let _serial = serial();
    let (r, _w) = std::io::pipe().unwrap();
    let e = RawMode::enter(r.as_fd()).err().unwrap().to_string();
    assert!(e.contains("tcgetattr on fd"), "{e}");
}

#[test]
fn second_raw_mode_is_refused_and_the_first_stays_active() {
    let _serial = serial();
    let a = support::pty();
    let b = support::pty();
    let first = RawMode::enter(a.slave.as_fd()).unwrap();
    let e = RawMode::enter(b.slave.as_fd()).err().unwrap();
    assert_eq!(e.kind(), std::io::ErrorKind::AlreadyExists);
    assert!(
        tcgetattr(b.slave.as_fd())
            .unwrap()
            .local_modes
            .contains(LocalModes::ICANON)
    );
    assert_raw(&tcgetattr(a.slave.as_fd()).unwrap());
    drop(first);
    assert!(
        tcgetattr(a.slave.as_fd())
            .unwrap()
            .local_modes
            .contains(LocalModes::ICANON)
    );
    let again = RawMode::enter(b.slave.as_fd()).unwrap();
    drop(again);
}

/// Overflows the stack; the base case is never reached in practice.
fn recurse(n: u64) -> u64 {
    if n == u64::MAX {
        return 0;
    }
    let buf = [n; 512];
    std::hint::black_box(&buf);
    recurse(n + 1).wrapping_add(buf[0])
}

/// Child half of the `dies_in_raw_mode_*` tests: enters raw mode on stdin,
/// leaks the guard so no destructor can help, then dies the way
/// XCOMM_TEST_DEATH names.
#[test]
fn death_child() {
    let Some(kind) = std::env::var_os("XCOMM_TEST_DEATH") else {
        return;
    };
    if kind == "segv-ignored" {
        unsafe { libc::signal(libc::SIGSEGV, libc::SIG_IGN) };
    }
    if kind == "abort-chained" {
        extern "C" fn earlier(_: libc::c_int) {
            let msg = b"XCOMM-EARLIER-HANDLER\n";
            unsafe { libc::write(1, msg.as_ptr().cast(), msg.len()) };
        }
        unsafe { libc::signal(libc::SIGABRT, earlier as *const () as libc::sighandler_t) };
    }
    let stdin = std::io::stdin();
    let raw = RawMode::enter(stdin.as_fd()).unwrap();
    std::mem::forget(raw);
    // Not dumpable: the deliberate aborts must not invoke the core handler.
    // (exec resets this, so it is set here rather than in the parent.)
    #[cfg(target_os = "linux")]
    unsafe {
        libc::prctl(libc::PR_SET_DUMPABLE, 0, 0, 0, 0);
    }
    println!("XCOMM-ENTERED-RAW");
    match kind.to_str().unwrap() {
        "panic" => {
            // End the process from a hook chained after ours, as panic=abort
            // does, but with _exit: no abort handler, no atexit, no unwinding,
            // so only the panic hook can restore the terminal.
            let ours = std::panic::take_hook();
            std::panic::set_hook(Box::new(move |info| {
                ours(info);
                unsafe { libc::_exit(101) };
            }));
            panic!("deliberate");
        }
        "abort" | "abort-chained" => std::process::abort(),
        "alloc" => std::alloc::handle_alloc_error(std::alloc::Layout::new::<[u8; 4096]>()),
        "overflow" => {
            std::hint::black_box(recurse(0));
        }
        "exit" => std::process::exit(3),
        "segv" | "segv-ignored" => unsafe {
            std::ptr::write_volatile(std::ptr::null_mut::<u8>(), 1)
        },
        "quit" => unsafe {
            libc::raise(libc::SIGQUIT);
        },
        other => panic!("unknown death {other}"),
    }
}

fn die_in_raw_mode(kind: &str) -> std::process::ExitStatus {
    die_in_raw_mode_output(kind).status
}

fn die_in_raw_mode_output(kind: &str) -> std::process::Output {
    let p = support::pty();
    let before = tcgetattr(p.slave.as_fd()).unwrap();
    let slave = p.slave.try_clone().unwrap();
    let mut child = Command::new(std::env::current_exe().unwrap());
    use std::os::unix::process::CommandExt;
    // SAFETY: setrlimit is async-signal-safe. No core file for the
    // deliberate aborts on systems without PR_SET_DUMPABLE.
    unsafe {
        child.pre_exec(|| {
            let none = libc::rlimit {
                rlim_cur: 0,
                rlim_max: 0,
            };
            libc::setrlimit(libc::RLIMIT_CORE, &none);
            Ok(())
        });
    }
    let mut running = child
        .args(["--exact", "death_child", "--nocapture", "--test-threads=1"])
        .env("XCOMM_TEST_DEATH", kind)
        .stdin(Stdio::from(slave))
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    // A broken restore path can loop instead of dying; fail, do not hang.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while running.try_wait().unwrap().is_none() {
        if std::time::Instant::now() > deadline {
            running.kill().unwrap();
            panic!("{kind}: child still running after 10 s");
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    let out = running.wait_with_output().unwrap();
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("XCOMM-ENTERED-RAW"),
        "{kind}: child never entered raw mode"
    );
    let after = tcgetattr(p.slave.as_fd()).unwrap();
    assert!(
        same_modes(&after, &before),
        "{kind}: terminal not restored: {after:?}"
    );
    out
}

fn aborted(status: std::process::ExitStatus) -> bool {
    use std::os::unix::process::ExitStatusExt;
    status.signal() == Some(libc::SIGABRT)
}

#[test]
fn dies_in_raw_mode_by_panic() {
    assert_eq!(die_in_raw_mode("panic").code(), Some(101));
}

#[test]
fn abort_restore_chains_to_an_earlier_handler() {
    let out = die_in_raw_mode_output("abort-chained");
    assert!(aborted(out.status));
    assert!(String::from_utf8_lossy(&out.stdout).contains("XCOMM-EARLIER-HANDLER"));
}

#[test]
fn dies_in_raw_mode_by_abort() {
    assert!(aborted(die_in_raw_mode("abort")));
}

#[test]
fn dies_in_raw_mode_by_allocation_failure() {
    assert!(aborted(die_in_raw_mode("alloc")));
}

#[test]
fn dies_in_raw_mode_by_stack_overflow() {
    // Our SIGSEGV handler restores the terminal, then calls Rust's, which
    // reports the overflow and aborts. The nested handlers can exhaust the
    // small alternate signal stack, so the process may end by SIGSEGV
    // instead of SIGABRT; either way the terminal was restored first.
    let status = die_in_raw_mode("overflow");
    assert!(
        aborted(status) || killed_by(status, libc::SIGSEGV),
        "{status:?}"
    );
}

fn killed_by(status: std::process::ExitStatus, sig: libc::c_int) -> bool {
    use std::os::unix::process::ExitStatusExt;
    status.signal() == Some(sig)
}

#[test]
fn dies_in_raw_mode_by_segfault() {
    assert!(killed_by(die_in_raw_mode("segv"), libc::SIGSEGV));
}

#[test]
fn dies_in_raw_mode_by_segfault_even_if_sigsegv_was_ignored() {
    assert!(killed_by(die_in_raw_mode("segv-ignored"), libc::SIGSEGV));
}

#[test]
fn dies_in_raw_mode_by_sigquit() {
    assert!(killed_by(die_in_raw_mode("quit"), libc::SIGQUIT));
}

#[test]
fn dies_in_raw_mode_by_process_exit() {
    assert_eq!(die_in_raw_mode("exit").code(), Some(3));
}

/// Child half of `leaked_guard_keeps_the_hook_on_its_own_terminal`: a leaked
/// guard keeps raw mode registered for the rest of the process, so it runs
/// in its own process.
#[test]
fn leaked_guard_child() {
    if std::env::var_os("XCOMM_TEST_LEAK_CHILD").is_none() {
        return;
    }
    let a = support::pty();
    let a_slave = a.slave.try_clone().unwrap();
    let guard = RawMode::enter(a_slave.as_fd()).unwrap();
    let registered = guard.registered_fd();
    std::mem::forget(guard);
    drop(a_slave);
    // The registered fd is RawMode's own duplicate: still open, still the
    // same terminal, so a new file cannot take its number.
    let b = support::pty();
    assert_ne!(std::os::fd::AsRawFd::as_raw_fd(&b.master), registered);
    assert_ne!(std::os::fd::AsRawFd::as_raw_fd(&b.slave), registered);
    let fd = unsafe { std::os::fd::BorrowedFd::borrow_raw(registered) };
    assert!(rustix::termios::isatty(fd));
    println!("XCOMM-LEAK-CHILD-OK");
}

#[test]
fn leaked_guard_keeps_the_hook_on_its_own_terminal() {
    let out = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "leaked_guard_child",
            "--nocapture",
            "--test-threads=1",
        ])
        .env("XCOMM_TEST_LEAK_CHILD", "1")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stdout)
    );
    assert!(String::from_utf8_lossy(&out.stdout).contains("XCOMM-LEAK-CHILD-OK"));
}

fn handler_of(sig: libc::c_int) -> libc::sighandler_t {
    let mut old: libc::sigaction = unsafe { std::mem::zeroed() };
    unsafe { libc::sigaction(sig, std::ptr::null(), &mut old) };
    old.sa_sigaction
}

fn flags_of(sig: libc::c_int) -> libc::c_int {
    let mut old: libc::sigaction = unsafe { std::mem::zeroed() };
    unsafe { libc::sigaction(sig, std::ptr::null(), &mut old) };
    old.sa_flags
}

/// Child half of `termination_signal_interrupts_a_blocked_read`: it installs
/// a SIGTERM handler, which must not leak into the parallel test threads.
#[test]
fn eintr_child() {
    if std::env::var_os("XCOMM_TEST_EINTR_CHILD").is_none() {
        return;
    }
    let _sp = SignalPipe::install(&[libc::SIGTERM, libc::SIGTSTP, libc::SIGWINCH]).unwrap();
    assert_eq!(flags_of(libc::SIGTERM) & libc::SA_RESTART, 0);
    assert_eq!(flags_of(libc::SIGTSTP) & libc::SA_RESTART, 0);
    assert_ne!(flags_of(libc::SIGWINCH) & libc::SA_RESTART, 0);
    let (r, w) = std::io::pipe().unwrap();
    let (tid_tx, tid_rx) = std::sync::mpsc::channel();
    let (done_tx, done_rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        tid_tx.send(unsafe { libc::pthread_self() }).unwrap();
        let mut b = [0u8; 1];
        let n = unsafe {
            libc::read(
                std::os::fd::AsRawFd::as_raw_fd(&r),
                b.as_mut_ptr().cast(),
                1,
            )
        };
        done_tx
            .send((n, std::io::Error::last_os_error().raw_os_error()))
            .unwrap();
    });
    let tid = tid_rx.recv().unwrap();
    // A signal that lands before the thread is inside read() is consumed
    // without an EINTR, so keep signalling until the read returns.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let result = loop {
        unsafe { libc::pthread_kill(tid, libc::SIGTERM) };
        if let Ok(r) = done_rx.recv_timeout(std::time::Duration::from_millis(50)) {
            break r;
        }
        if std::time::Instant::now() > deadline {
            // Closing the write end makes the read return 0, so the test
            // fails below instead of hanging.
            drop(w);
            break done_rx.recv().unwrap();
        }
    };
    assert_eq!(result, (-1, Some(libc::EINTR)));
    println!("XCOMM-EINTR-CHILD-OK");
}

#[test]
fn termination_signal_interrupts_a_blocked_read() {
    let out = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "eintr_child", "--nocapture", "--test-threads=1"])
        .env("XCOMM_TEST_EINTR_CHILD", "1")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stdout)
    );
    assert!(String::from_utf8_lossy(&out.stdout).contains("XCOMM-EINTR-CHILD-OK"));
}

// The signal pipe is process-global, so its checks run as one test.
#[test]
fn signal_pipe_lifecycle() {
    let sp = SignalPipe::install(&[libc::SIGUSR1, libc::SIGUSR2]).unwrap();
    assert_eq!(
        SignalPipe::install(&[libc::SIGWINCH]).err().unwrap().kind(),
        std::io::ErrorKind::AlreadyExists
    );

    unsafe { libc::raise(libc::SIGUSR1) };
    unsafe { libc::raise(libc::SIGUSR2) };
    unsafe { libc::raise(libc::SIGUSR1) };
    assert_eq!(
        sp.drain().unwrap(),
        (1 << libc::SIGUSR1) | (1 << libc::SIGUSR2)
    );
    assert_eq!(sp.drain().unwrap(), 0);
    drop(sp);
    assert_eq!(handler_of(libc::SIGUSR1), libc::SIG_DFL);
    assert_eq!(handler_of(libc::SIGUSR2), libc::SIG_DFL);

    // A full pipe must not lose a signal.
    let sp = SignalPipe::install(&[libc::SIGUSR1, libc::SIGUSR2]).unwrap();
    for _ in 0..70_000 {
        unsafe { libc::raise(libc::SIGUSR2) };
    }
    unsafe { libc::raise(libc::SIGUSR1) };
    assert_eq!(
        sp.drain().unwrap(),
        (1 << libc::SIGUSR1) | (1 << libc::SIGUSR2)
    );
    drop(sp);

    // A non-default previous handler comes back.
    unsafe { libc::signal(libc::SIGUSR1, libc::SIG_IGN) };
    let sp = SignalPipe::install(&[libc::SIGUSR1]).unwrap();
    assert_ne!(handler_of(libc::SIGUSR1), libc::SIG_IGN);
    drop(sp);
    assert_eq!(handler_of(libc::SIGUSR1), libc::SIG_IGN);
    unsafe { libc::signal(libc::SIGUSR1, libc::SIG_DFL) };

    // A failed install rolls back the handlers it already set.
    #[cfg(target_os = "linux")]
    {
        // glibc reserves signal 32 for its own use; sigaction rejects it.
        let e = SignalPipe::install(&[libc::SIGUSR1, 32])
            .err()
            .unwrap()
            .to_string();
        assert!(e.contains("sigaction for signal 32"), "{e}");
        assert_eq!(handler_of(libc::SIGUSR1), libc::SIG_DFL);
    }

    assert!(
        SignalPipe::install(&[libc::SIGABRT])
            .err()
            .unwrap()
            .to_string()
            .contains("reserved")
    );
    for fault in [
        libc::SIGSEGV,
        libc::SIGBUS,
        libc::SIGILL,
        libc::SIGFPE,
        libc::SIGTRAP,
        libc::SIGQUIT,
    ] {
        assert!(SignalPipe::install(&[fault]).is_err(), "signal {fault}");
    }
    for bad in [
        &[0][..],
        &[64],
        &[-1],
        &[libc::SIGKILL],
        &[libc::SIGSTOP],
        &[libc::SIGUSR1, libc::SIGUSR1],
    ] {
        assert_eq!(
            SignalPipe::install(bad).err().unwrap().kind(),
            std::io::ErrorKind::InvalidInput,
            "{bad:?}"
        );
    }
    assert_eq!(handler_of(libc::SIGUSR1), libc::SIG_DFL);
}

#[test]
fn a_suspended_raw_mode_leaves_the_foreground_programs_settings_alone() {
    let _serial = serial();
    let p = support::pty();
    let raw = RawMode::enter(p.slave.as_fd()).unwrap();
    raw.suspend().unwrap();
    assert!(
        tcgetattr(p.slave.as_fd())
            .unwrap()
            .local_modes
            .contains(LocalModes::ICANON)
    );
    // The program now in the foreground sets its own modes.
    let mut theirs = tcgetattr(p.slave.as_fd()).unwrap();
    theirs
        .local_modes
        .remove(LocalModes::ECHO | LocalModes::ICANON);
    rustix::termios::tcsetattr(
        p.slave.as_fd(),
        rustix::termios::OptionalActions::Now,
        &theirs,
    )
    .unwrap();
    drop(raw);
    let after = tcgetattr(p.slave.as_fd()).unwrap();
    assert!(
        !after
            .local_modes
            .intersects(LocalModes::ECHO | LocalModes::ICANON),
        "drop restored xcomm's saved settings over the foreground program's"
    );
    // And after reenter, it is ours again: drop restores.
    let p2 = support::pty();
    let raw = RawMode::enter(p2.slave.as_fd()).unwrap();
    raw.suspend().unwrap();
    raw.reenter().unwrap();
    assert_raw(&tcgetattr(p2.slave.as_fd()).unwrap());
    drop(raw);
    assert!(
        tcgetattr(p2.slave.as_fd())
            .unwrap()
            .local_modes
            .contains(LocalModes::ICANON)
    );
}

/// Child half of `a_background_job_does_not_restore_over_the_foreground`.
#[cfg(target_os = "linux")]
#[test]
fn background_child() {
    if std::env::var_os("XCOMM_TEST_BG_CHILD").is_none() {
        return;
    }
    let stdin = std::io::stdin();
    let raw = RawMode::enter(stdin.as_fd()).unwrap();
    // Another process group takes the foreground, as `bg` plus a foreground
    // program would arrange (a kill -STOP cannot be caught, so xcomm never
    // gave the terminal back itself).
    let other = unsafe { libc::fork() };
    if other == 0 {
        unsafe {
            libc::setpgid(0, 0);
            libc::pause();
            libc::_exit(0);
        }
    }
    unsafe {
        libc::setpgid(other, other);
        libc::signal(libc::SIGTTOU, libc::SIG_IGN);
        assert_eq!(libc::tcsetpgrp(0, other), 0);
        // The foreground program's own settings.
        let mut theirs = tcgetattr(stdin.as_fd()).unwrap();
        theirs.local_modes.insert(LocalModes::ICANON);
        theirs.local_modes.remove(LocalModes::ECHO);
        rustix::termios::tcsetattr(
            stdin.as_fd(),
            rustix::termios::OptionalActions::Now,
            &theirs,
        )
        .unwrap();
        libc::signal(libc::SIGTTOU, libc::SIG_DFL);
    }
    drop(raw);
    unsafe { libc::kill(other, libc::SIGKILL) };
    println!("XCOMM-BG-CHILD-OK");
}

#[cfg(target_os = "linux")]
#[test]
fn a_background_job_does_not_restore_over_the_foreground() {
    use std::os::unix::process::CommandExt;
    let p = support::pty();
    let slave = p.slave.try_clone().unwrap();
    let mut cmd = Command::new(std::env::current_exe().unwrap());
    cmd.args([
        "--exact",
        "background_child",
        "--nocapture",
        "--test-threads=1",
    ])
    .env("XCOMM_TEST_BG_CHILD", "1")
    .stdin(Stdio::from(slave))
    .stderr(Stdio::null());
    // SAFETY: setsid and ioctl are async-signal-safe.
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() < 0 || libc::ioctl(0, libc::TIOCSCTTY as _, 0) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let out = cmd.output().unwrap();
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("XCOMM-BG-CHILD-OK"),
        "{}",
        String::from_utf8_lossy(&out.stdout)
    );
    let after = tcgetattr(p.slave.as_fd()).unwrap();
    assert!(
        after.local_modes.contains(LocalModes::ICANON)
            && !after.local_modes.contains(LocalModes::ECHO),
        "the background job restored its saved settings over the foreground program's: {:?}",
        after.local_modes
    );
}
