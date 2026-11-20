// SPDX-FileCopyrightText: 2026 Su Kang Yin <paradyse@gmail.com>
// SPDX-License-Identifier: GPL-2.0-only

//! The session's end of the control socket, served from the session's own
//! poll loop: it never blocks, and costs nothing per device read while no
//! one subscribes.

use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use super::proto::{
    self, ModemLine, ModemOp, Request, SettingsPatch, Status, XferDir, XferProto,
};
use super::{CtlError, CtlMode, code};
use crate::ring::Ring;

/// Clients served at once; more are told so and closed as they come.
pub const MAX_CLIENTS: usize = 32;
/// Reads from one client per pass: a client sending fast cannot hold the
/// session loop.
const READS_PER_STEP: usize = 4;
/// How long accepting waits after it failed for want of descriptors: the
/// pending connection keeps the listener readable, and poll would spin.
const ACCEPT_PAUSE: Duration = Duration::from_secs(1);
/// How long a send whose client has closed may wait with the line taking
/// nothing at all (of anyone's) before the client is dropped. Not a limit
/// on the whole send: a slow line takes some all the time (350 KB at 9600
/// baud takes six minutes). One held off for good would otherwise keep the
/// client's place forever.
const UNATTENDED_STALL: Duration = Duration::from_secs(30);
/// While such a send waits, the server looks at the line this often; a
/// longer gap between two looks (the session loop blocked elsewhere, on a
/// terminal nobody reads) is not counted as the line taking nothing.
const STALL_LOOK: Duration = Duration::from_secs(1);

/// A request that acts on the session rather than on the server: the
/// session takes it with [`CtlServer::next_action`] and answers it with
/// [`CtlServer::finish_action`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    Break {
        ms: u32,
    },
    Modem {
        line: ModemLine,
        op: ModemOp,
        ms: u32,
    },
    Set(SettingsPatch),
    CaptureStart(PathBuf),
    CaptureStop,
    Xfer {
        dir: XferDir,
        proto: XferProto,
        files: Vec<PathBuf>,
        cwd: PathBuf,
    },
}

/// What the server needs from the session.
pub trait Session {
    /// Everything but the `ctl` field, which the server fills in.
    fn status(&self) -> Status;
    fn ring(&self) -> &Ring;
    /// Takes as much of `bytes` for the device as there is room for, and
    /// says how much. Fails with a reply code while the device cannot take
    /// any (disconnected, a file transfer running).
    fn send(&mut self, bytes: &[u8]) -> Result<usize, (u8, String)>;
    /// All of a client's send has been taken for the device.
    fn sent(&mut self, _bytes: &[u8]) {}
    /// Bytes the line has taken so far, anyone's (clients' and the keys'):
    /// a send waiting its turn behind others tells a busy line from a
    /// stuck one by it.
    fn written(&self) -> u64;
}

pub struct CtlServer {
    listener: UnixListener,
    dir: PathBuf,
    path: PathBuf,
    /// Device and inode of the socket file this server made: only that is
    /// removed on exit.
    file: (u64, u64),
    mode: CtlMode,
    clients: Vec<Client>,
    /// Accepting failed (EMFILE, ENFILE): not polled again until then.
    accept_paused: Option<Instant>,
    /// The id the next client gets.
    next_id: u64,
    /// UNATTENDED_STALL, shorter in tests.
    unattended_stall: Duration,
    /// Where the device went away, as ring offsets, in order: a send's
    /// reply counts them, and a subscription that gave that count ends at
    /// the next.
    drops: Vec<u64>,
    /// The clients [`poll_fds`](Self::poll_fds) appended, by id, in order:
    /// [`serve`](Self::serve) gives each its own result even if the list
    /// changed in between (a subscriber dropped, clients served during a
    /// transfer).
    polled: std::cell::RefCell<Vec<u64>>,
}

/// Bytes the session has taken from a client for the device.
struct Sending {
    bytes: Vec<u8>,
    done: usize,
    /// When the line last took anything, this send's or anyone's.
    moved: Instant,
    /// The line's [`Session::written`] then.
    line: u64,
    /// When the server last looked.
    looked: Instant,
    /// The ring's end when the request came: the send's offset.
    offset: u64,
    /// How many times the device had gone away then. Not when the reply
    /// goes: a send still waiting for room when the device went away and
    /// came back has its offset before that drop.
    drops: usize,
}

struct Client {
    id: u64,
    stream: UnixStream,
    /// Received, not yet handled: request lines.
    input: Vec<u8>,
    /// How much of `input` is known to hold no newline.
    scanned: usize,
    /// A reply not yet written, from `written` on.
    output: Vec<u8>,
    written: usize,
    sending: Option<Sending>,
    /// Subscribed: the offset of the next device byte to write.
    cursor: Option<u64>,
    /// The subscription ends where the device goes away after this many
    /// drops (the `drops` of the client's send).
    until_drop: Option<u64>,
    /// A request for the session, not taken yet.
    action: Option<Action>,
    /// Its deadline (`deadline_ms`), if the client gave one.
    action_deadline: Option<u64>,
    /// The session has taken its action and owes the reply.
    awaiting: bool,
    /// No more requests: close once the output is written.
    closing: bool,
    /// The peer sends no more. What it sent is still answered.
    eof: bool,
    /// It closed the connection with requests unread: they still run
    /// (`printf ... | socat -u`), with no one to answer.
    unattended: bool,
    /// Its replies cannot be written any more. No close need ever come,
    /// so it is dropped once what it sent has run.
    unwritable: bool,
    dead: bool,
}

impl Client {
    fn new(id: u64, stream: UnixStream) -> Client {
        Client {
            id,
            stream,
            input: Vec::new(),
            scanned: 0,
            output: Vec::new(),
            written: 0,
            sending: None,
            cursor: None,
            until_drop: None,
            action: None,
            action_deadline: None,
            awaiting: false,
            closing: false,
            eof: false,
            unattended: false,
            unwritable: false,
            dead: false,
        }
    }

    /// Serving a request: no other is read until it is done, so replies
    /// come in order and a client cannot queue unbounded work.
    fn busy(&self) -> bool {
        self.written < self.output.len()
            || self.sending.is_some()
            || self.action.is_some()
            || self.awaiting
    }

    fn events(&self, ring_end: u64) -> libc::c_short {
        if self.busy() {
            return if self.written < self.output.len() {
                libc::POLLOUT
            } else {
                0
            };
        }
        match self.cursor {
            Some(at) if at < ring_end => libc::POLLIN | libc::POLLOUT,
            Some(_) => libc::POLLIN,
            None if self.closing || self.eof => 0,
            None => libc::POLLIN,
        }
    }

    fn reply(&mut self, bytes: Vec<u8>) {
        self.output = bytes;
        self.written = 0;
    }

    /// Whether the peer has closed the connection, not just its writing
    /// side (that still waits for replies). Asked with a poll for POLLOUT,
    /// which shows POLLHUP only then, on Linux and macOS alike; a busy
    /// client is otherwise polled for no events, and macOS reports no
    /// hangup for those.
    fn peer_gone(&self) -> bool {
        let mut fd = pollfd(self.stream.as_raw_fd(), libc::POLLOUT);
        // SAFETY: one valid pollfd, no wait.
        let n = unsafe { libc::poll(&mut fd, 1, 0) };
        n > 0 && fd.revents & (libc::POLLHUP | libc::POLLERR | libc::POLLNVAL) != 0
    }

    /// Sending, or waiting on the session for its action's result.
    fn waiting(&self) -> bool {
        self.sending.is_some() || self.action.is_some() || self.awaiting
    }

    /// A whole request is in `input`, waiting to be handled.
    fn has_request(&self) -> bool {
        self.input[self.scanned..].contains(&b'\n')
    }

    /// Where the first request line in `input` ends.
    fn line_end(&mut self) -> Option<usize> {
        match self.input[self.scanned..].iter().position(|&b| b == b'\n') {
            Some(i) => Some(self.scanned + i),
            None => {
                self.scanned = self.input.len();
                None
            }
        }
    }

    /// Reads what has come, up to a whole request unless `to_end`; false
    /// when the peer sends no more.
    fn take_input(&mut self, to_end: bool) -> bool {
        let mut buf = [0u8; 16 << 10];
        for _ in 0..READS_PER_STEP {
            // One request at a time: the rest stays in the socket.
            if !to_end && self.cursor.is_none() && self.line_end().is_some() {
                return true;
            }
            match self.stream.read(&mut buf) {
                Ok(0) => return false,
                // A subscriber sends nothing more; anything is dropped.
                Ok(_) if self.cursor.is_some() => {}
                Ok(n) => {
                    self.input.extend_from_slice(&buf[..n]);
                    let long = match self.line_end() {
                        Some(end) => end >= proto::LINE_MAX,
                        None => self.input.len() >= proto::LINE_MAX,
                    };
                    if long {
                        self.input = Vec::new();
                        self.scanned = 0;
                        self.reply(proto::err(code::USAGE, "request too long"));
                        self.closing = true;
                        return true;
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => return true,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(_) => return false,
            }
        }
        true
    }

    /// Writes what can be written of the reply.
    fn flush(&mut self) {
        if self.unattended {
            self.output = Vec::new();
            self.written = 0;
        }
        while self.written < self.output.len() {
            match self.stream.write(&self.output[self.written..]) {
                Ok(n) => self.written += n,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => return,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                // The peer is gone: there is no one to answer, but what it
                // sent before closing still runs.
                Err(_) => {
                    self.unattended = true;
                    self.unwritable = true;
                    self.output = Vec::new();
                    self.written = 0;
                    // Closing: nothing more it sent runs, as for one whose
                    // last bytes were written.
                    self.dead |= self.closing && self.cursor.is_none();
                    return;
                }
            }
        }
        self.output = Vec::new();
        self.written = 0;
        if self.closing && self.cursor.is_none() {
            self.dead = true;
        }
    }

    /// Writes device bytes from the ring to a subscriber; one that ends
    /// where the device went away gets the bytes before it, then
    /// [`proto::gone_line`], and is closed.
    fn feed(&mut self, ring: &Ring, drops: &[u64]) {
        let Some(at) = self.cursor else {
            return;
        };
        if self.busy() {
            return;
        }
        let gone = self
            .until_drop
            .and_then(|k| drops.get(usize::try_from(k).ok()?))
            .copied();
        let end = gone.map_or(ring.end(), |g| g.min(ring.end()));
        if at >= end {
            if let Some(g) = gone {
                self.end_at_drop(g);
            }
            return;
        }
        let max = usize::try_from(end - at).unwrap_or(usize::MAX);
        match ring.slices(at, max) {
            Ok((a, b)) => {
                let mut sent = 0;
                for part in [a, b] {
                    if part.is_empty() {
                        continue;
                    }
                    match self.stream.write(part) {
                        Ok(n) => {
                            sent += n;
                            if n < part.len() {
                                break;
                            }
                        }
                        Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                        Err(e) if e.kind() == io::ErrorKind::Interrupted => break,
                        Err(_) => {
                            self.dead = true;
                            return;
                        }
                    }
                }
                self.cursor = Some(at + sent as u64);
                // Its last bytes before the drop are out: no event may come
                // to finish it while the device is away.
                if let Some(g) = gone.filter(|&g| at + sent as u64 >= g) {
                    self.end_at_drop(g);
                }
            }
            // The ring moved past what it had not taken yet.
            Err(_) => {
                let line = proto::dropped_line(at);
                // Only if it fits now: the session does not wait for it.
                let _ = self.stream.write(line.as_bytes());
                self.dead = true;
            }
        }
    }

    /// A subscriber that has had every byte before the drop at `g`: its
    /// last words, then the close.
    fn end_at_drop(&mut self, g: u64) {
        self.cursor = None;
        self.closing = true;
        self.reply(proto::gone_line(g).into_bytes());
        self.flush();
    }

    /// Moves the client on: queued bytes to the device, the next request.
    fn step(
        &mut self,
        revents: libc::c_short,
        mode: CtlMode,
        stall: Duration,
        drops: &[u64],
        s: &mut dyn Session,
    ) {
        if revents & libc::POLLNVAL != 0 {
            self.dead = true;
            return;
        }
        let hup = revents & (libc::POLLHUP | libc::POLLERR) != 0;
        let readable = revents & libc::POLLIN != 0 || hup;
        let was_waiting = self.waiting();
        // On a hangup, read on to the end: poll reports it until then.
        if readable && !self.eof && (!self.busy() || hup) && !self.take_input(hup) {
            self.eof = true;
        }
        if !self.unattended && (hup || (revents == 0 && was_waiting)) && self.peer_gone()
        {
            // What it was waiting for is not wanted now; what it sent
            // before closing still runs, so the rest of its requests are
            // read first (they are in the socket until then).
            self.unattended = true;
            if was_waiting {
                self.sending = None;
                self.action = None;
            }
            if !self.eof && !self.take_input(true) {
                self.eof = true;
            }
        }
        loop {
            self.send_more(s);
            if self.unattended
                && let Some(x) = &mut self.sending
            {
                let (line, now) = (s.written(), Instant::now());
                let away = now.duration_since(x.looked) > 2 * STALL_LOOK;
                x.looked = now;
                if line != x.line || away {
                    x.line = line;
                    x.moved = now;
                } else if now.duration_since(x.moved) >= stall {
                    // No one is waiting for it, and the line has taken
                    // nothing of anyone's that long. The client goes with
                    // its unread requests: run after a send cut short,
                    // they would join its bytes on the device.
                    self.sending = None;
                    self.dead = true;
                    break;
                }
            }
            while !self.busy() && self.cursor.is_none() {
                let Some(end) = self.line_end() else {
                    break;
                };
                let line: Vec<u8> = self.input.drain(..=end).collect();
                self.scanned = 0;
                self.handle(&line[..end], mode, drops, s);
                // Now, not at the next event: there may be none until it is.
                self.send_more(s);
            }
            self.flush();
            // A reply written whole frees the client for a request already
            // here, which no event would announce.
            if self.dead
                || self.busy()
                || self.cursor.is_some()
                || self.line_end().is_none()
            {
                break;
            }
        }
        if self.dead {
            return;
        }
        // Nothing can be written to it and it may never close: once what
        // it sent has run, it is only holding a place. A subscriber runs no
        // more requests, and nothing it subscribed to can reach it.
        if self.unwritable
            && !self.busy()
            && (self.cursor.is_some() || !self.has_request())
        {
            self.dead = true;
            return;
        }
        // A subscriber that closes even only its writing side is done,
        // also one that asked just before closing.
        if self.eof && self.cursor.is_some() {
            self.dead = true;
            return;
        }
        if self.eof && !self.busy() {
            // Answered all it sent; a part line never ends now.
            self.dead = true;
            return;
        }
        self.feed(s.ring(), drops);
    }

    /// Gives the session what it has room for of a send; replies once all
    /// of it is taken, or the session cannot take it.
    fn send_more(&mut self, s: &mut dyn Session) {
        let Some(sending) = &mut self.sending else {
            return;
        };
        match s.send(&sending.bytes[sending.done..]) {
            Ok(n) => {
                sending.done += n;
                if n > 0 {
                    sending.moved = Instant::now();
                }
                if sending.done == sending.bytes.len() {
                    if !sending.bytes.is_empty() {
                        s.sent(&sending.bytes);
                    }
                    let (offset, drops) = (sending.offset, sending.drops);
                    self.sending = None;
                    self.reply(proto::ok(
                        serde_json::json!({ "offset": offset, "drops": drops }),
                    ));
                }
            }
            Err((code, msg)) => {
                self.sending = None;
                self.reply(proto::err(code, &msg));
            }
        }
    }

    fn handle(&mut self, line: &[u8], mode: CtlMode, drops: &[u64], s: &mut dyn Session) {
        let (request, deadline) = match serde_json::from_slice::<proto::Envelope>(line) {
            Ok(e) => (e.request, e.deadline_ms),
            Err(e) => {
                self.reply(proto::err(code::USAGE, &format!("bad request: {e}")));
                self.closing = true;
                return;
            }
        };
        if deadline.is_some_and(|d| proto::now_ms() > d) {
            return self.reply(expired());
        }
        self.action_deadline = deadline;
        let ring = s.ring();
        match request {
            Request::Status => {
                let mut status = s.status();
                status.ctl = mode.name().into();
                let body = serde_json::to_value(status).expect("status serializes");
                self.reply(proto::ok(body));
            }
            Request::Read {
                since,
                last,
                max_bytes,
                from_oldest,
            } => {
                let start = match (since, last) {
                    (Some(at), _) => at,
                    (None, Some(n)) => ring.end().saturating_sub(n).max(ring.oldest()),
                    (None, None) => ring.oldest(),
                };
                let start = if from_oldest {
                    start.max(ring.oldest())
                } else {
                    start
                };
                let max = max_bytes.min(proto::READ_MAX) as usize;
                let mut data = Vec::new();
                match ring.read(start, max, &mut data) {
                    Ok(next) => self.reply(proto::ok(serde_json::json!({
                        "start": start,
                        "next": next,
                        "data_b64": proto::base64_encode(&data),
                    }))),
                    Err(e) => self.reply(proto::err(code::EVICTED, &e.to_string())),
                }
            }
            Request::Send { data_b64 } => {
                if mode != CtlMode::ReadWrite {
                    self.reply(proto::err(
                        code::DENIED,
                        "the control socket is read-only (--ctl ro)",
                    ));
                    return;
                }
                match proto::base64_decode(&data_b64) {
                    Some(bytes) => {
                        self.sending = Some(Sending {
                            bytes,
                            done: 0,
                            moved: Instant::now(),
                            line: s.written(),
                            looked: Instant::now(),
                            offset: ring.end(),
                            drops: drops.len(),
                        });
                    }
                    None => self.reply(proto::err(code::USAGE, "data_b64 is not base64")),
                }
            }
            Request::Subscribe {
                since,
                from_oldest,
                drops: until_drop,
            } => {
                let start = match since {
                    None => ring.end(),
                    Some(at) if at < ring.oldest() && from_oldest => ring.oldest(),
                    Some(at) if at < ring.oldest() => {
                        let e = crate::ring::Evicted {
                            requested: at,
                            oldest: ring.oldest(),
                        };
                        self.reply(proto::err(code::EVICTED, &e.to_string()));
                        self.closing = true;
                        return;
                    }
                    Some(at) => at,
                };
                self.reply(proto::ok(serde_json::json!({ "start": start })));
                self.cursor = Some(start);
                self.until_drop = until_drop;
            }
            Request::Break { ms } => {
                if let Some(e) = hold_error(ms, "break") {
                    return self.reply(e);
                }
                self.act(mode, Action::Break { ms });
            }
            Request::Modem { line, op, ms } => {
                if let Some(e) = hold_error(ms, "pulse") {
                    return self.reply(e);
                }
                self.act(mode, Action::Modem { line, op, ms });
            }
            Request::Set { patch } => {
                if patch.is_empty() {
                    return self.reply(proto::err(code::USAGE, "set needs a setting"));
                }
                if let Err(e) = patch.apply(crate::cli::LineSettings::default()) {
                    return self.reply(proto::err(code::USAGE, &e));
                }
                self.act(mode, Action::Set(patch));
            }
            Request::CaptureStart { path } => {
                if path.is_empty() {
                    return self.reply(proto::err(code::USAGE, "capture needs a file"));
                }
                self.act(mode, Action::CaptureStart(PathBuf::from(path)));
            }
            Request::CaptureStop => self.act(mode, Action::CaptureStop),
            Request::Xfer {
                dir,
                proto: xproto,
                files,
                cwd,
            } => {
                let want = match (dir, xproto) {
                    (XferDir::Send, _) if files.is_empty() => {
                        Some("xfer send needs a file")
                    }
                    (XferDir::Recv, XferProto::Xmodem) if files.len() != 1 => {
                        Some("xfer recv xmodem needs one file name")
                    }
                    (XferDir::Recv, XferProto::Zmodem | XferProto::Ymodem)
                        if !files.is_empty() =>
                    {
                        Some("the sender names the files: xfer recv takes none")
                    }
                    _ => None,
                };
                if let Some(why) = want {
                    return self.reply(proto::err(code::USAGE, why));
                }
                if !Path::new(&cwd).is_absolute() {
                    return self
                        .reply(proto::err(code::USAGE, "xfer needs an absolute cwd"));
                }
                self.act(
                    mode,
                    Action::Xfer {
                        dir,
                        proto: xproto,
                        files: files.into_iter().map(PathBuf::from).collect(),
                        cwd: PathBuf::from(cwd),
                    },
                );
            }
        }
    }

    /// Hands `action` to the session, unless the socket is read-only.
    fn act(&mut self, mode: CtlMode, action: Action) {
        if mode != CtlMode::ReadWrite {
            self.reply(proto::err(
                code::DENIED,
                "the control socket is read-only (--ctl ro)",
            ));
            return;
        }
        self.action = Some(action);
    }
}

/// The reply to a request read, or taken, after its deadline.
fn expired() -> Vec<u8> {
    proto::err(
        code::FAILED,
        "not run: the session took it after the client's deadline",
    )
}

/// A reply refusing a hold of `ms`, if it is out of range.
fn hold_error(ms: u32, what: &str) -> Option<Vec<u8>> {
    (!(1..=proto::HOLD_MAX).contains(&ms)).then(|| {
        proto::err(
            code::USAGE,
            &format!("{what} of {ms} ms: 1 to {} ms", proto::HOLD_MAX),
        )
    })
}

impl CtlServer {
    /// Listens on the socket for `tty_name` in `dir` (made and checked
    /// already). A socket left by a session that is gone is replaced; one
    /// a live session answers on is an error.
    pub fn bind(
        dir: &Path,
        tty_name: &str,
        mode: CtlMode,
    ) -> Result<CtlServer, CtlError> {
        let path = super::path::socket_path(dir, tty_name);
        let failed =
            |e: io::Error| CtlError(format!("control socket {}: {e}", path.display()));
        let listener = match UnixListener::bind(&path) {
            Ok(l) => l,
            Err(e) if e.kind() == io::ErrorKind::AddrInUse => {
                if UnixStream::connect(&path).is_ok() {
                    return Err(CtlError(format!(
                        "control socket {}: another session is serving it",
                        path.display()
                    )));
                }
                std::fs::remove_file(&path).map_err(failed)?;
                UnixListener::bind(&path).map_err(failed)?
            }
            Err(e) => return Err(failed(e)),
        };
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
            .map_err(failed)?;
        listener.set_nonblocking(true).map_err(failed)?;
        let m = std::fs::symlink_metadata(&path).map_err(failed)?;
        Ok(CtlServer {
            listener,
            dir: dir.to_path_buf(),
            path,
            file: (m.dev(), m.ino()),
            mode,
            clients: Vec::new(),
            accept_paused: None,
            next_id: 0,
            unattended_stall: UNATTENDED_STALL,
            drops: Vec::new(),
            polled: std::cell::RefCell::new(Vec::new()),
        })
    }

    /// How long a send whose client has closed may go with the line taking
    /// none of it before it is dropped (30 s): for tests.
    pub fn set_unattended_stall(&mut self, stall: Duration) {
        self.unattended_stall = stall;
    }

    /// The next request that acts on the session, not taken yet, with its
    /// client's id. One whose client has closed the connection is dropped:
    /// it was given up on (a client that timed out has said it failed).
    pub fn pending_action(&mut self) -> Option<(u64, &Action)> {
        // The clock is read only for an action with a deadline: not on
        // every pass of the session loop.
        let mut now = None;
        for c in &mut self.clients {
            let gone = c.action.is_some() && c.peer_gone();
            if gone && !c.unattended {
                c.action = None;
                c.dead = true;
            } else if gone && matches!(c.action, Some(Action::Xfer { .. })) {
                // The other requests of a client that closed still run; a
                // transfer cannot. It takes the port, sends the device a
                // handshake, and `abandoned` aborts it on the first turn
                // of the loop.
                c.action = None;
            } else if c.action.is_some()
                && c.action_deadline
                    .is_some_and(|d| *now.get_or_insert_with(proto::now_ms) > d)
            {
                c.action = None;
                c.reply(expired());
                c.flush();
            }
        }
        self.clients.retain(|c| !c.dead);
        self.clients
            .iter()
            .find_map(|c| c.action.as_ref().map(|a| (c.id, a)))
    }

    /// Takes [`pending_action`](Self::pending_action), with the client's
    /// id. The client waits for [`finish_action`](Self::finish_action).
    pub fn next_action(&mut self) -> Option<(u64, Action)> {
        self.pending_action()?;
        let c = self.clients.iter_mut().find(|c| c.action.is_some())?;
        c.awaiting = true;
        Some((c.id, c.action.take().expect("found with one")))
    }

    /// Answers client `id`'s action, if the client is still there.
    pub fn finish_action(
        &mut self,
        id: u64,
        result: Result<serde_json::Value, (u8, String)>,
    ) {
        if let Some(c) = self.clients.iter_mut().find(|c| c.id == id) {
            c.awaiting = false;
            c.reply(match result {
                Ok(body) => proto::ok(body),
                Err((code, msg)) => proto::err(code, &msg),
            });
            c.flush();
        }
        self.clients.retain(|c| !c.dead);
    }

    /// Client `id` has closed the connection: what it asked for is not
    /// wanted any more. (One that closed only its writing side waits for
    /// the reply.)
    pub fn abandoned(&self, id: u64) -> bool {
        self.clients
            .iter()
            .find(|c| c.id == id)
            .is_none_or(|c| c.peer_gone())
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Moves the socket to the name of the tty the device came back as,
    /// replacing a socket left there by a session that is gone: kept at
    /// the old name, it would answer for whichever device has that now.
    pub fn rename(&mut self, tty_name: &str) -> Result<(), CtlError> {
        let to = super::path::socket_path(&self.dir, tty_name);
        if to == self.path {
            return Ok(());
        }
        if std::fs::symlink_metadata(&to).is_ok() && UnixStream::connect(&to).is_ok() {
            return Err(CtlError(format!(
                "control socket {}: another session is serving it, staying at {}",
                to.display(),
                self.path.display()
            )));
        }
        // rename(2) replaces the dead one in the same step.
        std::fs::rename(&self.path, &to)
            .map_err(|e| CtlError(format!("control socket {}: {e}", to.display())))?;
        self.path = to;
        Ok(())
    }

    /// Appends what to poll: the listener, then each client, in the order
    /// [`serve`](Self::serve) takes their results. Returns when the server
    /// wants a pass with no event.
    pub fn poll_fds(
        &self,
        ring_end: u64,
        out: &mut Vec<libc::pollfd>,
    ) -> Option<Instant> {
        let paused = self.accept_paused.filter(|&t| Instant::now() < t);
        let listen = if paused.is_some() { 0 } else { libc::POLLIN };
        out.push(pollfd(self.listener.as_raw_fd(), listen));
        let mut polled = self.polled.borrow_mut();
        polled.clear();
        for c in &self.clients {
            polled.push(c.id);
            // One that closed its end and waits on the session has nothing
            // to poll for, and a hangup would be reported on every pass.
            let fd = if c.eof && c.written >= c.output.len() {
                -1
            } else {
                c.stream.as_raw_fd()
            };
            out.push(pollfd(fd, c.events(ring_end)));
        }
        // A client that has closed is polled for nothing, so a request of
        // its own that is already read needs a pass of its own to run in;
        // so does dropping one that cannot be answered.
        let ready = self.clients.iter().any(|c| {
            !c.busy() && (c.unwritable || (c.cursor.is_none() && c.has_request()))
        });
        // An unattended send is dropped once it has stalled long enough,
        // and nothing else may come to wake the loop then.
        let stalled = self
            .clients
            .iter()
            .filter(|c| c.unattended)
            .filter_map(|c| {
                c.sending
                    .as_ref()
                    .map(|x| (x.moved + self.unattended_stall).min(x.looked + STALL_LOOK))
            })
            .min();
        if ready {
            Some(Instant::now())
        } else {
            [paused, stalled].into_iter().flatten().min()
        }
    }

    /// Acts on what poll found, `ready` being what
    /// [`poll_fds`](Self::poll_fds) appended. Clients also move on without
    /// an event: a send waits for room in the session's queue.
    pub fn serve(&mut self, ready: &[libc::pollfd], s: &mut dyn Session) {
        let polled = self.polled.borrow();
        for c in &mut self.clients {
            let revents = polled
                .iter()
                .position(|&id| id == c.id)
                .and_then(|i| ready.get(i + 1))
                // Polled for this client: a transfer's own passes record
                // their clients over these, which may not be `ready`'s.
                .filter(|fd| fd.fd == c.stream.as_raw_fd())
                .map_or(0, |fd| fd.revents);
            c.step(revents, self.mode, self.unattended_stall, &self.drops, s);
        }
        drop(polled);
        self.clients.retain(|c| !c.dead);
        if self.accept_paused.is_some_and(|t| Instant::now() >= t) {
            self.accept_paused = None;
        }
        if self.accept_paused.is_none()
            && ready.first().is_some_and(|l| l.revents & libc::POLLIN != 0)
        {
            self.accept(s);
        }
    }

    /// Device bytes have just gone into the ring: subscribers get them now
    /// rather than at the next poll.
    pub fn device_output(&mut self, ring: &Ring) {
        let mut gone = false;
        for c in &mut self.clients {
            if c.cursor.is_some() {
                c.feed(ring, &self.drops);
                gone |= c.dead;
            }
        }
        if gone {
            self.clients.retain(|c| !c.dead);
        }
    }

    /// The device has gone away with the ring ending at `at`: subscribers
    /// that end there get the rest of their bytes and their last words.
    pub fn device_gone(&mut self, at: u64, ring: &Ring) {
        self.drops.push(at);
        let mut gone = false;
        for c in &mut self.clients {
            if c.until_drop.is_some() {
                c.feed(ring, &self.drops);
                gone |= c.dead;
            }
        }
        if gone {
            self.clients.retain(|c| !c.dead);
        }
    }

    fn accept(&mut self, s: &mut dyn Session) {
        loop {
            let stream = match self.listener.accept() {
                Ok((stream, _)) => stream,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => return,
                // Out of descriptors, or the like.
                Err(_) => {
                    self.accept_paused = Some(Instant::now() + ACCEPT_PAUSE);
                    return;
                }
            };
            // Another user (root, say) has no business here: closed with no
            // reply.
            if peer_uid(stream.as_raw_fd()) != Some(rustix::process::geteuid().as_raw())
                || stream.set_nonblocking(true).is_err()
            {
                continue;
            }
            if self.clients.len() >= MAX_CLIENTS {
                // A short line into an empty socket buffer: it fits.
                let msg = format!("{MAX_CLIENTS} clients are connected already");
                let _ = (&stream).write(&proto::err(code::FAILED, &msg));
                continue;
            }
            let mut c = Client::new(self.next_id, stream);
            self.next_id += 1;
            // A request may already be there, and the close with it: then
            // it is unread at the close like any other, not given up on.
            let hup = if c.peer_gone() { libc::POLLHUP } else { 0 };
            c.step(
                libc::POLLIN | hup,
                self.mode,
                self.unattended_stall,
                &self.drops,
                s,
            );
            if !c.dead {
                self.clients.push(c);
            }
        }
    }
}

impl Drop for CtlServer {
    fn drop(&mut self) {
        // Only the file this server made: another session may have
        // replaced it since.
        if std::fs::symlink_metadata(&self.path)
            .is_ok_and(|m| (m.dev(), m.ino()) == self.file)
        {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

fn pollfd(fd: RawFd, events: libc::c_short) -> libc::pollfd {
    libc::pollfd {
        fd,
        events,
        revents: 0,
    }
}

#[cfg(target_os = "linux")]
fn peer_uid(fd: RawFd) -> Option<u32> {
    let mut cred = libc::ucred {
        pid: 0,
        uid: 0,
        gid: 0,
    };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    // SAFETY: `cred` and `len` are valid for SO_PEERCRED's out arguments.
    let r = unsafe {
        libc::getsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&mut cred as *mut libc::ucred).cast(),
            &mut len,
        )
    };
    (r == 0).then_some(cred.uid)
}

#[cfg(not(target_os = "linux"))]
fn peer_uid(fd: RawFd) -> Option<u32> {
    let mut uid: libc::uid_t = 0;
    let mut gid: libc::gid_t = 0;
    // SAFETY: `uid` and `gid` are valid out pointers.
    let r = unsafe { libc::getpeereid(fd, &mut uid, &mut gid) };
    (r == 0).then_some(uid)
}
