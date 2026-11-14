//! Capturing device output to a file, on a spawned xcomm.

mod support;

use std::os::fd::AsFd;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use support::{lock_dir, read_until, spawn_with_env, wait_exit, write_all};

const T: Duration = Duration::from_secs(10);

/// xcomm on a fake device, with a scratch directory for capture files
/// (removed with the spawned process).
fn start(extra: &[&str], dir: &Path) -> (support::Pty, support::Spawned, String) {
    let dev = support::pty();
    let path = dev.slave_path.to_str().unwrap().to_string();
    let mut args: Vec<&str> = extra.to_vec();
    args.push(&path);
    let s = spawn_with_env(env!("CARGO_BIN_EXE_xcomm"), &args, dir, &[]);
    let banner = read_until(s.host.master.as_fd(), b"for help", T);
    (dev, s, String::from_utf8_lossy(&banner).into_owned())
}

fn quit(mut s: support::Spawned) -> support::Spawned {
    write_all(s.host.master.as_fd(), b"\x01x");
    assert!(wait_exit(&mut s.child, T).success());
    s
}

fn file_with(dir: &Path, name: &str) -> PathBuf {
    dir.join(name)
}

#[test]
fn dash_c_captures_device_output_exactly() {
    let dir = lock_dir("capture");
    let file = file_with(&dir, "boot.log");
    let (dev, s, banner) = start(&["-C", file.to_str().unwrap()], &dir);
    assert!(banner.contains("for help"));
    let mut data: Vec<u8> = (0..1 << 20).map(|i| (i * 7 % 251) as u8).collect();
    data.extend_from_slice(b"\x1b[1;31mEND");
    let master = dev.master.try_clone().unwrap();
    let sent = data.clone();
    let writer = std::thread::spawn(move || write_all(master.as_fd(), &sent));
    read_until(s.host.master.as_fd(), b"END", T);
    writer.join().unwrap();
    let s = quit(s);
    assert_eq!(std::fs::read(&file).unwrap(), data);
    drop(s);
}

#[test]
fn capture_appends_and_keeps_raw_bytes_with_color_off() {
    let dir = lock_dir("capture");
    let file = file_with(&dir, "raw.log");
    std::fs::write(&file, b"before\n").unwrap();
    let (dev, s, _) = start(&["-c", "off", "-C", file.to_str().unwrap()], &dir);
    write_all(dev.master.as_fd(), b"\x1b[31mred\x1b[0m");
    // The terminal gets the color removed (the reset stays), the capture
    // the bytes as sent.
    let shown = read_until(s.host.master.as_fd(), b"\x1b[0m", T);
    assert!(shown.ends_with(b"red\x1b[0m"), "{shown:?}");
    assert!(!shown.windows(5).any(|w| w == b"\x1b[31m"), "{shown:?}");
    let s = quit(s);
    assert_eq!(std::fs::read(&file).unwrap(), b"before\n\x1b[31mred\x1b[0m");
    drop(s);
}

#[test]
fn key_l_starts_and_stops_capture() {
    let dir = lock_dir("capture");
    let file = file_with(&dir, "between.log");
    let (dev, s, _) = start(&[], &dir);
    write_all(dev.master.as_fd(), b"one ");
    read_until(s.host.master.as_fd(), b"one ", T);
    write_all(s.host.master.as_fd(), b"\x01l");
    read_until(s.host.master.as_fd(), b"capture file [xcomm.cap]: ", T);
    write_all(s.host.master.as_fd(), file.to_str().unwrap().as_bytes());
    write_all(s.host.master.as_fd(), b"\r");
    read_until(s.host.master.as_fd(), b"capturing to", T);
    write_all(dev.master.as_fd(), b"two ");
    read_until(s.host.master.as_fd(), b"two ", T);
    write_all(s.host.master.as_fd(), b"\x01l");
    let out = read_until(s.host.master.as_fd(), b"bytes)", T);
    let out = String::from_utf8_lossy(&out);
    assert!(
        out.contains("capture closed:") && out.contains("(4 bytes)"),
        "{out}"
    );
    write_all(dev.master.as_fd(), b"three");
    read_until(s.host.master.as_fd(), b"three", T);
    let s = quit(s);
    assert_eq!(std::fs::read(&file).unwrap(), b"two ");
    drop(s);
}

#[test]
fn a_capture_that_cannot_be_written_stops_and_the_session_goes_on() {
    let dir = lock_dir("capture");
    let (dev, s, _) = start(&["-C", "/dev/full"], &dir);
    write_all(dev.master.as_fd(), b"data");
    let out = read_until(s.host.master.as_fd(), b"capture stopped: /dev/full", T);
    assert!(String::from_utf8_lossy(&out).contains("data"));
    write_all(dev.master.as_fd(), b"more");
    read_until(s.host.master.as_fd(), b"more", T);
    write_all(s.host.master.as_fd(), b"keys");
    assert_eq!(read_until(dev.master.as_fd(), b"keys", T), b"keys");
    drop(quit(s));
}

#[test]
fn captured_output_is_written_soon_after_it_arrives() {
    let dir = lock_dir("capture");
    let file = file_with(&dir, "idle.log");
    let (dev, s, _) = start(&["-C", file.to_str().unwrap()], &dir);
    write_all(dev.master.as_fd(), b"prompt> ");
    read_until(s.host.master.as_fd(), b"prompt> ", T);
    let started = Instant::now();
    while std::fs::read(&file).unwrap() != b"prompt> " {
        assert!(
            started.elapsed() < Duration::from_millis(200),
            "not written within 200 ms"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    drop(quit(s));
}

#[test]
fn a_dash_c_file_that_cannot_be_opened_ends_the_run() {
    let dir = lock_dir("capture");
    let dev = support::pty();
    let path = dev.slave_path.to_str().unwrap().to_string();
    let mut s = spawn_with_env(
        env!("CARGO_BIN_EXE_xcomm"),
        &["-C", "/nonexistent-dir/x.log", &path],
        &dir,
        &[],
    );
    let out = read_until(s.host.master.as_fd(), b"\n", T);
    let out = String::from_utf8_lossy(&out);
    assert!(out.contains("-C /nonexistent-dir/x.log:"), "{out}");
    assert_eq!(wait_exit(&mut s.child, T).code(), Some(1));
}

fn mkfifo(path: &Path) {
    let c = std::ffi::CString::new(path.to_str().unwrap()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
}

#[test]
fn a_fifo_with_no_reader_fails_at_once() {
    let dir = lock_dir("capture");
    let fifo = file_with(&dir, "fifo");
    mkfifo(&fifo);
    let dev = support::pty();
    let path = dev.slave_path.to_str().unwrap().to_string();
    let mut s = spawn_with_env(
        env!("CARGO_BIN_EXE_xcomm"),
        &["-C", fifo.to_str().unwrap(), &path],
        &dir,
        &[],
    );
    let out = read_until(s.host.master.as_fd(), b"\n", T);
    assert!(String::from_utf8_lossy(&out).contains("fifo:"), "{out:?}");
    assert_eq!(
        wait_exit(&mut s.child, Duration::from_secs(2)).code(),
        Some(1)
    );
}

#[test]
fn a_fifo_that_stops_reading_stops_the_capture_not_the_session() {
    let dir = lock_dir("capture");
    let fifo = file_with(&dir, "fifo");
    mkfifo(&fifo);
    // A reader that never reads.
    let _reader = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(&fifo)
        .unwrap();
    let (dev, mut s, _) = start(&["-C", fifo.to_str().unwrap()], &dir);
    // Device output until told to stop; the terminal side is read below
    // only until the notice, so writes must not block.
    let master = dev.master.try_clone().unwrap();
    let fl = rustix::fs::fcntl_getfl(&master).unwrap();
    rustix::fs::fcntl_setfl(&master, fl | rustix::fs::OFlags::NONBLOCK).unwrap();
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let flag = stop.clone();
    let writer = std::thread::spawn(move || {
        while !flag.load(std::sync::atomic::Ordering::Relaxed) {
            if rustix::io::write(&master, &[b'.'; 4096]).is_err() {
                std::thread::sleep(Duration::from_millis(1));
            }
        }
    });
    read_until(s.host.master.as_fd(), b"capture stopped:", T);
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    writer.join().unwrap();
    unsafe { libc::kill(s.child.id() as i32, libc::SIGTERM) };
    assert!(wait_exit(&mut s.child, T).success());
}

#[test]
fn a_command_writes_out_the_capture_first() {
    let dir = lock_dir("capture");
    let file = file_with(&dir, "prompt.log");
    let (dev, s, _) = start(&["-C", file.to_str().unwrap()], &dir);
    // Device output and a prompt opening at once: the loop then waits on
    // the prompt, so only the command itself can write the capture out.
    write_all(dev.master.as_fd(), b"abc");
    read_until(s.host.master.as_fd(), b"abc", T);
    write_all(s.host.master.as_fd(), b"\x01p");
    read_until(s.host.master.as_fd(), b"baud [", T);
    let started = Instant::now();
    while std::fs::read(&file).unwrap() != b"abc" {
        assert!(started.elapsed() < Duration::from_millis(200));
        std::thread::sleep(Duration::from_millis(5));
    }
    write_all(s.host.master.as_fd(), b"\x03");
    read_until(s.host.master.as_fd(), b"\r\n", T);
    drop(quit(s));
}

#[test]
fn the_file_prompt_takes_utf8_names_whole() {
    let dir = lock_dir("capture");
    let (dev, s, _) = start(&[], &dir);
    columns(&s, 200);
    write_all(s.host.master.as_fd(), b"\x01l");
    read_until(s.host.master.as_fd(), b"capture file [xcomm.cap]: ", T);
    write_all(s.host.master.as_fd(), dir.to_str().unwrap().as_bytes());
    // "/café中", Backspace (the whole wide character), ".log", Enter.
    write_all(s.host.master.as_fd(), b"/caf\xc3\xa9\xe4\xb8\xad\x7f.log\r");
    let out = read_until(s.host.master.as_fd(), b"capturing to", T);
    // The row is drawn again without it.
    let redraw = [
        &b"\rcapture file [xcomm.cap]: "[..],
        dir.to_str().unwrap().as_bytes(),
        b"/caf\xc3\xa9\x1b[K",
    ]
    .concat();
    assert!(out.windows(redraw.len()).any(|w| w == redraw), "{out:?}");
    write_all(dev.master.as_fd(), b"x");
    read_until(s.host.master.as_fd(), b"x", T);
    let s = quit(s);
    assert_eq!(std::fs::read(dir.join("café.log")).unwrap(), b"x");
    drop(s);
}

#[test]
fn a_port_that_cannot_be_had_leaves_no_capture_file() {
    let dir = lock_dir("capture");
    let file = file_with(&dir, "never.log");
    let mut s = spawn_with_env(
        env!("CARGO_BIN_EXE_xcomm"),
        &["-C", file.to_str().unwrap(), "/dev/xcomm-no-such-device"],
        &dir,
        &[],
    );
    assert_ne!(wait_exit(&mut s.child, T).code(), Some(0));
    assert!(!file.exists());
}

#[test]
fn a_pipe_reader_briefly_behind_gets_every_byte() {
    let dir = lock_dir("capture");
    let fifo = file_with(&dir, "fifo");
    mkfifo(&fifo);
    let reader = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(&fifo)
        .unwrap();
    let (dev, s, _) = start(&["-C", fifo.to_str().unwrap()], &dir);
    // More than the pipe holds, less than the pipe and the buffer together;
    // the reader starts late.
    let mut data: Vec<u8> = (0..100_000).map(|i| b'a' + (i % 26) as u8).collect();
    data.extend_from_slice(b"<END>");
    let master = dev.master.try_clone().unwrap();
    let sent = data.clone();
    let writer = std::thread::spawn(move || write_all(master.as_fd(), &sent));
    let collector = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(300));
        let fl = rustix::fs::fcntl_getfl(&reader).unwrap();
        rustix::fs::fcntl_setfl(&reader, fl - rustix::fs::OFlags::NONBLOCK).unwrap();
        let mut got = Vec::new();
        std::io::Read::read_to_end(&mut &reader, &mut got).unwrap();
        got
    });
    let shown = read_until(s.host.master.as_fd(), b"<END>", T);
    writer.join().unwrap();
    std::thread::sleep(Duration::from_millis(400));
    let s = quit(s);
    assert!(!String::from_utf8_lossy(&shown).contains("capture stopped"));
    assert_eq!(collector.join().unwrap(), data);
    drop(s);
}

#[test]
fn the_file_prompt_keeps_to_its_limit() {
    let dir = lock_dir("capture");
    let (_dev, s, _) = start(&[], &dir);
    write_all(s.host.master.as_fd(), b"\x01l");
    read_until(s.host.master.as_fd(), b"capture file [xcomm.cap]: ", T);
    // 1020 bytes, then a lead byte with fifty continuation bytes: only the
    // two bytes of one character are taken.
    let mut keys = vec![b'a'; 1020];
    keys.push(0xc3);
    keys.extend(std::iter::repeat_n(0xa9, 50));
    keys.push(b'\r');
    write_all(s.host.master.as_fd(), &keys);
    let out = read_until(s.host.master.as_fd(), b"\r\n", T);
    // The read may run on into the error line that follows.
    let end = out.windows(2).position(|w| w == b"\r\n").unwrap();
    assert_eq!(end, 1022, "{:?}", &out[1015..end + 2]);
    assert_eq!(&out[1018..end], b"aa\xc3\xa9");
    drop(quit(s));
}

#[test]
fn quitting_waits_briefly_for_a_pipe_reader_behind() {
    let dir = lock_dir("capture");
    let fifo = file_with(&dir, "fifo");
    mkfifo(&fifo);
    let reader = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(&fifo)
        .unwrap();
    let (dev, mut s, _) = start(&["-C", fifo.to_str().unwrap()], &dir);
    let mut data: Vec<u8> = (0..100_000).map(|i| b'a' + (i % 26) as u8).collect();
    data.extend_from_slice(b"<END>");
    let master = dev.master.try_clone().unwrap();
    let sent = data.clone();
    let writer = std::thread::spawn(move || write_all(master.as_fd(), &sent));
    read_until(s.host.master.as_fd(), b"<END>", T);
    writer.join().unwrap();
    // Quit with the pipe full; the reader starts only afterwards.
    write_all(s.host.master.as_fd(), b"\x01x");
    std::thread::sleep(Duration::from_millis(300));
    let fl = rustix::fs::fcntl_getfl(&reader).unwrap();
    rustix::fs::fcntl_setfl(&reader, fl - rustix::fs::OFlags::NONBLOCK).unwrap();
    let mut got = Vec::new();
    std::io::Read::read_to_end(&mut &reader, &mut got).unwrap();
    assert!(wait_exit(&mut s.child, T).success());
    assert_eq!(got, data);
}

fn columns(s: &support::Spawned, cols: u16) {
    rustix::termios::tcsetwinsize(
        &s.host.master,
        rustix::termios::Winsize {
            ws_row: 24,
            ws_col: cols,
            ws_xpixel: 0,
            ws_ypixel: 0,
        },
    )
    .unwrap();
}

#[test]
fn backspace_on_a_wrapped_answer_steps_back_instead_of_redrawing() {
    let dir = lock_dir("capture");
    let (_dev, s, _) = start(&[], &dir);
    columns(&s, 40);
    write_all(s.host.master.as_fd(), b"\x01l");
    read_until(s.host.master.as_fd(), b"capture file [xcomm.cap]: ", T);
    write_all(s.host.master.as_fd(), &[b'a'; 30]);
    write_all(s.host.master.as_fd(), b"\x7fZ");
    let out = read_until(s.host.master.as_fd(), b"Z", T);
    assert!(out.ends_with(b"a\x08 \x08Z"), "{out:?}");
    write_all(s.host.master.as_fd(), b"\x03");
    read_until(s.host.master.as_fd(), b"\r\n", T);
    drop(quit(s));
}

#[test]
fn a_character_cut_short_never_reaches_the_screen() {
    let dir = lock_dir("capture");
    let (_dev, s, _) = start(&[], &dir);
    columns(&s, 200);
    write_all(s.host.master.as_fd(), b"\x01l");
    read_until(s.host.master.as_fd(), b"capture file [xcomm.cap]: ", T);
    // Two bytes of a three-byte character, then `a`; then two more and
    // Backspace, which drops only the partial character.
    write_all(s.host.master.as_fd(), b"x\xe4\xb8a\xe4\xb8\x7fZ");
    let out = read_until(s.host.master.as_fd(), b"Z", T);
    assert!(out.ends_with(b"xaZ"), "{out:?}");
    write_all(s.host.master.as_fd(), b"\x03");
    read_until(s.host.master.as_fd(), b"\r\n", T);
    drop(quit(s));
}

#[test]
fn the_file_prompt_drops_invalid_utf8_and_takes_coded_characters() {
    let dir = lock_dir("capture");
    let (dev, s, _) = start(&[], &dir);
    columns(&s, 200);
    write_all(s.host.master.as_fd(), b"\x01l");
    read_until(s.host.master.as_fd(), b"capture file [xcomm.cap]: ", T);
    write_all(s.host.master.as_fd(), dir.to_str().unwrap().as_bytes());
    // A surrogate (ED A0 80) and an overlong form (E0 80 80) are dropped;
    // é sent coded (kitty "report all keys as escape codes") is taken.
    write_all(
        s.host.master.as_fd(),
        b"/k\xed\xa0\x80\xe0\x80\x80\x1b[233u.log\r",
    );
    read_until(s.host.master.as_fd(), b"capturing to", T);
    write_all(dev.master.as_fd(), b"y");
    read_until(s.host.master.as_fd(), b"y", T);
    let s = quit(s);
    assert_eq!(std::fs::read(dir.join("ké.log")).unwrap(), b"y");
    drop(s);
}

#[test]
fn backspace_redraws_a_short_line_ending_in_a_wide_character() {
    let dir = lock_dir("capture");
    let (_dev, s, _) = start(&[], &dir);
    columns(&s, 80);
    write_all(s.host.master.as_fd(), b"\x01l");
    read_until(s.host.master.as_fd(), b"capture file [xcomm.cap]: ", T);
    // 27 cells of answer plus one wide character: short of 80 columns.
    let mut keys = vec![b'a'; 27];
    keys.extend_from_slice("日".as_bytes());
    keys.push(0x7f);
    write_all(s.host.master.as_fd(), &keys);
    let out = read_until(s.host.master.as_fd(), b"\x1b[K", T);
    assert!(
        out.ends_with(
            &[
                b"\r".as_slice(),
                b"capture file [xcomm.cap]: ",
                &[b'a'; 27],
                b"\x1b[K"
            ]
            .concat()
        ),
        "{out:?}"
    );
    write_all(s.host.master.as_fd(), b"\x03");
    read_until(s.host.master.as_fd(), b"\r\n", T);
    drop(quit(s));
}
