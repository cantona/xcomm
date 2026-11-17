// SPDX-FileCopyrightText: 2026 Su Kang Yin <paradyse@gmail.com>
// SPDX-License-Identifier: GPL-2.0-only

//! Where session sockets live.

use std::os::unix::fs::{DirBuilderExt, MetadataExt};
use std::path::{Path, PathBuf};

use super::CtlError;

/// Where a session puts its socket: `$XDG_RUNTIME_DIR/xcomm`, else
/// [`fallback_dir`]. Not created.
///
/// `XDG_RUNTIME_DIR` counts only when it is a directory of this user's: a
/// stale one (a tmux server outliving the login) or another user's (root
/// under `sudo -E`) would stop the session from starting, or leave a
/// directory the user cannot use.
pub fn socket_dir(env: &dyn Fn(&str) -> Option<String>) -> PathBuf {
    let me = rustix::process::geteuid().as_raw();
    let runtime = env("XDG_RUNTIME_DIR")
        .filter(|d| !d.is_empty())
        .filter(|d| std::fs::metadata(d).is_ok_and(|m| m.is_dir() && m.uid() == me));
    match runtime {
        Some(dir) => Path::new(&dir).join("xcomm"),
        None => fallback_dir(),
    }
}

/// `/tmp/xcomm-$UID`.
pub fn fallback_dir() -> PathBuf {
    PathBuf::from(format!(
        "/tmp/xcomm-{}",
        rustix::process::geteuid().as_raw()
    ))
}

/// Where a client looks: [`socket_dir`] first, then the fallback, since
/// the session's environment may not be the client's (a tmux server with a
/// stale `XDG_RUNTIME_DIR`, a client in a fresh login).
pub fn socket_dirs(env: &dyn Fn(&str) -> Option<String>) -> Vec<PathBuf> {
    let first = socket_dir(env);
    let fallback = fallback_dir();
    if first == fallback {
        vec![first]
    } else {
        vec![first, fallback]
    }
}

/// The socket directory, made 0700 if missing. One that is not a directory
/// of this user's that only they can enter is an error: in /tmp anyone can
/// make it first.
pub fn make_socket_dir(
    env: &dyn Fn(&str) -> Option<String>,
) -> Result<PathBuf, CtlError> {
    let dir = socket_dir(env);
    match std::fs::DirBuilder::new().mode(0o700).create(&dir) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(e) => {
            return Err(CtlError(format!("socket directory {}: {e}", dir.display())));
        }
    }
    check_dir(&dir)?;
    Ok(dir)
}

/// That `dir` is a directory (not a link to one), owned by this user and
/// closed to everyone else.
pub fn check_dir(dir: &Path) -> Result<(), CtlError> {
    let m = std::fs::symlink_metadata(dir)
        .map_err(|e| CtlError(format!("socket directory {}: {e}", dir.display())))?;
    let me = rustix::process::geteuid().as_raw();
    let bad = if !m.file_type().is_dir() {
        Some("not a directory".to_string())
    } else if m.uid() != me {
        Some(format!("owned by uid {}, not {me}", m.uid()))
    } else if m.mode() & 0o077 != 0 {
        Some(format!("mode {:o}, not 700", m.mode() & 0o777))
    } else {
        None
    };
    match bad {
        Some(why) => Err(CtlError(format!(
            "socket directory {}: {why}",
            dir.display()
        ))),
        None => Ok(()),
    }
}

/// The socket of the session on `tty_name` (as [`crate::lock::lock_name`]
/// gives it).
pub fn socket_path(dir: &Path, tty_name: &str) -> PathBuf {
    dir.join(format!("{tty_name}.sock"))
}

/// The tty name a device argument names: `ttyUSB1`, `/dev/ttyUSB1`, or a
/// link to it. A link whose device is gone (unplugged, the session waiting)
/// is followed as far as it goes, so the waiting session is still found.
pub fn tty_name(device: &str) -> Option<String> {
    let path = crate::session::device_path(device);
    let real = std::fs::canonicalize(&path).unwrap_or_else(|_| follow_links(&path));
    crate::lock::lock_name(&real)
}

/// Where the links from `path` lead, by their text, without needing the
/// end to exist.
fn follow_links(path: &Path) -> PathBuf {
    let mut at = path.to_path_buf();
    for _ in 0..40 {
        let Ok(target) = std::fs::read_link(&at) else {
            break;
        };
        at = match at.parent() {
            Some(dir) if target.is_relative() => normalize(&dir.join(target)),
            _ => target,
        };
    }
    at
}

/// Removes `.` and `..` by the text alone.
fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in path.components() {
        match c {
            std::path::Component::ParentDir => {
                out.pop();
            }
            std::path::Component::CurDir => {}
            other => out.push(other),
        }
    }
    out
}
