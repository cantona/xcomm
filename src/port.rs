//! The serial port: open, line settings, modem lines and break.

use std::fmt;
use std::io;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::time::Duration;

use rustix::fs::{Mode, OFlags, fcntl_getfl, fcntl_setfl};
use std::os::unix::ffi::OsStrExt;

use rustix::termios::{
    ControlModes, InputModes, LocalModes, OptionalActions, OutputModes, SpecialCodeIndex, Termios,
    tcgetattr, tcsetattr,
};

use crate::cli::{DataBits, Flow, LineSettings, Parity, StopBits};

#[derive(Debug)]
pub enum PortError {
    Open {
        path: PathBuf,
        source: io::Error,
    },
    Config {
        path: PathBuf,
        what: &'static str,
        source: io::Error,
    },
    /// Configuring failed, and putting the previous settings back failed too.
    RevertFailed {
        path: PathBuf,
        cause: Box<PortError>,
        source: io::Error,
    },
}

impl fmt::Display for PortError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PortError::Open { path, source } => {
                write!(f, "cannot open {}: {source}", path.display())
            }
            PortError::Config { path, what, source } => {
                write!(f, "{}: {what}: {source}", path.display())
            }
            PortError::RevertFailed { cause, source, .. } => write!(
                f,
                "{cause}; restoring the previous settings also failed: {source}"
            ),
        }
    }
}

impl std::error::Error for PortError {}

/// For example `115200 8N1` or `9600 7E2 rts/cts`.
pub fn describe(s: &LineSettings) -> String {
    let bits = match s.data_bits {
        DataBits::Five => 5,
        DataBits::Six => 6,
        DataBits::Seven => 7,
        DataBits::Eight => 8,
    };
    let parity = match s.parity {
        Parity::None => 'N',
        Parity::Even => 'E',
        Parity::Odd => 'O',
    };
    let stop = match s.stop_bits {
        StopBits::One => 1,
        StopBits::Two => 2,
    };
    let flow = match s.flow {
        Flow::None => "",
        Flow::RtsCts => " rts/cts",
        Flow::XonXoff => " xon/xoff",
    };
    format!("{} {bits}{parity}{stop}{flow}", s.baud)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModemLines {
    pub dtr: bool,
    pub rts: bool,
    pub cts: bool,
    pub dsr: bool,
    pub dcd: bool,
    pub ri: bool,
}

/// What the driver did with requested line settings.
#[must_use]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Applied {
    /// Applied as asked (the baud rate may be rounded within tolerance).
    Exact,
    /// The driver applied these instead. Some drivers cannot do a format
    /// (the Linux pty driver forces CS8 without parity); some ignore line
    /// settings entirely (cellular modem AT ports on option/qcserial). The
    /// caller must show this to the user.
    Different(LineSettings),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LowLatency {
    Set,
    Unsupported,
}

pub struct Port {
    fd: OwnedFd,
    path: PathBuf,
    tty_name: String,
    settings: LineSettings,
    init: bool,
    /// Set once configure has applied settings to the tty.
    configured: bool,
}

/// ASYNC_LOW_LATENCY is a best-effort hint: a driver without it, or one
/// that refuses it to this user, leaves a usable port.
#[cfg(target_os = "linux")]
fn low_latency_refused(e: &io::Error) -> bool {
    unsupported(e)
        || matches!(
            e.raw_os_error(),
            Some(libc::EPERM) | Some(libc::EACCES) | Some(libc::EOPNOTSUPP)
        )
}

/// ENOTTY and EINVAL mean "this device has no such control", as on a pty.
fn unsupported(e: &io::Error) -> bool {
    matches!(e.raw_os_error(), Some(libc::ENOTTY) | Some(libc::EINVAL))
}

fn ioctl_int(fd: BorrowedFd<'_>, request: libc::c_ulong, arg: &mut libc::c_int) -> io::Result<()> {
    // SAFETY: every request passed here takes a pointer to an int.
    if unsafe { libc::ioctl(fd.as_raw_fd(), request as _, arg as *mut libc::c_int) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn ioctl_none(fd: BorrowedFd<'_>, request: libc::c_ulong) -> io::Result<()> {
    // SAFETY: every request passed here takes no argument.
    if unsafe { libc::ioctl(fd.as_raw_fd(), request as _) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Rates within this fraction of the request are accepted: drivers round to
/// what their divisors can do (a PL2303 runs 74880 as 74882), and a UART
/// tolerates a few percent. Drivers that report the requested rate rather
/// than the one their divisor gives (8250, ftdi_sio) cannot be checked.
const BAUD_TOLERANCE: f64 = 0.01;

pub fn baud_close_enough(requested: u32, actual: u32) -> bool {
    let r = f64::from(requested);
    (f64::from(actual) - r).abs() <= r * BAUD_TOLERANCE
}

fn settings_from(t: &Termios) -> LineSettings {
    let c = t.control_modes;
    let size = c & ControlModes::CSIZE;
    let data_bits = if size == ControlModes::CS5 {
        DataBits::Five
    } else if size == ControlModes::CS6 {
        DataBits::Six
    } else if size == ControlModes::CS7 {
        DataBits::Seven
    } else {
        DataBits::Eight
    };
    let parity = if !c.contains(ControlModes::PARENB) {
        Parity::None
    } else if c.contains(ControlModes::PARODD) {
        Parity::Odd
    } else {
        Parity::Even
    };
    let stop_bits = if c.contains(ControlModes::CSTOPB) {
        StopBits::Two
    } else {
        StopBits::One
    };
    let flow = if c.contains(ControlModes::CRTSCTS) {
        Flow::RtsCts
    } else if t.input_modes.contains(InputModes::IXON | InputModes::IXOFF) {
        Flow::XonXoff
    } else {
        Flow::None
    };
    LineSettings {
        baud: t.output_speed(),
        data_bits,
        parity,
        stop_bits,
        flow,
    }
}

/// The rate to put in termios. macOS drivers reject non-standard rates in
/// tcsetattr and take them through IOSSIOSPEED afterwards.
fn termios_rate(baud: u32) -> u32 {
    #[cfg(target_os = "macos")]
    if !is_standard_rate(baud) {
        return 9600;
    }
    baud
}

/// Rates every macOS driver takes through tcsetattr.
#[cfg(target_os = "macos")]
fn is_standard_rate(baud: u32) -> bool {
    matches!(
        baud,
        50 | 75
            | 110
            | 134
            | 150
            | 200
            | 300
            | 600
            | 1200
            | 1800
            | 2400
            | 4800
            | 9600
            | 19200
            | 38400
            | 57600
            | 115200
            | 230400
    )
}

impl Port {
    /// Opens and configures the port.
    ///
    /// With `init`, DTR and RTS are raised and HUPCL stays set, so closing
    /// drops DTR (as minicom does). Without it (`-o`), xcomm does not raise
    /// them and clears HUPCL, so exiting does not drop DTR. Linux itself
    /// raises DTR and RTS when a tty is opened, and drivers raise them when
    /// the rate moves away from B0; `-o` cannot prevent either.
    ///
    /// TIOCEXCL is set after the open and the TIOCGEXCL check, so two
    /// sessions opening at the same moment can both get in (and root can
    /// open an exclusive tty at any time). The UUCP lock taken before
    /// opening is what serialises sessions; `--no-lock` gives that up.
    pub fn open(
        path: &Path,
        settings: &LineSettings,
        init: bool,
    ) -> Result<(Port, Applied), PortError> {
        let open_err = |source: io::Error| PortError::Open {
            path: path.to_path_buf(),
            source,
        };
        let fd = rustix::fs::open(
            path,
            OFlags::RDWR | OFlags::NOCTTY | OFlags::NONBLOCK | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(|e| open_err(e.into()))?;
        if !rustix::termios::isatty(&fd) {
            return Err(open_err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "not a terminal device",
            )));
        }
        // Root can open a tty another program holds with TIOCEXCL; do not
        // share it, and do not clear that program's exclusivity on drop.
        #[cfg(target_os = "linux")]
        {
            let mut excl: libc::c_int = 0;
            ioctl_int(fd.as_fd(), libc::TIOCGEXCL as libc::c_ulong, &mut excl).map_err(open_err)?;
            if excl != 0 {
                return Err(open_err(io::Error::new(
                    io::ErrorKind::ResourceBusy,
                    "in exclusive use by another program",
                )));
            }
        }
        // picocom (built with USE_FLOCK, as on Ubuntu) takes no UUCP lock
        // and no TIOCEXCL; it only flocks the device. Honour that, and hold
        // the flock ourselves for as long as the fd is open.
        match rustix::fs::flock(&fd, rustix::fs::FlockOperation::NonBlockingLockExclusive) {
            Ok(()) => {}
            Err(rustix::io::Errno::WOULDBLOCK) => {
                return Err(open_err(io::Error::new(
                    io::ErrorKind::ResourceBusy,
                    "in use by another program (it holds a flock on the device)",
                )));
            }
            Err(e) => {
                let e = io::Error::from(e);
                return Err(open_err(io::Error::new(e.kind(), format!("flock: {e}"))));
            }
        }
        // Named from the open fd, not the path: a by-id link can be
        // retargeted between open and a path lookup. Without /proc (a
        // minimal container) ttyname fails; the resolved path is used then.
        let name = match rustix::termios::ttyname(&fd, Vec::new()) {
            Ok(name) => PathBuf::from(std::ffi::OsStr::from_bytes(name.as_bytes())),
            Err(_) => {
                let real = std::fs::canonicalize(path).map_err(open_err)?;
                // The path may have been retargeted since the open: only
                // trust it if it is still the same device.
                let opened = rustix::fs::fstat(&fd).map_err(|e| open_err(e.into()))?;
                let named = rustix::fs::stat(&real).map_err(|e| open_err(e.into()))?;
                if opened.st_rdev != named.st_rdev {
                    return Err(open_err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        format!("{} changed while being opened", path.display()),
                    )));
                }
                real
            }
        };
        let tty_name = name
            .file_name()
            .and_then(|n| n.to_str())
            .ok_or_else(|| {
                open_err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("cannot name the device {}", name.display()),
                ))
            })?
            .to_string();
        // Seeded with what the tty has now, so a revert during the first
        // configure restores that and not the request.
        let current = tcgetattr(&fd).map_err(|e| open_err(e.into()))?;
        let mut port = Port {
            fd,
            path: path.to_path_buf(),
            tty_name,
            settings: settings_from(&current),
            init,
            configured: false,
        };
        // Only now: from here on, dropping `port` clears TIOCEXCL again.
        ioctl_none(port.fd(), libc::TIOCEXCL as libc::c_ulong).map_err(|source| {
            PortError::Config {
                path: path.to_path_buf(),
                what: "TIOCEXCL",
                source,
            }
        })?;
        let applied = port.configure(settings)?;
        if init {
            for (line, what) in [
                (libc::TIOCM_DTR, "raise DTR"),
                (libc::TIOCM_RTS, "raise RTS"),
            ] {
                match port.set_line(line, true) {
                    Ok(()) => {}
                    Err(e) if unsupported(&e) => break,
                    Err(source) => {
                        return Err(PortError::Config {
                            path: port.path.clone(),
                            what,
                            source,
                        });
                    }
                }
            }
        }
        Ok((port, applied))
    }

    /// Applies `s`. Every mode word is built from scratch, so nothing an
    /// earlier user of the tty set (mark/space parity, INPCK, ISTRIP, a
    /// disabled VSTOP) carries over. The result is read back and
    /// `settings()` holds what the driver applied; `Applied::Different`
    /// reports when that is not `s`. An error after the termios was changed
    /// puts the previous one back.
    pub fn configure(&mut self, s: &LineSettings) -> Result<Applied, PortError> {
        let config_err = |what: &'static str, source: io::Error| PortError::Config {
            path: self.path.clone(),
            what,
            source,
        };
        if s.baud == 0 {
            return Err(config_err(
                "baud rate 0",
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "B0 means hang up, not a line rate",
                ),
            ));
        }
        let prior = tcgetattr(&self.fd).map_err(|e| config_err("tcgetattr", e.into()))?;
        let mut t = prior.clone();
        t.input_modes = match s.flow {
            Flow::XonXoff => InputModes::IXON | InputModes::IXOFF,
            _ => InputModes::empty(),
        };
        t.output_modes = OutputModes::empty();
        t.local_modes = LocalModes::empty();
        t.control_modes = ControlModes::CREAD | ControlModes::CLOCAL;
        t.control_modes.insert(match s.data_bits {
            DataBits::Five => ControlModes::CS5,
            DataBits::Six => ControlModes::CS6,
            DataBits::Seven => ControlModes::CS7,
            DataBits::Eight => ControlModes::CS8,
        });
        match s.parity {
            Parity::None => {}
            Parity::Even => t.control_modes.insert(ControlModes::PARENB),
            Parity::Odd => t
                .control_modes
                .insert(ControlModes::PARENB | ControlModes::PARODD),
        }
        if s.stop_bits == StopBits::Two {
            t.control_modes.insert(ControlModes::CSTOPB);
        }
        if s.flow == Flow::RtsCts {
            t.control_modes.insert(ControlModes::CRTSCTS);
        }
        if self.init {
            t.control_modes.insert(ControlModes::HUPCL);
        }
        t.special_codes[SpecialCodeIndex::VMIN] = 1;
        t.special_codes[SpecialCodeIndex::VTIME] = 0;
        t.special_codes[SpecialCodeIndex::VSTART] = 0x11;
        t.special_codes[SpecialCodeIndex::VSTOP] = 0x13;
        t.set_speed(termios_rate(s.baud))
            .map_err(|e| config_err("set baud rate", e.into()))?;
        tcsetattr(&self.fd, OptionalActions::Now, &t)
            .map_err(|e| config_err("tcsetattr", e.into()))?;
        let applied = match self.read_applied(s) {
            Ok(applied) => applied,
            Err(cause) => return Err(self.revert(&prior, cause)),
        };
        self.settings = applied;
        self.configured = true;
        let same_format = LineSettings {
            baud: s.baud,
            ..applied
        } == *s;
        Ok(if same_format && baud_close_enough(s.baud, applied.baud) {
            Applied::Exact
        } else {
            Applied::Different(applied)
        })
    }

    /// Finishes applying `s` (IOSSIOSPEED on macOS) and reads back what the
    /// driver actually applied.
    #[cfg_attr(not(target_os = "macos"), allow(unused_variables))]
    fn read_applied(&self, s: &LineSettings) -> Result<LineSettings, PortError> {
        let config_err = |what: &'static str, source: io::Error| PortError::Config {
            path: self.path.clone(),
            what,
            source,
        };
        #[cfg(target_os = "macos")]
        if termios_rate(s.baud) != s.baud {
            self.set_macos_speed(s.baud)
                .map_err(|e| config_err("IOSSIOSPEED", e))?;
        }
        let t = tcgetattr(&self.fd).map_err(|e| config_err("tcgetattr", e.into()))?;
        let applied = settings_from(&t);
        // macOS reports the termios rate, not the IOSSIOSPEED one. Not
        // testable on the Linux development machine.
        #[cfg(target_os = "macos")]
        let applied = LineSettings {
            baud: s.baud,
            ..applied
        };
        Ok(applied)
    }

    /// Puts back `prior` (and the current IOSSIOSPEED rate on macOS) so the
    /// port matches settings() again; reports both failures if that fails.
    fn revert(&self, prior: &Termios, cause: PortError) -> PortError {
        #[cfg_attr(not(target_os = "macos"), allow(unused_mut))]
        let mut result = tcsetattr(&self.fd, OptionalActions::Now, prior).map_err(io::Error::from);
        #[cfg(target_os = "macos")]
        if result.is_ok() && termios_rate(self.settings.baud) != self.settings.baud {
            result = self.set_macos_speed(self.settings.baud);
        }
        match result {
            Ok(()) => cause,
            Err(source) => PortError::RevertFailed {
                path: self.path.clone(),
                cause: Box::new(cause),
                source,
            },
        }
    }

    /// Drivers on macOS take non-standard rates only through IOSSIOSPEED.
    /// Not testable on the Linux development machine.
    #[cfg(target_os = "macos")]
    fn set_macos_speed(&self, baud: u32) -> io::Result<()> {
        // _IOW('T', 2, speed_t) with an 8-byte speed_t.
        const IOSSIOSPEED: libc::c_ulong = 0x8008_5402;
        let speed: libc::speed_t = baud.into();
        // SAFETY: IOSSIOSPEED takes a pointer to a speed_t.
        if unsafe { libc::ioctl(self.fd.as_raw_fd(), IOSSIOSPEED, &speed) } < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    pub fn read_settings(&self) -> Result<LineSettings, PortError> {
        let t = tcgetattr(&self.fd).map_err(|e| PortError::Config {
            path: self.path.clone(),
            what: "tcgetattr",
            source: e.into(),
        })?;
        Ok(settings_from(&t))
    }

    pub fn settings(&self) -> &LineSettings {
        &self.settings
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The device name after resolving symlinks, for example `ttyUSB1`.
    pub fn tty_name(&self) -> &str {
        &self.tty_name
    }

    pub fn fd(&self) -> BorrowedFd<'_> {
        self.fd.as_fd()
    }

    pub fn modem_lines(&self) -> io::Result<ModemLines> {
        let mut bits: libc::c_int = 0;
        ioctl_int(self.fd(), libc::TIOCMGET as libc::c_ulong, &mut bits)?;
        let has = |b: libc::c_int| bits & b != 0;
        Ok(ModemLines {
            dtr: has(libc::TIOCM_DTR),
            rts: has(libc::TIOCM_RTS),
            cts: has(libc::TIOCM_CTS),
            dsr: has(libc::TIOCM_DSR),
            dcd: has(libc::TIOCM_CAR),
            ri: has(libc::TIOCM_RNG),
        })
    }

    fn set_line(&self, line: libc::c_int, on: bool) -> io::Result<()> {
        let mut bits = line;
        let request = if on { libc::TIOCMBIS } else { libc::TIOCMBIC };
        ioctl_int(self.fd(), request as libc::c_ulong, &mut bits)
    }

    pub fn set_dtr(&self, on: bool) -> io::Result<()> {
        self.set_line(libc::TIOCM_DTR, on)
    }

    pub fn set_rts(&self, on: bool) -> io::Result<()> {
        self.set_line(libc::TIOCM_RTS, on)
    }

    /// Holds a break for `ms` (clamped to 1..=5000). Blocks for that long,
    /// plus the time pending output takes to go out at the line rate.
    ///
    /// TIOCSBRK first waits, with no timeout, for output to drain, including
    /// what FTDI and CP210x adapters hold in their own buffer. Under output
    /// flow control (CTS low, or XOFF received) that wait could last
    /// forever, so output flow control is switched off for the break:
    /// pending output is sent, then the break, then flow control comes back.
    /// With XON/XOFF that releases output the peer paused with XOFF; a break
    /// is a deliberate interrupt, and a hung terminal is the worse outcome.
    /// A signal during the drain (EINTR) means no break was sent.
    pub fn send_break(&self, ms: u32) -> io::Result<()> {
        self.without_output_flow_control(|| {
            ioctl_none(self.fd(), libc::TIOCSBRK as libc::c_ulong)
                .map_err(|e| io::Error::new(e.kind(), format!("break not sent: {e}")))?;
            std::thread::sleep(Duration::from_millis(ms.clamp(1, 5000).into()));
            ioctl_none(self.fd(), libc::TIOCCBRK as libc::c_ulong)
        })
    }

    /// Writes `t` with this port's rate, re-issuing IOSSIOSPEED on macOS,
    /// where termios only holds a standard placeholder rate.
    fn write_termios(&self, t: &mut Termios) -> io::Result<()> {
        t.set_speed(termios_rate(self.settings.baud))?;
        tcsetattr(&self.fd, OptionalActions::Now, t)?;
        #[cfg(target_os = "macos")]
        if termios_rate(self.settings.baud) != self.settings.baud {
            self.set_macos_speed(self.settings.baud)?;
        }
        Ok(())
    }

    /// Runs `f` with RTS/CTS and IXON (output flow control) switched off,
    /// then restores them. IXOFF stays on, so xcomm still sends XOFF when
    /// its own input fills up.
    fn without_output_flow_control<T>(&self, f: impl FnOnce() -> io::Result<T>) -> io::Result<T> {
        if self.settings.flow == Flow::None {
            return f();
        }
        let mut saved = tcgetattr(&self.fd)?;
        let mut free = saved.clone();
        free.control_modes.remove(ControlModes::CRTSCTS);
        free.input_modes.remove(InputModes::IXON);
        self.write_termios(&mut free)?;
        let result = f();
        let restored = self.write_termios(&mut saved);
        match (result, restored) {
            (Ok(v), Ok(())) => Ok(v),
            (Err(e), Ok(())) => Err(e),
            (Ok(_), Err(r)) => Err(io::Error::new(
                r.kind(),
                format!("flow control could not be restored: {r}"),
            )),
            (Err(e), Err(r)) => Err(io::Error::new(
                e.kind(),
                format!("{e}; flow control could not be restored either: {r}"),
            )),
        }
    }

    fn output_queued(&self) -> io::Result<libc::c_int> {
        let mut queued: libc::c_int = 0;
        ioctl_int(self.fd(), libc::TIOCOUTQ as libc::c_ulong, &mut queued)?;
        Ok(queued)
    }

    /// Asks the driver to deliver input without batching delay.
    #[cfg(target_os = "linux")]
    pub fn set_async_low_latency(&self) -> io::Result<LowLatency> {
        let mut s = SerialStruct::default();
        // SAFETY: TIOCGSERIAL/TIOCSSERIAL take a pointer to struct serial_struct.
        let rc = unsafe {
            libc::ioctl(
                self.fd.as_raw_fd(),
                libc::TIOCGSERIAL as _,
                &mut s as *mut SerialStruct,
            )
        };
        if rc < 0 {
            let e = io::Error::last_os_error();
            return if low_latency_refused(&e) {
                Ok(LowLatency::Unsupported)
            } else {
                Err(e)
            };
        }
        if s.flags & ASYNC_LOW_LATENCY != 0 {
            return Ok(LowLatency::Set);
        }
        s.flags |= ASYNC_LOW_LATENCY;
        // SAFETY: as above.
        let rc = unsafe {
            libc::ioctl(
                self.fd.as_raw_fd(),
                libc::TIOCSSERIAL as _,
                &s as *const SerialStruct,
            )
        };
        if rc < 0 {
            let e = io::Error::last_os_error();
            return if low_latency_refused(&e) {
                Ok(LowLatency::Unsupported)
            } else {
                Err(e)
            };
        }
        // Some drivers accept TIOCSSERIAL and drop the flag; read it back.
        let mut back = SerialStruct::default();
        // SAFETY: as above.
        let rc = unsafe {
            libc::ioctl(
                self.fd.as_raw_fd(),
                libc::TIOCGSERIAL as _,
                &mut back as *mut SerialStruct,
            )
        };
        if rc < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(if back.flags & ASYNC_LOW_LATENCY != 0 {
            LowLatency::Set
        } else {
            LowLatency::Unsupported
        })
    }

    #[cfg(not(target_os = "linux"))]
    pub fn set_async_low_latency(&self) -> io::Result<LowLatency> {
        Ok(LowLatency::Unsupported)
    }

    /// FTDI adapters batch input for `latency_timer` ms (16 by default).
    /// The sysfs file is found from the open fd's device number, so it is
    /// the right adapter even if names changed or were remapped.
    pub fn set_ftdi_latency_timer(&self, ms: u8) -> io::Result<()> {
        let rdev = rustix::fs::fstat(&self.fd)?.st_rdev;
        let path = format!(
            "/sys/dev/char/{}:{}/device/latency_timer",
            rustix::fs::major(rdev),
            rustix::fs::minor(rdev)
        );
        std::fs::write(&path, format!("{ms}\n"))
            .map_err(|e| io::Error::new(e.kind(), format!("{path}: {e}")))
    }

    pub fn set_blocking(&self, blocking: bool) -> io::Result<()> {
        let flags = fcntl_getfl(&self.fd)?;
        let flags = if blocking {
            flags - OFlags::NONBLOCK
        } else {
            flags | OFlags::NONBLOCK
        };
        Ok(fcntl_setfl(&self.fd, flags)?)
    }
}

/// How long closing waits for queued output before discarding it.
const CLOSE_DRAIN: Duration = Duration::from_millis(200);

impl Drop for Port {
    fn drop(&mut self) {
        // close(2) waits for output to drain for up to the driver's
        // closing_wait (30 s by default). Under flow control, bytes an FTDI
        // or CP210x holds in its own buffer never drain and TCOFLUSH does
        // not reach them, so flow control goes off first; then queued output
        // gets a short time and what is left is discarded.
        // Only on a port this Port configured: after a failed open the tty
        // holds the previous program's settings, which are left as they are.
        if self.configured
            && self.settings.flow != Flow::None
            && let Ok(mut t) = tcgetattr(&self.fd)
        {
            t.control_modes.remove(ControlModes::CRTSCTS);
            t.input_modes.remove(InputModes::IXON);
            let _ = self.write_termios(&mut t);
        }
        let deadline = std::time::Instant::now() + CLOSE_DRAIN;
        while matches!(self.output_queued(), Ok(n) if n > 0) && std::time::Instant::now() < deadline
        {
            std::thread::sleep(Duration::from_millis(10));
        }
        // Discard only output that is still stuck. With nothing queued there
        // is nothing to drop, and on a pty TCOFLUSH would also throw away
        // what the other end has not read yet.
        if matches!(self.output_queued(), Ok(n) if n > 0) {
            let _ = rustix::termios::tcflush(&self.fd, rustix::termios::QueueSelector::OFlush);
        }
        // TIOCEXCL outlives this fd if another fd on the tty is still open.
        // macOS has no TIOCGEXCL, so a root-only second opener there can
        // clear another program's exclusivity here.
        let _ = ioctl_none(self.fd(), libc::TIOCNXCL as libc::c_ulong);
    }
}

/// `ASYNC_LOW_LATENCY` from <linux/tty_flags.h>.
#[cfg(target_os = "linux")]
const ASYNC_LOW_LATENCY: libc::c_int = 1 << 13;

/// `struct serial_struct` from <linux/serial.h> (not in the libc crate).
#[cfg(target_os = "linux")]
#[repr(C)]
struct SerialStruct {
    kind: libc::c_int,
    line: libc::c_int,
    port: libc::c_uint,
    irq: libc::c_int,
    flags: libc::c_int,
    xmit_fifo_size: libc::c_int,
    custom_divisor: libc::c_int,
    baud_base: libc::c_int,
    close_delay: libc::c_ushort,
    io_type: libc::c_char,
    reserved_char: [libc::c_char; 1],
    hub6: libc::c_int,
    closing_wait: libc::c_ushort,
    closing_wait2: libc::c_ushort,
    iomem_base: *mut libc::c_uchar,
    iomem_reg_shift: libc::c_ushort,
    port_high: libc::c_uint,
    iomap_base: libc::c_ulong,
}

#[cfg(target_os = "linux")]
impl Default for SerialStruct {
    fn default() -> SerialStruct {
        // SAFETY: all-zero is a valid value for this plain C struct.
        unsafe { std::mem::zeroed() }
    }
}

// The kernel's struct is 72 bytes on 64-bit targets; a layout slip here would
// make TIOCSSERIAL write garbage into the driver.
#[cfg(all(target_os = "linux", target_pointer_width = "64"))]
const _: () = assert!(std::mem::size_of::<SerialStruct>() == 72);
