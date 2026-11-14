//! The session loop must not allocate once running.

mod support;

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::ops::ControlFlow;
use std::os::fd::AsFd;

use xcomm::cli::{Modifier, PrefixKey};
use xcomm::hosttty::SignalPipe;
use xcomm::session::{Control, ExitReason, Pump, pump};

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
}

/// One test: a process has one signal pipe at a time. The second run adds
/// the prefix parser, local echo, held Alt sequences (a CSI arrow key, an
/// ESC cut off at the end of a write) and commands; the third the SGR
/// filter, with sequences cut at the ends of reads.
#[test]
fn the_loop_does_not_allocate_in_steady_state() {
    run(None, false, b"typed", None);
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
        rustix::fs::OFlags::RDWR | rustix::fs::OFlags::NOCTTY | rustix::fs::OFlags::NONBLOCK,
        rustix::fs::Mode::empty(),
    )
    .unwrap();
    let mut t = rustix::termios::tcgetattr(&port_fd).unwrap();
    t.make_raw();
    rustix::termios::tcsetattr(&port_fd, rustix::termios::OptionalActions::Now, &t).unwrap();
    let mut count = Count {
        iterations: 0,
        warm: None,
        end: 0,
        echo,
    };
    let reason = {
        let p = Pump {
            stdin: Some(in_r.as_fd()),
            stdout: out_w.as_fd(),
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
