//! The capture file: device output copied to a file, in batches.
//!
//! Writes are synchronous. A capture on a hard-mounted network filesystem
//! whose server stops answering blocks the session in a wait no handled
//! signal interrupts (only SIGKILL ends it), as it would any program
//! writing there.

use std::fs::File;
use std::io;
use std::os::fd::IntoRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

/// Bytes gathered before a write to the file, and the most a reader may
/// fall behind before the capture stops.
pub const CAPTURE_BUF: usize = 64 * 1024;
/// How long closing waits for a pipe reader to take the last bytes.
const CLOSE_WAIT: std::time::Duration = std::time::Duration::from_secs(1);

pub struct Capture {
    file: File,
    path: PathBuf,
    /// Never grows past CAPTURE_BUF.
    buf: Vec<u8>,
    /// Bytes taken since the capture started.
    total: u64,
}

/// How a write to the file went.
enum Wrote {
    All,
    /// This many bytes; the file takes no more now (EAGAIN, EINTR).
    Some(usize),
}

impl Capture {
    /// Opens `path` for appending, creating it (mode 0644 before the umask).
    ///
    /// Non-blocking, so a pipe or FIFO reader cannot hold up the session: a
    /// FIFO with no reader fails here (ENXIO) instead of waiting for one,
    /// and what a reader is not ready for waits in the buffer for the next
    /// flush. Regular files are not affected.
    pub fn start(path: &Path) -> io::Result<Capture> {
        let file = std::fs::OpenOptions::new()
            .append(true)
            .create(true)
            .mode(0o644)
            .custom_flags(libc::O_NONBLOCK)
            .open(path)
            .map_err(|e| io::Error::new(e.kind(), format!("{}: {e}", path.display())))?;
        Ok(Capture {
            file,
            path: path.to_path_buf(),
            buf: Vec::with_capacity(CAPTURE_BUF),
            total: 0,
        })
    }

    /// Takes `bytes`, writing to the file when the buffer fills. Fails when
    /// a reader has fallen more than the buffer behind.
    pub fn write(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.total += bytes.len() as u64;
        if bytes.len() > CAPTURE_BUF - self.buf.len() {
            self.flush()?;
        }
        let mut rest = bytes;
        // Larger than the buffer: what the file takes now goes straight in.
        if rest.len() > CAPTURE_BUF && self.buf.is_empty() {
            match write_some(&self.file, rest)? {
                Wrote::All => return Ok(()),
                Wrote::Some(n) => rest = &rest[n..],
            }
        }
        if rest.len() > CAPTURE_BUF - self.buf.len() {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "the reader is not keeping up",
            ));
        }
        self.buf.extend_from_slice(rest);
        Ok(())
    }

    /// Writes what is buffered. What the file does not take now (a pipe
    /// reader behind) stays for the next flush. On an error the buffer is
    /// dropped.
    pub fn flush(&mut self) -> io::Result<()> {
        match write_some(&self.file, &self.buf) {
            Ok(Wrote::All) => self.buf.clear(),
            Ok(Wrote::Some(n)) => {
                self.buf.drain(..n);
            }
            Err(e) => {
                self.buf.clear();
                return Err(e);
            }
        }
        Ok(())
    }

    /// Flushes and closes the file, reporting what close(2) says (a
    /// network filesystem or quota can fail only there). A pipe reader
    /// behind gets up to CLOSE_WAIT to take the last bytes (a signal ends
    /// the wait); what it leaves is an error.
    pub fn close(mut self) -> io::Result<()> {
        let mut flushed = self.flush();
        let deadline = std::time::Instant::now() + CLOSE_WAIT;
        while flushed.is_ok() && self.is_dirty() {
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            if left.is_zero() {
                break;
            }
            let left = rustix::event::Timespec::try_from(left).expect("under CLOSE_WAIT");
            let mut fds = [rustix::event::PollFd::new(
                &self.file,
                rustix::event::PollFlags::OUT,
            )];
            if rustix::event::poll(&mut fds, Some(&left)).is_err() {
                break;
            }
            flushed = self.flush();
        }
        let pending = !self.buf.is_empty();
        // SAFETY: the descriptor is owned by `file` and closed once, here.
        let closed = match unsafe { libc::close(self.file.into_raw_fd()) } {
            0 => Ok(()),
            _ => match io::Error::last_os_error() {
                // Linux closes the descriptor even when close reports EINTR.
                e if e.kind() == io::ErrorKind::Interrupted => Ok(()),
                e => Err(e),
            },
        };
        flushed?;
        closed?;
        if pending {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "the reader did not take the last bytes",
            ));
        }
        Ok(())
    }

    /// Whether bytes are waiting in the buffer.
    pub fn is_dirty(&self) -> bool {
        !self.buf.is_empty()
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Bytes taken since the capture started.
    pub fn total(&self) -> u64 {
        self.total
    }
}

/// Writes as much of `bytes` as the file takes now. EAGAIN (a reader
/// behind) and EINTR (a signal for the loop to see) stop early without an
/// error.
fn write_some(file: &File, bytes: &[u8]) -> io::Result<Wrote> {
    let mut done = 0;
    while done < bytes.len() {
        match rustix::io::write(file, &bytes[done..]) {
            Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
            Ok(n) => done += n,
            Err(rustix::io::Errno::AGAIN | rustix::io::Errno::INTR) => {
                return Ok(Wrote::Some(done));
            }
            Err(e) => return Err(e.into()),
        }
    }
    Ok(Wrote::All)
}
