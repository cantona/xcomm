// SPDX-FileCopyrightText: 2026 Su Kang Yin <paradyse@gmail.com>
// SPDX-License-Identifier: GPL-2.0-only

//! lrzsz on the device's side of a fake line. Skipped, saying so, where
//! lrzsz is not installed.

use std::io::{Read, Write};
use std::os::fd::{AsFd, OwnedFd};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use rustix::event::{PollFd, PollFlags, poll};

use super::write_all;

const T: Duration = Duration::from_secs(30);

/// The name `name` is installed under: as is, or with lrzsz's own `l`
/// prefix (Homebrew installs the X- and YMODEM programs only so).
pub fn lrzsz(name: &str) -> Option<String> {
    let path = std::env::var_os("PATH")?;
    [name.to_string(), format!("l{name}")]
        .into_iter()
        .find(|n| std::env::split_paths(&path).any(|d| d.join(n).is_file()))
}

/// The installed names of `names`, or None after saying why the test is
/// skipped.
pub fn need(names: &[&str]) -> Option<Vec<String>> {
    let found: Option<Vec<String>> = names.iter().map(|n| lrzsz(n)).collect();
    if found.is_none() {
        eprintln!("skipped: lrzsz ({}) is not on PATH", names.join(", "));
    }
    found
}

/// A program on the device's side of the line: lrzsz run on pipes, with
/// the pipes relayed to the device pty's master (lrzsz on the master
/// itself would change the termios xcomm set on the slave).
pub struct Remote {
    child: Child,
    stop: Arc<AtomicBool>,
}

impl Remote {
    pub fn start(program: &str, args: &[&str], dir: &Path, master: &OwnedFd) -> Remote {
        let mut child = Command::new(program)
            .args(args)
            .current_dir(dir)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let mut to_child = child.stdin.take().unwrap();
        let from_line = master.try_clone().unwrap();
        let done = stop.clone();
        std::thread::spawn(move || {
            let mut buf = [0u8; 4096];
            while !done.load(Ordering::Relaxed) {
                let mut fds = [PollFd::new(&from_line, PollFlags::IN)];
                let wait = rustix::event::Timespec {
                    tv_sec: 0,
                    tv_nsec: 20_000_000,
                };
                if poll(&mut fds, Some(&wait)).unwrap_or(0) == 0 {
                    continue;
                }
                let n = match rustix::io::read(&from_line, &mut buf) {
                    Ok(n) if n > 0 => n,
                    _ => return,
                };
                if to_child.write_all(&buf[..n]).is_err() {
                    return;
                }
            }
        });
        let mut from_child = child.stdout.take().unwrap();
        let to_line = master.try_clone().unwrap();
        let done = stop.clone();
        std::thread::spawn(move || {
            let mut buf = [0u8; 4096];
            loop {
                match from_child.read(&mut buf) {
                    Ok(n) if n > 0 => write_all(to_line.as_fd(), &buf[..n]),
                    _ => break,
                }
            }
            done.store(true, Ordering::Relaxed);
        });
        Remote { child, stop }
    }

    /// Waits for `path` to hold `expected`, then stops the program. An rz
    /// can wait 30 s for the sender's last "OO" after the file is whole
    /// (lrzsz does that without xcomm too); that is not what is tested.
    pub fn received(mut self, path: &Path, expected: &[u8]) {
        let started = std::time::Instant::now();
        while std::fs::read(path).ok().as_deref() != Some(expected) {
            assert!(
                started.elapsed() < T,
                "{} never held the file",
                path.display()
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
        self.stop.store(true, Ordering::Relaxed);
    }

    pub fn wait(mut self) -> std::process::ExitStatus {
        let status = super::wait_exit(&mut self.child, T);
        self.stop.store(true, Ordering::Relaxed);
        status
    }
}
