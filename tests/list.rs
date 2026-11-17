// SPDX-FileCopyrightText: 2026 Su Kang Yin <paradyse@gmail.com>
// SPDX-License-Identifier: GPL-2.0-only

mod support;

use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::{DirBuilderExt, symlink};
use std::os::unix::net::UnixListener;
use std::path::PathBuf;

use xcomm::list::{PortInfo, enumerate_in, natural_cmp, table};

/// A fake root: /dev nodes are plain files (nothing is opened), sysfs links
/// relative as the kernel makes them.
fn tree() -> PathBuf {
    let root = support::lock_dir("list-root");
    let dev = root.join("dev");
    std::fs::create_dir_all(dev.join("serial/by-id")).unwrap();
    for name in [
        "ttyUSB0",
        "ttyUSB2",
        "ttyUSB10",
        "ttyACM0",
        "ttyS0",
        "ttyUSB",
        "cu.usbserial-1410",
    ] {
        std::fs::write(dev.join(name), b"").unwrap();
    }
    symlink(
        "../../ttyUSB0",
        dev.join("serial/by-id/usb-FTDI_X-if00-port0"),
    )
    .unwrap();
    // A link to nothing that exists any more, and one out of /dev.
    symlink("../../ttyUSB7", dev.join("serial/by-id/usb-gone-if00")).unwrap();
    symlink("/etc/passwd", dev.join("serial/by-id/usb-odd-if00")).unwrap();
    let usb = root.join("sys/devices/usb1/1-1");
    let iface = usb.join("1-1:1.0/ttyUSB0");
    std::fs::create_dir_all(&iface).unwrap();
    for (name, value) in [
        ("idVendor", "0403\n"),
        ("idProduct", "6001\n"),
        ("product", "FT232R USB UART\n"),
        ("serial", "A1B2\n"),
    ] {
        std::fs::write(usb.join(name), value).unwrap();
    }
    let class = root.join("sys/class/tty");
    std::fs::create_dir_all(class.join("ttyUSB0")).unwrap();
    symlink(
        "../../../devices/usb1/1-1/1-1:1.0/ttyUSB0",
        class.join("ttyUSB0/device"),
    )
    .unwrap();
    root
}

fn env_with<'a>(vars: &'a [(&'a str, String)]) -> impl Fn(&str) -> Option<String> + 'a {
    move |name: &str| {
        vars.iter()
            .find(|(k, _)| *k == name)
            .map(|(_, v)| v.clone())
    }
}

fn find<'a>(ports: &'a [PortInfo], tty: &str) -> &'a PortInfo {
    ports.iter().find(|p| p.tty == tty).expect(tty)
}

#[test]
fn ports_have_their_usb_identity_and_by_id_name() {
    let root = tree();
    let locks = support::lock_dir("list-locks");
    let vars = [("XCOMM_LOCK_DIR", locks.to_str().unwrap().to_string())];
    let ports = enumerate_in(&root, &env_with(&vars)).unwrap();
    let names: Vec<&str> = ports.iter().map(|p| p.tty.as_str()).collect();
    assert_eq!(
        names,
        [
            "cu.usbserial-1410",
            "ttyACM0",
            "ttyUSB0",
            "ttyUSB2",
            "ttyUSB10"
        ]
    );
    let p = find(&ports, "ttyUSB0");
    assert_eq!(p.path, "/dev/ttyUSB0");
    assert_eq!(
        p.by_id.as_deref(),
        Some("/dev/serial/by-id/usb-FTDI_X-if00-port0")
    );
    assert_eq!(p.vid_pid.as_deref(), Some("0403:6001"));
    assert_eq!(p.product.as_deref(), Some("FT232R USB UART"));
    assert_eq!(p.serial.as_deref(), Some("A1B2"));
    assert_eq!((p.lock_pid, &p.lock_error), (None, &None));
    let p = find(&ports, "ttyUSB2");
    assert_eq!((&p.by_id, &p.vid_pid, &p.product), (&None, &None, &None));
    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_dir_all(&locks);
}

#[test]
fn the_lock_holder_and_herdr_pane_are_shown() {
    let root = tree();
    let locks = support::lock_dir("list-locks");
    let (lock, _) = xcomm::lock::acquire(&locks, "ttyUSB0").unwrap();
    // A session socket that answers status with its pane.
    let runtime = support::lock_dir("list-run");
    std::fs::DirBuilder::new()
        .mode(0o700)
        .create(runtime.join("xcomm"))
        .unwrap();
    let listener = UnixListener::bind(runtime.join("xcomm/ttyUSB0.sock")).unwrap();
    let session = std::thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        let mut line = String::new();
        BufReader::new(&stream).read_line(&mut line).unwrap();
        assert!(line.contains("\"status\""), "{line}");
        (&stream)
            .write_all(
                b"{\"ok\":true,\"connected\":true,\"tty\":\"ttyUSB0\",\"herdr_pane\":\"w1:p7\"}\n",
            )
            .unwrap();
    });
    let vars = [
        ("XCOMM_LOCK_DIR", locks.to_str().unwrap().to_string()),
        ("XDG_RUNTIME_DIR", runtime.to_str().unwrap().to_string()),
    ];
    let ports = enumerate_in(&root, &env_with(&vars)).unwrap();
    session.join().unwrap();
    let p = find(&ports, "ttyUSB0");
    assert_eq!(p.lock_pid, Some(std::process::id() as i32));
    assert_eq!(p.herdr_pane.as_deref(), Some("w1:p7"));
    assert_eq!(find(&ports, "ttyUSB2").herdr_pane, None);
    let text = table(&ports);
    let row = text.lines().find(|l| l.starts_with("ttyUSB0 ")).unwrap();
    assert!(row.contains("locked by"), "{row}");
    assert!(row.contains("pane w1:p7"), "{row}");
    assert!(text.starts_with("TTY "), "{text}");
    lock.release().unwrap();
    for d in [root, locks, runtime] {
        let _ = std::fs::remove_dir_all(d);
    }
}

#[test]
fn natural_order_counts_numbers() {
    let mut v = vec!["ttyUSB10", "ttyUSB2", "ttyACM1", "ttyUSB02", "ttyUSB1", "a"];
    v.sort_by(|a, b| natural_cmp(a, b));
    assert_eq!(
        v,
        ["a", "ttyACM1", "ttyUSB1", "ttyUSB2", "ttyUSB02", "ttyUSB10"]
    );
}

#[test]
fn a_root_without_dev_is_an_error() {
    let root = support::lock_dir("list-empty");
    assert!(enumerate_in(&root, &|_| None).is_err());
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn list_on_this_machine_prints_json() {
    // Lists only: nothing it finds is opened.
    let o = std::process::Command::new(env!("CARGO_BIN_EXE_xcomm"))
        .args(["list", "--json"])
        .output()
        .unwrap();
    assert_eq!(
        o.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&o.stderr)
    );
    let v: serde_json::Value = serde_json::from_slice(&o.stdout).unwrap();
    assert!(v.is_array(), "{v}");
    let o = std::process::Command::new(env!("CARGO_BIN_EXE_xcomm"))
        .args(["list", "extra"])
        .output()
        .unwrap();
    assert_eq!(o.status.code(), Some(2));
}

#[test]
fn enumerate_on_this_machine_is_ok() {
    assert!(xcomm::list::enumerate(&|_| None).is_ok());
}

/// A socket in a fresh runtime directory answering one status with `reply`.
fn one_session(
    tty: &str,
    reply: &'static [u8],
) -> (PathBuf, std::thread::JoinHandle<()>) {
    let runtime = support::lock_dir("list-run");
    std::fs::DirBuilder::new()
        .mode(0o700)
        .create(runtime.join("xcomm"))
        .unwrap();
    let listener = UnixListener::bind(runtime.join(format!("xcomm/{tty}.sock"))).unwrap();
    let t = std::thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        let mut line = String::new();
        BufReader::new(&stream).read_line(&mut line).unwrap();
        (&stream).write_all(reply).unwrap();
    });
    (runtime, t)
}

#[test]
fn a_session_not_connected_to_that_tty_lends_it_no_pane() {
    let root = tree();
    for reply in [
        // Waiting for its own device, under the name it had.
        &b"{\"ok\":true,\"connected\":false,\"tty\":\"ttyUSB0\",\"herdr_pane\":\"w1:p9\"}\n"[..],
        b"{\"ok\":true,\"connected\":true,\"tty\":\"ttyUSB5\",\"herdr_pane\":\"w1:p9\"}\n",
    ] {
        let (runtime, t) = one_session("ttyUSB0", reply);
        let vars = [("XDG_RUNTIME_DIR", runtime.to_str().unwrap().to_string())];
        let ports = enumerate_in(&root, &env_with(&vars)).unwrap();
        t.join().unwrap();
        assert_eq!(find(&ports, "ttyUSB0").herdr_pane, None);
        let _ = std::fs::remove_dir_all(&runtime);
    }
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_session_that_accepts_nothing_does_not_hold_up_the_list() {
    use rustix::net::{AddressFamily, SocketAddrUnix, SocketType, bind, listen, socket};
    let root = tree();
    let runtime = support::lock_dir("list-run");
    std::fs::DirBuilder::new()
        .mode(0o700)
        .create(runtime.join("xcomm"))
        .unwrap();
    // A queue of one, never accepted: the second connect would wait.
    let fd = socket(AddressFamily::UNIX, SocketType::STREAM, None).unwrap();
    bind(
        &fd,
        &SocketAddrUnix::new(runtime.join("xcomm/ttyUSB0.sock")).unwrap(),
    )
    .unwrap();
    listen(&fd, 0).unwrap();
    let vars = [("XDG_RUNTIME_DIR", runtime.to_str().unwrap().to_string())];
    let t = std::time::Instant::now();
    for _ in 0..4 {
        let ports = enumerate_in(&root, &env_with(&vars)).unwrap();
        assert_eq!(find(&ports, "ttyUSB0").herdr_pane, None);
    }
    assert!(
        t.elapsed() < std::time::Duration::from_secs(2),
        "{:?}",
        t.elapsed()
    );
    drop(fd);
    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_dir_all(&runtime);
}

#[test]
fn a_lock_in_a_directory_this_user_cannot_write_is_still_read() {
    use std::os::unix::fs::PermissionsExt;
    let root = tree();
    let locks = support::lock_dir("list-locks");
    std::fs::write(
        locks.join("LCK..ttyUSB2"),
        format!("{:>10}\n", std::process::id()),
    )
    .unwrap();
    std::fs::set_permissions(&locks, std::fs::Permissions::from_mode(0o555)).unwrap();
    let vars = [("XCOMM_LOCK_DIR", locks.to_str().unwrap().to_string())];
    let ports = enumerate_in(&root, &env_with(&vars)).unwrap();
    std::fs::set_permissions(&locks, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(
        find(&ports, "ttyUSB2").lock_pid,
        Some(std::process::id() as i32)
    );
    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_dir_all(&locks);
}

#[test]
fn control_characters_from_a_device_are_not_printed() {
    let root = tree();
    std::fs::write(
        root.join("sys/devices/usb1/1-1/product"),
        "evil\x1b]0;x\x07\tname\n",
    )
    .unwrap();
    let ports = enumerate_in(&root, &|_| None).unwrap();
    assert_eq!(
        find(&ports, "ttyUSB0").product.as_deref(),
        Some("evil\x1b]0;x\x07\tname")
    );
    let text = table(&ports);
    assert!(!text.contains('\x1b') && !text.contains('\x07'), "{text:?}");
    assert!(text.contains("evil?]0;x??name"), "{text:?}");
    let _ = std::fs::remove_dir_all(&root);
}
