//! A pty taken as a device that can come back (XCOMM_TEST_PTY_AS_DEVICE),
//! so the session takes the path of a USB adapter. One test, in its own
//! binary: while waiting, the session follows the old pty name, and a pty
//! made meanwhile by another test could take that number.

mod support;

use std::fs;
use std::os::fd::AsFd;
use std::path::{Path, PathBuf};
use std::time::Duration;

use support::{lock_dir, read_until, spawn_with_env, write_all};

const T: Duration = Duration::from_secs(5);

fn relink(link: &Path, target: &Path) {
    let tmp = link.with_extension("tmp");
    let _ = fs::remove_file(&tmp);
    std::os::unix::fs::symlink(target, &tmp).unwrap();
    fs::rename(&tmp, link).unwrap();
}

fn lock_file(dir: &Path, device: &Path) -> PathBuf {
    let real = fs::canonicalize(device).unwrap();
    dir.join(format!("LCK..{}", xcomm::lock::lock_name(&real).unwrap()))
}

#[test]
fn a_device_unplugged_and_plugged_back() {
    let dir =
        std::env::temp_dir().join(format!("xcomm-reconnect-node-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    let link = dir.join("by-id-usb-X");
    let d1 = support::pty();
    relink(&link, &d1.slave_path);
    let locks = lock_dir("reconnect-node");
    let mut s = spawn_with_env(
        env!("CARGO_BIN_EXE_xcomm"),
        &[link.to_str().unwrap()],
        &locks,
        &[("XCOMM_TEST_PTY_AS_DEVICE", "1")],
    );
    read_until(s.host.master.as_fd(), b"\r\n", T);
    let old_lock = lock_file(&locks, &d1.slave_path);
    assert!(old_lock.exists());

    // udev removes the by-id link with the device; the session must not
    // follow the old pty name, which the next terminal made may take.
    relink(&link, &dir.join("gone"));
    drop(d1);
    let out = read_until(s.host.master.as_fd(), b"waiting", T);
    assert!(
        String::from_utf8_lossy(&out).contains("disconnected"),
        "{out:?}"
    );
    // Gone, not hung up: still waiting after the hangup grace.
    std::thread::sleep(Duration::from_millis(1500));
    assert!(s.child.try_wait().unwrap().is_none());
    assert!(!old_lock.exists(), "the old lock is held while waiting");

    let d2 = support::pty();
    relink(&link, &d2.slave_path);
    read_until(s.host.master.as_fd(), b"reconnected", T);
    assert!(lock_file(&locks, &d2.slave_path).exists());
    write_all(d2.master.as_fd(), b"from d2");
    read_until(s.host.master.as_fd(), b"from d2", T);
    write_all(s.host.master.as_fd(), b"to d2");
    read_until(d2.master.as_fd(), b"to d2", T);

    write_all(s.host.master.as_fd(), b"\x01x");
    assert!(s.wait_exit(T).success());
    assert!(!lock_file(&locks, &d2.slave_path).exists());
    let _ = fs::remove_dir_all(&dir);
}

/// A hangup (the node stays) ends the session as it always did, without
/// a word about waiting. macOS can hang up a pty the user owns with
/// revoke(2); Linux has no such call for a non-root user.
#[cfg(target_os = "macos")]
#[test]
fn a_device_hung_up_ends_the_session() {
    unsafe extern "C" {
        fn revoke(path: *const libc::c_char) -> libc::c_int;
    }
    let d1 = support::pty();
    let path = d1.slave_path.to_str().unwrap().to_string();
    let locks = lock_dir("reconnect-hangup");
    let mut s = spawn_with_env(
        env!("CARGO_BIN_EXE_xcomm"),
        &[&path],
        &locks,
        &[("XCOMM_TEST_PTY_AS_DEVICE", "1")],
    );
    read_until(s.host.master.as_fd(), b"\r\n", T);
    let c = std::ffi::CString::new(path).unwrap();
    assert_eq!(unsafe { revoke(c.as_ptr()) }, 0);
    assert_eq!(s.wait_exit(T).code(), Some(1));
    let out = s.read_after_exit(b"disconnected", T);
    assert!(
        !String::from_utf8_lossy(&out).contains("waiting"),
        "{out:?}"
    );
    drop(d1);
}
