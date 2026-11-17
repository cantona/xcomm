// SPDX-FileCopyrightText: 2026 Su Kang Yin <paradyse@gmail.com>
// SPDX-License-Identifier: GPL-2.0-only

mod support;

use std::io::{BufRead, BufReader, Write};
use std::os::fd::AsFd;
use std::os::unix::net::UnixListener;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::Value;
use support::{Spawned, lock_dir, read_until, spawn_with_env, write_all};

const T: Duration = Duration::from_secs(5);
const PANE: &str = "w1:p1";

/// A herdr server that records each request and answers it.
struct Fake {
    dir: PathBuf,
    sock: PathBuf,
    seen: Arc<Mutex<Vec<Value>>>,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

#[derive(Clone, Copy, PartialEq)]
enum Mode {
    Ok,
    Fail,
    /// Answers after 2 s, long after xcomm gave up.
    Slow,
}

impl Fake {
    fn new(mode: Mode) -> Fake {
        let dir = lock_dir("herdr");
        let sock = dir.join("herdr.sock");
        let listener = UnixListener::bind(&sock).unwrap();
        listener.set_nonblocking(true).unwrap();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let thread = {
            let (seen, stop) = (seen.clone(), stop.clone());
            std::thread::spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    let Ok((stream, _)) = listener.accept() else {
                        std::thread::sleep(Duration::from_millis(5));
                        continue;
                    };
                    stream.set_nonblocking(false).unwrap();
                    let mut line = String::new();
                    if BufReader::new(&stream).read_line(&mut line).is_err() {
                        continue;
                    }
                    let req: Value = serde_json::from_str(&line).unwrap();
                    let id = req["id"].clone();
                    seen.lock().unwrap().push(req);
                    if mode == Mode::Slow {
                        std::thread::sleep(Duration::from_secs(2));
                    }
                    let reply = if mode == Mode::Fail {
                        serde_json::json!({ "id": id, "error": { "code": "pane_not_found", "message": "no such pane" } })
                    } else {
                        serde_json::json!({ "id": id, "result": { "type": "ok" } })
                    };
                    let _ = (&stream).write_all(format!("{reply}\n").as_bytes());
                }
            })
        };
        Fake {
            dir,
            sock,
            seen,
            stop,
            thread: Some(thread),
        }
    }

    fn env(&self) -> [(&'static str, String); 2] {
        [
            ("HERDR_SOCKET_PATH", self.sock.to_str().unwrap().to_string()),
            ("HERDR_PANE_ID", PANE.to_string()),
        ]
    }

    fn count(&self) -> usize {
        self.seen.lock().unwrap().len()
    }

    /// The first request from the `from`th on that `want` accepts.
    fn wait_for(&self, from: usize, want: impl Fn(&Value) -> bool) -> (usize, Value) {
        let until = Instant::now() + T;
        loop {
            if let Some((i, v)) = self
                .seen
                .lock()
                .unwrap()
                .iter()
                .enumerate()
                .skip(from)
                .find(|(_, v)| want(v))
            {
                return (i, v.clone());
            }
            assert!(
                Instant::now() < until,
                "not seen; got {:?}",
                self.seen.lock().unwrap()
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

impl Drop for Fake {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn titled(label: &str) -> impl Fn(&Value) -> bool + '_ {
    move |v: &Value| {
        v["method"] == "pane.report_metadata" && v["params"]["title"] == label
    }
}

fn session(dev: &str, env: &[(&str, &str)]) -> Spawned {
    let s = spawn_with_env(env!("CARGO_BIN_EXE_xcomm"), &[dev], &lock_dir("herdr"), env);
    read_until(s.host.master.as_fd(), b"\r\n", T);
    s
}

fn ctl(s: &Spawned, args: &[&str]) {
    let o = std::process::Command::new(env!("CARGO_BIN_EXE_xcomm"))
        .arg("ctl")
        .args(args)
        .env("XDG_RUNTIME_DIR", &s.lock_dir)
        .output()
        .unwrap();
    assert_eq!(
        o.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&o.stderr)
    );
}

#[test]
fn the_pane_is_named_after_the_port_and_its_state() {
    let fake = Fake::new(Mode::Ok);
    let dev = support::pty();
    let path = dev.slave_path.to_str().unwrap().to_string();
    let tty = xcomm::ctl::path::tty_name(&path).unwrap();
    let env = fake.env();
    let env: Vec<(&str, &str)> = env.iter().map(|(k, v)| (*k, v.as_str())).collect();
    let mut s = session(&path, &env);
    let label = format!("xcomm {tty} 115200");
    let (i, meta) = fake.wait_for(0, titled(&label));
    assert_eq!(meta["params"]["pane_id"], PANE);
    assert!(meta["id"].as_str().unwrap().starts_with("xcomm:"));
    assert_eq!(meta["params"]["source"], "xcomm");
    // The pane's tokens are everyone's: xcomm's have its own names.
    assert_eq!(meta["params"]["tokens"]["xcomm_port"], tty.as_str());
    assert_eq!(meta["params"]["tokens"]["xcomm_baud"], "115200");
    // A pty has no by-id link.
    assert!(meta["params"]["tokens"]["xcomm_by_id"].is_null());
    // It expires unless renewed: a killed session leaves nothing for long.
    assert_eq!(meta["params"]["ttl_ms"], 600_000);
    // A capture shows; its end shows too, without new metadata.
    let file = s.lock_dir.join("cap.txt");
    ctl(&s, &[&path, "capture", "start", file.to_str().unwrap()]);
    let (i, _) = fake.wait_for(i, titled(&format!("{label} [cap]")));
    ctl(&s, &[&path, "capture", "stop"]);
    let (i, _) = fake.wait_for(i + 1, titled(&label));
    // Only on changes: device output asks nothing of herdr.
    let before = fake.count();
    write_all(dev.master.as_fd(), b"hello");
    read_until(s.host.master.as_fd(), b"hello", T);
    std::thread::sleep(Duration::from_millis(200));
    assert_eq!(fake.count(), before);
    // Quitting drops the title and tokens; the user's own name for the
    // pane was never touched.
    write_all(s.host.master.as_fd(), b"\x01x");
    s.wait_exit(T);
    let (_, clear) = fake.wait_for(i, |v| v["params"]["clear_title"] == true);
    assert!(clear["params"]["tokens"]["xcomm_port"].is_null());
    assert!(clear["params"].get("title").is_none());
    assert!(
        fake.seen
            .lock()
            .unwrap()
            .iter()
            .all(|v| v["method"] != "pane.rename")
    );
}

#[test]
fn a_disconnected_port_shows_as_gone() {
    let fake = Fake::new(Mode::Ok);
    let dir = lock_dir("herdr-link");
    let dev = support::pty();
    let link = dir.join("dev");
    std::os::unix::fs::symlink(&dev.slave_path, &link).unwrap();
    let tty = xcomm::ctl::path::tty_name(dev.slave_path.to_str().unwrap()).unwrap();
    let env = fake.env();
    let env: Vec<(&str, &str)> = env.iter().map(|(k, v)| (*k, v.as_str())).collect();
    let s = session(link.to_str().unwrap(), &env);
    let label = format!("xcomm {tty} 115200");
    let (i, _) = fake.wait_for(0, titled(&label));
    drop(dev);
    read_until(s.host.master.as_fd(), b"waiting", T);
    fake.wait_for(i, titled(&format!("{label} [gone]")));
    drop(s);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_herdr_error_is_said_once_and_the_session_goes_on() {
    let fake = Fake::new(Mode::Fail);
    let dev = support::pty();
    let path = dev.slave_path.to_str().unwrap().to_string();
    let env = fake.env();
    let env: Vec<(&str, &str)> = env.iter().map(|(k, v)| (*k, v.as_str())).collect();
    let s = session(&path, &env);
    read_until(
        s.host.master.as_fd(),
        b"xcomm: herdr: no such pane (pane_not_found)",
        T,
    );
    assert_eq!(fake.count(), 1);
    // Still a serial terminal, and herdr is not asked again.
    write_all(dev.master.as_fd(), b"from device");
    read_until(s.host.master.as_fd(), b"from device", T);
    write_all(s.host.master.as_fd(), b"to device");
    read_until(dev.master.as_fd(), b"to device", T);
    let file = s.lock_dir.join("cap.txt");
    ctl(&s, &[&path, "capture", "start", file.to_str().unwrap()]);
    std::thread::sleep(Duration::from_millis(200));
    assert_eq!(fake.count(), 1);
}

#[test]
fn without_herdr_nothing_is_asked() {
    let fake = Fake::new(Mode::Ok);
    let dev = support::pty();
    let path = dev.slave_path.to_str().unwrap().to_string();
    // The socket is there, the pane id is not.
    let env = [("HERDR_SOCKET_PATH", fake.sock.to_str().unwrap())];
    let s = session(&path, &env);
    let file = s.lock_dir.join("cap.txt");
    ctl(&s, &[&path, "capture", "start", file.to_str().unwrap()]);
    std::thread::sleep(Duration::from_millis(200));
    assert_eq!(fake.count(), 0);
}

#[test]
fn the_client_needs_both_variables_and_checks_the_reply() {
    use xcomm::herdr::{Herdr, HerdrError};
    let none = |_: &str| None;
    assert!(Herdr::from_env(&none).is_none());
    let only_pane = |k: &str| (k == "HERDR_PANE_ID").then(|| "p".to_string());
    assert!(Herdr::from_env(&only_pane).is_none());
    // A server that is not there fails at once.
    let gone = |k: &str| match k {
        "HERDR_SOCKET_PATH" => Some("/nonexistent/herdr.sock".to_string()),
        "HERDR_PANE_ID" => Some("p".to_string()),
        _ => None,
    };
    let h = Herdr::from_env(&gone).unwrap();
    assert_eq!(h.pane_id(), Some("p"));
    // The popup a picker runs in has the socket but no pane.
    let socket_only = |k: &str| (k == "HERDR_SOCKET_PATH").then(|| "/x".to_string());
    assert!(Herdr::from_env(&socket_only).is_none());
    let server = Herdr::server_from_env(&socket_only).unwrap();
    assert_eq!(server.pane_id(), None);
    assert!(matches!(
        server.report_metadata(Some("t"), &[], None),
        Err(HerdrError::Io(_))
    ));
    assert!(matches!(h.notify("t", None), Err(HerdrError::Io(_))));
    let fake = Fake::new(Mode::Ok);
    let sock = fake.sock.to_str().unwrap().to_string();
    let env = move |k: &str| match k {
        "HERDR_SOCKET_PATH" => Some(sock.clone()),
        "HERDR_PANE_ID" => Some("w2:p3".to_string()),
        _ => None,
    };
    let h = Herdr::from_env(&env).unwrap();
    h.notify("Serial added", Some("FT232R (ttyUSB2)")).unwrap();
    h.focus_pane("w2:p9").unwrap();
    let (_, n) = fake.wait_for(0, |v| v["method"] == "notification.show");
    assert_eq!(n["params"]["title"], "Serial added");
    assert_eq!(n["params"]["body"], "FT232R (ttyUSB2)");
    let (_, f) = fake.wait_for(0, |v| v["method"] == "pane.focus");
    assert_eq!(f["params"]["pane_id"], "w2:p9");
}

#[test]
fn a_transfer_shows_while_it_runs() {
    let has_rz = ["rz", "lrz"].iter().any(|p| {
        std::env::var_os("PATH")
            .is_some_and(|path| std::env::split_paths(&path).any(|d| d.join(p).is_file()))
    });
    if !has_rz {
        eprintln!("skipped: lrzsz is not on PATH");
        return;
    }
    let fake = Fake::new(Mode::Ok);
    let dev = support::pty();
    let path = dev.slave_path.to_str().unwrap().to_string();
    let tty = xcomm::ctl::path::tty_name(&path).unwrap();
    let env = fake.env();
    let env: Vec<(&str, &str)> = env.iter().map(|(k, v)| (*k, v.as_str())).collect();
    let s = session(&path, &env);
    let label = format!("xcomm {tty} 115200");
    let (i, _) = fake.wait_for(0, titled(&label));
    let here = s.lock_dir.clone();
    // No sender: rz waits until its client goes.
    let mut client = std::process::Command::new(env!("CARGO_BIN_EXE_xcomm"))
        .args(["ctl", &path, "xfer", "recv", "zmodem"])
        .env("XDG_RUNTIME_DIR", &here)
        .current_dir(&here)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let (i, _) = fake.wait_for(i, titled(&format!("{label} [xfer]")));
    client.kill().unwrap();
    client.wait().unwrap();
    fake.wait_for(i + 1, titled(&label));
}

#[test]
fn a_slow_herdr_holds_the_session_up_half_a_second_once() {
    let fake = Fake::new(Mode::Slow);
    let dev = support::pty();
    let path = dev.slave_path.to_str().unwrap().to_string();
    let env = fake.env();
    let env: Vec<(&str, &str)> = env.iter().map(|(k, v)| (*k, v.as_str())).collect();
    let t = Instant::now();
    let s = session(&path, &env);
    read_until(
        s.host.master.as_fd(),
        b"xcomm: herdr: pane.report_metadata: no reply within 500ms",
        T,
    );
    assert!(
        t.elapsed() < Duration::from_millis(1500),
        "{:?}",
        t.elapsed()
    );
    write_all(dev.master.as_fd(), b"from device");
    read_until(s.host.master.as_fd(), b"from device", T);
    let file = s.lock_dir.join("cap.txt");
    let t = Instant::now();
    ctl(&s, &[&path, "capture", "start", file.to_str().unwrap()]);
    assert!(
        t.elapsed() < Duration::from_millis(400),
        "{:?}",
        t.elapsed()
    );
}

#[test]
fn a_headless_session_leaves_the_pane_alone() {
    let fake = Fake::new(Mode::Ok);
    let dev = support::pty();
    let path = dev.slave_path.to_str().unwrap().to_string();
    let env = fake.env();
    let env: Vec<(&str, &str)> = env.iter().map(|(k, v)| (*k, v.as_str())).collect();
    let s = spawn_with_env(
        env!("CARGO_BIN_EXE_xcomm"),
        &["--headless", &path],
        &lock_dir("herdr"),
        &env,
    );
    read_until(s.host.master.as_fd(), b"headless session on", T);
    let file = s.lock_dir.join("cap.txt");
    ctl(&s, &[&path, "capture", "start", file.to_str().unwrap()]);
    std::thread::sleep(Duration::from_millis(200));
    assert_eq!(fake.count(), 0);
}

#[test]
fn a_session_in_a_multiplexer_inside_the_pane_leaves_it_alone() {
    let fake = Fake::new(Mode::Ok);
    let dev = support::pty();
    let path = dev.slave_path.to_str().unwrap().to_string();
    let mut env = fake.env().to_vec();
    env.push(("TMUX", "/tmp/tmux-1000/default,1,0".to_string()));
    let env: Vec<(&str, &str)> = env.iter().map(|(k, v)| (*k, v.as_str())).collect();
    let s = session(&path, &env);
    let file = s.lock_dir.join("cap.txt");
    ctl(&s, &[&path, "capture", "start", file.to_str().unwrap()]);
    std::thread::sleep(Duration::from_millis(200));
    assert_eq!(fake.count(), 0);
}
