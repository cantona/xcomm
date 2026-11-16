mod support;

use std::os::fd::AsFd;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::time::Duration;

use support::{lock_dir, read_until, spawn_with_env, write_all};
use xcomm::list::PortInfo;
use xcomm::pick::{
    PLUGIN_ID, PickAction, PickKey, PickState, Placement, decode_keys, incomplete, open_command,
    render,
};

const T: Duration = Duration::from_secs(5);

fn port(tty: &str, by_id: Option<&str>, pane: Option<&str>, lock: Option<i32>) -> PortInfo {
    PortInfo {
        tty: tty.into(),
        path: format!("/dev/{tty}"),
        by_id: by_id.map(|b| format!("/dev/serial/by-id/{b}")),
        vid_pid: Some("0403:6001".into()),
        product: Some("FT232R USB UART".into()),
        serial: None,
        lock_pid: lock,
        lock_error: None,
        herdr_pane: pane.map(str::to_string),
    }
}

fn two() -> PickState {
    PickState::new(vec![
        port("ttyUSB0", Some("usb-FTDI_A-if00-port0"), None, None),
        port("ttyUSB1", None, Some("w1:p4"), Some(4242)),
    ])
}

#[test]
fn keys_decode() {
    use PickKey::*;
    for (input, want) in [
        (&b"\x1b[A\x1b[B"[..], vec![Up, Down]),
        (b"\x1bOA\x1bOB", vec![Up, Down]),
        (b"kj", vec![Up, Down]),
        (b"\r\n", vec![Enter, Enter]),
        (b"bo", vec![Baud, Placement]),
        (b"\x1b", vec![Esc]),
        (b"q", vec![Esc]),
        (
            b"12\x7f\x08",
            vec![Digit(b'1'), Digit(b'2'), Backspace, Backspace],
        ),
        (b"\x1b[1;5Cz", vec![Other, Other]),
        // Alt with a key is not Esc then the key; Ctrl-C closes.
        (b"\x1bj", vec![Other]),
        (b"\x1b\x1b", vec![Esc, Esc]),
        (b"\x1bq", vec![Other]),
        // Alt held over Enter, Backspace or a non-ASCII key: not a close.
        (b"\x1b\r", vec![Other]),
        (b"\x1b\x7f", vec![Other]),
        (b"\x1b\x01", vec![Esc, Other]),
        (b"\x03", vec![Esc]),
        (b"\x1b[3~", vec![Other]),
    ] {
        assert_eq!(decode_keys(input), want, "{input:?}");
    }
    assert!(incomplete(b"x\x1b"));
    assert!(incomplete(b"\x1b["));
    assert!(!incomplete(b"\x1b[A"));
    // A key cut inside its parameters waits for the rest.
    assert!(incomplete(b"\x1b[1"));
    assert!(incomplete(b"\x1b[1;5"));
    assert!(!incomplete(b"\x1b[1;5A"));
    assert!(!incomplete(b"\x1bj"));
}

#[test]
fn moving_and_choosing() {
    let mut s = two();
    assert_eq!(s.apply(PickKey::Up), PickAction::Redraw);
    assert_eq!(s.selected, 0);
    assert_eq!(s.apply(PickKey::Enter), PickAction::Open);
    s.apply(PickKey::Down);
    s.apply(PickKey::Down);
    assert_eq!(s.selected, 1);
    // Open in a pane already: that pane is focused instead.
    assert_eq!(s.apply(PickKey::Enter), PickAction::Focus("w1:p4".into()));
    assert_eq!(s.apply(PickKey::Esc), PickAction::Quit);
    // Held with no pane known: it says who, rather than open a pane that
    // would fail on the lock.
    let mut s = PickState::new(vec![port("ttyUSB3", None, None, Some(77))]);
    assert_eq!(s.apply(PickKey::Enter), PickAction::Redraw);
    assert_eq!(
        s.message.as_deref(),
        Some("ttyUSB3 is in use by process 77")
    );
    let mut empty = PickState::new(vec![]);
    assert_eq!(empty.apply(PickKey::Enter), PickAction::Redraw);
    assert_eq!(empty.apply(PickKey::Down), PickAction::Redraw);
}

#[test]
fn the_baud_prompt_takes_digits_only() {
    let mut s = two();
    s.apply(PickKey::Baud);
    for k in [
        PickKey::Digit(b'9'),
        PickKey::Other,
        PickKey::Up,
        PickKey::Digit(b'6'),
        PickKey::Digit(b'0'),
        PickKey::Digit(b'1'),
        PickKey::Backspace,
        PickKey::Digit(b'0'),
    ] {
        assert_eq!(s.apply(k), PickAction::Redraw);
    }
    assert_eq!(s.prompt.as_deref(), Some("9600"));
    // Enter in the prompt sets the baud; it does not open the port.
    assert_eq!(s.apply(PickKey::Enter), PickAction::Redraw);
    assert_eq!((s.baud, &s.prompt), (9600, &None));
    s.apply(PickKey::Baud);
    s.apply(PickKey::Enter);
    assert_eq!(s.baud, 9600);
    assert!(s.message.as_deref().unwrap().contains("not a baud rate"));
    s.apply(PickKey::Baud);
    s.apply(PickKey::Digit(b'1'));
    // Esc leaves the prompt, not the picker.
    assert_eq!(s.apply(PickKey::Esc), PickAction::Redraw);
    assert_eq!(s.baud, 9600);
}

#[test]
fn placement_cycles_and_shapes_the_command() {
    let mut s = two();
    let argv = open_command(&s, PLUGIN_ID, Some("w1:p2"));
    assert_eq!(
        argv,
        [
            "plugin",
            "pane",
            "open",
            "--plugin",
            "cantona.herdr-xcomm",
            "--entrypoint",
            "session",
            "--placement",
            "split",
            "--direction",
            "right",
            "--target-pane",
            "w1:p2",
            "--env",
            "XCOMM_DEVICE=/dev/serial/by-id/usb-FTDI_A-if00-port0",
            "--env",
            "XCOMM_BAUD=115200",
            "--focus",
        ]
    );
    s.apply(PickKey::Placement);
    assert_eq!(s.placement, Placement::SplitDown);
    let argv = open_command(&s, PLUGIN_ID, None);
    assert_eq!(
        &argv[7..11],
        ["--placement", "split", "--direction", "down"]
    );
    assert!(!argv.contains(&"--target-pane".to_string()));
    s.apply(PickKey::Placement);
    s.apply(PickKey::Down);
    let argv = open_command(&s, PLUGIN_ID, Some("w1:p2"));
    assert_eq!(
        &argv[7..11],
        ["--placement", "tab", "--target-pane", "w1:p2"]
    );
    // No by-id link: the path.
    assert!(argv.contains(&"XCOMM_DEVICE=/dev/ttyUSB1".to_string()));
    s.apply(PickKey::Placement);
    assert_eq!(s.placement, Placement::SplitRight);
}

#[test]
fn a_new_list_keeps_the_selection_on_its_port() {
    let mut s = two();
    s.apply(PickKey::Down);
    s.set_ports(vec![
        port("ttyACM0", None, None, None),
        port("ttyUSB0", None, None, None),
        port("ttyUSB1", None, None, None),
    ]);
    assert_eq!(s.selected, 2);
    s.set_ports(vec![port("ttyACM0", None, None, None)]);
    assert_eq!(s.selected, 0);
    s.set_ports(vec![]);
    assert_eq!(s.selected, 0);
}

#[test]
fn the_screen() {
    let mut s = two();
    s.apply(PickKey::Down);
    let mut out = Vec::new();
    render(&s, 80, 10, &mut out);
    let text = String::from_utf8(out).unwrap();
    let lines: Vec<&str> = text
        .trim_start_matches("\x1b[H\x1b[2J")
        .split("\r\n")
        .collect();
    assert_eq!(
        lines,
        [
            "xcomm: serial ports   baud 115200   open in split right",
            "  ttyUSB0 0403:6001 -          FTDI_A-if00  FT232R USB UART",
            "> ttyUSB1 0403:6001 pane w1:p4 /dev/ttyUSB1 FT232R USB UART",
            "",
            "Enter open   b baud   o placement   Esc close",
        ]
    );
    assert!(lines.iter().all(|l| l.chars().count() <= 80));
    // Narrow: a long name keeps its end, where the serial is.
    let mut s = PickState::new(vec![port(
        "ttyUSB0",
        Some("usb-FTDI_FT2232H_device_FTA1B2C3-if01-port0"),
        None,
        Some(1230188),
    )]);
    s.selected = 0;
    let mut out = Vec::new();
    render(&s, 50, 10, &mut out);
    let text = String::from_utf8(out).unwrap();
    let row = text.split("\r\n").nth(1).unwrap();
    assert_eq!(row, "> ttyUSB0 0403:6001 pid 1230188 ~ice_FTA1B2C3-if01");
    // Narrow, short: lines cut, the selection kept in view.
    let many: Vec<PortInfo> = (0..20)
        .map(|i| port(&format!("ttyUSB{i}"), None, None, Some(i)))
        .collect();
    let mut s = PickState::new(many);
    for _ in 0..15 {
        s.apply(PickKey::Down);
    }
    let mut out = Vec::new();
    render(&s, 30, 8, &mut out);
    let text = String::from_utf8(out).unwrap();
    assert!(text.contains("> ttyUSB15"), "{text}");
    assert!(
        text.split("\r\n")
            .all(|l| l.trim_start_matches("\x1b[H\x1b[2J").chars().count() <= 30)
    );
    assert!(text.split("\r\n").count() <= 8);
    // A long message keeps its rows, end included, however long the list.
    s.message = Some(format!("{} the real reason", "x".repeat(200)));
    let mut out = Vec::new();
    render(&s, 30, 8, &mut out);
    let text = String::from_utf8(out).unwrap();
    // Wrapped at 30: the end of it is there.
    assert!(text.ends_with("the real \r\nreason"), "{text:?}");
    assert!(text.contains("> ttyUSB15"), "{text}");
    assert!(text.split("\r\n").count() <= 8);
}

/// A `herdr` that records its arguments and exits with `status`.
fn fake_herdr(dir: &Path, status: u8) -> std::path::PathBuf {
    let bin = dir.join("herdr");
    let log = dir.join("args");
    std::fs::write(
        &bin,
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$@\" > '{}'\necho 'no such plugin' >&2\nexit {status}\n",
            log.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
    bin
}

fn has_ports() -> bool {
    let o = std::process::Command::new(env!("CARGO_BIN_EXE_xcomm"))
        .args(["list", "--json"])
        .output()
        .unwrap();
    let v: serde_json::Value = serde_json::from_slice(&o.stdout).unwrap();
    v.as_array().is_some_and(|a| !a.is_empty())
}

#[test]
fn esc_closes_and_gives_the_terminal_back() {
    // Asked of the terminal from the same shell afterwards: macOS refuses
    // the question once the session leader is gone.
    let script = format!(
        "'{}' pick; echo status=$?; stty -a",
        env!("CARGO_BIN_EXE_xcomm")
    );
    let mut s = support::spawn_in_pty("/bin/sh", &["-c", &script], &lock_dir("pick"));
    read_until(s.host.master.as_fd(), b"Esc close", T);
    write_all(s.host.master.as_fd(), b"\x1b");
    let mut got =
        String::from_utf8_lossy(&read_until(s.host.master.as_fd(), b"status=0", T)).into_owned();
    got += &String::from_utf8_lossy(&s.read_after_exit(b"icanon", T));
    assert!(!got.contains("-icanon"), "{got}");
    // Alt-j or a split arrow does not close it; Ctrl-C does.
    let mut s = spawn_with_env(
        env!("CARGO_BIN_EXE_xcomm"),
        &["pick"],
        &lock_dir("pick"),
        &[],
    );
    read_until(s.host.master.as_fd(), b"Esc close", T);
    write_all(s.host.master.as_fd(), b"\x1bj");
    write_all(s.host.master.as_fd(), b"\x1b");
    std::thread::sleep(Duration::from_millis(5));
    write_all(s.host.master.as_fd(), b"[B");
    std::thread::sleep(Duration::from_millis(200));
    assert!(s.child.try_wait().unwrap().is_none(), "it closed");
    write_all(s.host.master.as_fd(), b"\x03");
    assert_eq!(s.wait_exit(T).code(), Some(0));
}

#[test]
fn enter_opens_the_port_through_herdr() {
    if !has_ports() {
        eprintln!("skipped: no serial ports here");
        return;
    }
    let dir = lock_dir("pick-herdr");
    let bin = fake_herdr(&dir, 0);
    let context = r#"{"focused_pane_id":"w1:p2"}"#;
    let mut s = spawn_with_env(
        env!("CARGO_BIN_EXE_xcomm"),
        &["pick"],
        &dir,
        &[
            ("HERDR_BIN_PATH", bin.to_str().unwrap()),
            ("HERDR_PLUGIN_CONTEXT_JSON", context),
        ],
    );
    read_until(s.host.master.as_fd(), b"Esc close", T);
    write_all(s.host.master.as_fd(), b"b9600\ro\r");
    assert_eq!(s.wait_exit(T).code(), Some(0));
    let args = std::fs::read_to_string(dir.join("args")).unwrap();
    let args: Vec<&str> = args.lines().collect();
    assert_eq!(&args[..3], ["plugin", "pane", "open"]);
    for want in [
        "--placement",
        "split",
        "down",
        "--target-pane",
        "w1:p2",
        "XCOMM_BAUD=9600",
        "--focus",
    ] {
        assert!(args.contains(&want), "{want}: {args:?}");
    }
    assert!(args.iter().any(|a| a.starts_with("XCOMM_DEVICE=/dev/")));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_failed_open_is_shown_and_the_picker_stays() {
    if !has_ports() {
        eprintln!("skipped: no serial ports here");
        return;
    }
    let dir = lock_dir("pick-herdr");
    let bin = fake_herdr(&dir, 3);
    let mut s = spawn_with_env(
        env!("CARGO_BIN_EXE_xcomm"),
        &["pick"],
        &dir,
        &[("HERDR_BIN_PATH", bin.to_str().unwrap())],
    );
    read_until(s.host.master.as_fd(), b"Esc close", T);
    write_all(s.host.master.as_fd(), b"\r");
    read_until(s.host.master.as_fd(), b"exit status: 3", T);
    assert!(s.child.try_wait().unwrap().is_none());
    write_all(s.host.master.as_fd(), b"q");
    assert_eq!(s.wait_exit(T).code(), Some(0));
    let _ = std::fs::remove_dir_all(&dir);
}
