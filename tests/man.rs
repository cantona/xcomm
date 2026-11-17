// SPDX-FileCopyrightText: 2026 Su Kang Yin <paradyse@gmail.com>
// SPDX-License-Identifier: GPL-2.0-only

mod support;

use std::path::Path;
use std::process::Command;
use std::time::Duration;

fn page() -> String {
    std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("xcomm.1"))
        .unwrap()
}

#[test]
fn piped_it_writes_the_built_in_page() {
    let o = Command::new(env!("CARGO_BIN_EXE_xcomm"))
        .arg("man")
        .output()
        .unwrap();
    assert_eq!(o.status.code(), Some(0), "{o:?}");
    assert_eq!(String::from_utf8(o.stdout).unwrap(), page());
    let o = Command::new(env!("CARGO_BIN_EXE_xcomm"))
        .args(["man", "extra"])
        .output()
        .unwrap();
    assert_eq!(o.status.code(), Some(2), "{o:?}");
}

#[test]
fn on_a_terminal_it_opens_the_page_in_man_and_cleans_up() {
    let dir = support::lock_dir("man");
    let bin = dir.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let (seen, at) = (dir.join("seen.1"), dir.join("at"));
    // A man that keeps what it was given, and where.
    support::write_exe(
        &bin.join("man"),
        &format!(
            "#!/bin/sh\n/bin/cat \"$1\" > '{}'\necho \"$1\" > '{}'\n",
            seen.display(),
            at.display()
        ),
    );
    let path = bin.to_str().unwrap();
    let mut s = support::spawn_with_env(
        env!("CARGO_BIN_EXE_xcomm"),
        &["man"],
        &dir,
        &[("PATH", path)],
    );
    assert!(s.wait_exit(Duration::from_secs(10)).success());
    assert_eq!(std::fs::read_to_string(&seen).unwrap(), page());
    let given = std::fs::read_to_string(&at).unwrap();
    assert!(given.trim().contains('/'), "{given}");
    assert!(!Path::new(given.trim()).exists(), "{given} is left");
    // A file already at the name it would take (anyone can make one in a
    // shared /tmp) is passed over, not opened.
    let tmp = dir.join("tmp");
    std::fs::create_dir_all(&tmp).unwrap();
    let mut s = support::spawn_with_env(
        "/bin/sh",
        &[
            "-c",
            &format!(
                ": > \"$TMPDIR/xcomm-$$-0.1\"; exec '{}' man",
                env!("CARGO_BIN_EXE_xcomm")
            ),
        ],
        &dir,
        &[("PATH", path), ("TMPDIR", tmp.to_str().unwrap())],
    );
    assert!(s.wait_exit(Duration::from_secs(10)).success());
    assert_eq!(std::fs::read_to_string(&seen).unwrap(), page());
    let given = std::fs::read_to_string(&at).unwrap();
    assert!(given.trim().ends_with("-1.1"), "{given}");
    let left: Vec<_> = std::fs::read_dir(&tmp)
        .unwrap()
        .flatten()
        .map(|e| e.file_name())
        .collect();
    assert_eq!(left.len(), 1, "{left:?}");
    // A Ctrl-C in the pager reaches xcomm too: it waits for man and still
    // cleans up.
    support::write_exe(
        &bin.join("man"),
        &format!(
            "#!/bin/sh\ntrap '' INT\necho \"$1\" > '{}'\nkill -INT 0\n/bin/cat \"$1\" > '{}'\n",
            at.display(),
            seen.display()
        ),
    );
    let mut s = support::spawn_with_env(
        env!("CARGO_BIN_EXE_xcomm"),
        &["man"],
        &dir,
        &[("PATH", path)],
    );
    assert!(s.wait_exit(Duration::from_secs(10)).success());
    assert_eq!(std::fs::read_to_string(&seen).unwrap(), page());
    let given = std::fs::read_to_string(&at).unwrap();
    assert!(!Path::new(given.trim()).exists(), "{given} is left");
    // A man that a Ctrl-C ends (macOS man does): xcomm ends the same way,
    // with no error, and still cleans up.
    support::write_exe(
        &bin.join("man"),
        &format!(
            "#!/bin/sh\necho \"$1\" > '{}'\nkill -INT $$\n",
            at.display()
        ),
    );
    let mut s = support::spawn_with_env(
        env!("CARGO_BIN_EXE_xcomm"),
        &["man"],
        &dir,
        &[("PATH", path)],
    );
    // Ended by the signal, it never got to say an error.
    use std::os::unix::process::ExitStatusExt;
    assert_eq!(
        s.wait_exit(Duration::from_secs(10)).signal(),
        Some(libc::SIGINT)
    );
    let given = std::fs::read_to_string(&at).unwrap();
    assert!(!Path::new(given.trim()).exists(), "{given} is left");
    // Ended by SIGQUIT: the status a shell shows, not a core of xcomm's.
    support::write_exe(
        &bin.join("man"),
        &format!(
            "#!/bin/sh\necho \"$1\" > '{}'\nkill -QUIT $$\n",
            at.display()
        ),
    );
    let mut s = support::spawn_with_env(
        env!("CARGO_BIN_EXE_xcomm"),
        &["man"],
        &dir,
        &[("PATH", path)],
    );
    assert_eq!(s.wait_exit(Duration::from_secs(10)).code(), Some(131));
    let given = std::fs::read_to_string(&at).unwrap();
    assert!(!Path::new(given.trim()).exists(), "{given} is left");
    // A SIGTERM for xcomm alone: passed on to man, and xcomm ends by it
    // once the file is gone.
    std::fs::remove_file(&at).unwrap();
    support::write_exe(
        &bin.join("man"),
        &format!(
            "#!/bin/sh\necho \"$1\" > '{}'\nexec /bin/sleep 20\n",
            at.display()
        ),
    );
    let mut s = support::spawn_with_env(
        env!("CARGO_BIN_EXE_xcomm"),
        &["man"],
        &dir,
        &[("PATH", path)],
    );
    let until = std::time::Instant::now() + Duration::from_secs(10);
    while !at.exists() {
        assert!(std::time::Instant::now() < until, "man never started");
        std::thread::sleep(Duration::from_millis(20));
    }
    std::thread::sleep(Duration::from_millis(100));
    let started = std::time::Instant::now();
    let pid = rustix::process::Pid::from_child(&s.child);
    rustix::process::kill_process(pid, rustix::process::Signal::TERM).unwrap();
    assert_eq!(
        s.wait_exit(Duration::from_secs(10)).signal(),
        Some(libc::SIGTERM)
    );
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "it waited for man"
    );
    let given = std::fs::read_to_string(&at).unwrap();
    assert!(!Path::new(given.trim()).exists(), "{given} is left");
    // A SIGINT the parent left blocked: man (which unblocks its own) dies
    // of it, and xcomm still ends by it rather than with an error.
    support::write_exe(
        &bin.join("man"),
        &format!(
            "#!/bin/sh\necho \"$1\" > '{}'\nexec /usr/bin/perl -MPOSIX -e \
             'sigprocmask(SIG_UNBLOCK, POSIX::SigSet->new(SIGINT)); kill INT => $$; sleep 5'\n",
            at.display()
        ),
    );
    let host = support::pty();
    let mut child = {
        use std::os::unix::process::CommandExt;
        let mut c = std::process::Command::new(env!("CARGO_BIN_EXE_xcomm"));
        c.arg("man")
            .env("PATH", path)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::from(host.slave.try_clone().unwrap()))
            .stderr(std::process::Stdio::from(host.slave.try_clone().unwrap()));
        // SAFETY: only sigprocmask between fork and exec.
        unsafe {
            c.pre_exec(|| {
                let mut set: libc::sigset_t = std::mem::zeroed();
                libc::sigemptyset(&mut set);
                libc::sigaddset(&mut set, libc::SIGINT);
                libc::sigprocmask(libc::SIG_BLOCK, &set, std::ptr::null_mut());
                Ok(())
            });
        }
        c.spawn().unwrap()
    };
    let status = child.wait().unwrap();
    assert_eq!(status.signal(), Some(libc::SIGINT), "{status:?}");
    let given = std::fs::read_to_string(&at).unwrap();
    assert!(!Path::new(given.trim()).exists(), "{given} is left");
    // A parent that ignores SIGCHLD: man's status is still waited for.
    support::write_exe(
        &bin.join("man"),
        &format!("#!/bin/sh\necho \"$1\" > '{}'\n", at.display()),
    );
    let status = {
        use std::os::unix::process::CommandExt;
        let mut c = std::process::Command::new(env!("CARGO_BIN_EXE_xcomm"));
        c.arg("man")
            .env("PATH", path)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::from(host.slave.try_clone().unwrap()))
            .stderr(std::process::Stdio::from(host.slave.try_clone().unwrap()));
        // SAFETY: only signal() between fork and exec.
        unsafe {
            c.pre_exec(|| {
                libc::signal(libc::SIGCHLD, libc::SIG_IGN);
                Ok(())
            });
        }
        c.status().unwrap()
    };
    assert_eq!(status.code(), Some(0), "{status:?}");
    let given = std::fs::read_to_string(&at).unwrap();
    assert!(!Path::new(given.trim()).exists(), "{given} is left");
    // A SIGTERM the parent ignores stays ignored, for xcomm and man alike
    // (as for `man ls`): both go on, and man's own end is xcomm's.
    std::fs::remove_file(&at).unwrap();
    support::write_exe(
        &bin.join("man"),
        &format!(
            "#!/bin/sh\necho \"$1\" > '{}'\n/bin/sleep 2\n",
            at.display()
        ),
    );
    let mut child = {
        use std::os::unix::process::CommandExt;
        let mut c = std::process::Command::new(env!("CARGO_BIN_EXE_xcomm"));
        c.arg("man")
            .env("PATH", path)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::from(host.slave.try_clone().unwrap()))
            .stderr(std::process::Stdio::from(host.slave.try_clone().unwrap()));
        // SAFETY: only signal() between fork and exec.
        unsafe {
            c.pre_exec(|| {
                libc::signal(libc::SIGTERM, libc::SIG_IGN);
                Ok(())
            });
        }
        c.spawn().unwrap()
    };
    let until = std::time::Instant::now() + Duration::from_secs(10);
    while !at.exists() {
        assert!(std::time::Instant::now() < until, "man never started");
        std::thread::sleep(Duration::from_millis(20));
    }
    let pid = rustix::process::Pid::from_child(&child);
    rustix::process::kill_process(pid, rustix::process::Signal::TERM).unwrap();
    std::thread::sleep(Duration::from_millis(300));
    assert!(
        child.try_wait().unwrap().is_none(),
        "xcomm ended by the ignored SIGTERM"
    );
    let status = child.wait().unwrap();
    assert_eq!(status.code(), Some(0), "{status:?}");
    let given = std::fs::read_to_string(&at).unwrap();
    assert!(!Path::new(given.trim()).exists(), "{given} is left");
    support::write_exe(
        &bin.join("man"),
        &format!(
            "#!/bin/sh\n/bin/cat \"$1\" > '{}'\necho \"$1\" > '{}'\n",
            seen.display(),
            at.display()
        ),
    );
    // An empty TMPDIR: still a path man takes as a file.
    let mut s = support::spawn_with_env(
        env!("CARGO_BIN_EXE_xcomm"),
        &["man"],
        &dir,
        &[("PATH", path), ("TMPDIR", "")],
    );
    assert!(s.wait_exit(Duration::from_secs(10)).success());
    let given = std::fs::read_to_string(&at).unwrap();
    assert!(given.starts_with("/tmp/"), "{given}");
    // A man that fails: the way round it is said.
    support::write_exe(&bin.join("man"), "#!/bin/sh\nexit 16\n");
    let mut s = support::spawn_with_env(
        env!("CARGO_BIN_EXE_xcomm"),
        &["man"],
        &dir,
        &[("PATH", path)],
    );
    let out = s.read_after_exit(b"xcomm.1", Duration::from_secs(10));
    assert_eq!(s.wait_exit(Duration::from_secs(10)).code(), Some(1));
    let out = String::from_utf8_lossy(&out);
    assert!(out.contains("xcomm man > xcomm.1"), "{out}");
    // Without man: it says how to get the page anyway.
    let mut s = support::spawn_with_env(
        env!("CARGO_BIN_EXE_xcomm"),
        &["man"],
        &dir,
        &[("PATH", dir.join("none").to_str().unwrap())],
    );
    let out = s.read_after_exit(b"xcomm.1", Duration::from_secs(10));
    assert_eq!(s.wait_exit(Duration::from_secs(10)).code(), Some(1));
    let out = String::from_utf8_lossy(&out);
    assert!(out.contains("xcomm man > xcomm.1"), "{out}");
}
