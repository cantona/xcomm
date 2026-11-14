//! The command prefix key (Ctrl-A, Alt-A, ...): splits keyboard input into
//! bytes for the device and the keys typed after the prefix. Runs on every
//! keystroke, so it scans with memchr and never allocates.

use std::time::{Duration, Instant};

use crate::cli::{Modifier, PrefixKey};

const ESC: u8 = 0x1b;
/// Most bytes held from one read to the next: an escape sequence cut off at
/// the end of a read. `feed` may forward this many bytes more than it was
/// given.
pub const HELD_MAX: usize = 16;
/// The command for a key that has no one-byte form (an arrow, a function
/// key).
pub const NO_KEY: u8 = 0;

/// Modifier bits of CSI-u and modifyOtherKeys sequences (whose parameter is
/// one more than the bits). Caps Lock and Num Lock are not modifiers here.
const MOD_ALT: u32 = 2;
const MOD_CTRL: u32 = 4;
const MOD_LOCKS: u32 = 64 | 128;
/// kitty event types (a sub-parameter of the modifiers).
const PRESS: u32 = 1;
const RELEASE: u32 = 3;

pub enum KeyAction<'a> {
    /// Bytes for the device.
    Forward(&'a [u8]),
    /// The key typed after the prefix: lowercased if a letter, ESC for a
    /// cancel, NO_KEY for a key with no one-byte form.
    Command(u8),
}

/// A key that starts with ESC.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EscKey {
    /// Cut off: more bytes may follow.
    Partial,
    /// `len` bytes long.
    Done { len: usize, key: Key },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key {
    /// ESC then ESC: the first is Esc on its own.
    Esc,
    /// ESC then a byte: that key with Alt.
    Alt(u8),
    /// A CSI-u (`ESC [ code ; mods[:event] u`) or modifyOtherKeys
    /// (`ESC [ 27 ; mods ; code ~`) key: its code, modifier bits and kitty
    /// event type (1 press, 2 repeat, 3 release).
    Coded { code: u32, mods: u32, event: u32 },
    /// Any other sequence (an arrow, a function key).
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Normal,
    AwaitCommand,
}

/// What one step of the scan did.
enum Step {
    /// Used this many bytes.
    Used(usize),
    /// Used this many bytes, the last of them ending a Command.
    Commanded(usize),
    /// The bytes are a sequence cut off at the end of the input.
    Hold,
}

fn number(digits: &[u8]) -> Option<u32> {
    if digits.is_empty() || digits.len() > 9 || !digits.iter().all(u8::is_ascii_digit) {
        return None;
    }
    Some(digits.iter().fold(0, |n, &d| n * 10 + u32::from(d - b'0')))
}

/// A parameter's number, without its `:` sub-parameters.
fn param(field: &[u8]) -> Option<u32> {
    number(field.split(|&c| c == b':').next()?)
}

/// Modifier bits and event type from a modifier parameter: none and a
/// press when it is absent.
fn mods_event(field: Option<&[u8]>) -> Option<(u32, u32)> {
    let Some(field) = field else {
        return Some((0, PRESS));
    };
    let mut sub = field.split(|&c| c == b':');
    let mods = number(sub.next()?)?.checked_sub(1)? & !MOD_LOCKS;
    let event = match sub.next() {
        Some(e) => number(e)?,
        None => PRESS,
    };
    Some((mods, event))
}

fn coded_key(params: &[u8], last: u8) -> Option<Key> {
    let mut fields = params.split(|&c| c == b';');
    let (code, (mods, event)) = match last {
        b'u' => (param(fields.next()?)?, mods_event(fields.next())?),
        b'~' if param(fields.next()?)? == 27 => {
            let m = mods_event(fields.next())?;
            (param(fields.next()?)?, m)
        }
        _ => return None,
    };
    Some(Key::Coded { code, mods, event })
}

/// The byte a terminal without a keyboard protocol sends for a coded key:
/// Ctrl-letter as the control byte. None past ASCII.
fn coded_byte(code: u32, mods: u32) -> Option<u8> {
    let c = u8::try_from(code).ok().filter(u8::is_ascii)?;
    Some(if mods & MOD_CTRL != 0 && c.is_ascii_alphabetic() {
        c & 0x1f
    } else {
        c
    })
}

/// A key starting `ESC [`.
fn csi_key(s: &[u8]) -> EscKey {
    // The Linux console's F1 to F5: ESC [ [ A to E.
    if s.get(2) == Some(&b'[') {
        return match s.get(3) {
            None => EscKey::Partial,
            Some(0x40..=0x7e) => EscKey::Done {
                len: 4,
                key: Key::Other,
            },
            // Not such a key: it ends before this byte.
            Some(_) => EscKey::Done {
                len: 3,
                key: Key::Other,
            },
        };
    }
    // Parameter and intermediate bytes, then a final byte.
    for (i, &c) in s.iter().enumerate().take(HELD_MAX).skip(2) {
        match c {
            0x20..=0x3f => {}
            0x40..=0x7e => {
                return EscKey::Done {
                    len: i + 1,
                    key: coded_key(&s[2..i], c).unwrap_or(Key::Other),
                };
            }
            // Not a CSI sequence: the key ends before this byte.
            _ => {
                return EscKey::Done {
                    len: i,
                    key: Key::Other,
                };
            }
        }
    }
    if s.len() < HELD_MAX {
        EscKey::Partial
    } else {
        EscKey::Done {
            len: HELD_MAX,
            key: Key::Other,
        }
    }
}

/// Splits off the key at the start of `s`, which starts with ESC.
pub fn escape_key(s: &[u8]) -> EscKey {
    let Some(&second) = s.get(1) else {
        return EscKey::Partial;
    };
    match second {
        b'[' => csi_key(s),
        b'O' => match s.get(2) {
            None => EscKey::Partial,
            Some(0x20..=0x7e) => EscKey::Done {
                len: 3,
                key: Key::Other,
            },
            // Not an SS3 key: Alt-Shift-O, then a key of its own.
            Some(_) => EscKey::Done {
                len: 2,
                key: Key::Alt(b'O'),
            },
        },
        ESC => EscKey::Done {
            len: 1,
            key: Key::Esc,
        },
        b => EscKey::Done {
            len: 2,
            key: Key::Alt(b),
        },
    }
}

/// The byte of a key pressed without Alt (Esc is ESC); None for an Alt key,
/// a key release, or a key with no byte. For reading a line, where keys
/// may arrive coded.
pub fn plain_byte(key: Key) -> Option<u8> {
    match key {
        Key::Esc => Some(ESC),
        Key::Coded { code, mods, event } if event != RELEASE && mods & MOD_ALT == 0 => {
            coded_byte(code, mods)
        }
        Key::Alt(_) | Key::Coded { .. } | Key::Other => None,
    }
}

pub struct KeyParser {
    modifier: Modifier,
    letter: u8,
    /// Ctrl prefix: the control byte. Alt prefix: unused.
    ctrl: u8,
    state: State,
    /// A sequence cut off at the end of a read, waiting for more.
    held: [u8; HELD_MAX],
    held_len: usize,
    held_since: Option<Instant>,
    prefix_time: Duration,
}

impl KeyParser {
    pub fn new(prefix: PrefixKey, prefix_time: Duration) -> KeyParser {
        KeyParser {
            modifier: prefix.modifier,
            letter: prefix.letter,
            ctrl: prefix.letter & 0x1f,
            state: State::Normal,
            held: [0; HELD_MAX],
            held_len: 0,
            held_since: None,
            prefix_time,
        }
    }

    /// When held bytes (a lone ESC, a cut-off sequence) must be given up on.
    pub fn deadline(&self) -> Option<Instant> {
        self.held_since.map(|t| t + self.prefix_time)
    }

    /// Whether an escape-sequence key is the prefix: ESC and the letter
    /// (Alt), or a coded form with the prefix's modifier alone.
    fn is_prefix(&self, key: Key) -> bool {
        let want = match self.modifier {
            Modifier::Alt => MOD_ALT,
            Modifier::Ctrl => MOD_CTRL,
        };
        match key {
            Key::Alt(b) => self.modifier == Modifier::Alt && b == self.letter,
            Key::Coded { code, mods, event } => {
                mods == want && code == u32::from(self.letter) && event != RELEASE
            }
            Key::Esc | Key::Other => false,
        }
    }

    fn send_prefix(&self, out: &mut dyn FnMut(KeyAction<'_>)) {
        match self.modifier {
            Modifier::Ctrl => out(KeyAction::Forward(&[self.ctrl])),
            Modifier::Alt => out(KeyAction::Forward(&[ESC, self.letter])),
        }
    }

    fn hold(&mut self, s: &[u8], now: Instant) {
        self.held[..s.len()].copy_from_slice(s);
        self.held_len = s.len();
        self.held_since = Some(now);
    }

    fn release(&mut self) {
        self.held_len = 0;
        self.held_since = None;
    }

    /// The key at the start of `s`, which starts with ESC. With a Ctrl
    /// prefix, ESC is a key of its own unless a CSI or SS3 sequence follows:
    /// Alt-letters are not looked for, and a lone ESC at the end of a read
    /// is not kept waiting (a terminal writes a sequence whole).
    fn escape(&self, s: &[u8]) -> EscKey {
        let k = escape_key(s);
        if self.modifier == Modifier::Alt {
            return k;
        }
        match k {
            EscKey::Partial if s.len() == 1 => EscKey::Done {
                len: 1,
                key: Key::Esc,
            },
            EscKey::Done {
                key: Key::Alt(_), ..
            } => EscKey::Done {
                len: 1,
                key: Key::Esc,
            },
            k => k,
        }
    }

    /// The key after the prefix, at the start of `s`.
    fn command(&mut self, s: &[u8], out: &mut dyn FnMut(KeyAction<'_>)) -> Step {
        // With a Ctrl prefix an Alt-letter is no command, and none of it
        // goes to the device. ESC and a control byte stay two keys: Esc
        // cancels, and the byte (the prefix, say) counts on its own.
        if self.modifier == Modifier::Ctrl
            && s[0] == ESC
            && let EscKey::Done {
                len: 2,
                key: Key::Alt(0x20..=0x7e),
            } = escape_key(s)
        {
            self.state = State::Normal;
            out(KeyAction::Command(NO_KEY));
            return Step::Commanded(2);
        }
        let (len, key) = if s[0] == ESC {
            match self.escape(s) {
                EscKey::Partial => return Step::Hold,
                EscKey::Done { len, key } => (len, Some(key)),
            }
        } else {
            (1, None)
        };
        // A key let go (the prefix's own, with kitty event reporting) is
        // not the key after the prefix.
        if let Some(Key::Coded { event: RELEASE, .. }) = key {
            return Step::Used(len);
        }
        self.state = State::Normal;
        let command = match key {
            None if self.modifier == Modifier::Ctrl && s[0] == self.ctrl => None,
            None => Some(s[0]),
            Some(k) if self.is_prefix(k) => None,
            Some(Key::Esc) => Some(ESC),
            Some(Key::Alt(b)) => Some(b),
            Some(Key::Coded { code, mods, .. }) => Some(coded_byte(code, mods).unwrap_or(NO_KEY)),
            Some(Key::Other) => Some(NO_KEY),
        };
        match command {
            Some(c) => {
                out(KeyAction::Command(c.to_ascii_lowercase()));
                Step::Commanded(len)
            }
            // The prefix again: send it.
            None => {
                self.send_prefix(out);
                Step::Used(len)
            }
        }
    }

    /// Handles what starts `s`, which runs to the end of the input.
    fn step(&mut self, s: &[u8], out: &mut dyn FnMut(KeyAction<'_>)) -> Step {
        if self.state == State::AwaitCommand {
            return self.command(s, out);
        }
        let found = match self.modifier {
            Modifier::Ctrl => memchr::memchr2(self.ctrl, ESC, s),
            Modifier::Alt => memchr::memchr(ESC, s),
        };
        match found {
            None => {
                out(KeyAction::Forward(s));
                return Step::Used(s.len());
            }
            Some(0) => {}
            Some(p) => {
                out(KeyAction::Forward(&s[..p]));
                return Step::Used(p);
            }
        }
        if s[0] != ESC {
            self.state = State::AwaitCommand;
            return Step::Used(1);
        }
        match self.escape(s) {
            EscKey::Partial => Step::Hold,
            EscKey::Done { len, key } if self.is_prefix(key) => {
                self.state = State::AwaitCommand;
                Step::Used(len)
            }
            // Another key: passed on whole.
            EscKey::Done { len, .. } => {
                out(KeyAction::Forward(&s[..len]));
                Step::Used(len)
            }
        }
    }

    /// Feeds one read. Stops after a Command, so the caller can act on it
    /// before the bytes that follow; returns how many bytes of `input` were
    /// used, which is all of them unless a Command was given.
    pub fn feed(
        &mut self,
        input: &[u8],
        now: Instant,
        out: &mut dyn FnMut(KeyAction<'_>),
    ) -> usize {
        let mut rest = input;
        if self.held_len > 0 {
            // The held bytes are scanned again with the start of this read
            // behind them.
            let held = self.held_len;
            let take = rest.len().min(HELD_MAX);
            let mut joined = [0u8; 2 * HELD_MAX];
            joined[..held].copy_from_slice(&self.held[..held]);
            joined[held..held + take].copy_from_slice(&rest[..take]);
            let joined = &joined[..held + take];
            self.release();
            let mut pos = 0;
            while pos < held {
                match self.step(&joined[pos..], out) {
                    Step::Used(n) => pos += n,
                    Step::Commanded(n) => {
                        pos += n;
                        if pos < held {
                            self.hold(&joined[pos..held], now);
                            return 0;
                        }
                        return pos - held;
                    }
                    // Only a tail shorter than HELD_MAX is held, and from
                    // before `held` that tail is all of the input.
                    Step::Hold => {
                        debug_assert_eq!(take, rest.len());
                        self.hold(&joined[pos..], now);
                        return input.len();
                    }
                }
            }
            rest = &rest[pos - held..];
        }
        while !rest.is_empty() {
            match self.step(rest, out) {
                Step::Used(n) => rest = &rest[n..],
                Step::Commanded(n) => return input.len() - rest.len() + n,
                Step::Hold => {
                    self.hold(rest, now);
                    break;
                }
            }
        }
        input.len()
    }

    /// Gives up on held bytes once the deadline has passed: they go to the
    /// device, or, after the prefix, the key is a cancel (ESC).
    pub fn on_deadline(&mut self, now: Instant, out: &mut dyn FnMut(KeyAction<'_>)) {
        let Some(due) = self.deadline() else {
            return;
        };
        if now < due {
            return;
        }
        let held = self.held;
        let len = self.held_len;
        self.release();
        if self.state == State::AwaitCommand {
            self.state = State::Normal;
            out(KeyAction::Command(ESC));
        } else {
            out(KeyAction::Forward(&held[..len]));
        }
    }
}
