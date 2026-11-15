//! The session's end of the control socket, served from the session's own
//! poll loop: it never blocks, and costs nothing per device read while no
//! one subscribes.

use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use super::proto::{self, Request, Status};
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

/// What the server needs from the session.
pub trait Session {
    /// Everything but the `ctl` field, which the server fills in.
    fn status(&self) -> Status;
    fn ring(&self) -> &Ring;
    /// Takes as much of `bytes` for the device as there is room for, and
    /// says how much. Fails with a reply code while the device cannot take
    /// any (disconnected, a file transfer running).
    fn send(&mut self, bytes: &[u8]) -> Result<usize, (u8, String)>;
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
}

/// Bytes the session has taken from a client for the device.
struct Sending {
    bytes: Vec<u8>,
    done: usize,
    /// The ring's end when the request came: the send's offset.
    offset: u64,
}

struct Client {
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
    /// No more requests: close once the output is written.
    closing: bool,
    /// The peer sends no more. What it sent is still answered.
    eof: bool,
    dead: bool,
}

impl Client {
    fn new(stream: UnixStream) -> Client {
        Client {
            stream,
            input: Vec::new(),
            scanned: 0,
            output: Vec::new(),
            written: 0,
            sending: None,
            cursor: None,
            closing: false,
            eof: false,
            dead: false,
        }
    }

    /// Serving a request: no other is read until it is done, so replies
    /// come in order and a client cannot queue unbounded work.
    fn busy(&self) -> bool {
        self.written < self.output.len() || self.sending.is_some()
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
        while self.written < self.output.len() {
            match self.stream.write(&self.output[self.written..]) {
                Ok(n) => self.written += n,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => return,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(_) => {
                    self.dead = true;
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

    /// Writes device bytes from the ring to a subscriber.
    fn feed(&mut self, ring: &Ring) {
        let Some(at) = self.cursor else {
            return;
        };
        if self.busy() || at >= ring.end() {
            return;
        }
        match ring.slices(at, usize::MAX) {
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
            }
            // The ring moved past what it had not taken yet.
            Err(_) => {
                let line = format!(
                    "\n{{\"ok\":false,\"code\":{},\"error\":\"subscriber too slow at offset {at}\"}}\n",
                    code::DROPPED
                );
                // Only if it fits now: the session does not wait for it.
                let _ = self.stream.write(line.as_bytes());
                self.dead = true;
            }
        }
    }

    /// Moves the client on: queued bytes to the device, the next request.
    fn step(&mut self, revents: libc::c_short, mode: CtlMode, s: &mut dyn Session) {
        if revents & libc::POLLNVAL != 0 {
            self.dead = true;
            return;
        }
        let hup = revents & (libc::POLLHUP | libc::POLLERR) != 0;
        let readable = revents & libc::POLLIN != 0 || hup;
        // On a hangup, read on to the end: poll reports it until then.
        if readable && !self.eof && (!self.busy() || hup) && !self.take_input(hup) {
            self.eof = true;
        }
        loop {
            self.send_more(s);
            while !self.busy() && self.cursor.is_none() {
                let Some(end) = self.line_end() else {
                    break;
                };
                let line: Vec<u8> = self.input.drain(..=end).collect();
                self.scanned = 0;
                self.handle(&line[..end], mode, s);
                // Now, not at the next event: there may be none until it is.
                self.send_more(s);
            }
            self.flush();
            // A reply written whole frees the client for a request already
            // here, which no event would announce.
            if self.dead || self.busy() || self.cursor.is_some() || self.line_end().is_none() {
                break;
            }
        }
        if self.dead {
            return;
        }
        // Closing its end (even only for writing) cancels a send and ends a
        // subscription, also one it asked for just before closing; else poll
        // would report the end on every pass until they finish.
        if self.eof && (self.sending.is_some() || self.cursor.is_some()) {
            self.dead = true;
            return;
        }
        if self.eof && !self.busy() {
            // Answered all it sent; a part line never ends now.
            self.dead = true;
            return;
        }
        self.feed(s.ring());
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
                if sending.done == sending.bytes.len() {
                    let offset = sending.offset;
                    self.sending = None;
                    self.reply(proto::ok(serde_json::json!({ "offset": offset })));
                }
            }
            Err((code, msg)) => {
                self.sending = None;
                self.reply(proto::err(code, &msg));
            }
        }
    }

    fn handle(&mut self, line: &[u8], mode: CtlMode, s: &mut dyn Session) {
        let request: Request = match serde_json::from_slice(line) {
            Ok(r) => r,
            Err(e) => {
                self.reply(proto::err(code::USAGE, &format!("bad request: {e}")));
                self.closing = true;
                return;
            }
        };
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
                            offset: ring.end(),
                        });
                    }
                    None => self.reply(proto::err(code::USAGE, "data_b64 is not base64")),
                }
            }
            Request::Subscribe { since, from_oldest } => {
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
            }
        }
    }
}

impl CtlServer {
    /// Listens on the socket for `tty_name` in `dir` (made and checked
    /// already). A socket left by a session that is gone is replaced; one
    /// a live session answers on is an error.
    pub fn bind(dir: &Path, tty_name: &str, mode: CtlMode) -> Result<CtlServer, CtlError> {
        let path = super::path::socket_path(dir, tty_name);
        let failed = |e: io::Error| CtlError(format!("control socket {}: {e}", path.display()));
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
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).map_err(failed)?;
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
        })
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
    pub fn poll_fds(&self, ring_end: u64, out: &mut Vec<libc::pollfd>) -> Option<Instant> {
        let paused = self.accept_paused.filter(|&t| Instant::now() < t);
        let listen = if paused.is_some() { 0 } else { libc::POLLIN };
        out.push(pollfd(self.listener.as_raw_fd(), listen));
        for c in &self.clients {
            out.push(pollfd(c.stream.as_raw_fd(), c.events(ring_end)));
        }
        paused
    }

    /// Acts on what poll found, `ready` being what
    /// [`poll_fds`](Self::poll_fds) appended. Clients also move on without
    /// an event: a send waits for room in the session's queue.
    pub fn serve(&mut self, ready: &[libc::pollfd], s: &mut dyn Session) {
        let n = self.clients.len().min(ready.len().saturating_sub(1));
        for (i, c) in self.clients.iter_mut().enumerate() {
            let revents = if i < n { ready[i + 1].revents } else { 0 };
            c.step(revents, self.mode, s);
        }
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
                c.feed(ring);
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
            let mut c = Client::new(stream);
            // A request may already be there.
            c.step(libc::POLLIN, self.mode, s);
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
        if std::fs::symlink_metadata(&self.path).is_ok_and(|m| (m.dev(), m.ino()) == self.file) {
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
