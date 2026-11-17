// SPDX-FileCopyrightText: 2026 Su Kang Yin <paradyse@gmail.com>
// SPDX-License-Identifier: GPL-2.0-only

//! Unix socket helpers shared by the clients xcomm runs inside a session
//! or a listing, where waiting on another process is not an option.

use std::io;
use std::os::unix::net::UnixStream;
use std::path::Path;

/// Connects without waiting: a server that is not accepting (stopped,
/// its queue full) fails at once rather than holding the caller up.
pub fn connect_now(path: &Path) -> io::Result<UnixStream> {
    use rustix::net::{AddressFamily, SocketAddrUnix, SocketType, connect};
    // Close-on-exec: a program another thread starts must not keep the
    // session's client open.
    #[cfg(not(target_os = "macos"))]
    let fd = rustix::net::socket_with(
        AddressFamily::UNIX,
        SocketType::STREAM,
        rustix::net::SocketFlags::CLOEXEC | rustix::net::SocketFlags::NONBLOCK,
        None,
    )?;
    // macOS has neither flag for socket(2); set as std sets them there.
    #[cfg(target_os = "macos")]
    let fd = {
        let fd = rustix::net::socket(AddressFamily::UNIX, SocketType::STREAM, None)?;
        rustix::io::fcntl_setfd(&fd, rustix::io::FdFlags::CLOEXEC)?;
        let flags = rustix::fs::fcntl_getfl(&fd)?;
        rustix::fs::fcntl_setfl(&fd, flags | rustix::fs::OFlags::NONBLOCK)?;
        fd
    };
    connect(&fd, &SocketAddrUnix::new(path)?)?;
    let stream = UnixStream::from(fd);
    stream.set_nonblocking(false)?;
    Ok(stream)
}
