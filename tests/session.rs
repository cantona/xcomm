mod support;

use std::os::fd::AsFd;
use std::time::Duration;

use rustix::termios::{LocalModes, tcgetattr};
use support::{lock_dir, read_until, spawn_xcomm, wait_exit, write_all};

const T: Duration = Duration::from_secs(5);

fn start(extra: &[&str]) -> (support::Pty, support::Spawned) {
    let dev = support::pty();
    let path = dev.slave_path.to_str().unwrap().to_string();
    let mut args: Vec<&str> = extra.to_vec();
    args.push(&path);
    let s = spawn_xcomm(&args, &lock_dir("session"));
    let banner = read_until(s.host.master.as_fd(), b"\r\n", T);
    assert!(
        String::from_utf8_lossy(&banner).contains(&path),
        "{}",
        String::from_utf8_lossy(&banner)
    );
    (dev, s)
}

#[test]
fn device_bytes_reach_the_terminal_unchanged() {
    let (dev, s) = start(&[]);
    let all: Vec<u8> = (0u8..=255).collect();
    write_all(dev.master.as_fd(), &all);
    assert_eq!(read_until(s.host.master.as_fd(), &[255], T), all);
}

#[test]
fn keyboard_bytes_reach_the_device_unchanged() {
    let (dev, s) = start(&[]);
    // All but the prefix, Ctrl-A (tests/session_keys.rs).
    let all: Vec<u8> = (0u8..=255).filter(|&b| b != 0x01).collect();
    write_all(s.host.master.as_fd(), &all);
    assert_eq!(read_until(dev.master.as_fd(), &[255], T), all);
}

#[test]
fn sigterm_exits_cleanly_and_restores_the_terminal() {
    let (_dev, mut s) = start(&[]);
    assert!(
        !tcgetattr(s.host.master.as_fd())
            .unwrap()
            .local_modes
            .contains(LocalModes::ICANON)
    );
    unsafe { libc::kill(s.child.id() as i32, libc::SIGTERM) };
    assert!(s.wait_exit(T).success());
    assert!(
        tcgetattr(s.host.master.as_fd())
            .unwrap()
            .local_modes
            .contains(LocalModes::ICANON)
    );
}

#[test]
fn terminal_hangup_ends_the_session() {
    let (_dev, mut s) = start(&[]);
    let support::Pty { master, slave, .. } = std::mem::replace(&mut s.host, support::pty());
    drop(master);
    drop(slave);
    assert!(s.wait_exit(T).success());
}

#[test]
fn unplugged_device_ends_the_session_with_status_1() {
    let (dev, mut s) = start(&[]);
    drop(dev);
    let status = s.wait_exit(T);
    assert_eq!(status.code(), Some(1));
    let out = s.read_after_exit(b"disconnected", T);
    assert!(String::from_utf8_lossy(&out).contains("disconnected"));
}

#[test]
fn a_locked_port_exits_2_naming_the_holder() {
    let dev = support::pty();
    let path = dev.slave_path.to_str().unwrap().to_string();
    let dir = lock_dir("conflict");
    let first = spawn_xcomm(&[&path], &dir);
    read_until(first.host.master.as_fd(), b"\r\n", T);
    let mut second = spawn_xcomm(&[&path], &dir);
    let status = second.wait_exit(T);
    assert_eq!(status.code(), Some(2));
    let msg = second.read_after_exit(b"\n", T);
    let msg = String::from_utf8_lossy(&msg);
    assert!(msg.contains(&first.child.id().to_string()), "{msg}");
    std::mem::forget(dir);
}

#[test]
fn the_banner_says_what_the_driver_applied() {
    // The Linux pty driver forces CS8, so 7 data bits come back as 8.
    let dev = support::pty();
    let path = dev.slave_path.to_str().unwrap().to_string();
    let s = spawn_xcomm(&["--databits", "7", &path], &lock_dir("banner"));
    let banner = read_until(s.host.master.as_fd(), b"\r\n", T);
    let banner = String::from_utf8_lossy(&banner);
    #[cfg(target_os = "linux")]
    assert!(
        banner.contains("requested 115200 7N1; the driver applied 115200 8N1"),
        "{banner}"
    );
    let _ = banner;
}

#[test]
fn a_large_burst_arrives_complete() {
    let (dev, s) = start(&[]);
    const SIZE: usize = 8 << 20;
    let mut x: u64 = 0x9e37_79b9_7f4a_7c15;
    let data: Vec<u8> = (0..SIZE)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x as u8
        })
        .collect();
    let expected = fnv(&data);
    let master = dev.master.try_clone().unwrap();
    let feeder = std::thread::spawn(move || write_all(master.as_fd(), &data));
    let mut got = 0usize;
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    let mut buf = vec![0u8; 65536];
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    while got < SIZE {
        assert!(std::time::Instant::now() < deadline, "only {got} bytes");
        let n = rustix::io::read(s.host.master.as_fd(), &mut buf[..]).unwrap();
        for &b in &buf[..n] {
            hash = (hash ^ b as u64).wrapping_mul(0x100_0000_01b3);
        }
        got += n;
    }
    feeder.join().unwrap();
    assert_eq!(got, SIZE);
    assert_eq!(hash, expected);
}

fn fnv(data: &[u8]) -> u64 {
    data.iter().fold(0xcbf2_9ce4_8422_2325u64, |h, &b| {
        (h ^ b as u64).wrapping_mul(0x100_0000_01b3)
    })
}

#[cfg(target_os = "linux")]
fn state(pid: u32) -> char {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap();
    stat.rsplit(") ").next().unwrap().chars().next().unwrap()
}

#[cfg(target_os = "linux")]
#[test]
fn job_control_stop_gives_the_terminal_back_and_resume_takes_it_again() {
    let (_dev, mut s) = start(&[]);
    let pid = s.child.id();
    let canon = |s: &support::Spawned| {
        tcgetattr(s.host.master.as_fd())
            .unwrap()
            .local_modes
            .contains(LocalModes::ICANON)
    };
    assert!(!canon(&s));
    unsafe { libc::kill(pid as i32, libc::SIGTSTP) };
    let deadline = std::time::Instant::now() + T;
    while state(pid) != 'T' {
        assert!(std::time::Instant::now() < deadline, "not stopped");
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(canon(&s), "terminal still raw while stopped");
    unsafe { libc::kill(pid as i32, libc::SIGCONT) };
    let deadline = std::time::Instant::now() + T;
    while canon(&s) {
        assert!(
            std::time::Instant::now() < deadline,
            "raw mode not restored on resume"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    unsafe { libc::kill(pid as i32, libc::SIGTERM) };
    assert!(s.wait_exit(T).success());
}

#[cfg(target_os = "linux")]
#[test]
fn hangup_while_stopped_ends_the_session_cleanly() {
    let (_dev, mut s) = start(&[]);
    let pid = s.child.id();
    unsafe { libc::kill(pid as i32, libc::SIGTSTP) };
    let deadline = std::time::Instant::now() + T;
    while state(pid) != 'T' {
        assert!(std::time::Instant::now() < deadline, "not stopped");
        std::thread::sleep(Duration::from_millis(10));
    }
    // Closing the terminal sends SIGHUP and SIGCONT to the session.
    let support::Pty { master, slave, .. } = std::mem::replace(&mut s.host, support::pty());
    drop(master);
    drop(slave);
    assert!(s.wait_exit(T).success());
}

#[cfg(target_os = "linux")]
#[test]
fn started_in_the_background_it_stops_instead_of_spinning() {
    // A job-control shell runs xcomm as a background job on the same
    // terminal; touching the terminal must stop it (SIGTTOU), not loop.
    let dev = support::pty();
    let path = dev.slave_path.to_str().unwrap().to_string();
    // Not `wait`: dash's wait returns when the job stops, and the shell
    // exiting would orphan the job.
    let script = format!(
        "{} {} & echo XPID=$!; while kill -0 $! 2>/dev/null; do sleep 0.1; done",
        env!("CARGO_BIN_EXE_xcomm"),
        path
    );
    let s = support::spawn_in_pty("sh", &["-m", "-c", &script], &lock_dir("bg"));
    let out = read_until(s.host.master.as_fd(), b"\n", T);
    let out = String::from_utf8_lossy(&out);
    let pid: u32 = out
        .split("XPID=")
        .nth(1)
        .and_then(|r| r.split_whitespace().next())
        .and_then(|p| p.parse().ok())
        .unwrap_or_else(|| panic!("{out}"));
    let deadline = std::time::Instant::now() + T;
    while state(pid) != 'T' {
        assert!(
            std::time::Instant::now() < deadline,
            "background xcomm not stopped: {}",
            state(pid)
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    let ticks = |pid: u32| -> u64 {
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap();
        let f: Vec<&str> = stat.rsplit(") ").next().unwrap().split(' ').collect();
        f[11].parse::<u64>().unwrap() + f[12].parse::<u64>().unwrap()
    };
    let before = ticks(pid);
    std::thread::sleep(Duration::from_millis(500));
    assert!(ticks(pid) - before < 5, "using CPU while stopped");
    unsafe { libc::kill(pid as i32, libc::SIGKILL) };
}

#[cfg(target_os = "linux")]
#[test]
fn a_background_xcomm_stopped_on_the_terminal_can_be_killed() {
    // `kill %1` sends SIGTERM then SIGCONT. The session must end and give up
    // its lock, not stop again in the same tcsetattr.
    let dev = support::pty();
    let path = dev.slave_path.to_str().unwrap().to_string();
    let dir = lock_dir("bgkill");
    let script = format!(
        "{} {} & echo XPID=$!; while kill -0 $! 2>/dev/null; do sleep 0.1; done; echo GONE",
        env!("CARGO_BIN_EXE_xcomm"),
        path
    );
    let s = support::spawn_in_pty("sh", &["-m", "-c", &script], &dir);
    let out = read_until(s.host.master.as_fd(), b"\n", T);
    let out = String::from_utf8_lossy(&out);
    let pid: i32 = out
        .split("XPID=")
        .nth(1)
        .and_then(|r| r.split_whitespace().next())
        .and_then(|p| p.parse().ok())
        .unwrap_or_else(|| panic!("{out}"));
    let deadline = std::time::Instant::now() + T;
    while state(pid as u32) != 'T' {
        assert!(std::time::Instant::now() < deadline, "not stopped");
        std::thread::sleep(Duration::from_millis(10));
    }
    unsafe {
        libc::kill(pid, libc::SIGTERM);
        libc::kill(pid, libc::SIGCONT);
    }
    read_until(s.host.master.as_fd(), b"GONE", T);
    let lock = dir.join(format!(
        "LCK..{}",
        xcomm::lock::lock_name(&dev.slave_path).unwrap()
    ));
    assert!(!lock.exists(), "lock left behind");
}

#[test]
fn hangup_while_the_device_streams_exits_cleanly() {
    for round in 0..5 {
        let (dev, mut s) = start(&[]);
        let master = dev.master.try_clone().unwrap();
        // Non-blocking: once xcomm is gone nobody drains the pty, and a
        // blocking write would never return.
        let fl = rustix::fs::fcntl_getfl(master.as_fd()).unwrap();
        rustix::fs::fcntl_setfl(master.as_fd(), fl | rustix::fs::OFlags::NONBLOCK).unwrap();
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let feeding = stop.clone();
        let feeder = std::thread::spawn(move || {
            while !feeding.load(std::sync::atomic::Ordering::Relaxed) {
                match rustix::io::write(master.as_fd(), &[0x43u8; 4096]) {
                    Ok(_) => {}
                    Err(rustix::io::Errno::AGAIN) => std::thread::sleep(Duration::from_millis(1)),
                    Err(_) => break,
                }
            }
        });
        std::thread::sleep(Duration::from_millis(100));
        let support::Pty { master, slave, .. } = std::mem::replace(&mut s.host, support::pty());
        drop(master);
        drop(slave);
        let status = s.wait_exit(T);
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        drop(dev);
        feeder.join().unwrap();
        assert!(status.success(), "round {round}: {status:?}");
    }
}

#[test]
fn a_write_only_stdin_means_no_input_not_an_error() {
    // nohup reopens stdin as /dev/null opened write-only.
    let dev = support::pty();
    let dir = lock_dir("nohup");
    let null = std::fs::OpenOptions::new()
        .write(true)
        .open("/dev/null")
        .unwrap();
    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_xcomm"))
        .arg(&dev.slave_path)
        .env("XCOMM_LOCK_DIR", &dir)
        .env("XDG_RUNTIME_DIR", &dir)
        .stdin(null)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    std::thread::sleep(Duration::from_millis(500));
    assert!(
        child.try_wait().unwrap().is_none(),
        "session ended without input"
    );
    unsafe { libc::kill(child.id() as i32, libc::SIGTERM) };
    let status = wait_exit(&mut child, T);
    let _ = std::fs::remove_dir_all(&dir);
    assert!(status.success(), "{status:?}");
}

#[test]
fn sigint_while_typing_floods_the_device_exits_cleanly() {
    // A signal during the device write gives EINTR there; that is not an
    // error.
    for round in 0..20 {
        let (dev, mut s) = start(&[]);
        let host = s.host.master.try_clone().unwrap();
        let fl = rustix::fs::fcntl_getfl(host.as_fd()).unwrap();
        rustix::fs::fcntl_setfl(host.as_fd(), fl | rustix::fs::OFlags::NONBLOCK).unwrap();
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let typing = stop.clone();
        let typist = std::thread::spawn(move || {
            while !typing.load(std::sync::atomic::Ordering::Relaxed) {
                if rustix::io::write(host.as_fd(), &[b'y'; 1024]).is_err() {
                    std::thread::sleep(Duration::from_millis(1));
                }
            }
        });
        let devm = dev.master.try_clone().unwrap();
        let fl = rustix::fs::fcntl_getfl(devm.as_fd()).unwrap();
        rustix::fs::fcntl_setfl(devm.as_fd(), fl | rustix::fs::OFlags::NONBLOCK).unwrap();
        let draining = stop.clone();
        let drain = std::thread::spawn(move || {
            let mut b = [0u8; 65536];
            while !draining.load(std::sync::atomic::Ordering::Relaxed) {
                if rustix::io::read(devm.as_fd(), &mut b).is_err() {
                    std::thread::sleep(Duration::from_millis(1));
                }
            }
        });
        std::thread::sleep(Duration::from_millis(20));
        unsafe { libc::kill(s.child.id() as i32, libc::SIGINT) };
        let status = s.wait_exit(T);
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        typist.join().unwrap();
        drain.join().unwrap();
        assert!(status.success(), "round {round}: {status:?}");
    }
}

#[test]
fn piped_input_reaches_the_device_complete_before_exit() {
    // `cat file | xcomm dev`: the tail still in the kernel when the queue
    // empties must not be discarded at exit.
    for round in 0..10 {
        let dev = support::pty();
        let dir = lock_dir("tail");
        let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_xcomm"))
            .arg(&dev.slave_path)
            .env("XCOMM_LOCK_DIR", &dir)
            .env("XDG_RUNTIME_DIR", &dir)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let data: Vec<u8> = (0..4000u32).map(|i| b'a' + (i % 26) as u8).collect();
        let mut stdin = child.stdin.take().unwrap();
        std::io::Write::write_all(&mut stdin, &data).unwrap();
        drop(stdin);
        // A slow device: it starts reading only after xcomm would long have
        // exited and discarded what the kernel still held.
        std::thread::sleep(Duration::from_millis(600));
        let mut got = Vec::new();
        let mut buf = [0u8; 4096];
        let deadline = std::time::Instant::now() + T;
        while got.len() < data.len() && std::time::Instant::now() < deadline {
            let mut fds = [rustix::event::PollFd::new(
                &dev.master,
                rustix::event::PollFlags::IN,
            )];
            let ts = rustix::event::Timespec::try_from(Duration::from_millis(100)).unwrap();
            if rustix::event::poll(&mut fds, Some(&ts)).unwrap() > 0 {
                match rustix::io::read(dev.master.as_fd(), &mut buf) {
                    Ok(n) => got.extend_from_slice(&buf[..n]),
                    Err(_) => break,
                }
            }
        }
        let status = wait_exit(&mut child, T);
        let _ = std::fs::remove_dir_all(&dir);
        assert!(status.success(), "round {round}: {status:?}");
        assert_eq!(got.len(), data.len(), "round {round}: tail lost");
        assert_eq!(got, data);
    }
}

#[test]
fn an_inherited_ignored_sighup_stays_ignored() {
    // nohup: a hangup must not end the session.
    use std::os::unix::process::CommandExt;
    let dev = support::pty();
    let dir = lock_dir("nohup-hup");
    let mut cmd = std::process::Command::new(env!("CARGO_BIN_EXE_xcomm"));
    // As nohup leaves it: /dev/null write-only, so there is no input (a
    // readable /dev/null would end the session at once with end of input).
    let null = std::fs::OpenOptions::new()
        .write(true)
        .open("/dev/null")
        .unwrap();
    cmd.arg(&dev.slave_path)
        .env("XCOMM_LOCK_DIR", &dir)
        .env("XDG_RUNTIME_DIR", &dir)
        .stdin(null)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    // SAFETY: signal(2) is async-signal-safe.
    unsafe {
        cmd.pre_exec(|| {
            libc::signal(libc::SIGHUP, libc::SIG_IGN);
            Ok(())
        });
    }
    let mut child = cmd.spawn().unwrap();
    std::thread::sleep(Duration::from_millis(300));
    unsafe { libc::kill(child.id() as i32, libc::SIGHUP) };
    std::thread::sleep(Duration::from_millis(300));
    let alive = child.try_wait().unwrap().is_none();
    unsafe { libc::kill(child.id() as i32, libc::SIGTERM) };
    let status = wait_exit(&mut child, T);
    let _ = std::fs::remove_dir_all(&dir);
    assert!(alive, "SIGHUP ended the session although it was ignored");
    assert!(status.success());
}

/// Stopped inside a write to a terminal that takes nothing (output stopped,
/// as by Ctrl-S), then `kill -TERM` and `kill -CONT`: the SIGTERM caught as
/// the process continues ends the session.
#[cfg(target_os = "linux")]
#[test]
fn sigterm_sent_while_stopped_in_a_blocked_write_ends_the_session() {
    let (dev, mut s) = start(&[]);
    rustix::termios::tcflow(&s.host.slave, rustix::termios::Action::OOff).unwrap();
    write_all(dev.master.as_fd(), b"more");
    let pid = s.child.id() as i32;
    support::wait_blocked_writing_stdout(pid, T);
    unsafe { libc::kill(pid, libc::SIGTSTP) };
    support::wait_stopped(pid, T);
    unsafe { libc::kill(pid, libc::SIGTERM) };
    unsafe { libc::kill(pid, libc::SIGCONT) };
    assert!(s.wait_exit(T).success());
}

const COLORED: &[u8] = b"\x1b[1;31mred\x1b[0m \x1b[2J\x1b[10;5Hend";
const UNCOLORED: &[u8] = b"\x1b[1mred\x1b[0m \x1b[2J\x1b[10;5Hend";

fn device_output_with(extra: &[&str], env: &[(&str, &str)]) -> Vec<u8> {
    let dev = support::pty();
    let path = dev.slave_path.to_str().unwrap().to_string();
    let mut args: Vec<&str> = extra.to_vec();
    args.push(&path);
    let s = support::spawn_with_env(env!("CARGO_BIN_EXE_xcomm"), &args, &lock_dir("color"), env);
    read_until(s.host.master.as_fd(), b"\r\n", T);
    write_all(dev.master.as_fd(), COLORED);
    read_until(s.host.master.as_fd(), b"end", T)
}

#[test]
fn color_never_strips_sgr_from_device_output() {
    for flag in [&["-c", "off"][..], &["--color", "never"]] {
        assert_eq!(device_output_with(flag, &[]), UNCOLORED, "{flag:?}");
    }
}

#[test]
fn color_auto_on_a_terminal_passes_device_output_untouched() {
    let dev = support::pty();
    let path = dev.slave_path.to_str().unwrap().to_string();
    let s = support::spawn_with_env(
        env!("CARGO_BIN_EXE_xcomm"),
        &[&path],
        &lock_dir("color"),
        &[],
    );
    read_until(s.host.master.as_fd(), b"\r\n", T);
    write_all(dev.master.as_fd(), COLORED);
    assert_eq!(read_until(s.host.master.as_fd(), b"end", T), COLORED);
}

#[test]
fn color_auto_follows_term_but_not_no_color() {
    // NO_COLOR is about a program's own color; device output is the board's.
    assert_eq!(device_output_with(&[], &[("NO_COLOR", "1")]), COLORED);
    assert_eq!(device_output_with(&[], &[("TERM", "dumb")]), UNCOLORED);
    let dev = support::pty();
    let path = dev.slave_path.to_str().unwrap().to_string();
    let s = support::spawn_with_env(
        env!("CARGO_BIN_EXE_xcomm"),
        &["--color", "always", &path],
        &lock_dir("color"),
        &[("TERM", "dumb")],
    );
    read_until(s.host.master.as_fd(), b"\r\n", T);
    write_all(dev.master.as_fd(), COLORED);
    assert_eq!(read_until(s.host.master.as_fd(), b"end", T), COLORED);
}

#[test]
fn color_auto_with_stdout_not_a_terminal_strips_sgr() {
    let dev = support::pty();
    let path = dev.slave_path.to_str().unwrap().to_string();
    let (out_r, out_w) = std::io::pipe().unwrap();
    let s = support::spawn_with(
        env!("CARGO_BIN_EXE_xcomm"),
        &[&path],
        &lock_dir("color"),
        &[],
        Some(out_w.into()),
    );
    read_until(s.host.master.as_fd(), b"\r\n", T);
    write_all(dev.master.as_fd(), COLORED);
    assert_eq!(read_until(out_r.as_fd(), b"end", T), UNCOLORED);
}
