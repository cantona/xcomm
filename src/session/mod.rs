//! The terminal session: one poll(2) loop moving bytes between the host
//! terminal and the serial port.

mod commands;
pub mod outq;

use std::io;
use std::ops::ControlFlow;
use std::os::fd::{AsFd, BorrowedFd};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use rustix::event::{PollFd, PollFlags, poll};

use crate::cli::TermOpts;
use crate::hosttty::{RawMode, SignalPipe};
use crate::keys::{KeyAction, KeyParser};
use crate::lock::{self, Cleared, UucpLock};
use crate::port::{self, Applied, LowLatency, Port};
use outq::OutQueue;

/// How a session ended.
#[derive(Debug, PartialEq, Eq)]
pub enum ExitReason {
    /// The terminal side closed (EOF or hangup on stdin).
    StdinClosed,
    /// SIGTERM, SIGINT or SIGHUP.
    Signal(i32),
    /// The device went away (unplugged, or the other end closed).
    DeviceGone,
    /// The output side went away: the terminal hung up (EIO) or the pipe's
    /// reader left (EPIPE).
    TerminalGone,
    /// The caller's `tick` asked to stop (tests).
    Requested,
    /// The user's quit command.
    Quit,
}

/// What a command key did.
#[derive(Debug, PartialEq, Eq)]
pub enum CommandOutcome {
    Continue,
    Exit(ExitReason),
}

/// What a command may use: the keyboard (for prompts) and the signal pipe
/// (which must still end the session while a prompt waits).
pub struct CommandIo<'a> {
    pub stdin: BorrowedFd<'a>,
    pub signals: &'a SignalPipe,
    /// Keys read after the command key and not yet handled. A prompt takes
    /// its input from here first; what it leaves is handled as usual.
    pub typed: &'a [u8],
}

#[derive(Debug)]
pub struct SessionError(pub String);

impl std::fmt::Display for SessionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// What the loop needs from its owner.
pub trait Control {
    /// SIGTSTP arrived: give the terminal back. `halt` follows.
    fn stop(&mut self) -> io::Result<()>;
    /// Stop the process until SIGCONT (raise SIGSTOP). A no-op by default,
    /// for tests that drive the loop in-process.
    fn halt(&mut self) {}
    /// Continued after `stop`: take the terminal again. EINTR (a signal
    /// while still stopped by SIGTTOU in the background) is retried by the
    /// caller unless that signal ends the session.
    fn resume(&mut self) -> io::Result<()> {
        Ok(())
    }
    /// Called once per loop iteration, before waiting.
    fn tick(&mut self) -> ControlFlow<()> {
        ControlFlow::Continue(())
    }
    /// The key typed after the prefix.
    fn command(
        &mut self,
        _key: u8,
        _io: &mut CommandIo<'_>,
    ) -> Result<CommandOutcome, SessionError> {
        Ok(CommandOutcome::Continue)
    }
    /// Whether keyboard input is also shown on the terminal.
    fn local_echo(&self) -> bool {
        false
    }
    /// Keys were dropped: the device has taken nothing for STALL. Called
    /// once per stall.
    fn dropping_input(&mut self) {}
}

/// The file descriptors the loop moves bytes between.
pub struct Pump<'a> {
    /// None when there is no input to forward. (A stdin closed before xcomm
    /// starts is reopened onto /dev/null by the Rust runtime, so it reads as
    /// end of input.)
    pub stdin: Option<BorrowedFd<'a>>,
    pub stdout: BorrowedFd<'a>,
    pub device: BorrowedFd<'a>,
    pub signals: &'a SignalPipe,
    /// The command prefix, when keyboard input is a terminal; None passes
    /// every byte through (piped input must not trigger commands).
    pub prefix: Option<(crate::cli::PrefixKey, std::time::Duration)>,
}

/// Room kept in the output queue for bytes the key parser holds, which it
/// releases in addition to the bytes of the next read.
const HELD_ROOM: usize = crate::keys::HELD_MAX;
/// How long the device may take nothing from a full queue before keyboard
/// input is read anyway, so that the commands (quit) still work. Keys that
/// do not fit are dropped then.
const STALL: Duration = Duration::from_secs(2);
/// How long quitting waits for queued keys to reach the device.
const QUIT_DRAIN: Duration = Duration::from_secs(1);

const BUF_SIZE: usize = 64 * 1024;

const TERMINATE: [i32; 3] = [libc::SIGTERM, libc::SIGINT, libc::SIGHUP];

/// The signals the session takes on its pipe. SIGTTIN and SIGTTOU keep
/// their default action: a session in the background stops when it touches
/// the terminal, until brought back to the foreground. A signal the parent
/// set to be ignored stays ignored (`nohup` ignores SIGHUP; a script's
/// background job ignores SIGINT).
pub fn signals() -> Vec<i32> {
    [TERMINATE[0], TERMINATE[1], TERMINATE[2], libc::SIGTSTP]
        .into_iter()
        .filter(|&sig| !crate::hosttty::ignored(sig))
        .collect()
}

fn has(bits: u64, sig: i32) -> bool {
    bits & (1u64 << sig) != 0
}

fn terminating(bits: u64) -> Option<ExitReason> {
    TERMINATE
        .iter()
        .find(|&&s| has(bits, s))
        .map(|&s| ExitReason::Signal(s))
}

/// What drained signals call for.
enum Signaled {
    Nothing,
    /// Stopped and continued: anything polled before is stale.
    Resumed,
    Exit(ExitReason),
}

fn pending_exit(signals: &SignalPipe) -> Result<Option<ExitReason>, SessionError> {
    Ok(terminating(
        signals.drain().map_err(|e| SessionError(e.to_string()))?,
    ))
}

/// Acts on drained signal bits: a termination signal ends the loop, SIGTSTP
/// stops the session until it is continued.
fn on_signals(
    mut bits: u64,
    signals: &SignalPipe,
    ctl: &mut dyn Control,
) -> Result<Signaled, SessionError> {
    let mut resumed = false;
    loop {
        if let Some(exit) = terminating(bits) {
            return Ok(Signaled::Exit(exit));
        }
        if !has(bits, libc::SIGTSTP) {
            return Ok(if resumed {
                Signaled::Resumed
            } else {
                Signaled::Nothing
            });
        }
        // Closing the terminal while stopped sends SIGHUP and SIGCONT; the
        // terminal is gone, so that hangup, not the failed call, is the
        // reason.
        if let Err(e) = ctl.stop() {
            return match pending_exit(signals)? {
                Some(exit) => Ok(Signaled::Exit(exit)),
                None => Err(SessionError(format!("job control: {e}"))),
            };
        }
        // A SIGTSTP caught while giving the terminal back (a quick second
        // Ctrl-Z) is part of this stop.
        if let Some(exit) = pending_exit(signals)? {
            return Ok(Signaled::Exit(exit));
        }
        ctl.halt();
        loop {
            match ctl.resume() {
                Ok(()) => break,
                Err(e) => {
                    if let Some(exit) = pending_exit(signals)? {
                        return Ok(Signaled::Exit(exit));
                    }
                    if e.kind() != io::ErrorKind::Interrupted {
                        return Err(SessionError(format!("job control: {e}")));
                    }
                }
            }
        }
        resumed = true;
        // A signal sent while stopped (`kill -TERM` then `-CONT`) was
        // caught as the process continued, and one may have come since:
        // act on it now, or the caller could block in a write with it
        // unseen.
        bits = signals.drain().map_err(|e| SessionError(e.to_string()))?;
    }
}

/// Writes all of `bytes` to the (blocking) terminal. Termination signals
/// arrive without SA_RESTART, so a write stalled on a terminal that is not
/// being read returns EINTR and the session can still exit. A signal that
/// arrives after the last check but before the write blocks is not seen
/// until the write returns: closing that window would cost a poll per write
/// on the data path, and a lock left by a later SIGKILL is cleared as stale.
fn write_out(
    fd: BorrowedFd<'_>,
    mut bytes: &[u8],
    p: &Pump<'_>,
    ctl: &mut dyn Control,
) -> Result<Signaled, SessionError> {
    // A stop and resume during the write makes the caller's readiness stale.
    let mut resumed = false;
    let mut check = |ctl: &mut dyn Control| -> Result<Option<ExitReason>, SessionError> {
        let bits = p.signals.drain().map_err(|e| SessionError(e.to_string()))?;
        match on_signals(bits, p.signals, ctl)? {
            Signaled::Exit(exit) => Ok(Some(exit)),
            Signaled::Resumed => {
                resumed = true;
                Ok(None)
            }
            Signaled::Nothing => Ok(None),
        }
    };
    while !bytes.is_empty() {
        match rustix::io::write(fd, bytes) {
            Ok(n) => {
                bytes = &bytes[n..];
                // A caught signal can end a blocked write early with a short
                // count instead of EINTR; the next write would block again
                // with the signal unseen.
                if !bytes.is_empty()
                    && let Some(exit) = check(ctl)?
                {
                    return Ok(Signaled::Exit(exit));
                }
            }
            // EIO: the terminal hung up. EPIPE: stdout is a pipe whose reader
            // left (`xcomm ... | head`). Either way the output side is gone.
            Err(rustix::io::Errno::IO | rustix::io::Errno::PIPE) => {
                return Ok(Signaled::Exit(ExitReason::TerminalGone));
            }
            Err(rustix::io::Errno::INTR) => {
                if let Some(exit) = check(ctl)? {
                    return Ok(Signaled::Exit(exit));
                }
            }
            // A non-blocking terminal (set so by another program): wait for
            // it, and for signals, which must still end or stop the session.
            Err(rustix::io::Errno::AGAIN) => {
                let signal_fd = p.signals.fd();
                let mut fds = [
                    PollFd::new(&fd, PollFlags::OUT),
                    PollFd::new(&signal_fd, PollFlags::IN),
                ];
                match poll(&mut fds, None) {
                    Ok(_) | Err(rustix::io::Errno::INTR) => {}
                    Err(e) => return Err(SessionError(format!("poll: {e}"))),
                }
                if let Some(exit) = check(ctl)? {
                    return Ok(Signaled::Exit(exit));
                }
            }
            Err(e) => return Err(SessionError(format!("write to terminal: {e}"))),
        }
    }
    Ok(if resumed {
        Signaled::Resumed
    } else {
        Signaled::Nothing
    })
}

/// Writes what is queued for the device, for up to QUIT_DRAIN: keys typed
/// before a quit still go out, and a device that takes nothing (held off by
/// flow control) cannot keep the session from ending. A termination signal
/// ends the wait.
fn drain_queue(
    queue: &mut OutQueue,
    p: &Pump<'_>,
    ctl: &mut dyn Control,
) -> Result<Option<ExitReason>, SessionError> {
    let deadline = Instant::now() + QUIT_DRAIN;
    let signal_fd = p.signals.fd();
    while !queue.is_empty() {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            break;
        }
        let left = rustix::event::Timespec::try_from(left).expect("under QUIT_DRAIN");
        let mut fds = [
            PollFd::new(&p.device, PollFlags::OUT),
            PollFd::new(&signal_fd, PollFlags::IN),
        ];
        match poll(&mut fds, Some(&left)) {
            Ok(_) | Err(rustix::io::Errno::INTR) => {}
            Err(e) => return Err(SessionError(format!("poll: {e}"))),
        }
        if fds[1].revents().contains(PollFlags::IN) {
            let bits = p.signals.drain().map_err(|e| SessionError(e.to_string()))?;
            if let Signaled::Exit(exit) = on_signals(bits, p.signals, ctl)? {
                return Ok(Some(exit));
            }
        }
        if fds[0]
            .revents()
            .intersects(PollFlags::OUT | PollFlags::ERR | PollFlags::HUP)
        {
            match queue.write_to(p.device) {
                Ok(_) => {}
                // Gone: nothing more can reach it, and the session is ending.
                Err(e)
                    if e.raw_os_error()
                        .is_some_and(|c| gone(rustix::io::Errno::from_raw_os_error(c))) =>
                {
                    break;
                }
                Err(e) => return Err(SessionError(format!("write to device: {e}"))),
            }
        }
    }
    Ok(None)
}

fn gone(e: rustix::io::Errno) -> bool {
    matches!(
        e,
        rustix::io::Errno::IO | rustix::io::Errno::NXIO | rustix::io::Errno::NODEV
    )
}

/// Moves bytes until something ends the session. Does not allocate once
/// running: the buffer and queue are made before the loop.
pub fn pump(p: &Pump<'_>, ctl: &mut dyn Control) -> Result<ExitReason, SessionError> {
    let mut buf = vec![0u8; BUF_SIZE].into_boxed_slice();
    let mut queue = OutQueue::with_capacity(BUF_SIZE);
    let stdin_fd = p.stdin;
    let mut stdin_open = stdin_fd.is_some();
    // Whether input can end the session; false when there is none to read.
    let mut have_input = stdin_fd.is_some();
    // poll needs a descriptor in every slot. When stdin is not wanted (closed,
    // or the queue is full) the slot holds the signal pipe with no events:
    // stdin itself would still report POLLHUP and make poll return at once.
    let signal_fd = p.signals.fd();
    let mut keys = p.prefix.map(|(key, time)| KeyParser::new(key, time));
    // Local echo of what goes to the device; never grows past this.
    let mut echo = Vec::with_capacity(if keys.is_some() {
        BUF_SIZE + HELD_ROOM
    } else {
        0
    });
    // Since when a full queue has kept keyboard input waiting, and whether
    // that has lasted STALL. Piped input just waits for the device.
    let mut full_since: Option<Instant> = None;
    let mut stalled = false;
    let mut told_dropping = false;
    loop {
        if ctl.tick().is_break() {
            return Ok(ExitReason::Requested);
        }
        let full = queue.free() <= HELD_ROOM;
        if full && stdin_open && keys.is_some() {
            let since = *full_since.get_or_insert_with(Instant::now);
            stalled = stalled || since.elapsed() >= STALL;
        } else {
            full_since = None;
            stalled = false;
            told_dropping = false;
        }
        let want_stdin = stdin_open && (!full || stalled);
        let device_events = if queue.is_empty() {
            PollFlags::IN
        } else {
            PollFlags::IN | PollFlags::OUT
        };
        let (stdin_slot, stdin_events) = match stdin_fd {
            Some(fd) if want_stdin => (fd, PollFlags::IN),
            _ => (signal_fd, PollFlags::empty()),
        };
        let mut fds = [
            PollFd::new(&signal_fd, PollFlags::IN),
            PollFd::new(&p.device, device_events),
            PollFd::new(&stdin_slot, stdin_events),
        ];
        // A held lone ESC is given up on at its deadline; a full queue is
        // looked at again when it would count as stalled.
        let held_due = keys.as_ref().and_then(|k| k.deadline());
        let stall_due = full_since.filter(|_| !stalled).map(|t| t + STALL);
        let due = match (held_due, stall_due) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        };
        let timeout = due.map(|due| {
            rustix::event::Timespec::try_from(due.saturating_duration_since(Instant::now()))
                .unwrap_or(rustix::event::Timespec {
                    tv_sec: 0,
                    tv_nsec: 0,
                })
        });
        match poll(&mut fds, timeout.as_ref()) {
            Ok(_) => {}
            Err(rustix::io::Errno::INTR) => continue,
            Err(e) => return Err(SessionError(format!("poll: {e}"))),
        }
        if let Some(k) = keys.as_mut().filter(|k| k.deadline().is_some()) {
            let echoing = ctl.local_echo();
            // Room was kept for held bytes, except for what a stall held:
            // that can find the queue still full.
            let mut dropped = false;
            k.on_deadline(Instant::now(), &mut |a| {
                if let KeyAction::Forward(b) = a {
                    let queued = queue.push(b);
                    dropped |= !queued;
                    if queued && echoing {
                        echo.extend_from_slice(b);
                    }
                }
            });
            if dropped && !told_dropping {
                told_dropping = true;
                ctl.dropping_input();
            }
            if !echo.is_empty() {
                let r = write_out(p.stdout, &echo, p, ctl)?;
                echo.clear();
                match r {
                    Signaled::Exit(exit) => return Ok(exit),
                    Signaled::Resumed => continue,
                    Signaled::Nothing => {}
                }
            }
        }
        let sig_ready = fds[0].revents();
        let dev_ready = fds[1].revents();
        let stdin_ready = if want_stdin {
            fds[2].revents()
        } else {
            PollFlags::empty()
        };

        if sig_ready.contains(PollFlags::IN) {
            let bits = p.signals.drain().map_err(|e| SessionError(e.to_string()))?;
            match on_signals(bits, p.signals, ctl)? {
                Signaled::Exit(exit) => return Ok(exit),
                // Readiness seen before the stop is stale (the shell may have
                // read the pending keystroke meanwhile): poll again.
                Signaled::Resumed => continue,
                Signaled::Nothing => {}
            }
        }

        if dev_ready.intersects(PollFlags::IN | PollFlags::HUP | PollFlags::ERR) {
            match rustix::io::read(p.device, &mut buf[..]) {
                Ok(0) => return Ok(ExitReason::DeviceGone),
                Ok(n) => match write_out(p.stdout, &buf[..n], p, ctl)? {
                    Signaled::Exit(exit) => return Ok(exit),
                    Signaled::Resumed => continue,
                    Signaled::Nothing => {}
                },
                Err(rustix::io::Errno::AGAIN | rustix::io::Errno::INTR) => {}
                Err(e) if gone(e) => return Ok(ExitReason::DeviceGone),
                Err(e) => return Err(SessionError(format!("read from device: {e}"))),
            }
        }

        if dev_ready.contains(PollFlags::OUT)
            && let Some(exit) = write_queue(&mut queue, p.device, &mut full_since, &mut stalled)?
        {
            return Ok(exit);
        }

        // Held bytes released earlier in this pass may have used the room
        // kept for them.
        let room = if stalled {
            buf.len()
        } else {
            queue.free().saturating_sub(HELD_ROOM).min(buf.len())
        };
        if let Some(stdin) = stdin_fd
            && room > 0
            && stdin_ready.intersects(PollFlags::IN | PollFlags::HUP | PollFlags::ERR)
        {
            match rustix::io::read(stdin, &mut buf[..room]) {
                Ok(0) => stdin_open = false,
                Ok(n) => match keys.as_mut() {
                    None => {
                        let queued = queue.push(&buf[..n]);
                        debug_assert!(queued, "read was bounded by the free space");
                    }
                    Some(k) => {
                        // A command acts before the keys after it are
                        // handled. Bytes queued before it may still reach
                        // the device after it.
                        let mut rest = &buf[..n];
                        while !rest.is_empty() {
                            let echoing = ctl.local_echo();
                            let mut command = None;
                            let mut dropped = false;
                            let used = k.feed(rest, Instant::now(), &mut |a| match a {
                                // The read was bounded by the free space,
                                // except in a stall, or with bytes a stall
                                // held.
                                KeyAction::Forward(b) => {
                                    let queued = queue.push(b);
                                    dropped |= !queued;
                                    if queued && echoing {
                                        echo.extend_from_slice(b);
                                    }
                                }
                                KeyAction::Command(c) => command = Some(c),
                            });
                            rest = &rest[used..];
                            if dropped && !told_dropping {
                                told_dropping = true;
                                ctl.dropping_input();
                            }
                            if !echo.is_empty() {
                                let r = write_out(p.stdout, &echo, p, ctl)?;
                                echo.clear();
                                if let Signaled::Exit(exit) = r {
                                    return Ok(exit);
                                }
                            }
                            let Some(c) = command else {
                                continue;
                            };
                            let mut io = CommandIo {
                                stdin,
                                signals: p.signals,
                                typed: rest,
                            };
                            let outcome = ctl.command(c, &mut io)?;
                            rest = io.typed;
                            match outcome {
                                CommandOutcome::Continue => {}
                                CommandOutcome::Exit(ExitReason::Quit) => {
                                    return Ok(drain_queue(&mut queue, p, ctl)?
                                        .unwrap_or(ExitReason::Quit));
                                }
                                CommandOutcome::Exit(exit) => return Ok(exit),
                            }
                        }
                    }
                },
                Err(rustix::io::Errno::AGAIN | rustix::io::Errno::INTR) => {}
                // The terminal is gone; that is the end of input too.
                Err(e) if gone(e) => stdin_open = false,
                // Not readable (stdin opened write-only): there is no input,
                // which does not end the session.
                Err(rustix::io::Errno::BADF | rustix::io::Errno::INVAL) => {
                    stdin_open = false;
                    have_input = false;
                }
                Err(e) => return Err(SessionError(format!("read from terminal: {e}"))),
            }
        }
        // Keys go out now, not after another poll says the device can take
        // them: that pass is most of a keystroke's latency. Keyboard input
        // only; piped input waits for poll as before. A device that cannot
        // take them says EAGAIN, and poll takes over.
        if keys.is_some()
            && stdin_ready.intersects(PollFlags::IN)
            && !queue.is_empty()
            && let Some(exit) = write_queue(&mut queue, p.device, &mut full_since, &mut stalled)?
        {
            return Ok(exit);
        }
        // End of input (not the absence of input) ends the session once
        // everything read has reached the device (`cat file | xcomm dev` must not lose the tail). The
        // loop keeps writing meanwhile; a signal still ends it at any time.
        if have_input && !stdin_open && queue.is_empty() {
            // The kernel and the adapter may still hold some of it; closing
            // the port would discard that. tcdrain waits until it has gone
            // out; a signal (no SA_RESTART) interrupts it. One that came in
            // since the last poll is looked at first, or tcdrain could wait
            // on a held-off device with it unseen.
            if let Signaled::Exit(exit) = on_signals(
                p.signals.drain().map_err(|e| SessionError(e.to_string()))?,
                p.signals,
                ctl,
            )? {
                return Ok(exit);
            }
            loop {
                match rustix::termios::tcdrain(p.device) {
                    Ok(()) => return Ok(ExitReason::StdinClosed),
                    Err(rustix::io::Errno::INTR) => {
                        let bits = p.signals.drain().map_err(|e| SessionError(e.to_string()))?;
                        if let Signaled::Exit(exit) = on_signals(bits, p.signals, ctl)? {
                            return Ok(exit);
                        }
                    }
                    Err(e) if gone(e) => return Ok(ExitReason::DeviceGone),
                    Err(e) => return Err(SessionError(format!("drain device output: {e}"))),
                }
            }
        }
    }
}

/// Writes what the device takes of `queue`; Some when the device is gone.
fn write_queue(
    queue: &mut OutQueue,
    device: BorrowedFd<'_>,
    full_since: &mut Option<Instant>,
    stalled: &mut bool,
) -> Result<Option<ExitReason>, SessionError> {
    match queue.write_to(device) {
        Ok(0) => Ok(None),
        Ok(_) => {
            *full_since = None;
            *stalled = false;
            Ok(None)
        }
        Err(e)
            if e.raw_os_error()
                .is_some_and(|c| gone(rustix::io::Errno::from_raw_os_error(c))) =>
        {
            Ok(Some(ExitReason::DeviceGone))
        }
        Err(e) => Err(SessionError(format!("write to device: {e}"))),
    }
}

/// Whether `fd` was opened for reading. nohup reopens stdin as /dev/null
/// write-only; there is then no input, and the session runs without it.
fn readable(fd: BorrowedFd<'_>) -> bool {
    match rustix::fs::fcntl_getfl(fd) {
        Ok(flags) => !flags.contains(rustix::fs::OFlags::WRONLY),
        Err(_) => false,
    }
}

/// `ttyUSB1` means `/dev/ttyUSB1`; anything with a `/` is used as given.
pub fn device_path(device: &str) -> PathBuf {
    if device.contains('/') {
        PathBuf::from(device)
    } else {
        Path::new("/dev").join(device)
    }
}

/// Status lines for the user on stderr. With the terminal in raw mode a
/// newline does not return the carriage, so the line ends with CR LF then.
struct Notices {
    raw: bool,
}

impl Notices {
    fn say(&self, msg: &str) {
        let end = if self.raw { "\r\n" } else { "\n" };
        let _ = io::Write::write_all(&mut io::stderr(), format!("xcomm: {msg}{end}").as_bytes());
    }
}

/// Errors that should exit with status 2 (the port cannot be had) rather
/// than 1.
pub enum Failure {
    Busy(String),
    Other(String),
}

impl From<SessionError> for Failure {
    fn from(e: SessionError) -> Failure {
        Failure::Other(e.0)
    }
}

/// Runs an interactive session on the process's own stdin and stdout.
pub fn run(opts: &TermOpts, env: &dyn Fn(&str) -> Option<String>) -> Result<ExitReason, Failure> {
    let path = device_path(&opts.device);
    // Before the lock: a SIGINT or SIGHUP while taking it must not kill the
    // process with lock files half made.
    let signals =
        SignalPipe::install(&signals()).map_err(|e| Failure::Other(format!("signals: {e}")))?;
    let lock = if opts.no_lock {
        None
    } else {
        Some(take_lock(&path, env)?)
    };
    let (port, applied) = Port::open(&path, &opts.line, !opts.no_init).map_err(|e| match &e {
        port::PortError::Open { source, .. } if source.kind() == io::ErrorKind::ResourceBusy => {
            Failure::Busy(e.to_string())
        }
        _ => Failure::Other(e.to_string()),
    })?;
    if opts.low_latency {
        port.set_ftdi_latency_timer(1)
            .map_err(|e| Failure::Other(format!("--low-latency: {e}")))?;
    }
    let low_latency = port
        .set_async_low_latency()
        .map_err(|e| Failure::Other(format!("ASYNC_LOW_LATENCY: {e}")))?;

    let stdin = io::stdin();
    let stdout = io::stdout();
    let raw = if rustix::termios::isatty(stdin.as_fd()) {
        match enter_raw(stdin.as_fd(), &signals)? {
            Some(raw) => Some(raw),
            // Ended by a signal before the session started.
            None => {
                drop(port);
                if let Some((lock, _)) = lock
                    && let Err(e) = lock.release()
                {
                    Notices { raw: false }.say(&format!("could not remove the lock: {e}"));
                }
                return Ok(ExitReason::Requested);
            }
        }
    } else {
        None
    };
    let keys_on = raw.is_some();
    let notices = Notices { raw: keys_on };
    if let Some((_, cleared)) = &lock {
        match cleared {
            Some(Cleared::DeadPid(pid)) => {
                notices.say(&format!("removed a stale lock of process {pid}"))
            }
            Some(Cleared::Empty) => notices.say("removed an abandoned empty lock"),
            None => {}
        }
    }
    notices.say(&banner(&path, opts, &port, applied, low_latency, keys_on));

    let reason = {
        let pump_io = Pump {
            stdin: readable(stdin.as_fd()).then_some(stdin.as_fd()),
            stdout: stdout.as_fd(),
            device: port.fd(),
            signals: &signals,
            prefix: keys_on.then_some((opts.prefix.key, opts.prefix_time)),
        };
        let stderr = io::stderr();
        let mut live = commands::Live::new(
            raw.as_ref(),
            &port,
            opts,
            stdout.as_fd(),
            stderr.as_fd(),
            &signals,
        );
        let reason = pump(&pump_io, &mut live);
        live.finish();
        reason
    };
    drop(raw);
    let notices = Notices { raw: false };
    // Errors are reported once, by the caller.
    if let Ok(ExitReason::DeviceGone) = &reason {
        notices.say(&format!("{} disconnected", path.display()));
    }
    drop(port);
    if let Some((lock, _)) = lock
        && let Err(e) = lock.release()
    {
        notices.say(&format!("could not remove the lock: {e}"));
    }
    drop(signals);
    Ok(reason?)
}

/// Enters raw mode. Started in the background, xcomm is stopped there by
/// SIGTTOU until brought to the foreground; a termination signal while
/// stopped (`kill %1` sends SIGTERM and SIGCONT) interrupts the call and
/// ends the session instead (None). SIGTSTP is honoured the same way.
fn enter_raw(fd: BorrowedFd<'_>, signals: &SignalPipe) -> Result<Option<RawMode>, Failure> {
    loop {
        match RawMode::enter(fd) {
            Ok(raw) => return Ok(Some(raw)),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {
                let bits = signals.drain().map_err(|e| Failure::Other(e.to_string()))?;
                if terminating(bits).is_some() {
                    return Ok(None);
                }
                if has(bits, libc::SIGTSTP) {
                    // SAFETY: SIGSTOP stops this process until SIGCONT.
                    unsafe { libc::raise(libc::SIGSTOP) };
                }
            }
            Err(e) => return Err(Failure::Other(e.to_string())),
        }
    }
}

fn take_lock(
    path: &Path,
    env: &dyn Fn(&str) -> Option<String>,
) -> Result<(UucpLock, Option<Cleared>), Failure> {
    let real = std::fs::canonicalize(path)
        .map_err(|e| Failure::Other(format!("cannot open {}: {e}", path.display())))?;
    let name = lock::lock_name(&real)
        .ok_or_else(|| Failure::Other(format!("cannot name a lock for {}", real.display())))?;
    let dir = lock::default_lock_dir(env).map_err(|e| Failure::Other(e.to_string()))?;
    lock::acquire(&dir, &name).map_err(|e| match e {
        lock::LockError::Held { .. } | lock::LockError::Contended { .. } => {
            Failure::Busy(e.to_string())
        }
        _ => Failure::Other(e.to_string()),
    })
}

fn banner(
    path: &Path,
    opts: &TermOpts,
    port: &Port,
    applied: Applied,
    low: LowLatency,
    keys_on: bool,
) -> String {
    let mut s = format!(
        "{} {} {}, pid {}",
        env!("CARGO_PKG_VERSION"),
        path.display(),
        port::describe(&port.settings()),
        std::process::id()
    );
    if let Applied::Different(actual) = applied {
        s.push_str(&format!(
            " (requested {}; the driver applied {})",
            port::describe(&opts.line),
            port::describe(&actual)
        ));
    }
    if low == LowLatency::Set {
        s.push_str(", low latency");
    }
    if keys_on {
        s.push_str(&format!(
            "; {} Z for help ({})",
            opts.prefix.key.display(),
            opts.prefix.source_text()
        ));
    }
    s
}
