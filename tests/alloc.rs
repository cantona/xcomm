//! The session loop must not allocate once running.

mod support;

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::ops::ControlFlow;
use std::os::fd::AsFd;

use xcomm::capture::Capture;
use xcomm::cli::{Modifier, PrefixKey};
use xcomm::ctl::CtlMode;
use xcomm::ctl::proto::Status;
use xcomm::ctl::server::{CtlServer, Session};
use xcomm::hosttty::SignalPipe;
use xcomm::session::{
    CommandOutcome, Control, ExitReason, OutQueue, Pump, SessionError, pump,
};

struct Counting;

thread_local! {
    static ALLOCS: Cell<u64> = const { Cell::new(0) };
}

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let _ = ALLOCS.try_with(|c| c.set(c.get() + 1));
        unsafe { System.alloc(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

fn allocs() -> u64 {
    ALLOCS.with(|c| c.get())
}

struct Count {
    iterations: u64,
    warm: Option<u64>,
    end: u64,
    echo: bool,
    capture: Option<Capture>,
    ring: xcomm::ring::Ring,
    server: Option<CtlServer>,
}

/// What the control server sees of the session.
struct View<'a> {
    ring: &'a xcomm::ring::Ring,
    queue: &'a mut OutQueue,
}

impl Session for View<'_> {
    fn status(&self) -> Status {
        Status {
            device: String::new(),
            tty: String::new(),
            stable: String::new(),
            settings: None,
            modem: None,
            connected: true,
            offset: self.ring.end(),
            oldest: self.ring.oldest(),
            capture: None,
            transfer: false,
            ctl: String::new(),
            herdr_pane: None,
            pid: 0,
        }
    }
    fn ring(&self) -> &xcomm::ring::Ring {
        self.ring
    }
    fn send(&mut self, bytes: &[u8]) -> Result<usize, (u8, String)> {
        let n = bytes.len().min(self.queue.free());
        assert!(self.queue.push(&bytes[..n]));
        Ok(n)
    }
    fn written(&self) -> u64 {
        self.queue.written()
    }
}

impl Control for Count {
    fn stop(&mut self) -> std::io::Result<()> {
        Ok(())
    }
    fn tick(&mut self) -> ControlFlow<()> {
        self.iterations += 1;
        if self.iterations == 50 {
            self.warm = Some(allocs());
        }
        if self.iterations == 2000 {
            self.end = allocs();
            return ControlFlow::Break(());
        }
        ControlFlow::Continue(())
    }
    fn local_echo(&self) -> bool {
        self.echo
    }
    fn capture(&mut self) -> Option<&mut Capture> {
        self.capture.as_mut()
    }
    fn capture_failed(&mut self, e: std::io::Error) {
        panic!("capture failed: {e}");
    }
    fn device_output(&mut self, bytes: &[u8]) {
        self.ring.push(bytes);
        if let Some(server) = &mut self.server {
            server.device_output(&self.ring);
        }
    }
    fn ctl_fds(&self, out: &mut Vec<libc::pollfd>) -> Option<std::time::Instant> {
        self.server
            .as_ref()
            .and_then(|s| s.poll_fds(self.ring.end(), out))
    }
    fn ctl_serve(
        &mut self,
        ready: &[libc::pollfd],
        queue: &mut OutQueue,
        _stdin: Option<std::os::fd::BorrowedFd<'_>>,
    ) -> Result<CommandOutcome, SessionError> {
        if let Some(server) = &mut self.server {
            let mut view = View {
                ring: &self.ring,
                queue,
            };
            server.serve(ready, &mut view);
        }
        Ok(CommandOutcome::Continue)
    }
}

/// One test: a process has one signal pipe at a time. The second run adds
/// the prefix parser, local echo, held Alt sequences (a CSI arrow key, an
/// ESC cut off at the end of a write) and commands; the third the SGR
/// filter, with sequences cut at the ends of reads, and capture; the
/// fourth the control socket with one subscriber reading the output.
#[test]
fn the_loop_does_not_allocate_in_steady_state() {
    run(None, false, b"typed", None);
    run_with_subscriber();
    let alt_a = PrefixKey {
        modifier: Modifier::Alt,
        letter: b'a',
    };
    run(
        Some((alt_a, std::time::Duration::from_millis(1))),
        true,
        b"typed\x1b[A\x1baz\x1b",
        None,
    );
    run(
        None,
        false,
        b"typed",
        Some(b"\x1b[1;31mred\x1b[0m \x1b[2J\x1b[10;5Hxy\x1b[38;2;1;2;3"),
    );
}

/// `colored`: device output with SGR and other sequences, run through the
/// SGR filter (color off).
fn run(
    prefix: Option<(PrefixKey, std::time::Duration)>,
    echo: bool,
    typed: &'static [u8],
    colored: Option<&'static [u8]>,
) {
    run_full(prefix, echo, typed, colored, None, 4093);
}

fn run_with_subscriber() {
    let dir = support::lock_dir("alloc-ctl");
    let server = CtlServer::bind(&dir, "alloc", CtlMode::ReadWrite).unwrap();
    let mut sub = std::os::unix::net::UnixStream::connect(server.path()).unwrap();
    std::io::Write::write_all(&mut sub, b"{\"cmd\":\"subscribe\"}\n").unwrap();
    let reader = std::thread::spawn(move || {
        let mut sub = std::io::BufReader::new(sub);
        let mut reply = String::new();
        std::io::BufRead::read_line(&mut sub, &mut reply).unwrap();
        let v: serde_json::Value = serde_json::from_str(&reply).unwrap();
        assert_eq!(v["ok"], true, "{reply}");
        let start = v["start"].as_u64().unwrap();
        let mut buf = vec![0u8; 65536];
        let mut total = 0u64;
        let mut tail = Vec::new();
        while let Ok(n) = std::io::Read::read(&mut sub, &mut buf) {
            if n == 0 {
                break;
            }
            total += n as u64;
            tail.extend_from_slice(&buf[..n]);
            let keep = tail.len().saturating_sub(200);
            tail.drain(..keep);
        }
        (start + total, tail)
    });
    // A subscriber is dropped (which allocates its message) only once it
    // is a ring behind, and the socket buffers more than that first.
    run_full(None, false, b"typed", None, Some(server), 65521);
    let (reached, tail) = reader.join().unwrap();
    // Fed after the ring had wrapped: late in the counted passes.
    assert!(reached > 65521, "the subscriber reached offset {reached}");
    assert!(
        !String::from_utf8_lossy(&tail).contains("too slow"),
        "the subscriber was dropped"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

fn run_full(
    prefix: Option<(PrefixKey, std::time::Duration)>,
    echo: bool,
    typed: &'static [u8],
    colored: Option<&'static [u8]>,
    server: Option<CtlServer>,
    ring_size: usize,
) {
    let dev = support::pty();
    let signals = SignalPipe::install(&[libc::SIGUSR1]).unwrap();
    let (out_r, out_w) = std::io::pipe().unwrap();
    let (in_r, in_w) = std::io::pipe().unwrap();
    // Keep both directions busy: device output to drain, input to forward.
    let master = dev.master.try_clone().unwrap();
    // Both run until the loop has stopped.
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    for fd in [master.as_fd(), in_w.as_fd()] {
        let fl = rustix::fs::fcntl_getfl(fd).unwrap();
        rustix::fs::fcntl_setfl(fd, fl | rustix::fs::OFlags::NONBLOCK).unwrap();
    }
    let feeder = keep_writing(master, colored.unwrap_or(&[0x55u8; 4096]), stop.clone());
    let typist = keep_writing(in_w.into(), typed, stop.clone());
    // What the loop sends to the device must be taken, or the queue fills
    // and the loop stops reading input.
    let drainer = {
        let master = dev.master.try_clone().unwrap();
        let stop = stop.clone();
        std::thread::spawn(move || {
            let mut buf = vec![0u8; 65536];
            while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                match rustix::io::read(master.as_fd(), &mut buf[..]) {
                    Ok(_) => {}
                    Err(rustix::io::Errno::AGAIN) => {
                        std::thread::sleep(std::time::Duration::from_millis(1))
                    }
                    Err(_) => break,
                }
            }
        })
    };
    let reader = std::thread::spawn(move || {
        let mut buf = vec![0u8; 65536];
        while let Ok(n) = rustix::io::read(out_r.as_fd(), &mut buf[..]) {
            if n == 0 {
                break;
            }
        }
    });
    let port_fd = rustix::fs::open(
        &dev.slave_path,
        rustix::fs::OFlags::RDWR
            | rustix::fs::OFlags::NOCTTY
            | rustix::fs::OFlags::NONBLOCK,
        rustix::fs::Mode::empty(),
    )
    .unwrap();
    let mut t = rustix::termios::tcgetattr(&port_fd).unwrap();
    t.make_raw();
    rustix::termios::tcsetattr(&port_fd, rustix::termios::OptionalActions::Now, &t)
        .unwrap();
    let mut count = Count {
        iterations: 0,
        warm: None,
        end: 0,
        echo,
        capture: colored
            .map(|_| Capture::start(std::path::Path::new("/dev/null")).unwrap()),
        // Small, so it wraps many times while counted.
        ring: xcomm::ring::Ring::new(ring_size),
        server,
    };
    let reason = {
        let p = Pump {
            stdin: Some(in_r.as_fd()),
            stdout: Some(out_w.as_fd()),
            device: port_fd.as_fd(),
            signals: &signals,
            prefix,
            strip_color: colored.is_some(),
        };
        pump(&p, &mut count).unwrap()
    };
    assert_eq!(reason, ExitReason::Requested);
    let warm = count.warm.unwrap();
    assert_eq!(count.end - warm, 0, "allocations in steady state");
    assert!(count.ring.end() > ring_size as u64, "the ring did not wrap");
    drop(count);
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    drop(out_w);
    drop(in_r);
    drop(port_fd);
    drop(dev);
    let _ = feeder.join();
    let _ = typist.join();
    let _ = drainer.join();
    let _ = reader.join();
}

fn keep_writing(
    fd: std::os::fd::OwnedFd,
    bytes: &'static [u8],
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        while !stop.load(std::sync::atomic::Ordering::Relaxed) {
            match rustix::io::write(fd.as_fd(), bytes) {
                Ok(_) => {}
                Err(rustix::io::Errno::AGAIN) => {
                    std::thread::sleep(std::time::Duration::from_millis(1))
                }
                Err(_) => break,
            }
        }
    })
}
