// SPDX-FileCopyrightText: 2026 Su Kang Yin <paradyse@gmail.com>
// SPDX-License-Identifier: GPL-2.0-only

mod support;

use std::io::{BufRead, BufReader, Read};
use std::os::fd::AsFd;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use support::{lock_dir, read_until, write_all};

const T: Duration = Duration::from_secs(5);

/// A child killed and reaped when the test is done with it, passed or not.
struct Reaped(Child);

impl Drop for Reaped {
    fn drop(&mut self) {
        if self.0.try_wait().ok().flatten().is_none() {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
}

/// A headless session with no terminal at all: stdin /dev/null, stdout and
/// stderr pipes. Returns it with the socket path its startup line names.
fn headless(
    dev: &str,
    dir: &Path,
    extra: &[&str],
) -> (Reaped, PathBuf, BufReader<std::process::ChildStderr>) {
    let child = Command::new(env!("CARGO_BIN_EXE_xcomm"))
        .arg("--headless")
        .args(extra)
        .arg(dev)
        .env("XCOMM_LOCK_DIR", dir)
        .env("XDG_RUNTIME_DIR", dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut child = Reaped(child);
    let mut err = BufReader::new(child.0.stderr.take().unwrap());
    let mut line = String::new();
    let until = Instant::now() + T;
    loop {
        line.clear();
        assert!(err.read_line(&mut line).unwrap() > 0, "no startup line");
        if let Some(rest) =
            line.strip_prefix(&format!("xcomm: headless session on {dev}, socket "))
        {
            break (child, PathBuf::from(rest.trim_end()), err);
        }
        assert!(Instant::now() < until, "no startup line");
    }
}

fn ctl(dir: &Path, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_xcomm"))
        .arg("ctl")
        .args(args)
        .env("XDG_RUNTIME_DIR", dir)
        .stdin(Stdio::null())
        .output()
        .unwrap()
}

#[test]
fn a_headless_session_answers_ctl_run_and_writes_no_output() {
    let dev = support::pty();
    let path = dev.slave_path.to_str().unwrap().to_string();
    let dir = lock_dir("headless");
    let (mut child, sock, _err) = headless(&path, &dir, &[]);
    assert!(sock.starts_with(&dir), "{sock:?}");
    assert!(sock.exists());
    let master = dev.master.try_clone().unwrap();
    let device = std::thread::spawn(move || {
        let got = read_until(master.as_fd(), b"\r", T);
        assert_eq!(got, b"uname\r");
        write_all(master.as_fd(), b"uname\r\nLinux\r\n# ");
    });
    let o = ctl(&dir, &["--json", &path, "run", "uname", "--until", "# $"]);
    device.join().unwrap();
    assert_eq!(
        o.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&o.stderr)
    );
    let v: serde_json::Value = serde_json::from_slice(&o.stdout).unwrap();
    assert_eq!(v["output"], "Linux\r\n");
    rustix::process::kill_process(
        rustix::process::Pid::from_child(&child.0),
        rustix::process::Signal::TERM,
    )
    .unwrap();
    let mut out = Vec::new();
    child
        .0
        .stdout
        .take()
        .unwrap()
        .read_to_end(&mut out)
        .unwrap();
    let status = support::wait_exit(&mut child.0, T);
    assert!(status.success(), "{status:?}");
    // Device output went to the ring only.
    assert!(out.is_empty(), "{:?}", String::from_utf8_lossy(&out));
    assert!(!sock.exists(), "the socket was left behind");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_headless_session_ends_on_sigint_and_sighup_too() {
    for sig in [rustix::process::Signal::INT, rustix::process::Signal::HUP] {
        let dev = support::pty();
        let path = dev.slave_path.to_str().unwrap().to_string();
        let dir = lock_dir("headless-sig");
        let (mut child, sock, _err) = headless(&path, &dir, &[]);
        rustix::process::kill_process(rustix::process::Pid::from_child(&child.0), sig)
            .unwrap();
        let status = support::wait_exit(&mut child.0, T);
        assert!(status.code().is_some(), "{sig:?}: killed by it: {status:?}");
        assert!(!sock.exists(), "{sig:?}: the socket was left behind");
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[test]
fn headless_with_ctl_off_is_a_usage_error() {
    let o = Command::new(env!("CARGO_BIN_EXE_xcomm"))
        .args(["--headless", "--ctl", "off", "ttyNONE"])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert_eq!(o.status.code(), Some(2));
    assert!(
        String::from_utf8_lossy(&o.stderr)
            .contains("--headless needs the control socket"),
        "{}",
        String::from_utf8_lossy(&o.stderr)
    );
}

#[test]
fn a_headless_session_ignores_a_terminal_on_stdin() {
    // Started from a shell in a terminal: it must not take the terminal
    // (raw mode, reading keys).
    let dev = support::pty();
    let path = dev.slave_path.to_str().unwrap().to_string();
    let dir = lock_dir("headless-tty");
    let mut s = support::spawn_xcomm(&["--headless", &path], &dir);
    read_until(s.host.master.as_fd(), b"headless session on", T);
    let attrs = rustix::termios::tcgetattr(&s.host.slave).unwrap();
    assert!(
        attrs
            .local_modes
            .contains(rustix::termios::LocalModes::ICANON),
        "the terminal was put in raw mode"
    );
    // Typed keys are not sent: ctrl-A x is not a quit, "abc" not data.
    write_all(s.host.master.as_fd(), b"abc\x01x\n");
    write_all(dev.master.as_fd(), b"from device");
    std::thread::sleep(Duration::from_millis(300));
    let mut buf = [0u8; 64];
    rustix::fs::fcntl_setfl(&dev.master, rustix::fs::OFlags::NONBLOCK).unwrap();
    assert!(
        rustix::io::read(&dev.master, &mut buf).is_err(),
        "keys reached the device"
    );
    assert!(s.child.try_wait().unwrap().is_none(), "it quit");
    // Nor is device output shown there.
    rustix::fs::fcntl_setfl(&s.host.master, rustix::fs::OFlags::NONBLOCK).unwrap();
    let mut shown = Vec::new();
    while let Ok(n) = rustix::io::read(&s.host.master, &mut buf) {
        if n == 0 {
            break;
        }
        shown.extend_from_slice(&buf[..n]);
    }
    assert!(
        !String::from_utf8_lossy(&shown).contains("from device"),
        "{:?}",
        String::from_utf8_lossy(&shown)
    );
    rustix::process::kill_process(
        rustix::process::Pid::from_child(&s.child),
        rustix::process::Signal::TERM,
    )
    .unwrap();
    s.wait_exit(T);
    let _ = std::fs::remove_dir_all(&dir);
}
