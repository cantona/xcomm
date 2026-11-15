use std::ffi::OsString;
use std::time::Duration;
use xcomm::cli::*;

type Env = &'static [(&'static str, &'static str)];

fn env_of(pairs: Env) -> impl Fn(&str) -> Option<String> {
    move |k| {
        pairs
            .iter()
            .find(|(n, _)| *n == k)
            .map(|(_, v)| v.to_string())
    }
}

fn args(v: &[&str]) -> Vec<OsString> {
    v.iter().map(OsString::from).collect()
}

fn parse_with(v: &[&str], env: Env) -> Result<Command, CliError> {
    parse(args(v), &env_of(env))
}

fn term(v: &[&str], env: Env) -> TermOpts {
    match parse_with(v, env).unwrap() {
        Command::Term(t) => t,
        c => panic!("expected Term, got {c:?}"),
    }
}

fn err(v: &[&str], env: Env) -> String {
    parse_with(v, env).unwrap_err().0
}

const CTRL_A: PrefixKey = PrefixKey {
    modifier: Modifier::Ctrl,
    letter: b'a',
};
const ALT_A: PrefixKey = PrefixKey {
    modifier: Modifier::Alt,
    letter: b'a',
};

#[test]
fn minicom_style_device_and_baud() {
    let t = term(&["-D", "/dev/ttyUSB1", "-b", "9600"], &[]);
    assert_eq!(t.device, "/dev/ttyUSB1");
    assert_eq!(t.line.baud, 9600);
}

#[test]
fn positional_device() {
    assert_eq!(term(&["ttyUSB1"], &[]).device, "ttyUSB1");
}

#[test]
fn defaults_are_115200_8n1_no_flow() {
    let t = term(&["x"], &[]);
    assert_eq!(
        t.line,
        LineSettings {
            baud: 115200,
            data_bits: DataBits::Eight,
            parity: Parity::None,
            stop_bits: StopBits::One,
            flow: Flow::None,
        }
    );
    assert_eq!(t.line, LineSettings::default());
    assert_eq!(t.prefix_time, Duration::from_millis(25));
    assert!(!t.no_init && !t.no_lock && !t.low_latency && !t.wrap);
    assert_eq!(t.color, ColorMode::Auto);
    assert_eq!(t.capture, None);
}

#[test]
fn capture_option() {
    let t = term(&["-C", "boot.log", "x"], &[]);
    assert_eq!(t.capture, Some(std::path::PathBuf::from("boot.log")));
    assert!(err(&["-C", "", "x"], &[]).contains("needs a file name"));
    assert!(err(&["-C", "-w", "x"], &[]).contains("use ./-w"));
    assert_eq!(
        term(&["-C", "./-w", "x"], &[]).capture,
        Some(std::path::PathBuf::from("./-w"))
    );
    assert!(err(&["-C", "a", "-C", "b", "x"], &[]).contains("more than once"));
}

#[test]
fn color_options() {
    for (v, want) in [
        (&["--color", "auto"][..], ColorMode::Auto),
        (&["--color", "always"], ColorMode::Always),
        (&["--color", "never"], ColorMode::Never),
        (&["--color=never"], ColorMode::Never),
        (&["-c", "on"], ColorMode::Always),
        (&["-c", "off"], ColorMode::Never),
        (&["-coff"], ColorMode::Never),
        (&["--color=on"], ColorMode::Always),
        (&["--color", "off"], ColorMode::Never),
    ] {
        let mut a = v.to_vec();
        a.push("x");
        assert_eq!(term(&a, &[]).color, want, "{v:?}");
    }
    let e = err(&["--color", "yes", "x"], &[]);
    assert!(e.contains("auto, always, never"), "{e}");
    let e = err(&["-c", "always", "x"], &[]);
    assert!(e.contains("on, off"), "{e}");
    let e = err(&["-c", "on", "--color", "never", "x"], &[]);
    assert!(e.contains("more than once"), "{e}");
}

#[test]
fn arbitrary_baud_accepted() {
    assert_eq!(term(&["-b", "3000000", "x"], &[]).line.baud, 3_000_000);
    assert_eq!(term(&["-b", "250000", "x"], &[]).line.baud, 250_000);
}

#[test]
fn bad_baud_rejected() {
    for b in ["0", "-1", "abc", "", "4294967296"] {
        assert!(parse_with(&["-b", b, "x"], &[]).is_err(), "{b:?}");
    }
}

#[test]
fn device_given_twice_rejected() {
    assert!(err(&["-D", "a", "b"], &[]).contains("device"));
    assert!(err(&["a", "b"], &[]).contains("device"));
}

#[test]
fn empty_device_rejected() {
    assert!(parse_with(&["-D", ""], &[]).is_err());
}

#[test]
fn no_device_is_usage_error() {
    assert_eq!(err(&[], &[]), "usage: xcomm [options] <device>");
}

#[test]
fn repeated_value_option_rejected() {
    assert!(err(&["-b", "9600", "-b", "4800", "x"], &[]).contains("-b"));
    assert!(parse_with(&["--parity", "odd", "--parity", "even", "x"], &[]).is_err());
}

#[test]
fn line_format_options() {
    let t = term(
        &[
            "--databits",
            "7",
            "--parity",
            "even",
            "--stopbits",
            "2",
            "--flow",
            "rts",
            "x",
        ],
        &[],
    );
    assert_eq!(
        (
            t.line.data_bits,
            t.line.parity,
            t.line.stop_bits,
            t.line.flow
        ),
        (DataBits::Seven, Parity::Even, StopBits::Two, Flow::RtsCts)
    );
    assert_eq!(
        term(&["--databits", "5", "x"], &[]).line.data_bits,
        DataBits::Five
    );
    assert_eq!(
        term(&["--databits", "6", "x"], &[]).line.data_bits,
        DataBits::Six
    );
    assert_eq!(
        term(&["--parity", "odd", "x"], &[]).line.parity,
        Parity::Odd
    );
    assert_eq!(term(&["--flow", "xon", "x"], &[]).line.flow, Flow::XonXoff);
    assert_eq!(term(&["--flow", "none", "x"], &[]).line.flow, Flow::None);
}

#[test]
fn bad_line_format_values_rejected() {
    for (opt, v) in [
        ("--databits", "4"),
        ("--databits", "9"),
        ("--parity", "mark"),
        ("--stopbits", "3"),
        ("--flow", "dtr"),
    ] {
        let e = err(&[opt, v, "x"], &[]);
        assert!(e.contains(opt), "{opt} {v}: {e}");
    }
}

#[test]
fn boolean_flags() {
    let t = term(&["-o", "--no-lock", "--low-latency", "-w", "x"], &[]);
    assert!(t.no_init && t.no_lock && t.low_latency && t.wrap);
}

#[test]
fn metakey8_is_rejected_with_reason() {
    for flag in ["-M", "--metakey8"] {
        let e = err(&[flag, "x"], &[]);
        assert!(e.contains("UTF-8") && e.contains("-m"), "{e}");
    }
}

#[test]
fn prefix_default_is_ctrl_a_outside_multiplexer() {
    let p = term(&["x"], &[]).prefix;
    assert_eq!(p.key, CTRL_A);
    assert_eq!(p.source, PrefixSource::Default);
}

#[test]
fn prefix_alt_a_inside_each_multiplexer() {
    for (var, name) in [
        ("HERDR_ENV", "herdr"),
        ("HC_TERMINAL_ID", "Herdr app"),
        ("TMUX", "tmux"),
        ("STY", "screen"),
        ("ZELLIJ", "zellij"),
    ] {
        let env = move |k: &str| (k == var).then(|| "1".to_string());
        let Command::Term(t) = parse(args(&["x"]), &env).unwrap() else {
            panic!()
        };
        assert_eq!(t.prefix.key, ALT_A, "{var}");
        assert_eq!(t.prefix.source, PrefixSource::Multiplexer(name), "{var}");
    }
}

#[test]
fn empty_multiplexer_variable_counts_as_unset() {
    assert_eq!(term(&["x"], &[("TMUX", "")]).prefix.key, CTRL_A);
}

#[test]
fn env_beats_multiplexer_and_flag_beats_env() {
    let t = term(&["x"], &[("TMUX", "1"), ("XCOMM_PREFIX", "ctrl-t")]);
    assert_eq!(
        t.prefix.key,
        PrefixKey {
            modifier: Modifier::Ctrl,
            letter: b't'
        }
    );
    assert_eq!(t.prefix.source, PrefixSource::Env);
    let t = term(&["--prefix", "alt-x", "x"], &[("XCOMM_PREFIX", "ctrl-t")]);
    assert_eq!(
        t.prefix.key,
        PrefixKey {
            modifier: Modifier::Alt,
            letter: b'x'
        }
    );
    assert_eq!(t.prefix.source, PrefixSource::Flag);
}

#[test]
fn escape_and_metakey_aliases() {
    assert_eq!(term(&["-e", "ctrl-b", "x"], &[]).prefix.key.letter, b'b');
    assert_eq!(
        term(&["--escape", "ctrl-b", "x"], &[]).prefix.key.letter,
        b'b'
    );
    assert_eq!(term(&["-m", "x"], &[]).prefix.key, ALT_A);
    assert_eq!(term(&["--metakey", "x"], &[]).prefix.key, ALT_A);
}

#[test]
fn prefix_given_twice_rejected_across_spellings() {
    assert!(err(&["-m", "--prefix", "ctrl-b", "x"], &[]).contains("prefix"));
    assert!(parse_with(&["-e", "ctrl-b", "--escape", "ctrl-c", "x"], &[]).is_err());
}

#[test]
fn prefix_parse_accepts_case_insensitive_letters() {
    assert_eq!(PrefixKey::parse("CTRL-A").unwrap(), CTRL_A);
    assert_eq!(PrefixKey::parse("Alt-A").unwrap(), ALT_A);
}

#[test]
fn bad_prefix_values_rejected() {
    for v in [
        "", "a", "ctrl-", "ctrl-1", "shift-a", "alt-ab", "ctrl-a ", "meta-a",
    ] {
        assert!(PrefixKey::parse(v).is_err(), "{v:?}");
    }
}

#[test]
fn prefix_keys_that_collide_with_terminal_input_rejected() {
    // Ctrl-H is Backspace on many terminals; Ctrl-I, Ctrl-J and Ctrl-M are
    // Tab, newline and Enter.
    for v in ["ctrl-h", "ctrl-i", "ctrl-j", "ctrl-m"] {
        let e = PrefixKey::parse(v).unwrap_err().0;
        assert!(e.contains(v), "{v}: {e}");
    }
}

#[test]
fn alt_o_is_allowed() {
    // Alt-O sends ESC o; F1-F4 and application arrow keys send ESC O.
    assert!(PrefixKey::parse("alt-o").is_ok());
}

#[test]
fn prefix_display() {
    assert_eq!(CTRL_A.display(), "Ctrl-A");
    assert_eq!(PrefixKey::parse("alt-x").unwrap().display(), "Alt-X");
}

#[test]
fn bad_env_prefix_is_an_error_not_ignored() {
    let e = err(&["x"], &[("XCOMM_PREFIX", "bogus")]);
    assert!(e.contains("XCOMM_PREFIX"), "{e}");
}

#[test]
fn empty_env_prefix_counts_as_unset() {
    assert_eq!(term(&["x"], &[("XCOMM_PREFIX", "")]).prefix.key, CTRL_A);
}

#[test]
fn prefix_time_bounds() {
    assert_eq!(
        term(&["--prefix-time", "100", "x"], &[]).prefix_time,
        Duration::from_millis(100)
    );
    for v in ["0", "1001", "x"] {
        assert!(parse_with(&["--prefix-time", v, "x"], &[]).is_err(), "{v}");
    }
}

#[test]
fn show_prefix_needs_no_device_and_honours_flags() {
    match parse_with(&["--show-prefix"], &[("STY", "1")]).unwrap() {
        Command::ShowPrefix(p) => assert_eq!(p.source, PrefixSource::Multiplexer("screen")),
        c => panic!("{c:?}"),
    }
    match parse_with(&["--show-prefix", "-m"], &[]).unwrap() {
        Command::ShowPrefix(p) => assert_eq!((p.key, p.source), (ALT_A, PrefixSource::Flag)),
        c => panic!("{c:?}"),
    }
}

#[test]
fn show_prefix_with_device_rejected() {
    assert!(parse_with(&["--show-prefix", "ttyUSB1"], &[]).is_err());
}

#[test]
fn show_prefix_rejects_non_prefix_options() {
    for extra in [
        &["-o"][..],
        &["-b", "9600"],
        &["--prefix-time", "5"],
        &["--no-lock"],
    ] {
        let mut v = vec!["--show-prefix"];
        v.extend_from_slice(extra);
        assert!(parse_with(&v, &[]).is_err(), "{v:?}");
    }
    assert!(parse_with(&["--show-prefix", "-e", "ctrl-b"], &[]).is_ok());
}

#[test]
fn bad_env_prefix_without_device_is_still_usage_error() {
    assert_eq!(
        err(&[], &[("XCOMM_PREFIX", "bogus")]),
        "usage: xcomm [options] <device>"
    );
}

#[test]
fn describe_names_each_source() {
    let d = |v: &[&str], env: Env| match parse_with(v, env).unwrap() {
        Command::ShowPrefix(p) => p.describe(),
        c => panic!("{c:?}"),
    };
    assert_eq!(
        d(&["--show-prefix", "--prefix", "alt-x"], &[]),
        "Alt-X (command line)"
    );
    assert_eq!(
        d(&["--show-prefix"], &[("XCOMM_PREFIX", "ctrl-t")]),
        "Ctrl-T (XCOMM_PREFIX)"
    );
    assert_eq!(
        d(&["--show-prefix"], &[("TMUX", "x")]),
        "Alt-A (multiplexer: tmux)"
    );
    assert_eq!(d(&["--show-prefix"], &[]), "Ctrl-A (default)");
}

#[test]
fn lexopt_spellings() {
    assert_eq!(term(&["-b9600", "x"], &[]).line.baud, 9600);
    assert_eq!(term(&["--prefix=alt-b", "x"], &[]).prefix.key.letter, b'b');
    assert_eq!(term(&["x", "-b", "4800"], &[]).line.baud, 4800);
    assert_eq!(term(&["--", "-x"], &[]).device, "-x");
    assert!(err(&["-D", "-x"], &[]).contains("-D needs a device"));
    assert!(err(&["-D", "-b", "x"], &[]).contains("-D needs a device"));
    assert!(parse_with(&["-VV"], &[]).is_err());
    assert!(parse_with(&["--version", "--version"], &[]).is_err());
    assert!(parse_with(&["-V", "--version"], &[]).is_err());
    assert!(parse_with(&["-D", "--", "x"], &[]).is_err());
    assert!(parse_with(&["-D", "--"], &[]).is_err());
}

#[test]
fn version_alone() {
    assert!(matches!(
        parse_with(&["--version"], &[]).unwrap(),
        Command::Version
    ));
    assert!(matches!(
        parse_with(&["-V"], &[]).unwrap(),
        Command::Version
    ));
}

#[test]
fn version_with_other_arguments_rejected() {
    assert!(parse_with(&["--version", "x"], &[]).is_err());
    assert!(parse_with(&["-b", "9600", "--version"], &[]).is_err());
}

#[test]
fn unknown_option_rejected() {
    assert!(parse_with(&["--frobnicate", "x"], &[]).is_err());
    assert!(parse_with(&["-Z", "x"], &[]).is_err());
}

#[test]
fn combined_short_flags() {
    let t = term(&["-om", "x"], &[]);
    assert!(t.no_init);
    assert_eq!(t.prefix.key, ALT_A);
}

#[test]
fn bad_env_prefix_reported_even_when_a_flag_overrides_it() {
    let e = err(
        &["--show-prefix", "--prefix", "alt-b"],
        &[("XCOMM_PREFIX", "garbage")],
    );
    assert!(e.contains("XCOMM_PREFIX"), "{e}");
    assert!(parse_with(&["-m", "x"], &[("XCOMM_PREFIX", "garbage")]).is_err());
}

#[test]
fn non_utf8_values_name_the_option() {
    use std::os::unix::ffi::OsStringExt;
    let bad = || OsString::from_vec(vec![0xff]);
    for (opt, name) in [
        ("-b", "-b"),
        ("-D", "-D"),
        ("--prefix", "--prefix"),
        ("-e", "-e"),
        ("--escape", "--escape"),
        ("--parity", "--parity"),
    ] {
        let e = parse(
            vec![OsString::from(opt), bad(), OsString::from("x")],
            &env_of(&[]),
        )
        .unwrap_err()
        .0;
        assert!(e.contains(name) && e.contains("UTF-8"), "{opt}: {e}");
    }
    let e = parse(vec![bad()], &env_of(&[])).unwrap_err().0;
    assert!(e.contains("device"), "{e}");
}

#[test]
fn bare_separator_counts_as_an_argument_for_version_and_show_prefix() {
    assert!(parse_with(&["--version", "--"], &[]).is_err());
    assert!(parse_with(&["--show-prefix", "--"], &[]).is_err());
}

#[test]
fn dash_devices_rejected_with_usable_hints() {
    let e = err(&["-D", "--"], &[]);
    assert!(!e.contains("use --"), "{e}");
    let e = err(&["-D", "-"], &[]);
    assert!(!e.contains("use --"), "{e}");
    assert!(err(&["-D", "-foo"], &[]).contains("use -- -foo"));
    assert_eq!(term(&["--", "-foo"], &[]).device, "-foo");
    assert!(parse_with(&["-"], &[]).is_err());
    assert!(parse_with(&["--", "--"], &[]).is_err());
}

#[test]
fn numbers_must_be_plain_digits() {
    for v in ["+9600", " 9600", "9600 ", "0x10"] {
        assert!(parse_with(&["-b", v, "x"], &[]).is_err(), "{v:?}");
    }
    assert!(parse_with(&["--prefix-time", "+25", "x"], &[]).is_err());
    assert_eq!(
        term(&["--prefix-time", "0025", "x"], &[]).prefix_time,
        Duration::from_millis(25)
    );
}

#[test]
fn help_wins_over_the_other_arguments() {
    for args in [
        &["-h"][..],
        &["--help", "-b", "9600", "ttyUSB0"],
        &["ttyUSB0", "-h"],
    ] {
        assert!(
            matches!(parse_with(args, &[]), Ok(Command::Help(..))),
            "{args:?}"
        );
    }
}

#[test]
fn help_is_an_option_like_the_others() {
    // An argument error before it is still reported.
    assert!(err(&["--help", "-b", "0"], &[]).contains("-b"));
    // An 'h' inside an attached value is that value, not -h.
    assert!(matches!(
        parse_with(&["-Dhvc0"], &[]),
        Ok(Command::Term(TermOpts { ref device, .. })) if device == "hvc0"
    ));
    assert!(matches!(
        parse_with(&["-Chist.log", "ttyUSB0"], &[]),
        Ok(Command::Term(TermOpts { ref capture, .. })) if capture.as_deref() == Some(std::path::Path::new("hist.log"))
    ));
    // After a clustered option, and after `--` (a device name).
    assert!(matches!(
        parse_with(&["-DttyUSB0", "-h"], &[]),
        Ok(Command::Help(..))
    ));
    assert!(!matches!(
        parse_with(&["--", "-h"], &[]),
        Ok(Command::Help(..))
    ));
}

#[test]
fn ring_size_takes_bytes_with_a_unit() {
    assert_eq!(term(&["x"], &[]).ring_size, 1 << 20);
    for (v, n) in [
        ("4096", 4096),
        ("4k", 4096),
        ("64K", 64 << 10),
        ("2m", 2 << 20),
        ("1G", 1 << 30),
    ] {
        assert_eq!(term(&["--ring-size", v, "x"], &[]).ring_size, n, "{v}");
    }
    for v in [
        "",
        "4095",
        "2g",
        "1.5m",
        "-1",
        "k",
        "4 k",
        "4kb",
        "99999999999999999999",
    ] {
        let e = err(&["--ring-size", v, "x"], &[]);
        assert!(e.contains("--ring-size"), "{v}: {e}");
    }
    let e = err(&["--ring-size", "4k", "--ring-size", "8k", "x"], &[]);
    assert!(e.contains("more than once"), "{e}");
}

fn ctl(v: &[&str]) -> CtlArgs {
    let mut all = vec!["ctl"];
    all.extend_from_slice(v);
    match parse_with(&all, &[]).unwrap() {
        Command::Ctl(c) => c,
        c => panic!("expected Ctl, got {c:?}"),
    }
}

fn ctl_err(v: &[&str]) -> String {
    let mut all = vec!["ctl"];
    all.extend_from_slice(v);
    err(&all, &[])
}

#[test]
fn ctl_commands_parse() {
    let c = ctl(&["ttyUSB1", "status"]);
    assert_eq!(
        (c.device.as_deref(), &c.cmd, c.json),
        (Some("ttyUSB1"), &CtlCmd::Status, false)
    );
    let c = ctl(&["--json", "sessions"]);
    assert_eq!((c.device, c.cmd, c.json), (None, CtlCmd::Sessions, true));
    assert!(ctl(&["sessions", "--json"]).json);
    assert_eq!(
        ctl(&["ttyUSB1", "send", "ls"]).cmd,
        CtlCmd::Send {
            data: SendData::Text("ls".into()),
            ending: LineEnding::Cr
        }
    );
    assert_eq!(
        ctl(&["ttyUSB1", "send", "-n", "ls"]).cmd,
        CtlCmd::Send {
            data: SendData::Text("ls".into()),
            ending: LineEnding::None
        }
    );
    assert_eq!(
        ctl(&["ttyUSB1", "send", "--hex", "0d 0A:ff"]).cmd,
        CtlCmd::Send {
            data: SendData::Hex(vec![0x0d, 0x0a, 0xff]),
            ending: LineEnding::None
        }
    );
    assert_eq!(
        ctl(&["ttyUSB1", "send", "--crlf", "--file", "f.bin"]).cmd,
        CtlCmd::Send {
            data: SendData::File("f.bin".into()),
            ending: LineEnding::CrLf
        }
    );
    assert_eq!(
        ctl(&["ttyUSB1", "read", "--last", "10", "--color", "never"]).cmd,
        CtlCmd::Read {
            since: None,
            last: Some(10),
            max_bytes: 1 << 20,
            from_oldest: false,
            color: ColorMode::Never
        }
    );
    assert_eq!(
        ctl(&[
            "ttyUSB1",
            "read",
            "--since",
            "5",
            "--max-bytes",
            "7",
            "--from-oldest"
        ])
        .cmd,
        CtlCmd::Read {
            since: Some(5),
            last: None,
            max_bytes: 7,
            from_oldest: true,
            color: ColorMode::Auto
        }
    );
}

#[test]
fn ctl_usage_errors() {
    for (v, want) in [
        (&[][..], "usage: xcomm ctl"),
        (&["ttyUSB1"], "usage: xcomm ctl"),
        (
            &["ttyUSB1", "frobnicate"],
            "unknown ctl command 'frobnicate'",
        ),
        (
            &["ttyUSB1", "status", "extra"],
            "unexpected argument 'extra'",
        ),
        (&["sessions", "extra"], "unexpected argument 'extra'"),
        (&["ttyUSB1", "send"], "needs <text>"),
        (&["ttyUSB1", "send", "a", "b"], "unexpected argument 'b'"),
        (&["ttyUSB1", "send", "a", "--hex", "00"], "one of"),
        (&["ttyUSB1", "send", "--hex", "0"], "--hex"),
        (&["ttyUSB1", "send", "--hex", "zz"], "--hex"),
        (&["ttyUSB1", "send", "-n", "--cr", "a"], "more than once"),
        (
            &["ttyUSB1", "status", "--hex", "00"],
            "--hex is for ctl send",
        ),
        (
            &["ttyUSB1", "send", "--last", "3", "a"],
            "--last is for ctl read",
        ),
        (
            &["ttyUSB1", "read", "--since", "1", "--last", "2"],
            "not both",
        ),
        (&["ttyUSB1", "read", "--since", "-1"], "--since"),
        (&["ttyUSB1", "read", "--since", "x"], "--since"),
    ] {
        let e = ctl_err(v);
        assert!(e.contains(want), "{v:?}: {e}");
    }
}

#[test]
fn ctl_option_of_a_session() {
    use xcomm::ctl::CtlMode;
    assert_eq!(term(&["x"], &[]).ctl, CtlMode::ReadWrite);
    assert_eq!(term(&["--ctl", "ro", "x"], &[]).ctl, CtlMode::ReadOnly);
    assert_eq!(term(&["--ctl", "off", "x"], &[]).ctl, CtlMode::Off);
    assert!(err(&["--ctl", "rx", "x"], &[]).contains("--ctl"));
    assert!(err(&["--ctl", "ro", "--ctl", "rw", "x"], &[]).contains("more than once"));
}
