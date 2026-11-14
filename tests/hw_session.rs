//! Hardware tests of the modem-line commands. Run with:
//!   XCOMM_HW_A=/dev/ttyUSB1 cargo test --test hw_session -- --ignored
//! Only /dev/ttyUSB1 may be used on the development machine: the other
//! adapters are board consoles. The session holds the port exclusively, so
//! its own readback (the driver's output latch, from TIOCMGET) is what the
//! test sees, not the measured line level.

mod support;

use std::os::fd::AsFd;
use std::path::{Path, PathBuf};
use std::time::Duration;

use support::{lock_dir, read_until, spawn_xcomm, wait_exit, write_all};

const ALLOWED: &str = "/dev/ttyUSB1";
const T: Duration = Duration::from_secs(5);

/// The canonical path, which is what gets opened: a by-id link could be
/// moved to a board console between the check and the open.
fn hw_port() -> PathBuf {
    let p = std::env::var_os("XCOMM_HW_A").expect("set XCOMM_HW_A=/dev/ttyUSB1");
    let real = std::fs::canonicalize(PathBuf::from(&p)).unwrap();
    assert_eq!(
        real,
        Path::new(ALLOWED),
        "refusing {}: only {ALLOWED} may be opened by these tests",
        Path::new(&p).display()
    );
    real
}

fn command(s: &support::Spawned, key: u8, reply: &str) -> String {
    write_all(s.host.master.as_fd(), &[0x01, key]);
    let out = read_until(s.host.master.as_fd(), reply.as_bytes(), T);
    String::from_utf8_lossy(&out).into_owned()
}

#[test]
#[ignore]
fn hw_modem_line_commands() {
    let port = hw_port();
    let s = spawn_xcomm(&[port.to_str().unwrap()], &lock_dir("hw-session"));
    read_until(s.host.master.as_fd(), b"for help", T);
    // Opening raised DTR and RTS, so each toggle turns its line off first.
    command(&s, b'd', "xcomm: DTR off\r\n");
    command(&s, b'd', "xcomm: DTR on\r\n");
    command(&s, b't', "xcomm: RTS off\r\n");
    command(&s, b't', "xcomm: RTS on\r\n");
    command(&s, b'f', "xcomm: break sent\r\n");
    command(&s, b'h', "xcomm: hung up (DTR dropped for 500 ms)\r\n");
    let mut s = s;
    write_all(s.host.master.as_fd(), b"\x01x");
    assert!(wait_exit(&mut s.child, T).success());
}
