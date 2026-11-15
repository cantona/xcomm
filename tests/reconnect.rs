mod support;

use std::fs;
use std::os::fd::AsFd;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use rustix::termios::tcgetattr;
use support::{Pty, Spawned, lock_dir, read_until, spawn_xcomm, write_all};
use xcomm::reconnect::{DeviceWatch, NodeId, Track, is_pty, stable_path_in, udev_link};

const T: Duration = Duration::from_secs(5);

/// A scratch directory, removed when the test ends.
struct TempDir(PathBuf);

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn temp_dir() -> TempDir {
    static N: AtomicUsize = AtomicUsize::new(0);
    let d = std::env::temp_dir().join(format!(
        "xcomm-reconnect-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::SeqCst)
    ));
    let _ = fs::remove_dir_all(&d);
    fs::create_dir_all(&d).unwrap();
    TempDir(d)
}

/// Points `link` at `target` in one step, as udev replaces a by-id link.
fn relink(link: &Path, target: &Path) {
    let tmp = link.with_extension("tmp");
    let _ = fs::remove_file(&tmp);
    std::os::unix::fs::symlink(target, &tmp).unwrap();
    fs::rename(&tmp, link).unwrap();
}

/// A session on a symlink to a fresh pty, as a by-id link names a device.
fn start(dir: &TempDir) -> (Pty, PathBuf, Spawned) {
    let dev = support::pty();
    let link = dir.0.join("dev");
    relink(&link, &dev.slave_path);
    let s = spawn_xcomm(&[link.to_str().unwrap()], &lock_dir("reconnect"));
    read_until(s.host.master.as_fd(), b"\r\n", T);
    (dev, link, s)
}

fn lock_file(s: &Spawned, device: &Path) -> PathBuf {
    let real = fs::canonicalize(device).unwrap();
    s.lock_dir
        .join(format!("LCK..{}", xcomm::lock::lock_name(&real).unwrap()))
}

#[test]
fn hangup_then_return_reconnects() {
    let dir = temp_dir();
    let (d1, link, mut s) = start(&dir);
    // Made first, so it gets another pty number and the lock has to move.
    let d2 = support::pty();
    let old_lock = lock_file(&s, &d1.slave_path);
    assert!(old_lock.exists());
    drop(d1);
    let out = read_until(s.host.master.as_fd(), b"waiting", T);
    assert!(
        String::from_utf8_lossy(&out).contains("disconnected"),
        "{out:?}"
    );
    assert!(s.child.try_wait().unwrap().is_none());
    // Waiting on a link, the old name is not held for a device that may
    // never come back under it.
    assert!(!old_lock.exists(), "the old lock is held while waiting");
    relink(&link, &d2.slave_path);
    let out = read_until(s.host.master.as_fd(), b"reconnected", T);
    assert!(
        String::from_utf8_lossy(&out).contains(link.to_str().unwrap()),
        "{out:?}"
    );
    write_all(d2.master.as_fd(), b"from d2\r\n");
    read_until(s.host.master.as_fd(), b"from d2", T);
    write_all(s.host.master.as_fd(), b"to d2");
    read_until(d2.master.as_fd(), b"to d2", T);
    assert!(lock_file(&s, &d2.slave_path).exists());
    write_all(s.host.master.as_fd(), b"\x01x");
    assert!(s.wait_exit(T).success());
    assert!(!lock_file(&s, &d2.slave_path).exists());
}

#[test]
fn quit_while_disconnected() {
    let dir = temp_dir();
    let (d1, _link, mut s) = start(&dir);
    drop(d1);
    read_until(s.host.master.as_fd(), b"waiting", T);
    write_all(s.host.master.as_fd(), b"\x01x");
    assert!(s.wait_exit(T).success());
}

#[test]
fn sigterm_while_disconnected_ends_the_session() {
    let dir = temp_dir();
    let (d1, _link, mut s) = start(&dir);
    drop(d1);
    read_until(s.host.master.as_fd(), b"waiting", T);
    unsafe { libc::kill(s.child.id() as i32, libc::SIGTERM) };
    assert!(s.wait_exit(T).success());
}

#[test]
fn typing_while_disconnected_is_not_queued() {
    let dir = temp_dir();
    let (d1, link, s) = start(&dir);
    drop(d1);
    read_until(s.host.master.as_fd(), b"waiting", T);
    write_all(s.host.master.as_fd(), b"abc");
    read_until(s.host.master.as_fd(), b"\x07", T);
    let d2 = support::pty();
    relink(&link, &d2.slave_path);
    read_until(s.host.master.as_fd(), b"reconnected", T);
    write_all(s.host.master.as_fd(), b"Z");
    assert_eq!(read_until(d2.master.as_fd(), b"Z", T), b"Z");
}

#[test]
fn port_commands_say_the_device_is_disconnected() {
    let dir = temp_dir();
    let (d1, _link, s) = start(&dir);
    drop(d1);
    read_until(s.host.master.as_fd(), b"waiting", T);
    write_all(s.host.master.as_fd(), b"\x01f");
    let out = read_until(s.host.master.as_fd(), b"disconnected\r\n", T);
    assert!(
        String::from_utf8_lossy(&out).contains("send break: the device is disconnected"),
        "{out:?}"
    );
}

#[test]
fn the_device_comes_back_with_the_settings_it_had() {
    let dir = temp_dir();
    let (d1, link, s) = start(&dir);
    write_all(s.host.master.as_fd(), b"\x01p9600\r");
    read_until(s.host.master.as_fd(), b"9600 8N1", T);
    drop(d1);
    read_until(s.host.master.as_fd(), b"waiting", T);
    let d2 = support::pty();
    relink(&link, &d2.slave_path);
    read_until(s.host.master.as_fd(), b"reconnected", T);
    assert_eq!(tcgetattr(d2.slave.as_fd()).unwrap().output_speed(), 9600);
}

#[test]
fn a_device_back_before_the_watch_is_found() {
    let dir = temp_dir();
    let (d1, link, s) = start(&dir);
    // The link already names the new device when the old one goes.
    let d2 = support::pty();
    relink(&link, &d2.slave_path);
    drop(d1);
    read_until(s.host.master.as_fd(), b"reconnected", T);
    write_all(d2.master.as_fd(), b"back");
    read_until(s.host.master.as_fd(), b"back", T);
}

/// A fake `/dev/serial` under `root` with `ttyFAKE` linked from by-id and
/// by-path.
fn fake_tree(root: &Path) -> (PathBuf, PathBuf) {
    use std::os::unix::fs::symlink;
    let links = root.join("serial");
    let tty = root.join("ttyFAKE");
    fs::write(&tty, b"").unwrap();
    for (kind, names) in [
        ("by-id", &["usb-X-if00-port0", "usb-Y-if00-port0"]),
        (
            "by-path",
            &["pci-usb-0:7:1.0-port0", "pci-usbv2-0:7:1.0-port0"],
        ),
    ] {
        fs::create_dir_all(links.join(kind)).unwrap();
        for name in names {
            symlink("../../ttyFAKE", links.join(kind).join(name)).unwrap();
        }
    }
    (links, tty)
}

#[test]
fn stable_path_takes_by_path_for_a_kernel_name() {
    // Not by-id: identical adapters and clones sharing a serial number
    // share one by-id name, which can then name another adapter.
    let root = temp_dir();
    let (links, tty) = fake_tree(&root.0);
    assert_eq!(
        stable_path_in(&links.join("by-path"), &tty),
        links.join("by-path/pci-usb-0:7:1.0-port0")
    );
}

#[test]
fn stable_path_keeps_a_name_the_user_chose() {
    let root = temp_dir();
    let (links, tty) = fake_tree(&root.0);
    let by_path = links.join("by-path");
    for chosen in [
        links.join("by-id/usb-Y-if00-port0"),
        links.join("by-path/pci-usbv2-0:7:1.0-port0"),
    ] {
        assert_eq!(stable_path_in(&by_path, &chosen), chosen);
    }
    let own = root.0.join("my-board");
    std::os::unix::fs::symlink(&tty, &own).unwrap();
    assert_eq!(stable_path_in(&by_path, &own), own);
}

#[test]
fn stable_path_is_the_path_without_links() {
    let root = temp_dir();
    let (links, _) = fake_tree(&root.0);
    let by_path = links.join("by-path");
    let other = root.0.join("ttyOTHER");
    fs::write(&other, b"").unwrap();
    assert_eq!(stable_path_in(&by_path, &other), other);
    let missing = root.0.join("ttyGONE");
    assert_eq!(stable_path_in(&by_path, &missing), missing);
    let (_, tty) = (0, root.0.join("ttyFAKE"));
    assert_eq!(stable_path_in(&root.0.join("none"), &tty), tty);
}

/// Calls check until it reports the device, or panics after `timeout`.
fn checked_within(w: &mut DeviceWatch, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    while !w.check().unwrap() {
        assert!(Instant::now() < deadline, "not reported in {timeout:?}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn watch_reports_a_new_node_once() {
    let dir = temp_dir();
    let path = dir.0.join("ttyX");
    let mut w = DeviceWatch::new(&path, &path, None, Track::Node);
    assert!(!w.check().unwrap());
    fs::write(&path, b"").unwrap();
    checked_within(&mut w, T);
    std::thread::sleep(Duration::from_millis(1100));
    assert!(!w.check().unwrap(), "the same node was reported twice");
    // Made again: another node.
    fs::remove_file(&path).unwrap();
    fs::write(&path, b"").unwrap();
    checked_within(&mut w, T);
}

#[test]
fn watch_skips_the_node_that_went_away() {
    let dir = temp_dir();
    let path = dir.0.join("ttyX");
    fs::write(&path, b"").unwrap();
    let gone = NodeId::at(&path).unwrap();
    let mut w = DeviceWatch::new(&path, &path, Some(gone), Track::Node);
    std::thread::sleep(Duration::from_millis(1100));
    assert!(!w.check().unwrap());
    assert!(!w.check().unwrap());
}

#[test]
fn watch_retries_the_same_node_when_asked() {
    let dir = temp_dir();
    let path = dir.0.join("ttyX");
    fs::write(&path, b"").unwrap();
    let mut w = DeviceWatch::new(&path, &path, None, Track::Node);
    assert!(w.check().unwrap());
    assert!(w.retry());
    let due = w.deadline().unwrap();
    assert!(due <= Instant::now() + Duration::from_millis(100));
    std::thread::sleep(due.saturating_duration_since(Instant::now()));
    assert!(w.check().unwrap());
}

#[test]
fn watch_finds_a_device_in_a_directory_made_later() {
    let dir = temp_dir();
    let by_id = dir.0.join("by-id");
    let path = by_id.join("usb-X");
    let mut w = DeviceWatch::new(&path, &path, None, Track::Node);
    assert!(!w.check().unwrap());
    fs::create_dir(&by_id).unwrap();
    fs::write(&path, b"").unwrap();
    checked_within(&mut w, Duration::from_secs(3));
}

#[test]
fn watch_follows_the_directory_going_and_coming_back() {
    let dir = temp_dir();
    let by_id = dir.0.join("by-id");
    let path = by_id.join("usb-X");
    fs::create_dir(&by_id).unwrap();
    let mut w = DeviceWatch::new(&path, &path, None, Track::Node);
    assert!(!w.check().unwrap());
    fs::remove_dir(&by_id).unwrap();
    std::thread::sleep(Duration::from_millis(50));
    assert!(!w.check().unwrap());
    fs::create_dir(&by_id).unwrap();
    fs::write(&path, b"").unwrap();
    checked_within(&mut w, Duration::from_secs(3));
}

#[test]
fn watch_of_a_link_ignores_its_target_coming_back() {
    let dir = temp_dir();
    let target = dir.0.join("pts9");
    let link = dir.0.join("dev");
    fs::write(&target, b"").unwrap();
    relink(&link, &target);
    let gone = NodeId::link_at(&link).ok();
    fs::remove_file(&target).unwrap();
    let mut w = DeviceWatch::new(&link, &target, gone, Track::Link);
    assert!(!w.check().unwrap());
    // The old name made again, as the next pty takes the number.
    fs::write(&target, b"").unwrap();
    std::thread::sleep(Duration::from_millis(1100));
    assert!(!w.check().unwrap(), "followed the old name to a new node");
    let other = dir.0.join("pts10");
    fs::write(&other, b"").unwrap();
    relink(&link, &other);
    checked_within(&mut w, T);
}

#[test]
fn ptys_are_told_apart() {
    let dev = support::pty();
    assert!(is_pty(&fs::canonicalize(&dev.slave_path).unwrap()));
    assert!(!is_pty(Path::new("/dev/ttyUSB1")));
    assert!(!is_pty(Path::new("/dev/null")));
}

fn root() -> bool {
    unsafe { libc::geteuid() == 0 }
}

/// The session waits on a device another program holds (picocom's flock),
/// and takes it once that program lets go.
#[test]
fn a_busy_device_is_tried_again() {
    use rustix::fs::{FlockOperation, flock};
    let dir = temp_dir();
    let (d1, link, s) = start(&dir);
    drop(d1);
    read_until(s.host.master.as_fd(), b"waiting", T);
    let d2 = support::pty();
    flock(&d2.slave, FlockOperation::NonBlockingLockExclusive).unwrap();
    relink(&link, &d2.slave_path);
    let out = read_until(s.host.master.as_fd(), b"still waiting", T);
    assert!(String::from_utf8_lossy(&out).contains("flock"), "{out:?}");
    flock(&d2.slave, FlockOperation::Unlock).unwrap();
    read_until(s.host.master.as_fd(), b"reconnected", T);
}

/// The new tty name is locked by another live process; once its lock goes
/// the session takes the device.
#[test]
fn a_locked_new_name_is_tried_again() {
    let dir = temp_dir();
    let (d1, link, s) = start(&dir);
    let d2 = support::pty();
    drop(d1);
    read_until(s.host.master.as_fd(), b"waiting", T);
    let lock = lock_file(&s, &d2.slave_path);
    fs::write(&lock, format!("{:>10}\n", std::process::id())).unwrap();
    relink(&link, &d2.slave_path);
    read_until(s.host.master.as_fd(), b"still waiting", T);
    fs::remove_file(&lock).unwrap();
    read_until(s.host.master.as_fd(), b"reconnected", T);
}

/// udev sets a new node's mode a moment after it appears.
#[test]
fn a_node_not_yet_permitted_is_tried_again() {
    use std::os::unix::fs::PermissionsExt;
    if root() {
        return;
    }
    let dir = temp_dir();
    let (d1, link, s) = start(&dir);
    drop(d1);
    read_until(s.host.master.as_fd(), b"waiting", T);
    let d2 = support::pty();
    let mode = fs::metadata(&d2.slave_path).unwrap().permissions().mode();
    fs::set_permissions(&d2.slave_path, fs::Permissions::from_mode(0o000)).unwrap();
    relink(&link, &d2.slave_path);
    std::thread::sleep(Duration::from_millis(400));
    fs::set_permissions(&d2.slave_path, fs::Permissions::from_mode(mode)).unwrap();
    let out = read_until(s.host.master.as_fd(), b"reconnected", T);
    assert!(
        !String::from_utf8_lossy(&out).contains("still waiting"),
        "{out:?}"
    );
}

#[test]
fn piped_input_ends_the_session_at_a_disconnect() {
    // What was read from the pipe but not written is lost: going on after
    // a replug would leave a hole in the stream.
    let dir = temp_dir();
    let d1 = support::pty();
    let link = dir.0.join("dev");
    relink(&link, &d1.slave_path);
    let locks = lock_dir("reconnect-piped");
    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_xcomm"))
        .arg(&link)
        .env("XCOMM_LOCK_DIR", &locks)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let stdin = child.stdin.take().unwrap();
    // The banner is out once the port is open.
    let mut err = child.stderr.take().unwrap();
    let mut banner = [0u8; 1];
    std::io::Read::read_exact(&mut err, &mut banner).unwrap();
    drop(d1);
    let status = support::wait_exit(&mut child, T);
    assert_eq!(status.code(), Some(1));
    let mut rest = String::new();
    std::io::Read::read_to_string(&mut err, &mut rest).unwrap();
    assert!(rest.contains("disconnected"), "{rest}");
    assert!(!rest.contains("waiting"), "{rest}");
    drop(stdin);
    let _ = fs::remove_dir_all(&locks);
}

#[cfg(target_os = "linux")]
#[test]
fn watch_follows_every_link_of_a_chain() {
    let dir = temp_dir();
    let mine = dir.0.join("board");
    let by_id = dir.0.join("by-id");
    fs::create_dir(&by_id).unwrap();
    let mid = by_id.join("usb-X");
    let old = dir.0.join("ttyUSB1");
    fs::write(&old, b"").unwrap();
    std::os::unix::fs::symlink(&mid, &mine).unwrap();
    relink(&mid, &old);
    let gone = NodeId::at(&mine).ok();
    fs::remove_file(&old).unwrap();
    let mut w = DeviceWatch::new(&mine, &old, gone, Track::Node);
    assert!(!w.check().unwrap());
    // Back under another tty name: only the middle link changes.
    let new = dir.0.join("ttyUSB2");
    fs::write(&new, b"").unwrap();
    relink(&mid, &new);
    std::thread::sleep(Duration::from_millis(50));
    assert!(w.check().unwrap(), "the middle link was not watched");
}

#[test]
fn udev_links_are_told_apart() {
    assert!(udev_link(Path::new(
        "/dev/serial/by-path/pci-usb-0:7:1.0-port0"
    )));
    assert!(udev_link(Path::new("/dev/serial/by-id/usb-X-if00-port0")));
    assert!(!udev_link(Path::new("/dev/ttyUSB1")));
    assert!(!udev_link(Path::new("/home/me/board")));
}

#[test]
fn same_node_ignores_a_ctime_change() {
    use std::os::unix::fs::PermissionsExt;
    let dir = temp_dir();
    let path = dir.0.join("ttyX");
    fs::write(&path, b"").unwrap();
    let before = NodeId::at(&path).unwrap();
    std::thread::sleep(Duration::from_millis(20));
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    let after = NodeId::at(&path).unwrap();
    assert_ne!(before, after, "chmod did not change ctime");
    assert!(before.same_node(&after));
    let other = dir.0.join("ttyY");
    fs::write(&other, b"").unwrap();
    assert!(!NodeId::at(&other).unwrap().same_node(&after));
}
