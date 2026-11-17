// SPDX-FileCopyrightText: 2026 Su Kang Yin <paradyse@gmail.com>
// SPDX-License-Identifier: GPL-2.0-only

mod support;

use std::os::fd::{AsFd, OwnedFd};
use std::path::Path;
use std::process::{Child, Command, Output, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use support::{Pty, Spawned, lock_dir, read_until, spawn_xcomm, write_all};

const T: Duration = Duration::from_secs(5);

/// A session on a fresh pty; its sockets live under its lock directory.
fn start(extra: &[&str]) -> (Pty, Spawned, String) {
    let dev = support::pty();
    let path = dev.slave_path.to_str().unwrap().to_string();
    let mut args: Vec<&str> = extra.to_vec();
    args.push(&path);
    let s = spawn_xcomm(&args, &lock_dir("ctlw"));
    read_until(s.host.master.as_fd(), b"\r\n", T);
    (dev, s, path)
}

fn ctl_cmd(runtime: &Path, args: &[&str]) -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_xcomm"));
    c.arg("ctl")
        .args(args)
        .env("XDG_RUNTIME_DIR", runtime)
        .stdin(Stdio::null());
    c
}

fn ctl(s: &Spawned, args: &[&str]) -> Output {
    ctl_cmd(&s.lock_dir, args).output().unwrap()
}

fn spawn_ctl(s: &Spawned, args: &[&str]) -> Child {
    ctl_cmd(&s.lock_dir, args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap()
}

fn stderr(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

fn json_of(o: &Output) -> serde_json::Value {
    serde_json::from_slice(&o.stdout)
        .unwrap_or_else(|e| panic!("{e}: {:?} / {}", o.stdout, stderr(o)))
}

/// Waits for the terminal to have shown `bytes`: then the session has read
/// them, and they are in the ring.
fn shown(s: &Spawned, bytes: &[u8]) {
    read_until(s.host.master.as_fd(), bytes, T);
}

fn offset(s: &Spawned, path: &str) -> u64 {
    json_of(&ctl(s, &["--json", path, "status"]))["offset"]
        .as_u64()
        .unwrap()
}

/// Reads and drops what `fd` gives until stopped: a macOS pty holds about
/// 1 KiB, and a session with a full terminal stops reading the device.
fn drain(fd: &OwnedFd) -> (Arc<AtomicBool>, std::thread::JoinHandle<()>) {
    let fd = fd.try_clone().unwrap();
    let fl = rustix::fs::fcntl_getfl(&fd).unwrap();
    rustix::fs::fcntl_setfl(&fd, fl | rustix::fs::OFlags::NONBLOCK).unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let seen = stop.clone();
    let t = std::thread::spawn(move || {
        let mut buf = [0u8; 4096];
        while !seen.load(Ordering::Relaxed) {
            if rustix::io::read(fd.as_fd(), &mut buf).is_err() {
                std::thread::sleep(Duration::from_millis(5));
            }
        }
    });
    (stop, t)
}

/// A device that answers each line ending in CR with `answer(line)` in
/// one write, until stopped.
fn device(
    dev: &Pty,
    answer: impl Fn(&[u8]) -> Vec<u8> + Send + 'static,
) -> (Arc<AtomicBool>, std::thread::JoinHandle<()>) {
    let fd = dev.master.try_clone().unwrap();
    let fl = rustix::fs::fcntl_getfl(&fd).unwrap();
    rustix::fs::fcntl_setfl(&fd, fl | rustix::fs::OFlags::NONBLOCK).unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let seen = stop.clone();
    let t = std::thread::spawn(move || {
        let mut line = Vec::new();
        let mut buf = [0u8; 4096];
        while !seen.load(Ordering::Relaxed) {
            let n = match rustix::io::read(fd.as_fd(), &mut buf) {
                Ok(n) => n,
                Err(_) => {
                    std::thread::sleep(Duration::from_millis(1));
                    continue;
                }
            };
            for &b in &buf[..n] {
                if b == b'\r' {
                    write_all(fd.as_fd(), &answer(&line));
                    line.clear();
                } else {
                    line.push(b);
                }
            }
        }
    });
    (stop, t)
}

#[test]
fn wait_matches_future_output() {
    let (dev, s, path) = start(&[]);
    let child = spawn_ctl(&s, &["--json", &path, "wait", r"login(\d):"]);
    // Over and over until it is seen: the wait starts from whenever it
    // subscribed.
    let done = Arc::new(AtomicBool::new(false));
    let master = dev.master.try_clone().unwrap();
    let flag = done.clone();
    let writer = std::thread::spawn(move || {
        while !flag.load(Ordering::Relaxed) {
            write_all(master.as_fd(), b"login7: ");
            std::thread::sleep(Duration::from_millis(50));
        }
    });
    let o = child.wait_with_output().unwrap();
    done.store(true, Ordering::Relaxed);
    writer.join().unwrap();
    assert_eq!(o.status.code(), Some(0), "{}", stderr(&o));
    let v = json_of(&o);
    assert_eq!(v["which"], "until");
    assert_eq!(v["text"], "login7:");
    assert_eq!(v["groups"], serde_json::json!(["7"]));
    let (start, end) = (v["start"].as_u64().unwrap(), v["end"].as_u64().unwrap());
    assert_eq!(end - start, 7);
    assert_eq!(start % 8, 0, "{v}");
    assert!(v["elapsed_ms"].is_u64());
}

#[test]
fn wait_since_replays_past_output() {
    let (dev, s, path) = start(&[]);
    write_all(dev.master.as_fd(), b"boot ok\r\n");
    shown(&s, b"boot ok");
    let o = ctl(&s, &[&path, "wait", r"boot (\w+)", "--since", "0"]);
    assert_eq!(o.status.code(), Some(0), "{}", stderr(&o));
    assert_eq!(String::from_utf8_lossy(&o.stdout), "boot ok\n");
    assert_eq!(stderr(&o), "end=7\n");
    let v = json_of(&ctl(
        &s,
        &["--json", &path, "wait", r"boot (\w+)", "--since", "0"],
    ));
    assert_eq!(v["groups"], serde_json::json!(["ok"]));
    assert_eq!((v["start"].as_u64(), v["end"].as_u64()), (Some(0), Some(7)));
    assert_eq!(v["offset"], 0);
    assert_eq!(v["pattern"], r"boot (\w+)");
}

#[test]
fn wait_times_out_exit_1() {
    let (dev, s, path) = start(&[]);
    write_all(dev.master.as_fd(), b"abc");
    shown(&s, b"abc");
    let t0 = Instant::now();
    let o = ctl(
        &s,
        &[&path, "wait", "never", "--since", "0", "--timeout", "0.3"],
    );
    let took = t0.elapsed();
    assert_eq!(o.status.code(), Some(1), "{}", stderr(&o));
    assert!(stderr(&o).contains("timed out"), "{}", stderr(&o));
    assert!(stderr(&o).ends_with("end=3\n"), "{}", stderr(&o));
    assert!(o.stdout.is_empty());
    assert!(took >= Duration::from_millis(300), "{took:?}");
    assert!(took < Duration::from_secs(3), "{took:?}");
    let v = json_of(&ctl(
        &s,
        &["--json", &path, "wait", "never", "--timeout", "0.1"],
    ));
    assert_eq!(v["which"], "timeout");
    assert_eq!(v["pattern"], serde_json::Value::Null);
    assert_eq!(v["end"], 3);
}

#[test]
fn fail_on_exit_7_names_pattern() {
    let (dev, s, path) = start(&[]);
    write_all(dev.master.as_fd(), b"Kernel panic - not syncing\r\nlogin: ");
    shown(&s, b"login: ");
    let o = ctl(
        &s,
        &[
            &path,
            "wait",
            "login:",
            "--since",
            "0",
            "--fail-on",
            "Oops",
            "--fail-on",
            "panic|BUG",
        ],
    );
    assert_eq!(o.status.code(), Some(7), "{}", stderr(&o));
    assert!(
        stderr(&o).contains("'panic|BUG' matched: panic"),
        "{}",
        stderr(&o)
    );
    let o = ctl(
        &s,
        &[
            "--json",
            &path,
            "wait",
            "login:",
            "--since",
            "0",
            "--fail-on",
            "panic",
        ],
    );
    assert_eq!(o.status.code(), Some(7));
    let v = json_of(&o);
    assert_eq!(v["which"], "fail_on");
    assert_eq!(v["pattern"], "panic");
    assert_eq!(v["start"], 7);
}

#[test]
fn match_split_across_chunks() {
    let (dev, s, path) = start(&[]);
    let from = offset(&s, &path).to_string();
    let child = spawn_ctl(&s, &["--json", &path, "wait", "login:", "--since", &from]);
    write_all(dev.master.as_fd(), b"log");
    shown(&s, b"log");
    std::thread::sleep(Duration::from_millis(50));
    write_all(dev.master.as_fd(), b"in:");
    let o = child.wait_with_output().unwrap();
    assert_eq!(o.status.code(), Some(0), "{}", stderr(&o));
    let v = json_of(&o);
    assert_eq!(v["text"], "login:");
    assert_eq!((v["start"].as_u64(), v["end"].as_u64()), (Some(0), Some(6)));
}

#[test]
fn match_ignores_color_codes() {
    let (dev, s, path) = start(&[]);
    write_all(dev.master.as_fd(), b"\x1b[32mlogin\x1b[0m: ");
    shown(&s, b": ");
    let v = json_of(&ctl(
        &s,
        &["--json", &path, "wait", "login:", "--since", "0"],
    ));
    assert_eq!(v["which"], "until");
    // Raw offsets: the text starts after ESC[32m and ends past ESC[0m:.
    assert_eq!(
        (v["start"].as_u64(), v["end"].as_u64()),
        (Some(5), Some(15))
    );
    let o = ctl(
        &s,
        &[
            &path,
            "wait",
            "login:",
            "--since",
            "0",
            "--raw",
            "--timeout",
            "0.3",
        ],
    );
    assert_eq!(o.status.code(), Some(1), "{}", stderr(&o));
    let o = ctl(&s, &[&path, "wait", r"\x1b\[0m:", "--since", "0", "--raw"]);
    assert_eq!(o.status.code(), Some(0), "{}", stderr(&o));
}

#[test]
fn idle_returns_after_quiet() {
    let (dev, s, path) = start(&[]);
    write_all(dev.master.as_fd(), b"xyz");
    shown(&s, b"xyz");
    let o = ctl(&s, &["--json", &path, "wait", "--idle", "300"]);
    assert_eq!(o.status.code(), Some(0), "{}", stderr(&o));
    let v = json_of(&o);
    assert_eq!(v["which"], "idle");
    assert_eq!(v["end"], 3);
    assert!(v["elapsed_ms"].as_u64().unwrap() >= 300, "{v}");
    // Output keeps it waiting; a pattern that matches first wins.
    let child = spawn_ctl(
        &s,
        &[
            "--json",
            &path,
            "wait",
            "done",
            "--idle",
            "400",
            "--timeout",
            "5",
        ],
    );
    for _ in 0..8 {
        write_all(dev.master.as_fd(), b".");
        std::thread::sleep(Duration::from_millis(100));
    }
    write_all(dev.master.as_fd(), b"done");
    let o = child.wait_with_output().unwrap();
    assert_eq!(json_of(&o)["which"], "until", "{}", stderr(&o));
    // The timeout ends it when the device never goes quiet for long.
    let child = spawn_ctl(
        &s,
        &["--json", &path, "wait", "--idle", "400", "--timeout", "0.5"],
    );
    for _ in 0..10 {
        write_all(dev.master.as_fd(), b".");
        std::thread::sleep(Duration::from_millis(100));
    }
    let o = child.wait_with_output().unwrap();
    assert_eq!(o.status.code(), Some(1));
    assert_eq!(json_of(&o)["which"], "timeout");
}

#[test]
fn run_has_no_race() {
    let (dev, s, path) = start(&[]);
    let (stop_term, term) = drain(&s.host.master);
    // The answer comes in the same write as the echo: the device is done
    // before the client could subscribe after the send.
    let (stop_dev, devt) = device(&dev, |line| {
        let mut out = line.to_vec();
        out.extend_from_slice(b"\r\nanswer ");
        out.extend_from_slice(line);
        out.extend_from_slice(b"\r\n# ");
        out
    });
    for i in 0..200 {
        let cmd = format!("c{i}");
        let o = ctl(&s, &["--json", &path, "run", &cmd, "--until", "# $"]);
        assert_eq!(o.status.code(), Some(0), "run {i}: {}", stderr(&o));
        let v = json_of(&o);
        assert_eq!(v["output"], format!("answer {cmd}\r\n"), "run {i}");
    }
    stop_dev.store(true, Ordering::Relaxed);
    devt.join().unwrap();
    stop_term.store(true, Ordering::Relaxed);
    term.join().unwrap();
}

/// What `args` writes with stdout on a terminal (else a pipe), and stderr on
/// the same terminal or pipe (`same`) or on a pipe of its own: what that
/// terminal or pipe got, and stderr's own pipe. The terminal has OPOST off,
/// so it shows the bytes as written.
fn ctl_to(s: &Spawned, args: &[&str], terminal: bool, same: bool) -> (String, String) {
    let (reader, writer) = if terminal {
        let term = support::pty();
        let mut t = rustix::termios::tcgetattr(&term.slave).unwrap();
        t.output_modes.remove(rustix::termios::OutputModes::OPOST);
        rustix::termios::tcsetattr(
            &term.slave,
            rustix::termios::OptionalActions::Now,
            &t,
        )
        .unwrap();
        (term.master, term.slave)
    } else {
        cloexec_pipe()
    };
    let mut cmd = ctl_cmd(&s.lock_dir, args);
    cmd.stdout(Stdio::from(writer.try_clone().unwrap()));
    if same {
        cmd.stderr(Stdio::from(writer.try_clone().unwrap()));
    }
    let o = cmd.output().unwrap();
    assert_eq!(o.status.code(), Some(0), "{}", stderr(&o));
    // The Command holds copies of the writer too.
    drop(cmd);
    let mut got = Vec::new();
    let mut buf = [0u8; 4096];
    if terminal {
        // macOS discards what the master has not read once the last slave
        // fd closes, so keep ours open, write a marker after everything
        // ctl wrote (the terminal keeps the order, OPOST is off) and read
        // up to it.
        const END: &[u8] = b"\x00end-of-ctl\x00";
        write_all(writer.as_fd(), END);
        got = read_until(reader.as_fd(), END, T);
        assert!(got.ends_with(END), "{got:?}");
        got.truncate(got.len() - END.len());
    } else {
        // A pipe keeps what was written: close the last writer and read to
        // the end.
        drop(writer);
        while let Ok(n @ 1..) = rustix::io::read(&reader, &mut buf) {
            got.extend_from_slice(&buf[..n]);
        }
    }
    (String::from_utf8_lossy(&got).into_owned(), stderr(&o))
}

/// A pipe no child spawned by another test thread can inherit: a held
/// write end would keep ctl_to's read from ever ending. macOS has no
/// pipe2(2); there the flag comes just after.
fn cloexec_pipe() -> (OwnedFd, OwnedFd) {
    #[cfg(target_os = "linux")]
    let p = rustix::pipe::pipe_with(rustix::pipe::PipeFlags::CLOEXEC).unwrap();
    #[cfg(not(target_os = "linux"))]
    let p = {
        let p = rustix::pipe::pipe().unwrap();
        for fd in [&p.0, &p.1] {
            rustix::io::fcntl_setfd(fd, rustix::io::FdFlags::CLOEXEC).unwrap();
        }
        p
    };
    p
}

/// Splits `end=N` (or `next=N`) and its line break off the end of `out`.
fn offset_line<'a>(out: &'a str, key: &str) -> &'a str {
    let at = out
        .rfind(key)
        .unwrap_or_else(|| panic!("no {key} in {out:?}"));
    let n = &out[at + key.len()..];
    assert!(
        n.ends_with('\n') && n[..n.len() - 1].bytes().all(|b| b.is_ascii_digit()),
        "{out:?}"
    );
    &out[..at]
}

#[test]
fn run_and_read_keep_the_offset_line_apart() {
    let (dev, s, path) = start(&[]);
    let (stop, devt) = device(&dev, |line| {
        let mut out = line.to_vec();
        if line == b"colour" {
            out.extend_from_slice(b"\r\nok\r\n\x1b[0m");
        } else {
            out.extend_from_slice(b"\r\n6.1+\r\n# ");
        }
        out
    });
    let run = [
        path.as_str(),
        "run",
        "uname -r",
        "--until",
        "\\r?\\n[^\\n]*# $",
    ];
    // One terminal, or one pipe (2>&1): a line break before end=.
    for terminal in [true, false] {
        let (got, _) = ctl_to(&s, &run, terminal, true);
        assert_eq!(offset_line(&got, "end="), "6.1+\n", "terminal {terminal}");
    }
    // stderr elsewhere: stdout is the output exactly, on a terminal too.
    for terminal in [true, false] {
        let (got, err) = ctl_to(&s, &run, terminal, false);
        assert_eq!(got, "6.1+", "terminal {terminal}");
        assert_eq!(offset_line(&err, "end="), "", "terminal {terminal}");
    }
    // A file: one open shared (2>&1) gets the line break; two opens of it
    // (>f 2>f) each write at their own offset, so it would land on the
    // output's first byte, and is left out.
    let dir =
        std::env::temp_dir().join(format!("xcomm-offset-line-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let f = dir.join("out");
    let shared = std::fs::File::create(&f).unwrap();
    let o = ctl_cmd(&s.lock_dir, &run)
        .stdout(shared.try_clone().unwrap())
        .stderr(shared)
        .output()
        .unwrap();
    assert_eq!(o.status.code(), Some(0));
    let got = std::fs::read_to_string(&f).unwrap();
    assert_eq!(offset_line(&got, "end="), "6.1+\n");
    let o = ctl_cmd(&s.lock_dir, &run)
        .stdout(std::fs::File::create(&f).unwrap())
        .stderr(std::fs::OpenOptions::new().write(true).open(&f).unwrap())
        .output()
        .unwrap();
    assert_eq!(o.status.code(), Some(0));
    let got = std::fs::read_to_string(&f).unwrap();
    assert!(got.starts_with("end="), "{got:?}");
    // Two opens for appending (>>f 2>>f) write at the end alike: one place.
    std::fs::write(&f, "").unwrap();
    let append = || std::fs::OpenOptions::new().append(true).open(&f).unwrap();
    let o = ctl_cmd(&s.lock_dir, &run)
        .stdout(append())
        .stderr(append())
        .output()
        .unwrap();
    assert_eq!(o.status.code(), Some(0));
    let got = std::fs::read_to_string(&f).unwrap();
    assert_eq!(offset_line(&got, "end="), "6.1+\n");
    std::fs::remove_dir_all(&dir).unwrap();
    // -q: no end= line at all, and stdout as it was.
    let q = [
        path.as_str(),
        "run",
        "-q",
        "uname -r",
        "--until",
        "\\r?\\n[^\\n]*# $",
    ];
    let (got, err) = ctl_to(&s, &q, false, false);
    assert_eq!((got.as_str(), err.as_str()), ("6.1+", ""));
    let (got, err) = ctl_to(&s, &[&path, "read", "-q", "--last", "2"], false, false);
    assert_eq!((got.as_str(), err.as_str()), ("# ", ""));
    // Nor a line break, where stderr is stdout's stream: nothing follows it.
    for terminal in [true, false] {
        let (got, _) = ctl_to(&s, &q, terminal, true);
        assert_eq!(got, "6.1+", "terminal {terminal}");
        let (got, _) = ctl_to(&s, &[&path, "read", "-q", "--last", "2"], terminal, true);
        assert_eq!(got, "# ", "terminal {terminal}");
    }
    // Unless a message follows: why the wait ended.
    let (r, w) = cloexec_pipe();
    let o = ctl_cmd(
        &s.lock_dir,
        &[
            &path,
            "run",
            "-q",
            "uname -r",
            "--until",
            "never",
            "--fail-on",
            "\\r?\\n[^\\n]*# $",
        ],
    )
    .stdout(Stdio::from(w.try_clone().unwrap()))
    .stderr(Stdio::from(w.try_clone().unwrap()))
    .output()
    .unwrap();
    assert_eq!(o.status.code(), Some(7));
    drop(w);
    let mut got = Vec::new();
    let mut buf = [0u8; 4096];
    while let Ok(n @ 1..) = rustix::io::read(&r, &mut buf) {
        got.extend_from_slice(&buf[..n]);
    }
    let got = String::from_utf8_lossy(&got);
    assert!(got.starts_with("6.1+\nxcomm: --fail-on "), "{got:?}");
    // read: the same, here for the prompt, which does not end its line.
    let read = [path.as_str(), "read", "--last", "2"];
    let (got, _) = ctl_to(&s, &read, true, true);
    assert_eq!(offset_line(&got, "next="), "# \n");
    let (got, _) = ctl_to(&s, &read, true, false);
    assert_eq!(got, "# ");
    // Output that ends its line gets no second line break.
    let ends = [path.as_str(), "run", "uname -r", "--until", "# $"];
    let (got, _) = ctl_to(&s, &ends, true, true);
    assert_eq!(offset_line(&got, "end="), "6.1+\r\n");
    // Nor when only escape sequences follow the line break: read keeps
    // them on a terminal, and they do not move the cursor.
    let o = ctl(&s, &[&path, "send", "colour"]);
    assert_eq!(o.status.code(), Some(0), "{}", stderr(&o));
    shown(&s, b"ok\r\n");
    let (got, _) = ctl_to(&s, &[&path, "read", "--last", "8"], true, true);
    assert_eq!(offset_line(&got, "next="), "ok\r\n\x1b[0m");
    stop.store(true, Ordering::Relaxed);
    devt.join().unwrap();
}

#[test]
fn run_json_output_excludes_echo_and_prompt() {
    let (dev, s, path) = start(&[]);
    let (stop, devt) = device(&dev, |line| {
        let mut out = line.to_vec();
        out.extend_from_slice(b"\r\nLinux x 6.1\r\n# ");
        out
    });
    let o = ctl(&s, &["--json", &path, "run", "uname -a", "--until", "# $"]);
    assert_eq!(o.status.code(), Some(0), "{}", stderr(&o));
    let v = json_of(&o);
    assert_eq!(v["output"], "Linux x 6.1\r\n");
    assert_eq!(v["which"], "until");
    assert_eq!(v["text"], "# ");
    let o = ctl(&s, &[&path, "run", "uname -a", "--until", "# $"]);
    assert_eq!(o.status.code(), Some(0), "{}", stderr(&o));
    assert_eq!(String::from_utf8_lossy(&o.stdout), "Linux x 6.1\r\n");
    // No echo: all of it is output.
    stop.store(true, Ordering::Relaxed);
    devt.join().unwrap();
    let (stop, devt) = device(&dev, |_| b"quiet\r\n# ".to_vec());
    let v = json_of(&ctl(&s, &["--json", &path, "run", "x", "--until", "# $"]));
    assert_eq!(v["output"], "quiet\r\n");
    stop.store(true, Ordering::Relaxed);
    devt.join().unwrap();
}

#[test]
fn run_reports_fail_on_and_idle() {
    let (dev, s, path) = start(&[]);
    let (stop, devt) = device(&dev, |line| {
        let mut out = line.to_vec();
        out.extend_from_slice(b"\r\nERROR 5\r\n# ");
        out
    });
    let o = ctl(
        &s,
        &[
            "--json",
            &path,
            "run",
            "go",
            "--until",
            "# $",
            "--fail-on",
            r"ERROR (\d)",
        ],
    );
    assert_eq!(o.status.code(), Some(7), "{}", stderr(&o));
    let v = json_of(&o);
    assert_eq!(v["which"], "fail_on");
    assert_eq!(v["groups"], serde_json::json!(["5"]));
    assert_eq!(v["output"], "");
    let o = ctl(&s, &["--json", &path, "run", "go", "--idle", "200"]);
    assert_eq!(o.status.code(), Some(0), "{}", stderr(&o));
    let v = json_of(&o);
    assert_eq!(v["which"], "idle");
    assert_eq!(v["output"], "ERROR 5\r\n# ");
    stop.store(true, Ordering::Relaxed);
    devt.join().unwrap();
}

#[test]
fn a_subscriber_that_falls_behind_exits_9() {
    let (dev, s, path) = start(&["--ring-size", "4k"]);
    let (stop_term, term) = drain(&s.host.master);
    let from = offset(&s, &path).to_string();
    let child = spawn_ctl(
        &s,
        &[&path, "wait", "never", "--since", &from, "--timeout", "0"],
    );
    // Time to subscribe; then it reads nothing while the ring goes round.
    std::thread::sleep(Duration::from_secs(1));
    let pid = rustix::process::Pid::from_child(&child);
    rustix::process::kill_process(pid, rustix::process::Signal::STOP).unwrap();
    let master = dev.master.try_clone().unwrap();
    let writer = std::thread::spawn(move || {
        write_all(master.as_fd(), &vec![b'x'; 2 << 20]);
        write_all(master.as_fd(), b"END");
    });
    writer.join().unwrap();
    // The session has read it all once a status shows the offset.
    let until = Instant::now() + T;
    while offset(&s, &path) < (2 << 20) + 3 {
        assert!(Instant::now() < until, "the session did not read it all");
        std::thread::sleep(Duration::from_millis(20));
    }
    rustix::process::kill_process(pid, rustix::process::Signal::CONT).unwrap();
    let o = child.wait_with_output().unwrap();
    assert_eq!(o.status.code(), Some(9), "{}", stderr(&o));
    assert!(
        stderr(&o).contains("subscriber too slow at offset"),
        "{}",
        stderr(&o)
    );
    stop_term.store(true, Ordering::Relaxed);
    term.join().unwrap();
}

#[test]
fn the_dropped_line_is_what_the_session_sends() {
    use xcomm::ctl::proto::{dropped_at, dropped_line, dropped_start};
    let line = dropped_line(1234);
    assert!(
        line.contains(&format!("\"code\":{}", xcomm::ctl::code::DROPPED)),
        "{line}"
    );
    let mut bytes = b"device bytes".to_vec();
    bytes.extend_from_slice(line.as_bytes());
    assert_eq!(dropped_at(&bytes), Some((12, 1234)));
    assert_eq!(dropped_at(&bytes[..bytes.len() - 1]), None);
    assert_eq!(dropped_at(b"\"}\n"), None);
    // Its start, cut short anywhere, is held back; device output is not.
    for cut in 2..line.len() {
        let mut bytes = b"abc".to_vec();
        bytes.extend_from_slice(&line.as_bytes()[..cut]);
        assert_eq!(dropped_start(&bytes), cut, "cut at {cut}");
    }
    assert_eq!(dropped_start(&bytes), 0);
    assert_eq!(dropped_start(b"line\n"), 0);
    assert_eq!(dropped_start(b"line\n{"), 2);
    assert_eq!(dropped_start(b"line\n{\"ok\":true"), 0);
}

#[test]
fn without_xcomm_wait_beside_it_wait_exits_2() {
    let dir = lock_dir("ctlw-alone");
    let alone = dir.join("xcomm");
    support::copy_exe(Path::new(env!("CARGO_BIN_EXE_xcomm")), &alone);
    let o = Command::new(&alone)
        .args(["ctl", "ttyNONE", "wait", "x"])
        .env("XDG_RUNTIME_DIR", &dir)
        .output()
        .unwrap();
    assert_eq!(o.status.code(), Some(2), "{}", stderr(&o));
    assert!(stderr(&o).contains("xcomm-wait"), "{}", stderr(&o));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn wait_on_missing_session_exit_3() {
    let dev = support::pty();
    let runtime = lock_dir("ctlw-none");
    let o = ctl_cmd(&runtime, &[dev.slave_path.to_str().unwrap(), "wait", "x"])
        .output()
        .unwrap();
    assert_eq!(o.status.code(), Some(3), "{}", stderr(&o));
    let o = ctl_cmd(
        &runtime,
        &[dev.slave_path.to_str().unwrap(), "run", "x", "--until", "y"],
    )
    .output()
    .unwrap();
    assert_eq!(o.status.code(), Some(3), "{}", stderr(&o));
    let _ = std::fs::remove_dir_all(&runtime);
}

#[test]
fn a_bad_pattern_is_a_usage_error_before_anything_is_sent() {
    let (dev, s, path) = start(&[]);
    let o = ctl(&s, &[&path, "run", "reboot", "--until", "("]);
    assert_eq!(o.status.code(), Some(2), "{}", stderr(&o));
    assert!(stderr(&o).contains("invalid pattern"), "{}", stderr(&o));
    let o = ctl(&s, &[&path, "send", "-n", "after"]);
    assert_eq!(o.status.code(), Some(0), "{}", stderr(&o));
    assert_eq!(read_until(dev.master.as_fd(), b"after", T), b"after");
}

#[test]
fn the_session_ending_ends_a_wait_with_3() {
    // The wait gets 500 ms to connect before the session quits. Under load
    // it can start later and find no session at all, which tests nothing:
    // that try is run again, and only a wait that connected is judged.
    let mut last = String::new();
    for _ in 0..3 {
        let (_dev, s, path) = start(&[]);
        let child = spawn_ctl(&s, &[&path, "wait", "never", "--timeout", "0"]);
        std::thread::sleep(Duration::from_millis(500));
        let mut s = s;
        write_all(s.host.master.as_fd(), b"\x01x");
        s.wait_exit(T);
        let o = child.wait_with_output().unwrap();
        if stderr(&o).contains("no session for") {
            last = stderr(&o);
            continue;
        }
        assert_eq!(o.status.code(), Some(3), "{}", stderr(&o));
        assert!(stderr(&o).contains("session ended"), "{}", stderr(&o));
        return;
    }
    panic!("the wait never found the session in three tries: {last}");
}
