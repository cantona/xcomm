// SPDX-FileCopyrightText: 2026 Su Kang Yin <paradyse@gmail.com>
// SPDX-License-Identifier: GPL-2.0-only

mod support;

use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::DirBuilderExt;
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::Value;
use support::lock_dir;
use xcomm::list::PortInfo;
use xcomm::watch::{WatchConfig, diff, ignored, load_config, parse_config};

const T: Duration = Duration::from_secs(5);

#[test]
fn the_config() {
    assert_eq!(parse_config("").unwrap(), WatchConfig::default());
    assert!(WatchConfig::default().notify);
    let cfg = parse_config(
        "# xcomm\nnotify = false  # quiet\nignore = [\n  \"0403:6001\", # FTDI\n  'usb-Silicon_*',\n]\n",
    )
    .unwrap();
    assert!(!cfg.notify);
    assert_eq!(cfg.ignore, ["0403:6001", "usb-Silicon_*"]);
    assert_eq!(
        parse_config("ignore = [\"a\\\"b\"]").unwrap().ignore,
        ["a\"b"]
    );
    for (text, want) in [
        (
            "notify = true\nnotfy = false\n",
            "line 2: unknown key 'notfy'",
        ),
        ("notify = yes", "line 1: notify is true or false"),
        ("notify = truest", "line 1: notify is true or false"),
        ("\n\nignore = \"x\"", "line 3: ignore is a list of strings"),
        (
            "ignore = [\"a\",\n 3]",
            "line 2: ignore is a list of strings",
        ),
        ("ignore = [\"a\"", "ignore is a list of strings"),
        ("notify = true true", "line 1: unexpected text"),
        (
            "notify = true\nnotify = false",
            "line 2: notify is given twice",
        ),
        ("[watch]\nnotify = true", "line 1: tables are not used"),
        ("= true", "line 1: expected a key"),
        ("notify true", "line 1: expected = after notify"),
    ] {
        let e = parse_config(text).unwrap_err();
        assert!(e.contains(want), "{text:?}: {e}");
    }
    let dir = lock_dir("watch-cfg");
    assert_eq!(
        load_config(&dir.join("none.toml")).unwrap(),
        WatchConfig::default()
    );
    std::fs::write(dir.join("bad.toml"), "notify = true\nbeep = 1\n").unwrap();
    let e = load_config(&dir.join("bad.toml")).unwrap_err().0;
    assert!(e.contains("bad.toml: line 2: unknown key 'beep'"), "{e}");
    let _ = std::fs::remove_dir_all(&dir);
}

fn port(tty: &str, vid_pid: Option<&str>, by_id: Option<&str>) -> PortInfo {
    PortInfo {
        tty: tty.into(),
        path: format!("/dev/{tty}"),
        by_id: by_id.map(|b| format!("/dev/serial/by-id/{b}")),
        vid_pid: vid_pid.map(str::to_string),
        product: None,
        serial: None,
        lock_pid: None,
        lock_error: None,
        herdr_pane: None,
    }
}

#[test]
fn diff_and_ignore() {
    let a = port(
        "ttyUSB0",
        Some("0403:6001"),
        Some("usb-FTDI_FT232R_A1-if00-port0"),
    );
    let b = port(
        "ttyUSB1",
        Some("10c4:ea60"),
        Some("usb-Silicon_Labs_CP2102-if00-port0"),
    );
    let c = port("ttyACM0", None, None);
    let (added, removed) = diff(&[a.clone(), b.clone()], &[b.clone(), c.clone()]);
    assert_eq!(added, std::slice::from_ref(&c));
    assert_eq!(removed, std::slice::from_ref(&a));
    let one = std::slice::from_ref(&a);
    assert_eq!(diff(one, one), (vec![], vec![]));
    // The by-id link comes after the node: the same port all along.
    let early = port("ttyUSB0", Some("0403:6001"), None);
    assert_eq!(diff(&[early], one), (vec![], vec![]));
    // Another adapter under the same name between two looks.
    let other = port(
        "ttyUSB0",
        Some("10c4:ea60"),
        Some("usb-Silicon_Labs_CP2102-if00-port0"),
    );
    let (added, removed) = diff(one, std::slice::from_ref(&other));
    assert_eq!((added.len(), removed.len()), (1, 1));
    let cfg = WatchConfig {
        notify: true,
        ignore: vec!["0403:6001".into(), "usb-Silicon_*".into()],
    };
    assert!(ignored(&cfg, &a));
    assert!(ignored(&cfg, &b));
    assert!(!ignored(&cfg, &c));
    let cfg = WatchConfig {
        notify: true,
        ignore: vec!["*FT232R_A?-if00*".into(), "0403:60".into()],
    };
    assert!(ignored(&cfg, &a));
    assert!(!ignored(&cfg, &b));
}

/// A herdr server that records notifications.
struct Fake {
    seen: Arc<Mutex<Vec<Value>>>,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Fake {
    fn new(dir: &Path) -> Fake {
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
                    let reply = serde_json::json!({ "id": req["id"], "result": { "type": "ok" } });
                    seen.lock().unwrap().push(req);
                    let _ = (&stream).write_all(format!("{reply}\n").as_bytes());
                }
            })
        };
        Fake {
            seen,
            stop,
            thread: Some(thread),
        }
    }

    fn bodies(&self) -> Vec<(String, String)> {
        self.seen
            .lock()
            .unwrap()
            .iter()
            .map(|v| {
                (
                    v["params"]["title"].as_str().unwrap_or("").to_string(),
                    v["params"]["body"].as_str().unwrap_or("").to_string(),
                )
            })
            .collect()
    }

    fn wait_count(&self, n: usize) {
        let until = Instant::now() + T;
        while self.seen.lock().unwrap().len() < n {
            assert!(Instant::now() < until, "got {:?}", self.bodies());
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
    }
}

struct Env {
    dir: PathBuf,
    vars: Vec<(&'static str, String)>,
}

impl Env {
    /// Where the watcher of this env's herdr server keeps its files.
    fn state(&self) -> std::path::PathBuf {
        let get = |k: &str| {
            self.vars
                .iter()
                .find(|(n, _)| *n == k)
                .map(|(_, v)| v.clone())
        };
        xcomm::watch::state_dir(&get).unwrap()
    }

    fn new() -> Env {
        let dir = lock_dir("watch");
        for d in ["state", "config", "root/dev", "locks", "run"] {
            std::fs::create_dir_all(dir.join(d)).unwrap();
        }
        let vars = vec![
            ("HERDR_PLUGIN_STATE_DIR", dir.join("state")),
            ("HERDR_PLUGIN_CONFIG_DIR", dir.join("config")),
            ("HERDR_SOCKET_PATH", dir.join("herdr.sock")),
            ("XCOMM_WATCH_ROOT", dir.join("root")),
            ("XCOMM_LOCK_DIR", dir.join("locks")),
            ("XDG_RUNTIME_DIR", dir.join("run")),
        ]
        .into_iter()
        .map(|(k, v)| (k, v.to_str().unwrap().to_string()))
        .collect();
        Env { dir, vars }
    }

    fn watch(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_xcomm"))
            .arg("watch")
            .args(args)
            .env_remove("HERDR_PANE_ID")
            .envs(self.vars.iter().map(|(k, v)| (*k, v.as_str())))
            .output()
            .unwrap()
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        let _ = self.watch(&["stop"]);
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn out(o: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&o.stdout),
        String::from_utf8_lossy(&o.stderr)
    )
}

/// Waits for the watcher's log to have `n` lines containing `text`.
fn wait_log(log: &Path, text: &str, n: usize) {
    let until = Instant::now() + T;
    while std::fs::read_to_string(log).map_or(0, |l| l.matches(text).count()) < n {
        assert!(Instant::now() < until, "{:?}", std::fs::read_to_string(log));
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// A session socket for `tty` that waits for its device, in `pane`.
fn waiting_session(
    dir: &Path,
    tty: &str,
    pane: &str,
) -> (Arc<AtomicBool>, std::thread::JoinHandle<()>) {
    let _ = std::fs::DirBuilder::new()
        .mode(0o700)
        .create(dir.join("run/xcomm"));
    let listener = UnixListener::bind(dir.join(format!("run/xcomm/{tty}.sock"))).unwrap();
    listener.set_nonblocking(true).unwrap();
    let reply = format!(
        "{{\"ok\":true,\"connected\":false,\"tty\":\"{tty}\",\"herdr_pane\":\"{pane}\"}}\n"
    );
    let stop = Arc::new(AtomicBool::new(false));
    let flag = stop.clone();
    let t = std::thread::spawn(move || {
        while !flag.load(Ordering::Relaxed) {
            let Ok((stream, _)) = listener.accept() else {
                std::thread::sleep(Duration::from_millis(5));
                continue;
            };
            stream.set_nonblocking(false).unwrap();
            let mut line = String::new();
            let _ = BufReader::new(&stream).read_line(&mut line);
            let _ = (&stream).write_all(reply.as_bytes());
        }
    });
    (stop, t)
}

#[test]
fn start_status_stop_and_notifications() {
    let env = Env::new();
    let fake = Fake::new(&env.dir);
    std::fs::write(env.dir.join("root/dev/ttyUSB0"), b"").unwrap();
    let o = env.watch(&["status"]);
    assert!(out(&o).starts_with("not running"), "{}", out(&o));
    let o = env.watch(&["--daemon"]);
    assert_eq!(o.status.code(), Some(0), "{}", out(&o));
    let started = out(&o);
    assert!(started.starts_with("started (pid "), "{started}");
    // The lock came with the watcher at once: a second start finds it.
    let o = env.watch(&["start"]);
    assert!(out(&o).starts_with("already running"), "{}", out(&o));
    let log = env.state().join("watch.log");
    wait_log(&log, "watching 1 ports", 1);
    let o = env.watch(&["status"]);
    let status = out(&o);
    let pid = started
        .trim()
        .trim_start_matches("started (pid ")
        .trim_end_matches(')');
    assert!(
        status.starts_with(&format!("running (pid {pid})")),
        "{status}"
    );
    assert!(status.contains("config.toml (none: defaults)"), "{status}");
    // A port comes, made in steps as udev does: one notification.
    std::fs::write(env.dir.join("root/dev/ttyACM3"), b"").unwrap();
    std::fs::write(env.dir.join("root/dev/other.tmp"), b"").unwrap();
    fake.wait_count(1);
    // It goes while a session has it: the session, waiting for it now,
    // names its pane.
    let session = waiting_session(&env.dir, "ttyACM3", "w1:p9");
    std::fs::remove_file(env.dir.join("root/dev/ttyACM3")).unwrap();
    fake.wait_count(2);
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!(
        fake.bodies(),
        [
            ("Serial added".to_string(), "ttyACM3".to_string()),
            (
                "Serial removed".to_string(),
                "ttyACM3, open in pane w1:p9".to_string()
            ),
        ]
    );
    session.0.store(true, Ordering::Relaxed);
    session.1.join().unwrap();
    let o = env.watch(&["stop"]);
    assert!(out(&o).starts_with("stopped (pid "), "{}", out(&o));
    let o = env.watch(&["status"]);
    let status = out(&o);
    assert!(status.starts_with("not running"), "{status}");
    assert!(status.contains("Serial removed: ttyACM3"), "{status}");
    // Emptied: no pid is left to name another process later.
    let pid_file = env.state().join("watch.pid");
    assert_eq!(std::fs::read_to_string(&pid_file).unwrap(), "");
    // A pid in the file with no watcher holding the lock is not believed.
    std::fs::write(&pid_file, "1\n").unwrap();
    assert!(out(&env.watch(&["status"])).starts_with("not running"));
    assert!(out(&env.watch(&["stop"])).starts_with("not running"));
}

#[test]
fn two_herdr_servers_each_get_their_own_watcher() {
    // herdr gives every server the same plugin state directory, and every
    // server's socket the same file name (herdr.sock).
    let a = Env::new();
    let mut b = Env::new();
    let shared = a.state().parent().unwrap().to_str().unwrap().to_string();
    for (k, v) in &mut b.vars {
        if *k == "HERDR_PLUGIN_STATE_DIR" {
            *v = shared.clone();
        }
    }
    let _servers = (Fake::new(&a.dir), Fake::new(&b.dir));
    assert_ne!(a.state(), b.state());
    for env in [&a, &b] {
        let o = env.watch(&["start"]);
        assert!(out(&o).starts_with("started (pid "), "{}", out(&o));
    }
    for env in [&a, &b] {
        let o = env.watch(&["status"]);
        assert!(out(&o).starts_with("running (pid "), "{}", out(&o));
    }
    // An older xcomm's watcher whose server cannot be told (pid 1's
    // environment is not this user's to read; macOS shows no other
    // process's): taken as this server's, as up to v0.1.2, and said so.
    let old = Path::new(&shared).join("herdr.sock");
    std::fs::create_dir_all(&old).unwrap();
    let held = std::fs::OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(old.join("watch.pid"))
        .unwrap();
    rustix::fs::flock(&held, rustix::fs::FlockOperation::NonBlockingLockExclusive)
        .unwrap();
    (&held).write_all(b"1\n").unwrap();
    let status = out(&a.watch(&["status"]));
    assert!(
        status.starts_with(
            "running (pid 1, started by an older xcomm, maybe for another herdr server)"
        ),
        "{status}"
    );
    drop(held);
}

/// A watcher as xcomm up to v0.1.2 left it: in `<state>/herdr.sock`, its
/// lock held by a process started with `socket` as HERDR_SOCKET_PATH.
fn old_watcher(state: &Path, socket: &str) -> std::process::Child {
    let old = state.join("herdr.sock");
    std::fs::create_dir_all(&old).unwrap();
    let path = old.join("watch.pid");
    std::fs::write(&path, "").unwrap();
    let c_path = std::ffi::CString::new(path.to_str().unwrap()).unwrap();
    let mut cmd = Command::new("sleep");
    cmd.arg("30").env("HERDR_SOCKET_PATH", socket);
    // The child takes the lock itself, on an fd it keeps across exec: an
    // inheritable fd made here could leak into another test's child and
    // hold the lock after this one ends.
    // SAFETY: open and flock only, on a path built before the fork.
    unsafe {
        std::os::unix::process::CommandExt::pre_exec(&mut cmd, move || {
            let fd = rustix::fs::open(
                c_path.as_c_str(),
                rustix::fs::OFlags::RDWR,
                rustix::fs::Mode::empty(),
            )?;
            rustix::fs::flock(&fd, rustix::fs::FlockOperation::NonBlockingLockExclusive)?;
            std::mem::forget(fd);
            Ok(())
        });
    }
    let child = cmd.spawn().unwrap();
    std::fs::write(&path, format!("{}\n", child.id())).unwrap();
    child
}

#[test]
fn an_older_xcomms_watcher_is_this_servers_or_left_alone() {
    let env = Env::new();
    let _server = Fake::new(&env.dir);
    let state = env.state().parent().unwrap().to_path_buf();
    let socket = env
        .vars
        .iter()
        .find(|(k, _)| *k == "HERDR_SOCKET_PATH")
        .map(|(_, v)| v.clone())
        .unwrap();
    // This server's (an upgrade runs start on a live server): it is the
    // watcher, and no second one starts.
    let mut old = old_watcher(&state, &socket);
    let pid = old.id();
    let o = env.watch(&["start"]);
    assert!(
        out(&o).starts_with(&format!(
            "already running (pid {pid}, started by an older xcomm"
        )),
        "{}",
        out(&o)
    );
    let status = out(&env.watch(&["status"]));
    assert!(
        status.starts_with(&format!("running (pid {pid}, started by an older xcomm")),
        "{status}"
    );
    let o = env.watch(&["stop"]);
    assert!(
        out(&o).starts_with(&format!("stopped (pid {pid}), started by an older xcomm")),
        "{}",
        out(&o)
    );
    let _ = old.wait();
    // Gone: a watcher starts. Its directory stays (an old xcomm may be
    // taking its lock); doctor does not report it.
    let o = env.watch(&["start"]);
    assert!(out(&o).starts_with("started (pid "), "{}", out(&o));
    assert!(xcomm::watch::is_old_state_dir(&state.join("herdr.sock")));
    assert!(!xcomm::watch::is_old_state_dir(&env.state()));
    assert!(out(&env.watch(&["stop"])).starts_with("stopped (pid "));
    // Another server's: left alone, and not reported. Only where another
    // process's environment can be read: macOS shows none.
    if !cfg!(target_os = "linux") {
        return;
    }
    let mut other = old_watcher(&state, "/elsewhere/herdr.sock");
    let status = out(&env.watch(&["status"]));
    assert!(status.starts_with("not running"), "{status}");
    assert!(!status.contains("older"), "{status}");
    let o = env.watch(&["start"]);
    assert!(out(&o).starts_with("started (pid "), "{}", out(&o));
    assert!(out(&env.watch(&["stop"])).starts_with("stopped (pid "));
    assert!(state.join("herdr.sock/watch.pid").exists());
    other.kill().unwrap();
    let _ = other.wait();
}

#[test]
fn without_a_herdr_socket_it_does_not_start() {
    let env = Env::new();
    let o = Command::new(env!("CARGO_BIN_EXE_xcomm"))
        .args(["watch", "start"])
        .env_remove("HERDR_SOCKET_PATH")
        .envs(
            env.vars
                .iter()
                .filter(|(k, _)| *k != "HERDR_SOCKET_PATH")
                .map(|(k, v)| (*k, v.as_str())),
        )
        .output()
        .unwrap();
    assert_eq!(o.status.code(), Some(1));
    assert!(
        out(&o).contains("HERDR_SOCKET_PATH is not set"),
        "{}",
        out(&o)
    );
}

#[test]
fn ignored_and_quiet_ports_are_logged_not_shown() {
    let env = Env::new();
    let fake = Fake::new(&env.dir);
    std::fs::write(
        env.dir.join("config/config.toml"),
        "ignore = [\"ttyNOPE*\", \"*by-id-only*\"]\n",
    )
    .unwrap();
    let o = env.watch(&["start"]);
    assert_eq!(o.status.code(), Some(0), "{}", out(&o));
    let log = env.state().join("watch.log");
    let until = Instant::now() + T;
    while !std::fs::read_to_string(&log).is_ok_and(|l| l.contains("watching")) {
        assert!(Instant::now() < until);
        std::thread::sleep(Duration::from_millis(20));
    }
    // No VID:PID or by-id in a fake tree: not ignored, shown.
    std::fs::write(env.dir.join("root/dev/ttyUSB5"), b"").unwrap();
    fake.wait_count(1);
    assert_eq!(fake.bodies()[0].1, "ttyUSB5");
    env.watch(&["stop"]);
    // notify = false: logged only.
    std::fs::write(env.dir.join("config/config.toml"), "notify = false\n").unwrap();
    env.watch(&["start"]);
    let until = Instant::now() + T;
    while std::fs::read_to_string(&log).map_or(0, |l| l.matches("watching").count()) < 2 {
        assert!(Instant::now() < until);
        std::thread::sleep(Duration::from_millis(20));
    }
    std::fs::write(env.dir.join("root/dev/ttyUSB6"), b"").unwrap();
    let until = Instant::now() + T;
    while !std::fs::read_to_string(&log).is_ok_and(|l| l.contains("ttyUSB6 (not shown)"))
    {
        assert!(
            Instant::now() < until,
            "{:?}",
            std::fs::read_to_string(&log)
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(fake.bodies().len(), 1);
}

#[test]
fn a_bad_config_stops_the_start() {
    let env = Env::new();
    std::fs::write(env.dir.join("config/config.toml"), "notify = maybe\n").unwrap();
    let o = env.watch(&["start"]);
    assert_eq!(o.status.code(), Some(1));
    assert!(
        out(&o).contains("config.toml: line 1: notify is true or false"),
        "{}",
        out(&o)
    );
    let o = env.watch(&["status"]);
    assert!(out(&o).starts_with("not running"), "{}", out(&o));
    assert!(out(&o).contains("(has an error)"), "{}", out(&o));
}

#[test]
fn relative_paths_are_refused() {
    let o = Command::new(env!("CARGO_BIN_EXE_xcomm"))
        .args(["watch", "status"])
        .env("HERDR_PLUGIN_STATE_DIR", "state")
        .output()
        .unwrap();
    assert_eq!(o.status.code(), Some(1));
    assert!(
        out(&o).contains("HERDR_PLUGIN_STATE_DIR must be an absolute path"),
        "{}",
        out(&o)
    );
}

#[test]
fn without_herdr_state_dir_it_says_so() {
    let o = Command::new(env!("CARGO_BIN_EXE_xcomm"))
        .args(["watch", "status"])
        .env_remove("HERDR_PLUGIN_STATE_DIR")
        .output()
        .unwrap();
    assert_eq!(o.status.code(), Some(1));
    assert!(
        out(&o).contains("HERDR_PLUGIN_STATE_DIR is not set"),
        "{}",
        out(&o)
    );
    let o = Command::new(env!("CARGO_BIN_EXE_xcomm"))
        .args(["watch", "frob"])
        .output()
        .unwrap();
    assert_eq!(o.status.code(), Some(2));
}

#[test]
fn a_late_by_id_link_still_counts_for_ignore() {
    let env = Env::new();
    let fake = Fake::new(&env.dir);
    std::fs::create_dir_all(env.dir.join("root/dev/serial/by-id")).unwrap();
    std::fs::write(
        env.dir.join("config/config.toml"),
        "ignore = [\"usb-FTDI_*\"]\n",
    )
    .unwrap();
    let o = env.watch(&["start"]);
    assert_eq!(o.status.code(), Some(0), "{}", out(&o));
    let log = env.state().join("watch.log");
    wait_log(&log, "watching", 1);
    // The node first, its link after the debounce, as a slow udev does.
    std::fs::write(env.dir.join("root/dev/ttyUSB7"), b"").unwrap();
    std::thread::sleep(Duration::from_millis(600));
    std::os::unix::fs::symlink(
        "../../ttyUSB7",
        env.dir.join("root/dev/serial/by-id/usb-FTDI_X-if00-port0"),
    )
    .unwrap();
    wait_log(&log, "Serial added: ttyUSB7 (not shown)", 1);
    // One that comes and goes before its link: nothing to say.
    std::fs::write(env.dir.join("root/dev/ttyUSB8"), b"").unwrap();
    std::thread::sleep(Duration::from_millis(600));
    std::fs::remove_file(env.dir.join("root/dev/ttyUSB8")).unwrap();
    std::thread::sleep(Duration::from_millis(2500));
    let text = std::fs::read_to_string(&log).unwrap();
    assert!(!text.contains("ttyUSB8"), "{text}");
    assert!(fake.bodies().is_empty(), "{:?}", fake.bodies());
}

#[test]
fn the_watcher_ends_with_its_herdr_server() {
    let env = Env::new();
    let fake = Fake::new(&env.dir);
    let o = env.watch(&["start"]);
    assert_eq!(o.status.code(), Some(0), "{}", out(&o));
    let log = env.state().join("watch.log");
    wait_log(&log, "watching", 1);
    // The server goes, its socket with it; a device event wakes the watcher.
    drop(fake);
    std::fs::remove_file(env.dir.join("herdr.sock")).unwrap();
    std::fs::write(env.dir.join("root/dev/ttyUSB3"), b"").unwrap();
    wait_log(&log, "the herdr server is gone; stopping", 1);
    let until = Instant::now() + T;
    while !out(&env.watch(&["status"])).starts_with("not running") {
        assert!(Instant::now() < until);
        std::thread::sleep(Duration::from_millis(50));
    }
}
