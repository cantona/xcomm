// SPDX-FileCopyrightText: 2026 Su Kang Yin <paradyse@gmail.com>
// SPDX-License-Identifier: GPL-2.0-only

//! The last bytes the device sent, by absolute offset: byte N is the Nth
//! byte read from the device since the session started.

/// A read asked for bytes the ring no longer holds.
#[derive(Debug, PartialEq, Eq)]
pub struct Evicted {
    pub requested: u64,
    pub oldest: u64,
}

impl std::fmt::Display for Evicted {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "offset {} evicted, oldest {}",
            self.requested, self.oldest
        )
    }
}

impl std::error::Error for Evicted {}

/// The memory for a ring could not be had.
#[derive(Debug, PartialEq, Eq)]
pub struct NoMemory(pub usize);

impl std::fmt::Display for NoMemory {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "cannot allocate {} bytes", self.0)
    }
}

impl std::error::Error for NoMemory {}

pub struct Ring {
    buf: Box<[u8]>,
    /// Offset of the next byte.
    end: u64,
}

impl Ring {
    /// Holds the last `size` bytes (at least one). Panics when the memory
    /// cannot be had; see [`try_new`](Self::try_new).
    pub fn new(size: usize) -> Ring {
        Ring::try_new(size).expect("memory for the ring")
    }

    /// Holds the last `size` bytes (at least one), or fails when the
    /// memory cannot be had. The buffer comes zeroed from the allocator, so
    /// pages are only touched as the ring fills.
    pub fn try_new(size: usize) -> Result<Ring, NoMemory> {
        let size = size.max(1);
        let layout = std::alloc::Layout::array::<u8>(size).map_err(|_| NoMemory(size))?;
        // SAFETY: the layout is not zero-sized (size >= 1).
        let ptr = unsafe { std::alloc::alloc_zeroed(layout) };
        if ptr.is_null() {
            return Err(NoMemory(size));
        }
        // SAFETY: `ptr` is a zeroed allocation of `size` bytes made by the
        // global allocator with the layout Box<[u8]> frees it with.
        let buf = unsafe { Box::from_raw(std::ptr::slice_from_raw_parts_mut(ptr, size)) };
        Ok(Ring { buf, end: 0 })
    }

    pub fn capacity(&self) -> usize {
        self.buf.len()
    }

    /// Appends `bytes`: at most two copies. Of a push longer than the ring,
    /// the tail is kept; the offsets still count every byte.
    pub fn push(&mut self, bytes: &[u8]) {
        let skip = bytes.len().saturating_sub(self.buf.len());
        let kept = &bytes[skip..];
        let at = self.index(self.end + skip as u64);
        let first = kept.len().min(self.buf.len() - at);
        self.buf[at..at + first].copy_from_slice(&kept[..first]);
        self.buf[..kept.len() - first].copy_from_slice(&kept[first..]);
        self.end += bytes.len() as u64;
    }

    /// Offset of the next byte to come.
    pub fn end(&self) -> u64 {
        self.end
    }

    /// Offset of the oldest byte held.
    pub fn oldest(&self) -> u64 {
        self.end.saturating_sub(self.buf.len() as u64)
    }

    /// Appends to `out` up to `max` bytes from offset `since`, and returns
    /// the offset after them. From `end` (or beyond: not sent yet) nothing
    /// is appended and `since` comes back.
    pub fn read(
        &self,
        since: u64,
        max: usize,
        out: &mut Vec<u8>,
    ) -> Result<u64, Evicted> {
        let (a, b) = self.slices(since, max)?;
        out.extend_from_slice(a);
        out.extend_from_slice(b);
        Ok(since + (a.len() + b.len()) as u64)
    }

    /// The bytes [`read`](Self::read) would append, in place: the second
    /// slice is the part after the wrap, often empty.
    pub fn slices(&self, since: u64, max: usize) -> Result<(&[u8], &[u8]), Evicted> {
        let oldest = self.oldest();
        if since < oldest {
            return Err(Evicted {
                requested: since,
                oldest,
            });
        }
        let n = usize::try_from(self.end.saturating_sub(since))
            .unwrap_or(usize::MAX)
            .min(max);
        let at = self.index(since);
        let first = n.min(self.buf.len() - at);
        Ok((&self.buf[at..at + first], &self.buf[..n - first]))
    }

    fn index(&self, offset: u64) -> usize {
        // Below the length, which is a usize.
        (offset % self.buf.len() as u64) as usize
    }
}
