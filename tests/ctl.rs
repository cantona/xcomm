mod support;

use std::io::{BufRead, BufReader, Read, Write};
use std::os::fd::AsFd;
use std::os::unix::fs::DirBuilderExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::Output;
use std::time::Duration;

use support::{Pty, Spawned, lock_dir, read_until, spawn_xcomm, write_all};
use xcomm::ctl::proto::{base64_decode, base64_encode};

const T: Duration = Duration::from_secs(5);

/// A session on a fresh pty; its sockets live under its lock directory.
fn start(extra: &[&str]) -> (Pty, Spawned, String) {
    let dev = support::pty();
    let path = dev.slave_path.to_str().unwrap().to_string();
    let mut args: Vec<&str> = extra.to_vec();
    args.push(&path);
    let s = spawn_xcomm(&args, &lock_dir("ctl"));
    read_until(s.host.master.as_fd(), b"\r\n", T);
    (dev, s, path)
}

fn ctl_in(runtime: &Path, args: &[&str]) -> Output {
    std::process::Command::new(env!("CARGO_BIN_EXE_xcomm"))
        .arg("ctl")
        .args(args)
        .env("XDG_RUNTIME_DIR", runtime)
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap()
}

fn ctl(s: &Spawned, args: &[&str]) -> Output {
    ctl_in(&s.lock_dir, args)
}

fn stdout(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

fn stderr(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

fn json(o: &Output) -> serde_json::Value {
    assert_eq!(o.status.code(), Some(0), "{}", stderr(o));
    serde_json::from_slice(&o.stdout).unwrap()
}

fn socket(s: &Spawned, dev: &str) -> PathBuf {
    let tty = xcomm::ctl::path::tty_name(dev).unwrap();
    s.lock_dir.join("xcomm").join(format!("{tty}.sock"))
}

/// Waits for the terminal to have shown `bytes`: then the session has read
/// them, and they are in the ring.
fn shown(s: &Spawned, bytes: &[u8]) {
    read_until(s.host.master.as_fd(), bytes, T);
}

/// A socket file nothing listens on, as a killed session leaves. A child
/// another test forks holds the listener until it execs, so it is dead
/// only once a connect is refused.
fn dead_socket(path: &Path) {
    drop(std::os::unix::net::UnixListener::bind(path).unwrap());
    let until = std::time::Instant::now() + T;
    while UnixStream::connect(path).is_ok() {
        assert!(std::time::Instant::now() < until, "{path:?} still answers");
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn base64_matches_rfc_4648() {
    for (plain, coded) in [
        ("", ""),
        ("f", "Zg=="),
        ("fo", "Zm8="),
        ("foo", "Zm9v"),
        ("foob", "Zm9vYg=="),
        ("fooba", "Zm9vYmE="),
        ("foobar", "Zm9vYmFy"),
    ] {
        assert_eq!(base64_encode(plain.as_bytes()), coded);
        assert_eq!(base64_decode(coded).unwrap(), plain.as_bytes());
    }
    let all: Vec<u8> = (0..=255).collect();
    assert_eq!(base64_decode(&base64_encode(&all)).unwrap(), all);
    for bad in ["Zg", "Zg=", "Z===", "Zm9v\n", "Zh==", "Zm9=Zm9v", "!!!!"] {
        assert_eq!(base64_decode(bad), None, "{bad:?}");
    }
}

#[test]
fn status_reports_device_and_offset() {
    let (dev, s, path) = start(&[]);
    let v = json(&ctl(&s, &["--json", &path, "status"]));
    assert_eq!(v["ok"], true);
    assert_eq!(v["device"], path.as_str());
    assert_eq!(v["connected"], true);
    assert_eq!(v["offset"], 0);
    assert_eq!(v["ctl"], "rw");
    assert_eq!(v["pid"], s.child.id());
    assert_eq!(v["settings"]["baud"], 115200);
    write_all(dev.master.as_fd(), b"hello");
    shown(&s, b"hello");
    let v = json(&ctl(&s, &["--json", &path, "status"]));
    assert_eq!(v["offset"], 5);
    let text = stdout(&ctl(&s, &[&path, "status"]));
    assert!(text.contains("connected  true"), "{text}");
    assert!(text.contains("offset     5"), "{text}");
}

#[test]
fn send_reaches_device_with_cr() {
    let (dev, s, path) = start(&[]);
    write_all(dev.master.as_fd(), b"abc");
    shown(&s, b"abc");
    let o = ctl(&s, &[&path, "send", "ls -l"]);
    assert_eq!(o.status.code(), Some(0), "{}", stderr(&o));
    // The offset at send time: what the device had sent so far.
    assert_eq!(stdout(&o), "3\n");
    assert_eq!(read_until(dev.master.as_fd(), b"\r", T), b"ls -l\r");
}

#[test]
fn send_line_endings_and_hex() {
    let (dev, s, path) = start(&[]);
    for args in [
        &["send", "-n", "a"][..],
        &["send", "--lf", "b"],
        &["send", "--crlf", "c"],
        &["send", "--hex", "01 ff:7e"],
        &["send", "--cr", "end"],
    ] {
        let mut all = vec![path.as_str()];
        all.extend_from_slice(args);
        let o = ctl(&s, &all);
        assert_eq!(o.status.code(), Some(0), "{args:?}: {}", stderr(&o));
    }
    assert_eq!(
        read_until(dev.master.as_fd(), b"end\r", T),
        b"ab\nc\r\n\x01\xff~end\r"
    );
}

#[test]
fn send_file_larger_than_a_request() {
    let (dev, s, path) = start(&[]);
    let mut x: u64 = 0x9e37_79b9_7f4a_7c15;
    let data: Vec<u8> = (0..1_500_000)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x as u8
        })
        .collect();
    let file = s.lock_dir.join("payload.bin");
    std::fs::write(&file, &data).unwrap();
    let master = dev.master.try_clone().unwrap();
    let want = data.len();
    let reader = std::thread::spawn(move || {
        let mut got = Vec::with_capacity(want);
        let mut buf = vec![0u8; 65536];
        while got.len() < want {
            match rustix::io::read(&master, &mut buf) {
                Ok(n) if n > 0 => got.extend_from_slice(&buf[..n]),
                _ => break,
            }
        }
        got
    });
    let o = ctl(&s, &[&path, "send", "--file", file.to_str().unwrap()]);
    assert_eq!(o.status.code(), Some(0), "{}", stderr(&o));
    let got = reader.join().unwrap();
    assert!(got == data, "{} of {} bytes, or different", got.len(), want);
}

#[test]
fn read_last_and_since() {
    let (dev, s, path) = start(&[]);
    write_all(dev.master.as_fd(), b"0123456789");
    shown(&s, b"9");
    let o = ctl(&s, &[&path, "read", "--last", "4"]);
    assert_eq!(o.status.code(), Some(0), "{}", stderr(&o));
    assert_eq!(stdout(&o), "6789");
    assert_eq!(stderr(&o), "next=10\n");
    let o = ctl(&s, &[&path, "read", "--since", "2", "--max-bytes", "3"]);
    assert_eq!(stdout(&o), "234");
    assert_eq!(stderr(&o), "next=5\n");
    let o = ctl(&s, &[&path, "read"]);
    assert_eq!(stdout(&o), "0123456789");
    let v = json(&ctl(&s, &["--json", &path, "read", "--since", "8"]));
    assert_eq!(v["next"], 10);
    assert_eq!(
        base64_decode(v["data_b64"].as_str().unwrap()).unwrap(),
        b"89"
    );
}

#[test]
fn read_evicted_exits_6() {
    let (dev, s, path) = start(&["--ring-size", "4k"]);
    // From a thread: a macOS pty holds less than this, so the session
    // blocks on the terminal until the test drains it.
    let master = dev.master.try_clone().unwrap();
    let writer = std::thread::spawn(move || {
        write_all(master.as_fd(), &[b'x'; 5000]);
        write_all(master.as_fd(), b"END");
    });
    shown(&s, b"END");
    writer.join().unwrap();
    let o = ctl(&s, &[&path, "read", "--since", "0"]);
    assert_eq!(o.status.code(), Some(6));
    assert!(
        stderr(&o).contains("offset 0 evicted, oldest 907"),
        "{}",
        stderr(&o)
    );
    let o = ctl(&s, &[&path, "read", "--since", "0", "--from-oldest"]);
    assert_eq!(o.status.code(), Some(0), "{}", stderr(&o));
    assert_eq!(o.stdout.len(), 4096);
}

#[test]
fn read_strips_escape_sequences_when_piped() {
    let (dev, s, path) = start(&[]);
    write_all(dev.master.as_fd(), b"\x1b[31mred\x1b[0m done");
    shown(&s, b"done");
    assert_eq!(stdout(&ctl(&s, &[&path, "read"])), "red done");
    assert_eq!(
        stdout(&ctl(&s, &[&path, "read", "--color", "always"])),
        "\x1b[31mred\x1b[0m done"
    );
}

#[test]
fn ro_session_rejects_send_with_code_8() {
    let (_dev, s, path) = start(&["--ctl", "ro"]);
    let o = ctl(&s, &[&path, "send", "x"]);
    assert_eq!(o.status.code(), Some(8));
    assert!(stderr(&o).contains("read-only"), "{}", stderr(&o));
    assert_eq!(json(&ctl(&s, &["--json", &path, "status"]))["ctl"], "ro");
}

#[test]
fn off_session_has_no_socket() {
    let (_dev, s, path) = start(&["--ctl", "off"]);
    let o = ctl(&s, &[&path, "status"]);
    assert_eq!(o.status.code(), Some(3));
    assert!(stderr(&o).contains("no session"), "{}", stderr(&o));
}

#[test]
fn no_session_exits_3() {
    let dev = support::pty();
    let runtime = lock_dir("ctl-none");
    let o = ctl_in(&runtime, &[dev.slave_path.to_str().unwrap(), "status"]);
    assert_eq!(o.status.code(), Some(3));
    let _ = std::fs::remove_dir_all(&runtime);
}

#[test]
fn wrong_dir_mode_is_an_error() {
    use std::os::unix::fs::PermissionsExt;
    let dev = support::pty();
    let dir = lock_dir("ctl-mode");
    let sockets = dir.join("xcomm");
    std::fs::create_dir(&sockets).unwrap();
    std::fs::set_permissions(&sockets, std::fs::Permissions::from_mode(0o755)).unwrap();
    let mut s = spawn_xcomm(&[dev.slave_path.to_str().unwrap()], &dir);
    assert_eq!(s.wait_exit(T).code(), Some(1));
    let out = s.read_after_exit(b"700", T);
    let out = String::from_utf8_lossy(&out);
    assert!(
        out.contains(&format!(
            "socket directory {}: mode 755, not 700",
            sockets.display()
        )),
        "{out}"
    );
    // Nothing left behind: the lock went with the failed start.
    assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);
}

#[test]
fn stale_socket_is_replaced() {
    use std::os::unix::fs::DirBuilderExt;
    let dev = support::pty();
    let path = dev.slave_path.to_str().unwrap().to_string();
    let dir = lock_dir("ctl-stale");
    let sockets = dir.join("xcomm");
    std::fs::DirBuilder::new()
        .mode(0o700)
        .create(&sockets)
        .unwrap();
    let tty = xcomm::ctl::path::tty_name(&path).unwrap();
    dead_socket(&sockets.join(format!("{tty}.sock")));
    let s = spawn_xcomm(&[&path], &dir);
    read_until(s.host.master.as_fd(), b"\r\n", T);
    assert_eq!(
        json(&ctl(&s, &["--json", &path, "status"]))["connected"],
        true
    );
}

#[test]
fn the_socket_goes_with_the_session() {
    let (_dev, mut s, path) = start(&[]);
    let sock = socket(&s, &path);
    assert!(sock.exists());
    write_all(s.host.master.as_fd(), b"\x01x");
    assert!(s.wait_exit(T).success());
    assert!(!sock.exists());
}

fn raw_request(sock: &Path, line: &str) -> (BufReader<UnixStream>, serde_json::Value) {
    let mut stream = UnixStream::connect(sock).unwrap();
    stream.set_read_timeout(Some(T)).unwrap();
    stream.write_all(line.as_bytes()).unwrap();
    let mut reader = BufReader::new(stream);
    let mut reply = String::new();
    reader.read_line(&mut reply).unwrap();
    (reader, serde_json::from_str(&reply).unwrap())
}

#[test]
fn a_bad_request_gets_code_2() {
    let (_dev, s, path) = start(&[]);
    let sock = socket(&s, &path);
    let (_, v) = raw_request(&sock, "{\"cmd\":\"nonsense\"}\n");
    assert_eq!(v["ok"], false);
    assert_eq!(v["code"], 2);
    let (_, v) = raw_request(&sock, "{\"cmd\":\"send\",\"data_b64\":\"!!\"}\n");
    assert_eq!(v["code"], 2);
}

#[test]
fn subscribe_replays_then_streams() {
    let (dev, s, path) = start(&[]);
    write_all(dev.master.as_fd(), b"before ");
    shown(&s, b"before ");
    let (mut reader, v) = raw_request(
        &socket(&s, &path),
        "{\"cmd\":\"subscribe\",\"since\":0,\"from_oldest\":false}\n",
    );
    assert_eq!(v["ok"], true);
    assert_eq!(v["start"], 0);
    let mut got = vec![0u8; 7];
    reader.read_exact(&mut got).unwrap();
    assert_eq!(got, b"before ");
    write_all(dev.master.as_fd(), b"after");
    let mut got = vec![0u8; 5];
    reader.read_exact(&mut got).unwrap();
    assert_eq!(got, b"after");
}

#[test]
fn slow_subscriber_is_dropped_session_unaffected() {
    let (dev, s, path) = start(&[]);
    let (mut reader, v) = raw_request(&socket(&s, &path), "{\"cmd\":\"subscribe\"}\n");
    assert_eq!(v["ok"], true);
    const SIZE: usize = 16 << 20;
    let master = dev.master.try_clone().unwrap();
    let writer = std::thread::spawn(move || {
        let chunk = vec![b'z'; 65536];
        let mut sent = 0;
        while sent < SIZE {
            write_all(master.as_fd(), &chunk);
            sent += chunk.len();
        }
        write_all(master.as_fd(), b"|END|");
    });
    // The terminal gets every byte while the subscriber reads none.
    let mut total = 0usize;
    let mut buf = vec![0u8; 65536];
    let mut tail: Vec<u8> = Vec::new();
    while !tail.ends_with(b"|END|") {
        let n = rustix::io::read(&s.host.master, &mut buf).unwrap();
        assert!(n > 0);
        total += n;
        tail.extend_from_slice(&buf[..n]);
        let keep = tail.len().saturating_sub(5);
        tail.drain(..keep);
    }
    writer.join().unwrap();
    assert_eq!(total, SIZE + 5);
    // It was dropped: it reads to the end of the stream, which closes.
    let mut rest = Vec::new();
    reader.read_to_end(&mut rest).unwrap();
    assert!(rest.len() < SIZE, "the subscriber got everything");
}

#[test]
fn sessions_lists_running() {
    let (_dev, s, path) = start(&[]);
    // Others' sessions in /tmp/xcomm-$UID may be listed too.
    let v = json(&ctl(&s, &["--json", "sessions"]));
    let ours: Vec<_> = v
        .as_array()
        .unwrap()
        .iter()
        .filter(|x| x["pid"] == s.child.id())
        .collect();
    assert_eq!(ours.len(), 1, "{v}");
    assert_eq!(ours[0]["device"], path.as_str());
    let text = stdout(&ctl(&s, &["sessions"]));
    assert!(text.contains(&path) && text.contains("connected"), "{text}");
}

#[test]
fn a_disconnected_session_answers_and_refuses_sends() {
    let dir = std::env::temp_dir().join(format!("xcomm-ctl-gone-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let d1 = support::pty();
    let link = dir.join("dev");
    std::os::unix::fs::symlink(&d1.slave_path, &link).unwrap();
    let link_s = link.to_str().unwrap().to_string();
    let s = spawn_xcomm(&[&link_s], &lock_dir("ctl-gone"));
    read_until(s.host.master.as_fd(), b"\r\n", T);
    drop(d1);
    read_until(s.host.master.as_fd(), b"waiting", T);
    // The link now leads nowhere; its text still names the session's tty.
    let v = json(&ctl(&s, &["--json", &link_s, "status"]));
    assert_eq!(v["connected"], false);
    assert!(v["settings"].is_null());
    let o = ctl(&s, &[&link_s, "send", "x"]);
    assert_eq!(o.status.code(), Some(4), "{}", stderr(&o));
    // udev removes the link outright: the session is asked by the name it
    // was started with.
    std::fs::remove_file(&link).unwrap();
    let v = json(&ctl(&s, &["--json", &link_s, "status"]));
    assert_eq!(v["device"], link_s.as_str());
    assert_eq!(v["connected"], false);
    let _ = std::fs::remove_dir_all(&dir);
}

/// Reads reply lines until the session closes the connection. Closed with
/// bytes of ours unread, it reads as a reset once its replies are read.
fn replies(reader: &mut BufReader<UnixStream>) -> Vec<serde_json::Value> {
    let mut out = Vec::new();
    let mut line = String::new();
    loop {
        match reader.read_line(&mut line) {
            Ok(0) => break,
            Ok(_) => out.push(serde_json::from_str(&line).unwrap()),
            Err(e) if e.kind() == std::io::ErrorKind::ConnectionReset => break,
            Err(e) => panic!("{e}"),
        }
        line.clear();
    }
    out
}

#[test]
fn pipelined_requests_are_all_answered_in_order() {
    let (dev, s, path) = start(&[]);
    write_all(dev.master.as_fd(), b"abc");
    shown(&s, b"abc");
    let mut stream = UnixStream::connect(socket(&s, &path)).unwrap();
    stream.set_read_timeout(Some(T)).unwrap();
    stream
        .write_all(b"{\"cmd\":\"status\"}\n{\"cmd\":\"read\"}\n{\"cmd\":\"status\"}\n")
        .unwrap();
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut line = String::new();
    for want in ["offset", "data_b64", "offset"] {
        line.clear();
        reader.read_line(&mut line).unwrap();
        let v: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(v["ok"], true, "{line}");
        assert!(!v[want].is_null(), "{line}");
    }
}

#[test]
fn a_half_closed_client_gets_every_reply_then_eof() {
    let (_dev, s, path) = start(&[]);
    let mut stream = UnixStream::connect(socket(&s, &path)).unwrap();
    stream.set_read_timeout(Some(T)).unwrap();
    stream
        .write_all(b"{\"cmd\":\"status\"}\n{\"cmd\":\"read\"}\n")
        .unwrap();
    stream.shutdown(std::net::Shutdown::Write).unwrap();
    let got = replies(&mut BufReader::new(stream));
    assert_eq!(got.len(), 2, "{got:?}");
    assert!(got.iter().all(|v| v["ok"] == true), "{got:?}");
}

#[test]
fn an_unended_line_after_a_request_is_still_too_long() {
    let (_dev, s, path) = start(&[]);
    let stream = UnixStream::connect(socket(&s, &path)).unwrap();
    stream.set_read_timeout(Some(T)).unwrap();
    let mut writer = stream.try_clone().unwrap();
    let feeder = std::thread::spawn(move || {
        let _ = writer.write_all(b"{\"cmd\":\"status\"}\n");
        let _ = writer.write_all(&vec![b'a'; (1 << 20) + 1]);
    });
    let got = replies(&mut BufReader::new(stream));
    feeder.join().unwrap();
    assert_eq!(got.len(), 2, "{got:?}");
    assert_eq!(got[0]["ok"], true);
    assert_eq!(got[1]["code"], 2);
    assert_eq!(got[1]["error"], "request too long");
}

#[test]
fn a_client_past_the_limit_is_told_why() {
    let (_dev, s, path) = start(&[]);
    let sock = socket(&s, &path);
    let held: Vec<UnixStream> = (0..xcomm::ctl::server::MAX_CLIENTS)
        .map(|_| UnixStream::connect(&sock).unwrap())
        .collect();
    // Each held one answers: all are being served.
    for c in &held {
        let mut c = c.try_clone().unwrap();
        c.set_read_timeout(Some(T)).unwrap();
        c.write_all(b"{\"cmd\":\"status\"}\n").unwrap();
        let mut line = String::new();
        BufReader::new(c).read_line(&mut line).unwrap();
        assert!(line.contains("\"ok\":true"), "{line}");
    }
    let extra = UnixStream::connect(&sock).unwrap();
    extra.set_read_timeout(Some(T)).unwrap();
    let got = replies(&mut BufReader::new(extra));
    assert_eq!(got.len(), 1, "{got:?}");
    assert_eq!(got[0]["code"], 1);
    assert!(
        got[0]["error"].as_str().unwrap().contains("clients"),
        "{got:?}"
    );
    let o = ctl(&s, &[&path, "status"]);
    assert_eq!(o.status.code(), Some(1), "{}", stderr(&o));
    drop(held);
}

#[test]
fn keys_still_work_through_a_long_send() {
    let (dev, s, path) = start(&[]);
    // The device takes 1 KiB per 10 ms: a 1 MiB send lasts ~10 s.
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let drainer = {
        let master = dev.master.try_clone().unwrap();
        // Not blocking: once the session is gone nothing more comes, and
        // the stop flag must still be seen.
        let fl = rustix::fs::fcntl_getfl(&master).unwrap();
        rustix::fs::fcntl_setfl(&master, fl | rustix::fs::OFlags::NONBLOCK).unwrap();
        let stop = stop.clone();
        std::thread::spawn(move || {
            let mut buf = [0u8; 1024];
            while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                let _ = rustix::io::read(master.as_fd(), &mut buf);
                std::thread::sleep(Duration::from_millis(10));
            }
        })
    };
    let file = s.lock_dir.join("big");
    std::fs::write(&file, vec![b'k'; 1 << 20]).unwrap();
    let mut client = std::process::Command::new(env!("CARGO_BIN_EXE_xcomm"))
        .args(["ctl", &path, "send", "--file", file.to_str().unwrap()])
        .env("XDG_RUNTIME_DIR", &s.lock_dir)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    std::thread::sleep(Duration::from_millis(500));
    let asked = std::time::Instant::now();
    let mut s = s;
    write_all(s.host.master.as_fd(), b"\x01x");
    s.wait_exit(Duration::from_secs(10));
    let took = asked.elapsed();
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    drainer.join().unwrap();
    let _ = client.kill();
    let _ = client.wait();
    assert!(took < Duration::from_secs(4), "quit took {took:?}");
}

#[test]
fn the_runtime_dir_counts_only_when_it_is_the_users() {
    use xcomm::ctl::path::socket_dir;
    let me = rustix::process::geteuid().as_raw();
    let fallback = PathBuf::from(format!("/tmp/xcomm-{me}"));
    let own = lock_dir("ctl-runtime");
    let own_s = own.to_str().unwrap().to_string();
    let with = |v: &str| {
        let v = v.to_string();
        socket_dir(&move |k: &str| (k == "XDG_RUNTIME_DIR").then(|| v.clone()))
    };
    assert_eq!(with(&own_s), own.join("xcomm"));
    assert_eq!(with(""), fallback);
    assert_eq!(with(&format!("{own_s}/missing")), fallback);
    if me != 0 {
        assert_eq!(with("/"), fallback);
    }
    let _ = std::fs::remove_dir_all(&own);
}

#[test]
fn a_send_slower_than_the_reply_timeout_completes() {
    let (dev, s, path) = start(&["-b", "2400"]);
    // ~12 KiB/s: what the queue and the pty do not hold takes over 10 s.
    let size = 200 << 10;
    let got = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let drainer = {
        let master = dev.master.try_clone().unwrap();
        let got = got.clone();
        std::thread::spawn(move || {
            let mut buf = [0u8; 1024];
            while got.load(std::sync::atomic::Ordering::Relaxed) < size {
                if let Ok(n) = rustix::io::read(master.as_fd(), &mut buf) {
                    got.fetch_add(n, std::sync::atomic::Ordering::Relaxed);
                }
                std::thread::sleep(Duration::from_millis(80));
            }
        })
    };
    let file = s.lock_dir.join("slow");
    std::fs::write(&file, vec![b's'; size]).unwrap();
    let started = std::time::Instant::now();
    let o = ctl(&s, &[&path, "send", "--file", file.to_str().unwrap()]);
    assert_eq!(o.status.code(), Some(0), "{}", stderr(&o));
    assert!(
        started.elapsed() > Duration::from_secs(10),
        "too fast to prove"
    );
    drainer.join().unwrap();
    assert_eq!(got.load(std::sync::atomic::Ordering::Relaxed), size);
}

#[test]
fn closing_the_write_side_ends_a_subscription_at_once() {
    let (_dev, s, path) = start(&[]);
    let stream = UnixStream::connect(socket(&s, &path)).unwrap();
    stream.set_read_timeout(Some(T)).unwrap();
    (&stream).write_all(b"{\"cmd\":\"subscribe\"}\n").unwrap();
    stream.shutdown(std::net::Shutdown::Write).unwrap();
    let mut rest = Vec::new();
    // EOF, not the read timeout: the session closed it.
    BufReader::new(stream).read_to_end(&mut rest).unwrap();
    let o = ctl(&s, &[&path, "status"]);
    assert_eq!(o.status.code(), Some(0), "{}", stderr(&o));
}

#[test]
fn a_name_that_leads_to_another_device_is_not_matched_by_name() {
    let dir = std::env::temp_dir().join(format!("xcomm-ctl-moved-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let (d1, d2) = (support::pty(), support::pty());
    let link = dir.join("dev");
    std::os::unix::fs::symlink(&d1.slave_path, &link).unwrap();
    let link_s = link.to_str().unwrap().to_string();
    let s = spawn_xcomm(&[&link_s], &lock_dir("ctl-moved"));
    read_until(s.host.master.as_fd(), b"\r\n", T);
    // The name now leads to a device with no session.
    std::fs::remove_file(&link).unwrap();
    std::os::unix::fs::symlink(&d2.slave_path, &link).unwrap();
    let o = ctl(&s, &[&link_s, "status"]);
    assert_eq!(o.status.code(), Some(3), "{}", stdout(&o));
    drop(d1);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_session_in_the_fallback_dir_is_found_from_a_fresh_login() {
    let dev = support::pty();
    let path = dev.slave_path.to_str().unwrap().to_string();
    let locks = lock_dir("ctl-fallback");
    // The session's XDG_RUNTIME_DIR is stale: it uses /tmp/xcomm-$UID.
    let stale = locks.join("gone");
    let s = support::spawn_with_env(
        env!("CARGO_BIN_EXE_xcomm"),
        &[&path],
        &locks,
        &[("XDG_RUNTIME_DIR", stale.to_str().unwrap())],
    );
    read_until(s.host.master.as_fd(), b"\r\n", T);
    // The client's is valid, and has only a dead socket for it, left by a
    // killed session.
    let client_dir = locks.join("xcomm");
    std::fs::DirBuilder::new()
        .mode(0o700)
        .create(&client_dir)
        .unwrap();
    let tty = xcomm::ctl::path::tty_name(&path).unwrap();
    dead_socket(&client_dir.join(format!("{tty}.sock")));
    let v = json(&ctl_in(&locks, &["--json", &path, "status"]));
    assert_eq!(v["device"], path.as_str());
    let v = json(&ctl_in(&locks, &["--json", "sessions"]));
    assert!(
        v.as_array()
            .unwrap()
            .iter()
            .any(|x| x["pid"] == s.child.id()),
        "{v}"
    );
    // Quit, so the socket in the shared /tmp directory goes with it.
    let mut s = s;
    write_all(s.host.master.as_fd(), b"\x01x");
    s.wait_exit(T);
    let sock = xcomm::ctl::path::fallback_dir().join(format!(
        "{}.sock",
        xcomm::ctl::path::tty_name(&path).unwrap()
    ));
    assert!(std::fs::symlink_metadata(&sock).is_err(), "{sock:?} left");
}

#[test]
fn rename_replaces_a_socket_a_killed_session_left() {
    use xcomm::ctl::{CtlMode, server::CtlServer};
    let dir = lock_dir("ctl-rename");
    let mut server = CtlServer::bind(&dir, "ttyA", CtlMode::ReadWrite).unwrap();
    let dead = dir.join("ttyB.sock");
    dead_socket(&dead);
    server.rename("ttyB").unwrap();
    assert_eq!(server.path(), dead.as_path());
    assert!(std::fs::symlink_metadata(dir.join("ttyA.sock")).is_err());
    UnixStream::connect(&dead).unwrap();
    // A live one is not taken over.
    let live = CtlServer::bind(&dir, "ttyC", CtlMode::ReadWrite).unwrap();
    assert!(server.rename("ttyC").is_err());
    assert_eq!(server.path(), dead.as_path());
    drop(live);
    drop(server);
    let _ = std::fs::remove_dir_all(&dir);
}
