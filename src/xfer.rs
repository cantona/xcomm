// SPDX-FileCopyrightText: 2026 Su Kang Yin <paradyse@gmail.com>
// SPDX-License-Identifier: GPL-2.0-only

//! File transfer: lrzsz run on the serial port.

use std::io;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};

use crate::port::Port;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Proto {
    Zmodem,
    Ymodem,
    Xmodem,
}

impl Proto {
    /// `zmodem` or `z`, and so on.
    pub fn parse(name: &str) -> Option<Proto> {
        match name {
            "zmodem" | "z" => Some(Proto::Zmodem),
            "ymodem" | "y" => Some(Proto::Ymodem),
            "xmodem" | "x" => Some(Proto::Xmodem),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Send,
    Receive,
}

/// The lrzsz program for a transfer, by the name Debian and most
/// distributions install.
pub fn program(proto: Proto, dir: Direction) -> &'static str {
    match (proto, dir) {
        (Proto::Zmodem, Direction::Send) => "sz",
        (Proto::Zmodem, Direction::Receive) => "rz",
        (Proto::Ymodem, Direction::Send) => "sb",
        (Proto::Ymodem, Direction::Receive) => "rb",
        (Proto::Xmodem, Direction::Send) => "sx",
        (Proto::Xmodem, Direction::Receive) => "rx",
    }
}

/// The same program under the name lrzsz itself installs, which is the only
/// one Homebrew gives the X- and YMODEM programs.
fn prefixed(proto: Proto, dir: Direction) -> &'static str {
    match (proto, dir) {
        (Proto::Zmodem, Direction::Send) => "lsz",
        (Proto::Zmodem, Direction::Receive) => "lrz",
        (Proto::Ymodem, Direction::Send) => "lsb",
        (Proto::Ymodem, Direction::Receive) => "lrb",
        (Proto::Xmodem, Direction::Send) => "lsx",
        (Proto::Xmodem, Direction::Receive) => "lrx",
    }
}

#[derive(Debug)]
pub enum XferError {
    /// Neither name of the program is on PATH; holds the plain name.
    NotFound(&'static str),
    Spawn(&'static str, io::Error),
    /// The receiver could not be kept to the working directory, so it was
    /// not started.
    Confine(&'static str, io::Error),
    /// The program started, but waiting for it failed.
    Wait(&'static str, io::Error),
    /// The port could not be switched for the child.
    Port(io::Error),
    Failed {
        program: &'static str,
        status: ExitStatus,
    },
}

impl std::fmt::Display for XferError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        use std::os::unix::process::ExitStatusExt;
        match self {
            XferError::NotFound(p) => write!(f, "{p} not found"),
            XferError::Spawn(p, e) => write!(f, "cannot start {p}: {e}"),
            XferError::Wait(p, e) => write!(f, "waiting for {p}: {e}"),
            XferError::Confine(p, e) => {
                write!(f, "cannot keep {p} to the working directory: {e}")
            }
            XferError::Port(e) => write!(f, "port: {e}"),
            XferError::Failed { program, status } => {
                match (status.code(), status.signal()) {
                    (Some(code), _) => write!(f, "{program} exited with status {code}"),
                    (None, Some(sig)) => {
                        write!(f, "{program} was killed by signal {sig}")
                    }
                    (None, None) => write!(f, "{program} ended: {status}"),
                }
            }
        }
    }
}

impl std::error::Error for XferError {}

/// A running lrzsz program. Its stdin and stdout are the port, set blocking
/// for it; its stderr is xcomm's, for progress, unless no one watches it.
pub struct Transfer<'a> {
    port: &'a Port,
    child: Child,
    program: &'static str,
    /// Set once the child has been waited for.
    status: Option<ExitStatus>,
    /// The port was given back by release().
    released: bool,
}

impl<'a> Transfer<'a> {
    /// Starts the program for `proto` and `dir` in `cwd`, with `files` as
    /// its arguments (the files to send, or the name to receive an XMODEM
    /// file as). `progress` false sends the program's messages nowhere.
    pub fn start(
        port: &'a Port,
        proto: Proto,
        dir: Direction,
        files: &[PathBuf],
        cwd: &Path,
        progress: bool,
    ) -> Result<Transfer<'a>, XferError> {
        let plain = program(proto, dir);
        let other = prefixed(proto, dir);
        let (name, path) = match find_program(plain) {
            Some(path) => (plain, path),
            None => match find_program(other) {
                Some(path) => (other, path),
                None => return Err(XferError::NotFound(plain)),
            },
        };
        // The sender names ZMODEM and YMODEM files, and lrzsz takes absolute
        // names: the receiver may write only below `cwd`. An XMODEM name is
        // the user's own.
        let confine = dir == Direction::Receive && proto != Proto::Xmodem;
        let jail = if confine {
            Some(Jail::new(cwd).map_err(|e| XferError::Confine(name, e))?)
        } else {
            None
        };
        port.set_blocking(true).map_err(XferError::Port)?;
        let spawn = || -> io::Result<Child> {
            let input = port.fd().try_clone_to_owned()?;
            let output = input.try_clone()?;
            let mut cmd = match &jail {
                Some(jail) => jail.command(&path),
                None => Command::new(&path),
            };
            // lrzsz's own restriction on top: no names starting with a dot
            // (.profile, .ssh/) and no new directories, even below `cwd`.
            if confine {
                cmd.args(["-R", "-R"]);
            }
            // A name starting with '-' is still a file.
            if !files.is_empty() {
                cmd.arg("--").args(files);
            }
            cmd.current_dir(cwd)
                .stdin(Stdio::from(input))
                .stdout(Stdio::from(output))
                .stderr(if progress {
                    Stdio::inherit()
                } else {
                    Stdio::null()
                });
            stop_with_parent(&mut cmd);
            cmd.spawn()
        };
        let started = spawn()
            .map(|child| (child, name))
            .map_err(|e| XferError::Spawn(name, e));
        match started {
            Ok((child, program)) => Ok(Transfer {
                port,
                child,
                program,
                status: None,
                released: false,
            }),
            Err(e) => {
                let _ = port.set_blocking(false);
                Err(e)
            }
        }
    }

    /// The program that runs.
    pub fn program(&self) -> &'static str {
        self.program
    }

    pub fn id(&self) -> u32 {
        self.child.id()
    }

    /// The exit status once the program has ended; a non-zero one is an
    /// error.
    pub fn try_finish(&mut self) -> Result<Option<ExitStatus>, XferError> {
        let status = match self.status {
            Some(status) => status,
            None => match self.child.try_wait() {
                Ok(Some(status)) => {
                    self.status = Some(status);
                    status
                }
                Ok(None) => return Ok(None),
                Err(e) => return Err(XferError::Wait(self.program, e)),
            },
        };
        if status.success() {
            Ok(Some(status))
        } else {
            Err(XferError::Failed {
                program: self.program,
                status,
            })
        }
    }

    /// Asks the program to stop (SIGTERM): lrzsz then cancels the transfer
    /// on the line and puts the port's modes back.
    pub fn abort(&mut self) {
        if self.status.is_none() {
            // SAFETY: kill(2) on the child's pid, which is not yet reaped.
            unsafe { libc::kill(self.child.id() as libc::pid_t, libc::SIGTERM) };
        }
    }

    /// Ends the program now (SIGKILL) and waits for it; try_finish then
    /// reports how it ended.
    pub fn kill(&mut self) {
        if self.status.is_none() {
            let _ = self.child.kill();
            if let Ok(status) = self.child.wait() {
                self.status = Some(status);
            }
        }
    }

    /// Ends the program if it still runs and gives the port back to the
    /// session (non-blocking again).
    pub fn release(mut self) -> io::Result<()> {
        self.kill();
        self.released = true;
        self.port.set_blocking(false)
    }
}

/// `name` as the shell would find it on PATH: an executable file, by an
/// absolute path. The program runs in the transfer's directory, so a
/// relative PATH entry (`.`, or an empty one) is taken from where xcomm
/// runs now; left relative, it would name a file in the transfer directory
/// (one an earlier transfer may have written).
fn find_program(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    // Only a relative entry needs it: a directory removed since xcomm
    // started still finds a program on an absolute one.
    find_program_in(name, &path, std::env::current_dir().ok().as_deref())
}

fn find_program_in(
    name: &str,
    path: &std::ffi::OsStr,
    cwd: Option<&Path>,
) -> Option<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    std::env::split_paths(path)
        .filter_map(|dir| match cwd {
            _ if dir.is_absolute() => Some(dir.join(name)),
            Some(cwd) => Some(cwd.join(dir).join(name)),
            None => None,
        })
        .find(|p| {
            std::fs::metadata(p)
                .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        })
}

/// Linux: Landlock (kernel 5.13 and later) lets the program create, change
/// and remove files only below the working directory, and write to
/// /dev/null and /dev/tty; it may read anything. Nothing else below /dev:
/// /dev/shm is a writable file system, and other terminals and ports are
/// devices it could write into.
#[cfg(target_os = "linux")]
struct Jail {
    cwd: std::os::fd::OwnedFd,
    null: std::os::fd::OwnedFd,
    tty: std::os::fd::OwnedFd,
    access: u64,
    /// The rights that apply to a file (not a directory) of those.
    file_access: u64,
}

#[cfg(target_os = "linux")]
mod landlock {
    //! From <linux/landlock.h>; not in the libc crate.
    pub const CREATE_RULESET_VERSION: libc::c_uint = 1 << 0;
    pub const RULE_PATH_BENEATH: libc::c_int = 1;
    pub const WRITE_FILE: u64 = 1 << 1;
    pub const REMOVE_DIR: u64 = 1 << 4;
    pub const REMOVE_FILE: u64 = 1 << 5;
    pub const MAKE_CHAR: u64 = 1 << 6;
    pub const MAKE_DIR: u64 = 1 << 7;
    pub const MAKE_REG: u64 = 1 << 8;
    pub const MAKE_SOCK: u64 = 1 << 9;
    pub const MAKE_FIFO: u64 = 1 << 10;
    pub const MAKE_BLOCK: u64 = 1 << 11;
    pub const MAKE_SYM: u64 = 1 << 12;
    /// ABI 2.
    pub const REFER: u64 = 1 << 13;
    /// ABI 3.
    pub const TRUNCATE: u64 = 1 << 14;

    /// Only the first field: the kernel takes a shorter struct as an older
    /// ABI's.
    #[repr(C)]
    pub struct RulesetAttr {
        pub handled_access_fs: u64,
    }

    #[repr(C, packed)]
    pub struct PathBeneathAttr {
        pub allowed_access: u64,
        pub parent_fd: i32,
    }
}

#[cfg(target_os = "linux")]
impl Jail {
    fn new(cwd: &Path) -> io::Result<Jail> {
        use landlock::*;
        // SAFETY: the version query takes no attribute.
        let abi = unsafe {
            libc::syscall(
                libc::SYS_landlock_create_ruleset,
                std::ptr::null::<RulesetAttr>(),
                0usize,
                CREATE_RULESET_VERSION,
            )
        };
        if abi < 1 {
            let e = io::Error::last_os_error();
            return Err(io::Error::new(
                e.kind(),
                format!("Landlock is not available ({e})"),
            ));
        }
        let mut access = WRITE_FILE
            | REMOVE_DIR
            | REMOVE_FILE
            | MAKE_CHAR
            | MAKE_DIR
            | MAKE_REG
            | MAKE_SOCK
            | MAKE_FIFO
            | MAKE_BLOCK
            | MAKE_SYM;
        if abi >= 2 {
            access |= REFER;
        }
        if abi >= 3 {
            access |= TRUNCATE;
        }
        // O_PATH opens no device: /dev/tty is fine without a terminal.
        let path = |p: &Path, extra: rustix::fs::OFlags| {
            rustix::fs::open(
                p,
                rustix::fs::OFlags::PATH | rustix::fs::OFlags::CLOEXEC | extra,
                rustix::fs::Mode::empty(),
            )
            .map_err(|e| {
                io::Error::new(io::Error::from(e).kind(), format!("{}: {e}", p.display()))
            })
        };
        Ok(Jail {
            cwd: path(cwd, rustix::fs::OFlags::DIRECTORY)?,
            null: path(Path::new("/dev/null"), rustix::fs::OFlags::empty())?,
            tty: path(Path::new("/dev/tty"), rustix::fs::OFlags::empty())?,
            access,
            file_access: access & (WRITE_FILE | TRUNCATE),
        })
    }

    fn command(&self, program: &Path) -> Command {
        use std::os::fd::AsRawFd;
        use std::os::unix::process::CommandExt;
        let (cwd, null, tty) = (
            self.cwd.as_raw_fd(),
            self.null.as_raw_fd(),
            self.tty.as_raw_fd(),
        );
        let (access, file_access) = (self.access, self.file_access);
        let mut cmd = Command::new(program);
        // SAFETY: only syscalls between fork and exec; no allocation.
        unsafe {
            cmd.pre_exec(move || {
                use landlock::*;
                let attr = RulesetAttr {
                    handled_access_fs: access,
                };
                let ruleset = libc::syscall(
                    libc::SYS_landlock_create_ruleset,
                    &attr as *const RulesetAttr,
                    std::mem::size_of::<RulesetAttr>(),
                    0u32,
                );
                if ruleset < 0 {
                    return Err(io::Error::last_os_error());
                }
                let ruleset = ruleset as libc::c_int;
                let rules = [
                    PathBeneathAttr {
                        allowed_access: access,
                        parent_fd: cwd,
                    },
                    PathBeneathAttr {
                        allowed_access: file_access,
                        parent_fd: null,
                    },
                    PathBeneathAttr {
                        allowed_access: file_access,
                        parent_fd: tty,
                    },
                ];
                for rule in &rules {
                    if libc::syscall(
                        libc::SYS_landlock_add_rule,
                        ruleset,
                        RULE_PATH_BENEATH,
                        rule as *const PathBeneathAttr,
                        0u32,
                    ) != 0
                    {
                        return Err(io::Error::last_os_error());
                    }
                }
                if libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0
                    || libc::syscall(libc::SYS_landlock_restrict_self, ruleset, 0u32) != 0
                {
                    return Err(io::Error::last_os_error());
                }
                libc::close(ruleset);
                Ok(())
            });
        }
        cmd
    }
}

/// macOS: the program runs under sandbox-exec with writes denied outside
/// the working directory, /dev/null and /dev/tty.
#[cfg(target_os = "macos")]
struct Jail {
    profile: String,
}

#[cfg(target_os = "macos")]
const SANDBOX_EXEC: &str = "/usr/bin/sandbox-exec";

#[cfg(target_os = "macos")]
impl Jail {
    fn new(cwd: &Path) -> io::Result<Jail> {
        if !Path::new(SANDBOX_EXEC).is_file() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("{SANDBOX_EXEC} not found"),
            ));
        }
        // The sandbox matches resolved paths (/tmp is /private/tmp).
        let dir = std::fs::canonicalize(cwd)
            .map_err(|e| io::Error::new(e.kind(), format!("{}: {e}", cwd.display())))?;
        let dir = dir
            .to_str()
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "the working directory is not UTF-8",
                )
            })?
            .replace('\\', "\\\\")
            .replace('"', "\\\"");
        Ok(Jail {
            profile: format!(
                "(version 1)(allow default)(deny file-write* (require-not (require-any (subpath \"{dir}\") (literal \"/dev/null\") (literal \"/dev/tty\"))))"
            ),
        })
    }

    fn command(&self, program: &Path) -> Command {
        let mut cmd = Command::new(SANDBOX_EXEC);
        cmd.arg("-p").arg(&self.profile).arg(program);
        cmd
    }
}

/// Linux: the program gets SIGTERM if xcomm dies without stopping it
/// (SIGKILL), so it does not go on using the port once the lock is free.
/// macOS has no such thing; there the program runs until lrzsz's own
/// timeouts end it.
#[cfg(target_os = "linux")]
fn stop_with_parent(cmd: &mut Command) {
    use std::os::unix::process::CommandExt;
    let parent = rustix::process::getpid().as_raw_nonzero().get();
    // SAFETY: prctl(2) and getppid(2) are async-signal-safe.
    unsafe {
        cmd.pre_exec(move || {
            if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM) != 0 {
                return Err(io::Error::last_os_error());
            }
            // xcomm died before the prctl took effect.
            if libc::getppid() != parent {
                return Err(io::Error::from_raw_os_error(libc::ESRCH));
            }
            Ok(())
        });
    }
}

#[cfg(not(target_os = "linux"))]
fn stop_with_parent(_cmd: &mut Command) {}

impl Drop for Transfer<'_> {
    fn drop(&mut self) {
        if !self.released {
            self.kill();
            let _ = self.port.set_blocking(false);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::find_program_in;
    use std::os::unix::fs::PermissionsExt;
    use std::path::Path;

    #[test]
    fn a_relative_path_entry_is_taken_from_where_xcomm_runs() {
        let here =
            std::env::temp_dir().join(format!("xcomm-find-{}", std::process::id()));
        std::fs::create_dir_all(here.join("sub")).unwrap();
        for p in [here.join("rz"), here.join("sub/sz")] {
            std::fs::write(&p, "#!/bin/sh\n").unwrap();
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let find = |name, path: &str| find_program_in(name, path.as_ref(), Some(&here));
        // Absolute whatever the entry: the program runs in another directory.
        assert_eq!(find("rz", ".:/nonexistent"), Some(here.join("./rz")));
        assert_eq!(find("rz", ":/nonexistent"), Some(here.join("rz")));
        assert_eq!(find("sz", "sub"), Some(here.join("sub/sz")));
        assert!(find("rz", "sub").is_none());
        let abs = here.join("sub");
        assert_eq!(find("sz", abs.to_str().unwrap()), Some(abs.join("sz")));
        assert!(find("rz", "").map(|p| p.is_absolute()).unwrap_or(false));
        assert!(Path::new(&find("rz", ".").unwrap()).is_absolute());
        // No cwd (removed since xcomm started): relative entries are
        // skipped, absolute ones still found.
        let abs = format!(".:{}", here.join("sub").display());
        assert_eq!(
            find_program_in("sz", abs.as_ref(), None),
            Some(here.join("sub/sz"))
        );
        assert!(find_program_in("rz", ".".as_ref(), None).is_none());
        std::fs::remove_dir_all(&here).unwrap();
    }
}
