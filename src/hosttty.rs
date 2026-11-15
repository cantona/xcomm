//! The host terminal: raw mode that is always restored, and a self-pipe that
//! turns signals into poll(2) readiness.

use std::cell::UnsafeCell;
use std::io;
use std::mem::MaybeUninit;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd, RawFd};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU64, Ordering};

use rustix::termios::{InputModes, OptionalActions, OutputModes, Termios, tcgetattr, tcsetattr};

/// The terminal state to restore if the process dies in raw mode.
///
/// `panic = "abort"` runs no destructors, and neither do aborts, fatal
/// signals or `process::exit`, so the saved state is restored from the panic
/// hook, from handlers for the fatal signals (see FATAL) and from an `atexit`
/// hook. All read it lock-free, since a signal handler may run at any point.
/// xcomm is single-threaded; concurrent enter/drop from several threads is
/// not supported.
struct Saved(UnsafeCell<MaybeUninit<(RawFd, Termios)>>);

// SAFETY: written only by the thread that won CLAIMED, before ARMED is set
// with Release; read only after ARMED is seen with Acquire.
unsafe impl Sync for Saved {}

static SAVED: Saved = Saved(UnsafeCell::new(MaybeUninit::uninit()));
/// Set while a RawMode exists (or was leaked); guards the single slot.
static CLAIMED: AtomicBool = AtomicBool::new(false);
/// Set once SAVED holds a value the restore paths may use.
static ARMED: AtomicBool = AtomicBool::new(false);
static RESTORE_HOOKS: OnceLock<Result<(), String>> = OnceLock::new();

/// Whether this process's group is the terminal's foreground group: only
/// then is the terminal still ours to restore. A job moved to the background
/// (including after an uncatchable SIGSTOP and `bg`) must not overwrite the
/// settings of the program now in the foreground. Async-signal-safe.
fn in_foreground(fd: BorrowedFd<'_>) -> bool {
    match rustix::termios::tcgetpgrp(fd) {
        Ok(group) => group == rustix::process::getpgrp(),
        // Not a controlling terminal of ours: nobody else owns it either.
        Err(_) => true,
    }
}

/// Async-signal-safe: an atomic load, tcgetpgrp(3) and tcsetattr(3).
fn restore_saved() {
    if ARMED.load(Ordering::Acquire) {
        // SAFETY: ARMED implies SAVED is initialised (see Saved).
        let (fd, termios) = unsafe { (*SAVED.0.get()).assume_init_ref() };
        // SAFETY: the registered fd is RawMode's own duplicate. It is closed
        // only after disarm(), and leaked (never closed) if the guard is
        // forgotten, so the number always names that terminal.
        let fd = unsafe { BorrowedFd::borrow_raw(*fd) };
        if in_foreground(fd) {
            let _ = tcsetattr(fd, OptionalActions::Now, termios);
        }
    }
}

/// Signals whose default action ends the process without running any Rust
/// code. SIGTERM, SIGHUP and SIGINT are not here: the session receives them
/// through SignalPipe and exits through the normal restore path.
const FATAL: [libc::c_int; 6] = [
    libc::SIGABRT,
    libc::SIGSEGV,
    libc::SIGBUS,
    libc::SIGILL,
    libc::SIGFPE,
    libc::SIGQUIT,
];

/// Synchronous faults: returning from their handler re-runs the instruction.
fn is_fault(sig: libc::c_int) -> bool {
    matches!(
        sig,
        libc::SIGSEGV | libc::SIGBUS | libc::SIGILL | libc::SIGFPE
    )
}

/// The dispositions that were installed before ours, one per FATAL entry.
struct Previous(UnsafeCell<[MaybeUninit<libc::sigaction>; FATAL.len()]>);

// SAFETY: each entry is written once, before our handler for that signal is
// installed, and only read by that handler.
unsafe impl Sync for Previous {}

static PREVIOUS: Previous = Previous(UnsafeCell::new(
    [const { MaybeUninit::uninit() }; FATAL.len()],
));

/// Restores the terminal, then hands the signal to whatever was installed
/// before: a handler is called (Rust's own SIGSEGV handler among them), the
/// default action is re-raised. An ignored signal that is not a fault stays
/// ignored and leaves the terminal alone (the session goes on in raw mode),
/// except SIGABRT from abort(3).
extern "C" fn restore_on_fatal(
    sig: libc::c_int,
    info: *mut libc::siginfo_t,
    context: *mut libc::c_void,
) {
    let Some(i) = FATAL.iter().position(|&s| s == sig) else {
        return;
    };
    // SAFETY: written before this handler was installed (see Previous).
    let previous = unsafe { (*PREVIOUS.0.get())[i].assume_init_ref() };
    let handler = previous.sa_sigaction;
    // abort(3) forces the default action on an ignored SIGABRT once this
    // returns, so one the process sent itself is not really ignored.
    let forced = sig == libc::SIGABRT && sent_by_self(info);
    if handler == libc::SIG_IGN && !is_fault(sig) && !forced {
        return;
    }
    restore_saved();
    // SIG_DFL and SIG_IGN are checked before SA_SIGINFO: macOS keeps that
    // flag across exec while resetting the handler to SIG_DFL, so a parent's
    // SA_SIGINFO handler leaves SIG_DFL with SA_SIGINFO set.
    if handler == libc::SIG_DFL || (handler == libc::SIG_IGN && is_fault(sig)) {
        // An ignored fault cannot be ignored: returning would re-execute the
        // faulting instruction forever. The kernel forces the default action
        // in that case, and so does this.
        // The signal is blocked while this handler runs, so the re-raised
        // one is delivered, with the default action, once it returns.
        // SAFETY: signal(2) and raise(3) are async-signal-safe.
        unsafe {
            libc::signal(sig, libc::SIG_DFL);
            libc::raise(sig);
        }
    } else if handler == libc::SIG_IGN {
        // SIGABRT the process sent itself: abort(3) re-raises it with the
        // default action, raise(3) just returns.
    } else if previous.sa_flags & libc::SA_SIGINFO != 0 {
        // SAFETY: an SA_SIGINFO handler obtained from sigaction.
        let h: extern "C" fn(libc::c_int, *mut libc::siginfo_t, *mut libc::c_void) =
            unsafe { std::mem::transmute(handler) };
        h(sig, info, context);
    } else {
        // SAFETY: a plain handler obtained from sigaction.
        let h: extern "C" fn(libc::c_int) = unsafe { std::mem::transmute(handler) };
        h(sig);
    }
}

/// Whether the signal came from this process (abort(3), raise(3)).
/// Async-signal-safe.
fn sent_by_self(info: *const libc::siginfo_t) -> bool {
    // SAFETY: the kernel hands an SA_SIGINFO handler a valid siginfo.
    !info.is_null()
        && unsafe { (*info).si_pid() } == rustix::process::getpid().as_raw_nonzero().get()
}

extern "C" fn restore_at_exit() {
    restore_saved();
}

fn install_restore_hooks() -> io::Result<()> {
    RESTORE_HOOKS
        .get_or_init(|| {
            let previous = std::panic::take_hook();
            std::panic::set_hook(Box::new(move |info| {
                restore_saved();
                previous(info);
            }));
            for (i, &sig) in FATAL.iter().enumerate() {
                // SAFETY: zeroed sigaction values are valid; PREVIOUS[i] is
                // written before the handler that reads it is installed.
                unsafe {
                    let mut old: libc::sigaction = std::mem::zeroed();
                    if libc::sigaction(sig, std::ptr::null(), &mut old) != 0 {
                        return Err(format!(
                            "sigaction query for signal {sig}: {}",
                            io::Error::last_os_error()
                        ));
                    }
                    (*PREVIOUS.0.get())[i].write(old);
                    let mut action: libc::sigaction = std::mem::zeroed();
                    action.sa_sigaction = restore_on_fatal as *const () as libc::sighandler_t;
                    // SA_ONSTACK: a stack overflow must run on the alternate stack.
                    action.sa_flags = libc::SA_SIGINFO | libc::SA_ONSTACK;
                    if libc::sigaction(sig, &action, std::ptr::null_mut()) != 0 {
                        return Err(format!(
                            "sigaction for signal {sig}: {}",
                            io::Error::last_os_error()
                        ));
                    }
                }
            }
            // SAFETY: registers a plain extern "C" function.
            if unsafe { libc::atexit(restore_at_exit) } != 0 {
                return Err("atexit registration failed".into());
            }
            Ok(())
        })
        .clone()
        .map_err(io::Error::other)
}

fn context(e: impl Into<io::Error>, what: impl std::fmt::Display) -> io::Error {
    let e = e.into();
    io::Error::new(e.kind(), format!("{what}: {e}"))
}

/// Raw mode on a terminal for as long as the value lives. Only one may exist
/// at a time.
///
/// Install the SignalPipe for SIGTERM, SIGHUP and SIGINT before entering raw
/// mode and drop it after the RawMode, so those signals always reach the
/// session (and its restore path) while the terminal is raw. SIGTSTP is the
/// session's job too: catch it through the pipe, restore, stop, and enter
/// raw mode again on resume. Never catch SIGTTIN or SIGTTOU (see
/// SignalPipe): their default stop is what a background session needs.
pub struct RawMode {
    /// A duplicate of the caller's fd, owned here so the number registered
    /// for the restore paths cannot be closed and reused for another file
    /// while it is registered. If the guard is leaked, so is the duplicate.
    fd: OwnedFd,
    saved: Termios,
    raw: Termios,
    /// False while suspended: the terminal is not ours to restore then.
    active: std::cell::Cell<bool>,
}

impl RawMode {
    pub fn enter(fd: BorrowedFd<'_>) -> io::Result<RawMode> {
        let raw_fd = fd.as_raw_fd();
        let saved =
            tcgetattr(fd).map_err(|e| context(e, format_args!("tcgetattr on fd {raw_fd}")))?;
        let mut raw = saved.clone();
        raw.make_raw();
        // make_raw leaves these: IXOFF would send ^S/^Q to the terminal when
        // xcomm stops reading input, IUCLC would lowercase typed letters.
        raw.input_modes
            .remove(InputModes::IXOFF | InputModes::IXANY);
        #[cfg(target_os = "linux")]
        raw.input_modes.remove(InputModes::IUCLC);
        install_restore_hooks()?;
        if CLAIMED
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "raw mode is already active",
            ));
        }
        let fd = match fd.try_clone_to_owned() {
            Ok(fd) => fd,
            Err(e) => {
                CLAIMED.store(false, Ordering::Release);
                return Err(context(e, format_args!("dup of fd {raw_fd}")));
            }
        };
        // SAFETY: CLAIMED is ours and ARMED is false, so no reader exists.
        unsafe { (*SAVED.0.get()).write((fd.as_raw_fd(), saved.clone())) };
        ARMED.store(true, Ordering::Release);
        // A background process is stopped by SIGTTOU here until brought to
        // the foreground. A caught signal meanwhile (one without SA_RESTART,
        // such as SIGTERM) fails the call with EINTR: the error has kind
        // Interrupted, so the caller can look at its signals and try again.
        if let Err(e) = tcsetattr(&fd, OptionalActions::Now, &raw) {
            disarm();
            return Err(context(e, format_args!("tcsetattr on fd {raw_fd}")));
        }
        Ok(RawMode {
            fd,
            saved,
            raw,
            active: std::cell::Cell::new(true),
        })
    }

    /// The fd number registered for the restore paths.
    pub fn registered_fd(&self) -> RawFd {
        self.fd.as_raw_fd()
    }

    /// Puts the terminal back into raw mode, for example after a job-control
    /// stop during which it was restored.
    pub fn reenter(&self) -> io::Result<()> {
        tcsetattr(&self.fd, OptionalActions::Now, &self.raw).map_err(|e| {
            context(
                e,
                format_args!("tcsetattr (raw) on fd {}", self.fd.as_raw_fd()),
            )
        })?;
        self.active.set(true);
        ARMED.store(true, Ordering::Release);
        Ok(())
    }

    /// Turns output processing (OPOST, ONLCR) on or off in raw mode, for a
    /// program that shares the terminal and ends its lines with LF only.
    /// Nothing while suspended.
    pub fn process_output(&self, on: bool) -> io::Result<()> {
        if !self.active.get() {
            return Ok(());
        }
        let mut t = self.raw.clone();
        if on {
            t.output_modes
                .insert(OutputModes::OPOST | OutputModes::ONLCR);
        }
        tcsetattr(&self.fd, OptionalActions::Now, &t).map_err(|e| {
            context(
                e,
                format_args!("tcsetattr (output) on fd {}", self.fd.as_raw_fd()),
            )
        })
    }

    /// Gives the terminal back (a job-control stop). Until `reenter`, the
    /// terminal belongs to whatever runs in the foreground, so neither the
    /// drop nor the crash paths restore it over that program's settings.
    pub fn suspend(&self) -> io::Result<()> {
        self.restore()?;
        self.active.set(false);
        ARMED.store(false, Ordering::Release);
        Ok(())
    }

    /// Restores the saved settings. SIGTTOU is blocked for the call: from
    /// the background, tcsetattr would otherwise stop the process (keeping
    /// the port and its lock) just to give back a cooked terminal.
    pub fn restore(&self) -> io::Result<()> {
        // SAFETY: plain sigset manipulation on this thread's mask.
        let old = unsafe {
            let mut block: libc::sigset_t = std::mem::zeroed();
            let mut old: libc::sigset_t = std::mem::zeroed();
            libc::sigemptyset(&mut block);
            libc::sigaddset(&mut block, libc::SIGTTOU);
            libc::pthread_sigmask(libc::SIG_BLOCK, &block, &mut old);
            old
        };
        let result = tcsetattr(&self.fd, OptionalActions::Now, &self.saved);
        // SAFETY: restores the mask saved above.
        unsafe { libc::pthread_sigmask(libc::SIG_SETMASK, &old, std::ptr::null_mut()) };
        result.map_err(|e| {
            context(
                e,
                format_args!("tcsetattr (restore) on fd {}", self.fd.as_raw_fd()),
            )
        })
    }
}

fn disarm() {
    ARMED.store(false, Ordering::Release);
    CLAIMED.store(false, Ordering::Release);
}

impl Drop for RawMode {
    fn drop(&mut self) {
        if self.active.get() && in_foreground(self.fd.as_fd()) {
            let _ = self.restore();
        }
        disarm();
    }
}

/// Whether `sig` is currently set to be ignored (inherited from the parent,
/// for example SIGHUP under nohup).
pub fn ignored(sig: libc::c_int) -> bool {
    // SAFETY: queries the disposition only.
    unsafe {
        let mut old: libc::sigaction = std::mem::zeroed();
        libc::sigaction(sig, std::ptr::null(), &mut old) == 0 && old.sa_sigaction == libc::SIG_IGN
    }
}

#[cfg(target_os = "linux")]
fn nonblocking_pipe() -> rustix::io::Result<(OwnedFd, OwnedFd)> {
    use rustix::pipe::{PipeFlags, pipe_with};
    pipe_with(PipeFlags::CLOEXEC | PipeFlags::NONBLOCK)
}

/// macOS has no pipe2(2), so the flags are set after creation; a fork in
/// between could inherit the fds, but xcomm installs this before spawning.
#[cfg(not(target_os = "linux"))]
fn nonblocking_pipe() -> rustix::io::Result<(OwnedFd, OwnedFd)> {
    use rustix::fs::{OFlags, fcntl_getfl, fcntl_setfl};
    use rustix::io::{FdFlags, fcntl_setfd};
    let (read, write) = rustix::pipe::pipe()?;
    for fd in [&read, &write] {
        fcntl_setfd(fd, FdFlags::CLOEXEC)?;
        fcntl_setfl(fd, fcntl_getfl(fd)? | OFlags::NONBLOCK)?;
    }
    Ok((read, write))
}

static SIGNAL_WRITE_FD: AtomicI32 = AtomicI32::new(-1);
/// Bit n set when signal n arrived. The pipe byte only wakes poll(2); the
/// mask is the record, so a full pipe cannot lose a signal.
static PENDING: AtomicU64 = AtomicU64::new(0);

#[cfg(target_os = "linux")]
fn errno_ptr() -> *mut libc::c_int {
    // SAFETY: always valid for the calling thread.
    unsafe { libc::__errno_location() }
}

#[cfg(target_os = "macos")]
fn errno_ptr() -> *mut libc::c_int {
    // SAFETY: always valid for the calling thread.
    unsafe { libc::__error() }
}

extern "C" fn on_signal(sig: libc::c_int) {
    let fd = SIGNAL_WRITE_FD.load(Ordering::Relaxed);
    if fd < 0 {
        return;
    }
    PENDING.fetch_or(1u64 << (sig & 63), Ordering::SeqCst);
    let byte = sig as u8;
    let errno = errno_ptr();
    // SAFETY: write(2) and errno access are async-signal-safe. A full pipe
    // drops the byte, which is fine: the pipe is then readable anyway.
    unsafe {
        let saved = *errno;
        libc::write(fd, (&raw const byte).cast(), 1);
        *errno = saved;
    }
}

/// Delivers the given signals as readiness on a non-blocking pipe.
/// Process-wide: only one may exist at a time. Dropping it restores the
/// previous handlers.
///
/// SIGTERM, SIGINT, SIGHUP and SIGTSTP are installed without SA_RESTART, so
/// a read or write blocked on a stalled terminal returns EINTR and the
/// caller can get back to `drain()` and exit or stop. Other signals restart
/// interrupted calls. Do not catch SIGTTIN or SIGTTOU here: a background
/// process's tcsetattr would get SIGTTOU, the handler would run, and the
/// restarted call would get it again, for ever; their default action (stop
/// until brought to the foreground) is the right one.
pub struct SignalPipe {
    read: OwnedFd,
    /// Never closed: a handler running on another thread may have loaded the
    /// fd number just before drop, and must not write into a reused number.
    _write: std::mem::ManuallyDrop<OwnedFd>,
    previous: Vec<(libc::c_int, libc::sigaction)>,
}

impl SignalPipe {
    pub fn install(signals: &[libc::c_int]) -> io::Result<SignalPipe> {
        for (i, &sig) in signals.iter().enumerate() {
            if signals[..i].contains(&sig) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("signal {sig} listed twice"),
                ));
            }
            if FATAL.contains(&sig) || sig == libc::SIGTRAP {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!(
                        "signal {sig} cannot go through the signal pipe: it is reserved for restoring the terminal"
                    ),
                ));
            }
            if !(1..64).contains(&sig) || sig == libc::SIGKILL || sig == libc::SIGSTOP {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("signal {sig} cannot be caught"),
                ));
            }
        }
        let (read, write) = nonblocking_pipe().map_err(|e| context(e, "pipe for signals"))?;
        if SIGNAL_WRITE_FD
            .compare_exchange(-1, write.as_raw_fd(), Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "a signal pipe is already installed",
            ));
        }
        PENDING.store(0, Ordering::SeqCst);
        let mut pipe = SignalPipe {
            read,
            _write: std::mem::ManuallyDrop::new(write),
            previous: Vec::with_capacity(signals.len()),
        };
        for &sig in signals {
            // SAFETY: a zeroed sigaction is a valid starting value.
            let mut action: libc::sigaction = unsafe { std::mem::zeroed() };
            action.sa_sigaction = on_signal as *const () as libc::sighandler_t;
            action.sa_flags = if matches!(
                sig,
                libc::SIGTERM | libc::SIGINT | libc::SIGHUP | libc::SIGTSTP
            ) {
                0
            } else {
                libc::SA_RESTART
            };
            // SAFETY: valid pointers to initialised sigaction values.
            let mut old: libc::sigaction = unsafe { std::mem::zeroed() };
            if unsafe { libc::sigaction(sig, &action, &mut old) } != 0 {
                // Drop restores what was installed so far.
                return Err(context(
                    io::Error::last_os_error(),
                    format_args!("sigaction for signal {sig}"),
                ));
            }
            pipe.previous.push((sig, old));
        }
        Ok(pipe)
    }

    pub fn fd(&self) -> BorrowedFd<'_> {
        self.read.as_fd()
    }

    /// Returns a bitmask with bit `n` set for each signal `n` received since
    /// the last call. Does not allocate.
    pub fn drain(&self) -> io::Result<u64> {
        let mut buf = [0u8; 64];
        loop {
            match rustix::io::read(&self.read, &mut buf) {
                // A short read emptied the pipe; no second read needed.
                Ok(n) if n < buf.len() => break,
                Ok(_) => continue,
                Err(rustix::io::Errno::AGAIN) => break,
                Err(rustix::io::Errno::INTR) => continue,
                Err(e) => return Err(context(e, "read from signal pipe")),
            }
        }
        // After emptying the pipe: a signal arriving now sets its bit (taken
        // here or next time) and writes a byte that wakes the next poll.
        Ok(PENDING.swap(0, Ordering::SeqCst))
    }
}

impl Drop for SignalPipe {
    fn drop(&mut self) {
        for (sig, old) in self.previous.drain(..).rev() {
            // SAFETY: restores a sigaction obtained from the kernel.
            unsafe { libc::sigaction(sig, &old, std::ptr::null_mut()) };
        }
        SIGNAL_WRITE_FD.store(-1, Ordering::SeqCst);
    }
}
