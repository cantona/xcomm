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

use crate::ansi::{SGR_HELD_MAX, SgrFilter};
use crate::capture::Capture;
use crate::cli::{LineSettings, TermOpts};
use crate::hosttty::{RawMode, SignalPipe};
use crate::keys::{KeyAction, KeyParser};
use crate::lock::{self, Cleared, UucpLock};
use crate::port::{self, Applied, LowLatency, Port};
use crate::reconnect::{self, DeviceWatch, NodeId, Track};
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
    /// The capture file, while one is open: device output is copied to it.
    fn capture(&mut self) -> Option<&mut Capture> {
        None
    }
    /// A capture write failed: stop capturing, and say so.
    fn capture_failed(&mut self, _error: io::Error) {}
    /// The ring device output is kept in, by offset.
    fn ring(&mut self) -> Option<&mut crate::ring::Ring> {
        None
    }
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
    /// Remove color parameters from SGR sequences in device output (color
    /// off); attributes stay.
    pub strip_color: bool,
}

/// Room kept in the output queue for bytes the key parser holds, which it
/// releases in addition to the bytes of the next read.
const HELD_ROOM: usize = crate::keys::HELD_MAX;
/// How long the device may take nothing from a full queue before keyboard
/// input is read anyway, so that the commands (quit) still work. Keys that
/// do not fit are dropped then.
const STALL: Duration = Duration::from_secs(2);
/// Longest time device output waits in the capture buffer.
const CAPTURE_FLUSH: Duration = Duration::from_millis(50);
/// How long quitting waits for queued keys to reach the device.
const QUIT_DRAIN: Duration = Duration::from_secs(1);
/// A device that went away has its node removed at once (an unplug); one
/// still there this much later was hung up instead.
const HANGUP_GRACE: Duration = Duration::from_secs(1);
const HANGUP_TICK: Duration = Duration::from_millis(20);
/// How often a device another program holds is tried again.
const BUSY_RETRY: Duration = Duration::from_secs(1);

const BUF_SIZE: usize = 64 * 1024;

const TERMINATE: [i32; 3] = [libc::SIGTERM, libc::SIGINT, libc::SIGHUP];

/// What the keys typed after `key` do, one line each, for `--help`.
pub fn commands(key: &str) -> String {
    commands::listed(key, "\n")
}

/// The signals the session takes on its pipe. SIGTTIN and SIGTTOU keep
/// their default action: a session in the background stops when it touches
/// the terminal, until brought back to the foreground. A signal the parent
/// set to be ignored stays ignored (`nohup` ignores SIGHUP; a script's
/// background job ignores SIGINT). SIGCHLD, which ends a file transfer's
/// wait, is taken even then: ignored, the kernel would reap the transfer
/// program itself and its exit status would be lost.
pub fn signals() -> Vec<i32> {
    [TERMINATE[0], TERMINATE[1], TERMINATE[2], libc::SIGTSTP]
        .into_iter()
        .filter(|&sig| !crate::hosttty::ignored(sig))
        .chain([libc::SIGCHLD])
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
    let mut keys = p.prefix.map(|(key, time)| KeyParser::new(key, time));
    pump_with(p, &mut keys, ctl)
}

/// [`pump`] with the key parser given, so that a prefix typed just before
/// the device goes away (or comes back) still takes the next key.
fn pump_with(
    p: &Pump<'_>,
    keys: &mut Option<KeyParser>,
    ctl: &mut dyn Control,
) -> Result<ExitReason, SessionError> {
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
    // Only in the path when color is off. An unfinished sequence it holds
    // when the session ends is dropped: never text, only a cut-off escape.
    let mut sgr = p.strip_color.then(SgrFilter::new);
    let mut filtered = Vec::with_capacity(if sgr.is_some() {
        2 * BUF_SIZE + SGR_HELD_MAX
    } else {
        0
    });
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
    // Since when the capture buffer has held bytes: it is written out
    // CAPTURE_FLUSH later, even while output keeps coming. While a write to
    // the terminal blocks, the capture waits too: a file write per device
    // read is what honouring the deadline there would cost.
    let mut capture_since: Option<Instant> = None;
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
        let capture_due = capture_since.map(|t| t + CAPTURE_FLUSH);
        let due = [held_due, stall_due, capture_due]
            .into_iter()
            .flatten()
            .min();
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
        if capture_due.is_some_and(|due| Instant::now() >= due) {
            capture_since = None;
            match ctl.capture().map(|c| c.flush().map(|()| c.is_dirty())) {
                Some(Err(e)) => ctl.capture_failed(e),
                // A pipe reader behind: try again a while later.
                Some(Ok(true)) => capture_since = Some(Instant::now()),
                _ => {}
            }
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
                Ok(n) => {
                    // The capture gets the device's bytes as they came.
                    match ctl
                        .capture()
                        .map(|c| c.write(&buf[..n]).map(|()| c.is_dirty()))
                    {
                        Some(Err(e)) => ctl.capture_failed(e),
                        Some(Ok(true)) if capture_since.is_none() => {
                            capture_since = Some(Instant::now());
                        }
                        _ => {}
                    }
                    let shown = match sgr.as_mut() {
                        None => &buf[..n],
                        Some(f) => {
                            filtered.clear();
                            f.filter(&buf[..n], &mut filtered);
                            &filtered
                        }
                    };
                    let shown = write_out(p.stdout, shown, p, ctl);
                    // After the terminal has them, whatever the write did,
                    // as the capture has them: raw, as read.
                    if let Some(ring) = ctl.ring() {
                        ring.push(&buf[..n]);
                    }
                    match shown? {
                        Signaled::Exit(exit) => return Ok(exit),
                        Signaled::Resumed => continue,
                        Signaled::Nothing => {}
                    }
                }
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

/// The identity of `path` when it is a symbolic link.
fn through_link(path: &Path) -> Option<NodeId> {
    std::fs::symlink_metadata(path)
        .is_ok_and(|m| m.file_type().is_symlink())
        .then(|| NodeId::link_at(path).ok())
        .flatten()
}

/// A UUCP lock held for the device, and the tty name it is for.
struct Held {
    lock: UucpLock,
    name: String,
}

/// Runs an interactive session on the process's own stdin and stdout.
pub fn run(opts: &TermOpts, env: &dyn Fn(&str) -> Option<String>) -> Result<ExitReason, Failure> {
    let path = device_path(&opts.device);
    // First: nothing to undo yet if the memory cannot be had.
    let ring = crate::ring::Ring::try_new(opts.ring_size)
        .map_err(|e| Failure::Other(format!("--ring-size {}: {e}", opts.ring_size)))?;
    // Before the lock: a SIGINT or SIGHUP while taking it must not kill the
    // process with lock files half made.
    let signals =
        SignalPipe::install(&signals()).map_err(|e| Failure::Other(format!("signals: {e}")))?;
    let (mut held, cleared) = if opts.no_lock {
        (None, None)
    } else {
        let real = std::fs::canonicalize(&path)
            .map_err(|e| Failure::Other(format!("cannot open {}: {e}", path.display())))?;
        let (held, cleared) = take_lock(&real, env).map_err(|e| match e {
            LockFailure::Busy(msg) => Failure::Busy(msg),
            LockFailure::Other(msg) => Failure::Other(msg),
        })?;
        (Some(held), cleared)
    };
    // Taken before the open: a link replaced after it is a change to see.
    let stable = reconnect::stable_path(&path);
    let mut resolved = std::fs::canonicalize(&stable).unwrap_or_else(|_| stable.clone());
    // The link the path went through, if it is one.
    let mut link = through_link(&stable);
    let (port, applied) = Port::open(&path, &opts.line, !opts.no_init).map_err(|e| match &e {
        port::PortError::Open { source, .. } if source.kind() == io::ErrorKind::ResourceBusy => {
            Failure::Busy(e.to_string())
        }
        _ => Failure::Other(e.to_string()),
    })?;
    if through_link(&stable) != link {
        return Err(Failure::Other(format!(
            "{} changed while being opened",
            stable.display()
        )));
    }
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
                if let Some(held) = held
                    && let Err(e) = held.lock.release()
                {
                    Notices { raw: false }.say(&format!("could not remove the lock: {e}"));
                }
                return Ok(ExitReason::Requested);
            }
        }
    } else {
        None
    };
    // Once the session is sure to start, so a run that ends before it (no
    // port, a signal while waiting for the terminal) leaves no file behind.
    let capture = opts
        .capture
        .as_deref()
        .map(Capture::start)
        .transpose()
        .map_err(|e| Failure::Other(format!("-C {e}")))?;
    let keys_on = raw.is_some();
    let notices = Notices { raw: keys_on };
    match cleared {
        Some(Cleared::DeadPid(pid)) => {
            notices.say(&format!("removed a stale lock of process {pid}"))
        }
        Some(Cleared::Empty) => notices.say("removed an abandoned empty lock"),
        None => {}
    }
    notices.say(&banner(&path, opts, &port, applied, low_latency, keys_on));
    if let Some(c) = &capture {
        notices.say(&format!("capturing to {}", c.path().display()));
    }

    let stderr = io::stderr();
    let stdin_fd = readable(stdin.as_fd()).then_some(stdin.as_fd());
    // Piped input read but not yet written is lost when the device goes
    // away, so going on after it comes back would leave a hole in the
    // stream: a disconnect ends such a session, as it always did.
    let piped = !keys_on && stdin_fd.is_some();
    let prefix = keys_on.then_some((opts.prefix.key, opts.prefix_time));
    let strip_color = !crate::ansi::device_color_enabled(
        opts.color,
        rustix::termios::isatty(stdout.as_fd()),
        env,
    );
    let mut kept = commands::Kept::new(capture, ring);
    let mut port_tty = port.tty_name().to_string();
    // Taken at the open: a descriptor revoked by a hangup cannot be asked
    // (macOS fstat fails on it).
    let mut node = NodeId::of(port.fd()).ok();
    let mut port = port;
    let mut first = true;
    let mut keys = prefix.map(|(key, time)| KeyParser::new(key, time));
    let (reason, port) = loop {
        let mut live = commands::Live::new(
            raw.as_ref(),
            Some(&port),
            opts,
            stdout.as_fd(),
            stderr.as_fd(),
            &signals,
            kept,
        );
        if std::mem::take(&mut first) && opts.wrap {
            live.set_wrap(true);
        }
        let pump_io = Pump {
            stdin: stdin_fd,
            stdout: stdout.as_fd(),
            device: port.fd(),
            signals: &signals,
            prefix,
            strip_color,
        };
        let reason = pump_with(&pump_io, &mut keys, &mut live);
        // A pty opened by its own name does not come back: its number goes
        // to the next terminal made.
        // XCOMM_TEST_PTY_AS_DEVICE (test-only) treats a pty as a device
        // that can come back, so tests can take the path of a real one.
        let pty = reconnect::is_pty(&resolved) && env("XCOMM_TEST_PTY_AS_DEVICE").is_none();
        if !matches!(reason, Ok(ExitReason::DeviceGone)) || piped || (pty && link.is_none()) {
            live.finish();
            break (reason, Some(port));
        }
        live.flush_capture();
        kept = live.into_kept();
        let settings = port.settings();
        let (track, gone) = if pty || reconnect::udev_link(&stable) {
            (Track::Link, link)
        } else {
            (Track::Node, node)
        };
        drop(port);
        let mut live = commands::Live::new(
            raw.as_ref(),
            None,
            opts,
            stdout.as_fd(),
            stderr.as_fd(),
            &signals,
            kept,
        );
        let notice = if keys_on {
            format!(
                "{} disconnected; waiting for it to come back ({} x quits)",
                path.display(),
                opts.prefix.key.display()
            )
        } else {
            format!(
                "{} disconnected; waiting for it to come back",
                path.display()
            )
        };
        // Waiting on a link (by-id, by-path, the user's own), the tty name
        // may go to another device meanwhile: its lock goes too, and the
        // name the device comes back under is locked then.
        if link.is_some()
            && let Some(h) = held.take()
            && let Err(e) = h.lock.release()
        {
            live.say(&format!("could not remove the lock: {e}"));
        }
        let old_tty = port_tty.clone();
        let mut watch = DeviceWatch::new(&stable, &resolved, gone, track);
        // A node that stays (a hangup, not an unplug) is not waited for:
        // that ends the session, as it always did.
        let hung_up = node
            .filter(|_| !pty)
            .map(|id| (Instant::now() + HANGUP_GRACE, id));
        let waited = wait_for_device(
            &mut live,
            &mut watch,
            &stable,
            &notice,
            hung_up,
            stdin_fd.filter(|_| keys_on),
            &mut keys,
            &signals,
            &mut || reopen(&stable, &settings, opts, &mut held, env),
        );
        match waited {
            Ok(Waited::Back(Reopened {
                port: new,
                applied,
                notes,
                resolved: new_resolved,
                link: new_link,
            })) => {
                let mut msg = format!("{} reconnected", path.display());
                if new.tty_name() != old_tty {
                    msg.push_str(&format!(" as {}", new.tty_name()));
                }
                if let Applied::Different(actual) = applied {
                    msg.push_str(&format!(
                        " (requested {}; the driver applied {})",
                        port::describe(&settings),
                        port::describe(&actual)
                    ));
                }
                live.say(&msg);
                for note in notes.iter().chain(&live.restore_lines(&new)) {
                    live.say(note);
                }
                kept = live.into_kept();
                resolved = new_resolved;
                link = new_link;
                port_tty = new.tty_name().to_string();
                node = NodeId::of(new.fd()).ok();
                port = new;
            }
            Ok(Waited::Exit(exit)) => {
                live.finish();
                break (Ok(exit), None);
            }
            Err(e) => {
                live.finish();
                break (Err(e), None);
            }
        }
    };
    drop(raw);
    let notices = Notices { raw: false };
    // Errors are reported once, by the caller.
    if let Ok(ExitReason::DeviceGone) = &reason {
        notices.say(&format!("{} disconnected", path.display()));
    }
    // Closed before the lock goes: a program that takes the lock must find
    // the port free.
    drop(port);
    if let Some(held) = held
        && let Err(e) = held.lock.release()
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

enum LockFailure {
    Busy(String),
    Other(String),
}

/// Locks the device that `real` (a resolved path) names.
fn take_lock(
    real: &Path,
    env: &dyn Fn(&str) -> Option<String>,
) -> Result<(Held, Option<Cleared>), LockFailure> {
    let name = lock::lock_name(real)
        .ok_or_else(|| LockFailure::Other(format!("cannot name a lock for {}", real.display())))?;
    let dir = lock::default_lock_dir(env).map_err(|e| LockFailure::Other(e.to_string()))?;
    let (lock, cleared) = lock::acquire(&dir, &name).map_err(|e| match e {
        lock::LockError::Held { .. } | lock::LockError::Contended { .. } => {
            LockFailure::Busy(e.to_string())
        }
        _ => LockFailure::Other(e.to_string()),
    })?;
    Ok((Held { lock, name }, cleared))
}

/// The device opened again.
struct Reopened {
    port: Port,
    applied: Applied,
    /// For the user.
    notes: Vec<String>,
    /// The resolved path and link identity, taken before the open.
    resolved: PathBuf,
    link: Option<NodeId>,
}

/// How the disconnected state ended.
enum Waited {
    /// The device is back.
    Back(Reopened),
    Exit(ExitReason),
}

enum ReopenError {
    /// Another program holds the device or its lock; it may let go.
    Busy(String),
    /// Anything else. A node just made can fail for a moment (udev has not
    /// given it its mode yet, the driver is still setting up), so it is
    /// tried again for a while.
    Failed(String),
}

/// Opens the device again with the settings it had. A device that comes
/// back under another tty name is locked under that name, and the old lock
/// released.
fn reopen(
    stable: &Path,
    settings: &LineSettings,
    opts: &TermOpts,
    held: &mut Option<Held>,
    env: &dyn Fn(&str) -> Option<String>,
) -> Result<Reopened, ReopenError> {
    let open_failed = |kind: io::ErrorKind, msg: String| match kind {
        io::ErrorKind::ResourceBusy => ReopenError::Busy(msg),
        _ => ReopenError::Failed(msg),
    };
    let mut notes = Vec::new();
    let mut new_lock = None;
    let link = through_link(stable);
    let real = std::fs::canonicalize(stable)
        .map_err(|e| open_failed(e.kind(), format!("cannot open {}: {e}", stable.display())))?;
    if !opts.no_lock {
        let name = lock::lock_name(&real);
        if held.as_ref().map(|h| h.name.as_str()) != name.as_deref() {
            let (lock, cleared) = take_lock(&real, env).map_err(|e| match e {
                LockFailure::Busy(msg) => ReopenError::Busy(msg),
                LockFailure::Other(msg) => ReopenError::Failed(msg),
            })?;
            match cleared {
                Some(Cleared::DeadPid(pid)) => {
                    notes.push(format!("removed a stale lock of process {pid}"))
                }
                Some(Cleared::Empty) => notes.push("removed an abandoned empty lock".to_string()),
                None => {}
            }
            new_lock = Some(lock);
        }
    }
    let (port, applied) = Port::open(stable, settings, !opts.no_init).map_err(|e| match &e {
        port::PortError::Open { source, .. } => open_failed(source.kind(), e.to_string()),
        _ => ReopenError::Failed(e.to_string()),
    })?;
    // The lock is for `real`: the link must not have moved on since.
    let same = |port: &Port| -> io::Result<bool> {
        Ok(rustix::fs::fstat(port.fd())?.st_rdev == rustix::fs::stat(&real)?.st_rdev)
    };
    if !same(&port).unwrap_or(false) || through_link(stable) != link {
        return Err(ReopenError::Failed(format!(
            "{} changed while being opened",
            stable.display()
        )));
    }
    if opts.low_latency
        && let Err(e) = port.set_ftdi_latency_timer(1)
    {
        notes.push(format!("--low-latency: {e}"));
    }
    if let Err(e) = port.set_async_low_latency() {
        notes.push(format!("ASYNC_LOW_LATENCY: {e}"));
    }
    if let Some(lock) = new_lock
        && let Some(old) = held.replace(lock)
        && let Err(e) = old.lock.release()
    {
        notes.push(format!("could not remove the lock: {e}"));
    }
    Ok(Reopened {
        port,
        applied,
        notes,
        resolved: real,
        link,
    })
}

/// The disconnected state: waits for the device to come back, or for the
/// session to end. Keys typed at a terminal are read: commands work, and
/// what would go to the device is dropped with a bell.
#[allow(clippy::too_many_arguments)]
fn wait_for_device(
    live: &mut commands::Live<'_>,
    watch: &mut DeviceWatch,
    path: &Path,
    notice: &str,
    mut hung_up: Option<(Instant, NodeId)>,
    stdin: Option<BorrowedFd<'_>>,
    keys: &mut Option<KeyParser>,
    signals: &SignalPipe,
    reopen: &mut dyn FnMut() -> Result<Reopened, ReopenError>,
) -> Result<Waited, SessionError> {
    let mut buf = [0u8; 256];
    let signal_fd = signals.fd();
    let mut look = true;
    // The last failure told, so that retries do not repeat it.
    let mut told: Option<String> = None;
    let mut tell = |live: &commands::Live<'_>, msg: String| {
        if told.as_ref() != Some(&msg) {
            live.say(&format!("{msg}; still waiting"));
            told = Some(msg);
        }
    };
    // Told once the node has gone; a node that stays was hung up, which
    // ends the session without a word of waiting.
    if hung_up.is_none() {
        live.say(notice);
    }
    loop {
        if let Some((due, gone)) = hung_up {
            if !NodeId::at(path).is_ok_and(|id| id.same_node(&gone)) {
                hung_up = None;
                live.say(notice);
            } else if Instant::now() >= due {
                return Ok(Waited::Exit(ExitReason::DeviceGone));
            }
        }
        if look
            && watch
                .check()
                .map_err(|e| SessionError(format!("watch: {e}")))?
        {
            match reopen() {
                Ok(back) => return Ok(Waited::Back(back)),
                Err(ReopenError::Failed(msg)) => {
                    if !watch.retry() {
                        tell(live, msg);
                    }
                }
                Err(ReopenError::Busy(msg)) => {
                    watch.retry_in(BUSY_RETRY);
                    tell(live, msg);
                }
            }
        }
        let held_due = keys.as_ref().and_then(|k| k.deadline());
        // Until the node is seen gone, it is looked at every HANGUP_TICK.
        let hung_up_due = hung_up.map(|(t, _)| t.min(Instant::now() + HANGUP_TICK));
        let due = [held_due, watch.deadline(), hung_up_due]
            .into_iter()
            .flatten()
            .min();
        let timeout = due.map(|due| {
            rustix::event::Timespec::try_from(due.saturating_duration_since(Instant::now()))
                .unwrap_or(rustix::event::Timespec {
                    tv_sec: 0,
                    tv_nsec: 0,
                })
        });
        let (sig_ready, watch_ready, stdin_ready) = {
            let (watch_slot, watch_events) = match watch.fd() {
                Some(fd) => (fd, PollFlags::IN),
                None => (signal_fd, PollFlags::empty()),
            };
            // Not while it may be a hangup: keys stay for whoever reads the
            // terminal after the session.
            let (stdin_slot, stdin_events) = match stdin {
                Some(fd) if hung_up.is_none() => (fd, PollFlags::IN),
                _ => (signal_fd, PollFlags::empty()),
            };
            let mut fds = [
                PollFd::new(&signal_fd, PollFlags::IN),
                PollFd::new(&watch_slot, watch_events),
                PollFd::new(&stdin_slot, stdin_events),
            ];
            match poll(&mut fds, timeout.as_ref()) {
                Ok(_) => {}
                Err(rustix::io::Errno::INTR) => continue,
                Err(e) => return Err(SessionError(format!("poll: {e}"))),
            }
            (
                fds[0].revents(),
                if watch_events.is_empty() {
                    PollFlags::empty()
                } else {
                    fds[1].revents()
                },
                if stdin_events.is_empty() {
                    PollFlags::empty()
                } else {
                    fds[2].revents()
                },
            )
        };
        look = watch_ready.contains(PollFlags::IN)
            || watch.deadline().is_some_and(|d| Instant::now() >= d);

        if sig_ready.contains(PollFlags::IN) {
            let bits = signals.drain().map_err(|e| SessionError(e.to_string()))?;
            match on_signals(bits, signals, live)? {
                Signaled::Exit(exit) => return Ok(Waited::Exit(exit)),
                Signaled::Resumed => continue,
                Signaled::Nothing => {}
            }
        }
        if let Some(k) = keys.as_mut().filter(|k| k.deadline().is_some()) {
            let mut dropped = false;
            k.on_deadline(Instant::now(), &mut |a| {
                dropped |= matches!(a, KeyAction::Forward(_));
            });
            if dropped {
                live.bell();
            }
        }
        let (Some(fd), Some(k)) = (stdin, keys.as_mut()) else {
            continue;
        };
        if !stdin_ready.intersects(PollFlags::IN | PollFlags::HUP | PollFlags::ERR) {
            continue;
        }
        match rustix::io::read(fd, &mut buf) {
            Ok(0) => return Ok(Waited::Exit(ExitReason::StdinClosed)),
            Ok(n) => {
                let mut rest = &buf[..n];
                let mut dropped = false;
                while !rest.is_empty() {
                    let mut command = None;
                    let used = k.feed(rest, Instant::now(), &mut |a| match a {
                        KeyAction::Forward(_) => dropped = true,
                        KeyAction::Command(c) => command = Some(c),
                    });
                    rest = &rest[used..];
                    let Some(c) = command else {
                        continue;
                    };
                    if std::mem::take(&mut dropped) {
                        live.bell();
                    }
                    let mut io = CommandIo {
                        stdin: fd,
                        signals,
                        typed: rest,
                    };
                    let outcome = live.command(c, &mut io)?;
                    rest = io.typed;
                    if let CommandOutcome::Exit(exit) = outcome {
                        return Ok(Waited::Exit(exit));
                    }
                }
                if dropped {
                    live.bell();
                }
            }
            Err(rustix::io::Errno::AGAIN | rustix::io::Errno::INTR) => {}
            Err(e) if gone(e) => return Ok(Waited::Exit(ExitReason::StdinClosed)),
            Err(e) => return Err(SessionError(format!("read from terminal: {e}"))),
        }
    }
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
