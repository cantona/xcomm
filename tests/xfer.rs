//! File transfer through lrzsz, on a spawned xcomm. Skipped, saying so,
//! where lrzsz is not installed.

mod support;

use std::os::fd::AsFd;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use rustix::event::{PollFd, PollFlags, poll};
use support::remote::{Remote, lrzsz, need};
use support::{lock_dir, read_until, spawn_in_pty, spawn_with_env, write_all};

const T: Duration = Duration::from_secs(30);

/// xcomm on a fake device, started in `cwd`.
fn start_in(cwd: &Path, env: &[(&str, &str)]) -> (support::Pty, support::Spawned) {
    let dev = support::pty();
    let path = dev.slave_path.to_str().unwrap().to_string();
    let s = if env.is_empty() {
        let script = format!(
            "cd '{}' && exec '{}' '{path}'",
            cwd.display(),
            env!("CARGO_BIN_EXE_xcomm")
        );
        spawn_in_pty("/bin/sh", &["-c", &script], &lock_dir("xfer"))
    } else {
        spawn_with_env(
            env!("CARGO_BIN_EXE_xcomm"),
            &[&path],
            &lock_dir("xfer"),
            env,
        )
    };
    read_until(s.host.master.as_fd(), b"for help", T);
    (dev, s)
}

/// Types `keys`, then waits for `until` on the terminal.
fn type_then(s: &support::Spawned, keys: &[u8], until: &[u8]) -> String {
    write_all(s.host.master.as_fd(), keys);
    String::from_utf8_lossy(&read_until(s.host.master.as_fd(), until, T)).into_owned()
}

fn scratch(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("xcomm-xfer-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// Bytes that no simple pattern compresses or repeats.
fn data(len: usize) -> Vec<u8> {
    let mut x: u64 = 0x9e37_79b9_7f4a_7c15;
    (0..len)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x as u8
        })
        .collect()
}

/// The line saying how the transfer ended, once it is on the terminal.
fn outcome(s: &support::Spawned) -> String {
    outcome_after(s, String::new())
}

/// Like outcome, with `out` read already.
fn outcome_after(s: &support::Spawned, mut out: String) -> String {
    loop {
        // What was read already may hold the whole line.
        for end in ["transfer done", "transfer failed", "transfer aborted"] {
            if let Some(line) = out.find(end).and_then(|i| out[i..].split_once('\n')) {
                return line.0.trim_end().to_string();
            }
        }
        out += &String::from_utf8_lossy(&read_until(s.host.master.as_fd(), b"\n", T));
    }
}

/// Starts a send of `file` with `proto` and waits for the transfer to
/// start. Gives what was read from there on: a quick program's result can
/// come in the same read.
fn send(s: &support::Spawned, proto: &str, file: &Path) -> String {
    type_then(s, b"\x01s", b"[zmodem]: ");
    type_then(s, format!("{proto}\r").as_bytes(), b"file to send: ");
    let text = type_then(
        s,
        format!("{}\r", file.display()).as_bytes(),
        b"transfer started",
    );
    text[text.find("transfer started").unwrap()..].to_string()
}

#[test]
fn zmodem_send_roundtrip() {
    let Some(p) = need(&["rz"]) else { return };
    let (here, there) = (scratch("zs-here"), scratch("zs-there"));
    let file = here.join("one.bin");
    let sent = data(1 << 20);
    std::fs::write(&file, &sent).unwrap();
    let (dev, s) = start_in(&here, &[]);
    let remote = Remote::start(&p[0], &["-y"], &there, &dev.master);
    let seen = send(&s, "zmodem", &file);
    assert_eq!(
        outcome_after(&s, seen),
        format!("transfer done: {}", lrzsz("sz").unwrap())
    );
    remote.received(&there.join("one.bin"), &sent);
    drop(s);
    let _ = std::fs::remove_dir_all(&here);
    let _ = std::fs::remove_dir_all(&there);
}

#[test]
fn zmodem_receive_roundtrip() {
    let Some(p) = need(&["sz"]) else { return };
    let (here, there) = (scratch("zr-here"), scratch("zr-there"));
    let sent = data(300_000);
    std::fs::write(there.join("two.bin"), &sent).unwrap();
    let (dev, s) = start_in(&here, &[]);
    type_then(&s, b"\x01r", b"[zmodem]: ");
    type_then(&s, b"zmodem\r", b"transfer started");
    let remote = Remote::start(&p[0], &["two.bin"], &there, &dev.master);
    assert_eq!(
        outcome(&s),
        format!("transfer done: {}", lrzsz("rz").unwrap())
    );
    assert!(remote.wait().success());
    assert_eq!(std::fs::read(here.join("two.bin")).unwrap(), sent);
    drop(s);
    let _ = std::fs::remove_dir_all(&here);
    let _ = std::fs::remove_dir_all(&there);
}

#[test]
fn xmodem_send_roundtrip() {
    let Some(p) = need(&["rx", "sx"]) else { return };
    let (here, there) = (scratch("xs-here"), scratch("xs-there"));
    let file = here.join("three.bin");
    // Whole 128-byte blocks: XMODEM pads the last one.
    let sent = data(1024 * 128);
    std::fs::write(&file, &sent).unwrap();
    let (dev, s) = start_in(&here, &[]);
    let remote = Remote::start(&p[0], &["got.bin"], &there, &dev.master);
    let seen = send(&s, "xmodem", &file);
    assert_eq!(outcome_after(&s, seen), format!("transfer done: {}", p[1]));
    assert!(remote.wait().success());
    assert_eq!(std::fs::read(there.join("got.bin")).unwrap(), sent);
    drop(s);
    let _ = std::fs::remove_dir_all(&here);
    let _ = std::fs::remove_dir_all(&there);
}

#[test]
fn missing_program_is_reported() {
    let empty = scratch("nopath");
    let (_dev, s) = start_in(&empty, &[("PATH", empty.to_str().unwrap())]);
    type_then(&s, b"\x01s", b"[zmodem]: ");
    type_then(&s, b"\r", b"file to send: ");
    write_all(s.host.master.as_fd(), b"x\r");
    assert_eq!(outcome(&s), "transfer failed: sz not found");
    drop(s);
    let _ = std::fs::remove_dir_all(&empty);
}

#[test]
fn a_failing_program_is_reported_with_its_status() {
    if need(&["sz"]).is_none() {
        return;
    }
    let here = scratch("fail");
    let (_dev, s) = start_in(&here, &[]);
    let seen = send(&s, "", &here.join("no-such-file"));
    let out = outcome_after(&s, seen);
    assert!(
        out.starts_with("transfer failed: ") && out.contains(" exited with status "),
        "{out}"
    );
    drop(s);
    let _ = std::fs::remove_dir_all(&here);
}

/// Aborts a send to a device that never answers with `keys`; the session
/// goes on forwarding both ways.
fn abort_with(keys: &[u8], tag: &str) {
    if need(&["sz"]).is_none() {
        return;
    }
    let here = scratch(tag);
    let file = here.join("four.bin");
    std::fs::write(&file, data(4096)).unwrap();
    let (dev, s) = start_in(&here, &[]);
    let seen = send(&s, "zmodem", &file);
    // lrzsz's progress lines end in LF: the terminal adds the CR meanwhile.
    let opost = |s: &support::Spawned| {
        rustix::termios::tcgetattr(&s.host.master)
            .unwrap()
            .output_modes
            .contains(rustix::termios::OutputModes::OPOST)
    };
    assert!(opost(&s));
    let started = std::time::Instant::now();
    write_all(s.host.master.as_fd(), keys);
    assert_eq!(outcome_after(&s, seen), "transfer aborted");
    assert!(!opost(&s));
    // SIGTERM did it, not the SIGKILL that follows 2 s later.
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "{:?}",
        started.elapsed()
    );
    write_all(dev.master.as_fd(), b"after-abort");
    read_until(s.host.master.as_fd(), b"after-abort", T);
    write_all(s.host.master.as_fd(), b"keys!");
    read_until(dev.master.as_fd(), b"keys!", T);
    drop(s);
    let _ = std::fs::remove_dir_all(&here);
}

#[test]
fn prefix_x_aborts_the_transfer() {
    abort_with(b"\x01x", "abort-x");
}

#[test]
fn ctrl_c_aborts_the_transfer() {
    abort_with(b"\x03", "abort-c");
}

#[test]
fn a_coded_ctrl_c_aborts_the_transfer() {
    abort_with(b"\x1b[99;5u", "abort-csi-u");
    abort_with(b"\x1b[27;5;99~", "abort-mok");
}

#[test]
fn a_file_name_starting_with_a_dash_is_a_file() {
    let Some(p) = need(&["rz"]) else { return };
    let (here, there) = (scratch("dash-here"), scratch("dash-there"));
    let sent = data(5000);
    std::fs::write(here.join("-y"), &sent).unwrap();
    let (dev, s) = start_in(&here, &[]);
    let remote = Remote::start(&p[0], &["-y"], &there, &dev.master);
    let seen = send(&s, "zmodem", Path::new("-y"));
    assert_eq!(
        outcome_after(&s, seen),
        format!("transfer done: {}", lrzsz("sz").unwrap())
    );
    remote.received(&there.join("-y"), &sent);
    drop(s);
    let _ = std::fs::remove_dir_all(&here);
    let _ = std::fs::remove_dir_all(&there);
}

/// Linux only: macOS has no way to have the program told.
#[cfg(target_os = "linux")]
#[test]
fn the_program_stops_when_xcomm_is_killed() {
    if need(&["sz"]).is_none() {
        return;
    }
    let here = scratch("pdeath");
    let file = here.join("six.bin");
    std::fs::write(&file, data(4096)).unwrap();
    let (_dev, mut s) = start_in(&here, &[]);
    send(&s, "zmodem", &file);
    let program = child_of(s.child.id());
    unsafe { libc::kill(s.child.id() as i32, libc::SIGKILL) };
    s.wait_exit(T);
    gone(program);
    drop(s);
    let _ = std::fs::remove_dir_all(&here);
}

/// The one child of process `pid`.
fn child_of(pid: u32) -> i32 {
    let out = Command::new("pgrep")
        .args(["-P", &pid.to_string()])
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout)
        .trim()
        .parse()
        .expect("one transfer program")
}

/// Waits for process `pid` to be gone.
fn gone(pid: i32) {
    let started = std::time::Instant::now();
    while unsafe { libc::kill(pid, 0) } == 0 {
        assert!(started.elapsed() < T, "process {pid} is still running");
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// A transfer program that ignores SIGTERM is killed, and the port gets
/// back the modes it had changed.
#[test]
fn a_program_ignoring_sigterm_is_killed_and_the_port_restored() {
    let here = scratch("stubborn");
    let bin = here.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let fake = bin.join("sz");
    // stty works on its stdin, which is the port.
    std::fs::write(&fake, "#!/bin/sh\ntrap '' TERM\nstty 1200\nexec sleep 30\n").unwrap();
    let mut perm = std::fs::metadata(&fake).unwrap().permissions();
    std::os::unix::fs::PermissionsExt::set_mode(&mut perm, 0o755);
    std::fs::set_permissions(&fake, perm).unwrap();
    let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap());
    let (dev, s) = start_in(&here, &[("PATH", &path)]);
    let before = rustix::termios::tcgetattr(&dev.slave)
        .unwrap()
        .output_speed();
    let seen = send(&s, "zmodem", &here.join("whatever"));
    // Until the fake has changed the speed.
    let started = std::time::Instant::now();
    while rustix::termios::tcgetattr(&dev.slave)
        .unwrap()
        .output_speed()
        == before
    {
        assert!(started.elapsed() < T, "the fake never ran stty");
        std::thread::sleep(Duration::from_millis(10));
    }
    write_all(s.host.master.as_fd(), b"\x01x");
    assert_eq!(outcome_after(&s, seen), "transfer aborted");
    assert_eq!(
        rustix::termios::tcgetattr(&dev.slave)
            .unwrap()
            .output_speed(),
        before
    );
    drop(s);
    let _ = std::fs::remove_dir_all(&here);
}

/// SIGTERM while a transfer runs: the program is stopped and the session
/// ends.
#[test]
fn sigterm_during_a_transfer_stops_the_program_and_ends_the_session() {
    if need(&["sz"]).is_none() {
        return;
    }
    let here = scratch("sigterm");
    let file = here.join("five.bin");
    std::fs::write(&file, data(4096)).unwrap();
    let (_dev, mut s) = start_in(&here, &[]);
    send(&s, "zmodem", &file);
    let program = child_of(s.child.id());
    unsafe { libc::kill(s.child.id() as i32, libc::SIGTERM) };
    assert!(s.wait_exit(T).success());
    gone(program);
    drop(s);
    let _ = std::fs::remove_dir_all(&here);
}

/// Enter sent as CR LF answers each prompt once: the LF is not an empty
/// answer to the next one.
#[test]
fn cr_lf_enter_answers_each_prompt_once() {
    let empty = scratch("crlf");
    let (_dev, s) = start_in(&empty, &[("PATH", empty.to_str().unwrap())]);
    write_all(s.host.master.as_fd(), b"\x01s\r\nx\r\n");
    assert_eq!(outcome(&s), "transfer failed: sz not found");
    drop(s);
    let _ = std::fs::remove_dir_all(&empty);
}

/// With Ctrl-C as the prefix, the hint names only the prefix and x, which
/// is what aborts.
#[test]
fn with_a_ctrl_c_prefix_the_hint_names_the_prefix_only() {
    if need(&["sz"]).is_none() {
        return;
    }
    let here = scratch("ctrlc-prefix");
    let file = here.join("seven.bin");
    std::fs::write(&file, data(4096)).unwrap();
    let dev = support::pty();
    let path = dev.slave_path.to_str().unwrap().to_string();
    let s = spawn_with_env(
        env!("CARGO_BIN_EXE_xcomm"),
        &["--prefix", "ctrl-c", &path],
        &lock_dir("xfer"),
        &[],
    );
    read_until(s.host.master.as_fd(), b"for help", T);
    type_then(&s, b"\x03s", b"[zmodem]: ");
    type_then(&s, b"\r", b"file to send: ");
    let out = type_then(&s, format!("{}\r", file.display()).as_bytes(), b"aborts)");
    assert!(out.contains("(Ctrl-C x aborts)"), "{out}");
    write_all(s.host.master.as_fd(), b"\x03x");
    assert_eq!(outcome(&s), "transfer aborted");
    drop(s);
    let _ = std::fs::remove_dir_all(&here);
}

/// The ZMODEM receiver writes only below xcomm's working directory: the
/// sender names the files, and lrzsz takes absolute names.
#[test]
fn the_zmodem_receiver_cannot_write_outside_the_working_directory() {
    let here = scratch("jail-here");
    let outside = scratch("jail-outside");
    let bin = scratch("jail-bin");
    let fake = bin.join("rz");
    // Into another terminal, and (Linux) an existing file on /dev/shm: both
    // below /dev, and neither is the working directory.
    let victim = support::pty();
    #[cfg(target_os = "linux")]
    let shm = {
        let shm = PathBuf::from(format!("/dev/shm/xcomm-jail-{}", std::process::id()));
        std::fs::write(&shm, b"orig\n").unwrap();
        shm
    };
    #[cfg(not(target_os = "linux"))]
    let shm = outside.join("no-shm");
    std::fs::write(
        &fake,
        format!(
            "#!/bin/sh\necho in > inside.txt\necho out > '{}/outside.txt'\necho injected >> '{}'\necho injected >> '{}'\necho null > /dev/null || exit 9\nexit 0\n",
            outside.display(),
            victim.slave_path.display(),
            shm.display(),
        ),
    )
    .unwrap();
    let mut perm = std::fs::metadata(&fake).unwrap().permissions();
    std::os::unix::fs::PermissionsExt::set_mode(&mut perm, 0o755);
    std::fs::set_permissions(&fake, perm).unwrap();
    let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap());
    let script = format!(
        "cd '{}' && PATH='{path}' exec '{}' '{}'",
        here.display(),
        env!("CARGO_BIN_EXE_xcomm"),
        "DEVICE"
    );
    let dev = support::pty();
    let script = script.replace("DEVICE", dev.slave_path.to_str().unwrap());
    let s = spawn_in_pty("/bin/sh", &["-c", &script], &lock_dir("xfer"));
    read_until(s.host.master.as_fd(), b"for help", T);
    type_then(&s, b"\x01r", b"[zmodem]: ");
    write_all(s.host.master.as_fd(), b"zmodem\r");
    assert_eq!(outcome(&s), "transfer done: rz");
    assert_eq!(std::fs::read(here.join("inside.txt")).unwrap(), b"in\n");
    assert!(!outside.join("outside.txt").exists());
    let mut fds = [PollFd::new(&victim.master, PollFlags::IN)];
    let wait = rustix::event::Timespec {
        tv_sec: 0,
        tv_nsec: 200_000_000,
    };
    assert_eq!(
        poll(&mut fds, Some(&wait)).unwrap(),
        0,
        "another terminal was written"
    );
    #[cfg(target_os = "linux")]
    {
        assert_eq!(std::fs::read(&shm).unwrap(), b"orig\n");
        let _ = std::fs::remove_file(&shm);
    }
    drop(s);
    drop(dev);
    for d in [&here, &outside, &bin] {
        let _ = std::fs::remove_dir_all(d);
    }
}

/// Names starting with a dot are refused even below the working directory:
/// started from $HOME, a device could otherwise plant a .profile.
#[test]
fn a_received_dot_file_is_refused() {
    let Some(p) = need(&["sz"]) else { return };
    let (here, there) = (scratch("dot-here"), scratch("dot-there"));
    std::fs::write(there.join(".profile"), b"evil\n").unwrap();
    let (dev, s) = start_in(&here, &[]);
    type_then(&s, b"\x01r", b"[zmodem]: ");
    type_then(&s, b"zmodem\r", b"transfer started");
    let _remote = Remote::start(&p[0], &["--full-path", ".profile"], &there, &dev.master);
    let out = outcome(&s);
    assert!(out.starts_with("transfer failed: "), "{out}");
    assert!(!here.join(".profile").exists());
    drop(s);
    let _ = std::fs::remove_dir_all(&here);
    let _ = std::fs::remove_dir_all(&there);
}

/// The end of a transfer is noticed even with SIGCHLD blocked by the
/// parent (the signal mask survives exec).
#[test]
fn a_blocked_sigchld_does_not_hide_the_end_of_a_transfer() {
    if need(&["sz"]).is_none() {
        return;
    }
    let here = scratch("blocked-chld");
    let dev = support::pty();
    let script = format!(
        "cd '{}' && exec perl -e 'use POSIX; sigprocmask(SIG_BLOCK, POSIX::SigSet->new(SIGCHLD)) or die; exec @ARGV or die' '{}' '{}'",
        here.display(),
        env!("CARGO_BIN_EXE_xcomm"),
        dev.slave_path.display()
    );
    let s = spawn_in_pty("/bin/sh", &["-c", &script], &lock_dir("xfer"));
    read_until(s.host.master.as_fd(), b"for help", T);
    let seen = send(&s, "", &here.join("no-such-file"));
    let out = outcome_after(&s, seen);
    assert!(out.starts_with("transfer failed: "), "{out}");
    drop(s);
    let _ = std::fs::remove_dir_all(&here);
}

fn ctl(s: &support::Spawned, args: &[&str]) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_xcomm"));
    cmd.arg("ctl")
        .args(args)
        .env("XDG_RUNTIME_DIR", &s.lock_dir)
        .stdin(Stdio::null());
    cmd
}

#[test]
fn ctl_xfer_send_roundtrip() {
    let Some(p) = need(&["rz"]) else { return };
    let (here, there) = (scratch("cs-here"), scratch("cs-there"));
    let sent = data(256 << 10);
    std::fs::write(here.join("two.bin"), &sent).unwrap();
    let (dev, s) = start_in(&there, &[]);
    let path = dev.slave_path.to_str().unwrap().to_string();
    let remote = Remote::start(&p[0], &["-y"], &there, &dev.master);
    // The terminal is read meanwhile: sz writes its progress there, and a
    // macOS pty holds about 1 KiB.
    let stop = Arc::new(AtomicBool::new(false));
    let reader = {
        let master = s.host.master.try_clone().unwrap();
        let fl = rustix::fs::fcntl_getfl(&master).unwrap();
        rustix::fs::fcntl_setfl(&master, fl | rustix::fs::OFlags::NONBLOCK).unwrap();
        let stop = stop.clone();
        std::thread::spawn(move || {
            let mut buf = [0u8; 4096];
            while !stop.load(Ordering::Relaxed) {
                if rustix::io::read(&master, &mut buf).is_err() {
                    std::thread::sleep(Duration::from_millis(5));
                }
            }
        })
    };
    // A relative name is the client's: from `here`, not the session's cwd.
    let o = ctl(&s, &[&path, "xfer", "send", "zmodem", "two.bin"])
        .current_dir(&here)
        .output()
        .unwrap();
    stop.store(true, Ordering::Relaxed);
    reader.join().unwrap();
    assert_eq!(
        o.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&o.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&o.stdout),
        format!("transfer done: {}\n", lrzsz("sz").unwrap())
    );
    remote.received(&there.join("two.bin"), &sent);
    drop(s);
    let _ = std::fs::remove_dir_all(&here);
    let _ = std::fs::remove_dir_all(&there);
}

#[test]
fn closing_the_client_aborts_its_transfer() {
    if need(&["rz"]).is_none() {
        return;
    }
    let here = scratch("ca-here");
    let (dev, s) = start_in(&here, &[]);
    let path = dev.slave_path.to_str().unwrap().to_string();
    // No sender: rz waits until it is aborted.
    let mut client = ctl(&s, &[&path, "xfer", "recv", "zmodem"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    read_until(
        s.host.master.as_fd(),
        b"transfer started for a control client",
        T,
    );
    // The port is the program's meanwhile.
    for args in [
        &[path.as_str(), "break"][..],
        &[&path, "send", "x"],
        &[&path, "run", "x", "--until", "y"],
    ] {
        let o = ctl(&s, args).output().unwrap();
        assert_eq!(o.status.code(), Some(5), "{args:?}");
    }
    client.kill().unwrap();
    client.wait().unwrap();
    assert_eq!(outcome(&s), "transfer aborted");
    let o = ctl(&s, &["--json", &path, "status"]).output().unwrap();
    let v: serde_json::Value = serde_json::from_slice(&o.stdout).unwrap();
    assert_eq!(v["transfer"], false);
    drop(dev);
    let _ = std::fs::remove_dir_all(&here);
}

#[test]
fn a_headless_transfer_names_no_keys() {
    if need(&["rz"]).is_none() {
        return;
    }
    let dev = support::pty();
    let path = dev.slave_path.to_str().unwrap().to_string();
    let s = support::spawn_xcomm(&["--headless", &path], &lock_dir("xfer"));
    read_until(s.host.master.as_fd(), b"headless session on", T);
    let mut client = ctl(&s, &[&path, "xfer", "recv", "zmodem"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    read_until(
        s.host.master.as_fd(),
        b"transfer started for a control client: rz (closing the client aborts it)",
        T,
    );
    client.kill().unwrap();
    client.wait().unwrap();
    assert_eq!(outcome(&s), "transfer aborted");
}
