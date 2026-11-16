//! `xcomm pick`: the port picker herdr shows in a popup. A plain list in
//! raw mode, no TUI library. Enter opens the port in a new plugin pane, or
//! focuses the pane already showing it.

use std::io;
use std::os::fd::BorrowedFd;
use std::time::{Duration, Instant};

use crate::list::PortInfo;

/// The plugin xcomm installs into herdr.
pub const PLUGIN_ID: &str = "cantona.herdr-xcomm";
/// The baud a port opens at unless `b` changes it.
pub const DEFAULT_BAUD: u32 = 115_200;
/// How long a lone ESC waits for the rest of a key, as in a session.
const ESC_WAIT: Duration = Duration::from_millis(25);
/// How often the list is read again: locks and panes change with no
/// event in /dev.
const REFRESH: Duration = Duration::from_secs(1);
/// Most digits the baud prompt takes.
const BAUD_DIGITS: usize = 10;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Placement {
    SplitRight,
    SplitDown,
    Tab,
}

impl Placement {
    fn next(self) -> Placement {
        match self {
            Placement::SplitRight => Placement::SplitDown,
            Placement::SplitDown => Placement::Tab,
            Placement::Tab => Placement::SplitRight,
        }
    }

    fn describe(self) -> &'static str {
        match self {
            Placement::SplitRight => "split right",
            Placement::SplitDown => "split down",
            Placement::Tab => "new tab",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PickKey {
    Up,
    Down,
    Enter,
    Baud,
    Placement,
    Esc,
    Digit(u8),
    Backspace,
    Other,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PickAction {
    /// Nothing to do but draw again.
    Redraw,
    Quit,
    /// Open the selected port in a new pane.
    Open,
    /// The port is open in this herdr pane already.
    Focus(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PickState {
    pub ports: Vec<PortInfo>,
    pub selected: usize,
    pub baud: u32,
    pub placement: Placement,
    /// The baud being typed, while `b`'s prompt is open.
    pub prompt: Option<String>,
    /// A line for the user: why opening failed, or a bad baud.
    pub message: Option<String>,
}

impl PickState {
    pub fn new(ports: Vec<PortInfo>) -> PickState {
        PickState {
            ports,
            selected: 0,
            baud: DEFAULT_BAUD,
            placement: Placement::SplitRight,
            prompt: None,
            message: None,
        }
    }

    /// A new list, keeping the selection on the same port when it is still
    /// there.
    pub fn set_ports(&mut self, ports: Vec<PortInfo>) {
        let was = self.ports.get(self.selected).map(|p| p.tty.clone());
        self.ports = ports;
        self.selected = was
            .and_then(|tty| self.ports.iter().position(|p| p.tty == tty))
            .unwrap_or(0)
            .min(self.ports.len().saturating_sub(1));
    }

    pub fn apply(&mut self, key: PickKey) -> PickAction {
        if let Some(typed) = &mut self.prompt {
            match key {
                PickKey::Digit(d) if typed.len() < BAUD_DIGITS => typed.push(char::from(d)),
                PickKey::Backspace => {
                    typed.pop();
                }
                PickKey::Enter => {
                    match typed.parse::<u32>().ok().filter(|&b| b > 0) {
                        Some(baud) => {
                            self.baud = baud;
                            self.message = None;
                        }
                        None => self.message = Some(format!("not a baud rate: '{typed}'")),
                    }
                    self.prompt = None;
                }
                PickKey::Esc => self.prompt = None,
                _ => {}
            }
            return PickAction::Redraw;
        }
        match key {
            PickKey::Up => self.selected = self.selected.saturating_sub(1),
            PickKey::Down => {
                self.selected = (self.selected + 1).min(self.ports.len().saturating_sub(1));
            }
            PickKey::Baud => {
                self.prompt = Some(String::new());
                self.message = None;
            }
            PickKey::Placement => self.placement = self.placement.next(),
            PickKey::Esc => return PickAction::Quit,
            PickKey::Enter => {
                let Some(port) = self.ports.get(self.selected) else {
                    return PickAction::Redraw;
                };
                return match (&port.herdr_pane, port.lock_pid) {
                    (Some(pane), _) => PickAction::Focus(pane.clone()),
                    // Held, with no pane known (another program, or a
                    // session too slow to answer): a new session would
                    // only fail on the lock.
                    (None, Some(pid)) => {
                        self.message = Some(format!("{} is in use by process {pid}", port.tty));
                        PickAction::Redraw
                    }
                    (None, None) => PickAction::Open,
                };
            }
            PickKey::Digit(_) | PickKey::Backspace | PickKey::Other => {}
        }
        PickAction::Redraw
    }
}

/// The keys in `input`. A trailing ESC that may start a key is the caller's
/// to hold back (see `incomplete`); here it counts as Esc.
pub fn decode_keys(input: &[u8]) -> Vec<PickKey> {
    let mut keys = Vec::new();
    let mut i = 0;
    while i < input.len() {
        let (key, used) = match input[i] {
            0x1b => match (input.get(i + 1), input.get(i + 2)) {
                (Some(b'[' | b'O'), Some(b'A')) => (PickKey::Up, 3),
                (Some(b'[' | b'O'), Some(b'B')) => (PickKey::Down, 3),
                (Some(b'[' | b'O'), Some(_)) => {
                    // Another key's sequence: up to its final byte.
                    let rest = &input[i + 2..];
                    let end = rest
                        .iter()
                        .position(|b| (0x40..=0x7e).contains(b))
                        .map_or(rest.len(), |p| p + 1);
                    (PickKey::Other, 2 + end)
                }
                // Alt with a key: not a lone Esc, which would close.
                (Some(0x20..=0x7e | b'\r' | b'\n' | 0x7f | 0x08 | 0x80..), _) => {
                    (PickKey::Other, 2)
                }
                // ESC ESC (keys typed while the loop was busy), or ESC and
                // a control: an Esc, then the next byte as itself.
                _ => (PickKey::Esc, 1),
            },
            // Raw mode: Ctrl-C is a byte, not a signal.
            0x03 => (PickKey::Esc, 1),
            b'k' => (PickKey::Up, 1),
            b'j' => (PickKey::Down, 1),
            b'\r' | b'\n' => (PickKey::Enter, 1),
            b'b' => (PickKey::Baud, 1),
            b'o' => (PickKey::Placement, 1),
            b'q' => (PickKey::Esc, 1),
            0x7f | 0x08 => (PickKey::Backspace, 1),
            d @ b'0'..=b'9' => (PickKey::Digit(d), 1),
            _ => (PickKey::Other, 1),
        };
        keys.push(key);
        i += used;
    }
    keys
}

/// Whether `input` ends in what may be the start of a key sequence: an
/// ESC, or ESC [ / ESC O with no final byte yet.
pub fn incomplete(input: &[u8]) -> bool {
    let Some(esc) = input.iter().rposition(|&b| b == 0x1b) else {
        return false;
    };
    match &input[esc + 1..] {
        [] => true,
        [b'[' | b'O', rest @ ..] => !rest.iter().any(|b| (0x40..=0x7e).contains(b)),
        _ => false,
    }
}

/// The screen, drawn from the top: a title line, one row per port (the
/// selected one marked), the keys, and any message, wrapped.
pub fn render(state: &PickState, width: u16, height: u16, out: &mut Vec<u8>) {
    let width = usize::from(width.max(20));
    let height = usize::from(height.max(5));
    out.extend_from_slice(b"\x1b[H\x1b[2J");
    // A message (a command's error) wraps, and its rows are kept: its end
    // says what went wrong.
    let message: Vec<String> = state.message.as_deref().map_or_else(Vec::new, |m| {
        let chars: Vec<char> = m.chars().map(printable).collect();
        chars.chunks(width).map(|c| c.iter().collect()).collect()
    });
    let keep = message.len().min(height.saturating_sub(4));
    let message = &message[message.len() - keep..];
    let room = height.saturating_sub(3 + message.len()).max(1);
    let first = state.selected.saturating_sub(room - 1);
    let rows: Vec<[String; 5]> = state
        .ports
        .iter()
        .map(|p| {
            [
                p.tty.clone(),
                p.vid_pid.clone().unwrap_or_else(|| "-".into()),
                match (&p.herdr_pane, p.lock_pid) {
                    (Some(pane), _) => format!("pane {pane}"),
                    (None, Some(pid)) => format!("pid {pid}"),
                    (None, None) => "-".into(),
                },
                // Before the product, which it mostly repeats: its serial
                // and interface tell two of the same adapter apart, so the
                // parts every name has go.
                p.by_id.as_deref().map_or_else(
                    || p.path.clone(),
                    |b| {
                        let name = b.rsplit('/').next().unwrap_or(b);
                        let name = name.strip_prefix("usb-").unwrap_or(name);
                        name.strip_suffix("-port0").unwrap_or(name).to_string()
                    },
                ),
                p.product.clone().unwrap_or_else(|| "-".into()),
            ]
        })
        .collect();
    let mut widths = [0usize; 5];
    for row in &rows {
        for (w, cell) in widths.iter_mut().zip(row) {
            *w = (*w).max(cell.chars().count());
        }
    }
    // A name longer than the room left loses its front, not the serial and
    // interface at its end.
    let room_for_name = width.saturating_sub(2 + widths[..3].iter().map(|w| w + 1).sum::<usize>());
    widths[3] = widths[3].min(room_for_name);
    let rows: Vec<[String; 5]> = rows
        .into_iter()
        .map(|mut row| {
            let n = row[3].chars().count();
            if n > widths[3] && widths[3] > 1 {
                let tail: String = row[3].chars().skip(n - (widths[3] - 1)).collect();
                row[3] = format!("~{tail}");
            }
            row
        })
        .collect();
    let mut lines: Vec<String> = vec![format!(
        "xcomm: serial ports   baud {}   open in {}",
        state.baud,
        state.placement.describe()
    )];
    if rows.is_empty() {
        lines.push("  no serial ports".into());
    }
    for (i, row) in rows.iter().enumerate().skip(first).take(room) {
        let mut line = String::from(if i == state.selected { "> " } else { "  " });
        for (n, cell) in row.iter().enumerate() {
            if n + 1 == row.len() {
                line.push_str(cell);
            } else {
                line.push_str(&format!("{cell:<w$} ", w = widths[n]));
            }
        }
        lines.push(line);
    }
    lines.push(String::new());
    lines.push(match &state.prompt {
        Some(typed) => format!("baud: {typed}_   (Enter sets, Esc cancels)"),
        None => "Enter open   b baud   o placement   Esc close".into(),
    });
    lines.extend(message.iter().cloned());
    for (n, line) in lines.iter().take(height).enumerate() {
        if n > 0 {
            out.extend_from_slice(b"\r\n");
        }
        let shown: String = line.chars().map(printable).take(width).collect();
        out.extend_from_slice(shown.as_bytes());
    }
}

fn printable(c: char) -> char {
    if c.is_control() { '?' } else { c }
}

/// The arguments to `herdr` that open the selected port in a new pane.
pub fn open_command(state: &PickState, plugin_id: &str, target_pane: Option<&str>) -> Vec<String> {
    let port = &state.ports[state.selected];
    let mut argv: Vec<String> = [
        "plugin",
        "pane",
        "open",
        "--plugin",
        plugin_id,
        "--entrypoint",
        "session",
        "--placement",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    match state.placement {
        Placement::SplitRight => argv.extend(["split", "--direction", "right"].map(String::from)),
        Placement::SplitDown => argv.extend(["split", "--direction", "down"].map(String::from)),
        Placement::Tab => argv.push("tab".into()),
    }
    if let Some(pane) = target_pane {
        argv.extend(["--target-pane".to_string(), pane.to_string()]);
    }
    let device = port.by_id.as_deref().unwrap_or(&port.path);
    argv.extend([
        "--env".to_string(),
        format!("XCOMM_DEVICE={device}"),
        "--env".to_string(),
        format!("XCOMM_BAUD={}", state.baud),
        "--focus".to_string(),
    ]);
    argv
}

#[derive(Debug)]
pub struct PickError(pub String);

impl std::fmt::Display for PickError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for PickError {}

/// The pane the popup was opened from, as herdr tells a plugin pane.
fn target_pane(env: &dyn Fn(&str) -> Option<String>) -> Option<String> {
    let json = env("HERDR_PLUGIN_CONTEXT_JSON")?;
    let v: serde_json::Value = serde_json::from_str(&json).ok()?;
    v["focused_pane_id"].as_str().map(str::to_string)
}

/// Runs the picker on the terminal `input`/`output` until it opens a port,
/// focuses its pane or is closed; returns the exit status.
pub fn run(
    input: BorrowedFd<'_>,
    output: BorrowedFd<'_>,
    env: &dyn Fn(&str) -> Option<String>,
) -> Result<i32, PickError> {
    use rustix::event::{PollFd, PollFlags, Timespec, poll};
    let err = |what: &str, e: io::Error| PickError(format!("{what}: {e}"));
    let signals = crate::hosttty::SignalPipe::install(&[
        libc::SIGTERM,
        libc::SIGINT,
        libc::SIGHUP,
        libc::SIGWINCH,
    ])
    .map_err(|e| err("signals", e))?;
    let raw = crate::hosttty::RawMode::enter(input).map_err(|e| err("raw mode", e))?;
    // The popup has the server's socket but no pane of its own.
    let herdr = crate::herdr::Herdr::server_from_env(env);
    let target = target_pane(env);
    let herdr_bin = env("HERDR_BIN_PATH")
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "herdr".into());
    let mut state = PickState::new(crate::list::enumerate(env).map_err(|e| err("ports", e))?);
    // Listing asks each session for its pane, which a busy one may take
    // its time over: off the thread that reads keys.
    let lister = Lister::start(env).map_err(|e| err("lister", e))?;
    let mut pending: Vec<u8> = Vec::new();
    let mut pending_since = Instant::now();
    let mut screen = Vec::new();
    let draw = |state: &PickState, screen: &mut Vec<u8>| -> Result<(), PickError> {
        let size = rustix::termios::tcgetwinsize(output).ok();
        // A terminal that does not say its size (0) is taken as 80x24.
        let cols = size.map_or(0, |s| s.ws_col);
        let rows = size.map_or(0, |s| s.ws_row);
        screen.clear();
        render(
            state,
            if cols == 0 { 80 } else { cols },
            if rows == 0 { 24 } else { rows },
            screen,
        );
        write_all(output, screen).map_err(|e| err("terminal", e))
    };
    draw(&state, &mut screen)?;
    let outcome = loop {
        let held = incomplete(&pending);
        let wait = if held {
            ESC_WAIT.saturating_sub(pending_since.elapsed())
        } else {
            REFRESH
        };
        let timeout = Timespec::try_from(wait).unwrap_or_default();
        let sig_fd = signals.fd();
        let mut fds = [
            PollFd::new(&input, PollFlags::IN),
            PollFd::new(&sig_fd, PollFlags::IN),
            PollFd::from_borrowed_fd(lister.fd(), PollFlags::IN),
        ];
        match poll(&mut fds, Some(&timeout)) {
            Ok(_) | Err(rustix::io::Errno::INTR) => {}
            Err(e) => break Err(err("poll", e.into())),
        }
        let input_ready = !fds[0].revents().is_empty();
        let signal_ready = !fds[1].revents().is_empty();
        let list_ready = !fds[2].revents().is_empty();
        if signal_ready {
            let bits = signals.drain().map_err(|e| err("signals", e))?;
            if bits & !(1u64 << libc::SIGWINCH) != 0 {
                break Ok(1);
            }
            draw(&state, &mut screen)?;
        }
        if list_ready
            && let Some(ports) = lister.take()
            && ports != state.ports
        {
            state.set_ports(ports);
            draw(&state, &mut screen)?;
        }
        if input_ready {
            let mut buf = [0u8; 256];
            match rustix::io::read(input, &mut buf) {
                Ok(0) => break Ok(1),
                Ok(n) => {
                    if pending.is_empty() {
                        pending_since = Instant::now();
                    }
                    pending.extend_from_slice(&buf[..n]);
                }
                Err(rustix::io::Errno::INTR | rustix::io::Errno::AGAIN) => {}
                Err(e) => break Err(err("terminal", e.into())),
            }
        }
        // A key that may still be coming waits its 25 ms, whatever else
        // woke the loop.
        if pending.is_empty() || (incomplete(&pending) && pending_since.elapsed() < ESC_WAIT) {
            continue;
        }
        let keys = decode_keys(&pending);
        pending.clear();
        let mut done = None;
        for key in keys {
            match state.apply(key) {
                PickAction::Redraw => {}
                PickAction::Quit => {
                    done = Some(0);
                    break;
                }
                PickAction::Focus(pane) => match herdr.as_ref().map(|h| h.focus_pane(&pane)) {
                    Some(Ok(())) => {
                        done = Some(0);
                        break;
                    }
                    Some(Err(e)) => state.message = Some(format!("cannot focus {pane}: {e}")),
                    None => state.message = Some("not inside herdr".into()),
                },
                PickAction::Open => {
                    // udev makes the by-id link a little after the node:
                    // the session should follow the adapter, not the name.
                    let port = &mut state.ports[state.selected];
                    if port.by_id.is_none() {
                        port.by_id = crate::list::by_id_of(&port.tty);
                    }
                    let argv = open_command(&state, PLUGIN_ID, target.as_deref());
                    match std::process::Command::new(&herdr_bin)
                        .args(&argv)
                        .stdin(std::process::Stdio::null())
                        .output()
                    {
                        Ok(o) if o.status.success() => {
                            done = Some(0);
                            break;
                        }
                        Ok(o) => {
                            let why = String::from_utf8_lossy(&o.stderr);
                            state.message =
                                Some(format!("{herdr_bin} failed ({}): {}", o.status, why.trim()));
                        }
                        Err(e) => state.message = Some(format!("{herdr_bin}: {e}")),
                    }
                }
            }
        }
        if let Some(code) = done {
            break Ok(code);
        }
        draw(&state, &mut screen)?;
    };
    let _ = write_all(output, b"\x1b[H\x1b[2J");
    drop(raw);
    outcome
}

fn write_all(fd: BorrowedFd<'_>, mut bytes: &[u8]) -> io::Result<()> {
    while !bytes.is_empty() {
        match rustix::io::write(fd, bytes) {
            Ok(n) => bytes = &bytes[n..],
            Err(rustix::io::Errno::INTR) => {}
            Err(e) => return Err(e.into()),
        }
    }
    Ok(())
}

/// Lists the ports on a thread of its own, every REFRESH and soon after a
/// device node comes or goes, and wakes the picker through a pipe.
struct Lister {
    wake: std::os::fd::OwnedFd,
    latest: std::sync::Arc<std::sync::Mutex<Option<Vec<PortInfo>>>>,
}

impl Lister {
    fn start(env: &dyn Fn(&str) -> Option<String>) -> io::Result<Lister> {
        // What listing reads of the environment, owned by the thread.
        let vars: Vec<(&str, String)> = ["XCOMM_LOCK_DIR", "XDG_RUNTIME_DIR"]
            .into_iter()
            .filter_map(|k| env(k).map(|v| (k, v)))
            .collect();
        let (read, write) = rustix::pipe::pipe()?;
        for end in [&read, &write] {
            rustix::io::fcntl_setfd(end, rustix::io::FdFlags::CLOEXEC)?;
        }
        let latest = std::sync::Arc::new(std::sync::Mutex::new(None));
        let shared = latest.clone();
        std::thread::spawn(move || {
            let env = |k: &str| vars.iter().find(|(n, _)| *n == k).map(|(_, v)| v.clone());
            let watch = crate::devwatch::DevWatch::new(std::path::Path::new("/"));
            loop {
                watch.wait(REFRESH);
                if let Ok(ports) = crate::list::enumerate(&env) {
                    *shared.lock().unwrap_or_else(|e| e.into_inner()) = Some(ports);
                    // The picker gone: so is the reader.
                    if rustix::io::write(&write, b"x").is_err() {
                        return;
                    }
                }
            }
        });
        Ok(Lister { wake: read, latest })
    }

    fn fd(&self) -> BorrowedFd<'_> {
        std::os::fd::AsFd::as_fd(&self.wake)
    }

    /// The latest list, once per wake-up.
    fn take(&self) -> Option<Vec<PortInfo>> {
        let mut buf = [0u8; 64];
        let _ = rustix::io::read(&self.wake, &mut buf);
        self.latest.lock().unwrap_or_else(|e| e.into_inner()).take()
    }
}

/// What a plugin action's prompt asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptKind {
    /// A file to capture to.
    Capture,
    Baud,
    /// Files to send with ZMODEM.
    SendFile,
    /// The directory ZMODEM files are received into.
    RecvFile,
}

impl PromptKind {
    pub fn parse(s: &str) -> Option<PromptKind> {
        Some(match s {
            "capture" => PromptKind::Capture,
            "baud" => PromptKind::Baud,
            "send-file" => PromptKind::SendFile,
            "recv-file" => PromptKind::RecvFile,
            _ => return None,
        })
    }

    /// Short enough for a small popup; the long form goes under it.
    fn question(self) -> &'static str {
        match self {
            PromptKind::Capture => "capture to file",
            PromptKind::Baud => "baud rate",
            PromptKind::SendFile => "send files",
            PromptKind::RecvFile => "receive into",
        }
    }

    fn hint(self) -> &'static str {
        match self {
            PromptKind::Capture => "a path; ~/ is your home",
            PromptKind::Baud => "digits, as 115200",
            PromptKind::SendFile => "paths, separated by spaces (ZMODEM)",
            PromptKind::RecvFile => "a directory; empty is your home (ZMODEM)",
        }
    }
}

/// The `xcomm ctl` request an answer makes, for the session in `pane`.
/// Paths that are not absolute are from `home` (a plugin's commands run
/// in the plugin's own directory). Err: what is wrong with the answer.
pub fn prompt_request(
    kind: PromptKind,
    answer: &str,
    pane: &str,
    home: &std::path::Path,
) -> Result<(crate::cli::CtlArgs, Option<std::path::PathBuf>), String> {
    use crate::cli::{CtlArgs, CtlCmd};
    use crate::ctl::proto::{SettingsPatch, XferDir, XferProto};
    let answer = answer.trim();
    let path = |p: &str| {
        let p = match p.strip_prefix("~/") {
            // `~//boot.txt` is $HOME/boot.txt, as in a shell:
            // `home.join("/boot.txt")` would be `/boot.txt`.
            Some(rest) => home.join(rest.trim_start_matches('/')),
            None => std::path::PathBuf::from(p),
        };
        if p.is_absolute() { p } else { home.join(p) }
    };
    if answer.is_empty() && kind != PromptKind::RecvFile {
        return Err("nothing given".into());
    }
    let mut cwd = None;
    let cmd = match kind {
        PromptKind::Capture => CtlCmd::CaptureStart(path(answer)),
        PromptKind::Baud => {
            let baud = answer
                .parse::<u32>()
                .ok()
                .filter(|&b| b > 0 && answer.bytes().all(|b| b.is_ascii_digit()))
                .ok_or_else(|| format!("not a baud rate: '{answer}'"))?;
            CtlCmd::Set {
                patch: SettingsPatch {
                    baud: Some(baud),
                    ..SettingsPatch::default()
                },
            }
        }
        PromptKind::SendFile => CtlCmd::Xfer {
            dir: XferDir::Send,
            proto: XferProto::Zmodem,
            files: answer.split_whitespace().map(path).collect(),
        },
        PromptKind::RecvFile => {
            cwd = Some(if answer.is_empty() {
                home.to_path_buf()
            } else {
                path(answer)
            });
            CtlCmd::Xfer {
                dir: XferDir::Recv,
                proto: XferProto::Zmodem,
                files: Vec::new(),
            }
        }
    };
    Ok((
        CtlArgs {
            device: None,
            pane: Some(pane.to_string()),
            cmd,
            json: false,
        },
        cwd,
    ))
}

/// Columns `c` takes on a terminal: 2 in the East Asian wide and emoji
/// ranges, 1 otherwise. Close enough to cut a line to a popup's width.
fn columns(c: char) -> usize {
    match u32::from(c) {
        0x1100..=0x115f
        | 0x2e80..=0x303e
        | 0x3041..=0x33ff
        | 0x3400..=0x4dbf
        | 0x4e00..=0x9fff
        | 0xa000..=0xa4cf
        | 0xac00..=0xd7a3
        | 0xf900..=0xfaff
        | 0xfe30..=0xfe4f
        | 0xff00..=0xff60
        | 0xffe0..=0xffe6
        | 0x1f300..=0x1f64f
        | 0x1f900..=0x1f9ff
        | 0x20000..=0x3fffd => 2,
        _ => 1,
    }
}

/// The start of `text` that fits in `cols` columns.
pub fn head_cols(text: &str, cols: usize) -> String {
    let mut used = 0;
    text.chars()
        .take_while(|&c| {
            used += columns(c);
            used <= cols
        })
        .collect()
}

/// The end of `text` that fits in `cols` columns.
pub fn tail_cols(text: &str, cols: usize) -> String {
    let mut used = 0;
    let mut tail: Vec<char> = text
        .chars()
        .rev()
        .take_while(|&c| {
            used += columns(c);
            used <= cols
        })
        .collect();
    tail.reverse();
    tail.into_iter().collect()
}

/// The prompt popup: asks for one line, runs the request it makes on the
/// session in `pane`, and on a failure shows why until a key is pressed.
pub fn run_prompt(
    kind: PromptKind,
    pane: &str,
    input: BorrowedFd<'_>,
    output: BorrowedFd<'_>,
    env: &dyn Fn(&str) -> Option<String>,
) -> Result<i32, PickError> {
    // The popup pane closes the moment this returns, so what went wrong is
    // shown here and held: on stderr it would be gone before it was read.
    let hold = |text: &str| -> Result<i32, PickError> {
        let _ = write_all(
            output,
            format!("\r\nxcomm: {text}\r\npress Enter to close").as_bytes(),
        );
        // Keys typed before the message was up must not close it.
        let _ = rustix::termios::tcflush(input, rustix::termios::QueueSelector::IFlush);
        let mut buf = [0u8; 16];
        let _ = rustix::io::read(input, &mut buf);
        Ok(1)
    };
    let Some(home) = env("HOME")
        .filter(|h| !h.is_empty())
        .map(std::path::PathBuf::from)
    else {
        return hold("HOME is not set");
    };
    let raw = match crate::hosttty::RawMode::enter(input) {
        Ok(raw) => raw,
        Err(e) => return hold(&format!("raw mode: {e}")),
    };
    let mut line: Vec<u8> = Vec::new();
    let mut message: Option<String> = None;
    let answer = loop {
        // A popup is small and herdr draws a border round it: each line
        // is cut to the width, and the hint goes only where there is room.
        let size = rustix::termios::tcgetwinsize(output).ok();
        let cols = usize::from(size.map_or(0, |s| s.ws_col)).max(8);
        let rows = size.map_or(3, |s| s.ws_row);
        let cut = |text: &str| head_cols(text, cols);
        let mut lines = vec![cut(&format!("{} (Esc cancels)", kind.question()))];
        match (&message, rows >= 3) {
            (Some(m), _) => lines.push(cut(m)),
            (None, true) => lines.push(cut(kind.hint())),
            (None, false) => {}
        }
        // The end of the line, where the typing is, stays in view.
        let typed = String::from_utf8_lossy(&line);
        lines.push(format!("> {}", tail_cols(&typed, cols.saturating_sub(2))));
        let screen = format!("\x1b[H\x1b[2J{}", lines.join("\r\n"));
        // Out of raw mode first, so that it is Enter that closes.
        if let Err(e) = write_all(output, screen.as_bytes()) {
            drop(raw);
            return hold(&format!("terminal: {e}"));
        }
        let mut buf = [0u8; 256];
        let n = match rustix::io::read(input, &mut buf) {
            Ok(0) => return Ok(1),
            Ok(n) => n,
            Err(rustix::io::Errno::INTR) => continue,
            Err(e) => {
                drop(raw);
                return hold(&format!("terminal: {e}"));
            }
        };
        let mut keys = buf[..n].to_vec();
        // A key's sequence cut between reads waits its 25 ms for the rest,
        // as in the picker: a lone ESC is Esc only once none comes.
        let since = Instant::now();
        while incomplete(&keys) {
            let left = ESC_WAIT.saturating_sub(since.elapsed());
            let timeout = rustix::event::Timespec::try_from(left).unwrap_or_default();
            let mut fds = [rustix::event::PollFd::new(
                &input,
                rustix::event::PollFlags::IN,
            )];
            if left.is_zero() || !matches!(rustix::event::poll(&mut fds, Some(&timeout)), Ok(1..)) {
                break;
            }
            match rustix::io::read(input, &mut buf) {
                Ok(0) => break,
                Ok(n) => keys.extend_from_slice(&buf[..n]),
                Err(_) => break,
            }
        }
        let mut done = None;
        let mut i = 0;
        while i < keys.len() {
            let b = keys[i];
            i += 1;
            match b {
                b'\r' | b'\n' => {
                    done = Some(Some(line.clone()));
                    break;
                }
                // An arrow, Home or F-key: up to its final byte, ignored.
                0x1b if matches!(keys.get(i), Some(b'[' | b'O')) => {
                    let rest = &keys[i + 1..];
                    i += 1 + rest
                        .iter()
                        .position(|b| (0x40..=0x7e).contains(b))
                        .map_or(rest.len(), |p| p + 1);
                }
                // Alt with a key: not a lone Esc.
                0x1b if keys.get(i).is_some_and(|&b| b != 0x1b) => i += 1,
                0x1b | 0x03 => {
                    done = Some(None);
                    break;
                }
                0x7f | 0x08 => {
                    // One character, which UTF-8 spreads over bytes.
                    while line.pop().is_some_and(|b| (0x80..0xc0).contains(&b)) {}
                }
                0x15 => line.clear(),
                0x20..=0x7e | 0x80.. => line.push(b),
                _ => {}
            }
        }
        match done {
            Some(None) => return Ok(0),
            // Replacing what is not UTF-8 would name another file than
            // the one typed.
            Some(Some(answer)) => match String::from_utf8(answer) {
                Ok(answer) => match prompt_request(kind, &answer, pane, &home) {
                    Ok(request) => break request,
                    Err(why) => message = Some(why),
                },
                Err(_) => message = Some("not UTF-8; Ctrl-U clears".into()),
            },
            None => {}
        }
    };
    let _ = write_all(output, b"\x1b[H\x1b[2J");
    drop(raw);
    let (args, cwd) = answer;
    if let Some(dir) = &cwd
        && let Err(e) = std::env::set_current_dir(dir)
    {
        return hold(&format!("{}: {e}", dir.display()));
    }
    if matches!(args.cmd, crate::cli::CtlCmd::Xfer { .. }) {
        let _ = write_all(output, b"xcomm: transfer running (Ctrl-C aborts it)\r\n");
    }
    let status = crate::ctl::client::run(&args, env);
    if status != 0 {
        // The error is above, on stderr: leave it in view. Keys typed
        // while the command ran must not close it.
        let _ = write_all(output, b"\r\npress Enter to close");
        let _ = rustix::termios::tcflush(input, rustix::termios::QueueSelector::IFlush);
        let mut buf = [0u8; 16];
        let _ = rustix::io::read(input, &mut buf);
    }
    Ok(i32::from(status))
}
