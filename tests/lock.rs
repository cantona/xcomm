use std::fs;
use std::os::fd::AsFd;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};

use xcomm::lock::{Cleared, LockError, acquire, default_lock_dir, holder, lock_name};

/// A scratch lock directory, removed when the test ends.
#[derive(Debug)]
struct TempDir(PathBuf);

impl std::ops::Deref for TempDir {
    type Target = PathBuf;
    fn deref(&self) -> &PathBuf {
        &self.0
    }
}

impl AsRef<std::path::Path> for TempDir {
    fn as_ref(&self) -> &std::path::Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn temp_dir() -> TempDir {
    static N: AtomicUsize = AtomicUsize::new(0);
    let d = std::env::temp_dir().join(format!(
        "xcomm-lock-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::SeqCst)
    ));
    let _ = fs::remove_dir_all(&d);
    fs::create_dir_all(&d).unwrap();
    TempDir(d)
}

fn hdb(pid: u32) -> String {
    format!("{pid:>10}\n")
}

/// A pid with no process: a child that has already been reaped.
fn dead_pid() -> u32 {
    let mut c = std::process::Command::new("true").spawn().unwrap();
    let pid = c.id();
    c.wait().unwrap();
    pid
}

#[test]
fn acquire_writes_hdb_format_and_drop_removes_it() {
    let d = temp_dir();
    let path = d.join("LCK..ttyFAKE0");
    {
        let (lock, stale) = acquire(&d, "ttyFAKE0").unwrap();
        assert_eq!(stale, None);
        assert_eq!(fs::read_to_string(&path).unwrap(), hdb(std::process::id()));
        assert_eq!(lock.path(), path.as_path());
    }
    assert!(!path.exists());
    let leftovers: Vec<_> = fs::read_dir(&d).unwrap().collect();
    assert!(leftovers.is_empty(), "temp files left: {leftovers:?}");
}

#[test]
fn second_acquire_in_the_same_process_is_refused() {
    let d = temp_dir();
    let (_lock, _) = acquire(&d, "ttyFAKE1").unwrap();
    match acquire(&d, "ttyFAKE1") {
        Err(e @ LockError::Io { .. }) => {
            assert!(e.to_string().contains("already held here"), "{e}")
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn lock_held_by_another_live_program_is_reported() {
    // A live process that is not xcomm (minicom, picocom) holds the lock
    // without flock; its pid still makes the lock held.
    let d = temp_dir();
    let mut sleeper = std::process::Command::new("sleep")
        .arg("30")
        .spawn()
        .unwrap();
    fs::write(d.join("LCK..ttyFAKE2"), hdb(sleeper.id())).unwrap();
    let r = acquire(&d, "ttyFAKE2");
    sleeper.kill().unwrap();
    sleeper.wait().unwrap();
    match r {
        Err(LockError::Held { pid, .. }) => assert_eq!(pid, sleeper.id() as i32),
        other => panic!("{other:?}"),
    }
}

#[test]
fn stale_lock_is_replaced_and_reported() {
    let d = temp_dir();
    let dead = dead_pid();
    fs::write(d.join("LCK..ttyFAKE3"), hdb(dead)).unwrap();
    let (_lock, stale) = acquire(&d, "ttyFAKE3").unwrap();
    assert_eq!(stale, Some(Cleared::DeadPid(dead as i32)));
    assert_eq!(
        fs::read_to_string(d.join("LCK..ttyFAKE3")).unwrap(),
        hdb(std::process::id())
    );
}

#[test]
fn ascii_pid_without_padding_is_understood() {
    // Some programs write the pid without the HDB padding.
    let d = temp_dir();
    let dead = dead_pid();
    fs::write(d.join("LCK..ttyFAKE4"), format!("{dead}\n")).unwrap();
    let (_lock, stale) = acquire(&d, "ttyFAKE4").unwrap();
    assert_eq!(stale, Some(Cleared::DeadPid(dead as i32)));
}

#[test]
fn garbage_lock_is_an_error_not_replaced() {
    let d = temp_dir();
    let path = d.join("LCK..ttyFAKE5");
    fs::write(&path, "hello\n").unwrap();
    match acquire(&d, "ttyFAKE5") {
        Err(e @ LockError::Io { .. }) => {
            assert!(e.to_string().contains("LCK..ttyFAKE5"), "{e}");
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(fs::read_to_string(&path).unwrap(), "hello\n");
}

#[test]
fn stale_looking_lock_under_flock_is_not_removed() {
    // A dead pid but a held flock means another xcomm is in the middle of
    // taking the lock over; it must not be deleted from under it.
    let d = temp_dir();
    let path = d.join("LCK..ttyFAKE6");
    fs::write(&path, hdb(dead_pid())).unwrap();
    let f = fs::File::open(&path).unwrap();
    rustix::fs::flock(
        f.as_fd(),
        rustix::fs::FlockOperation::NonBlockingLockExclusive,
    )
    .unwrap();
    match acquire(&d, "ttyFAKE6") {
        Err(LockError::Contended { path: p, why }) => {
            assert_eq!(p, path);
            assert!(why.contains("flock is held"), "{why}");
        }
        other => panic!("{other:?}"),
    }
    assert!(path.exists());
}

#[test]
fn drop_leaves_a_lock_that_is_no_longer_ours() {
    let d = temp_dir();
    let path = d.join("LCK..ttyFAKE7");
    let (lock, _) = acquire(&d, "ttyFAKE7").unwrap();
    fs::remove_file(&path).unwrap();
    fs::write(&path, hdb(1)).unwrap();
    drop(lock);
    assert_eq!(fs::read_to_string(&path).unwrap(), hdb(1));
}

#[test]
fn holder_reports_live_pids_only() {
    let d = temp_dir();
    assert_eq!(holder(&d, "ttyFAKE8").unwrap(), None);
    let (_lock, _) = acquire(&d, "ttyFAKE8").unwrap();
    assert_eq!(
        holder(&d, "ttyFAKE8").unwrap(),
        Some(std::process::id() as i32)
    );
    fs::write(d.join("LCK..ttyFAKE9"), hdb(dead_pid())).unwrap();
    assert_eq!(holder(&d, "ttyFAKE9").unwrap(), None);
    fs::write(d.join("LCK..ttyFAKE9b"), "garbage").unwrap();
    assert!(
        holder(&d, "ttyFAKE9b").is_err(),
        "unreadable lock must not look free"
    );
}

#[test]
fn tty_names_with_path_separators_are_refused() {
    let d = temp_dir();
    for bad in ["", "..", "a/b", "../ttyS0"] {
        assert!(acquire(&d, bad).is_err(), "{bad:?}");
    }
}

#[test]
fn default_lock_dir_honours_the_test_hook_and_checks_it() {
    let d = temp_dir();
    let ds = d.to_str().unwrap().to_string();
    let env = move |k: &str| (k == "XCOMM_LOCK_DIR").then(|| ds.clone());
    assert_eq!(default_lock_dir(&env).unwrap(), *d);
    let missing =
        |k: &str| (k == "XCOMM_LOCK_DIR").then(|| "/nonexistent/xcomm".to_string());
    assert!(default_lock_dir(&missing).is_err());
    let none = |_: &str| None;
    #[cfg(not(target_os = "macos"))]
    let standard = [
        "/run/lock/lockdev",
        "/var/lock/lockdev",
        "/run/lock",
        "/var/lock",
        "/var/spool/lock",
    ];
    #[cfg(target_os = "macos")]
    let standard = ["/var/spool/lock", "/var/lock", "/tmp"];
    match default_lock_dir(&none) {
        Ok(dir) => assert!(standard.contains(&dir.to_str().unwrap())),
        Err(LockError::NoLockDir { tried }) => {
            assert_eq!(tried.len(), standard.len());
            assert!(
                standard.iter().all(|d| rustix::fs::access(
                    *d,
                    rustix::fs::Access::WRITE_OK
                )
                .is_err())
            );
        }
        Err(e) => panic!("{e}"),
    }
}

#[test]
fn lock_names_match_minicom() {
    use std::path::Path;
    // Observed: minicom on /dev/pts/165 created /run/lock/LCK..pts_165.
    assert_eq!(
        lock_name(Path::new("/dev/pts/165")).as_deref(),
        Some("pts_165")
    );
    assert_eq!(
        lock_name(Path::new("/dev/ttyUSB1")).as_deref(),
        Some("ttyUSB1")
    );
    assert_eq!(
        lock_name(Path::new("/tmp/x/ttyFAKE")).as_deref(),
        Some("ttyFAKE")
    );
    assert_eq!(lock_name(Path::new("/dev/")), None);
}

#[test]
fn pid_followed_by_words_is_understood() {
    let d = temp_dir();
    let dead = dead_pid();
    fs::write(d.join("LCK..ttyFAKE10"), format!("{dead:>10} prog user\n")).unwrap();
    let (_lock, stale) = acquire(&d, "ttyFAKE10").unwrap();
    assert_eq!(stale, Some(Cleared::DeadPid(dead as i32)));
}

#[test]
fn fifo_lock_file_is_refused_without_blocking() {
    let d = temp_dir();
    let path = d.join("LCK..ttyFIFO");
    let c = std::ffi::CString::new(path.to_str().unwrap()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o644) }, 0);
    let start = std::time::Instant::now();
    let e = acquire(&d, "ttyFIFO").err().unwrap().to_string();
    assert!(start.elapsed() < std::time::Duration::from_secs(2));
    assert!(e.contains("not a regular file"), "{e}");
    assert!(holder(&d, "ttyFIFO").is_err());
}

#[test]
fn symlinked_lock_file_is_refused() {
    let d = temp_dir();
    let target = d.join("elsewhere");
    fs::write(&target, hdb(dead_pid())).unwrap();
    std::os::unix::fs::symlink(&target, d.join("LCK..ttyLINK")).unwrap();
    assert!(acquire(&d, "ttyLINK").is_err());
    assert!(target.exists());
}

#[test]
fn oversized_lock_file_is_refused() {
    let d = temp_dir();
    fs::write(d.join("LCK..ttyBIG"), vec![b'1'; 4096]).unwrap();
    let e = acquire(&d, "ttyBIG").err().unwrap().to_string();
    assert!(e.contains("too large"), "{e}");
}

#[test]
fn empty_lock_file_is_contention_while_young_and_cleared_when_old() {
    // Another program created it and has not written its pid yet...
    let d = temp_dir();
    let path = d.join("LCK..ttyEMPTY");
    fs::write(&path, "").unwrap();
    match acquire(&d, "ttyEMPTY") {
        Err(LockError::Contended { why, .. }) => {
            assert!(why.contains("being written"), "{why}")
        }
        other => panic!("{other:?}"),
    }
    assert!(matches!(
        holder(&d, "ttyEMPTY"),
        Err(LockError::Contended { .. })
    ));
    // ...or died before it could: an old empty file is cleared.
    let set_mtime = |t: std::time::SystemTime| {
        fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(t)
            .unwrap()
    };
    set_mtime(std::time::SystemTime::now() - std::time::Duration::from_secs(60));
    assert_eq!(holder(&d, "ttyEMPTY").unwrap(), None);
    let (lock, cleared) = acquire(&d, "ttyEMPTY").unwrap();
    assert_eq!(cleared, Some(Cleared::Empty));
    drop(lock);
    // A clock stepped back must not keep an empty file young for ever.
    fs::write(&path, "").unwrap();
    set_mtime(std::time::SystemTime::now() + std::time::Duration::from_secs(3600));
    let (_lock, cleared) = acquire(&d, "ttyEMPTY").unwrap();
    assert_eq!(cleared, Some(Cleared::Empty));
}

#[test]
fn lock_owned_by_another_users_live_process_is_held() {
    // kill(1, 0) fails with EPERM for non-root: the process exists.
    if rustix::process::getuid().is_root() {
        return;
    }
    let d = temp_dir();
    fs::write(d.join("LCK..ttyINIT"), hdb(1)).unwrap();
    assert!(matches!(
        acquire(&d, "ttyINIT"),
        Err(LockError::Held { pid: 1, .. })
    ));
}

#[test]
fn concurrent_acquirers_of_one_stale_lock_get_exactly_one_lock() {
    for round in 0..20 {
        let d = temp_dir();
        let name = format!("ttyRACE{round}");
        fs::write(d.join(format!("LCK..{name}")), hdb(dead_pid())).unwrap();
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(8));
        let handles: Vec<_> = (0..8)
            .map(|_| {
                let (d, name, barrier) = (d.clone(), name.clone(), barrier.clone());
                std::thread::spawn(move || {
                    barrier.wait();
                    acquire(&d, &name)
                })
            })
            .collect();
        let results: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        let winners = results.iter().filter(|r| r.is_ok()).count();
        for r in &results {
            if let Err(e) = r {
                // Threads share the pid, so a loser may also see its own pid.
                let own_pid = e.to_string().contains("own pid");
                assert!(
                    own_pid
                        || matches!(
                            e,
                            LockError::Held { .. } | LockError::Contended { .. }
                        ),
                    "{e}"
                );
            }
        }
        assert_eq!(winners, 1, "round {round}: {results:?}");
    }
}

/// Child half of `lock_is_world_readable_whatever_the_umask`.
#[test]
fn umask_child() {
    let Some(dir) = std::env::var_os("XCOMM_TEST_UMASK_DIR") else {
        return;
    };
    unsafe { libc::umask(0o077) };
    let (lock, _) = acquire(std::path::Path::new(&dir), "ttyUMASK").unwrap();
    std::mem::forget(lock);
}

#[test]
fn lock_is_world_readable_whatever_the_umask() {
    use std::os::unix::fs::PermissionsExt;
    let d = temp_dir();
    let out = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "umask_child", "--test-threads=1"])
        .env("XCOMM_TEST_UMASK_DIR", &*d)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stdout)
    );
    let mode = fs::metadata(d.join("LCK..ttyUMASK"))
        .unwrap()
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode, 0o644);
}

/// Child half of `leftover_private_file_does_not_block_acquire`: a fresh
/// process, so the private-name counter starts at 0 and meets the planted
/// files.
#[test]
fn leftover_child() {
    let Some(dir) = std::env::var_os("XCOMM_TEST_LEFTOVER_DIR") else {
        return;
    };
    let dir = PathBuf::from(dir);
    for n in 0..5 {
        fs::write(
            dir.join(format!("LCK..ttyLEFT.{}.{n}.tmp", std::process::id())),
            "x",
        )
        .unwrap();
    }
    let (lock, _) = acquire(&dir, "ttyLEFT").unwrap();
    drop(lock);
    println!("XCOMM-LEFTOVER-OK");
}

#[test]
fn leftover_private_file_does_not_block_acquire() {
    let d = temp_dir();
    let out = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "leftover_child",
            "--nocapture",
            "--test-threads=1",
        ])
        .env("XCOMM_TEST_LEFTOVER_DIR", &*d)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stdout)
    );
    assert!(String::from_utf8_lossy(&out.stdout).contains("XCOMM-LEFTOVER-OK"));
}

/// Child half of `concurrent_processes_get_exactly_one_lock`: waits for the
/// start file, tries once, reports, and holds a won lock for a while.
#[test]
fn race_child() {
    let Some(dir) = std::env::var_os("XCOMM_TEST_RACE_DIR") else {
        return;
    };
    let dir = PathBuf::from(dir);
    while !dir.join("go").exists() {
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
    match acquire(&dir, "ttyPROC") {
        Ok((lock, _)) => {
            println!("XCOMM-RACE-WON");
            std::thread::sleep(std::time::Duration::from_millis(300));
            drop(lock);
        }
        Err(e) => println!("XCOMM-RACE-LOST {e}"),
    }
}

#[test]
fn concurrent_processes_get_exactly_one_lock() {
    for round in 0..10 {
        let d = temp_dir();
        fs::write(d.join("LCK..ttyPROC"), hdb(dead_pid())).unwrap();
        let children: Vec<_> = (0..8)
            .map(|_| {
                std::process::Command::new(std::env::current_exe().unwrap())
                    .args(["--exact", "race_child", "--nocapture", "--test-threads=1"])
                    .env("XCOMM_TEST_RACE_DIR", &*d)
                    .stdout(std::process::Stdio::piped())
                    .spawn()
                    .unwrap()
            })
            .collect();
        std::thread::sleep(std::time::Duration::from_millis(100));
        fs::write(d.join("go"), "").unwrap();
        let mut won = 0;
        for c in children {
            let out = c.wait_with_output().unwrap();
            let text = String::from_utf8_lossy(&out.stdout).into_owned();
            assert!(text.contains("XCOMM-RACE-"), "{text}");
            won += text.matches("XCOMM-RACE-WON").count();
            if let Some(lost) = text.lines().find(|l| l.starts_with("XCOMM-RACE-LOST")) {
                assert!(
                    lost.contains("held by process")
                        || lost.contains("flock is held")
                        || lost.contains("being written")
                        || lost.contains("changing hands"),
                    "loser failed for another reason: {lost}"
                );
            }
        }
        assert_eq!(won, 1, "round {round}");
    }
}

#[test]
fn holder_reports_a_held_flock_with_an_invisible_pid() {
    // A live xcomm in another pid namespace: its pid is not visible here,
    // but it holds the flock. The port must not look free.
    let d = temp_dir();
    let path = d.join("LCK..ttyNS");
    fs::write(&path, hdb(dead_pid())).unwrap();
    let f = fs::File::open(&path).unwrap();
    rustix::fs::flock(
        f.as_fd(),
        rustix::fs::FlockOperation::NonBlockingLockExclusive,
    )
    .unwrap();
    match holder(&d, "ttyNS") {
        Err(LockError::Contended { why, .. }) => {
            assert!(why.contains("not visible here"), "{why}")
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn own_pid_in_a_lock_is_reported_not_cleared() {
    // It may be a leftover of an earlier process with the same pid, or a
    // live lock from another pid namespace sharing the directory: never
    // deleted, and the message says how to resolve it.
    let d = temp_dir();
    let path = d.join("LCK..ttyOWN");
    fs::write(&path, hdb(std::process::id())).unwrap();
    let e = acquire(&d, "ttyOWN").err().unwrap().to_string();
    assert!(e.contains("own pid"), "{e}");
    assert!(e.contains("remove it by hand"), "{e}");
    assert_eq!(fs::read_to_string(&path).unwrap(), hdb(std::process::id()));
}

#[test]
fn release_removes_the_lock_and_reports_success() {
    let d = temp_dir();
    let (lock, _) = acquire(&d, "ttyREL").unwrap();
    lock.release().unwrap();
    assert!(!d.join("LCK..ttyREL").exists());
}

#[test]
fn release_leaves_a_replaced_lock_alone() {
    let d = temp_dir();
    let path = d.join("LCK..ttyREL2");
    let (lock, _) = acquire(&d, "ttyREL2").unwrap();
    fs::remove_file(&path).unwrap();
    fs::write(&path, hdb(1)).unwrap();
    lock.release().unwrap();
    assert_eq!(fs::read_to_string(&path).unwrap(), hdb(1));
}

#[test]
fn errors_name_the_step_and_the_path_once() {
    if rustix::process::geteuid().is_root() {
        return;
    }
    let d = temp_dir();
    let ro = d.join("ro");
    fs::create_dir(&ro).unwrap();
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(&ro, fs::Permissions::from_mode(0o555)).unwrap();
    let e = acquire(&ro, "ttyRO").err().unwrap().to_string();
    fs::set_permissions(&ro, fs::Permissions::from_mode(0o755)).unwrap();
    assert!(e.contains("create "), "{e}");
    assert!(
        !e.contains(&format!("{}: {}", ro.display(), ro.display())),
        "{e}"
    );
}
