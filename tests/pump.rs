//! In-process scenarios for the session loop that need precise control of
//! its file descriptors.

mod support;

use std::ops::ControlFlow;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use xcomm::hosttty::SignalPipe;
use xcomm::session::{Control, ExitReason, Pump, pump};

/// The signal pipe is process-wide; scenarios take turns.
static ONE_AT_A_TIME: Mutex<()> = Mutex::new(());

struct Clock {
    started: Instant,
    limit: Duration,
    ticks: u64,
}

impl Control for Clock {
    fn stop(&mut self) -> std::io::Result<()> {
        Ok(())
    }
    fn tick(&mut self) -> ControlFlow<()> {
        self.ticks += 1;
        if self.started.elapsed() > self.limit {
            return ControlFlow::Break(());
        }
        ControlFlow::Continue(())
    }
}

fn nonblocking(fd: BorrowedFd<'_>) {
    let fl = rustix::fs::fcntl_getfl(fd).unwrap();
    rustix::fs::fcntl_setfl(fd, fl | rustix::fs::OFlags::NONBLOCK).unwrap();
}

fn raw_device(p: &support::Pty) -> OwnedFd {
    let fd = rustix::fs::open(
        &p.slave_path,
        rustix::fs::OFlags::RDWR | rustix::fs::OFlags::NOCTTY | rustix::fs::OFlags::NONBLOCK,
        rustix::fs::Mode::empty(),
    )
    .unwrap();
    let mut t = rustix::termios::tcgetattr(&fd).unwrap();
    t.make_raw();
    rustix::termios::tcsetattr(&fd, rustix::termios::OptionalActions::Now, &t).unwrap();
    fd
}

#[test]
fn closed_input_with_a_full_queue_does_not_spin() {
    let _turn = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let signals = SignalPipe::install(&[libc::SIGUSR1]).unwrap();
    let dev = support::pty();
    let device = raw_device(&dev);
    // Nobody reads the device side, so the queue fills; then input closes.
    let (in_r, in_w) = std::io::pipe().unwrap();
    let (_out_r, out_w) = std::io::pipe().unwrap();
    let writer = std::thread::spawn(move || {
        let chunk = [b'k'; 4096];
        let deadline = Instant::now() + Duration::from_millis(300);
        nonblocking(in_w.as_fd());
        while Instant::now() < deadline {
            let _ = rustix::io::write(in_w.as_fd(), &chunk);
            std::thread::sleep(Duration::from_millis(1));
        }
    });
    // An idle loop blocks in poll; a periodic SIGUSR1 wakes it so the clock
    // can end the scenario. A spinning loop ticks far more often than this.
    let done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let waking = done.clone();
    // Aimed at this thread: a process-wide signal could be delivered late,
    // after the handler is gone, and kill the test binary.
    let me = unsafe { libc::pthread_self() } as usize;
    let waker = std::thread::spawn(move || {
        while !waking.load(std::sync::atomic::Ordering::Relaxed) {
            std::thread::sleep(Duration::from_millis(100));
            unsafe { libc::pthread_kill(me as libc::pthread_t, libc::SIGUSR1) };
        }
    });
    let mut clock = Clock {
        started: Instant::now(),
        limit: Duration::from_millis(1500),
        ticks: 0,
    };
    let reason = pump(
        &Pump {
            stdin: Some(in_r.as_fd()),
            stdout: out_w.as_fd(),
            device: device.as_fd(),
            signals: &signals,
            prefix: None,
        },
        &mut clock,
    )
    .unwrap();
    done.store(true, std::sync::atomic::Ordering::Relaxed);
    writer.join().unwrap();
    // The signal pipe outlives the waker, so a late SIGUSR1 is still caught.
    waker.join().unwrap();
    drop(signals);
    assert_eq!(reason, ExitReason::Requested);
    assert!(clock.ticks < 200, "spinning: {} iterations", clock.ticks);
}

#[test]
fn sigterm_ends_the_session_while_a_nonblocking_terminal_is_stalled() {
    let _turn = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let signals = SignalPipe::install(&[libc::SIGTERM]).unwrap();
    let dev = support::pty();
    let device = raw_device(&dev);
    // The terminal is a full, non-blocking pipe nobody reads.
    let (_out_r, out_w) = std::io::pipe().unwrap();
    nonblocking(out_w.as_fd());
    let master = dev.master.try_clone().unwrap();
    nonblocking(master.as_fd());
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let feeding = stop.clone();
    let feeder = std::thread::spawn(move || {
        while !feeding.load(std::sync::atomic::Ordering::Relaxed) {
            if rustix::io::write(master.as_fd(), &[0x41u8; 4096]).is_err() {
                std::thread::sleep(Duration::from_millis(1));
            }
        }
    });
    let me = unsafe { libc::pthread_self() } as usize;
    let killer = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(300));
        unsafe { libc::pthread_kill(me as libc::pthread_t, libc::SIGTERM) };
    });
    let mut clock = Clock {
        started: Instant::now(),
        limit: Duration::from_secs(5),
        ticks: 0,
    };
    let started = Instant::now();
    let reason = pump(
        &Pump {
            stdin: None,
            stdout: out_w.as_fd(),
            device: device.as_fd(),
            signals: &signals,
            prefix: None,
        },
        &mut clock,
    )
    .unwrap();
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    killer.join().unwrap();
    feeder.join().unwrap();
    assert_eq!(reason, ExitReason::Signal(libc::SIGTERM));
    assert!(started.elapsed() < Duration::from_secs(2));
}

#[test]
fn sigterm_ends_the_session_while_a_blocking_terminal_is_stalled() {
    // A blocked write cut short by the signal returns a short count, not
    // EINTR; the signal must still be seen.
    let _turn = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let signals = SignalPipe::install(&[libc::SIGTERM]).unwrap();
    let dev = support::pty();
    let device = raw_device(&dev);
    let (out_r, out_w) = std::io::pipe().unwrap();
    let master = dev.master.try_clone().unwrap();
    nonblocking(master.as_fd());
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let feeding = stop.clone();
    let feeder = std::thread::spawn(move || {
        while !feeding.load(std::sync::atomic::Ordering::Relaxed) {
            if rustix::io::write(master.as_fd(), &[0x42u8; 4096]).is_err() {
                std::thread::sleep(Duration::from_millis(1));
            }
        }
    });
    // The terminal frees a little room once, just before the signal, then
    // stalls for good: the blocked write has copied part of its buffer when
    // the signal arrives (a short count), and writing the rest would block
    // for ever.
    let reader = std::thread::spawn(move || {
        nonblocking(out_r.as_fd());
        std::thread::sleep(Duration::from_millis(250));
        let mut b = [0u8; 512];
        let _ = rustix::io::read(out_r.as_fd(), &mut b);
        out_r
    });
    // Aimed at this thread, as in single-threaded xcomm (a process-wide
    // signal could land on the feeder instead). A second one after 3 s
    // unblocks a broken loop so the test fails on time instead of hanging.
    let me = unsafe { libc::pthread_self() } as usize;
    let killer = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(300));
        unsafe { libc::pthread_kill(me as libc::pthread_t, libc::SIGTERM) };
        std::thread::sleep(Duration::from_secs(3));
        unsafe { libc::pthread_kill(me as libc::pthread_t, libc::SIGTERM) };
    });
    let mut clock = Clock {
        started: Instant::now(),
        limit: Duration::from_secs(10),
        ticks: 0,
    };
    let started = Instant::now();
    let reason = pump(
        &Pump {
            stdin: None,
            stdout: out_w.as_fd(),
            device: device.as_fd(),
            signals: &signals,
            prefix: None,
        },
        &mut clock,
    )
    .unwrap();
    let took = started.elapsed();
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    killer.join().unwrap();
    feeder.join().unwrap();
    drop(reader.join().unwrap());
    assert_eq!(reason, ExitReason::Signal(libc::SIGTERM));
    assert!(
        took < Duration::from_secs(2),
        "took {took:?}: the first SIGTERM was lost"
    );
}

#[test]
fn a_terminal_that_goes_away_ends_the_session_as_gone() {
    // Writing to a pty slave whose master is closed fails with EIO: the
    // terminal is gone, which is an ordinary end, not an error.
    let _turn = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let signals = SignalPipe::install(&[libc::SIGUSR1]).unwrap();
    let dev = support::pty();
    let device = raw_device(&dev);
    let support::Pty { master, slave, .. } = support::pty();
    drop(master);
    support::write_all(dev.master.as_fd(), b"output for a closed terminal");
    let mut clock = Clock {
        started: Instant::now(),
        limit: Duration::from_secs(5),
        ticks: 0,
    };
    let reason = pump(
        &Pump {
            stdin: None,
            stdout: slave.as_fd(),
            device: device.as_fd(),
            signals: &signals,
            prefix: None,
        },
        &mut clock,
    )
    .unwrap();
    assert_eq!(reason, ExitReason::TerminalGone);
}

#[test]
fn a_pipe_reader_that_leaves_ends_the_session_as_gone() {
    // `xcomm ... | head -c 5`: once head exits, writes fail with EPIPE.
    let _turn = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let signals = SignalPipe::install(&[libc::SIGUSR1]).unwrap();
    let dev = support::pty();
    let device = raw_device(&dev);
    let (out_r, out_w) = std::io::pipe().unwrap();
    drop(out_r);
    support::write_all(dev.master.as_fd(), b"output nobody reads");
    let mut clock = Clock {
        started: Instant::now(),
        limit: Duration::from_secs(5),
        ticks: 0,
    };
    let reason = pump(
        &Pump {
            stdin: None,
            stdout: out_w.as_fd(),
            device: device.as_fd(),
            signals: &signals,
            prefix: None,
        },
        &mut clock,
    )
    .unwrap();
    assert_eq!(reason, ExitReason::TerminalGone);
}

#[test]
fn end_of_input_waits_until_everything_reached_the_device() {
    // `cat file | xcomm dev` with a slow device: the session ends only after
    // the last byte was written to it.
    let _turn = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let signals = SignalPipe::install(&[libc::SIGUSR1]).unwrap();
    let dev = support::pty();
    let device = raw_device(&dev);
    let (in_r, in_w) = std::io::pipe().unwrap();
    let (_out_r, out_w) = std::io::pipe().unwrap();
    let data: Vec<u8> = (0..200_000u32).map(|i| (i % 251) as u8).collect();
    let expected = data.clone();
    let writer = std::thread::spawn(move || {
        support::write_all(in_w.as_fd(), &data);
    });
    // The device side starts reading only after a while.
    let master = dev.master.try_clone().unwrap();
    let reader = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(500));
        let mut got = Vec::new();
        let mut buf = [0u8; 65536];
        while got.len() < 200_000 {
            let n = rustix::io::read(master.as_fd(), &mut buf).unwrap();
            got.extend_from_slice(&buf[..n]);
        }
        got
    });
    let mut clock = Clock {
        started: Instant::now(),
        limit: Duration::from_secs(10),
        ticks: 0,
    };
    let reason = pump(
        &Pump {
            stdin: Some(in_r.as_fd()),
            stdout: out_w.as_fd(),
            device: device.as_fd(),
            signals: &signals,
            prefix: None,
        },
        &mut clock,
    )
    .unwrap();
    writer.join().unwrap();
    assert_eq!(reason, ExitReason::StdinClosed);
    assert_eq!(reader.join().unwrap(), expected);
}

/// Counts stops; the first one also catches a second SIGTSTP before it
/// "stops", as a quick second Ctrl-Z can.
struct StopTwice {
    stops: u32,
}

impl Control for StopTwice {
    fn stop(&mut self) -> std::io::Result<()> {
        self.stops += 1;
        if self.stops == 1 {
            unsafe { libc::pthread_kill(libc::pthread_self(), libc::SIGTSTP) };
        }
        Ok(())
    }
}

#[test]
fn a_sigtstp_caught_before_the_stop_does_not_stop_again() {
    let _turn = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let signals = SignalPipe::install(&[libc::SIGTSTP, libc::SIGTERM]).unwrap();
    let dev = support::pty();
    let device = raw_device(&dev);
    let (_out_r, out_w) = std::io::pipe().unwrap();
    unsafe { libc::pthread_kill(libc::pthread_self(), libc::SIGTSTP) };
    // Nothing else wakes the loop: SIGTERM ends it.
    let me = unsafe { libc::pthread_self() } as usize;
    let killer = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(300));
        unsafe { libc::pthread_kill(me as libc::pthread_t, libc::SIGTERM) };
    });
    let mut ctl = StopTwice { stops: 0 };
    let reason = pump(
        &Pump {
            stdin: None,
            stdout: out_w.as_fd(),
            device: device.as_fd(),
            signals: &signals,
            prefix: None,
        },
        &mut ctl,
    )
    .unwrap();
    killer.join().unwrap();
    assert_eq!(reason, ExitReason::Signal(libc::SIGTERM));
    assert_eq!(ctl.stops, 1);
}

/// The first resume catches a SIGTSTP, as one sent right after SIGCONT is.
struct StopAfterContinue {
    stops: u32,
    resumes: u32,
}

impl Control for StopAfterContinue {
    fn stop(&mut self) -> std::io::Result<()> {
        self.stops += 1;
        Ok(())
    }
    fn resume(&mut self) -> std::io::Result<()> {
        self.resumes += 1;
        if self.resumes == 1 {
            unsafe { libc::pthread_kill(libc::pthread_self(), libc::SIGTSTP) };
        }
        Ok(())
    }
}

#[test]
fn a_sigtstp_after_the_continue_stops_again() {
    let _turn = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let signals = SignalPipe::install(&[libc::SIGTSTP, libc::SIGTERM]).unwrap();
    let dev = support::pty();
    let device = raw_device(&dev);
    let (_out_r, out_w) = std::io::pipe().unwrap();
    unsafe { libc::pthread_kill(libc::pthread_self(), libc::SIGTSTP) };
    let me = unsafe { libc::pthread_self() } as usize;
    let killer = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(300));
        unsafe { libc::pthread_kill(me as libc::pthread_t, libc::SIGTERM) };
    });
    let mut ctl = StopAfterContinue {
        stops: 0,
        resumes: 0,
    };
    let reason = pump(
        &Pump {
            stdin: None,
            stdout: out_w.as_fd(),
            device: device.as_fd(),
            signals: &signals,
            prefix: None,
        },
        &mut ctl,
    )
    .unwrap();
    killer.join().unwrap();
    assert_eq!(reason, ExitReason::Signal(libc::SIGTERM));
    assert_eq!((ctl.stops, ctl.resumes), (2, 2));
}
