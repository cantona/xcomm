// SPDX-FileCopyrightText: 2026 Su Kang Yin <paradyse@gmail.com>
// SPDX-License-Identifier: GPL-2.0-only

//! Hardware tests. Run with:
//!   XCOMM_HW_A=/dev/ttyUSB1 cargo test --test hw_port -- --ignored
//! (they take the port one at a time; it is opened exclusively).
//! Only /dev/ttyUSB1 may be used on the development machine: the other
//! adapters are board consoles.

use std::path::{Path, PathBuf};

use xcomm::cli::LineSettings;
use xcomm::port::{LowLatency, Port};

const ALLOWED: &str = "/dev/ttyUSB1";

/// Checks XCOMM_HW_A and returns the canonical path, which is what gets
/// opened: a by-id link could be moved to a board console between the check
/// and the open.
fn hw_port() -> PathBuf {
    let p = std::env::var_os("XCOMM_HW_A").expect("set XCOMM_HW_A=/dev/ttyUSB1");
    let p = PathBuf::from(p);
    let real = std::fs::canonicalize(&p).unwrap();
    assert_eq!(
        real,
        Path::new(ALLOWED),
        "refusing {}: only {ALLOWED} may be opened by these tests",
        p.display()
    );
    real
}

/// The port is opened exclusively, so the tests take turns.
static ONE_AT_A_TIME: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn open(baud: u32) -> (Port, std::sync::MutexGuard<'static, ()>) {
    let guard = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let s = LineSettings {
        baud,
        ..LineSettings::default()
    };
    let (port, applied) = Port::open(&hw_port(), &s, true).unwrap();
    println!("{baud}: {applied:?}");
    (port, guard)
}

#[test]
#[ignore]
fn hw_open_and_baud_readback() {
    for baud in [115_200, 921_600, 3_000_000, 250_000] {
        let (port, _guard) = open(baud);
        assert_eq!(port.read_settings().unwrap().baud, baud, "{baud}");
    }
}

#[test]
#[ignore]
fn hw_dtr_rts_driver_latch_readback() {
    // TIOCMGET reports the driver's output latch, not the measured line level.
    let (port, _guard) = open(115_200);
    for on in [false, true] {
        port.set_dtr(on).unwrap();
        port.set_rts(on).unwrap();
        let m = port.modem_lines().unwrap();
        assert_eq!((m.dtr, m.rts), (on, on), "latch readback for {on}");
    }
}

#[test]
#[ignore]
fn hw_break_and_low_latency() {
    let (port, _guard) = open(115_200);
    port.send_break(250).unwrap();
    let ll = port.set_async_low_latency().unwrap();
    assert!(matches!(ll, LowLatency::Set | LowLatency::Unsupported));
    println!("ASYNC_LOW_LATENCY: {ll:?}");
}

#[test]
#[ignore]
fn hw_rounded_rate_is_accepted_and_reported() {
    // The PL2303 runs 74880 as 74882: within tolerance, and settings() says
    // what the driver applied.
    let (port, _guard) = open(74_880);
    let applied = port.settings().baud;
    println!("74880 applied as {applied}");
    assert!(xcomm::port::baud_close_enough(74_880, applied));
    assert_eq!(port.read_settings().unwrap().baud, applied);
}

#[test]
#[ignore]
fn hw_break_and_close_do_not_hang_when_flow_control_holds_output() {
    // With RTS/CTS flow and nothing attached, CTS stays low and output is
    // held. Break and close switch flow control off, so both finish.
    let _guard = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let s = LineSettings {
        flow: xcomm::cli::Flow::RtsCts,
        ..LineSettings::default()
    };
    let (port, _) = Port::open(&hw_port(), &s, true).unwrap();
    if port.modem_lines().unwrap().cts {
        println!("CTS is high (something is attached); cannot hold output");
        return;
    }
    let _ = rustix::io::write(port.fd(), &[0x55u8; 256]);
    let mut queued: libc::c_int = 0;
    unsafe {
        libc::ioctl(
            std::os::fd::AsRawFd::as_raw_fd(&port.fd()),
            libc::TIOCOUTQ as _,
            &mut queued,
        )
    };
    println!("queued while CTS low: {queued}");
    assert!(queued > 0, "flow control did not hold output");
    let start = std::time::Instant::now();
    port.send_break(10).unwrap();
    println!("break with held output took {:?}", start.elapsed());
    assert!(start.elapsed() < std::time::Duration::from_secs(2));
    assert_eq!(
        port.read_settings().unwrap().flow,
        xcomm::cli::Flow::RtsCts,
        "flow not restored"
    );
    let _ = rustix::io::write(port.fd(), &[0x55u8; 256]);
    let start = std::time::Instant::now();
    drop(port);
    println!("drop with held output took {:?}", start.elapsed());
    assert!(start.elapsed() < std::time::Duration::from_secs(2));
}
