//! Command-line parsing and prefix key resolution.

use std::ffi::OsString;
use std::path::PathBuf;
use std::time::Duration;

use lexopt::prelude::*;

pub const USAGE: &str = "usage: xcomm [options] <device>";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CliError(pub String);

impl std::fmt::Display for CliError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<lexopt::Error> for CliError {
    fn from(e: lexopt::Error) -> CliError {
        CliError(e.to_string())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Modifier {
    Ctrl,
    Alt,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PrefixKey {
    pub modifier: Modifier,
    /// Lowercase ASCII letter.
    pub letter: u8,
}

impl PrefixKey {
    pub const CTRL_A: PrefixKey = PrefixKey {
        modifier: Modifier::Ctrl,
        letter: b'a',
    };
    pub const ALT_A: PrefixKey = PrefixKey {
        modifier: Modifier::Alt,
        letter: b'a',
    };

    pub fn parse(s: &str) -> Result<PrefixKey, CliError> {
        let lower = s.to_ascii_lowercase();
        let (modifier, rest) = if let Some(rest) = lower.strip_prefix("ctrl-") {
            (Modifier::Ctrl, rest)
        } else if let Some(rest) = lower.strip_prefix("alt-") {
            (Modifier::Alt, rest)
        } else {
            return Err(bad_prefix(s));
        };
        let letter = match rest.as_bytes() {
            [l] if l.is_ascii_lowercase() => *l,
            _ => return Err(bad_prefix(s)),
        };
        let clash = match (modifier, letter) {
            (Modifier::Ctrl, b'h') => Some("Backspace on many terminals"),
            (Modifier::Ctrl, b'i') => Some("Tab"),
            (Modifier::Ctrl, b'j') => Some("newline"),
            (Modifier::Ctrl, b'm') => Some("Enter"),
            _ => None,
        };
        if let Some(what) = clash {
            return Err(CliError(format!(
                "prefix key {lower} cannot be used: it is {what}"
            )));
        }
        Ok(PrefixKey { modifier, letter })
    }

    pub fn display(&self) -> String {
        let m = match self.modifier {
            Modifier::Ctrl => "Ctrl",
            Modifier::Alt => "Alt",
        };
        format!("{m}-{}", self.letter.to_ascii_uppercase() as char)
    }
}

fn bad_prefix(s: &str) -> CliError {
    CliError(format!(
        "invalid prefix key '{s}': expected ctrl-<letter> or alt-<letter>"
    ))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrefixSource {
    Flag,
    Env,
    Multiplexer(&'static str),
    Default,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PrefixChoice {
    pub key: PrefixKey,
    pub source: PrefixSource,
}

impl PrefixChoice {
    /// Why this key was chosen, for example `multiplexer: tmux`.
    pub fn source_text(&self) -> String {
        match self.source {
            PrefixSource::Flag => "command line".to_string(),
            PrefixSource::Env => "XCOMM_PREFIX".to_string(),
            PrefixSource::Multiplexer(name) => format!("multiplexer: {name}"),
            PrefixSource::Default => "default".to_string(),
        }
    }

    /// For example `Alt-A (multiplexer: tmux)`.
    pub fn describe(&self) -> String {
        format!("{} ({})", self.key.display(), self.source_text())
    }
}

const MULTIPLEXERS: [(&str, &str); 5] = [
    ("HERDR_ENV", "herdr"),
    ("HC_TERMINAL_ID", "Herdr app"),
    ("TMUX", "tmux"),
    ("STY", "screen"),
    ("ZELLIJ", "zellij"),
];

/// Flag first, then `XCOMM_PREFIX`, then Alt-A inside a terminal multiplexer
/// (whose own prefix is often Ctrl-A), else Ctrl-A. Empty variables count as
/// unset.
pub fn resolve_prefix(
    flag: Option<PrefixKey>,
    env: &dyn Fn(&str) -> Option<String>,
) -> Result<PrefixChoice, CliError> {
    let set = |name: &str| env(name).filter(|v| !v.is_empty());
    // Validated even when a flag overrides it, so a broken setting is reported
    // rather than hidden.
    let from_env = match set("XCOMM_PREFIX") {
        Some(v) => Some(PrefixKey::parse(&v).map_err(|e| CliError(format!("XCOMM_PREFIX: {e}")))?),
        None => None,
    };
    if let Some(key) = flag {
        return Ok(PrefixChoice {
            key,
            source: PrefixSource::Flag,
        });
    }
    if let Some(key) = from_env {
        return Ok(PrefixChoice {
            key,
            source: PrefixSource::Env,
        });
    }
    for (var, name) in MULTIPLEXERS {
        if set(var).is_some() {
            return Ok(PrefixChoice {
                key: PrefixKey::ALT_A,
                source: PrefixSource::Multiplexer(name),
            });
        }
    }
    Ok(PrefixChoice {
        key: PrefixKey::CTRL_A,
        source: PrefixSource::Default,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DataBits {
    Five,
    Six,
    Seven,
    Eight,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Parity {
    None,
    Even,
    Odd,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopBits {
    One,
    Two,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Flow {
    None,
    RtsCts,
    XonXoff,
}

/// `--color`: whether escape sequences that color text reach the terminal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColorMode {
    /// Device output: color when stdout is a terminal and TERM is not
    /// `dumb`. xcomm's own messages: also only while NO_COLOR is unset or
    /// empty.
    Auto,
    Always,
    Never,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LineSettings {
    pub baud: u32,
    pub data_bits: DataBits,
    pub parity: Parity,
    pub stop_bits: StopBits,
    pub flow: Flow,
}

impl Default for LineSettings {
    fn default() -> LineSettings {
        LineSettings {
            baud: 115_200,
            data_bits: DataBits::Eight,
            parity: Parity::None,
            stop_bits: StopBits::One,
            flow: Flow::None,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct TermOpts {
    pub device: String,
    pub line: LineSettings,
    /// `-o`: leave DTR and RTS alone at startup.
    pub no_init: bool,
    pub no_lock: bool,
    pub low_latency: bool,
    pub prefix: PrefixChoice,
    pub prefix_time: Duration,
    /// `-w`: turn the terminal's line wrap on at startup.
    pub wrap: bool,
    pub color: ColorMode,
    /// `-C`: capture device output to this file from the start.
    pub capture: Option<PathBuf>,
    /// `--ring-size`: how much device output is kept, in bytes.
    pub ring_size: usize,
    /// `--ctl`: what the control socket allows.
    pub ctl: crate::ctl::CtlMode,
    /// `--headless`: no terminal; the control socket is the only way in.
    pub headless: bool,
    /// Show what control clients send in the terminal (`--no-ctl-marker`
    /// turns it off).
    pub ctl_marker: bool,
}

/// What `xcomm ctl` sends.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SendData {
    Text(String),
    Hex(Vec<u8>),
    File(PathBuf),
}

/// What `xcomm ctl send` adds after the text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineEnding {
    None,
    Cr,
    Lf,
    CrLf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CtlCmd {
    Status,
    Sessions,
    Send {
        data: SendData,
        ending: LineEnding,
    },
    Read {
        since: Option<u64>,
        last: Option<u64>,
        max_bytes: u64,
        from_oldest: bool,
        color: ColorMode,
    },
    Break {
        ms: u32,
    },
    Modem {
        line: crate::ctl::proto::ModemLine,
        op: crate::ctl::proto::ModemOp,
        ms: u32,
    },
    Set {
        patch: crate::ctl::proto::SettingsPatch,
    },
    CaptureStart(PathBuf),
    CaptureStop,
    Xfer {
        dir: crate::ctl::proto::XferDir,
        proto: crate::ctl::proto::XferProto,
        files: Vec<PathBuf>,
    },
    /// Output from `since` (default: from now on) until a pattern matches.
    Wait {
        since: Option<u64>,
        opts: WaitOpts,
    },
    /// `text` sent, then a wait from the offset of the send.
    Run {
        text: String,
        ending: LineEnding,
        opts: WaitOpts,
        /// Most text kept for the output.
        max_bytes: u64,
    },
}

/// What `ctl wait` and `ctl run` wait for, and how long.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WaitOpts {
    pub until: Option<String>,
    pub fail_on: Vec<String>,
    /// None waits as long as it takes.
    pub timeout: Option<Duration>,
    /// Done once the device has sent nothing for this long.
    pub idle: Option<Duration>,
    /// Match escape sequences too.
    pub raw: bool,
    /// Unicode-aware `\w`, `\d`, `\s` and `(?i)` (the `unicode` feature).
    pub unicode: bool,
}

/// `--timeout` of `wait` and `run` when not given.
pub const WAIT_TIMEOUT: Duration = Duration::from_secs(10);
/// `--max-bytes` of `run` when not given.
pub const RUN_MAX_BYTES: u64 = 16 << 20;

/// `xcomm ctl [--json] <device> <command> ...` or `xcomm ctl sessions`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CtlArgs {
    pub device: Option<String>,
    pub cmd: CtlCmd,
    pub json: bool,
}

/// `--ring-size` when not given.
pub const DEFAULT_RING_SIZE: usize = 1 << 20;
const RING_MIN: u64 = 4 << 10;
const RING_MAX: u64 = 1 << 30;

#[derive(Debug, Clone, PartialEq)]
pub enum Command {
    /// `-h`/`--help`, with the prefix a session would use here, and why a
    /// session would not start if XCOMM_PREFIX is broken.
    Help(PrefixChoice, Option<String>),
    Version,
    ShowPrefix(PrefixChoice),
    Term(TermOpts),
    Ctl(CtlArgs),
    /// `xcomm list [--json]`.
    List {
        json: bool,
    },
    /// `xcomm pick`: the port picker of the herdr popup.
    Pick,
}

/// The text for `--help`. `commands` is what the keys after the prefix do,
/// as the in-session help lists them.
pub fn help(version: &str, prefix: &PrefixChoice, broken: Option<&str>, commands: &str) -> String {
    let key = prefix.key.display();
    let multiplexers = {
        let names: Vec<&str> = MULTIPLEXERS.iter().map(|(_, name)| *name).collect();
        match names.split_last() {
            Some((last, rest)) if !rest.is_empty() => format!("{} and {last}", rest.join(", ")),
            _ => names.concat(),
        }
    };
    let mut s = format!(
        "\
xcomm {version}: a serial terminal

Usage:
  xcomm [options] <device>
  xcomm --headless [options] <device>
  xcomm ctl [--json] <device> status|send|read|wait|run ...
  xcomm ctl [--json] sessions
  xcomm list [--json]
  xcomm pick
  xcomm --show-prefix [--prefix <key> | -m]
  xcomm -h | --help | -V | --version

<device> is a name in /dev (ttyUSB0) or, with a '/' in it, a path
(/dev/serial/by-id/...); -D <device> gives it as an option. Put -- before
a name that starts with '-' (or is ctl, list or pick).

xcomm list shows the serial ports: tty, USB VID:PID, product, the lock
holder, the herdr pane of an xcomm session on it, and the by-id name.
xcomm pick is that list for herdr's popup: Enter opens the port in a new
pane (or goes to the pane that has it), b sets the baud, o the placement.

Line settings:
  -b <baud>                  baud rate (default 115200)
  --databits 5|6|7|8         data bits (default 8)
  --parity none|even|odd     parity (default none)
  --stopbits 1|2             stop bits (default 1)
  --flow none|rts|xon        flow control: RTS/CTS or XON/XOFF (default none)

Port:
  -o                         leave DTR and RTS alone: xcomm does not raise
                             them (the kernel may, as the port opens) or
                             drop DTR on exit
  --no-lock                  take no UUCP lock file (the port is still
                             opened exclusively)
  --low-latency              FTDI adapters on Linux: 1 ms latency timer
                             (needs write access to its sysfs file)

Display:
  --color auto|always|never  colors in device output (default auto: kept
                             on a terminal, removed when piped or with
                             TERM=dumb)
  -c on|off                  the same as --color always|never
  -w                         turn line wrap on
  -C <file>                  append device output to <file>
  --ring-size <bytes>        device output kept in memory for the control
                             socket (4k to 1g, default 1m; k, m and g are
                             KiB, MiB and GiB)

Control socket (for scripts and agents, from another terminal):
  --ctl rw|ro|off            what the session's socket allows: everything,
                             status and read only, or no socket (default rw)
  --headless                 no terminal: device output goes only to the
                             socket's clients and -C; runs until SIGTERM,
                             SIGINT or SIGHUP (not with --ctl off)
  --no-ctl-marker            do not show what clients send as a dim
                             [ctl] line in the terminal
  xcomm ctl <device> status  port, settings, modem lines and offsets
  xcomm ctl <device> send <text>
                             send <text> and Enter (CR); -n for no line
                             ending, --lf or --crlf for another; --hex <hex>
                             or --file <path> send bytes as they are
  xcomm ctl <device> read    device output the session keeps (escape
                             sequences removed when piped, see --color);
                             --since <offset> or --last <bytes>,
                             --max-bytes <n>, --from-oldest; the next offset
                             goes to stderr as next=<offset>
  xcomm ctl <device> wait <regex>
                             wait for device output to match <regex>, from
                             --since <offset> or from now on, and print the
                             match; the offset after it goes to stderr as
                             end=<offset>. Escape sequences are removed
                             first unless --raw; a match spans at most
                             64 KiB, and one that reaches the end of the
                             output so far waits 50 ms for more (the rest
                             of a number; end the pattern with what comes
                             after it, as in 'inet ([0-9.]+)/', to be sure
                             of all of it). --timeout <s> (default 10, 0
                             for none) exits 1, --fail-on <regex>
                             (repeatable) exits 7 when it matches first,
                             --idle <ms> ends the wait (exit 0) once the
                             device is that quiet. \\w, \\d, \\s and (?i) are
                             ASCII; --unicode makes them Unicode-aware in
                             a build with --features unicode
  xcomm ctl <device> run <text> --until <regex>
                             send <text> as send does, then wait as wait
                             does from the offset of the send (or --idle
                             instead of --until); prints the output between
                             the echoed line and the match, at most
                             --max-bytes <n> (default 16 MiB)
  xcomm ctl <device> break [ms]
                             send a break (default 250 ms, at most 5000)
  xcomm ctl <device> dtr|rts on|off|toggle|pulse [ms]
                             set a modem line; pulse holds the other state
                             for ms (default 500), then puts it back
  xcomm ctl <device> set <name>=<value>...
                             change line settings: baud, databits, parity,
                             stopbits, flow (values as for the options)
  xcomm ctl <device> capture start <file> | capture stop
  xcomm ctl <device> xfer send|recv zmodem|ymodem|xmodem [<file>...]
                             run lrzsz on the session's port; the reply comes
                             when it ends, and Ctrl-C aborts it
  xcomm ctl sessions         the running sessions
  --json                     the reply as JSON

Keys:
  --prefix <key>             the command prefix: ctrl-<letter> (not h, i,
                             j or m) or alt-<letter> (also -e, --escape)
  -m, --metakey              the same as --prefix alt-a
  --prefix-time <ms>         how long a key cut short (Esc, Alt keys, key
                             sequences) waits for its rest (1 to 1000,
                             default 25)
  --show-prefix              print the prefix a session would use, and why

The prefix is the first of: --prefix; XCOMM_PREFIX (which must be valid
even when --prefix is given); Alt-A inside one of
{multiplexers}; Ctrl-A.
Here it is {described}.{broken}

Commands, typed after {key} (with a terminal on stdin; piped input goes
to the device as it is):
",
        described = prefix.describe(),
        broken = broken
            .map(|e| {
                let why = e.strip_prefix("XCOMM_PREFIX: ").unwrap_or(e);
                format!("\nA session would not start: XCOMM_PREFIX is not usable:\n  {why}")
            })
            .unwrap_or_default(),
    );
    s.push_str(commands);
    s.push_str(
        "\n\
Exit status: 0 after quit, the end of input, SIGTERM, SIGINT or SIGHUP, or
when the terminal goes away; 1 on an error or when the device goes away;
2 on a usage error or when the port is in use.\n",
    );
    s
}

fn text(value: OsString, name: &str) -> Result<String, CliError> {
    value.into_string().map_err(|v| {
        CliError(format!(
            "invalid value {} for {name}: not valid UTF-8",
            v.to_string_lossy()
        ))
    })
}

fn set_device(slot: &mut Option<String>, v: String) -> Result<(), CliError> {
    if v.is_empty() || v == "-" || v == "--" {
        return Err(CliError(format!("invalid device '{v}'")));
    }
    set_once(slot, v, "device")
}

fn set_once<T>(slot: &mut Option<T>, value: T, name: &str) -> Result<(), CliError> {
    if slot.is_some() {
        return Err(CliError(format!("{name} given more than once")));
    }
    *slot = Some(value);
    Ok(())
}

fn choose<T: Copy>(value: &str, name: &str, table: &[(&str, T)]) -> Result<T, CliError> {
    table
        .iter()
        .find(|(k, _)| *k == value)
        .map(|(_, v)| *v)
        .ok_or_else(|| {
            let allowed: Vec<&str> = table.iter().map(|(k, _)| *k).collect();
            CliError(format!(
                "invalid value '{value}' for {name}: expected one of {}",
                allowed.join(", ")
            ))
        })
}

fn digits(value: &str) -> bool {
    !value.is_empty() && value.bytes().all(|b| b.is_ascii_digit())
}

fn parse_baud(value: &str) -> Result<u32, CliError> {
    match value
        .parse::<u32>()
        .ok()
        .filter(|_| digits(value))
        .ok_or(())
    {
        Ok(b) if b > 0 => Ok(b),
        _ => Err(CliError(format!(
            "invalid value '{value}' for -b: expected a baud rate from 1 to {}",
            u32::MAX
        ))),
    }
}

/// Bytes, with an optional k, m or g (KiB, MiB, GiB).
fn parse_ring_size(value: &str) -> Result<usize, CliError> {
    let (number, unit) = match value.as_bytes().last() {
        Some(b'k' | b'K') => (&value[..value.len() - 1], 1u64 << 10),
        Some(b'm' | b'M') => (&value[..value.len() - 1], 1 << 20),
        Some(b'g' | b'G') => (&value[..value.len() - 1], 1 << 30),
        _ => (value, 1),
    };
    number
        .parse::<u64>()
        .ok()
        .filter(|_| digits(number))
        .and_then(|n| n.checked_mul(unit))
        .filter(|n| (RING_MIN..=RING_MAX).contains(n))
        .map(|n| n as usize)
        .ok_or_else(|| {
            CliError(format!(
                "invalid value '{value}' for --ring-size: expected 4k to 1g (bytes, \
                 with an optional k, m or g)"
            ))
        })
}

fn parse_offset(value: &str, name: &str) -> Result<u64, CliError> {
    value
        .parse::<u64>()
        .ok()
        .filter(|_| digits(value))
        .ok_or_else(|| {
            CliError(format!(
                "invalid value '{value}' for {name}: expected a number"
            ))
        })
}

fn parse_hex(value: &str) -> Result<Vec<u8>, CliError> {
    let digits: Vec<u8> = value
        .bytes()
        .filter(|b| !matches!(b, b' ' | b':'))
        .collect();
    let bad = || {
        CliError(format!(
            "invalid value '{value}' for --hex: expected hex bytes (0d0a, 0d 0a or 0d:0a)"
        ))
    };
    if digits.is_empty() || !digits.len().is_multiple_of(2) {
        return Err(bad());
    }
    digits
        .chunks(2)
        .map(|pair| {
            std::str::from_utf8(pair)
                .ok()
                .and_then(|p| u8::from_str_radix(p, 16).ok())
                .ok_or_else(bad)
        })
        .collect()
}

pub const CTL_USAGE: &str = "usage: xcomm ctl [--json] <device> \
     status|send|read|wait|run|break|dtr|rts|set|capture|xfer ..., \
     or xcomm ctl [--json] sessions";

fn parse_ctl(args: &[OsString]) -> Result<Command, CliError> {
    let mut p = lexopt::Parser::from_args(args.iter().cloned());
    let mut json = false;
    let mut words: Vec<String> = Vec::new();
    let mut ending: Option<LineEnding> = None;
    let mut hex: Option<Vec<u8>> = None;
    let mut file: Option<PathBuf> = None;
    let mut since: Option<u64> = None;
    let mut last: Option<u64> = None;
    let mut max_bytes: Option<u64> = None;
    let mut from_oldest = false;
    let mut color: Option<ColorMode> = None;
    let mut timeout: Option<Option<Duration>> = None;
    let mut idle: Option<Duration> = None;
    let mut until: Option<String> = None;
    let mut fail_on: Vec<String> = Vec::new();
    let mut raw = false;
    let mut unicode = false;
    // Each option given, and the commands it belongs to, for the check
    // below.
    const SEND: &[&str] = &["send"];
    const SEND_RUN: &[&str] = &["send", "run"];
    const READ: &[&str] = &["read"];
    const WAIT_RUN: &[&str] = &["wait", "run"];
    let mut given: Vec<(&str, &[&str])> = Vec::new();
    while let Some(arg) = p.next()? {
        match arg {
            Long("json") => json = true,
            Short('n') => {
                set_once(&mut ending, LineEnding::None, "a line ending")?;
                given.push(("-n", SEND_RUN));
            }
            Long("cr") => {
                set_once(&mut ending, LineEnding::Cr, "a line ending")?;
                given.push(("--cr", SEND_RUN));
            }
            Long("lf") => {
                set_once(&mut ending, LineEnding::Lf, "a line ending")?;
                given.push(("--lf", SEND_RUN));
            }
            Long("crlf") => {
                set_once(&mut ending, LineEnding::CrLf, "a line ending")?;
                given.push(("--crlf", SEND_RUN));
            }
            Long("hex") => {
                set_once(&mut hex, parse_hex(&text(p.value()?, "--hex")?)?, "--hex")?;
                given.push(("--hex", SEND));
            }
            Long("file") => {
                set_once(&mut file, PathBuf::from(p.value()?), "--file")?;
                given.push(("--file", SEND));
            }
            Long("since") => {
                let v = parse_offset(&text(p.value()?, "--since")?, "--since")?;
                set_once(&mut since, v, "--since")?;
                given.push(("--since", &["read", "wait"]));
            }
            Long("last") => {
                let v = parse_offset(&text(p.value()?, "--last")?, "--last")?;
                set_once(&mut last, v, "--last")?;
                given.push(("--last", READ));
            }
            Long("max-bytes") => {
                let v = parse_offset(&text(p.value()?, "--max-bytes")?, "--max-bytes")?;
                set_once(&mut max_bytes, v, "--max-bytes")?;
                given.push(("--max-bytes", &["read", "run"]));
            }
            Long("from-oldest") => {
                from_oldest = true;
                given.push(("--from-oldest", READ));
            }
            Long("color") => {
                let v = choose(
                    &text(p.value()?, "--color")?,
                    "--color",
                    &[
                        ("auto", ColorMode::Auto),
                        ("always", ColorMode::Always),
                        ("never", ColorMode::Never),
                    ],
                )?;
                set_once(&mut color, v, "--color")?;
                given.push(("--color", READ));
            }
            Long("timeout") => {
                let v = parse_timeout(&text(p.value()?, "--timeout")?)?;
                set_once(&mut timeout, v, "--timeout")?;
                given.push(("--timeout", WAIT_RUN));
            }
            Long("idle") => {
                let value = text(p.value()?, "--idle")?;
                let ms = parse_offset(&value, "--idle")
                    .ok()
                    .filter(|&ms| ms > 0)
                    .ok_or_else(|| {
                        CliError(format!(
                            "invalid value '{value}' for --idle: expected ms, at least 1"
                        ))
                    })?;
                set_once(&mut idle, Duration::from_millis(ms), "--idle")?;
                given.push(("--idle", WAIT_RUN));
            }
            Long("until") => {
                set_once(&mut until, text(p.value()?, "--until")?, "--until")?;
                given.push(("--until", &["run"]));
            }
            Long("fail-on") => {
                fail_on.push(text(p.value()?, "--fail-on")?);
                given.push(("--fail-on", WAIT_RUN));
            }
            Long("raw") => {
                raw = true;
                given.push(("--raw", WAIT_RUN));
            }
            Long("unicode") => {
                if !cfg!(feature = "unicode") {
                    return Err(CliError(
                        "--unicode: this xcomm was built without it \
                         (cargo build --features unicode)"
                            .into(),
                    ));
                }
                unicode = true;
                given.push(("--unicode", WAIT_RUN));
            }
            Value(v) => words.push(text(v, "argument")?),
            _ => return Err(arg.unexpected().into()),
        }
    }
    let usage = || CliError(CTL_USAGE.into());
    let (device, cmd, rest) = match words.as_slice() {
        [cmd, rest @ ..] if cmd == "sessions" => (None, "sessions", rest),
        [device, cmd, rest @ ..] => (Some(device.clone()), cmd.as_str(), rest),
        _ => return Err(usage()),
    };
    if let Some((o, cmds)) = given.iter().find(|(_, cmds)| !cmds.contains(&cmd)) {
        return Err(CliError(format!(
            "{o} is for ctl {}",
            cmds.join(" or ctl ")
        )));
    }
    let wait_opts = |until: Option<String>| WaitOpts {
        until,
        fail_on: fail_on.clone(),
        timeout: timeout.unwrap_or(Some(WAIT_TIMEOUT)),
        idle,
        raw,
        unicode,
    };
    let extra = |n: usize| -> Result<(), CliError> {
        match rest.get(n) {
            Some(word) => Err(CliError(format!("ctl {cmd}: unexpected argument '{word}'"))),
            None => Ok(()),
        }
    };
    let cmd = match cmd {
        "sessions" => {
            extra(0)?;
            CtlCmd::Sessions
        }
        "status" => {
            extra(0)?;
            CtlCmd::Status
        }
        "send" => {
            let data = match (rest.first(), hex, file) {
                (Some(t), None, None) => {
                    extra(1)?;
                    SendData::Text(t.clone())
                }
                (None, Some(h), None) => SendData::Hex(h),
                (None, None, Some(f)) => SendData::File(f),
                (None, None, None) => {
                    return Err(CliError(
                        "ctl send needs <text>, --hex <hex> or --file <path>".into(),
                    ));
                }
                _ => {
                    return Err(CliError(
                        "ctl send takes one of <text>, --hex and --file".into(),
                    ));
                }
            };
            // Raw bytes go as they are unless a line ending is asked for.
            let default = match data {
                SendData::Text(_) => LineEnding::Cr,
                _ => LineEnding::None,
            };
            CtlCmd::Send {
                data,
                ending: ending.unwrap_or(default),
            }
        }
        "read" => {
            extra(0)?;
            if since.is_some() && last.is_some() {
                return Err(CliError(
                    "ctl read takes --since or --last, not both".into(),
                ));
            }
            CtlCmd::Read {
                since,
                last,
                max_bytes: max_bytes.unwrap_or(crate::ctl::proto::READ_MAX),
                from_oldest,
                color: color.unwrap_or(ColorMode::Auto),
            }
        }
        "wait" => {
            extra(1)?;
            let pattern = rest.first().cloned();
            if pattern.is_none() && idle.is_none() {
                return Err(CliError("ctl wait needs <regex> or --idle <ms>".into()));
            }
            CtlCmd::Wait {
                since,
                opts: wait_opts(pattern),
            }
        }
        "run" => {
            let Some(text) = rest.first() else {
                return Err(CliError("ctl run needs <text>".into()));
            };
            extra(1)?;
            if until.is_none() && idle.is_none() {
                return Err(CliError(
                    "ctl run needs --until <regex> or --idle <ms>".into(),
                ));
            }
            CtlCmd::Run {
                text: text.clone(),
                ending: ending.unwrap_or(LineEnding::Cr),
                opts: wait_opts(until.clone()),
                max_bytes: max_bytes.unwrap_or(RUN_MAX_BYTES),
            }
        }
        "break" => {
            extra(1)?;
            let ms = match rest.first() {
                Some(v) => parse_hold(v, "break")?,
                None => crate::ctl::proto::BREAK_MS,
            };
            CtlCmd::Break { ms }
        }
        "dtr" | "rts" => {
            use crate::ctl::proto::{ModemLine, ModemOp};
            let line = if cmd == "dtr" {
                ModemLine::Dtr
            } else {
                ModemLine::Rts
            };
            let Some(op) = rest.first() else {
                return Err(CliError(format!(
                    "ctl {cmd} needs on, off, toggle or pulse"
                )));
            };
            let op = choose(
                op,
                &format!("ctl {cmd}"),
                &[
                    ("on", ModemOp::On),
                    ("off", ModemOp::Off),
                    ("toggle", ModemOp::Toggle),
                    ("pulse", ModemOp::Pulse),
                ],
            )?;
            let ms = match (op, rest.get(1)) {
                (ModemOp::Pulse, Some(v)) => {
                    extra(2)?;
                    parse_hold(v, "pulse")?
                }
                _ => {
                    extra(1)?;
                    crate::ctl::proto::PULSE_MS
                }
            };
            CtlCmd::Modem { line, op, ms }
        }
        "set" => CtlCmd::Set {
            patch: parse_settings_patch(rest)?,
        },
        "capture" => match rest.first().map(String::as_str) {
            Some("start") => {
                let Some(file) = rest.get(1) else {
                    return Err(CliError("ctl capture start needs a file".into()));
                };
                extra(2)?;
                CtlCmd::CaptureStart(PathBuf::from(file))
            }
            Some("stop") => {
                extra(1)?;
                CtlCmd::CaptureStop
            }
            _ => return Err(CliError("ctl capture needs start <file> or stop".into())),
        },
        "xfer" => {
            use crate::ctl::proto::{XferDir, XferProto};
            let (Some(dir), Some(proto)) = (rest.first(), rest.get(1)) else {
                return Err(CliError(
                    "ctl xfer needs send|recv and zmodem|ymodem|xmodem".into(),
                ));
            };
            let dir = choose(
                dir,
                "ctl xfer",
                &[("send", XferDir::Send), ("recv", XferDir::Recv)],
            )?;
            let proto = choose(
                proto,
                "ctl xfer",
                &[
                    ("zmodem", XferProto::Zmodem),
                    ("ymodem", XferProto::Ymodem),
                    ("xmodem", XferProto::Xmodem),
                ],
            )?;
            let files: Vec<PathBuf> = rest[2..].iter().map(PathBuf::from).collect();
            match (dir, proto) {
                (XferDir::Send, _) if files.is_empty() => {
                    return Err(CliError("ctl xfer send needs a file".into()));
                }
                (XferDir::Recv, XferProto::Xmodem) if files.len() != 1 => {
                    return Err(CliError("ctl xfer recv xmodem needs one file name".into()));
                }
                (XferDir::Recv, XferProto::Zmodem | XferProto::Ymodem) if !files.is_empty() => {
                    return Err(CliError(
                        "ctl xfer recv: the sender names the files; give none".into(),
                    ));
                }
                _ => {}
            }
            CtlCmd::Xfer { dir, proto, files }
        }
        other => {
            return Err(CliError(format!(
                "unknown ctl command '{other}'; {CTL_USAGE}"
            )));
        }
    };
    Ok(Command::Ctl(CtlArgs { device, cmd, json }))
}

/// Seconds, fractions allowed; 0 for none.
fn parse_timeout(value: &str) -> Result<Option<Duration>, CliError> {
    let secs = value
        .parse::<f64>()
        .ok()
        .filter(|_| value.bytes().all(|b| b.is_ascii_digit() || b == b'.'))
        .and_then(|s| Duration::try_from_secs_f64(s).ok())
        .ok_or_else(|| {
            CliError(format!(
                "invalid value '{value}' for --timeout: expected seconds (0 for none)"
            ))
        })?;
    Ok((!secs.is_zero()).then_some(secs))
}

fn parse_list(args: &[OsString]) -> Result<Command, CliError> {
    let mut p = lexopt::Parser::from_args(args.iter().cloned());
    let mut json = false;
    while let Some(arg) = p.next()? {
        match arg {
            Long("json") => json = true,
            Value(v) => {
                return Err(CliError(format!(
                    "list: unexpected argument '{}'",
                    v.to_string_lossy()
                )));
            }
            _ => return Err(arg.unexpected().into()),
        }
    }
    Ok(Command::List { json })
}

/// A break's or pulse's length in ms.
fn parse_hold(value: &str, what: &str) -> Result<u32, CliError> {
    let max = crate::ctl::proto::HOLD_MAX;
    value
        .parse::<u32>()
        .ok()
        .filter(|ms| digits(value) && (1..=max).contains(ms))
        .ok_or_else(|| CliError(format!("invalid {what} length '{value}': 1 to {max} ms")))
}

/// `baud=N databits=N parity=P stopbits=N flow=F`, named and valued as
/// the session's options are.
fn parse_settings_patch(words: &[String]) -> Result<crate::ctl::proto::SettingsPatch, CliError> {
    let mut patch = crate::ctl::proto::SettingsPatch::default();
    if words.is_empty() {
        return Err(CliError(
            "ctl set needs baud=, databits=, parity=, stopbits= or flow=".into(),
        ));
    }
    for word in words {
        let Some((key, value)) = word.split_once('=') else {
            return Err(CliError(format!("ctl set: '{word}' is not <name>=<value>")));
        };
        match key {
            "baud" => {
                let v = parse_baud(value)
                    .map_err(|_| CliError(format!("ctl set: invalid baud rate '{value}'")))?;
                set_once(&mut patch.baud, v, "baud=")?;
            }
            "databits" => {
                let v = choose(
                    value,
                    "databits=",
                    &[("5", 5), ("6", 6), ("7", 7), ("8", 8)],
                )?;
                set_once(&mut patch.data_bits, v, "databits=")?;
            }
            "parity" => {
                let v = choose(
                    value,
                    "parity=",
                    &[("none", "none"), ("even", "even"), ("odd", "odd")],
                )?;
                set_once(&mut patch.parity, v.to_string(), "parity=")?;
            }
            "stopbits" => {
                let v = choose(value, "stopbits=", &[("1", 1), ("2", 2)])?;
                set_once(&mut patch.stop_bits, v, "stopbits=")?;
            }
            "flow" => {
                let v = choose(
                    value,
                    "flow=",
                    &[("none", "none"), ("rts", "rts"), ("xon", "xon")],
                )?;
                set_once(&mut patch.flow, v.to_string(), "flow=")?;
            }
            other => {
                return Err(CliError(format!(
                    "ctl set: unknown setting '{other}' (baud, databits, parity, stopbits, flow)"
                )));
            }
        }
    }
    Ok(patch)
}

fn parse_prefix_time(value: &str) -> Result<Duration, CliError> {
    match value
        .parse::<u64>()
        .ok()
        .filter(|_| digits(value))
        .ok_or(())
    {
        Ok(ms @ 1..=1000) => Ok(Duration::from_millis(ms)),
        _ => Err(CliError(format!(
            "invalid value '{value}' for --prefix-time: expected 1 to 1000 (ms)"
        ))),
    }
}

pub fn parse(
    args: impl IntoIterator<Item = OsString>,
    env: &dyn Fn(&str) -> Option<String>,
) -> Result<Command, CliError> {
    let args: Vec<OsString> = args.into_iter().collect();
    if args.first().is_some_and(|a| a == "ctl") {
        return parse_ctl(&args[1..]);
    }
    if args.first().is_some_and(|a| a == "list") {
        return parse_list(&args[1..]);
    }
    if args.first().is_some_and(|a| a == "pick") {
        return match args.get(1) {
            None => Ok(Command::Pick),
            Some(a) => Err(CliError(format!(
                "pick: unexpected argument '{}'",
                a.to_string_lossy()
            ))),
        };
    }
    // lexopt consumes a bare `--` without returning it; count it here so
    // `--version --` is rejected like any other extra argument.
    let separator = args.iter().any(|a| a == "--");
    let mut p = lexopt::Parser::from_args(args);
    let mut device: Option<String> = None;
    let mut baud: Option<u32> = None;
    let mut data_bits: Option<DataBits> = None;
    let mut parity: Option<Parity> = None;
    let mut stop_bits: Option<StopBits> = None;
    let mut flow: Option<Flow> = None;
    let mut prefix: Option<PrefixKey> = None;
    let mut prefix_time: Option<Duration> = None;
    let mut no_init = false;
    let mut no_lock = false;
    let mut low_latency = false;
    let mut wrap = false;
    let mut color: Option<ColorMode> = None;
    let mut capture: Option<PathBuf> = None;
    let mut ring_size: Option<usize> = None;
    let mut ctl: Option<crate::ctl::CtlMode> = None;
    let mut headless = false;
    let mut ctl_marker = true;
    let mut show_prefix = false;
    let mut version = false;
    let mut help = false;
    // For --version (alone) and --show-prefix (prefix options only).
    let mut others = usize::from(separator);
    let mut non_prefix = usize::from(separator);

    while let Some(arg) = p.next()? {
        if !matches!(arg, Short('V') | Long("version")) || version {
            others += 1;
        }
        if !matches!(
            arg,
            Long("show-prefix")
                | Long("prefix")
                | Short('e')
                | Long("escape")
                | Short('m')
                | Long("metakey")
        ) {
            non_prefix += 1;
        }
        match arg {
            Short('D') => {
                let v = text(p.value()?, "-D")?;
                if v.starts_with('-') && v != "-" && v != "--" {
                    return Err(CliError(format!(
                        "-D needs a device, got '{v}' (use -- {v} for a device whose name starts with -)"
                    )));
                }
                if v.starts_with('-') {
                    return Err(CliError(format!("-D needs a device, got '{v}'")));
                }
                set_device(&mut device, v)?;
            }
            Value(v) => set_device(&mut device, text(v, "device")?)?,
            Short('b') => set_once(&mut baud, parse_baud(&text(p.value()?, "-b")?)?, "-b")?,
            Long("databits") => {
                let v = choose(
                    &text(p.value()?, "--databits")?,
                    "--databits",
                    &[
                        ("5", DataBits::Five),
                        ("6", DataBits::Six),
                        ("7", DataBits::Seven),
                        ("8", DataBits::Eight),
                    ],
                )?;
                set_once(&mut data_bits, v, "--databits")?;
            }
            Long("parity") => {
                let v = choose(
                    &text(p.value()?, "--parity")?,
                    "--parity",
                    &[
                        ("none", Parity::None),
                        ("even", Parity::Even),
                        ("odd", Parity::Odd),
                    ],
                )?;
                set_once(&mut parity, v, "--parity")?;
            }
            Long("stopbits") => {
                let v = choose(
                    &text(p.value()?, "--stopbits")?,
                    "--stopbits",
                    &[("1", StopBits::One), ("2", StopBits::Two)],
                )?;
                set_once(&mut stop_bits, v, "--stopbits")?;
            }
            Long("flow") => {
                let v = choose(
                    &text(p.value()?, "--flow")?,
                    "--flow",
                    &[
                        ("none", Flow::None),
                        ("rts", Flow::RtsCts),
                        ("xon", Flow::XonXoff),
                    ],
                )?;
                set_once(&mut flow, v, "--flow")?;
            }
            Short('o') => no_init = true,
            Long("no-lock") => no_lock = true,
            Long("low-latency") => low_latency = true,
            Long("headless") => headless = true,
            Long("no-ctl-marker") => ctl_marker = false,
            Long("ctl") => {
                let v = choose(
                    &text(p.value()?, "--ctl")?,
                    "--ctl",
                    &[
                        ("rw", crate::ctl::CtlMode::ReadWrite),
                        ("ro", crate::ctl::CtlMode::ReadOnly),
                        ("off", crate::ctl::CtlMode::Off),
                    ],
                )?;
                set_once(&mut ctl, v, "--ctl")?;
            }
            Long("ring-size") => set_once(
                &mut ring_size,
                parse_ring_size(&text(p.value()?, "--ring-size")?)?,
                "--ring-size",
            )?,
            Short('w') => wrap = true,
            Short('C') => {
                let v = p.value()?;
                if v.is_empty() {
                    return Err(CliError("-C needs a file name".into()));
                }
                if v.as_encoded_bytes().starts_with(b"-") {
                    return Err(CliError(format!(
                        "-C needs a file name, got '{}' (use ./{0} for a file whose name starts with -)",
                        v.to_string_lossy()
                    )));
                }
                set_once(&mut capture, PathBuf::from(v), "-C")?;
            }
            Long("color") => {
                let v = choose(
                    &text(p.value()?, "--color")?,
                    "--color",
                    &[
                        ("auto", ColorMode::Auto),
                        ("always", ColorMode::Always),
                        ("never", ColorMode::Never),
                        // minicom's spelling
                        ("on", ColorMode::Always),
                        ("off", ColorMode::Never),
                    ],
                )?;
                set_once(&mut color, v, "--color/-c")?;
            }
            Short('c') => {
                let v = choose(
                    &text(p.value()?, "-c")?,
                    "-c",
                    &[("on", ColorMode::Always), ("off", ColorMode::Never)],
                )?;
                set_once(&mut color, v, "--color/-c")?;
            }
            Long("prefix") | Short('e') | Long("escape") => {
                let name = match arg {
                    Short('e') => "-e",
                    Long("escape") => "--escape",
                    _ => "--prefix",
                };
                let key = PrefixKey::parse(&text(p.value()?, name)?)?;
                set_once(&mut prefix, key, "prefix key")?;
            }
            Short('m') | Long("metakey") => set_once(&mut prefix, PrefixKey::ALT_A, "prefix key")?,
            Short('M') | Long("metakey8") => {
                return Err(CliError(
                    "-M/--metakey8 is not supported: 8-bit meta collides with UTF-8; use -m".into(),
                ));
            }
            Long("prefix-time") => set_once(
                &mut prefix_time,
                parse_prefix_time(&text(p.value()?, "--prefix-time")?)?,
                "--prefix-time",
            )?,
            Long("show-prefix") => show_prefix = true,
            Short('V') | Long("version") => version = true,
            Short('h') | Long("help") => help = true,
            _ => return Err(arg.unexpected().into()),
        }
    }

    if help {
        // A broken XCOMM_PREFIX must not keep the help from showing: the
        // prefix is worked out without it, and the help says what is wrong.
        return Ok(match resolve_prefix(prefix, env) {
            Ok(choice) => Command::Help(choice, None),
            Err(e) => {
                let without = |name: &str| (name != "XCOMM_PREFIX").then(|| env(name)).flatten();
                Command::Help(resolve_prefix(prefix, &without)?, Some(e.0))
            }
        });
    }

    if version {
        if others > 0 {
            return Err(CliError("--version takes no other arguments".into()));
        }
        return Ok(Command::Version);
    }
    if show_prefix {
        if non_prefix > 0 {
            return Err(CliError(
                "--show-prefix takes only --prefix, -e/--escape or -m/--metakey".into(),
            ));
        }
        return Ok(Command::ShowPrefix(resolve_prefix(prefix, env)?));
    }
    let Some(device) = device else {
        return Err(CliError(USAGE.into()));
    };
    let ctl = ctl.unwrap_or(crate::ctl::CtlMode::ReadWrite);
    if headless && ctl == crate::ctl::CtlMode::Off {
        return Err(CliError(
            "--headless needs the control socket: with --ctl off nothing could reach the session"
                .into(),
        ));
    }
    let prefix = resolve_prefix(prefix, env)?;
    let defaults = LineSettings::default();
    Ok(Command::Term(TermOpts {
        device,
        line: LineSettings {
            baud: baud.unwrap_or(defaults.baud),
            data_bits: data_bits.unwrap_or(defaults.data_bits),
            parity: parity.unwrap_or(defaults.parity),
            stop_bits: stop_bits.unwrap_or(defaults.stop_bits),
            flow: flow.unwrap_or(defaults.flow),
        },
        no_init,
        no_lock,
        low_latency,
        prefix,
        prefix_time: prefix_time.unwrap_or(Duration::from_millis(25)),
        wrap,
        color: color.unwrap_or(ColorMode::Auto),
        capture,
        ring_size: ring_size.unwrap_or(DEFAULT_RING_SIZE),
        ctl,
        headless,
        ctl_marker,
    }))
}
