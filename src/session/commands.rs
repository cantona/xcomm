//! What the keys typed after the prefix do.

use std::io;
use std::os::fd::BorrowedFd;
use std::time::{Duration, Instant};

use rustix::event::{PollFd, PollFlags, poll};

use super::{CommandIo, CommandOutcome, Control, ExitReason, SessionError, Signaled, on_signals};
use crate::cli::{LineSettings, PrefixChoice, TermOpts};
use crate::hosttty::{RawMode, SignalPipe};
use crate::keys::{self, EscKey, HELD_MAX};
use crate::port::{self, Applied, ModemLines, Port};

const ESC: u8 = 0x1b;
const BREAK_MS: u32 = 250;
const HANGUP: Duration = Duration::from_millis(500);
/// Starts what xcomm writes to a terminal: CAN ends an escape sequence the
/// device left unfinished, which would otherwise take xcomm's bytes as its
/// own. The rest of the device's sequence then shows as text.
const CANCEL: &str = "\x18";
const WRAP_ON: &[u8] = b"\x18\x1b[?7h";
const WRAP_OFF: &[u8] = b"\x18\x1b[?7l";
/// Longest line the prompt takes.
const PROMPT_MAX: usize = 32;

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
    Ignore,
    Exit(ExitReason),
}

enum Prompt {
    Line(String),
    Cancelled,
    Exit(ExitReason),
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
    port: &'a Port,
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
}

impl<'a> Live<'a> {
    pub(super) fn new(
        raw: Option<&'a RawMode>,
        port: &'a Port,
        opts: &TermOpts,
        stdout: BorrowedFd<'a>,
        stderr: BorrowedFd<'a>,
        signals: &'a SignalPipe,
    ) -> Live<'a> {
        let live = Live {
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
            echo: false,
            wrap: true,
        };
        if opts.wrap {
            live.set_wrap(true);
        }
        live
    }

    /// Puts back what commands changed on the terminal.
    pub(super) fn finish(&self) {
        if !self.wrap {
            self.set_wrap_if_writable(true);
        }
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

    fn set_wrap(&self, on: bool) {
        if let Some(fd) = self.screen {
            self.write_to(fd, if on { WRAP_ON } else { WRAP_OFF });
        }
    }

    /// A status line, on a line of its own after whatever the device sent.
    fn say(&self, msg: &str) {
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
        for (letter, _, what) in COMMANDS {
            s.push_str(&format!("  {key} {}  {what}\r\n", *letter as char));
        }
        s.push_str(&format!(
            "  {key} {key}  send {key} itself\r\n  {key} Esc  cancel\r\nprefix: {}\r\n",
            self.prefix.describe()
        ));
        self.write(s.as_bytes());
    }

    fn hang_up(&self, io: &CommandIo<'_>) {
        match self.port.modem_lines() {
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
        if let Err(e) = self.port.set_dtr(false) {
            self.say(&format!("hang up: {e}"));
            return;
        }
        // A signal cuts the wait short, and stays pending for the loop.
        let signal_fd = io.signals.fd();
        let mut fds = [PollFd::new(&signal_fd, PollFlags::IN)];
        let wait = rustix::event::Timespec::try_from(HANGUP).expect("500 ms fits");
        let started = Instant::now();
        let cut_short = !matches!(poll(&mut fds, Some(&wait)), Ok(0));
        match self.port.set_dtr(true) {
            Ok(()) if cut_short => self.say(&format!(
                "hang up cut short by a signal: DTR dropped for {} ms",
                started.elapsed().as_millis()
            )),
            Ok(()) => self.say("hung up (DTR dropped for 500 ms)"),
            Err(e) => self.say(&format!("hang up: DTR is left off: {e}")),
        }
    }

    /// Flips a modem line and reports what the driver reads back.
    fn toggle(
        &self,
        name: &str,
        get: fn(&ModemLines) -> bool,
        set: fn(&Port, bool) -> io::Result<()>,
    ) {
        let result = self.port.modem_lines().and_then(|m| {
            let want = !get(&m);
            set(self.port, want)?;
            Ok((want, get(&self.port.modem_lines()?)))
        });
        match result {
            Ok((want, now)) if want == now => self.say(&format!("{name} {}", on_off(now))),
            Ok((_, now)) => self.say(&format!(
                "{name} is still {}: the driver did not change it",
                on_off(now)
            )),
            Err(e) => self.say(&format!("{name}: {e}")),
        }
    }

    fn change_baud(&mut self, io: &mut CommandIo<'_>) -> Result<CommandOutcome, SessionError> {
        let current = self.port.settings();
        let line = match self.prompt(&format!("baud [{}]: ", current.baud), io)? {
            Prompt::Line(line) => line,
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
        match self.port.configure(&wanted) {
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

    /// Reads a line of printable ASCII from the keyboard: Backspace edits,
    /// Enter finishes, Esc or Ctrl-C cancels. Device output waits meanwhile.
    fn prompt(&mut self, text: &str, io: &mut CommandIo<'_>) -> Result<Prompt, SessionError> {
        self.write(format!("{}\r\n{text}", self.ui_cancel).as_bytes());
        let mut line = String::new();
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
                        self.write(format!("\r\n{text}{line}").as_bytes());
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
                    EscInPrompt::Ignore => continue,
                    EscInPrompt::Exit(exit) => return Ok(Prompt::Exit(exit)),
                }
            } else {
                b
            };
            match b {
                b'\r' | b'\n' => {
                    self.write(b"\r\n");
                    return Ok(Prompt::Line(line));
                }
                0x03 | ESC => {
                    self.write(b"\r\n");
                    return Ok(Prompt::Cancelled);
                }
                0x7f | 0x08 => {
                    if line.pop().is_some() {
                        self.write(b"\x08 \x08");
                    }
                }
                0x20..=0x7e if line.len() < PROMPT_MAX => {
                    line.push(char::from(b));
                    self.write(&[b]);
                }
                _ => {}
            }
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
                return Ok(match keys::plain_byte(key) {
                    Some(b) => EscInPrompt::Byte(b),
                    None => EscInPrompt::Ignore,
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
        if !self.wrap {
            self.set_wrap_if_writable(true);
        }
        if let Some(raw) = self.raw {
            raw.suspend()?;
        }
        // SAFETY: raise(3) is safe to call; SIGSTOP stops this process here
        // and execution continues after SIGCONT.
        unsafe { libc::raise(libc::SIGSTOP) };
        Ok(())
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
        if key == ESC {
            return Ok(CommandOutcome::Continue);
        }
        let Some(&(_, cmd, _)) = COMMANDS.iter().find(|(k, ..)| *k == key) else {
            self.write(format!("{}\x07", self.ui_cancel).as_bytes());
            return Ok(CommandOutcome::Continue);
        };
        match cmd {
            Cmd::Help => self.help(),
            Cmd::Quit => return Ok(CommandOutcome::Exit(ExitReason::Quit)),
            Cmd::Break => match self.port.send_break(BREAK_MS) {
                Ok(()) => self.say("break sent"),
                Err(e) => self.say(&e.to_string()),
            },
            Cmd::HangUp => self.hang_up(io),
            Cmd::Dtr => self.toggle("DTR", |m| m.dtr, Port::set_dtr),
            Cmd::Rts => self.toggle("RTS", |m| m.rts, Port::set_rts),
            Cmd::Baud => return self.change_baud(io),
            Cmd::Echo => {
                self.echo = !self.echo;
                self.say(&format!("local echo {}", on_off(self.echo)));
            }
            Cmd::Wrap if self.screen.is_none() => {
                self.say("line wrap: the output is not a terminal");
            }
            Cmd::Wrap => {
                self.wrap = !self.wrap;
                self.set_wrap(self.wrap);
                self.say(&format!("line wrap {}", on_off(self.wrap)));
            }
        }
        Ok(CommandOutcome::Continue)
    }

    fn local_echo(&self) -> bool {
        self.echo
    }

    fn dropping_input(&mut self) {
        self.say(&format!(
            "the device has taken no input for {} s: typed keys are dropped until it does ({} x quits)",
            super::STALL.as_secs(),
            self.prefix.key.display()
        ));
    }
}
