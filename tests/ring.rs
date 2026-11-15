mod support;

use xcomm::ring::{Evicted, Ring};

fn read_all(r: &Ring, since: u64) -> (Vec<u8>, u64) {
    let mut out = Vec::new();
    let next = r.read(since, usize::MAX, &mut out).unwrap();
    (out, next)
}

#[test]
fn capacity_is_the_size_asked_for() {
    // Not rounded up: --ring-size 513m must not take 1 GiB.
    assert_eq!(Ring::new(1000).capacity(), 1000);
    assert_eq!(Ring::new(1024).capacity(), 1024);
    assert_eq!(Ring::new(0).capacity(), 1);
}

#[test]
fn a_ring_too_big_for_memory_is_an_error() {
    assert_eq!(
        Ring::try_new(usize::MAX).err().map(|e| e.to_string()),
        Some(format!("cannot allocate {} bytes", usize::MAX))
    );
}

/// The session allocates the ring before it takes the lock, the port or
/// the terminal: a failure leaves nothing behind. RLIMIT_AS makes the
/// allocator fail (macOS does not enforce it).
#[cfg(target_os = "linux")]
#[test]
fn a_session_without_memory_for_its_ring_ends_before_it_starts() {
    use std::os::unix::process::CommandExt;
    let dev = support::pty();
    let locks = support::lock_dir("ring-memory");
    let mut cmd = std::process::Command::new(env!("CARGO_BIN_EXE_xcomm"));
    cmd.args(["--ring-size", "1g"])
        .arg(&dev.slave_path)
        .env("XCOMM_LOCK_DIR", &locks)
        .stdin(std::process::Stdio::null());
    // SAFETY: setrlimit is async-signal-safe.
    unsafe {
        cmd.pre_exec(|| {
            let limit = libc::rlimit {
                rlim_cur: 512 << 20,
                rlim_max: 512 << 20,
            };
            if libc::setrlimit(libc::RLIMIT_AS, &limit) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let out = cmd.output().unwrap();
    let err = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(1), "{err}");
    assert!(
        err.contains("--ring-size 1073741824: cannot allocate"),
        "{err}"
    );
    assert_eq!(
        std::fs::read_dir(&locks).unwrap().count(),
        0,
        "a lock was left"
    );
    let _ = std::fs::remove_dir_all(&locks);
}

#[test]
fn a_ring_of_odd_size_wraps() {
    let mut r = Ring::new(5);
    r.push(b"abc");
    r.push(b"defg");
    assert_eq!((r.oldest(), r.end()), (2, 7));
    assert_eq!(read_all(&r, 2), (b"cdefg".to_vec(), 7));
    r.push(b"0123456789");
    assert_eq!(read_all(&r, 12), (b"56789".to_vec(), 17));
}

#[test]
fn offsets_count_from_zero() {
    let mut r = Ring::new(16);
    assert_eq!((r.oldest(), r.end()), (0, 0));
    r.push(b"hello");
    assert_eq!((r.oldest(), r.end()), (0, 5));
    assert_eq!(read_all(&r, 0), (b"hello".to_vec(), 5));
    assert_eq!(read_all(&r, 2), (b"llo".to_vec(), 5));
}

#[test]
fn a_read_across_the_wrap_is_contiguous() {
    let mut r = Ring::new(8);
    r.push(b"abcdef");
    r.push(b"ghij");
    assert_eq!((r.oldest(), r.end()), (2, 10));
    assert_eq!(read_all(&r, 2), (b"cdefghij".to_vec(), 10));
    assert_eq!(read_all(&r, 5), (b"fghij".to_vec(), 10));
}

#[test]
fn a_read_at_the_end_returns_nothing() {
    let mut r = Ring::new(8);
    r.push(b"abc");
    assert_eq!(read_all(&r, 3), (Vec::new(), 3));
    // Not sent yet.
    assert_eq!(read_all(&r, 7), (Vec::new(), 7));
}

#[test]
fn an_evicted_offset_is_an_error() {
    let mut r = Ring::new(4);
    r.push(b"abcdefgh");
    let mut out = Vec::new();
    assert_eq!(
        r.read(3, 10, &mut out),
        Err(Evicted {
            requested: 3,
            oldest: 4
        })
    );
    assert!(out.is_empty());
    assert_eq!(
        Evicted {
            requested: 3,
            oldest: 4
        }
        .to_string(),
        "offset 3 evicted, oldest 4"
    );
}

#[test]
fn a_push_longer_than_the_ring_keeps_its_tail() {
    let mut r = Ring::new(4);
    r.push(b"xy");
    r.push(b"0123456789");
    assert_eq!(r.end(), 12);
    assert_eq!(read_all(&r, 8), (b"6789".to_vec(), 12));
}

#[test]
fn max_limits_a_read() {
    let mut r = Ring::new(8);
    r.push(b"abcdefghij");
    let mut out = b"kept:".to_vec();
    assert_eq!(r.read(3, 4, &mut out), Ok(7));
    assert_eq!(out, b"kept:defg");
    let mut out = Vec::new();
    assert_eq!(r.read(3, 0, &mut out), Ok(3));
    assert!(out.is_empty());
}

#[test]
fn random_pushes_match_a_plain_vec() {
    let mut x: u64 = 0x2545_f491_4f6c_dd1d;
    let mut next = move || {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        x
    };
    let mut r = Ring::new(250);
    let mut model: Vec<u8> = Vec::new();
    for i in 0..10_000 {
        let len = (next() % 300) as usize;
        let bytes: Vec<u8> = (0..len).map(|_| next() as u8).collect();
        r.push(&bytes);
        model.extend_from_slice(&bytes);
        assert_eq!(r.end(), model.len() as u64);
        let oldest = model.len().saturating_sub(250) as u64;
        assert_eq!(r.oldest(), oldest);
        let since = oldest + next() % (r.end() - oldest + 1);
        let max = (next() % 300) as usize;
        let mut out = Vec::new();
        let got = r.read(since, max, &mut out).unwrap();
        let from = since as usize;
        let to = (from + max).min(model.len());
        assert_eq!(out, &model[from..to], "push {i}");
        assert_eq!(got, to as u64);
        if oldest > 0 {
            assert!(r.read(oldest - 1, 1, &mut Vec::new()).is_err());
        }
    }
}
