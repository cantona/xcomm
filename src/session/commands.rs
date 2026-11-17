// SPDX-FileCopyrightText: 2026 Su Kang Yin <paradyse@gmail.com>
// SPDX-License-Identifier: GPL-2.0-only

//! What the keys typed after the prefix do.

use std::ffi::OsString;
use std::io;
use std::os::fd::BorrowedFd;
use std::os::unix::ffi::OsStringExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use rustix::event::{PollFd, PollFlags, poll};

use super::outq::OutQueue;
use super::{
    CommandIo, CommandOutcome, Control, ExitReason, STALL, SessionError, Signaled,
    on_signals,
};
use crate::capture::Capture;
use crate::cli::{LineSettings, PrefixChoice, TermOpts};
use crate::ctl::{
    self, proto,
    server::{Action, CtlServer},
};
use crate::hosttty::{RawMode, SignalPipe};
use crate::keys::{self, EscKey, HELD_MAX, KeyAction, KeyParser};
use crate::port::{self, Applied, ModemLines, Port};
use crate::ring::Ring;
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

/// Reads one modem line, and sets it.
type LineGet = fn(&ModemLines) -> bool;
type LineSet = fn(&Port, bool) -> io::Result<()>;

/// Why the device cannot be acted on, for reply code `code`.
fn why_not(code: u8) -> &'static str {
    if code == ctl::code::BUSY {
        "a file transfer has the port"
    } else {
        "the device is disconnected"
    }
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
    /// Show what control clients send on `screen`, and how.
    marker: Option<MarkerStyle>,
    /// Markers not shown yet: the device's output stopped inside an escape
    /// sequence or a character. Since when, to show them anyway later.
    markers: Vec<u8>,
    markers_since: Option<Instant>,
    /// The rest of markers a signal cut short, written as they are next.
    unshown: std::cell::RefCell<Vec<u8>>,
    /// Sends not shown: the markers waiting had reached MARKERS_MAX.
    unmarked: usize,
    /// When to try `unshown` again, and the wait after that: longer while
    /// the terminal takes nothing.
    retry_at: Option<Instant>,
    retry: Duration,
    /// No terminal at all (`--headless`).
    headless: bool,
    herdr: PaneLabel,
    signals: &'a SignalPipe,
    echo: bool,
    /// Line wrap as xcomm last set it; terminals start with it on.
    wrap: bool,
    capture: Option<Capture>,
    /// Device output by offset; it outlives a disconnect, so offsets go on.
    ring: Ring,
    /// The control socket; None with --ctl off.
    server: Option<CtlServer>,
    names: Names,
    /// A file transfer has the port.
    transferring: bool,
    /// A client's break or pulse under way.
    hold: Option<Hold>,
    /// A client's action waiting for what was queued before it.
    action_wait: Option<ActionWait>,
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
    ring: Ring,
    server: Option<CtlServer>,
    names: Names,
    lines: (Option<bool>, Option<bool>),
    /// Show what control clients send, and how.
    marker: Option<MarkerStyle>,
    herdr: PaneLabel,
}

/// This session's herdr pane, named after the port and its state.
pub(super) struct PaneLabel {
    herdr: Option<crate::herdr::Herdr>,
    /// A request failed: said once, and no more are made.
    failed: bool,
    shown: Option<PaneState>,
    /// When to report again, before the last report expires.
    renew_at: Option<Instant>,
}

/// What the pane's name and labels were last set from.
#[derive(Clone, PartialEq, Eq)]
struct PaneState {
    tty: String,
    baud: u32,
    connected: bool,
    xfer: bool,
    cap: bool,
}

impl PaneLabel {
    pub(super) fn new(herdr: Option<crate::herdr::Herdr>) -> PaneLabel {
        PaneLabel {
            herdr,
            failed: false,
            shown: None,
            renew_at: None,
        }
    }
}

impl Kept {
    pub(super) fn new(
        capture: Option<Capture>,
        ring: Ring,
        server: Option<CtlServer>,
        names: Names,
        marker: Option<MarkerStyle>,
        herdr: PaneLabel,
    ) -> Kept {
        Kept {
            echo: false,
            wrap: true,
            capture,
            ring,
            server,
            names,
            lines: (None, None),
            marker,
            herdr,
        }
    }
}

/// What `status` names the session by.
pub(super) struct Names {
    /// The device as given.
    pub(super) device: String,
    /// The name it is reopened by.
    pub(super) stable: String,
    /// The tty it is now.
    pub(super) tty: String,
    pub(super) herdr_pane: Option<String>,
}

/// A break or a pulse a client asked for: the session goes on meanwhile.
struct Hold {
    id: u64,
    started: Instant,
    until: Instant,
    kind: HoldKind,
}

enum HoldKind {
    Break,
    /// The line is at the other state; `back` is the one to restore.
    Pulse {
        line: proto::ModemLine,
        back: bool,
    },
}

/// Keys are written in the pass that read them only with no client break,
/// pulse or action in hand: one in hand orders itself against the queue
/// (`line_ready`), and needs to see the keys there to keep its place.
fn keys_go_at_once(hold: Option<&Hold>, action: Option<&ActionWait>) -> bool {
    hold.is_none() && action.is_none()
}

/// An action waits for the bytes queued before it to go out, until the
/// line has taken nothing for STALL (a line held off would otherwise keep
/// the commands that free it from running). Bytes queued after it do not
/// count.
struct ActionWait {
    /// The client whose action it is.
    id: u64,
    /// Since when nothing went out.
    since: Instant,
    phase: WaitPhase,
}

enum WaitPhase {
    /// The queue still holds some: out once `written` reaches `until`.
    Queue { until: u64, written: u64 },
    /// The driver holds what is left; nothing more is written meanwhile,
    /// and no event says when it is out. `pending` as last seen.
    Driver { pending: usize },
}

/// How often the driver's queue is looked at while an action waits on it.
const DRIVER_CHECK: Duration = Duration::from_millis(10);

/// The getter and setter of a modem line.
fn line_ops(line: proto::ModemLine) -> (LineGet, LineSet) {
    match line {
        proto::ModemLine::Dtr => (|m| m.dtr, Port::set_dtr),
        proto::ModemLine::Rts => (|m| m.rts, Port::set_rts),
    }
}

/// A transfer to run, and who asked for it.
struct TransferRun<'r> {
    proto: Proto,
    dir: Direction,
    files: &'r [PathBuf],
    cwd: &'r Path,
    /// The keyboard, read for the keys that abort it; None without one.
    stdin: Option<BorrowedFd<'r>>,
    /// Keys typed after the command key, not handled yet.
    typed: &'r [u8],
    /// The control client that asked for it.
    requester: Option<u64>,
    /// Started from a prompt (the line it said goes after it).
    after_prompt: bool,
}

/// What the control socket sees of the session while it serves it.
struct View<'v> {
    port: Option<&'v Port>,
    ring: &'v Ring,
    capture: Option<&'v Capture>,
    names: &'v Names,
    transferring: bool,
    /// Where sent bytes go, or the reply code why they cannot.
    queue: Result<&'v mut OutQueue, u8>,
    /// Whether to show sent bytes, how, and where they wait to be shown.
    marker: Option<MarkerStyle>,
    markers: &'v mut Vec<u8>,
    unmarked: &'v mut usize,
}

impl ctl::server::Session for View<'_> {
    fn status(&self) -> proto::Status {
        let settings = self.port.map(|p| proto::SettingsView::of(&p.settings()));
        let modem =
            self.port
                .and_then(|p| p.modem_lines().ok())
                .map(|m| proto::ModemView {
                    dtr: m.dtr,
                    rts: m.rts,
                    cts: m.cts,
                    dsr: m.dsr,
                    dcd: m.dcd,
                    ri: m.ri,
                });
        proto::Status {
            device: self.names.device.clone(),
            tty: self.names.tty.clone(),
            stable: self.names.stable.clone(),
            settings,
            modem,
            connected: self.port.is_some(),
            offset: self.ring.end(),
            oldest: self.ring.oldest(),
            capture: self.capture.map(|c| c.path().display().to_string()),
            transfer: self.transferring,
            ctl: String::new(),
            herdr_pane: self.names.herdr_pane.clone(),
            pid: std::process::id(),
        }
    }

    fn ring(&self) -> &Ring {
        self.ring
    }

    fn send(&mut self, bytes: &[u8]) -> Result<usize, (u8, String)> {
        match &mut self.queue {
            Ok(queue) => {
                let n = bytes
                    .len()
                    .min(queue.free().saturating_sub(super::KEYS_ROOM));
                let queued = queue.push(&bytes[..n]);
                debug_assert!(queued, "bounded by the free space");
                Ok(n)
            }
            Err(code) => Err((*code, why_not(*code).into())),
        }
    }

    fn written(&self) -> u64 {
        self.queue.as_ref().map_or(0, |q| q.written())
    }

    fn sent(&mut self, bytes: &[u8]) {
        if self.marker.is_none() {
            return;
        }
        if self.markers.len() < MARKERS_MAX {
            marker_line(bytes, self.markers);
        } else {
            *self.unmarked += 1;
        }
    }
}

/// Best effort: a terminal that is gone is noticed by the loop. A signal
/// gives up on the rest, so it is acted on without waiting for a terminal
/// that is not being read. Returns how much was written.
fn write_all_to(fd: BorrowedFd<'_>, signals: &SignalPipe, mut bytes: &[u8]) -> usize {
    let all = bytes.len();
    while !bytes.is_empty() {
        match rustix::io::write(fd, bytes) {
            Ok(n) => {
                bytes = &bytes[n..];
                // A caught signal can end a blocked write early with a short
                // count instead of EINTR.
                if !bytes.is_empty() && signal_waiting(signals) {
                    return all - bytes.len();
                }
            }
            Err(rustix::io::Errno::INTR) => {
                if signal_waiting(signals) {
                    return all - bytes.len();
                }
            }
            // Non-blocking (set so by another program): wait for room.
            Err(rustix::io::Errno::AGAIN) => {
                let signal_fd = signals.fd();
                let mut fds = [
                    PollFd::new(&fd, PollFlags::OUT),
                    PollFd::new(&signal_fd, PollFlags::IN),
                ];
                if poll(&mut fds, None).is_err()
                    || fds[1].revents().contains(PollFlags::IN)
                {
                    return all - bytes.len();
                }
            }
            Err(_) => return all - bytes.len(),
        }
    }
    all
}

/// Writes what the terminal takes until `until` (now: what it has room
/// for), in pieces small enough not to block on it, and returns how much.
/// A signal waiting ends it.
fn write_within(
    fd: BorrowedFd<'_>,
    signals: &SignalPipe,
    bytes: &[u8],
    until: Instant,
) -> usize {
    let mut done = 0;
    let signal_fd = signals.fd();
    for piece in bytes.chunks(64) {
        let mut fds = [
            PollFd::new(&fd, PollFlags::OUT),
            PollFd::new(&signal_fd, PollFlags::IN),
        ];
        let wait = rustix::event::Timespec::try_from(
            until.saturating_duration_since(Instant::now()),
        )
        .unwrap_or_default();
        let ready = matches!(poll(&mut fds, Some(&wait)), Ok(n) if n > 0)
            && fds[0].revents().contains(PollFlags::OUT)
            && !fds[1].revents().contains(PollFlags::IN);
        if !ready {
            break;
        }
        match rustix::io::write(fd, piece) {
            Ok(n) => {
                done += n;
                if n < piece.len() {
                    break;
                }
            }
            Err(_) => break,
        }
    }
    done
}

fn signal_waiting(signals: &SignalPipe) -> bool {
    let fd = signals.fd();
    let mut fds = [PollFd::new(&fd, PollFlags::IN)];
    matches!(poll(&mut fds, Some(&rustix::event::Timespec::default())), Ok(n) if n > 0)
}

/// How long herdr keeps the pane's title and tokens unless they are
/// reported again (at half this): a session killed outright leaves none
/// for long.
const HERDR_TTL: Duration = Duration::from_secs(600);
/// Longest token value herdr keeps whole. Tokens are the pane's, shared
/// with every other reporter, hence xcomm's own names for them.
const HERDR_TOKEN_MAX: usize = 80;

/// Most bytes of a send a marker shows.
const MARKER_MAX: usize = 200;
/// How soon the rest of a marker the terminal did not take is tried again,
/// at first and at most.
const MARKER_RETRY: Duration = Duration::from_millis(10);
const MARKER_RETRY_MAX: Duration = Duration::from_secs(1);
/// What ends a marker cut short at the end of the session: CAN ends a
/// sequence it stopped in, and the reset undoes its dim.
const END_CUT: &[u8] = b"\x18\x1b[0m\r\n";
/// How long markers wait for the device's output to leave an escape
/// sequence or a character before they are shown anyway.
const MARKER_WAIT: Duration = Duration::from_millis(100);

/// Most marker bytes waiting for a terminal that is not being read.
const MARKERS_MAX: usize = 64 << 10;
/// How much of the latest device output decides how a marker is shown.
const MARKER_LOOK: u64 = 64 << 10;

/// How what control clients send is shown: dim when xcomm's own messages
/// may use color (NO_COLOR turns that off), and the device's attributes
/// given back after it with their colors when device color is on.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MarkerStyle {
    pub dim: bool,
    pub device_color: bool,
}

/// Appends `[ctl] <bytes>` on a new line to `out`: C0 controls as `^X`, C1
/// controls and bytes that are not UTF-8 as `\xNN`, at most MARKER_MAX
/// bytes of the send, cut between characters.
fn marker_line(bytes: &[u8], out: &mut Vec<u8>) {
    out.extend_from_slice(b"\r\n[ctl] ");
    let hex = |out: &mut Vec<u8>, v: u32| {
        out.extend_from_slice(format!("\\x{v:02x}").as_bytes())
    };
    let mut shown = 0;
    'all: for chunk in bytes.utf8_chunks() {
        for c in chunk.valid().chars() {
            if shown + c.len_utf8() > MARKER_MAX {
                break 'all;
            }
            shown += c.len_utf8();
            match c {
                '\0'..='\x1f' => out.extend_from_slice(&[b'^', c as u8 + 0x40]),
                '\x7f' => out.extend_from_slice(b"^?"),
                '\u{80}'..='\u{9f}' => hex(out, c as u32),
                _ => out.extend_from_slice(c.encode_utf8(&mut [0; 4]).as_bytes()),
            }
        }
        for &b in chunk.invalid() {
            if shown + 1 > MARKER_MAX {
                break 'all;
            }
            shown += 1;
            hex(out, b.into());
        }
    }
    if shown < bytes.len() {
        out.extend_from_slice(format!("... ({} bytes)", bytes.len()).as_bytes());
    }
}

impl<'a> Live<'a> {
    /// Serves the clients, then runs what they asked of the session: in
    /// the session loop (`queue` given) once the bytes queued before it are
    /// written; else (`queue` the code why not) only a capture, the rest
    /// answered with that code.
    fn serve(
        &mut self,
        ready: &[libc::pollfd],
        mut queue: Result<&mut OutQueue, u8>,
        stdin: Option<BorrowedFd<'_>>,
    ) -> Result<CommandOutcome, SessionError> {
        // Here, as this runs on every pass: connected, waiting, transferring.
        self.update_herdr();
        let Some(server) = &mut self.server else {
            return Ok(CommandOutcome::Continue);
        };
        let mut view = View {
            port: self.port,
            ring: &self.ring,
            capture: self.capture.as_ref(),
            names: &self.names,
            transferring: self.transferring,
            queue: match &mut queue {
                Ok(q) => Ok(&mut **q),
                Err(code) => Err(*code),
            },
            marker: self.marker,
            markers: &mut self.markers,
            unmarked: &mut self.unmarked,
        };
        server.serve(ready, &mut view);
        self.show_markers();
        if self
            .hold
            .as_ref()
            .is_some_and(|h| Instant::now() >= h.until)
        {
            self.end_hold(None);
        }
        loop {
            if self.hold.is_some() {
                return Ok(CommandOutcome::Continue);
            }
            let Some((wait_id, action)) =
                self.server.as_mut().and_then(|s| s.pending_action())
            else {
                self.action_wait = None;
                return Ok(CommandOutcome::Continue);
            };
            let on_line =
                !matches!(action, Action::CaptureStart(_) | Action::CaptureStop);
            // A wait is the action's it began for, not another client's.
            if self.action_wait.as_ref().is_some_and(|w| w.id != wait_id) {
                self.action_wait = None;
            }
            if on_line
                && let (Ok(q), Some(port)) = (&queue, self.port)
                && !self.line_ready(wait_id, q, port)
            {
                return Ok(CommandOutcome::Continue);
            }
            self.action_wait = None;
            let Some((id, action)) = self.server.as_mut().and_then(|s| s.next_action())
            else {
                return Ok(CommandOutcome::Continue);
            };
            let result = match (action, &queue, self.port) {
                (Action::CaptureStart(path), ..) => self.ctl_capture_start(&path),
                (Action::CaptureStop, ..) => self.ctl_capture_stop(),
                (_, Err(code), _) => Err((*code, why_not(*code).into())),
                (_, Ok(_), None) => Err((
                    ctl::code::DISCONNECTED,
                    why_not(ctl::code::DISCONNECTED).into(),
                )),
                (Action::Break { ms }, Ok(_), Some(port)) => match port.set_break(true) {
                    Ok(()) => {
                        self.start_hold(id, ms, HoldKind::Break);
                        continue;
                    }
                    Err(e) => Err((ctl::code::FAILED, e.to_string())),
                },
                (Action::Modem { line, op, ms }, Ok(_), Some(port)) => {
                    match self.ctl_modem(port, line, op) {
                        Ok(Some(back)) => {
                            self.start_hold(id, ms, HoldKind::Pulse { line, back });
                            continue;
                        }
                        Ok(None) => self.modem_reply(port, line),
                        Err(e) => Err(e),
                    }
                }
                (Action::Set(patch), Ok(_), Some(port)) => self.ctl_set(port, &patch),
                (
                    Action::Xfer {
                        dir,
                        proto,
                        files,
                        cwd,
                    },
                    Ok(_),
                    Some(port),
                ) => {
                    self.flush_capture();
                    let run = TransferRun {
                        proto: match proto {
                            proto::XferProto::Zmodem => Proto::Zmodem,
                            proto::XferProto::Ymodem => Proto::Ymodem,
                            proto::XferProto::Xmodem => Proto::Xmodem,
                        },
                        dir: match dir {
                            proto::XferDir::Send => Direction::Send,
                            proto::XferDir::Recv => Direction::Receive,
                        },
                        files: &files,
                        cwd: &cwd,
                        stdin,
                        typed: &[],
                        requester: Some(id),
                        after_prompt: false,
                    };
                    match self.run_transfer(port, run)? {
                        Ok((true, line)) => Ok(serde_json::json!({ "result": line })),
                        Ok((false, line)) => Err((ctl::code::FAILED, line)),
                        Err(exit) => {
                            if let Some(s) = self.server.as_mut() {
                                s.finish_action(
                                    id,
                                    Err((
                                        ctl::code::FAILED,
                                        "the session is ending".into(),
                                    )),
                                );
                            }
                            return Ok(CommandOutcome::Exit(exit));
                        }
                    }
                }
            };
            if let Some(s) = self.server.as_mut() {
                s.finish_action(id, result);
            }
        }
    }

    fn ctl_capture_start(
        &mut self,
        path: &Path,
    ) -> Result<serde_json::Value, (u8, String)> {
        if let Some(c) = &self.capture {
            return Err((
                ctl::code::FAILED,
                format!("capturing to {} already", c.path().display()),
            ));
        }
        match Capture::start(path) {
            Ok(c) => {
                self.say(&format!(
                    "capturing to {} for a control client",
                    path.display()
                ));
                self.capture = Some(c);
                Ok(serde_json::json!({ "capture": path.display().to_string() }))
            }
            Err(e) => Err((ctl::code::FAILED, format!("capture: {e}"))),
        }
    }

    fn ctl_capture_stop(&mut self) -> Result<serde_json::Value, (u8, String)> {
        let Some(c) = self.capture.take() else {
            return Err((ctl::code::FAILED, "no capture is running".into()));
        };
        let (path, total) = (c.path().display().to_string(), c.total());
        match c.close() {
            Ok(()) => {
                self.say(&format!(
                    "capture closed for a control client: {path} ({total} bytes)"
                ));
                Ok(serde_json::json!({ "path": path, "bytes": total }))
            }
            Err(e) => {
                self.say(&format!("capture stopped: {path}: {e}"));
                Err((ctl::code::FAILED, format!("capture {path}: {e}")))
            }
        }
    }

    /// Puts a modem line where `op` says. For a pulse, gives the state to
    /// put back when the hold ends.
    fn ctl_modem(
        &mut self,
        port: &Port,
        line: proto::ModemLine,
        op: proto::ModemOp,
    ) -> Result<Option<bool>, (u8, String)> {
        let (get, set) = line_ops(line);
        let name = line.name();
        let failed = |e: io::Error| (ctl::code::FAILED, format!("{name}: {e}"));
        let now = get(&port.modem_lines().map_err(failed)?);
        let want = match op {
            proto::ModemOp::On => true,
            proto::ModemOp::Off => false,
            proto::ModemOp::Toggle | proto::ModemOp::Pulse => !now,
        };
        set(port, want).map_err(failed)?;
        let after = get(&port.modem_lines().map_err(failed)?);
        if after != want {
            return Err((
                ctl::code::FAILED,
                format!("{name} is {}: the driver did not change it", on_off(after)),
            ));
        }
        if op == proto::ModemOp::Pulse {
            return Ok(Some(now));
        }
        self.say(&format!("{name} {} for a control client", on_off(after)));
        Ok(None)
    }

    /// The reply for a modem line now, recording it for a reconnect.
    fn modem_reply(
        &mut self,
        port: &Port,
        line: proto::ModemLine,
    ) -> Result<serde_json::Value, (u8, String)> {
        let (get, _) = line_ops(line);
        let on = port
            .modem_lines()
            .map(|m| get(&m))
            .map_err(|e| (ctl::code::FAILED, format!("{}: {e}", line.name())))?;
        match line {
            proto::ModemLine::Dtr => self.lines.0 = Some(on),
            proto::ModemLine::Rts => self.lines.1 = Some(on),
        }
        Ok(serde_json::json!({ "line": line.name(), "on": on }))
    }

    /// Whether the bytes queued before a client's action are out: those in
    /// the queue then are written, the driver has sent its buffer (writes
    /// pause for that) and, with no flow control to hold it up, the
    /// adapter its own (tcdrain). Or the line has taken nothing for STALL.
    fn line_ready(&mut self, id: u64, queue: &OutQueue, port: &Port) -> bool {
        let now = Instant::now();
        let wait = self.action_wait.get_or_insert(ActionWait {
            id,
            since: now,
            phase: WaitPhase::Queue {
                until: queue.written() + queue.len() as u64,
                written: queue.written(),
            },
        });
        if let WaitPhase::Queue { until, written } = wait.phase {
            if queue.written() < until {
                if queue.written() > written {
                    wait.since = now;
                    wait.phase = WaitPhase::Queue {
                        until,
                        written: queue.written(),
                    };
                }
                return now.duration_since(wait.since) >= STALL;
            }
            wait.since = now;
            wait.phase = WaitPhase::Driver {
                pending: usize::MAX,
            };
        }
        let pending = port.output_pending().unwrap_or(0);
        if pending == 0 {
            if port.settings().flow == crate::cli::Flow::None {
                // What the adapter holds: a few hundred bytes at most, at
                // the line's rate. EINTR leaves it to the loop's signal.
                let _ = rustix::termios::tcdrain(port.fd());
            }
            return true;
        }
        if let WaitPhase::Driver { pending: last } = &mut wait.phase {
            if pending < *last {
                wait.since = now;
            }
            *last = pending;
        }
        now.duration_since(wait.since) >= STALL
    }

    /// Lets a break or pulse a client started run to its end before a key
    /// command takes the line (or opens a prompt, which serves no client).
    /// A signal cuts the wait short; the reply then says how long it was.
    fn finish_hold(&mut self) {
        let Some(until) = self.hold.as_ref().map(|h| h.until) else {
            return;
        };
        let left = until.saturating_duration_since(Instant::now());
        if !left.is_zero() {
            let signal_fd = self.signals.fd();
            let mut fds = [PollFd::new(&signal_fd, PollFlags::IN)];
            let wait = rustix::event::Timespec::try_from(left).expect("at most HOLD_MAX");
            let _ = poll(&mut fds, Some(&wait));
        }
        self.end_hold(None);
    }

    fn start_hold(&mut self, id: u64, ms: u32, kind: HoldKind) {
        let started = Instant::now();
        self.hold = Some(Hold {
            id,
            started,
            until: started + Duration::from_millis(ms.into()),
            kind,
        });
    }

    /// Ends the break or pulse under way and answers its client: with the
    /// result, or with `cut` when the session or the device is going.
    fn end_hold(&mut self, cut: Option<(u8, String)>) {
        let Some(hold) = self.hold.take() else {
            return;
        };
        // As held: a prompt or the session's stopping may have made it
        // longer, a signal shorter.
        let ms = u64::try_from(hold.started.elapsed().as_millis()).unwrap_or(u64::MAX);
        let result = match (self.port, hold.kind) {
            (None, _) => Err((
                ctl::code::DISCONNECTED,
                why_not(ctl::code::DISCONNECTED).into(),
            )),
            (Some(port), HoldKind::Break) => match port.set_break(false) {
                Ok(()) => {
                    self.say(&format!("break sent ({ms} ms) for a control client"));
                    Ok(serde_json::json!({ "ms": ms }))
                }
                Err(e) => {
                    self.say(&e.to_string());
                    Err((ctl::code::FAILED, e.to_string()))
                }
            },
            (Some(port), HoldKind::Pulse { line, back }) => {
                let (_, set) = line_ops(line);
                let name = line.name();
                match set(port, back) {
                    Ok(()) => {
                        self.say(&format!(
                            "{name} pulsed {} for {ms} ms for a control client",
                            on_off(!back)
                        ));
                        self.modem_reply(port, line)
                    }
                    Err(e) => {
                        let msg = format!("{name} is left {}: {e}", on_off(!back));
                        self.say(&msg);
                        Err((ctl::code::FAILED, msg))
                    }
                }
            }
        };
        let result = match cut {
            Some(why) => Err(why),
            None => result,
        };
        if let Some(s) = self.server.as_mut() {
            s.finish_action(hold.id, result);
        }
    }

    fn ctl_set(
        &mut self,
        port: &Port,
        patch: &proto::SettingsPatch,
    ) -> Result<serde_json::Value, (u8, String)> {
        let wanted = patch
            .apply(port.settings())
            .map_err(|e| (ctl::code::USAGE, e))?;
        let applied = match port.configure(&wanted) {
            Ok(Applied::Exact) => wanted,
            Ok(Applied::Different(actual)) => actual,
            Err(e) => return Err((ctl::code::FAILED, e.to_string())),
        };
        let exact = applied == wanted;
        if exact {
            self.say(&format!(
                "{} for a control client",
                port::describe(&applied)
            ));
        } else {
            self.say(&format!(
                "requested {} for a control client; the driver applied {}",
                port::describe(&wanted),
                port::describe(&applied)
            ));
        }
        Ok(serde_json::json!({
            "settings": proto::SettingsView::of(&applied),
            "exact": exact,
        }))
    }

    /// The device came back as `tty`: the socket follows its name.
    pub(super) fn renamed(&mut self, tty: &str) {
        self.names.tty = tty.to_string();
        if let Some(Err(e)) = self.server.as_mut().map(|s| s.rename(tty)) {
            self.say(&e.0);
        }
    }

    /// Serves the control socket while nothing can go to the device:
    /// `why` is the code a send gets.
    pub(super) fn serve_ctl(&mut self, ready: &[libc::pollfd], why: u8) {
        // With no queue no transfer runs, the one thing that can end it.
        let _ = self.serve(ready, Err(why), None);
    }

    pub(super) fn new(
        raw: Option<&'a RawMode>,
        port: Option<&'a Port>,
        opts: &TermOpts,
        stdout: Option<BorrowedFd<'a>>,
        stderr: BorrowedFd<'a>,
        signals: &'a SignalPipe,
        kept: Kept,
    ) -> Live<'a> {
        let screen = stdout.filter(|fd| rustix::termios::isatty(fd));
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
            screen,
            marker: kept.marker.filter(|_| screen.is_some()),
            markers: Vec::new(),
            markers_since: None,
            unshown: std::cell::RefCell::new(Vec::new()),
            unmarked: 0,
            retry_at: None,
            retry: MARKER_RETRY,
            headless: opts.headless,
            herdr: kept.herdr,
            signals,
            echo: kept.echo,
            wrap: kept.wrap,
            capture: kept.capture,
            ring: kept.ring,
            server: kept.server,
            names: kept.names,
            transferring: false,
            lines: kept.lines,
            hold: None,
            action_wait: None,
            cr_ended: false,
        }
    }

    /// The device is gone: a client's break or pulse ends with it. There
    /// is no break to end on it, and the error trying would say (EIO, "DTR
    /// is left on") only misleads.
    pub(super) fn device_gone(&mut self) {
        self.port = None;
        self.end_hold(Some((
            ctl::code::DISCONNECTED,
            "the device went away during it".into(),
        )));
    }

    pub(super) fn into_kept(mut self) -> Kept {
        self.show_markers_now(true);
        self.device_gone();
        Kept {
            echo: self.echo,
            wrap: self.wrap,
            capture: self.capture,
            ring: self.ring,
            server: self.server,
            names: self.names,
            lines: self.lines,
            marker: self.marker,
            herdr: self.herdr,
        }
    }

    /// Titles the herdr pane after the port and its state, when that has
    /// changed: `xcomm ttyUSB0 115200`, then `[gone]`, `[xfer]`, `[cap]`;
    /// and reports port, baud and by-id name as tokens. The report expires
    /// unless renewed, so one a crash leaves goes away. A failure is said
    /// once; the session goes on without herdr.
    pub(super) fn update_herdr(&mut self) {
        let Some(h) = self.herdr.herdr.as_ref().filter(|_| !self.herdr.failed) else {
            return;
        };
        let last = self.herdr.shown.as_ref();
        let baud = self
            .port
            .map(|p| p.settings().baud)
            .or(last.map(|s| s.baud))
            .unwrap_or(0);
        let (connected, xfer, cap) = (
            self.port.is_some(),
            self.transferring,
            self.capture.is_some(),
        );
        let same = last.is_some_and(|s| {
            s.tty == self.names.tty
                && s.baud == baud
                && s.connected == connected
                && s.xfer == xfer
                && s.cap == cap
        });
        if same && self.herdr.renew_at.is_some_and(|t| Instant::now() < t) {
            return;
        }
        let tty = self.names.tty.clone();
        let mut title = format!("xcomm {tty} {baud}");
        for (on, tag) in [(!connected, " [gone]"), (xfer, " [xfer]"), (cap, " [cap]")] {
            if on {
                title.push_str(tag);
            }
        }
        let baud_text = baud.to_string();
        let by_id = crate::list::by_id_of(&tty);
        // herdr keeps 80 characters of a token: a longer name would name
        // no link.
        let link = by_id
            .as_deref()
            .map(|p| p.rsplit('/').next().unwrap_or(p))
            .filter(|l| l.chars().count() <= HERDR_TOKEN_MAX);
        let result = h.report_metadata(
            Some(&title),
            &[
                ("xcomm_port", Some(&tty)),
                ("xcomm_baud", Some(&baud_text)),
                ("xcomm_by_id", link),
            ],
            Some(HERDR_TTL),
        );
        self.herdr.shown = Some(PaneState {
            tty,
            baud,
            connected,
            xfer,
            cap,
        });
        self.herdr.renew_at = Some(Instant::now() + HERDR_TTL / 2);
        if let Err(e) = result {
            self.herdr.failed = true;
            self.say(&format!("herdr: {e}"));
        }
    }

    /// Drops xcomm's title and tokens from the pane, even after a failure:
    /// what was reported should not outlive the session.
    fn clear_herdr(&mut self) {
        let Some(h) = self.herdr.herdr.as_ref() else {
            return;
        };
        if self.herdr.shown.take().is_none() {
            return;
        }
        self.herdr.renew_at = None;
        let result = h.report_metadata(
            None,
            &[
                ("xcomm_port", None),
                ("xcomm_baud", None),
                ("xcomm_by_id", None),
            ],
            None,
        );
        if let Err(e) = result
            && !self.herdr.failed
        {
            self.herdr.failed = true;
            self.say(&format!("herdr: {e}"));
        }
    }

    /// Writes out the capture, and puts back what commands changed on the
    /// terminal.
    pub(super) fn finish(&mut self) {
        self.clear_herdr();
        self.end_hold(Some((ctl::code::FAILED, "the session is ending".into())));
        self.show_markers_now(true);
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

    fn toggle_capture(
        &mut self,
        io: &mut CommandIo<'_>,
    ) -> Result<CommandOutcome, SessionError> {
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
        if matches!(poll(&mut fds, Some(&wait)), Ok(1))
            && fds[0].revents().contains(PollFlags::OUT)
        {
            let _ = rustix::io::write(fd, if on { WRAP_ON } else { WRAP_OFF });
        }
    }

    fn write_to(&self, fd: BorrowedFd<'_>, bytes: &[u8]) {
        let _ = write_all_to(fd, self.signals, bytes);
    }

    /// Moves the markers waiting to `unshown`, framed, once the device's
    /// output is between escape sequences and characters (it would take
    /// them in otherwise), or when `force`d or MARKER_WAIT on, after CAN to
    /// end what the device left unfinished. They are drawn plain or dim,
    /// then the device's own attributes are given back: a reset and the
    /// SGR sequences it sent since its last one.
    fn frame_markers(&mut self, force: bool) {
        if self.markers.is_empty() {
            return;
        }
        let from = self
            .ring
            .end()
            .saturating_sub(MARKER_LOOK)
            .max(self.ring.oldest());
        // The ring's own bytes when they are in one piece (all but a window
        // across the wrap): no 64 KiB copy for every marker, and none again
        // on each pass while one waits for a sequence to end.
        let state = match self.ring.slices(from, MARKER_LOOK as usize) {
            Ok((a, [])) => crate::ansi::term_state(a),
            Ok((a, b)) => crate::ansi::term_state(&[a, b].concat()),
            Err(_) => crate::ansi::term_state(&[]),
        };
        let late = force
            || self
                .markers_since
                .is_some_and(|t| t.elapsed() >= MARKER_WAIT);
        if state.inside && !late {
            self.markers_since.get_or_insert_with(Instant::now);
            return;
        }
        self.markers_since = None;
        let style = self.marker.unwrap_or_default();
        let out = self.unshown.get_mut();
        if state.inside {
            out.extend_from_slice(CANCEL.as_bytes());
        }
        out.extend_from_slice(if style.dim { b"\x1b[0;2m" } else { b"\x1b[0m" });
        out.append(&mut self.markers);
        if self.unmarked > 0 {
            out.extend_from_slice(
                format!("\r\n[ctl] ({} more sends not shown)", self.unmarked).as_bytes(),
            );
            self.unmarked = 0;
        }
        out.extend_from_slice(b"\x1b[0m");
        if style.device_color {
            out.extend_from_slice(&state.sgr);
        } else {
            // Color is off: the device's attributes without their colors.
            crate::ansi::SgrFilter::new().filter(&state.sgr, out);
        }
        out.extend_from_slice(b"\r\n");
    }

    /// Writes the markers, framed, as far as the terminal takes them now:
    /// a terminal nobody reads must not stop the session. At the end
    /// (`force`) a slow one still gets a while, and what it does not take
    /// gives way to CAN and a reset, so the terminal is not left dim or
    /// inside a sequence.
    fn show_markers_now(&mut self, force: bool) {
        let Some(fd) = self.screen else {
            self.markers.clear();
            self.unshown.get_mut().clear();
            return;
        };
        let until = Instant::now()
            + if force {
                MARKER_WAIT * 10
            } else {
                Duration::ZERO
            };
        // What was begun goes first, whole, before the next markers.
        if self.unshown.get_mut().is_empty() {
            self.frame_markers(force);
        }
        let rest = self.unshown.get_mut();
        let mut wrote = write_within(fd, self.signals, rest, until);
        rest.drain(..wrote);
        if rest.is_empty() && !self.markers.is_empty() {
            self.frame_markers(force);
            let rest = self.unshown.get_mut();
            let n = write_within(fd, self.signals, rest, until);
            rest.drain(..n);
            wrote += n;
        }
        let rest = self.unshown.get_mut();
        if rest.is_empty() {
            self.retry_at = None;
            self.retry = MARKER_RETRY;
        } else if force {
            rest.clear();
            let _ = write_within(fd, self.signals, END_CUT, until);
        } else {
            self.retry = if wrote > 0 {
                MARKER_RETRY
            } else {
                (self.retry * 2).min(MARKER_RETRY_MAX)
            };
            self.retry_at = Some(Instant::now() + self.retry);
        }
    }

    fn show_markers(&mut self) {
        self.show_markers_now(false);
    }

    fn write(&self, bytes: &[u8]) {
        // Not into the middle of a marker on the same terminal: the rest
        // goes first, as this write would wait for it.
        if let Some(fd) = self.screen {
            let mut rest = self.unshown.borrow_mut();
            if !rest.is_empty() {
                let n = write_all_to(fd, self.signals, &rest);
                rest.drain(..n);
            }
        }
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

    fn say_maybe_after_prompt(&self, after_prompt: bool, msg: &str) {
        if after_prompt {
            self.say_after_prompt(msg);
        } else {
            self.say(msg);
        }
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
        let line = match self.prompt(
            &format!("baud [{}]: ", current.baud),
            BAUD_MAX,
            false,
            io,
        )? {
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
                Prompt::Line(line) if line.is_empty() => {
                    return Ok(CommandOutcome::Continue);
                }
                Prompt::Line(line) => files.push(PathBuf::from(OsString::from_vec(line))),
                Prompt::Cancelled => return Ok(CommandOutcome::Continue),
                Prompt::Exit(exit) => return Ok(CommandOutcome::Exit(exit)),
            }
        }
        let typed = std::mem::take(&mut io.typed);
        let run = TransferRun {
            proto,
            dir,
            files: &files,
            cwd: Path::new("."),
            stdin: Some(io.stdin),
            typed,
            requester: None,
            after_prompt: true,
        };
        match self.run_transfer(port, run)? {
            Ok(_) => Ok(CommandOutcome::Continue),
            Err(exit) => Ok(CommandOutcome::Exit(exit)),
        }
    }

    /// Runs lrzsz on the port until it ends, as the session's command or
    /// for a client. Gives whether it succeeded and the line saying how it
    /// went (said on the terminal too), or why the session ends.
    fn run_transfer(
        &mut self,
        port: &Port,
        run: TransferRun<'_>,
    ) -> Result<Result<(bool, String), ExitReason>, SessionError> {
        // Before the program starts: its first line comes at once.
        self.process_output(true);
        // Headless, its progress would fill a pipe no one reads.
        let progress = !self.headless;
        let mut transfer =
            match Transfer::start(port, run.proto, run.dir, run.files, run.cwd, progress)
            {
                Ok(t) => t,
                Err(e) => {
                    self.process_output(false);
                    let line = format!("transfer failed: {e}");
                    self.say_maybe_after_prompt(run.after_prompt, &line);
                    return Ok(Ok((false, line)));
                }
            };
        let key = self.prefix.key.display();
        // With Ctrl-C as the prefix, Ctrl-C alone waits for the command key.
        let ctrl_c_prefix = self.prefix.key.modifier == crate::cli::Modifier::Ctrl
            && self.prefix.key.letter == b'c';
        // Keys are read only from a terminal in raw mode.
        let how = if self.raw.is_none() {
            "closing the client aborts it".to_string()
        } else if ctrl_c_prefix {
            format!("{key} x aborts")
        } else {
            format!("{key} x or Ctrl-C aborts")
        };
        let by = if run.requester.is_some() {
            " for a control client"
        } else {
            ""
        };
        self.say_maybe_after_prompt(
            run.after_prompt,
            &format!("transfer started{by}: {} ({how})", transfer.program()),
        );
        self.transferring = true;
        self.update_herdr();
        let ended =
            self.wait_transfer(&mut transfer, run.stdin, run.typed, run.requester);
        self.transferring = false;
        self.update_herdr();
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
        let ended = ended?;
        if let Ok((_, line)) = &ended {
            self.say(line);
        }
        Ok(ended)
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
        stdin: Option<BorrowedFd<'_>>,
        typed: &[u8],
        requester: Option<u64>,
    ) -> Result<Result<(bool, String), ExitReason>, SessionError> {
        let mut keys = KeyParser::new(self.prefix.key, self.prefix_time);
        // Keys typed after the command key are dropped too, unless they abort.
        let mut typed = typed;
        let mut stdin_open = stdin.is_some();
        let mut aborted: Option<Instant> = None;
        let signals = self.signals;
        let signal_fd = signals.fd();
        let mut buf = [0u8; 256];
        let mut polled: Vec<libc::pollfd> = Vec::with_capacity(super::POLL_ROOM);
        loop {
            match transfer.try_finish() {
                // Done before the abort took effect: done.
                Ok(Some(_)) => {
                    return Ok(Ok((
                        true,
                        format!("transfer done: {}", transfer.program()),
                    )));
                }
                Err(XferError::Failed { .. }) if aborted.is_some() => {
                    return Ok(Ok((false, "transfer aborted".into())));
                }
                Err(e) => return Ok(Ok((false, format!("transfer failed: {e}")))),
                Ok(None) => {}
            }
            // The client that asked for it closed its end: not wanted now.
            if aborted.is_none()
                && let Some(id) = requester
                && self.server.as_ref().is_none_or(|s| s.abandoned(id))
            {
                aborted = start_abort(transfer);
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
            let (stdin_slot, stdin_events) = match stdin {
                Some(fd) if stdin_open => (fd, PollFlags::IN),
                _ => (signal_fd, PollFlags::empty()),
            };
            polled.clear();
            polled.push(super::entry(signal_fd, PollFlags::IN));
            polled.push(super::entry(stdin_slot, stdin_events));
            let due = due.into_iter().chain(self.ctl_fds(&mut polled)).min();
            let timeout = due.map(|due| due.saturating_duration_since(Instant::now()));
            match super::poll_entries(&mut polled, timeout) {
                Ok(()) => {}
                Err(rustix::io::Errno::INTR) => {
                    polled.iter_mut().for_each(|e| e.revents = 0)
                }
                Err(e) => return Err(SessionError(format!("poll: {e}"))),
            }
            // The program has the port: clients cannot send meanwhile, and
            // subscribers get nothing (the program reads the device).
            self.serve_ctl(&polled[2..], ctl::code::BUSY);
            // A lone ESC held past its time is a key like any other here.
            keys.on_deadline(Instant::now(), &mut |a| {
                if let KeyAction::Forward(b) = a {
                    abort |= has_ctrl_c(b);
                }
            });
            if super::revents(&polled[0]).contains(PollFlags::IN) {
                // SIGCHLD only wakes the loop; try_finish looks.
                let bits = signals.drain().map_err(|e| SessionError(e.to_string()))?;
                let signaled = on_signals(bits, signals, self)?;
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
                    while matches!(transfer.try_finish(), Ok(None))
                        && Instant::now() < deadline
                    {
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    transfer.kill();
                    return Ok(Err(exit));
                }
            }
            if let Some(fd) = stdin.filter(|_| stdin_open)
                && super::revents(&polled[1])
                    .intersects(PollFlags::IN | PollFlags::HUP | PollFlags::ERR)
            {
                match rustix::io::read(fd, &mut buf) {
                    Ok(0) => stdin_open = false,
                    Ok(n) => {
                        let mut rest = &buf[..n];
                        while !rest.is_empty() {
                            let mut command = None;
                            let used =
                                keys.feed(rest, Instant::now(), &mut |a| match a {
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
    fn escape_in_prompt(
        &mut self,
        io: &mut CommandIo<'_>,
    ) -> Result<EscInPrompt, SessionError> {
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
                    Key::Timeout | Key::Resumed | Key::Gone => {
                        return Ok(EscInPrompt::Byte(ESC));
                    }
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
        let timeout =
            timeout.map(|t| rustix::event::Timespec::try_from(t).expect("fits"));
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

    fn command(
        &mut self,
        key: u8,
        io: &mut CommandIo<'_>,
    ) -> Result<CommandOutcome, SessionError> {
        self.cr_ended = false;
        if key == ESC {
            return Ok(CommandOutcome::Continue);
        }
        let Some(&(_, cmd, what)) = COMMANDS.iter().find(|(k, ..)| *k == key) else {
            self.bell();
            return Ok(CommandOutcome::Continue);
        };
        // A client's break or pulse ends first: a prompt serves no client,
        // and a key acting on the line would be undone at its end.
        self.finish_hold();
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
            (Cmd::Receive, Some(port)) => {
                return self.transfer(port, Direction::Receive, io);
            }
        }
        Ok(CommandOutcome::Continue)
    }

    fn local_echo(&self) -> bool {
        self.echo
    }

    fn capture(&mut self) -> Option<&mut Capture> {
        self.capture.as_mut()
    }

    fn device_output(&mut self, bytes: &[u8]) {
        self.ring.push(bytes);
        if let Some(server) = &mut self.server {
            server.device_output(&self.ring);
        }
        if !self.markers.is_empty() || !self.unshown.get_mut().is_empty() {
            self.show_markers();
        }
    }

    fn cut_unshown(&mut self) {
        *self.unshown.get_mut() = END_CUT.to_vec();
    }

    fn take_unshown(&mut self) -> Vec<u8> {
        // Markers for sends made before this output come before it too.
        if self.screen.is_some() {
            self.frame_markers(false);
        }
        std::mem::take(self.unshown.get_mut())
    }

    fn may_write(&self) -> bool {
        let breaking = matches!(
            &self.hold,
            Some(Hold {
                kind: HoldKind::Break,
                ..
            })
        );
        let draining = matches!(
            &self.action_wait,
            Some(ActionWait {
                phase: WaitPhase::Driver { .. },
                ..
            })
        );
        !breaking && !draining
    }

    fn keys_go_at_once(&self) -> bool {
        keys_go_at_once(self.hold.as_ref(), self.action_wait.as_ref())
    }

    fn ctl_fds(&self, out: &mut Vec<libc::pollfd>) -> Option<Instant> {
        let server_due = self
            .server
            .as_ref()
            .and_then(|server| server.poll_fds(self.ring.end(), out));
        // A marker the terminal has not all taken goes on as soon as it can.
        let markers_due = if self.unshown.borrow().is_empty() {
            self.markers_since.map(|t| t + MARKER_WAIT)
        } else {
            self.retry_at
        };
        let wait_due = self.action_wait.as_ref().map(|w| {
            let stalled = w.since + STALL;
            match w.phase {
                WaitPhase::Queue { .. } => stalled,
                WaitPhase::Driver { .. } => stalled.min(Instant::now() + DRIVER_CHECK),
            }
        });
        let herdr_due = self.herdr.renew_at.filter(|_| !self.herdr.failed);
        [
            server_due,
            self.hold.as_ref().map(|h| h.until),
            wait_due,
            markers_due,
            herdr_due,
        ]
        .into_iter()
        .flatten()
        .min()
    }

    fn ctl_serve(
        &mut self,
        ready: &[libc::pollfd],
        queue: &mut OutQueue,
        stdin: Option<BorrowedFd<'_>>,
    ) -> Result<CommandOutcome, SessionError> {
        self.serve(ready, Ok(queue), stdin)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_wait_while_a_client_break_pulse_or_action_is_in_hand() {
        let now = Instant::now();
        let hold = |kind| Hold {
            id: 1,
            started: now,
            until: now,
            kind,
        };
        let wait = |phase| ActionWait {
            id: 1,
            since: now,
            phase,
        };
        assert!(keys_go_at_once(None, None));
        assert!(!keys_go_at_once(Some(&hold(HoldKind::Break)), None));
        let pulse = hold(HoldKind::Pulse {
            line: proto::ModemLine::Dtr,
            back: true,
        });
        assert!(!keys_go_at_once(Some(&pulse), None));
        // Still in the queue: may_write() is true here, and this is where
        // a key written at once would pass the action.
        let queued = wait(WaitPhase::Queue {
            until: 10,
            written: 4,
        });
        assert!(!keys_go_at_once(None, Some(&queued)));
        let driver = wait(WaitPhase::Driver { pending: 3 });
        assert!(!keys_go_at_once(None, Some(&driver)));
    }
}
