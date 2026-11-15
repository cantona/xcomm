//! A fixed-capacity byte queue for output to the device. It never grows,
//! so the loop does not allocate; a full queue tells the caller to stop
//! reading its input.

use std::io;
use std::os::fd::BorrowedFd;

pub struct OutQueue {
    buf: Box<[u8]>,
    head: usize,
    len: usize,
    /// Bytes written out since the queue was made.
    written: u64,
}

impl OutQueue {
    pub fn with_capacity(cap: usize) -> OutQueue {
        assert!(cap > 0, "OutQueue needs a capacity");
        OutQueue {
            buf: vec![0; cap].into_boxed_slice(),
            head: 0,
            len: 0,
            written: 0,
        }
    }

    pub fn free(&self) -> usize {
        self.buf.len() - self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn len(&self) -> usize {
        self.len
    }

    /// Bytes written out so far: with [`len`](Self::len), where a byte
    /// queued now stands.
    pub fn written(&self) -> u64 {
        self.written
    }

    /// Appends all of `bytes`, or nothing if they do not fit.
    pub fn push(&mut self, bytes: &[u8]) -> bool {
        if bytes.len() > self.free() {
            return false;
        }
        let cap = self.buf.len();
        let tail = (self.head + self.len) % cap;
        let first = bytes.len().min(cap - tail);
        self.buf[tail..tail + first].copy_from_slice(&bytes[..first]);
        self.buf[..bytes.len() - first].copy_from_slice(&bytes[first..]);
        self.len += bytes.len();
        true
    }

    /// One write of the contiguous part at the head. EAGAIN and EINTR are
    /// `Ok(0)`.
    pub fn write_to(&mut self, fd: BorrowedFd<'_>) -> io::Result<usize> {
        if self.len == 0 {
            return Ok(0);
        }
        let end = (self.head + self.len).min(self.buf.len());
        match rustix::io::write(fd, &self.buf[self.head..end]) {
            Ok(n) => {
                self.head = (self.head + n) % self.buf.len();
                self.len -= n;
                self.written += n as u64;
                if self.len == 0 {
                    self.head = 0;
                }
                Ok(n)
            }
            // EINTR: a caught signal; the loop sees it through the signal
            // pipe and writes the rest later.
            Err(rustix::io::Errno::AGAIN | rustix::io::Errno::INTR) => Ok(0),
            Err(e) => Err(e.into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::OutQueue;
    use std::os::fd::AsFd;

    fn drain(q: &mut OutQueue) -> Vec<u8> {
        let (r, w) = std::io::pipe().unwrap();
        while !q.is_empty() {
            q.write_to(w.as_fd()).unwrap();
        }
        drop(w);
        let mut out = Vec::new();
        std::io::Read::read_to_end(&mut &r, &mut out).unwrap();
        out
    }

    #[test]
    fn keeps_order_across_wraparound() {
        let mut q = OutQueue::with_capacity(8);
        assert!(q.push(b"abcde"));
        assert_eq!(drain(&mut q), b"abcde");
        assert!(q.push(b"123456"));
        assert!(q.push(b"78"));
        assert!(!q.push(b"9"), "full");
        assert_eq!(q.free(), 0);
        assert_eq!(drain(&mut q), b"12345678");
        assert_eq!(q.free(), 8);
    }

    #[test]
    fn push_is_all_or_nothing() {
        let mut q = OutQueue::with_capacity(4);
        assert!(q.push(b"ab"));
        assert!(!q.push(b"cde"));
        assert_eq!(drain(&mut q), b"ab");
    }
}
