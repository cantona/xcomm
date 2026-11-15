//! The control protocol: one JSON object per line each way. Bytes inside
//! JSON are base64 (RFC 4648, with padding).

use serde::{Deserialize, Serialize};

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
}

fn read_max() -> u64 {
    READ_MAX
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SettingsView {
    pub baud: u32,
    pub data_bits: u8,
    pub parity: String,
    pub stop_bits: u8,
    pub flow: String,
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
