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
}

#[derive(Debug, Clone, PartialEq)]
pub enum Command {
    /// `-h`/`--help`, with the prefix a session would use here, and why a
    /// session would not start if XCOMM_PREFIX is broken.
    Help(PrefixChoice, Option<String>),
    Version,
    ShowPrefix(PrefixChoice),
    Term(TermOpts),
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
  xcomm --show-prefix [--prefix <key> | -m]
  xcomm -h | --help | -V | --version

<device> is a name in /dev (ttyUSB0) or, with a '/' in it, a path
(/dev/serial/by-id/...); -D <device> gives it as an option. Put -- before
a name that starts with '-'.

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
    }))
}
