use std::process::{Command, Stdio};

/// Strips variables that change prefix resolution, so the tests do not
/// depend on the environment they run in.
fn cmd() -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_xcomm"));
    for v in [
        "XCOMM_PREFIX",
        "HERDR_ENV",
        "HC_TERMINAL_ID",
        "TMUX",
        "STY",
        "ZELLIJ",
    ] {
        c.env_remove(v);
    }
    c
}

fn xcomm(args: &[&str]) -> std::process::Output {
    cmd().args(args).output().unwrap()
}

#[test]
fn version_flag_prints_name_and_version() {
    let out = xcomm(&["--version"]);
    assert!(out.status.success());
    assert_eq!(
        String::from_utf8(out.stdout).unwrap(),
        format!("xcomm {}\n", env!("CARGO_PKG_VERSION"))
    );
    assert!(out.stderr.is_empty());
}

#[test]
fn no_arguments_prints_usage_and_exits_2() {
    let out = xcomm(&[]);
    assert_eq!(out.status.code(), Some(2));
    assert!(out.stdout.is_empty());
    assert_eq!(
        String::from_utf8(out.stderr).unwrap(),
        "xcomm: usage: xcomm [options] <device>\nTry 'xcomm --help' for more information.\n"
    );
}

#[test]
fn version_with_extra_argument_is_rejected() {
    let out = xcomm(&["--version", "extra"]);
    assert_eq!(out.status.code(), Some(2));
    assert!(out.stdout.is_empty());
    assert_eq!(
        String::from_utf8(out.stderr).unwrap(),
        "xcomm: --version takes no other arguments\nTry 'xcomm --help' for more information.\n"
    );
}

#[test]
fn show_prefix_prints_key_and_reason() {
    let out = cmd().args(["--show-prefix", "-m"]).output().unwrap();
    assert!(out.status.success());
    assert_eq!(
        String::from_utf8(out.stdout).unwrap(),
        "Alt-A (command line)\n"
    );
}

#[test]
fn bad_option_exits_2_with_message() {
    let out = xcomm(&["--frobnicate"]);
    assert_eq!(out.status.code(), Some(2));
    let err = String::from_utf8(out.stderr).unwrap();
    assert!(
        err.starts_with("xcomm: ") && err.contains("--frobnicate"),
        "{err}"
    );
}

#[test]
fn non_utf8_prefix_env_is_an_error() {
    use std::os::unix::ffi::OsStrExt;
    let out = cmd()
        .arg("--show-prefix")
        .env("XCOMM_PREFIX", std::ffi::OsStr::from_bytes(b"ctrl-\xff"))
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("XCOMM_PREFIX"));
}

#[test]
fn version_to_closed_pipe_reports_error_without_aborting() {
    let out = cmd()
        .arg("--version")
        .stdout(closed_pipe())
        .stderr(Stdio::piped())
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    let err = String::from_utf8(out.stderr).unwrap();
    assert!(err.starts_with("xcomm: cannot write to stdout: "), "{err}");
}

fn closed_pipe() -> std::io::PipeWriter {
    let (reader, writer) = std::io::pipe().unwrap();
    drop(reader);
    writer
}

#[test]
fn usage_error_with_closed_stderr_still_exits_2() {
    let out = cmd().stderr(closed_pipe()).output().unwrap();
    assert_eq!(out.status.code(), Some(2));
}

#[test]
fn version_with_stdout_and_stderr_closed_exits_1() {
    let out = cmd()
        .arg("--version")
        .stdout(closed_pipe())
        .stderr(closed_pipe())
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
}

#[test]
fn help_prints_options_and_every_command_key() {
    for flag in ["--help", "-h"] {
        let out = xcomm(&[flag]);
        assert!(out.status.success());
        assert!(out.stderr.is_empty());
        let text = String::from_utf8(out.stdout).unwrap();
        assert!(
            text.contains("Usage:\n  xcomm [options] <device>"),
            "{text}"
        );
        for option in [
            "-b <baud>",
            "--flow",
            "--prefix <key>",
            "-C <file>",
            "--color",
        ] {
            assert!(text.contains(option), "{option} missing");
        }
        for letter in "zxqfhdtpewlsr".chars() {
            assert!(
                text.contains(&format!("  Ctrl-A {letter}  ")),
                "{letter} missing"
            );
        }
        assert!(text.contains("Here it is Ctrl-A (default)."), "{text}");
        assert!(
            text.lines().all(|l| l.len() <= 80),
            "a line is wider than 80"
        );
    }
}

#[test]
fn help_shows_the_prefix_a_session_would_use() {
    let out = cmd().arg("--help").env("TMUX", "x").output().unwrap();
    let text = String::from_utf8(out.stdout).unwrap();
    assert!(
        text.contains("Here it is Alt-A (multiplexer: tmux)."),
        "{text}"
    );
    assert!(text.contains("  Alt-A x  quit"), "{text}");
    // A broken setting does not keep the help from showing, says so, and
    // leaves the flag and the multiplexer as they would be.
    let out = cmd()
        .args(["--help", "--prefix", "alt-b"])
        .env("XCOMM_PREFIX", "bogus")
        .output()
        .unwrap();
    assert!(out.status.success());
    let text = String::from_utf8(out.stdout).unwrap();
    assert!(
        text.contains(
            "Here it is Alt-B (command line).\nA session would not start: XCOMM_PREFIX is not usable:\n  invalid prefix key 'bogus'"
        ),
        "{text}"
    );
    // The longest of those messages still fits 80 columns.
    let out = cmd()
        .arg("--help")
        .env("XCOMM_PREFIX", "ctrl-h")
        .output()
        .unwrap();
    let text = String::from_utf8(out.stdout).unwrap();
    assert!(text.contains("Backspace"), "{text}");
    assert!(text.lines().all(|l| l.len() <= 80), "{text}");
    let out = cmd()
        .arg("-h")
        .env("XCOMM_PREFIX", "bogus")
        .env("TMUX", "x")
        .output()
        .unwrap();
    let text = String::from_utf8(out.stdout).unwrap();
    assert!(
        text.contains("Here it is Alt-A (multiplexer: tmux)."),
        "{text}"
    );
}

#[test]
fn a_usage_error_points_to_help() {
    let out = xcomm(&["--frobnicate"]);
    assert_eq!(out.status.code(), Some(2));
    let err = String::from_utf8(out.stderr).unwrap();
    assert!(
        err.ends_with("\nTry 'xcomm --help' for more information.\n"),
        "{err}"
    );
}
