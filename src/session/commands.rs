//! What the keys typed after the prefix do.

use std::ffi::OsString;
use std::io;
use std::os::fd::BorrowedFd;
use std::os::unix::ffi::OsStringExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use rustix::event::{PollFd, PollFlags, poll};

use super::{CommandIo, CommandOutcome, Control, ExitReason, SessionError, Signaled, on_signals};
use crate::capture::Capture;
use crate::cli::{LineSettings, PrefixChoice, TermOpts};
use crate::hosttty::{RawMode, SignalPipe};
use crate::keys::{self, EscKey, HELD_MAX, KeyAction, KeyParser};
use crate::port::{self, Applied, ModemLines, Port};
use crate::xfer::{Direction, Proto, Transfer, XferError};

const ESC: u8 = 0x1b;
const BREAK_MS: u32 = 250;
const HANGUP: Duration = Duration::from_millis(500);
/// Starts what xcomm writes to a terminal: CAN ends an escape sequence the
/// device left unfinished, which would otherwise take xcomm's bytes as its
/// own. The rest of the device's sequence then shows as text.
const CANCEL: &str = "\x18";
const WRAP_ON: &[u8] = b"\x18\x1b[?7h";
const WRAP_OFF: &[u8] = b"\x18\x1b[?7l";
/// Longest baud rate answer.
const BAUD_MAX: usize = 32;
/// Longest file name answer, in bytes.
const PATH_MAX: usize = 1024;
const DEFAULT_CAPTURE: &str = "xcomm.cap";
/// Longest protocol answer.
const PROTO_MAX: usize = 8;
const CTRL_C: u8 = 0x03;
/// How long an aborted transfer program gets to stop before SIGKILL.
const ABORT_GRACE: Duration = Duration::from_secs(2);
/// How often a transfer's end is looked for without a SIGCHLD.
const CHILD_CHECK: Duration = Duration::from_millis(500);
/// How long a transfer program gets when the session is ending.
const EXIT_GRACE: Duration = Duration::from_secs(1);

#[derive(Debug, Clone, Copy)]
enum Cmd {
    Help,
    Quit,
    Break,
    HangUp,
    Dtr,
    Rts,
    Baud,
    Echo,
    Wrap,
    Capture,
    Send,
    Receive,
}

/// One line per key typed after `key` (the prefix's name) and what it
/// does, each ended by `eol`: the in-session help and `--help` both.
pub(super) fn listed(key: &str, eol: &str) -> String {
    let mut s = String::new();
    for (letter, _, what) in COMMANDS {
        s.push_str(&format!("  {key} {}  {what}{eol}", *letter as char));
    }
    s.push_str(&format!(
        "  {key} {key}  send {key} itself{eol}  {key} Esc  cancel{eol}"
    ));
    s
}

/// The keys after the prefix, in the order help lists them.
const COMMANDS: &[(u8, Cmd, &str)] = &[
    (b'z', Cmd::Help, "help"),
    (b'x', Cmd::Quit, "quit"),
    (b'q', Cmd::Quit, "quit"),
    (b'f', Cmd::Break, "send break"),
    (b'h', Cmd::HangUp, "hang up (drop DTR for 500 ms)"),
    (b'd', Cmd::Dtr, "toggle DTR"),
    (b't', Cmd::Rts, "toggle RTS"),
    (b'p', Cmd::Baud, "change the baud rate"),
    (b'e', Cmd::Echo, "toggle local echo"),
    (b'w', Cmd::Wrap, "toggle line wrap"),
    (b'l', Cmd::Capture, "start or stop capture"),
    (b's', Cmd::Send, "send a file (zmodem, ymodem, xmodem)"),
    (b'r', Cmd::Receive, "receive files (zmodem, ymodem, xmodem)"),
];

enum Key {
    Byte(u8),
    Timeout,
    Exit(ExitReason),
    /// Stopped and continued.
    Resumed,
    /// End of input, or the terminal hung up.
    Gone,
}

enum EscInPrompt {
    /// The key's byte; ESC for Esc.
    Byte(u8),
    /// A key past ASCII that came coded.
    Char(char),
    Ignore,
    Exit(ExitReason),
}

enum Prompt {
    Line(Vec<u8>),
    Cancelled,
    Exit(ExitReason),
}

/// Removes the last character of `line`: a whole UTF-8 character when the
/// line ends in one, else one byte. False when the line was empty.
fn pop_char(line: &mut Vec<u8>) -> bool {
    let from = line.len().saturating_sub(4);
    let last = (from..line.len()).find_map(|i| match std::str::from_utf8(&line[i..]) {
        Ok(s) if s.chars().count() == 1 => s.chars().next(),
        _ => None,
    });
    match last {
        Some(c) => {
            line.truncate(line.len() - c.len_utf8());
            true
        }
        None => line.pop().is_some(),
    }
}

/// Asks the transfer program to stop, unless it has already ended (its own
/// exit status is then the result, not an abort). When it was asked.
fn start_abort(transfer: &mut Transfer<'_>) -> Option<Instant> {
    match transfer.try_finish() {
        Ok(None) => {
            transfer.abort();
            Some(Instant::now())
        }
        _ => None,
    }
}

/// Whether forwarded bytes hold Ctrl-C: the byte, or the key coded as
/// modifyOtherKeys or CSI-u.
fn has_ctrl_c(bytes: &[u8]) -> bool {
    bytes.contains(&CTRL_C)
        || (0..bytes.len()).any(|i| {
            bytes[i] == ESC
                && matches!(keys::escape_key(&bytes[i..]),
                    EscKey::Done { key, .. } if keys::plain_byte(key) == Some(CTRL_C))
        })
}

fn on_off(on: bool) -> &'static str {
    if on { "on" } else { "off" }
}

/// The interactive session's controller. Stops on SIGTSTP. A session leader
/// (an xcomm started as a tmux or herdr pane command) is in an orphaned
/// process group with no shell to send SIGCONT; it stays stopped until
/// something does (`kill -CONT`).
pub(super) struct Live<'a> {
    raw: Option<&'a RawMode>,
    /// None while the device is disconnected.
    port: Option<&'a Port>,
    prefix: PrefixChoice,
    prefix_time: Duration,
    /// Help, status lines and prompts: stderr, like the other notices, so
    /// that stdout carries device bytes only (`xcomm dev > capture`).
    ui: BorrowedFd<'a>,
    /// CANCEL when `ui` is a terminal.
    ui_cancel: &'static str,
    /// stdout when it is a terminal: where line wrap is switched.
    screen: Option<BorrowedFd<'a>>,
    signals: &'a SignalPipe,
    echo: bool,
    /// Line wrap as xcomm last set it; terminals start with it on.
    wrap: bool,
    capture: Option<Capture>,
    /// DTR and RTS as the user last set them; None where they did not.
    lines: (Option<bool>, Option<bool>),
    /// The last prompt of this command ended on CR: an LF right after it is
    /// the rest of that Enter (a terminal sending CR LF), not an answer.
    cr_ended: bool,
}

/// What a [`Live`] hands to the next one when the device goes away or
/// comes back.
pub(super) struct Kept {
    echo: bool,
    wrap: bool,
    capture: Option<Capture>,
    lines: (Option<bool>, Option<bool>),
}

impl Kept {
    pub(super) fn new(capture: Option<Capture>) -> Kept {
        Kept {
            echo: false,
            wrap: true,
            capture,
            lines: (None, None),
        }
    }
}

impl<'a> Live<'a> {
    pub(super) fn new(
        raw: Option<&'a RawMode>,
        port: Option<&'a Port>,
        opts: &TermOpts,
        stdout: BorrowedFd<'a>,
        stderr: BorrowedFd<'a>,
        signals: &'a SignalPipe,
        kept: Kept,
    ) -> Live<'a> {
        Live {
            raw,
            port,
            prefix: opts.prefix,
            prefix_time: opts.prefix_time,
            ui: stderr,
            ui_cancel: if rustix::termios::isatty(stderr) {
                CANCEL
            } else {
                ""
            },
            screen: rustix::termios::isatty(stdout).then_some(stdout),
            signals,
            echo: kept.echo,
            wrap: kept.wrap,
            capture: kept.capture,
            lines: kept.lines,
            cr_ended: false,
        }
    }

    pub(super) fn into_kept(self) -> Kept {
        Kept {
            echo: self.echo,
            wrap: self.wrap,
            capture: self.capture,
            lines: self.lines,
        }
    }

    /// Writes out the capture, and puts back what commands changed on the
    /// terminal.
    pub(super) fn finish(&mut self) {
        if let Some(c) = self.capture.take() {
            let path = c.path().to_path_buf();
            if let Err(e) = c.close() {
                self.say(&format!("capture stopped: {}: {e}", path.display()));
            }
        }
        if !self.wrap {
            self.set_wrap_if_writable(true);
        }
    }

    /// Writes out what the capture holds; an error stops it.
    pub(super) fn flush_capture(&mut self) {
        if let Some(Err(e)) = self.capture.as_mut().map(|c| c.flush()) {
            self.capture_failed(e);
        }
    }

    fn toggle_capture(&mut self, io: &mut CommandIo<'_>) -> Result<CommandOutcome, SessionError> {
        if let Some(c) = self.capture.take() {
            let (path, total) = (c.path().to_path_buf(), c.total());
            match c.close() {
                Ok(()) => self.say(&format!(
                    "capture closed: {} ({total} bytes)",
                    path.display()
                )),
                Err(e) => self.say(&format!("capture stopped: {}: {e}", path.display())),
            }
            return Ok(CommandOutcome::Continue);
        }
        let text = format!("capture file [{DEFAULT_CAPTURE}]: ");
        let line = match self.prompt(&text, PATH_MAX, true, io)? {
            Prompt::Line(line) => line,
            Prompt::Cancelled => return Ok(CommandOutcome::Continue),
            Prompt::Exit(exit) => return Ok(CommandOutcome::Exit(exit)),
        };
        let path = if line.is_empty() {
            PathBuf::from(DEFAULT_CAPTURE)
        } else {
            PathBuf::from(OsString::from_vec(line))
        };
        match Capture::start(&path) {
            Ok(c) => {
                self.say_after_prompt(&format!("capturing to {}", path.display()));
                self.capture = Some(c);
            }
            Err(e) => self.say_after_prompt(&format!("capture: {e}")),
        }
        Ok(CommandOutcome::Continue)
    }

    /// Switches line wrap when a blocked write might not be interrupted:
    /// the signal ending, stopping or continuing the session has been taken
    /// (one arriving while stopped is taken as the session continues). The
    /// sequence goes out only if the terminal takes output within 100 ms. A
    /// terminal reports that once its queue is nearly empty, so the few
    /// bytes then go in at once; a stalled one is left as it is rather than
    /// keep xcomm from exiting or stopping.
    fn set_wrap_if_writable(&self, on: bool) {
        let Some(fd) = self.screen else {
            return;
        };
        let mut fds = [PollFd::new(&fd, PollFlags::OUT)];
        let wait = rustix::event::Timespec {
            tv_sec: 0,
            tv_nsec: 100_000_000,
        };
        if matches!(poll(&mut fds, Some(&wait)), Ok(1)) && fds[0].revents().contains(PollFlags::OUT)
        {
            let _ = rustix::io::write(fd, if on { WRAP_ON } else { WRAP_OFF });
        }
    }

    /// Best effort: a terminal that is gone is noticed by the loop. A signal
    /// gives up on the rest, so it is acted on without waiting for a
    /// terminal that is not being read.
    fn write_to(&self, fd: BorrowedFd<'_>, mut bytes: &[u8]) {
        while !bytes.is_empty() {
            match rustix::io::write(fd, bytes) {
                Ok(n) => bytes = &bytes[n..],
                // Non-blocking (set so by another program): wait for room.
                Err(rustix::io::Errno::AGAIN) => {
                    let signal_fd = self.signals.fd();
                    let mut fds = [
                        PollFd::new(&fd, PollFlags::OUT),
                        PollFd::new(&signal_fd, PollFlags::IN),
                    ];
                    if poll(&mut fds, None).is_err() || fds[1].revents().contains(PollFlags::IN) {
                        return;
                    }
                }
                Err(_) => return,
            }
        }
    }

    fn write(&self, bytes: &[u8]) {
        self.write_to(self.ui, bytes);
    }

    pub(super) fn set_wrap(&self, on: bool) {
        if let Some(fd) = self.screen {
            self.write_to(fd, if on { WRAP_ON } else { WRAP_OFF });
        }
    }

    /// A status line, on a line of its own after whatever the device sent.
    pub(super) fn say(&self, msg: &str) {
        self.write(format!("{}\r\nxcomm: {msg}\r\n", self.ui_cancel).as_bytes());
    }

    /// A status line after a prompt, which already ended its line.
    fn say_after_prompt(&self, msg: &str) {
        self.write(format!("xcomm: {msg}\r\n").as_bytes());
    }

    fn help(&self) {
        let key = self.prefix.key.display();
        let mut s = format!(
            "{}\r\nxcomm commands, typed after {key}:\r\n",
            self.ui_cancel
        );
        s.push_str(&listed(&key, "\r\n"));
        s.push_str(&format!("prefix: {}\r\n", self.prefix.describe()));
        self.write(s.as_bytes());
    }

    /// Keys that cannot reach the device: one bell.
    pub(super) fn bell(&self) {
        self.write(format!("{}\x07", self.ui_cancel).as_bytes());
    }

    fn hang_up(&mut self, port: &Port, io: &CommandIo<'_>) {
        match port.modem_lines() {
            Ok(m) if !m.dtr => {
                self.say("hang up: DTR is off already");
                return;
            }
            Ok(_) => {}
            Err(e) => {
                self.say(&format!("hang up: {e}"));
                return;
            }
        }
        if let Err(e) = port.set_dtr(false) {
            self.say(&format!("hang up: {e}"));
            return;
        }
        // A signal cuts the wait short, and stays pending for the loop.
        let signal_fd = io.signals.fd();
        let mut fds = [PollFd::new(&signal_fd, PollFlags::IN)];
        let wait = rustix::event::Timespec::try_from(HANGUP).expect("500 ms fits");
        let started = Instant::now();
        let cut_short = !matches!(poll(&mut fds, Some(&wait)), Ok(0));
        match port.set_dtr(true) {
            Ok(()) if cut_short => self.say(&format!(
                "hang up cut short by a signal: DTR dropped for {} ms",
                started.elapsed().as_millis()
            )),
            Ok(()) => self.say("hung up (DTR dropped for 500 ms)"),
            Err(e) => {
                self.lines.0 = Some(false);
                self.say(&format!("hang up: DTR is left off: {e}"));
            }
        }
    }

    /// Flips a modem line and reports what the driver reads back.
    /// The line's state after, when the driver could be asked.
    fn toggle(
        &self,
        port: &Port,
        name: &str,
        get: fn(&ModemLines) -> bool,
        set: fn(&Port, bool) -> io::Result<()>,
    ) -> Option<bool> {
        let result = port.modem_lines().and_then(|m| {
            let want = !get(&m);
            set(port, want)?;
            Ok((want, get(&port.modem_lines()?)))
        });
        match result {
            Ok((want, now)) if want == now => {
                self.say(&format!("{name} {}", on_off(now)));
                Some(now)
            }
            Ok((_, now)) => {
                self.say(&format!(
                    "{name} is still {}: the driver did not change it",
                    on_off(now)
                ));
                Some(now)
            }
            Err(e) => {
                self.say(&format!("{name}: {e}"));
                None
            }
        }
    }

    /// Sets DTR and RTS on a port opened again as the user last set them.
    pub(super) fn restore_lines(&self, port: &Port) -> Vec<String> {
        let mut notes = Vec::new();
        for (state, name, set) in [
            (
                self.lines.0,
                "DTR",
                Port::set_dtr as fn(&Port, bool) -> io::Result<()>,
            ),
            (self.lines.1, "RTS", Port::set_rts),
        ] {
            if let Some(on) = state {
                match set(port, on) {
                    Ok(()) => notes.push(format!("{name} {}, as it was", on_off(on))),
                    Err(e) => notes.push(format!("{name}: {e}")),
                }
            }
        }
        notes
    }

    fn change_baud(
        &mut self,
        port: &Port,
        io: &mut CommandIo<'_>,
    ) -> Result<CommandOutcome, SessionError> {
        let current = port.settings();
        let line = match self.prompt(&format!("baud [{}]: ", current.baud), BAUD_MAX, false, io)? {
            Prompt::Line(line) => String::from_utf8_lossy(&line).into_owned(),
            Prompt::Cancelled => return Ok(CommandOutcome::Continue),
            Prompt::Exit(exit) => return Ok(CommandOutcome::Exit(exit)),
        };
        if line.is_empty() {
            return Ok(CommandOutcome::Continue);
        }
        let baud = line
            .parse::<u32>()
            .ok()
            .filter(|&b| b > 0 && line.bytes().all(|c| c.is_ascii_digit()));
        let Some(baud) = baud else {
            self.say_after_prompt(&format!("invalid baud rate '{line}'"));
            return Ok(CommandOutcome::Continue);
        };
        let wanted = LineSettings { baud, ..current };
        match port.configure(&wanted) {
            Ok(Applied::Exact) => self.say_after_prompt(&port::describe(&wanted)),
            Ok(Applied::Different(actual)) => self.say_after_prompt(&format!(
                "requested {}; the driver applied {}",
                port::describe(&wanted),
                port::describe(&actual)
            )),
            Err(e) => self.say_after_prompt(&e.to_string()),
        }
        Ok(CommandOutcome::Continue)
    }

    /// Asks for the protocol and the file, then runs lrzsz on the port
    /// until it ends. The device is not read meanwhile: its bytes are the
    /// program's. Keys are dropped, except the prefix and `x` (or `q`) or
    /// Ctrl-C, which abort the transfer.
    fn transfer(
        &mut self,
        port: &Port,
        dir: Direction,
        io: &mut CommandIo<'_>,
    ) -> Result<CommandOutcome, SessionError> {
        let line = match self.prompt(
            "protocol (zmodem, ymodem, xmodem) [zmodem]: ",
            PROTO_MAX,
            false,
            io,
        )? {
            Prompt::Line(line) => String::from_utf8_lossy(&line).into_owned(),
            Prompt::Cancelled => return Ok(CommandOutcome::Continue),
            Prompt::Exit(exit) => return Ok(CommandOutcome::Exit(exit)),
        };
        let proto = if line.is_empty() {
            Proto::Zmodem
        } else {
            match Proto::parse(&line) {
                Some(proto) => proto,
                None => {
                    self.say_after_prompt(&format!("unknown protocol '{line}'"));
                    return Ok(CommandOutcome::Continue);
                }
            }
        };
        // ZMODEM and YMODEM receivers take the names from the sender.
        let ask = match (dir, proto) {
            (Direction::Send, _) => Some("file to send: "),
            (Direction::Receive, Proto::Xmodem) => Some("save as: "),
            (Direction::Receive, _) => None,
        };
        let mut files = Vec::new();
        if let Some(text) = ask {
            match self.prompt(text, PATH_MAX, true, io)? {
                Prompt::Line(line) if line.is_empty() => return Ok(CommandOutcome::Continue),
                Prompt::Line(line) => files.push(PathBuf::from(OsString::from_vec(line))),
                Prompt::Cancelled => return Ok(CommandOutcome::Continue),
                Prompt::Exit(exit) => return Ok(CommandOutcome::Exit(exit)),
            }
        }
        // Before the program starts: its first line comes at once.
        self.process_output(true);
        let mut transfer = match Transfer::start(port, proto, dir, &files, Path::new(".")) {
            Ok(t) => t,
            Err(e) => {
                self.process_output(false);
                self.say_after_prompt(&format!("transfer failed: {e}"));
                return Ok(CommandOutcome::Continue);
            }
        };
        let key = self.prefix.key.display();
        // With Ctrl-C as the prefix, Ctrl-C alone waits for the command key.
        let ctrl_c_prefix = self.prefix.key.modifier == crate::cli::Modifier::Ctrl
            && self.prefix.key.letter == b'c';
        let how = if ctrl_c_prefix {
            format!("{key} x aborts")
        } else {
            format!("{key} x or Ctrl-C aborts")
        };
        self.say_after_prompt(&format!("transfer started: {} ({how})", transfer.program()));
        let ended = self.wait_transfer(&mut transfer, io);
        if let Err(e) = transfer.release() {
            self.say(&format!("after the transfer: port: {e}"));
        }
        self.process_output(false);
        // lrzsz switches the port's modes while it runs; one killed cannot
        // put them back. Before the result, which means it is all over.
        let settings = port.settings();
        if let Err(e) = port.configure(&settings) {
            self.say(&format!("after the transfer: {e}"));
        }
        match ended? {
            Ok(result) => {
                self.say(&result);
                Ok(CommandOutcome::Continue)
            }
            Err(exit) => Ok(CommandOutcome::Exit(exit)),
        }
    }

    /// lrzsz ends its progress lines with LF only.
    fn process_output(&self, on: bool) {
        if let Some(Err(e)) = self.raw.map(|r| r.process_output(on)) {
            self.say(&format!("terminal: {e}"));
        }
    }

    /// Waits for the transfer program to end. Gives the line saying how it
    /// went, or why the session ends.
    fn wait_transfer(
        &mut self,
        transfer: &mut Transfer<'_>,
        io: &mut CommandIo<'_>,
    ) -> Result<Result<String, ExitReason>, SessionError> {
        let mut keys = KeyParser::new(self.prefix.key, self.prefix_time);
        // Keys typed after the command key are dropped too, unless they abort.
        let mut typed = std::mem::take(&mut io.typed);
        let mut stdin_open = true;
        let mut aborted: Option<Instant> = None;
        let signal_fd = io.signals.fd();
        let mut buf = [0u8; 256];
        loop {
            match transfer.try_finish() {
                // Done before the abort took effect: done.
                Ok(Some(_)) => return Ok(Ok(format!("transfer done: {}", transfer.program()))),
                Err(XferError::Failed { .. }) if aborted.is_some() => {
                    return Ok(Ok("transfer aborted".into()));
                }
                Err(e) => return Ok(Ok(format!("transfer failed: {e}"))),
                Ok(None) => {}
            }
            if aborted.is_some_and(|t| t.elapsed() >= ABORT_GRACE) {
                transfer.kill();
                continue;
            }
            let mut abort = false;
            while !typed.is_empty() {
                let mut command = None;
                let used = keys.feed(typed, Instant::now(), &mut |a| match a {
                    KeyAction::Forward(b) => abort |= has_ctrl_c(b),
                    KeyAction::Command(c) => command = Some(c),
                });
                typed = &typed[used..];
                abort |= matches!(command, Some(b'x' | b'q'));
            }
            if abort && aborted.is_none() {
                aborted = start_abort(transfer);
            }
            // SIGCHLD wakes the loop, unless the parent left it blocked:
            // look now and then anyway.
            let due = [
                keys.deadline(),
                aborted.map(|t| t + ABORT_GRACE),
                Some(Instant::now() + CHILD_CHECK),
            ]
            .into_iter()
            .flatten()
            .min();
            let timeout = due.map(|due| {
                rustix::event::Timespec::try_from(due.saturating_duration_since(Instant::now()))
                    .expect("under ABORT_GRACE")
            });
            let (stdin_slot, stdin_events) = if stdin_open {
                (io.stdin, PollFlags::IN)
            } else {
                (signal_fd, PollFlags::empty())
            };
            let mut fds = [
                PollFd::new(&signal_fd, PollFlags::IN),
                PollFd::new(&stdin_slot, stdin_events),
            ];
            match poll(&mut fds, timeout.as_ref()) {
                Ok(_) | Err(rustix::io::Errno::INTR) => {}
                Err(e) => return Err(SessionError(format!("poll: {e}"))),
            }
            // A lone ESC held past its time is a key like any other here.
            keys.on_deadline(Instant::now(), &mut |a| {
                if let KeyAction::Forward(b) = a {
                    abort |= has_ctrl_c(b);
                }
            });
            if fds[0].revents().contains(PollFlags::IN) {
                // SIGCHLD only wakes the loop; try_finish looks.
                let bits = io
                    .signals
                    .drain()
                    .map_err(|e| SessionError(e.to_string()))?;
                let signaled = on_signals(bits, io.signals, self)?;
                if let Signaled::Resumed = signaled {
                    // Raw mode came back as it was entered. Readiness seen
                    // before the stop is stale (the shell may have read the
                    // key): poll again.
                    self.process_output(true);
                    if abort && aborted.is_none() {
                        aborted = start_abort(transfer);
                    }
                    continue;
                }
                if let Signaled::Exit(exit) = signaled {
                    transfer.abort();
                    let deadline = Instant::now() + EXIT_GRACE;
                    while matches!(transfer.try_finish(), Ok(None)) && Instant::now() < deadline {
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    transfer.kill();
                    return Ok(Err(exit));
                }
            }
            if stdin_open
                && fds[1]
                    .revents()
                    .intersects(PollFlags::IN | PollFlags::HUP | PollFlags::ERR)
            {
                match rustix::io::read(io.stdin, &mut buf) {
                    Ok(0) => stdin_open = false,
                    Ok(n) => {
                        let mut rest = &buf[..n];
                        while !rest.is_empty() {
                            let mut command = None;
                            let used = keys.feed(rest, Instant::now(), &mut |a| match a {
                                KeyAction::Forward(b) => abort |= has_ctrl_c(b),
                                KeyAction::Command(c) => command = Some(c),
                            });
                            rest = &rest[used..];
                            abort |= matches!(command, Some(b'x' | b'q'));
                        }
                    }
                    Err(rustix::io::Errno::AGAIN | rustix::io::Errno::INTR) => {}
                    Err(_) => stdin_open = false,
                }
            }
            if abort && aborted.is_none() {
                aborted = start_abort(transfer);
            }
        }
    }

    /// Reads a line of up to `max` bytes of printable ASCII (and with
    /// `utf8`, UTF-8 characters, each taken whole or not at all) from the
    /// keyboard: Backspace edits, Enter finishes, Esc or Ctrl-C cancels.
    /// Device output waits meanwhile.
    fn prompt(
        &mut self,
        text: &str,
        max: usize,
        utf8: bool,
        io: &mut CommandIo<'_>,
    ) -> Result<Prompt, SessionError> {
        self.write(format!("{}\r\n{text}", self.ui_cancel).as_bytes());
        let mut line: Vec<u8> = Vec::new();
        // Continuation bytes still to come of a UTF-8 character: to take
        // (`need`, the character starting at `start`) or, for one too long
        // for the line, to drop (`skip`).
        let mut need = 0usize;
        let mut start = 0usize;
        let mut skip = 0usize;
        loop {
            let b = match io.typed.split_first() {
                Some((&b, more)) => {
                    io.typed = more;
                    b
                }
                None => match self.read_key(io, None)? {
                    Key::Byte(b) => b,
                    Key::Timeout => continue,
                    Key::Exit(exit) => return Ok(Prompt::Exit(exit)),
                    Key::Resumed => {
                        self.write(format!("\r\n{text}").as_bytes());
                        // Not a character still being typed: it is shown
                        // once whole.
                        self.write(&line[..if need > 0 { start } else { line.len() }]);
                        continue;
                    }
                    // Let the loop see the keyboard gone again.
                    Key::Gone => {
                        self.write(b"\r\n");
                        return Ok(Prompt::Cancelled);
                    }
                },
            };
            let b = if b == ESC {
                match self.escape_in_prompt(io)? {
                    EscInPrompt::Byte(b) => b,
                    EscInPrompt::Char(c) => {
                        if need > 0 {
                            line.truncate(start);
                            need = 0;
                        }
                        let mut bytes = [0u8; 4];
                        let bytes = c.encode_utf8(&mut bytes).as_bytes();
                        if utf8 && line.len() + bytes.len() <= max {
                            line.extend_from_slice(bytes);
                            self.write(bytes);
                        }
                        continue;
                    }
                    EscInPrompt::Ignore => continue,
                    EscInPrompt::Exit(exit) => return Ok(Prompt::Exit(exit)),
                }
            } else {
                b
            };
            // A character cut short by another byte is taken out again; it
            // was not shown yet.
            if need > 0 && !(0x80..=0xbf).contains(&b) {
                line.truncate(start);
                need = 0;
                if matches!(b, 0x7f | 0x08) {
                    continue;
                }
            }
            let after_cr = std::mem::take(&mut self.cr_ended);
            match b {
                b'\n' if after_cr && line.is_empty() => {}
                b'\r' | b'\n' => {
                    self.cr_ended = b == b'\r';
                    self.write(b"\r\n");
                    return Ok(Prompt::Line(line));
                }
                0x03 | ESC => {
                    self.write(b"\r\n");
                    return Ok(Prompt::Cancelled);
                }
                0x7f | 0x08 => {
                    if pop_char(&mut line) {
                        self.erase_last(text, &line);
                    }
                }
                0x80..=0xbf if skip > 0 => skip -= 1,
                // Shown once the character is whole.
                0x80..=0xbf if need > 0 => {
                    need -= 1;
                    line.push(b);
                    if need == 0 {
                        // Overlong forms, surrogates and past U+10FFFF are
                        // not characters.
                        if std::str::from_utf8(&line[start..]).is_ok() {
                            self.write(&line[start..]);
                        } else {
                            line.truncate(start);
                        }
                    }
                }
                0x20..=0x7e if line.len() < max => {
                    skip = 0;
                    line.push(b);
                    self.write(&[b]);
                }
                0xc2..=0xf4 if utf8 => {
                    let len = match b {
                        0xc2..=0xdf => 2,
                        0xe0..=0xef => 3,
                        _ => 4,
                    };
                    skip = 0;
                    if line.len() + len <= max {
                        start = line.len();
                        need = len - 1;
                        line.push(b);
                    } else {
                        skip = len - 1;
                    }
                }
                _ => {}
            }
        }
    }

    /// Takes the last character off the screen after Backspace. When the
    /// prompt and the answer surely fit on one row, the row is drawn again, whatever width the
    /// removed character had. Otherwise the cursor steps back one cell: a
    /// wrapped answer cannot be redrawn in place, as the screen may have
    /// scrolled.
    fn erase_last(&self, text: &str, line: &[u8]) {
        // A character never takes more cells than it has bytes.
        let most_cells = text.len() + line.len() + 1;
        let fits = rustix::termios::tcgetwinsize(self.ui)
            .is_ok_and(|w| most_cells < usize::from(w.ws_col));
        if fits {
            self.write(format!("\r{text}").as_bytes());
            self.write(line);
            self.write(b"\x1b[K");
        } else {
            self.write(b"\x08 \x08");
        }
    }

    /// Reads the rest of a key that began with ESC. A lone ESC (nothing
    /// more within --prefix-time) is Esc; a coded key (kitty keyboard
    /// protocol, modifyOtherKeys) counts as its byte; any other key (an
    /// arrow, an Alt-letter) is ignored whole, so none of it is left over for
    /// the device.
    fn escape_in_prompt(&mut self, io: &mut CommandIo<'_>) -> Result<EscInPrompt, SessionError> {
        let mut seq = [0u8; HELD_MAX];
        seq[0] = ESC;
        let mut len = 1;
        loop {
            // Done by HELD_MAX bytes at the latest.
            if let EscKey::Done { key, .. } = keys::escape_key(&seq[..len]) {
                return Ok(match (keys::plain_byte(key), keys::plain_char(key)) {
                    (Some(b), _) => EscInPrompt::Byte(b),
                    (None, Some(c)) => EscInPrompt::Char(c),
                    (None, None) => EscInPrompt::Ignore,
                });
            }
            seq[len] = match io.typed.split_first() {
                Some((&b, more)) => {
                    io.typed = more;
                    b
                }
                None => match self.read_key(io, Some(self.prefix_time))? {
                    Key::Byte(b) => b,
                    Key::Exit(exit) => return Ok(EscInPrompt::Exit(exit)),
                    Key::Timeout | Key::Resumed | Key::Gone => return Ok(EscInPrompt::Byte(ESC)),
                },
            };
            len += 1;
        }
    }

    /// One byte from the keyboard, so keys after the prompt's Enter stay
    /// unread for the loop. Signals are handled as in the loop.
    fn read_key(
        &mut self,
        io: &CommandIo<'_>,
        timeout: Option<Duration>,
    ) -> Result<Key, SessionError> {
        let signal_fd = io.signals.fd();
        let timeout = timeout.map(|t| rustix::event::Timespec::try_from(t).expect("fits"));
        loop {
            let mut fds = [
                PollFd::new(&io.stdin, PollFlags::IN),
                PollFd::new(&signal_fd, PollFlags::IN),
            ];
            match poll(&mut fds, timeout.as_ref()) {
                Ok(0) => return Ok(Key::Timeout),
                Ok(_) => {}
                Err(rustix::io::Errno::INTR) => continue,
                Err(e) => return Err(SessionError(format!("poll: {e}"))),
            }
            let stdin_ready = fds[0].revents();
            if fds[1].revents().contains(PollFlags::IN) {
                let bits = io
                    .signals
                    .drain()
                    .map_err(|e| SessionError(e.to_string()))?;
                match on_signals(bits, io.signals, self)? {
                    Signaled::Exit(exit) => return Ok(Key::Exit(exit)),
                    Signaled::Resumed => return Ok(Key::Resumed),
                    Signaled::Nothing => {}
                }
            }
            if !stdin_ready.intersects(PollFlags::IN | PollFlags::HUP | PollFlags::ERR) {
                continue;
            }
            let mut b = [0u8; 1];
            return match rustix::io::read(io.stdin, &mut b) {
                Ok(1) => Ok(Key::Byte(b[0])),
                Err(rustix::io::Errno::AGAIN | rustix::io::Errno::INTR) => continue,
                Ok(_) | Err(_) => Ok(Key::Gone),
            };
        }
    }
}

impl Control for Live<'_> {
    fn stop(&mut self) -> io::Result<()> {
        self.flush_capture();
        if !self.wrap {
            self.set_wrap_if_writable(true);
        }
        if let Some(raw) = self.raw {
            raw.suspend()?;
        }
        Ok(())
    }

    fn halt(&mut self) {
        // SAFETY: raise(3) is safe to call; SIGSTOP stops this process here
        // and execution continues after SIGCONT.
        unsafe { libc::raise(libc::SIGSTOP) };
    }

    fn resume(&mut self) -> io::Result<()> {
        if let Some(raw) = self.raw {
            raw.reenter()?;
        }
        if !self.wrap {
            self.set_wrap_if_writable(false);
        }
        Ok(())
    }

    fn command(&mut self, key: u8, io: &mut CommandIo<'_>) -> Result<CommandOutcome, SessionError> {
        self.cr_ended = false;
        if key == ESC {
            return Ok(CommandOutcome::Continue);
        }
        let Some(&(_, cmd, what)) = COMMANDS.iter().find(|(k, ..)| *k == key) else {
            self.bell();
            return Ok(CommandOutcome::Continue);
        };
        // A command can take a while (a prompt, a break): what was captured
        // goes to the file first. Stopping the capture does that itself, and
        // a failed flush here would turn that stop into a start.
        if !matches!(cmd, Cmd::Capture) {
            self.flush_capture();
        }
        match (cmd, self.port) {
            (Cmd::Help, _) => self.help(),
            (Cmd::Quit, _) => return Ok(CommandOutcome::Exit(ExitReason::Quit)),
            (Cmd::Echo, _) => {
                self.echo = !self.echo;
                self.say(&format!("local echo {}", on_off(self.echo)));
            }
            (Cmd::Capture, _) => return self.toggle_capture(io),
            (Cmd::Wrap, _) if self.screen.is_none() => {
                self.say("line wrap: the output is not a terminal");
            }
            (Cmd::Wrap, _) => {
                self.wrap = !self.wrap;
                self.set_wrap(self.wrap);
                self.say(&format!("line wrap {}", on_off(self.wrap)));
            }
            (_, None) => self.say(&format!("{what}: the device is disconnected")),
            (Cmd::Break, Some(port)) => match port.send_break(BREAK_MS) {
                Ok(()) => self.say("break sent"),
                Err(e) => self.say(&e.to_string()),
            },
            (Cmd::HangUp, Some(port)) => self.hang_up(port, io),
            (Cmd::Dtr, Some(port)) => {
                if let Some(on) = self.toggle(port, "DTR", |m| m.dtr, Port::set_dtr) {
                    self.lines.0 = Some(on);
                }
            }
            (Cmd::Rts, Some(port)) => {
                if let Some(on) = self.toggle(port, "RTS", |m| m.rts, Port::set_rts) {
                    self.lines.1 = Some(on);
                }
            }
            (Cmd::Baud, Some(port)) => return self.change_baud(port, io),
            (Cmd::Send, Some(port)) => return self.transfer(port, Direction::Send, io),
            (Cmd::Receive, Some(port)) => return self.transfer(port, Direction::Receive, io),
        }
        Ok(CommandOutcome::Continue)
    }

    fn local_echo(&self) -> bool {
        self.echo
    }

    fn capture(&mut self) -> Option<&mut Capture> {
        self.capture.as_mut()
    }

    fn capture_failed(&mut self, error: io::Error) {
        if let Some(c) = self.capture.take() {
            self.say(&format!("capture stopped: {}: {error}", c.path().display()));
        }
    }

    fn dropping_input(&mut self) {
        self.say(&format!(
            "the device has taken no input for {} s: typed keys are dropped until it does ({} x quits)",
            super::STALL.as_secs(),
            self.prefix.key.display()
        ));
    }
}
