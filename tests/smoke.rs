use std::process::{Command, Stdio};

fn xcomm(args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_xcomm"))
        .args(args)
        .output()
        .unwrap()
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
        "xcomm: usage: xcomm [options] <device>\n"
    );
}

#[test]
fn version_with_extra_argument_is_a_usage_error() {
    let out = xcomm(&["--version", "extra"]);
    assert_eq!(out.status.code(), Some(2));
    assert!(out.stdout.is_empty());
    assert_eq!(
        String::from_utf8(out.stderr).unwrap(),
        "xcomm: usage: xcomm [options] <device>\n"
    );
}

#[test]
fn version_to_closed_pipe_reports_error_without_aborting() {
    let out = Command::new(env!("CARGO_BIN_EXE_xcomm"))
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
    let out = Command::new(env!("CARGO_BIN_EXE_xcomm"))
        .stderr(closed_pipe())
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
}

#[test]
fn version_with_stdout_and_stderr_closed_exits_1() {
    let out = Command::new(env!("CARGO_BIN_EXE_xcomm"))
        .arg("--version")
        .stdout(closed_pipe())
        .stderr(closed_pipe())
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
}
