//! The control interface: a socket per session that agents and scripts use
//! to read device output and send to the device (`xcomm ctl`).

pub mod client;
pub mod matcher;
pub mod path;
pub mod proto;
pub mod server;
pub mod wait;

/// The binary beside `xcomm` that runs `ctl wait` and `ctl run`.
pub const WAIT_HELPER: &str = "xcomm-wait";

/// Exit codes of `xcomm ctl` (spec 4.4), also the `code` of a reply.
pub mod code {
    pub const OK: u8 = 0;
    /// `wait`/`run` timed out; also any other runtime error.
    pub const FAILED: u8 = 1;
    pub const USAGE: u8 = 2;
    pub const NO_SESSION: u8 = 3;
    pub const DISCONNECTED: u8 = 4;
    pub const BUSY: u8 = 5;
    pub const EVICTED: u8 = 6;
    /// A `--fail-on` pattern of `wait`/`run` matched.
    pub const FAIL_ON: u8 = 7;
    pub const DENIED: u8 = 8;
    pub const DROPPED: u8 = 9;
}

/// What the control socket allows (`--ctl`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CtlMode {
    ReadWrite,
    /// Read-only commands only.
    ReadOnly,
    /// No socket.
    Off,
}

impl CtlMode {
    pub fn name(self) -> &'static str {
        match self {
            CtlMode::ReadWrite => "rw",
            CtlMode::ReadOnly => "ro",
            CtlMode::Off => "off",
        }
    }
}

#[derive(Debug)]
pub struct CtlError(pub String);

impl std::fmt::Display for CtlError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for CtlError {}
