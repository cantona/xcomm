//! `xcomm watch`: the hotplug watcher herdr starts with the plugin. It tells
//! herdr (`notification.show`) when a serial port comes or goes.

use std::io::{self, Write};
use std::os::fd::AsFd;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::list::PortInfo;

/// How long the device nodes must be quiet before the ports are listed:
/// an adapter makes several (node, by-id link, by-path link).
const DEBOUNCE: Duration = Duration::from_millis(300);
/// Where nothing can be watched (macOS), how often the ports are listed.
const POLL: Duration = Duration::from_secs(1);
/// How long a new port waits for its by-id link before it is announced
/// without one, and how often it is looked at meanwhile.
const LINK_WAIT: Duration = Duration::from_secs(2);
const LINK_RECHECK: Duration = Duration::from_millis(500);
/// How often the ports are listed anyway, inotify or not.
const RELIST: Duration = Duration::from_secs(60);
/// How long `stop` waits for the watcher to end.
const STOP_WAIT: Duration = Duration::from_secs(2);
/// A log larger than this is moved aside when the watcher starts.
const LOG_MAX: u64 = 1 << 20;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WatchConfig {
    pub notify: bool,
    /// `0403:6001` (VID:PID) or a glob (`*`, `?`) on the by-id name.
    pub ignore: Vec<String>,
}

impl Default for WatchConfig {
    fn default() -> WatchConfig {
        WatchConfig {
            notify: true,
            ignore: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WatchError(pub String);

impl std::fmt::Display for WatchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for WatchError {}

/// What `xcomm watch` was asked to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WatchAction {
    /// In the background (`start`, `--daemon`).
    Start,
    Stop,
    Status,
    /// In the foreground.
    Run,
}

/// The config file; a missing one is the defaults.
pub fn load_config(path: &Path) -> Result<WatchConfig, WatchError> {
    match std::fs::read_to_string(path) {
        Ok(text) => parse_config(&text).map_err(|e| WatchError(format!("{}: {e}", path.display()))),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(WatchConfig::default()),
        Err(e) => Err(WatchError(format!("{}: {e}", path.display()))),
    }
}

/// The TOML xcomm's config uses: `notify = true|false` and
/// `ignore = ["...", ...]` (on one line or several), with comments. Anything
/// else is an error naming its line.
pub fn parse_config(text: &str) -> Result<WatchConfig, String> {
    let mut p = Parser {
        s: text.as_bytes(),
        i: 0,
        line: 1,
    };
    let mut cfg = WatchConfig::default();
    let mut seen: Vec<String> = Vec::new();
    loop {
        p.skip_blank();
        if p.i >= p.s.len() {
            return Ok(cfg);
        }
        let line = p.line;
        if p.s[p.i] == b'[' {
            return Err(format!("line {line}: tables are not used here"));
        }
        let key = p
            .key()
            .ok_or_else(|| format!("line {line}: expected a key"))?;
        p.skip_space();
        if !p.eat(b'=') {
            return Err(format!("line {line}: expected = after {key}"));
        }
        p.skip_space();
        if seen.contains(&key) {
            return Err(format!("line {line}: {key} is given twice"));
        }
        match key.as_str() {
            "notify" => {
                cfg.notify = if p.word("true") {
                    true
                } else if p.word("false") {
                    false
                } else {
                    return Err(format!("line {line}: notify is true or false"));
                };
            }
            "ignore" => {
                cfg.ignore = p
                    .string_list()
                    .map_err(|at| format!("line {at}: ignore is a list of strings"))?;
            }
            other => {
                return Err(format!(
                    "line {line}: unknown key '{other}' (notify, ignore)"
                ));
            }
        }
        seen.push(key);
        p.skip_space();
        if p.i < p.s.len() && p.s[p.i] != b'\n' && p.s[p.i] != b'#' {
            return Err(format!("line {}: unexpected text after the value", p.line));
        }
    }
}

struct Parser<'a> {
    s: &'a [u8],
    i: usize,
    line: usize,
}

impl Parser<'_> {
    fn skip_space(&mut self) {
        while self.i < self.s.len() && matches!(self.s[self.i], b' ' | b'\t' | b'\r') {
            self.i += 1;
        }
    }

    /// Spaces, newlines and comments.
    fn skip_blank(&mut self) {
        while self.i < self.s.len() {
            match self.s[self.i] {
                b' ' | b'\t' | b'\r' => self.i += 1,
                b'\n' => {
                    self.i += 1;
                    self.line += 1;
                }
                b'#' => {
                    while self.i < self.s.len() && self.s[self.i] != b'\n' {
                        self.i += 1;
                    }
                }
                _ => return,
            }
        }
    }

    fn eat(&mut self, b: u8) -> bool {
        let yes = self.s.get(self.i) == Some(&b);
        if yes {
            self.i += 1;
        }
        yes
    }

    fn key(&mut self) -> Option<String> {
        let start = self.i;
        while self
            .s
            .get(self.i)
            .is_some_and(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'))
        {
            self.i += 1;
        }
        (self.i > start).then(|| String::from_utf8_lossy(&self.s[start..self.i]).into_owned())
    }

    /// `w` as a whole word.
    fn word(&mut self, w: &str) -> bool {
        let end = self.i + w.len();
        let whole = self.s.get(self.i..end) == Some(w.as_bytes())
            && !self
                .s
                .get(end)
                .is_some_and(|b| b.is_ascii_alphanumeric() || *b == b'_');
        if whole {
            self.i = end;
        }
        whole
    }

    /// `["a", 'b',]`, over several lines if need be. Err: the line where it
    /// went wrong.
    fn string_list(&mut self) -> Result<Vec<String>, usize> {
        if !self.eat(b'[') {
            return Err(self.line);
        }
        let mut out = Vec::new();
        loop {
            self.skip_blank();
            if self.eat(b']') {
                return Ok(out);
            }
            out.push(self.string().ok_or(self.line)?);
            self.skip_blank();
            if self.eat(b']') {
                return Ok(out);
            }
            if !self.eat(b',') {
                return Err(self.line);
            }
        }
    }

    /// A basic ("...", with \\ \" \t \n escapes) or literal ('...') string
    /// on one line.
    fn string(&mut self) -> Option<String> {
        let quote = *self.s.get(self.i)?;
        if quote != b'"' && quote != b'\'' {
            return None;
        }
        self.i += 1;
        let mut out = Vec::new();
        loop {
            let b = *self.s.get(self.i)?;
            self.i += 1;
            match b {
                b'\n' => return None,
                b if b == quote => return String::from_utf8(out).ok(),
                b'\\' if quote == b'"' => {
                    let e = *self.s.get(self.i)?;
                    self.i += 1;
                    out.push(match e {
                        b'\\' => b'\\',
                        b'"' => b'"',
                        b't' => b'\t',
                        b'n' => b'\n',
                        _ => return None,
                    });
                }
                b => out.push(b),
            }
        }
    }
}

/// Ports in `after` and not `before`, and the other way round. A port is
/// its tty, by-id name and VID:PID: another adapter that gets the same tty
/// between two looks is a removal and an addition.
pub fn diff(before: &[PortInfo], after: &[PortInfo]) -> (Vec<PortInfo>, Vec<PortInfo>) {
    // A field one list has and the other not yet (udev makes the by-id
    // link after the node) does not make another adapter.
    let agree = |x: &Option<String>, y: &Option<String>| x.is_none() || y.is_none() || x == y;
    let same = |a: &PortInfo, b: &PortInfo| {
        a.tty == b.tty && agree(&a.by_id, &b.by_id) && agree(&a.vid_pid, &b.vid_pid)
    };
    let added = after
        .iter()
        .filter(|a| !before.iter().any(|b| same(a, b)))
        .cloned()
        .collect();
    let removed = before
        .iter()
        .filter(|b| !after.iter().any(|a| same(a, b)))
        .cloned()
        .collect();
    (added, removed)
}

/// Whether the config says to say nothing of `p`.
pub fn ignored(cfg: &WatchConfig, p: &PortInfo) -> bool {
    cfg.ignore.iter().any(|pat| {
        p.vid_pid
            .as_deref()
            .is_some_and(|v| v.eq_ignore_ascii_case(pat))
            || p.by_id.as_deref().is_some_and(|b| {
                glob(pat, b) || b.rsplit('/').next().is_some_and(|name| glob(pat, name))
            })
    })
}

/// `*` any run, `?` any one character.
fn glob(pat: &str, s: &str) -> bool {
    let (p, s): (Vec<char>, Vec<char>) = (pat.chars().collect(), s.chars().collect());
    let (mut pi, mut si) = (0, 0);
    let mut star: Option<(usize, usize)> = None;
    while si < s.len() {
        match p.get(pi) {
            Some('*') => {
                star = Some((pi, si));
                pi += 1;
            }
            Some(&c) if c == '?' || c == s[si] => {
                pi += 1;
                si += 1;
            }
            _ => match star {
                Some((sp, ss)) => {
                    pi = sp + 1;
                    si = ss + 1;
                    star = Some((sp, ss + 1));
                }
                None => return false,
            },
        }
    }
    p[pi..].iter().all(|&c| c == '*')
}

/// `$HERDR_PLUGIN_STATE_DIR/<server>`: one watcher per herdr server, named
/// after its socket's file name.
pub fn state_dir(env: &dyn Fn(&str) -> Option<String>) -> Result<PathBuf, WatchError> {
    let base = env("HERDR_PLUGIN_STATE_DIR")
        .filter(|s| !s.is_empty())
        .ok_or_else(|| WatchError("HERDR_PLUGIN_STATE_DIR is not set (run from herdr)".into()))?;
    let server = env("HERDR_SOCKET_PATH")
        .filter(|s| !s.is_empty())
        .and_then(|s| {
            Path::new(&s)
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
        })
        .unwrap_or_else(|| "default".into());
    Ok(Path::new(&base).join(server))
}

/// `$HERDR_PLUGIN_CONFIG_DIR/config.toml`, if herdr says where.
pub fn config_path(env: &dyn Fn(&str) -> Option<String>) -> Option<PathBuf> {
    env("HERDR_PLUGIN_CONFIG_DIR")
        .filter(|s| !s.is_empty())
        .map(|d| Path::new(&d).join("config.toml"))
}

/// Carries out `action`; returns the line to print.
pub fn act(
    action: WatchAction,
    env: &dyn Fn(&str) -> Option<String>,
) -> Result<String, WatchError> {
    // The watcher runs in / : a relative path would name another place
    // there than here.
    for var in [
        "HERDR_PLUGIN_STATE_DIR",
        "HERDR_PLUGIN_CONFIG_DIR",
        "HERDR_SOCKET_PATH",
        "XCOMM_WATCH_ROOT",
    ] {
        if let Some(v) = env(var).filter(|v| !v.is_empty() && !Path::new(v).is_absolute()) {
            return Err(WatchError(format!(
                "{var} must be an absolute path, not '{v}'"
            )));
        }
    }
    let dir = state_dir(env)?;
    std::fs::create_dir_all(&dir).map_err(|e| WatchError(format!("{}: {e}", dir.display())))?;
    let files = Files::new(&dir);
    match action {
        WatchAction::Start => start(&files, env),
        WatchAction::Stop => stop(&files),
        WatchAction::Status => Ok(status(&files, env)),
        WatchAction::Run => {
            herdr(env)?;
            let lock = files.lock()?;
            if !try_lock(&lock)? {
                return Ok(already(&files));
            }
            set_pid(&lock, Some(std::process::id()));
            let r = watch(env);
            set_pid(&lock, None);
            r.map(|()| "stopped".into())
        }
    }
}

/// The watcher's files. `watch.pid` is also its lock: the watcher holds
/// an flock on it for as long as it runs and writes its pid there through
/// that same open file, so a pid is believed only while the lock is held,
/// and no stale file from a killed watcher can name another process.
struct Files {
    pid: PathBuf,
    log: PathBuf,
}

impl Files {
    fn new(dir: &Path) -> Files {
        Files {
            pid: dir.join("watch.pid"),
            log: dir.join("watch.log"),
        }
    }

    fn lock(&self) -> Result<std::fs::File, WatchError> {
        std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&self.pid)
            .map_err(|e| WatchError(format!("{}: {e}", self.pid.display())))
    }

    /// The pid the lock's holder wrote; None until it has.
    fn read_pid(&self) -> Option<i32> {
        std::fs::read_to_string(&self.pid).ok()?.trim().parse().ok()
    }
}

/// Writes `pid` into the lock file the caller holds (or empties it).
fn set_pid(lock: &std::fs::File, pid: Option<u32>) {
    use std::os::unix::fs::FileExt;
    let _ = lock.set_len(0);
    if let Some(pid) = pid {
        let _ = lock.write_all_at(format!("{pid}\n").as_bytes(), 0);
    }
}

/// Takes the lock without waiting long; false when a watcher holds it.
/// `status` and `stop` take it for a moment to look, so a held lock is
/// looked at again a few times before it counts as the watcher's.
fn try_lock(file: &std::fs::File) -> Result<bool, WatchError> {
    use rustix::fs::{FlockOperation, flock};
    for _ in 0..LOCK_TRIES {
        match flock(file, FlockOperation::NonBlockingLockExclusive) {
            Ok(()) => return Ok(true),
            Err(rustix::io::Errno::WOULDBLOCK) => std::thread::sleep(LOCK_RETRY),
            Err(e) => return Err(WatchError(format!("lock: {e}"))),
        }
    }
    Ok(false)
}

const LOCK_TRIES: usize = 10;
const LOCK_RETRY: Duration = Duration::from_millis(20);

/// The herdr server notifications go to; without one there is no point.
fn herdr(env: &dyn Fn(&str) -> Option<String>) -> Result<crate::herdr::Herdr, WatchError> {
    crate::herdr::Herdr::server_from_env(env)
        .ok_or_else(|| WatchError("HERDR_SOCKET_PATH is not set (run from herdr)".into()))
}

/// Whether a watcher runs: its lock is held.
fn running(files: &Files) -> bool {
    use rustix::fs::{FlockOperation, flock};
    let Ok(file) = files.lock() else {
        return false;
    };
    match try_lock(&file) {
        Ok(false) => true,
        _ => {
            let _ = flock(&file, FlockOperation::Unlock);
            false
        }
    }
}

fn already(files: &Files) -> String {
    match files.read_pid() {
        Some(pid) => format!("already running (pid {pid})"),
        None => "already running".into(),
    }
}

/// Forks a watcher that holds the lock for as long as it runs.
fn start(files: &Files, env: &dyn Fn(&str) -> Option<String>) -> Result<String, WatchError> {
    herdr(env)?;
    let lock = files.lock()?;
    if !try_lock(&lock)? {
        return Ok(already(files));
    }
    // Until the watcher writes its own: a pid left by a killed one names
    // no watcher.
    set_pid(&lock, None);
    // A bad config is said now, to whoever started it, as well as in the log.
    if let Some(path) = config_path(env) {
        load_config(&path)?;
    }
    if std::fs::metadata(&files.log).is_ok_and(|m| m.len() > LOG_MAX) {
        let _ = std::fs::rename(&files.log, files.log.with_extension("log.1"));
    }
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&files.log)
        .map_err(|e| WatchError(format!("{}: {e}", files.log.display())))?;
    let null =
        std::fs::File::open("/dev/null").map_err(|e| WatchError(format!("/dev/null: {e}")))?;
    // SAFETY: the process is single-threaded here, so the child may go on
    // running Rust code.
    match unsafe { libc::fork() } {
        -1 => Err(WatchError(format!("fork: {}", io::Error::last_os_error()))),
        0 => {
            // The child: its own session, stdio to the log; the lock came
            // with it (it is the open file's, shared until both close it).
            unsafe { libc::setsid() };
            // Not holding herdr's plugin directory (or its mount) busy.
            let _ = std::env::set_current_dir("/");
            let fds = [
                (null.as_fd(), libc::STDIN_FILENO),
                (log.as_fd(), libc::STDOUT_FILENO),
                (log.as_fd(), libc::STDERR_FILENO),
            ];
            for (from, to) in fds {
                // SAFETY: dup2 onto the standard descriptors.
                unsafe { libc::dup2(std::os::fd::AsRawFd::as_raw_fd(&from), to) };
            }
            drop((null, log));
            set_pid(&lock, Some(std::process::id()));
            let code = match watch(env) {
                Ok(()) => 0,
                Err(e) => {
                    eprintln!("{} xcomm watch: {e}", stamp());
                    1
                }
            };
            set_pid(&lock, None);
            drop(lock);
            std::process::exit(code);
        }
        child => Ok(format!("started (pid {child})")),
    }
}

fn stop(files: &Files) -> Result<String, WatchError> {
    if !running(files) {
        return Ok("not running".into());
    }
    // Just started, the watcher may not have written it yet.
    let until = Instant::now() + STOP_WAIT;
    let pid = loop {
        if let Some(pid) = files.read_pid() {
            break pid;
        }
        if Instant::now() > until {
            return Err(WatchError(format!(
                "running, but {} names no pid",
                files.pid.display()
            )));
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let target =
        rustix::process::Pid::from_raw(pid).ok_or_else(|| WatchError(format!("bad pid {pid}")))?;
    rustix::process::kill_process(target, rustix::process::Signal::TERM)
        .map_err(|e| WatchError(format!("kill {pid}: {e}")))?;
    let until = Instant::now() + STOP_WAIT;
    while running(files) {
        if Instant::now() > until {
            return Err(WatchError(format!("pid {pid} did not stop")));
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    Ok(format!("stopped (pid {pid})"))
}

fn status(files: &Files, env: &dyn Fn(&str) -> Option<String>) -> String {
    let mut out = if running(files) {
        match files.read_pid() {
            Some(pid) => format!("running (pid {pid})"),
            None => "running".into(),
        }
    } else {
        "not running".into()
    };
    match config_path(env) {
        Some(p) => {
            let state = match load_config(&p) {
                Ok(_) if p.exists() => "",
                Ok(_) => " (none: defaults)",
                Err(_) => " (has an error)",
            };
            out.push_str(&format!("\nconfig {}{state}", p.display()));
        }
        None => out.push_str("\nconfig - (HERDR_PLUGIN_CONFIG_DIR is not set)"),
    }
    out.push_str(&format!("\nlog {}", files.log.display()));
    if let Ok(text) = std::fs::read_to_string(&files.log) {
        let lines: Vec<&str> = text.lines().collect();
        for line in &lines[lines.len().saturating_sub(5)..] {
            out.push_str(&format!("\n  {line}"));
        }
    }
    out
}

fn held_back(unannounced: &[String], tty: &str) -> bool {
    unannounced.iter().any(|t| t == tty)
}

/// Whether a signal waits: a long round of notifications stops for it.
fn signal_waiting(signals: &crate::hosttty::SignalPipe) -> bool {
    use rustix::event::{PollFd, PollFlags, Timespec, poll};
    let fd = signals.fd();
    let mut fds = [PollFd::new(&fd, PollFlags::IN)];
    matches!(poll(&mut fds, Some(&Timespec::default())), Ok(n) if n > 0)
}

/// Seconds since the epoch, for log lines.
fn stamp() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// The watching itself, until SIGTERM, SIGINT or SIGHUP.
fn watch(env: &dyn Fn(&str) -> Option<String>) -> Result<(), WatchError> {
    use rustix::event::{PollFd, PollFlags, Timespec, poll};
    let say = |line: &str| {
        let _ = writeln!(io::stderr(), "{} {line}", stamp());
    };
    let cfg = match config_path(env) {
        Some(path) => load_config(&path)?,
        None => WatchConfig::default(),
    };
    let signals = crate::hosttty::SignalPipe::install(&[libc::SIGTERM, libc::SIGINT, libc::SIGHUP])
        .map_err(|e| WatchError(format!("signals: {e}")))?;
    let herdr = herdr(env)?;
    // Looked up when wanted: a directory made after the start counts.
    let socket_dirs = || -> Vec<PathBuf> {
        crate::ctl::path::socket_dirs(env)
            .into_iter()
            .filter(|d| crate::ctl::path::check_dir(d).is_ok())
            .collect()
    };
    // XCOMM_WATCH_ROOT is test-only: a tree that stands for / .
    let root = env("XCOMM_WATCH_ROOT")
        .filter(|s| !s.is_empty())
        .map_or_else(|| PathBuf::from("/"), PathBuf::from);
    let list = || crate::list::enumerate_in(&root, env);
    let devices = crate::devwatch::DevWatch::new(&root);
    let mut ports = list().map_err(|e| WatchError(format!("ports: {e}")))?;
    say(&format!("watching {} ports", ports.len()));
    if let Some(why) = devices.problem() {
        say(&format!("{why}; looking every {} s", POLL.as_secs()));
    }
    let mut due: Option<Instant> = None;
    // New ports waiting for their by-id link, and since when.
    let mut unannounced: Vec<(String, Instant)> = Vec::new();
    loop {
        let wait = match (due, devices.fd()) {
            (Some(at), _) => at.saturating_duration_since(Instant::now()),
            // A missed event is found at the next look all the same.
            (None, Some(_)) => RELIST,
            (None, None) => POLL,
        };
        let timeout = Timespec::try_from(wait).unwrap_or_default();
        let sig_fd = signals.fd();
        let mut fds = vec![PollFd::new(&sig_fd, PollFlags::IN)];
        if let Some(fd) = devices.fd() {
            fds.push(PollFd::from_borrowed_fd(fd, PollFlags::IN));
        }
        match poll(&mut fds, Some(&timeout)) {
            Ok(_) | Err(rustix::io::Errno::INTR) => {}
            Err(e) => return Err(WatchError(format!("poll: {e}"))),
        }
        let signalled = !fds[0].revents().is_empty();
        let event = fds.get(1).is_some_and(|f| !f.revents().is_empty());
        drop(fds);
        if signalled {
            let _ = signals.drain();
            say("stopping");
            return Ok(());
        }
        if event && devices.take_events() {
            due = Some(Instant::now() + DEBOUNCE);
            continue;
        }
        if due.is_some_and(|at| Instant::now() < at) {
            continue;
        }
        due = None;
        let now = match list() {
            Ok(now) => now,
            Err(e) => {
                say(&format!("ports: {e}"));
                continue;
            }
        };
        // Ports not announced yet as this list began: their going is not
        // announced either.
        let unannounced_before: Vec<String> = unannounced.iter().map(|(t, _)| t.clone()).collect();
        let (added, removed) = diff(&ports, &now);
        // A new port without its by-id link yet waits a little for it: an
        // ignore glob on the by-id name must see it.
        let at = Instant::now();
        for p in &added {
            // Another adapter on a name still waiting: one entry, its time.
            unannounced.retain(|(tty, _)| tty != &p.tty);
            unannounced.push((p.tty.clone(), at));
        }
        let mut ready = Vec::new();
        unannounced.retain(|(tty, since)| match now.iter().find(|p| &p.tty == tty) {
            // Gone before it was announced: nothing to say either way.
            None => false,
            Some(p) if p.by_id.is_some() || since.elapsed() >= LINK_WAIT => {
                ready.push(p.clone());
                false
            }
            Some(_) => true,
        });
        if !unannounced.is_empty() {
            due = Some(Instant::now() + LINK_RECHECK);
        }
        let removed: Vec<PortInfo> = removed
            .into_iter()
            .filter(|p| !held_back(&unannounced_before, &p.tty))
            .collect();
        for (title, p) in ready
            .iter()
            .map(|p| ("Serial added", p))
            .chain(removed.iter().map(|p| ("Serial removed", p)))
        {
            let mut body = match &p.product {
                Some(product) => format!("{product} ({})", p.tty),
                None => p.tty.clone(),
            };
            // The session now waits for its device: asked now, as the
            // list from before may predate it.
            if title == "Serial removed"
                && let Some(pane) = crate::list::session_pane(&socket_dirs(), &p.tty, false)
                    .or_else(|| p.herdr_pane.clone())
            {
                body.push_str(&format!(", open in pane {pane}"));
            }
            if signal_waiting(&signals) {
                say("stopping");
                return Ok(());
            }
            let quiet = !cfg.notify || ignored(&cfg, p);
            say(&format!(
                "{title}: {body}{}",
                if quiet { " (not shown)" } else { "" }
            ));
            if !quiet && let Err(e) = herdr.notify(title, Some(&body)) {
                say(&format!("herdr: {e}"));
            }
        }
        ports = now;
    }
}
