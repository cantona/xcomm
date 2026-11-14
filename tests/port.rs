mod support;

use std::os::fd::AsFd;
use std::path::Path;
use std::time::Duration;

use rustix::termios::{ControlModes, tcgetattr};
use xcomm::cli::{DataBits, Flow, LineSettings, Parity, StopBits};
use xcomm::port::{Applied, LowLatency, Port, describe};

fn open(p: &support::Pty, s: &LineSettings) -> Port {
    let (port, applied) = Port::open(&p.slave_path, s, false).unwrap();
    assert_eq!(applied, Applied::Exact, "{s:?}");
    port
}

#[test]
fn open_configures_raw_8n1_and_custom_baud() {
    let p = support::pty();
    #[cfg(target_os = "linux")]
    let baud = 250_000;
    #[cfg(not(target_os = "linux"))]
    let baud = 230_400;
    let s = LineSettings {
        baud,
        ..LineSettings::default()
    };
    let port = open(&p, &s);
    assert_eq!(port.read_settings().unwrap(), s);
    assert_eq!(port.settings(), s);
    let t = tcgetattr(port.fd()).unwrap();
    assert!(
        t.control_modes
            .contains(ControlModes::CREAD | ControlModes::CLOCAL)
    );
}

#[test]
fn every_line_format_is_applied_or_reported_as_different() {
    let p = support::pty();
    let port = open(&p, &LineSettings::default());
    let mut exact = 0;
    for data_bits in [
        DataBits::Five,
        DataBits::Six,
        DataBits::Seven,
        DataBits::Eight,
    ] {
        for parity in [Parity::None, Parity::Even, Parity::Odd] {
            for stop_bits in [StopBits::One, StopBits::Two] {
                for flow in [Flow::None, Flow::RtsCts, Flow::XonXoff] {
                    let s = LineSettings {
                        baud: 9600,
                        data_bits,
                        parity,
                        stop_bits,
                        flow,
                    };
                    match port.configure(&s).unwrap() {
                        Applied::Exact => {
                            exact += 1;
                            assert_eq!(port.read_settings().unwrap(), s);
                            assert_eq!(port.settings(), s);
                        }
                        Applied::Different(applied) => {
                            assert_ne!(applied, s);
                            assert_eq!(port.read_settings().unwrap(), applied);
                            assert_eq!(port.settings(), applied, "settings() must be the truth");
                        }
                    }
                }
            }
        }
    }
    // The Linux pty driver forces CS8 and clears PARENB: only 8N formats
    // (2 stop-bit and 3 flow choices) apply as asked.
    #[cfg(target_os = "linux")]
    assert_eq!(exact, 6);
    #[cfg(not(target_os = "linux"))]
    let _ = exact;
}

#[test]
fn standard_and_odd_bauds_round_trip() {
    let p = support::pty();
    let port = open(&p, &LineSettings::default());
    for baud in [
        50, 300, 9600, 57_600, 115_200, 230_400, 460_800, 921_600, 1_000_000, 3_000_000, 31_250,
        74_880,
    ] {
        let s = LineSettings {
            baud,
            ..LineSettings::default()
        };
        assert_eq!(port.configure(&s).unwrap(), Applied::Exact, "{baud}");
        assert_eq!(port.read_settings().unwrap().baud, baud);
    }
}

#[test]
fn open_missing_path_names_the_path() {
    let e = Port::open(
        Path::new("/dev/xcomm-does-not-exist"),
        &LineSettings::default(),
        false,
    )
    .err()
    .unwrap()
    .to_string();
    assert!(e.contains("/dev/xcomm-does-not-exist"), "{e}");
}

#[test]
fn open_non_tty_is_an_error() {
    let e = Port::open(Path::new("/dev/null"), &LineSettings::default(), false)
        .err()
        .unwrap()
        .to_string();
    assert!(e.contains("/dev/null"), "{e}");
}

#[test]
fn port_is_exclusive() {
    let p = support::pty();
    let _a = open(&p, &LineSettings::default());
    let e = Port::open(&p.slave_path, &LineSettings::default(), false)
        .err()
        .unwrap()
        .to_string();
    assert!(e.contains(&p.slave_path.display().to_string()), "{e}");
}

#[test]
fn tty_name_comes_from_the_resolved_path() {
    let p = support::pty();
    let dir = std::env::temp_dir().join(format!("xcomm-port-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let link = dir.join("by-id-link");
    let _ = std::fs::remove_file(&link);
    std::os::unix::fs::symlink(&p.slave_path, &link).unwrap();
    let (port, _) = Port::open(&link, &LineSettings::default(), false).unwrap();
    let expected = p.slave_path.file_name().unwrap().to_str().unwrap();
    assert_eq!(port.tty_name(), expected);
    assert_eq!(port.path(), link.as_path());
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn bytes_flow_both_ways_unchanged() {
    let p = support::pty();
    let port = open(&p, &LineSettings::default());
    let all: Vec<u8> = (0u8..=255).collect();
    support::write_all(p.master.as_fd(), &all);
    let got = support::read_until(port.fd(), &[255], Duration::from_secs(2));
    assert_eq!(got, all);
    support::write_all(port.fd(), &all);
    let back = support::read_until(p.master.as_fd(), &[255], Duration::from_secs(2));
    assert_eq!(back, all);
}

#[test]
fn port_fd_is_non_blocking_and_can_switch() {
    let p = support::pty();
    let port = open(&p, &LineSettings::default());
    let mut b = [0u8; 1];
    assert_eq!(
        rustix::io::read(port.fd(), &mut b).err(),
        Some(rustix::io::Errno::AGAIN)
    );
    port.set_blocking(true).unwrap();
    let fl = rustix::fs::fcntl_getfl(port.fd()).unwrap();
    assert!(!fl.contains(rustix::fs::OFlags::NONBLOCK));
    port.set_blocking(false).unwrap();
    let fl = rustix::fs::fcntl_getfl(port.fd()).unwrap();
    assert!(fl.contains(rustix::fs::OFlags::NONBLOCK));
}

#[test]
fn no_init_clears_hupcl_and_default_keeps_it() {
    let p = support::pty();
    // Also proves the exclusive lock is released on drop while the test
    // still holds the slave open.
    let (kept, _) = Port::open(&p.slave_path, &LineSettings::default(), true).unwrap();
    assert!(
        tcgetattr(kept.fd())
            .unwrap()
            .control_modes
            .contains(ControlModes::HUPCL)
    );
    drop(kept);
    let (left, _) = Port::open(&p.slave_path, &LineSettings::default(), false).unwrap();
    assert!(
        !tcgetattr(left.fd())
            .unwrap()
            .control_modes
            .contains(ControlModes::HUPCL)
    );
}

#[test]
fn modem_lines_on_a_pty_are_reported_unsupported() {
    let p = support::pty();
    let port = open(&p, &LineSettings::default());
    assert!(port.modem_lines().is_err());
    assert!(port.set_dtr(true).is_err());
}

#[test]
fn async_low_latency_on_pty_is_unsupported_not_an_error() {
    let p = support::pty();
    let port = open(&p, &LineSettings::default());
    assert_eq!(
        port.set_async_low_latency().unwrap(),
        LowLatency::Unsupported
    );
}

#[test]
fn ftdi_latency_timer_error_names_the_sysfs_path() {
    let p = support::pty();
    let port = open(&p, &LineSettings::default());
    let e = port.set_ftdi_latency_timer(1).err().unwrap().to_string();
    assert!(e.contains("/sys/dev/char/"), "{e}");
    assert!(e.contains("latency_timer"), "{e}");
}

#[test]
fn describe_formats_settings() {
    assert_eq!(describe(&LineSettings::default()), "115200 8N1");
    let s = LineSettings {
        baud: 9600,
        data_bits: DataBits::Seven,
        parity: Parity::Even,
        stop_bits: StopBits::Two,
        flow: Flow::RtsCts,
    };
    assert_eq!(describe(&s), "9600 7E2 rts/cts");
}

fn dirty(p: &support::Pty) {
    use rustix::termios::{InputModes, OptionalActions, SpecialCodeIndex, tcsetattr};
    let mut t = tcgetattr(p.slave.as_fd()).unwrap();
    #[cfg(target_os = "linux")]
    t.control_modes.insert(ControlModes::CMSPAR);
    t.input_modes
        .insert(InputModes::INPCK | InputModes::IGNPAR | InputModes::ISTRIP | InputModes::PARMRK);
    t.special_codes[SpecialCodeIndex::VSTOP] = 0;
    tcsetattr(p.slave.as_fd(), OptionalActions::Now, &t).unwrap();
}

#[test]
fn leftovers_from_an_earlier_user_do_not_survive() {
    use rustix::termios::{InputModes, SpecialCodeIndex};
    let p = support::pty();
    dirty(&p);
    let s = LineSettings {
        flow: Flow::XonXoff,
        ..LineSettings::default()
    };
    let port = open(&p, &s);
    let t = tcgetattr(port.fd()).unwrap();
    #[cfg(target_os = "linux")]
    assert!(!t.control_modes.contains(ControlModes::CMSPAR));
    assert!(!t.input_modes.intersects(
        InputModes::INPCK | InputModes::IGNPAR | InputModes::ISTRIP | InputModes::PARMRK
    ));
    assert_eq!(t.input_modes, InputModes::IXON | InputModes::IXOFF);
    assert_eq!(t.special_codes[SpecialCodeIndex::VSTART], 0x11);
    assert_eq!(t.special_codes[SpecialCodeIndex::VSTOP], 0x13);
}

#[test]
fn baud_tolerance_is_one_percent() {
    use xcomm::port::baud_close_enough;
    assert!(baud_close_enough(74_880, 75_000));
    assert!(baud_close_enough(115_200, 115_200));
    assert!(baud_close_enough(100_000, 101_000));
    assert!(!baud_close_enough(100_000, 101_001));
    assert!(!baud_close_enough(9_600, 4_800));
    assert!(!baud_close_enough(250_000, 38_400));
}

#[test]
fn baud_zero_is_refused_and_leaves_the_port_alone() {
    let p = support::pty();
    let port = open(&p, &LineSettings::default());
    let s = LineSettings {
        baud: 0,
        ..LineSettings::default()
    };
    let e = port.configure(&s).err().unwrap().to_string();
    assert!(e.contains("baud rate 0"), "{e}");
    assert_eq!(port.settings(), LineSettings::default());
    assert_eq!(port.read_settings().unwrap(), LineSettings::default());
}

#[test]
fn a_port_already_held_exclusively_is_refused() {
    // As non-root the open itself fails with EBUSY; as root the TIOCGEXCL
    // check refuses it. Either way the holder keeps its exclusivity.
    let p = support::pty();
    let holder = open(&p, &LineSettings::default());
    assert!(Port::open(&p.slave_path, &LineSettings::default(), false).is_err());
    #[cfg(target_os = "linux")]
    {
        let mut excl: libc::c_int = 0;
        unsafe {
            libc::ioctl(
                std::os::fd::AsRawFd::as_raw_fd(&holder.fd()),
                libc::TIOCGEXCL as _,
                &mut excl,
            )
        };
        assert_ne!(excl, 0, "holder lost exclusivity");
    }
    drop(holder);
}

#[test]
fn device_flocked_by_another_program_is_refused() {
    // picocom built with USE_FLOCK only flocks the device.
    let p = support::pty();
    let other = std::fs::File::open(&p.slave_path).unwrap();
    rustix::fs::flock(&other, rustix::fs::FlockOperation::NonBlockingLockExclusive).unwrap();
    let e = Port::open(&p.slave_path, &LineSettings::default(), false)
        .err()
        .unwrap()
        .to_string();
    assert!(e.contains("flock"), "{e}");
    drop(other);
    // Opened before the Port: TIOCEXCL refuses opens after it.
    let probe = std::fs::File::open(&p.slave_path).unwrap();
    let (port, _) = Port::open(&p.slave_path, &LineSettings::default(), false).unwrap();
    // And xcomm holds one too, so such a program is kept off.
    assert!(
        rustix::fs::flock(&probe, rustix::fs::FlockOperation::NonBlockingLockExclusive).is_err()
    );
    drop(port);
}
