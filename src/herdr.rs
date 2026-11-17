// SPDX-FileCopyrightText: 2026 Su Kang Yin <paradyse@gmail.com>
// SPDX-License-Identifier: GPL-2.0-only

//! The herdr API, spoken directly over `HERDR_SOCKET_PATH`: one JSON
//! request line per connection, one reply line (herdr 0.9.5).

use std::cell::Cell;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::time::Duration;

use serde_json::{Value, json};

/// How long one request may take: herdr answers these at once, and the
/// serial session waits meanwhile.
const TIMEOUT: Duration = Duration::from_millis(500);

/// The herdr server this process runs under, and its pane.
pub struct Herdr {
    socket: PathBuf,
    /// None for a client of the server alone: herdr starts popups without
    /// HERDR_PANE_ID.
    pane: Option<String>,
    next_id: Cell<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HerdrError {
    /// The socket could not be reached, or the reply did not come.
    Io(String),
    /// herdr answered with an error.
    Api { code: String, message: String },
    /// The reply was not one herdr sends.
    BadReply(String),
}

impl std::fmt::Display for HerdrError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            HerdrError::Io(e) => write!(f, "{e}"),
            HerdrError::Api { code, message } => write!(f, "{message} ({code})"),
            HerdrError::BadReply(e) => write!(f, "bad reply: {e}"),
        }
    }
}

impl std::error::Error for HerdrError {}

impl Herdr {
    /// Inside a herdr pane: both `HERDR_SOCKET_PATH` and `HERDR_PANE_ID`
    /// are set.
    pub fn from_env(env: &dyn Fn(&str) -> Option<String>) -> Option<Herdr> {
        let socket = env("HERDR_SOCKET_PATH").filter(|s| !s.is_empty())?;
        let pane = env("HERDR_PANE_ID").filter(|s| !s.is_empty())?;
        Some(Herdr {
            socket: PathBuf::from(socket),
            pane: Some(pane),
            next_id: Cell::new(1),
        })
    }

    /// A client of the herdr server alone (the popup a picker runs in has
    /// no pane id): `HERDR_SOCKET_PATH` is set.
    pub fn server_from_env(env: &dyn Fn(&str) -> Option<String>) -> Option<Herdr> {
        let socket = env("HERDR_SOCKET_PATH").filter(|s| !s.is_empty())?;
        Some(Herdr {
            socket: PathBuf::from(socket),
            pane: None,
            next_id: Cell::new(1),
        })
    }

    pub fn pane_id(&self) -> Option<&str> {
        self.pane.as_deref()
    }

    /// Sends `method` with `params` and returns the reply's `result`.
    pub fn request(&self, method: &str, params: Value) -> Result<Value, HerdrError> {
        let id = format!("xcomm:{}", self.next_id.get());
        self.next_id.set(self.next_id.get() + 1);
        let io = |what: &str, e: std::io::Error| {
            HerdrError::Io(format!("{what} {}: {e}", self.socket.display()))
        };
        // The whole request within TIMEOUT: a reply in pieces must not
        // stretch it.
        let deadline = std::time::Instant::now() + TIMEOUT;
        let left = || {
            deadline
                .checked_duration_since(std::time::Instant::now())
                .filter(|d| !d.is_zero())
                .ok_or_else(|| {
                    HerdrError::Io(format!("{method}: no reply within {TIMEOUT:?}"))
                })
        };
        let stream =
            crate::sock::connect_now(&self.socket).map_err(|e| io("connect", e))?;
        stream
            .set_write_timeout(Some(left()?))
            .map_err(|e| io("timeout on", e))?;
        let mut line =
            serde_json::to_vec(&json!({ "id": id, "method": method, "params": params }))
                .expect("a request serializes");
        line.push(b'\n');
        (&stream).write_all(&line).map_err(|e| io("write to", e))?;
        let mut reply = Vec::new();
        let mut buf = [0u8; 4096];
        while !reply.ends_with(b"\n") {
            stream
                .set_read_timeout(Some(left()?))
                .map_err(|e| io("timeout on", e))?;
            match (&stream).read(&mut buf) {
                Ok(0) => break,
                Ok(n) => reply.extend_from_slice(&buf[..n]),
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(e)
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) =>
                {
                    left()?;
                }
                Err(e) => return Err(io("read from", e)),
            }
        }
        if reply.is_empty() {
            return Err(HerdrError::Io(format!(
                "{} closed the connection",
                self.socket.display()
            )));
        }
        let v: Value = serde_json::from_slice(&reply)
            .map_err(|e| HerdrError::BadReply(format!("{method}: {e}")))?;
        if v["id"] != id.as_str() {
            return Err(HerdrError::BadReply(format!("{method}: id {}", v["id"])));
        }
        if let Some(e) = v.get("error") {
            return Err(HerdrError::Api {
                code: e["code"].as_str().unwrap_or("?").to_string(),
                message: e["message"].as_str().unwrap_or("?").to_string(),
            });
        }
        v.get("result")
            .cloned()
            .ok_or_else(|| HerdrError::BadReply(format!("{method}: no result")))
    }

    /// Reports this pane's title (None clears it) and metadata tokens (a
    /// None value clears one) as source `xcomm`; with `ttl`, herdr drops
    /// them after that unless reported again. The title shows ahead of the
    /// user's own name for the pane, which stays as it was.
    pub fn report_metadata(
        &self,
        title: Option<&str>,
        tokens: &[(&str, Option<&str>)],
        ttl: Option<Duration>,
    ) -> Result<(), HerdrError> {
        let Some(pane) = &self.pane else {
            return Err(HerdrError::Io("no herdr pane (HERDR_PANE_ID)".into()));
        };
        let mut params = json!({ "pane_id": pane, "source": "xcomm" });
        match title {
            Some(title) => params["title"] = title.into(),
            None => params["clear_title"] = true.into(),
        }
        if !tokens.is_empty() {
            let map: serde_json::Map<String, Value> = tokens
                .iter()
                .map(|(k, v)| (k.to_string(), v.map_or(Value::Null, Value::from)))
                .collect();
            params["tokens"] = Value::Object(map);
        }
        if let Some(ttl) = ttl {
            params["ttl_ms"] = u64::try_from(ttl.as_millis()).unwrap_or(u64::MAX).into();
        }
        self.request("pane.report_metadata", params).map(drop)
    }

    pub fn notify(&self, title: &str, body: Option<&str>) -> Result<(), HerdrError> {
        self.request("notification.show", json!({ "title": title, "body": body }))
            .map(drop)
    }

    pub fn focus_pane(&self, pane_id: &str) -> Result<(), HerdrError> {
        self.request("pane.focus", json!({ "pane_id": pane_id }))
            .map(drop)
    }
}
