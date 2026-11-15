//! The control protocol: one JSON object per line each way. Bytes inside
//! JSON are base64 (RFC 4648, with padding).

use serde::{Deserialize, Serialize};

use crate::cli::{DataBits, Flow, LineSettings, Parity, StopBits};

/// Longest request line, newline included. A client sends more than this
/// as several `send` requests.
pub const LINE_MAX: usize = 1 << 20;
/// Most bytes one `read` returns.
pub const READ_MAX: u64 = 1 << 20;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum Request {
    Status,
    /// Raw bytes for the device; the client has added any line ending.
    Send {
        data_b64: String,
    },
    /// From `since`, or the last `last` bytes, or (neither) the oldest held.
    Read {
        #[serde(default)]
        since: Option<u64>,
        #[serde(default)]
        last: Option<u64>,
        #[serde(default = "read_max")]
        max_bytes: u64,
        /// An evicted `since` starts at the oldest byte instead of failing.
        #[serde(default)]
        from_oldest: bool,
    },
    /// A reply line, then raw device bytes from `since` (default: from now
    /// on) until either side closes.
    Subscribe {
        #[serde(default)]
        since: Option<u64>,
        #[serde(default)]
        from_oldest: bool,
    },
    /// The requests below act on the line. Each waits for the bytes queued
    /// before it to be written to the device, so it comes after them.
    Break {
        #[serde(default = "break_ms")]
        ms: u32,
    },
    Modem {
        line: ModemLine,
        op: ModemOp,
        /// How long `pulse` holds the line at the other state.
        #[serde(default = "pulse_ms")]
        ms: u32,
    },
    Set {
        #[serde(flatten)]
        patch: SettingsPatch,
    },
    CaptureStart {
        path: String,
    },
    CaptureStop,
    /// Runs lrzsz on the port in `cwd`; the reply comes when it ends.
    /// Closing the connection aborts it.
    Xfer {
        dir: XferDir,
        proto: XferProto,
        #[serde(default)]
        files: Vec<String>,
        cwd: String,
    },
}

/// A request line: the request, and when the client stops waiting for
/// its reply (Unix time in ms). A request read after that is not run: the
/// client has said it failed. Without one a request has no deadline.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Envelope {
    #[serde(flatten)]
    pub request: Request,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deadline_ms: Option<u64>,
}

/// A subscriber that falls behind gets this line, with the offset it had
/// reached, as its last bytes; then the session closes it.
pub fn dropped_line(at: u64) -> String {
    format!("{DROPPED_HEAD}{at}{DROPPED_TAIL}")
}

const DROPPED_HEAD: &str = "\n{\"ok\":false,\"code\":9,\"error\":\"subscriber too slow at offset ";
const DROPPED_TAIL: &str = "\"}\n";

/// Where `bytes` ends with a `dropped_line`, and its offset.
pub fn dropped_at(bytes: &[u8]) -> Option<(usize, u64)> {
    let body = bytes.strip_suffix(DROPPED_TAIL.as_bytes())?;
    let digits = body.iter().rev().take_while(|b| b.is_ascii_digit()).count();
    let (head, at) = body.split_at(body.len() - digits);
    let line = head.strip_suffix(DROPPED_HEAD.as_bytes())?;
    let at = std::str::from_utf8(at).ok()?.parse().ok()?;
    Some((line.len(), at))
}

/// How many bytes at the end of `bytes` could be a `dropped_line` cut
/// short: 2 at least, as a lone newline is too common to hold back.
pub fn dropped_start(bytes: &[u8]) -> usize {
    let longest = DROPPED_HEAD.len() + 20 + DROPPED_TAIL.len();
    let look = bytes.len().saturating_sub(longest);
    memchr::memchr_iter(b'\n', &bytes[look..])
        .map(|p| &bytes[look + p..])
        .find(|t| t.len() >= 2 && starts_dropped(t))
        .map_or(0, <[u8]>::len)
}

/// Whether `t` is the start of a `dropped_line`, but not all of it.
fn starts_dropped(t: &[u8]) -> bool {
    let head = DROPPED_HEAD.as_bytes();
    let Some(rest) = t.strip_prefix(head) else {
        return head.starts_with(t);
    };
    let digits = rest.iter().take_while(|b| b.is_ascii_digit()).count();
    let after = &rest[digits..];
    after.len() < DROPPED_TAIL.len() && DROPPED_TAIL.as_bytes().starts_with(after)
}

/// Now as Unix time in ms, as `deadline_ms` counts.
pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

fn read_max() -> u64 {
    READ_MAX
}

/// A break's length when none is given, as the break key sends.
pub const BREAK_MS: u32 = 250;
/// Longest break or pulse.
pub const HOLD_MAX: u32 = 5000;
/// A pulse's length when none is given.
pub const PULSE_MS: u32 = 500;

fn break_ms() -> u32 {
    BREAK_MS
}

fn pulse_ms() -> u32 {
    PULSE_MS
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModemLine {
    Dtr,
    Rts,
}

impl ModemLine {
    pub fn name(self) -> &'static str {
        match self {
            ModemLine::Dtr => "DTR",
            ModemLine::Rts => "RTS",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModemOp {
    On,
    Off,
    Toggle,
    /// The other state for a while, then back.
    Pulse,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum XferDir {
    Send,
    Recv,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum XferProto {
    Zmodem,
    Ymodem,
    Xmodem,
}

/// Line settings to change; the rest stay. Values are named as in
/// [`SettingsView`] and the command line options.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SettingsPatch {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub baud: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data_bits: Option<u8>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parity: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_bits: Option<u8>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub flow: Option<String>,
}

impl SettingsPatch {
    pub fn is_empty(&self) -> bool {
        *self == SettingsPatch::default()
    }

    /// `base` with the changes; an error names a value out of range.
    pub fn apply(&self, base: LineSettings) -> Result<LineSettings, String> {
        let mut s = base;
        if let Some(baud) = self.baud {
            if baud == 0 {
                return Err("baud must be above 0".into());
            }
            s.baud = baud;
        }
        if let Some(bits) = self.data_bits {
            s.data_bits = match bits {
                5 => DataBits::Five,
                6 => DataBits::Six,
                7 => DataBits::Seven,
                8 => DataBits::Eight,
                other => return Err(format!("data bits {other}: 5, 6, 7 or 8")),
            };
        }
        if let Some(parity) = &self.parity {
            s.parity = match parity.as_str() {
                "none" => Parity::None,
                "even" => Parity::Even,
                "odd" => Parity::Odd,
                other => return Err(format!("parity '{other}': none, even or odd")),
            };
        }
        if let Some(bits) = self.stop_bits {
            s.stop_bits = match bits {
                1 => StopBits::One,
                2 => StopBits::Two,
                other => return Err(format!("stop bits {other}: 1 or 2")),
            };
        }
        if let Some(flow) = &self.flow {
            s.flow = match flow.as_str() {
                "none" => Flow::None,
                "rts" => Flow::RtsCts,
                "xon" => Flow::XonXoff,
                other => return Err(format!("flow '{other}': none, rts or xon")),
            };
        }
        Ok(s)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SettingsView {
    pub baud: u32,
    pub data_bits: u8,
    pub parity: String,
    pub stop_bits: u8,
    pub flow: String,
}

impl SettingsView {
    pub fn of(s: &LineSettings) -> SettingsView {
        SettingsView {
            baud: s.baud,
            data_bits: match s.data_bits {
                DataBits::Five => 5,
                DataBits::Six => 6,
                DataBits::Seven => 7,
                DataBits::Eight => 8,
            },
            parity: match s.parity {
                Parity::None => "none",
                Parity::Even => "even",
                Parity::Odd => "odd",
            }
            .into(),
            stop_bits: match s.stop_bits {
                StopBits::One => 1,
                StopBits::Two => 2,
            },
            flow: match s.flow {
                Flow::None => "none",
                Flow::RtsCts => "rts",
                Flow::XonXoff => "xon",
            }
            .into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModemView {
    pub dtr: bool,
    pub rts: bool,
    pub cts: bool,
    pub dsr: bool,
    pub dcd: bool,
    pub ri: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Status {
    /// The device as the session was started with it.
    pub device: String,
    /// The tty it is now (`ttyUSB1`, `pts_3`), which names the socket.
    pub tty: String,
    /// The name the session reopens the device by after a disconnect.
    pub stable: String,
    /// What the port has now; None while disconnected.
    pub settings: Option<SettingsView>,
    /// None while disconnected, or where the driver has no modem lines.
    pub modem: Option<ModemView>,
    pub connected: bool,
    /// Offset of the next byte from the device.
    pub offset: u64,
    /// Offset of the oldest byte `read` can still return.
    pub oldest: u64,
    pub capture: Option<String>,
    pub transfer: bool,
    pub ctl: String,
    pub herdr_pane: Option<String>,
    pub pid: u32,
}

/// A successful reply carrying `body`'s fields.
pub fn ok(body: serde_json::Value) -> Vec<u8> {
    let mut v = match body {
        serde_json::Value::Object(map) => map,
        serde_json::Value::Null => serde_json::Map::new(),
        other => {
            let mut map = serde_json::Map::new();
            map.insert("value".into(), other);
            map
        }
    };
    v.insert("ok".into(), true.into());
    v.insert("code".into(), 0.into());
    line(&serde_json::Value::Object(v))
}

/// A failed reply.
pub fn err(code: u8, msg: &str) -> Vec<u8> {
    line(&serde_json::json!({ "ok": false, "code": code, "error": msg }))
}

fn line(v: &serde_json::Value) -> Vec<u8> {
    let mut out = serde_json::to_vec(v).expect("a JSON value serializes");
    out.push(b'\n');
    out
}

const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

pub fn base64_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            chunk.get(1).copied().unwrap_or(0),
            chunk.get(2).copied().unwrap_or(0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(ALPHABET[(n >> (18 - 6 * i)) as usize & 63] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// Strict: padded to a multiple of four, standard alphabet, no whitespace,
/// and no bits set past the end.
pub fn base64_decode(text: &str) -> Option<Vec<u8>> {
    let s = text.as_bytes();
    if !s.len().is_multiple_of(4) {
        return None;
    }
    let value = |c: u8| -> Option<u32> {
        Some(match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => return None,
        } as u32)
    };
    let mut out = Vec::with_capacity(s.len() / 4 * 3);
    for (i, quad) in s.chunks(4).enumerate() {
        let last = i == s.len() / 4 - 1;
        let pad = quad.iter().rev().take_while(|&&c| c == b'=').count();
        if pad > 2 || (pad > 0 && !last) {
            return None;
        }
        let mut n = 0u32;
        for &c in &quad[..4 - pad] {
            n = (n << 6) | value(c)?;
        }
        n <<= 6 * pad as u32;
        let bytes = [(n >> 16) as u8, (n >> 8) as u8, n as u8];
        let keep = 3 - pad;
        // Bits past the data must be zero: one text per byte string.
        if bytes[keep..].iter().any(|&b| b != 0) {
            return None;
        }
        out.extend_from_slice(&bytes[..keep]);
    }
    Some(out)
}
