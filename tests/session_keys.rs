//! The prefix key and the commands after it, on a spawned xcomm.

mod support;

use std::io::Write;
use std::os::fd::AsFd;
use std::time::Duration;

use rustix::termios::{LocalModes, tcgetattr};
use support::{lock_dir, read_until, spawn_with, spawn_with_env, wait_exit, write_all};

const T: Duration = Duration::from_secs(5);

/// Starts xcomm on a fake device and returns it with its banner.
fn start(extra: &[&str], env: &[(&str, &str)]) -> (support::Pty, support::Spawned, String) {
    let dev = support::pty();
    let path = dev.slave_path.to_str().unwrap().to_string();
    let mut args: Vec<&str> = extra.to_vec();
    args.push(&path);
    let s = spawn_with_env(env!("CARGO_BIN_EXE_xcomm"), &args, &lock_dir("keys"), env);
    let banner = text(&read_until(s.host.master.as_fd(), b"\r\n", T));
    assert!(banner.contains("for help"), "{banner}");
    (dev, s, banner)
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

fn speed(dev: &support::Pty) -> u32 {
    tcgetattr(dev.slave.as_fd()).unwrap().output_speed()
}

fn quits_with(keys: &[u8], env: &[(&str, &str)]) {
    let (_dev, mut s, _) = start(&[], env);
    write_all(s.host.master.as_fd(), keys);
    assert!(wait_exit(&mut s.child, T).success());
    assert!(
        tcgetattr(s.host.slave.as_fd())
            .unwrap()
            .local_modes
            .contains(LocalModes::ICANON)
    );
}

#[test]
fn ctrl_a_x_quits() {
    quits_with(b"\x01x", &[]);
}

#[test]
fn ctrl_a_q_quits() {
    quits_with(b"\x01q", &[]);
}

#[test]
fn ctrl_a_uppercase_x_quits() {
    quits_with(b"\x01X", &[]);
}

#[test]
fn alt_a_x_quits_inside_tmux_env() {
    quits_with(b"\x1bax", &[("TMUX", "1")]);
}

#[test]
fn alt_a_csi_u_x_quits() {
    quits_with(b"\x1b[97;3ux", &[("TMUX", "1")]);
}

#[test]
fn banner_names_the_prefix_and_why() {
    let (_dev, _s, banner) = start(&[], &[]);
    assert!(banner.contains("Ctrl-A Z for help (default)"), "{banner}");
    let (_dev, _s, banner) = start(&[], &[("TMUX", "1")]);
    assert!(
        banner.contains("Alt-A Z for help (multiplexer: tmux)"),
        "{banner}"
    );
    let (_dev, _s, banner) = start(&["--prefix", "ctrl-b"], &[("TMUX", "1")]);
    assert!(
        banner.contains("Ctrl-B Z for help (command line)"),
        "{banner}"
    );
}

#[test]
fn ctrl_a_z_shows_help_with_prefix_name() {
    let (_dev, s, _) = start(&[], &[]);
    write_all(s.host.master.as_fd(), b"\x01z");
    let help = text(&read_until(s.host.master.as_fd(), b"prefix: ", T));
    for line in [
        "Ctrl-A z  help",
        "Ctrl-A x  quit",
        "Ctrl-A q  quit",
        "Ctrl-A f  send break",
        "Ctrl-A h  hang up",
        "Ctrl-A d  toggle DTR",
        "Ctrl-A t  toggle RTS",
        "Ctrl-A p  change the baud rate",
        "Ctrl-A e  toggle local echo",
        "Ctrl-A w  toggle line wrap",
        "Ctrl-A l  start or stop capture",
        "Ctrl-A Ctrl-A  send Ctrl-A itself",
        "Ctrl-A Esc  cancel",
    ] {
        assert!(help.contains(line), "{line:?} missing from {help}");
    }
}

#[test]
fn ctrl_a_ctrl_a_sends_0x01_to_device() {
    let (dev, s, _) = start(&[], &[]);
    write_all(s.host.master.as_fd(), b"\x01\x01end");
    assert_eq!(read_until(dev.master.as_fd(), b"end", T), b"\x01end");
}

#[test]
fn alt_a_alt_a_sends_esc_a_to_device() {
    let (dev, s, _) = start(&[], &[("TMUX", "1")]);
    write_all(s.host.master.as_fd(), b"\x1ba\x1baend");
    assert_eq!(read_until(dev.master.as_fd(), b"end", T), b"\x1baend");
}

#[test]
fn lone_esc_reaches_the_device_with_alt_prefix() {
    let (dev, s, _) = start(&[], &[("TMUX", "1")]);
    write_all(s.host.master.as_fd(), b"\x1b");
    assert_eq!(read_until(dev.master.as_fd(), b"\x1b", T), b"\x1b");
    write_all(s.host.master.as_fd(), b"\x1b[A");
    assert_eq!(read_until(dev.master.as_fd(), b"[A", T), b"\x1b[A");
}

#[test]
fn esc_after_the_prefix_cancels() {
    for (env, prefix) in [(&[][..], &b"\x01"[..]), (&[("TMUX", "1")][..], b"\x1ba")] {
        let (dev, s, _) = start(&[], env);
        write_all(s.host.master.as_fd(), prefix);
        write_all(s.host.master.as_fd(), b"\x1b");
        // Past --prefix-time, so the ESC is a key of its own.
        std::thread::sleep(Duration::from_millis(100));
        write_all(s.host.master.as_fd(), b"xend");
        assert_eq!(read_until(dev.master.as_fd(), b"end", T), b"xend");
    }
}

#[test]
fn ctrl_a_p_changes_baud() {
    let (dev, s, _) = start(&[], &[]);
    write_all(s.host.master.as_fd(), b"\x01p");
    read_until(s.host.master.as_fd(), b"baud [115200]: ", T);
    write_all(s.host.master.as_fd(), b"96\x7f\x7f9600\r");
    let out = text(&read_until(s.host.master.as_fd(), b"8N1\r\n", T));
    assert!(out.contains("xcomm: 9600 8N1"), "{out:?}");
    assert_eq!(speed(&dev), 9600);
    // The session goes on at the new rate.
    write_all(s.host.master.as_fd(), b"after");
    assert_eq!(read_until(dev.master.as_fd(), b"after", T), b"after");
}

#[test]
fn ctrl_a_p_esc_or_empty_keeps_the_baud() {
    let (dev, s, _) = start(&[], &[]);
    for answer in [&b"9600\x1b"[..], b"\r"] {
        write_all(s.host.master.as_fd(), b"\x01p");
        read_until(s.host.master.as_fd(), b"baud [115200]: ", T);
        write_all(s.host.master.as_fd(), answer);
        read_until(s.host.master.as_fd(), b"\r\n", T);
    }
    write_all(s.host.master.as_fd(), b"after");
    assert_eq!(read_until(dev.master.as_fd(), b"after", T), b"after");
    assert_eq!(speed(&dev), 115_200);
}

#[test]
fn the_baud_prompt_takes_ascii_only() {
    let (dev, s, _) = start(&[], &[]);
    write_all(s.host.master.as_fd(), b"\x01p");
    read_until(s.host.master.as_fd(), b"baud [115200]: ", T);
    write_all(s.host.master.as_fd(), b"96\xc3\xa900\r");
    let out = text(&read_until(s.host.master.as_fd(), b"8N1\r\n", T));
    assert!(out.contains("xcomm: 9600 8N1"), "{out:?}");
    assert_eq!(speed(&dev), 9600);
}

#[test]
fn ctrl_a_p_rejects_a_bad_baud() {
    let (dev, s, _) = start(&[], &[]);
    write_all(s.host.master.as_fd(), b"\x01p");
    read_until(s.host.master.as_fd(), b"baud [115200]: ", T);
    write_all(s.host.master.as_fd(), b"0\r");
    let out = text(&read_until(s.host.master.as_fd(), b"'0'", T));
    assert!(out.contains("invalid baud rate '0'"), "{out:?}");
    assert_eq!(speed(&dev), 115_200);
}

#[test]
fn ctrl_a_e_local_echo() {
    let (dev, s, _) = start(&[], &[]);
    write_all(s.host.master.as_fd(), b"\x01e");
    read_until(s.host.master.as_fd(), b"local echo on\r\n", T);
    write_all(s.host.master.as_fd(), b"abc");
    assert_eq!(read_until(s.host.master.as_fd(), b"abc", T), b"abc");
    assert_eq!(read_until(dev.master.as_fd(), b"abc", T), b"abc");
    write_all(s.host.master.as_fd(), b"\x01e");
    read_until(s.host.master.as_fd(), b"local echo off\r\n", T);
    // Nothing echoed now: the next terminal output is the device's.
    write_all(s.host.master.as_fd(), b"def");
    assert_eq!(read_until(dev.master.as_fd(), b"def", T), b"def");
    write_all(dev.master.as_fd(), b"!");
    assert_eq!(read_until(s.host.master.as_fd(), b"!", T), b"!");
}

#[test]
fn ctrl_a_w_toggles_line_wrap_and_restores_it_on_exit() {
    let (_dev, mut s, _) = start(&[], &[]);
    write_all(s.host.master.as_fd(), b"\x01w");
    let out = read_until(s.host.master.as_fd(), b"line wrap off\r\n", T);
    assert!(out.starts_with(b"\x18\x1b[?7l"), "{out:?}");
    write_all(s.host.master.as_fd(), b"\x01w");
    let out = read_until(s.host.master.as_fd(), b"line wrap on\r\n", T);
    assert!(out.starts_with(b"\x18\x1b[?7h"), "{out:?}");
    write_all(s.host.master.as_fd(), b"\x01w");
    read_until(s.host.master.as_fd(), b"line wrap off\r\n", T);
    write_all(s.host.master.as_fd(), b"\x01x");
    assert_eq!(
        read_until(s.host.master.as_fd(), b"\x1b[?7h", T),
        b"\x18\x1b[?7h"
    );
    assert!(wait_exit(&mut s.child, T).success());
}

#[test]
fn dash_w_turns_line_wrap_on_at_start() {
    let dev = support::pty();
    let path = dev.slave_path.to_str().unwrap().to_string();
    let s = spawn_with_env(
        env!("CARGO_BIN_EXE_xcomm"),
        &["-w", &path],
        &lock_dir("keys"),
        &[],
    );
    read_until(s.host.master.as_fd(), b"\x1b[?7h", T);
}

#[test]
fn unknown_command_letter_beeps_and_continues() {
    let (dev, s, _) = start(&[], &[]);
    write_all(s.host.master.as_fd(), b"\x01k");
    assert_eq!(read_until(s.host.master.as_fd(), b"\x07", T), b"\x18\x07");
    write_all(s.host.master.as_fd(), b"still");
    assert_eq!(read_until(dev.master.as_fd(), b"still", T), b"still");
}

#[test]
fn piped_input_passes_the_prefix_through() {
    let dev = support::pty();
    let dir = lock_dir("keys");
    let mut cmd = std::process::Command::new(env!("CARGO_BIN_EXE_xcomm"));
    cmd.arg(&dev.slave_path)
        .env("XCOMM_LOCK_DIR", &dir)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    for v in [
        "XCOMM_PREFIX",
        "HERDR_ENV",
        "HC_TERMINAL_ID",
        "TMUX",
        "STY",
        "ZELLIJ",
    ] {
        cmd.env_remove(v);
    }
    let mut child = cmd.spawn().unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"\x01x\x01zend")
        .unwrap();
    assert_eq!(read_until(dev.master.as_fd(), b"end", T), b"\x01x\x01zend");
    assert!(wait_exit(&mut child, T).success());
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn keys_typed_ahead_of_the_prompt_answer_it() {
    let (dev, s, _) = start(&[], &[]);
    // One read, as ssh or mosh can deliver it.
    write_all(s.host.master.as_fd(), b"ab\x01p9600\rcd");
    let out = text(&read_until(s.host.master.as_fd(), b"8N1\r\n", T));
    assert!(out.contains("baud [115200]: 9600\r\n"), "{out:?}");
    assert_eq!(speed(&dev), 9600);
    assert_eq!(read_until(dev.master.as_fd(), b"cd", T), b"abcd");
}

#[test]
fn echo_turned_on_applies_to_the_rest_of_the_same_read() {
    let (_dev, s, _) = start(&[], &[]);
    write_all(s.host.master.as_fd(), b"\x01eabc");
    let out = read_until(s.host.master.as_fd(), b"abc", T);
    assert!(text(&out).contains("local echo on\r\nabc"), "{out:?}");
}

#[test]
fn an_arrow_key_after_the_prefix_is_not_sent() {
    let (dev, s, _) = start(&[], &[]);
    write_all(s.host.master.as_fd(), b"\x01\x1b[A");
    assert_eq!(read_until(s.host.master.as_fd(), b"\x07", T), b"\x18\x07");
    write_all(s.host.master.as_fd(), b"end");
    assert_eq!(read_until(dev.master.as_fd(), b"end", T), b"end");
}

#[test]
fn sigterm_while_prompting_ends_the_session() {
    let (_dev, mut s, _) = start(&[], &[]);
    write_all(s.host.master.as_fd(), b"\x01p");
    read_until(s.host.master.as_fd(), b"baud [115200]: ", T);
    unsafe { libc::kill(s.child.id() as i32, libc::SIGTERM) };
    assert!(wait_exit(&mut s.child, T).success());
    assert!(
        tcgetattr(s.host.slave.as_fd())
            .unwrap()
            .local_modes
            .contains(LocalModes::ICANON)
    );
}

#[test]
fn ctrl_a_in_csi_u_form_quits() {
    quits_with(b"\x1b[97;5ux", &[]);
}

#[test]
fn keys_with_escapes_in_the_prompt_are_ignored_whole() {
    let (dev, s, _) = start(&[], &[]);
    write_all(s.host.master.as_fd(), b"\x01p");
    read_until(s.host.master.as_fd(), b"baud [115200]: ", T);
    // Left arrow, Alt-x, F1: none of them reach the line or the device.
    write_all(s.host.master.as_fd(), b"96\x1b[D\x1bx\x1bOP00\r");
    let out = text(&read_until(s.host.master.as_fd(), b"8N1\r\n", T));
    assert!(out.contains("xcomm: 9600 8N1"), "{out:?}");
    assert_eq!(speed(&dev), 9600);
    write_all(s.host.master.as_fd(), b"end");
    assert_eq!(read_until(dev.master.as_fd(), b"end", T), b"end");
}

#[test]
fn with_stdout_redirected_commands_talk_on_the_terminal() {
    let dev = support::pty();
    let path = dev.slave_path.to_str().unwrap().to_string();
    let (out_r, out_w) = std::io::pipe().unwrap();
    let mut s = spawn_with(
        env!("CARGO_BIN_EXE_xcomm"),
        &[&path],
        &lock_dir("keys"),
        &[],
        Some(out_w.into()),
    );
    read_until(s.host.master.as_fd(), b"for help", T);
    write_all(s.host.master.as_fd(), b"\x01z");
    read_until(s.host.master.as_fd(), b"prefix: ", T);
    write_all(s.host.master.as_fd(), b"\x01w");
    read_until(s.host.master.as_fd(), b"not a terminal\r\n", T);
    write_all(dev.master.as_fd(), b"data");
    assert_eq!(read_until(out_r.as_fd(), b"data", T), b"data");
    write_all(s.host.master.as_fd(), b"\x01x");
    assert!(wait_exit(&mut s.child, T).success());
    // stdout carried the device bytes and nothing else.
    let mut rest = Vec::new();
    std::io::Read::read_to_end(
        &mut std::fs::File::from(std::os::fd::OwnedFd::from(out_r)),
        &mut rest,
    )
    .unwrap();
    assert_eq!(rest, b"");
}

/// A device that takes nothing (its other end is never read) and a paste
/// larger than the queue: after a while the keys are read anyway, and quit
/// works.
#[test]
fn quit_works_while_the_device_takes_no_input() {
    let (_dev, mut s, _) = start(&[], &[]);
    let host = s.host.master.try_clone().unwrap();
    let typist = std::thread::spawn(move || {
        write_all(host.as_fd(), &vec![b'a'; 256 * 1024]);
        write_all(host.as_fd(), b"\x01x");
    });
    let out = text(&read_until(
        s.host.master.as_fd(),
        b"x quits)",
        Duration::from_secs(20),
    ));
    assert!(out.contains("typed keys are dropped"), "{out:?}");
    assert!(wait_exit(&mut s.child, Duration::from_secs(20)).success());
    typist.join().unwrap();
}

#[test]
fn quit_sends_the_keys_typed_before_it() {
    let (dev, mut s, _) = start(&[], &[]);
    write_all(s.host.master.as_fd(), b"hello\r\x01x");
    assert!(wait_exit(&mut s.child, T).success());
    assert_eq!(read_until(dev.master.as_fd(), b"\r", T), b"hello\r");
}

#[test]
fn the_prompt_takes_coded_keys() {
    let (dev, s, _) = start(&[], &[]);
    // kitty "report all keys as escape codes": 9 6 0 0 Enter.
    write_all(s.host.master.as_fd(), b"\x01p");
    read_until(s.host.master.as_fd(), b"baud [115200]: ", T);
    write_all(
        s.host.master.as_fd(),
        b"\x1b[57u\x1b[54u\x1b[48u\x1b[48u\x1b[13u",
    );
    let out = text(&read_until(s.host.master.as_fd(), b"8N1\r\n", T));
    assert!(out.contains("xcomm: 9600 8N1"), "{out:?}");
    assert_eq!(speed(&dev), 9600);
    // Ctrl-C in CSI-u form cancels.
    write_all(s.host.master.as_fd(), b"\x01p");
    read_until(s.host.master.as_fd(), b"baud [9600]: ", T);
    write_all(s.host.master.as_fd(), b"1200\x1b[99;5u");
    read_until(s.host.master.as_fd(), b"\r\n", T);
    write_all(s.host.master.as_fd(), b"end");
    assert_eq!(read_until(dev.master.as_fd(), b"end", T), b"end");
    assert_eq!(speed(&dev), 9600);
}

#[test]
fn status_lines_cancel_an_unfinished_device_sequence() {
    let (dev, s, _) = start(&[], &[]);
    write_all(dev.master.as_fd(), b"\x1b[31");
    read_until(s.host.master.as_fd(), b"\x1b[31", T);
    write_all(s.host.master.as_fd(), b"\x01e");
    let out = read_until(s.host.master.as_fd(), b"local echo on\r\n", T);
    assert!(out.starts_with(b"\x18\r\nxcomm: "), "{out:?}");
}

/// Stops output on xcomm's terminal (as Ctrl-S does with IXON): every write
/// to it blocks.
fn stall_the_terminal(s: &support::Spawned) {
    rustix::termios::tcflow(&s.host.slave, rustix::termios::Action::OOff).unwrap();
}

#[test]
fn sigterm_ends_a_session_with_wrap_off_and_the_terminal_stalled() {
    let (dev, mut s, _) = start(&[], &[]);
    write_all(s.host.master.as_fd(), b"\x01w");
    read_until(s.host.master.as_fd(), b"line wrap off\r\n", T);
    stall_the_terminal(&s);
    // Device output now blocks xcomm in its write to the terminal.
    write_all(dev.master.as_fd(), b"more");
    std::thread::sleep(Duration::from_millis(100));
    unsafe { libc::kill(s.child.id() as i32, libc::SIGTERM) };
    assert!(wait_exit(&mut s.child, T).success());
}

/// Stopped and continued with wrap off and the terminal taking nothing:
/// neither the stop nor the continue blocks on the wrap sequence, and the
/// SIGTERM sent while stopped ends the session.
#[cfg(target_os = "linux")]
#[test]
fn sigterm_sent_while_stopped_with_wrap_off_and_the_terminal_stalled_ends_the_session() {
    let (dev, mut s, _) = start(&[], &[]);
    write_all(s.host.master.as_fd(), b"\x01w");
    read_until(s.host.master.as_fd(), b"line wrap off\r\n", T);
    stall_the_terminal(&s);
    write_all(dev.master.as_fd(), b"more");
    let pid = s.child.id() as i32;
    support::wait_blocked_writing_stdout(pid, T);
    unsafe { libc::kill(pid, libc::SIGTSTP) };
    support::wait_stopped(pid, T);
    unsafe { libc::kill(pid, libc::SIGTERM) };
    unsafe { libc::kill(pid, libc::SIGCONT) };
    assert!(wait_exit(&mut s.child, T).success());
}
