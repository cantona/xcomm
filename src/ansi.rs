//! Escape sequences in device output: the SGR color filter used when color
//! is off (`--color never`, or `auto` with stdout not a terminal or
//! TERM=dumb), and the stripper that gives regex matching plain text. Both keep their
//! state across calls, so a sequence cut between two reads is still handled,
//! and neither allocates when `out` has the room each one documents.
//!
//! 8-bit C1 controls (0x9b for CSI, ...) are not recognised: in UTF-8 those
//! bytes are continuation bytes.

use crate::cli::ColorMode;

const ESC: u8 = 0x1b;
const BEL: u8 = 0x07;
/// CAN and SUB abort an escape sequence.
const CAN: u8 = 0x18;
const SUB: u8 = 0x1a;

const DEL: u8 = 0x7f;

/// A control a terminal carries out in the middle of an escape sequence,
/// which goes on after it. DEL is ignored there.
fn runs_inside_sequence(b: u8) -> bool {
    b < 0x20 && b != ESC && b != CAN && b != SUB
}

/// Longest CSI sequence the SGR filter holds back to decide on: room for
/// any SGR in practice (`ESC [ 0;1;3;4;38;2;255;255;255;48;2;255;255;255 m`
/// is 48 bytes). A longer one is passed on unfiltered.
pub const SGR_HELD_MAX: usize = 256;

/// Whether color in device output reaches the terminal. `auto`: stdout is
/// a terminal and TERM is not `dumb`. NO_COLOR does not apply: device output
/// is the board's data, not xcomm's own coloring.
pub fn device_color_enabled(
    mode: ColorMode,
    stdout_is_tty: bool,
    env: &dyn Fn(&str) -> Option<String>,
) -> bool {
    match mode {
        ColorMode::Always => true,
        ColorMode::Never => false,
        ColorMode::Auto => stdout_is_tty && env("TERM").as_deref() != Some("dumb"),
    }
}

/// Whether xcomm's own messages may use color. `auto`: stdout is a terminal,
/// NO_COLOR is unset or empty (no-color.org), and TERM is not `dumb`.
pub fn color_enabled(
    mode: ColorMode,
    stdout_is_tty: bool,
    env: &dyn Fn(&str) -> Option<String>,
) -> bool {
    match mode {
        ColorMode::Always => true,
        ColorMode::Never => false,
        ColorMode::Auto => {
            stdout_is_tty
                && env("NO_COLOR").is_none_or(|v| v.is_empty())
                && env("TERM").as_deref() != Some("dumb")
        }
    }
}

/// The number before any `:` sub-parameter; empty is 0.
fn leading_number(field: &[u8]) -> u32 {
    field
        .iter()
        .take_while(|c| c.is_ascii_digit())
        .fold(0u32, |n, &d| {
            n.saturating_mul(10).saturating_add(u32::from(d - b'0'))
        })
}

/// SGR parameters that set a color: foreground, background, bright, and
/// underline color. 38, 48 and 58 take arguments.
fn is_color(n: u32) -> bool {
    matches!(n, 30..=39 | 40..=49 | 58 | 59 | 90..=97 | 100..=107)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Sgr {
    Ground,
    /// An ESC, held back.
    Esc,
    /// ESC and intermediate bytes (`ESC ( B`), passed on.
    EscIntermediate,
    /// SS2 or SS3 (`ESC N`, `ESC O`), passed on: one more byte.
    OneMore,
    /// A CSI sequence that can still be SGR, held back.
    Csi,
    /// A CSI sequence that is not SGR, or too long to hold: passed on.
    CsiPass,
    /// OSC, ended by BEL or ST; passed on.
    Osc,
    /// DCS, SOS, PM or APC, ended by ST; passed on.
    Str,
    /// An ESC inside OSC or a string, held back: ST if `\` follows.
    OscEsc,
    StrEsc,
}

/// Removes the color parameters from SGR sequences (`ESC [ <digits ; :> m`)
/// and keeps the attributes (bold, reverse, underline, resets), so menu
/// highlights survive with color off; an SGR left empty is removed.
/// Everything else, cursor movement and private modes included, passes
/// unchanged.
///
/// It follows every kind of escape sequence, so that removing one cannot
/// join the bytes around it into a different one. An ESC ends whatever
/// sequence was in progress; when the sequence such an ESC starts is a
/// removed SGR, CAN takes its place, so the terminal still ends the cut-off
/// sequence (a partial CSI, an unterminated OSC) instead of reading the
/// following text as part of it.
pub struct SgrFilter {
    state: Sgr,
    held: [u8; SGR_HELD_MAX],
    held_len: usize,
    /// The ESC that started the current sequence ended another one.
    cut_off: bool,
}

impl Default for SgrFilter {
    fn default() -> SgrFilter {
        SgrFilter::new()
    }
}

impl SgrFilter {
    pub fn new() -> SgrFilter {
        SgrFilter {
            state: Sgr::Ground,
            held: [0; SGR_HELD_MAX],
            held_len: 0,
            cut_off: false,
        }
    }

    fn flush_held(&mut self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.held[..self.held_len]);
        self.held_len = 0;
    }

    /// Writes the held SGR without its color parameters: unchanged when it
    /// has none (the kept fields joined again are the original bytes),
    /// nothing when it has only those (CAN if its ESC cut off a sequence).
    /// Never longer than the held bytes and `m`.
    fn emit_without_color(&self, out: &mut Vec<u8>) {
        let start = out.len();
        out.extend_from_slice(&[ESC, b'[']);
        let mut kept = 0usize;
        let mut fields = self.held[2..self.held_len].split(|&c| c == b';');
        while let Some(f) = fields.next() {
            let n = leading_number(f);
            if !is_color(n) {
                if kept > 0 {
                    out.push(b';');
                }
                out.extend_from_slice(f);
                kept += 1;
            } else if matches!(n, 38 | 48 | 58) && !f.contains(&b':') {
                // 38;5;N and 38;2;R;G;B carry their arguments as parameters
                // of their own (38:5:N keeps them in one).
                let more = match fields.next().map(leading_number) {
                    Some(5) => 1,
                    Some(2) => 3,
                    _ => 0,
                };
                for _ in 0..more {
                    fields.next();
                }
            }
        }
        if kept > 0 {
            out.push(b'm');
        } else {
            out.truncate(start);
            if self.cut_off {
                out.push(CAN);
            }
        }
    }

    /// An ESC that ends the sequence in progress: held, and remembered.
    fn esc_cutting_off(&mut self) {
        self.state = Sgr::Esc;
        self.cut_off = true;
    }

    /// A control the terminal carries out inside the held sequence, passed
    /// on ahead of it. If the ESC holding it cut a sequence off, that
    /// sequence is still open in the output, and an OSC or string there
    /// would swallow the control: CAN ends it first.
    fn control_ahead(&mut self, b: u8, out: &mut Vec<u8>) {
        if self.cut_off {
            out.push(CAN);
            self.cut_off = false;
        }
        out.push(b);
    }

    /// Appends the filtered `input` to `out`: at most what was held from the
    /// previous call (SGR_HELD_MAX bytes) plus `input` plus one CAN per ESC
    /// in `input`. With `2 * input.len() + SGR_HELD_MAX` spare capacity
    /// `out` does not reallocate.
    pub fn filter(&mut self, input: &[u8], out: &mut Vec<u8>) {
        let mut i = 0;
        while i < input.len() {
            let b = input[i];
            // Each arm consumes `b` unless it sets `again`: then `b` is looked
            // at afresh in the new state.
            let mut again = false;
            match self.state {
                Sgr::Ground => match memchr::memchr(ESC, &input[i..]) {
                    None => {
                        out.extend_from_slice(&input[i..]);
                        return;
                    }
                    Some(p) => {
                        out.extend_from_slice(&input[i..i + p]);
                        i += p + 1;
                        self.state = Sgr::Esc;
                        self.cut_off = false;
                        continue;
                    }
                },
                Sgr::Esc => match b {
                    b'[' => {
                        self.held[..2].copy_from_slice(&[ESC, b'[']);
                        self.held_len = 2;
                        self.state = Sgr::Csi;
                    }
                    // The held ESC is ended by this one.
                    ESC => {
                        out.push(ESC);
                        self.cut_off = true;
                    }
                    _ if runs_inside_sequence(b) => self.control_ahead(b, out),
                    DEL => {}
                    _ => {
                        out.extend_from_slice(&[ESC, b]);
                        self.state = match b {
                            b']' => Sgr::Osc,
                            b'P' | b'X' | b'^' | b'_' => Sgr::Str,
                            b'N' | b'O' => Sgr::OneMore,
                            0x20..=0x2f => Sgr::EscIntermediate,
                            _ => Sgr::Ground,
                        };
                    }
                },
                Sgr::EscIntermediate => match b {
                    0x20..=0x2f => out.push(b),
                    0x30..=0x7e => {
                        out.push(b);
                        self.state = Sgr::Ground;
                    }
                    ESC => self.esc_cutting_off(),
                    _ if runs_inside_sequence(b) || b == DEL => out.push(b),
                    _ => {
                        again = true;
                        self.state = Sgr::Ground;
                    }
                },
                Sgr::OneMore => match b {
                    0x20..=0x7e => {
                        out.push(b);
                        self.state = Sgr::Ground;
                    }
                    ESC => self.esc_cutting_off(),
                    _ if runs_inside_sequence(b) || b == DEL => out.push(b),
                    _ => {
                        again = true;
                        self.state = Sgr::Ground;
                    }
                },
                Sgr::Csi => match b {
                    b'0'..=b'9' | b';' | b':' if self.held_len < SGR_HELD_MAX => {
                        self.held[self.held_len] = b;
                        self.held_len += 1;
                    }
                    // Ignored inside a sequence by terminals.
                    DEL => {}
                    // Too long to hold, or a private marker or intermediate
                    // byte: not SGR.
                    0x20..=0x3f => {
                        self.flush_held(out);
                        again = true;
                        self.state = Sgr::CsiPass;
                    }
                    b'm' => {
                        self.emit_without_color(out);
                        self.held_len = 0;
                        self.state = Sgr::Ground;
                    }
                    0x40..=0x7e => {
                        self.flush_held(out);
                        out.push(b);
                        self.state = Sgr::Ground;
                    }
                    ESC => {
                        self.flush_held(out);
                        self.esc_cutting_off();
                    }
                    // Carried out at once by the terminal; the sequence goes
                    // on.
                    _ if runs_inside_sequence(b) => self.control_ahead(b, out),
                    // CAN, SUB or a byte past ASCII end it.
                    _ => {
                        self.flush_held(out);
                        again = true;
                        self.state = Sgr::Ground;
                    }
                },
                Sgr::CsiPass => match b {
                    0x20..=0x3f | DEL => out.push(b),
                    0x40..=0x7e => {
                        out.push(b);
                        self.state = Sgr::Ground;
                    }
                    ESC => self.esc_cutting_off(),
                    _ if runs_inside_sequence(b) => out.push(b),
                    _ => {
                        again = true;
                        self.state = Sgr::Ground;
                    }
                },
                Sgr::Osc | Sgr::Str => {
                    let rest = &input[i..];
                    let hit = if self.state == Sgr::Osc {
                        let end = memchr::memchr3(BEL, ESC, CAN, rest);
                        memchr::memchr(SUB, &rest[..end.unwrap_or(rest.len())]).or(end)
                    } else {
                        memchr::memchr3(ESC, CAN, SUB, rest)
                    };
                    let Some(p) = hit else {
                        out.extend_from_slice(rest);
                        return;
                    };
                    out.extend_from_slice(&rest[..p]);
                    i += p;
                    if input[i] == ESC {
                        self.state = if self.state == Sgr::Osc {
                            Sgr::OscEsc
                        } else {
                            Sgr::StrEsc
                        };
                    } else {
                        // BEL ends an OSC; CAN and SUB abort either.
                        out.push(input[i]);
                        self.state = Sgr::Ground;
                    }
                }
                Sgr::OscEsc | Sgr::StrEsc => {
                    if b == b'\\' {
                        out.extend_from_slice(&[ESC, b'\\']);
                        self.state = Sgr::Ground;
                    } else {
                        // The ESC ends the string and starts a sequence of
                        // its own.
                        self.esc_cutting_off();
                        again = true;
                    }
                }
            }
            if !again {
                i += 1;
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Strip {
    Ground,
    Esc,
    /// ESC and intermediate bytes (`ESC ( B`).
    EscIntermediate,
    Csi,
    /// OSC, ended by BEL or ST.
    Osc,
    OscEsc,
    /// DCS, SOS, PM or APC, ended by ST.
    Str,
    StrEsc,
    /// SS2 or SS3: one more byte.
    OneMore,
}

/// Removes every escape sequence: CSI, OSC (ended by BEL or ST), DCS, SOS,
/// PM and APC (ended by ST), SS2 and SS3 with their byte, and any other ESC
/// with its intermediate and final bytes.
pub struct EscapeStripper {
    state: Strip,
}

impl Default for EscapeStripper {
    fn default() -> EscapeStripper {
        EscapeStripper::new()
    }
}

impl EscapeStripper {
    pub fn new() -> EscapeStripper {
        EscapeStripper {
            state: Strip::Ground,
        }
    }

    /// Appends `input` without its escape sequences to `out`: at most
    /// `input.len()` bytes.
    pub fn strip(&mut self, input: &[u8], out: &mut Vec<u8>) {
        let mut i = 0;
        while i < input.len() {
            let b = input[i];
            // Each arm consumes `b` unless it sets `again`: then `b` is looked
            // at afresh in the new state.
            let mut again = false;
            let in_sequence = matches!(
                self.state,
                Strip::Esc | Strip::EscIntermediate | Strip::Csi | Strip::OneMore
            );
            if in_sequence && (runs_inside_sequence(b) || b == DEL) {
                // Carried out (DEL ignored) without ending the sequence.
                if b != DEL {
                    out.push(b);
                }
                i += 1;
                continue;
            }
            match self.state {
                Strip::Ground => match memchr::memchr(ESC, &input[i..]) {
                    None => {
                        out.extend_from_slice(&input[i..]);
                        return;
                    }
                    Some(p) => {
                        out.extend_from_slice(&input[i..i + p]);
                        i += p + 1;
                        self.state = Strip::Esc;
                        continue;
                    }
                },
                Strip::Esc => {
                    self.state = match b {
                        b'[' => Strip::Csi,
                        b']' => Strip::Osc,
                        b'P' | b'X' | b'^' | b'_' => Strip::Str,
                        b'N' | b'O' => Strip::OneMore,
                        ESC => Strip::Esc,
                        0x20..=0x2f => Strip::EscIntermediate,
                        0x30..=0x7e | CAN | SUB => Strip::Ground,
                        _ => {
                            again = true;
                            Strip::Ground
                        }
                    }
                }
                Strip::EscIntermediate | Strip::Csi => {
                    let more = if self.state == Strip::Csi {
                        0x20..=0x3f
                    } else {
                        0x20..=0x2f
                    };
                    if !more.contains(&b) {
                        again = !(0x30..=0x7e).contains(&b) && b != CAN && b != SUB;
                        self.state = Strip::Ground;
                    }
                }
                Strip::Osc => {
                    let rest = &input[i..];
                    let end = memchr::memchr3(BEL, ESC, CAN, rest);
                    let sub = memchr::memchr(SUB, &rest[..end.unwrap_or(rest.len())]);
                    match sub.or(end) {
                        None => return,
                        Some(p) => {
                            i += p;
                            self.state = match input[i] {
                                ESC => Strip::OscEsc,
                                _ => Strip::Ground,
                            };
                        }
                    }
                }
                Strip::Str => match memchr::memchr3(ESC, CAN, SUB, &input[i..]) {
                    None => return,
                    Some(p) => {
                        i += p;
                        self.state = match input[i] {
                            ESC => Strip::StrEsc,
                            _ => Strip::Ground,
                        };
                    }
                },
                // ESC \ is ST; any other ESC ends the string and starts a
                // sequence of its own.
                Strip::OscEsc | Strip::StrEsc => {
                    if b == b'\\' {
                        self.state = Strip::Ground;
                    } else {
                        again = true;
                        self.state = Strip::Esc;
                    }
                }
                Strip::OneMore => {
                    again = !(0x20..=0x7e).contains(&b) && b != CAN && b != SUB;
                    self.state = Strip::Ground;
                }
            }
            if !again {
                i += 1;
            }
        }
    }
}
