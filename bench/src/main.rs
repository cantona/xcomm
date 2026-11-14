//! Benchmarks a serial terminal on a pty pair: device-to-terminal
//! throughput, CPU and memory per MiB, keystroke-to-device latency and
//! syscalls per MiB. The same harness runs xcomm, minicom, picocom and tio,
//! and `compare` gates a change against its parent.

use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};
use std::time::{Duration, Instant};

use rustix::event::{PollFd, PollFlags, Timespec, poll};
use rustix::fs::{Mode, OFlags};
use rustix::pty::{OpenptFlags, grantpt, openpt, ptsname, unlockpt};
use serde_json::{Value, json};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Xcomm,
    Minicom,
    Picocom,
    Tio,
}

impl Kind {
    fn parse(s: &str) -> Result<Kind, String> {
        match s {
            "xcomm" => Ok(Kind::Xcomm),
            "minicom" => Ok(Kind::Minicom),
            "picocom" => Ok(Kind::Picocom),
            "tio" => Ok(Kind::Tio),
            _ => Err(format!(
                "unknown tool kind '{s}' (xcomm, minicom, picocom, tio)"
            )),
        }
    }

    fn name(self) -> &'static str {
        match self {
            Kind::Xcomm => "xcomm",
            Kind::Minicom => "minicom",
            Kind::Picocom => "picocom",
            Kind::Tio => "tio",
        }
    }

    /// minicom emulates a terminal and rewrites what it receives, so its
    /// output cannot be counted byte for byte.
    fn passes_bytes_through(self) -> bool {
        self != Kind::Minicom
    }

    /// The command line for a plain session at 115200 8N1 on `dev`, with no
    /// configuration file, init string or lock.
    fn command(self, tool: &str, dev: &Path, home: &Path) -> Command {
        let mut c = Command::new(tool);
        let dev = dev.to_str().expect("pty paths are UTF-8");
        match self {
            Kind::Xcomm => c.args(["--no-lock", "-b", "115200", dev]),
            Kind::Minicom => c.args(["-o", "-D", dev, "-b", "115200"]),
            Kind::Picocom => c.args(["-q", "--noreset", "-b", "115200", dev]),
            Kind::Tio => c.args(["-b", "115200", "-n", "--mute", dev]),
        };
        c.env("HOME", home)
            .env("XDG_CONFIG_HOME", home.join(".config"))
            .env("LANG", "C")
            .env_remove("NO_COLOR")
            .env("LC_ALL", "C")
            .env("TERM", "xterm")
            .env("XCOMM_PREFIX", "ctrl-a")
            .env_remove("MINICOM");
        c
    }
}

struct Pty {
    master: OwnedFd,
    slave: OwnedFd,
    path: PathBuf,
}

fn pty() -> Result<Pty, String> {
    // CLOEXEC: the tool under test must not inherit the harness's ends.
    #[cfg(target_os = "linux")]
    let flags = OpenptFlags::RDWR | OpenptFlags::NOCTTY | OpenptFlags::CLOEXEC;
    #[cfg(not(target_os = "linux"))]
    let flags = OpenptFlags::RDWR | OpenptFlags::NOCTTY;
    let master = openpt(flags).map_err(|e| format!("openpt: {e}"))?;
    #[cfg(not(target_os = "linux"))]
    rustix::io::fcntl_setfd(&master, rustix::io::FdFlags::CLOEXEC).map_err(|e| e.to_string())?;
    grantpt(&master).map_err(|e| format!("grantpt: {e}"))?;
    unlockpt(&master).map_err(|e| format!("unlockpt: {e}"))?;
    let name = ptsname(&master, Vec::new()).map_err(|e| format!("ptsname: {e}"))?;
    let path = PathBuf::from(name.to_str().map_err(|e| e.to_string())?);
    let slave = rustix::fs::open(
        &path,
        OFlags::RDWR | OFlags::NOCTTY | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(|e| format!("open {}: {e}", path.display()))?;
    let mut t = rustix::termios::tcgetattr(&slave).map_err(|e| e.to_string())?;
    t.make_raw();
    rustix::termios::tcsetattr(&slave, rustix::termios::OptionalActions::Now, &t)
        .map_err(|e| e.to_string())?;
    Ok(Pty {
        master,
        slave,
        path,
    })
}

fn wait_readable(fd: BorrowedFd<'_>, timeout: Duration) -> bool {
    let mut fds = [PollFd::new(&fd, PollFlags::IN)];
    let ts = Timespec::try_from(timeout).unwrap_or(Timespec {
        tv_sec: 1,
        tv_nsec: 0,
    });
    matches!(poll(&mut fds, Some(&ts)), Ok(n) if n > 0)
}

/// Reads until nothing arrives for `quiet` (startup banners); returns up to
/// 4 KiB of what was read, for error messages.
fn drain_quiet(fd: BorrowedFd<'_>, quiet: Duration, limit: Duration) -> Vec<u8> {
    let end = Instant::now() + limit;
    let mut buf = [0u8; 65536];
    let mut seen = Vec::new();
    while Instant::now() < end && wait_readable(fd, quiet) {
        let n = rustix::io::read(fd, &mut buf).unwrap_or(0);
        if n == 0 {
            break;
        }
        let room = 4096usize.saturating_sub(seen.len());
        seen.extend_from_slice(&buf[..n.min(room)]);
    }
    seen
}

fn write_all(fd: BorrowedFd<'_>, mut bytes: &[u8]) -> Result<(), String> {
    while !bytes.is_empty() {
        match rustix::io::write(fd, bytes) {
            Ok(n) => bytes = &bytes[n..],
            Err(rustix::io::Errno::INTR) => {}
            Err(e) => return Err(format!("write: {e}")),
        }
    }
    Ok(())
}

fn text_payload(size: usize) -> Vec<u8> {
    let line = b"0123456789abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ!?\r\n";
    let mut v = Vec::with_capacity(size + 32);
    while v.len() < size {
        v.extend_from_slice(line);
    }
    v.truncate(size);
    v.extend_from_slice(b"\r\nXCOMM-BENCH-END\r\n");
    v
}

fn binary_payload(size: usize) -> Vec<u8> {
    let mut x: u64 = 0x9e37_79b9_7f4a_7c15;
    (0..size)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x as u8
        })
        .collect()
}

/// Feeds `data` into the device side and times until the terminal side has
/// seen `done` (all bytes counted, or the sentinel found).
fn throughput(
    dev: &Pty,
    host: &Pty,
    data: Vec<u8>,
    count_bytes: Option<usize>,
) -> Result<f64, String> {
    let master = dev.master.try_clone().map_err(|e| e.to_string())?;
    let mib = data.len() as f64 / (1024.0 * 1024.0);
    let started = Instant::now();
    let feeder = std::thread::spawn(move || write_all(master.as_fd(), &data));
    let mut got = 0usize;
    let mut tail: Vec<u8> = Vec::new();
    let mut buf = vec![0u8; 1 << 16];
    // No overall deadline: a slow tool (tio moves about 1 MiB/s) just takes
    // long. A stall is caught by the quiet check below.
    loop {
        if !wait_readable(host.master.as_fd(), Duration::from_secs(5)) {
            return Err(format!("terminal side went quiet after {got} bytes"));
        }
        let n = rustix::io::read(host.master.as_fd(), &mut buf[..]).map_err(|e| e.to_string())?;
        got += n;
        match count_bytes {
            Some(want) if got >= want => break,
            Some(_) => {}
            None => {
                tail.extend_from_slice(&buf[..n]);
                if tail.windows(17).any(|w| w == b"XCOMM-BENCH-END\r\n") {
                    break;
                }
                let keep = tail.len().saturating_sub(32);
                tail.drain(..keep);
            }
        }
    }
    let secs = started.elapsed().as_secs_f64();
    feeder.join().map_err(|_| "feeder panicked".to_string())??;
    Ok(mib / secs)
}

/// Keystroke-to-device latency in microseconds, sorted.
fn latency(dev: &Pty, host: &Pty, samples: usize) -> Result<Vec<f64>, String> {
    let mut out = Vec::with_capacity(samples);
    let mut b = [0u8; 64];
    for _ in 0..samples {
        let t = Instant::now();
        write_all(host.master.as_fd(), b"a")?;
        if !wait_readable(dev.master.as_fd(), Duration::from_secs(2)) {
            return Err("a keystroke did not reach the device".into());
        }
        let n = rustix::io::read(dev.master.as_fd(), &mut b).map_err(|e| e.to_string())?;
        out.push(t.elapsed().as_secs_f64() * 1e6);
        // Anything but exactly the one byte would leave data that makes the
        // next sample return at once.
        if b[..n] != *b"a" {
            return Err(format!("one keystroke reached the device as {:?}", &b[..n]));
        }
    }
    out.sort_by(|a, b| a.total_cmp(b));
    Ok(out)
}

fn percentile(sorted: &[f64], p: f64) -> f64 {
    let i = ((sorted.len() as f64 - 1.0) * p).round() as usize;
    sorted[i]
}

struct Running {
    pid: libc::pid_t,
    /// Set once the child has been reaped: its pid may then belong to
    /// another process and must not be signalled.
    reaped: Option<libc::c_int>,
}

fn spawn(cmd: &mut Command, host: &Pty) -> Result<Running, String> {
    let s = || host.slave.try_clone().map_err(|e| e.to_string());
    cmd.stdin(Stdio::from(s()?))
        .stdout(Stdio::from(s()?))
        .stderr(Stdio::from(s()?));
    // SAFETY: setsid and ioctl are async-signal-safe.
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() < 0 || libc::ioctl(0, libc::TIOCSCTTY as _, 0) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let child = cmd.spawn().map_err(|e| format!("spawn: {e}"))?;
    Ok(Running {
        pid: child.id() as libc::pid_t,
        reaped: None,
    })
}

/// Peak resident memory of `pid` since its exec, in KiB.
fn peak_rss_kib(pid: libc::pid_t) -> Result<u64, String> {
    let path = format!("/proc/{pid}/status");
    let status = std::fs::read_to_string(&path).map_err(|e| format!("{path}: {e}"))?;
    status
        .lines()
        .find_map(|l| l.strip_prefix("VmHWM:"))
        .and_then(|v| v.split_whitespace().next())
        .and_then(|v| v.parse().ok())
        .ok_or_else(|| format!("{path}: no VmHWM"))
}

/// Ends the tool (SIGTERM, SIGKILL after 10 s) and reaps it.
fn finish(r: Running) -> Result<(), String> {
    if let Some(status) = r.reaped {
        return Err(format!(
            "the tool had already exited (wait status {status:#x})"
        ));
    }
    let reap = |flags| {
        let mut status = 0;
        // SAFETY: waitpid on our own child.
        match unsafe { libc::waitpid(r.pid, &mut status, flags) } {
            w if w == r.pid => Ok(true),
            0 => Ok(false),
            _ => Err(format!("waitpid: {}", std::io::Error::last_os_error())),
        }
    };
    // SAFETY: signals to our own, not yet reaped, child.
    unsafe { libc::kill(r.pid, libc::SIGTERM) };
    // minicom takes about 3 s to exit on SIGTERM; killing it sooner would
    // leave its lock file behind.
    let deadline = Instant::now() + Duration::from_secs(10);
    while !reap(libc::WNOHANG)? {
        if Instant::now() > deadline {
            // SAFETY: as above.
            unsafe { libc::kill(r.pid, libc::SIGKILL) };
            reap(0)?;
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    Ok(())
}

/// CPU time the process has used so far, in ms, summed over its threads from
/// the scheduler's nanosecond counters. Read around the throughput phases
/// only, so startup and the latency phase do not dilute a regression there.
fn cpu_ms(pid: libc::pid_t) -> Result<f64, String> {
    let dir = format!("/proc/{pid}/task");
    let mut ns = 0u64;
    for task in std::fs::read_dir(&dir).map_err(|e| format!("{dir}: {e}"))? {
        let task = task.map_err(|e| e.to_string())?;
        let stat = std::fs::read_to_string(task.path().join("schedstat"))
            .map_err(|e| format!("{}: {e}", task.path().display()))?;
        ns += stat
            .split_whitespace()
            .next()
            .and_then(|v| v.parse::<u64>().ok())
            .ok_or_else(|| format!("unexpected schedstat: {stat}"))?;
    }
    Ok(ns as f64 / 1e6)
}

/// A temporary HOME for the tool, removed when dropped.
struct Home(PathBuf);

impl Drop for Home {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

struct Opts {
    tool: String,
    kind: Kind,
    size_mib: usize,
    runs: usize,
    samples: usize,
    strace: bool,
}

struct Sample {
    text: f64,
    binary: Option<f64>,
    cpu_ms_per_mib: f64,
    rss_kib: u64,
    p50: f64,
    p99: f64,
}

fn home_dir() -> Result<Home, String> {
    use std::sync::atomic::{AtomicUsize, Ordering};
    static N: AtomicUsize = AtomicUsize::new(0);
    let d = std::env::temp_dir().join(format!(
        "xcomm-bench-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ));
    // create_dir, not create_dir_all: an existing directory of that name
    // (left over, or put there by someone else) is not reused as HOME.
    std::fs::create_dir(&d).map_err(|e| format!("{}: {e}", d.display()))?;
    Ok(Home(d))
}

/// Fails if the tool has already exited, with what it printed.
fn still_running(r: &mut Running, what: &str, output: &[u8]) -> Result<(), String> {
    if r.reaped.is_none() {
        let mut status = 0;
        // SAFETY: a non-blocking wait on our own child.
        if unsafe { libc::waitpid(r.pid, &mut status, libc::WNOHANG) } == r.pid {
            r.reaped = Some(status);
        }
    }
    match r.reaped {
        Some(status) => Err(format!(
            "the tool exited {what} (wait status {status:#x}): {}",
            String::from_utf8_lossy(output).trim()
        )),
        None => Ok(()),
    }
}

fn one_run(o: &Opts) -> Result<Sample, String> {
    let dev = pty()?;
    let host = pty()?;
    let home = home_dir()?;
    let mut cmd = o.kind.command(&o.tool, &dev.path, &home.0);
    let mut running = spawn(&mut cmd, &host)?;
    let opened = wait_open(running.pid, &dev.path, Duration::from_secs(10));
    let startup = drain_quiet(
        host.master.as_fd(),
        Duration::from_millis(300),
        Duration::from_secs(5),
    );
    if let Err(e) = opened {
        still_running(&mut running, "at startup", &startup)?;
        let _ = finish(running);
        return Err(e);
    }
    let size = o.size_mib << 20;
    let pid = running.pid;
    let result = (|| {
        still_running(&mut running, "at startup", &startup)?;
        let cpu_before = cpu_ms(pid)?;
        let crashed = |e: String, running: &mut Running| match still_running(
            running,
            "during the run",
            b"",
        ) {
            Err(why) => format!("{e}; {why}"),
            Ok(()) => e,
        };
        let text = throughput(&dev, &host, text_payload(size), None)
            .map_err(|e| crashed(e, &mut running))?;
        // Nothing of the text phase may be counted in the binary phase.
        drain_quiet(
            host.master.as_fd(),
            Duration::from_millis(100),
            Duration::from_secs(5),
        );
        let binary = if o.kind.passes_bytes_through() {
            Some(
                throughput(&dev, &host, binary_payload(size), Some(size))
                    .map_err(|e| crashed(e, &mut running))?,
            )
        } else {
            None
        };
        let cpu = cpu_ms(pid)? - cpu_before;
        drain_quiet(
            host.master.as_fd(),
            Duration::from_millis(200),
            Duration::from_secs(5),
        );
        let lat = latency(&dev, &host, o.samples).map_err(|e| crashed(e, &mut running))?;
        Ok::<_, String>((text, binary, cpu, lat))
    })();
    // VmHWM, not ru_maxrss: the forked child counts the harness's own pages
    // until exec, and ru_maxrss keeps that peak; VmHWM starts at exec.
    let rss = peak_rss_kib(pid);
    let finished = finish(running);
    let (text, binary, cpu, lat) = result?;
    finished?;
    let rss = rss?;
    let mib_moved = o.size_mib as f64 * if binary.is_some() { 2.0 } else { 1.0 };
    Ok(Sample {
        text,
        binary,
        cpu_ms_per_mib: cpu / mib_moved,
        rss_kib: rss,
        p50: percentile(&lat, 0.5),
        p99: percentile(&lat, 0.99),
    })
}

/// Syscalls per MiB of device output, counted by strace over a text run.
fn syscalls(o: &Opts) -> Result<Option<f64>, String> {
    if !o.strace {
        return Ok(None);
    }
    let dev = pty()?;
    let host = pty()?;
    let home = home_dir()?;
    let trace = home.0.join("trace");
    let inner = o.kind.command(&o.tool, &dev.path, &home.0);
    let mut cmd = Command::new("strace");
    cmd.args(["-f", "-c", "-o"])
        .arg(&trace)
        .arg(inner.get_program());
    cmd.args(inner.get_args());
    for (k, v) in inner.get_envs() {
        match v {
            Some(v) => cmd.env(k, v),
            None => cmd.env_remove(k),
        };
    }
    let mut running = spawn(&mut cmd, &host)?;
    let opened = wait_open(running.pid, &dev.path, Duration::from_secs(10));
    let startup = drain_quiet(
        host.master.as_fd(),
        Duration::from_millis(300),
        Duration::from_secs(5),
    );
    if let Err(e) = opened {
        still_running(&mut running, "at startup", &startup)?;
        let _ = finish(running);
        return Err(e);
    }
    let size = o.size_mib << 20;
    let r = throughput(&dev, &host, text_payload(size), None).map_err(|e| {
        match still_running(&mut running, "during the strace run", b"") {
            Err(why) => format!("{e}; {why}"),
            Ok(()) => e,
        }
    });
    // strace does not pass SIGTERM on: end the traced program, and strace
    // writes its summary and exits after it.
    let traced = match children(running.pid) {
        Ok(c) => c,
        Err(e) => {
            let _ = finish(running);
            let mut msg = format!("cannot find the traced program: {e}");
            if let Err(first) = &r {
                msg = format!("{first}; {msg}");
            }
            return Err(msg);
        }
    };
    for child in traced {
        // SAFETY: a plain kill of a process we started.
        unsafe { libc::kill(child, libc::SIGTERM) };
    }
    let finished = finish(running);
    let report = std::fs::read_to_string(&trace).map_err(|e| format!("{}: {e}", trace.display()));
    finished?;
    r?;
    let report = report?;
    let total = calls_in_summary(&report)
        .ok_or_else(|| format!("cannot read the strace summary:\n{report}"))?;
    Ok(Some(total / o.size_mib as f64))
}

/// The direct children of `pid` (Linux /proc).
fn children(pid: libc::pid_t) -> Result<Vec<libc::pid_t>, String> {
    let path = format!("/proc/{pid}/task/{pid}/children");
    Ok(std::fs::read_to_string(&path)
        .map_err(|e| format!("{path}: {e}"))?
        .split_whitespace()
        .filter_map(|p| p.parse().ok())
        .collect())
}

/// Waits until `pid` or one of its children has `dev` open, so the timed
/// phases start with the tool actually on the device (the tools print
/// nothing at startup, so a quiet terminal proves nothing).
fn wait_open(pid: libc::pid_t, dev: &Path, timeout: Duration) -> Result<(), String> {
    let deadline = Instant::now() + timeout;
    loop {
        let mut pids = vec![pid];
        pids.extend(children(pid).unwrap_or_default());
        for p in pids {
            if let Ok(entries) = std::fs::read_dir(format!("/proc/{p}/fd")) {
                for e in entries.flatten() {
                    if std::fs::read_link(e.path()).is_ok_and(|t| t == dev) {
                        return Ok(());
                    }
                }
            }
        }
        if Instant::now() > deadline {
            return Err(format!(
                "the tool did not open {} within {timeout:?}",
                dev.display()
            ));
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// The call count on the `total` line of `strace -c`. Columns are right
/// aligned under the header, and some may be empty (usecs/call, errors), so
/// the count is the number that ends where the "calls" header ends.
fn calls_in_summary(report: &str) -> Option<f64> {
    let header = report.lines().find(|l| l.contains("calls"))?;
    let end = header.find("calls")? + "calls".len();
    let total = report.lines().find(|l| l.trim_end().ends_with(" total"))?;
    total
        .get(..end)?
        .split_whitespace()
        .last()?
        .parse::<f64>()
        .ok()
}

fn median(mut v: Vec<f64>) -> f64 {
    v.sort_by(|a, b| a.total_cmp(b));
    v[v.len() / 2]
}

fn machine() -> Value {
    let cpu = std::fs::read_to_string("/proc/cpuinfo")
        .ok()
        .and_then(|s| {
            s.lines()
                .find(|l| l.starts_with("model name"))
                .and_then(|l| l.split(':').nth(1))
                .map(|m| m.trim().to_string())
        })
        .unwrap_or_default();
    let uname = rustix::system::uname();
    let rustc = Command::new("rustc")
        .arg("--version")
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default();
    json!({
        "cpu": cpu,
        "rustc": rustc,
        "kernel": uname.release().to_string_lossy(),
        "cpus": std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1),
    })
}

fn report(o: &Opts, samples: &[Sample]) -> Result<Value, String> {
    let pick = |f: &dyn Fn(&Sample) -> f64| median(samples.iter().map(f).collect());
    let binary = if o.kind.passes_bytes_through() {
        json!(pick(&|s| s.binary.unwrap_or(0.0)))
    } else {
        Value::Null
    };
    Ok(json!({
        "tool": o.kind.name(),
        "size_mib": o.size_mib,
        "runs": samples.len(),
        "throughput_text_mib_s": pick(&|s| s.text),
        "throughput_bin_mib_s": binary,
        "cpu_ms_per_mib": pick(&|s| s.cpu_ms_per_mib),
        "peak_rss_kib": pick(&|s| s.rss_kib as f64),
        "latency_p50_us": pick(&|s| s.p50),
        "latency_p99_us": pick(&|s| s.p99),
        "syscalls_per_mib": syscalls(o)?,
        "machine": machine(),
    }))
}

fn run(o: &Opts) -> Result<Value, String> {
    let samples: Vec<Sample> = (0..o.runs).map(|_| one_run(o)).collect::<Result<_, _>>()?;
    report(o, &samples)
}

/// Runs `base` and `new` alternately (base, new, base, new, ...) so that
/// drift in the machine's load affects both the same way.
fn ab(base: &Opts, new: &Opts) -> Result<(Value, Value), String> {
    let (mut a, mut b) = (Vec::new(), Vec::new());
    // ABBA ABBA...: neither side always gets the first slot after an idle
    // pause.
    for i in 0..base.runs {
        if i % 2 == 0 {
            a.push(one_run(base)?);
            b.push(one_run(new)?);
        } else {
            b.push(one_run(new)?);
            a.push(one_run(base)?);
        }
    }
    Ok((report(base, &a)?, report(new, &b)?))
}

/// Metrics and whether a higher value is better.
const METRICS: [(&str, bool); 7] = [
    ("throughput_text_mib_s", true),
    ("throughput_bin_mib_s", true),
    ("cpu_ms_per_mib", false),
    ("peak_rss_kib", false),
    ("latency_p50_us", false),
    ("latency_p99_us", false),
    ("syscalls_per_mib", false),
];

/// Percent change of `new` against `base`, positive meaning worse.
fn regression(base: f64, new: f64, higher_is_better: bool) -> f64 {
    let change = (new - base) / base * 100.0;
    if higher_is_better { -change } else { change }
}

/// Prints a table and returns the metrics worse than `threshold` percent.
fn compare(base: &Value, new: &Value, threshold: f64) -> Vec<String> {
    let mut worse = Vec::new();
    println!(
        "{:<24} {:>12} {:>12} {:>9}",
        "metric", "base", "new", "worse %"
    );
    for (name, higher_is_better) in METRICS {
        match (base[name].as_f64(), new[name].as_f64()) {
            (Some(b), Some(n)) => {
                // A zero base cannot be a percentage base: equal is fine, any
                // move away from it counts as a change in that direction.
                let r = if b == 0.0 {
                    if n == 0.0 {
                        0.0
                    } else if (n > b) == higher_is_better {
                        -100.0
                    } else {
                        100.0
                    }
                } else {
                    regression(b, n, higher_is_better)
                };
                println!("{name:<24} {b:>12.2} {n:>12.2} {r:>+9.1}");
                if r > threshold {
                    worse.push(format!("{name} {r:+.1}%"));
                }
            }
            (None, None) => println!("{name:<24} {:>12} {:>12} {:>9}", "-", "-", "skipped"),
            (b, n) => {
                println!("{name:<24} {b:>12?} {n:>12?} {:>9}", "MISSING");
                worse.push(format!("{name} missing on one side"));
            }
        }
    }
    worse
}

fn parse_threshold(s: &str) -> Result<f64, String> {
    match s.parse::<f64>() {
        Ok(t) if t.is_finite() && t >= 0.0 => Ok(t),
        _ => Err(format!(
            "--threshold: expected a percentage >= 0, got '{s}'"
        )),
    }
}

/// Metrics every report has; the others may be null (binary throughput for
/// minicom, syscalls without --strace).
const REQUIRED: [&str; 5] = [
    "throughput_text_mib_s",
    "cpu_ms_per_mib",
    "peak_rss_kib",
    "latency_p50_us",
    "latency_p99_us",
];

fn check_report(v: &Value, what: &str) -> Result<(), String> {
    for key in REQUIRED {
        if !v[key].is_number() {
            return Err(format!(
                "{what}: not a benchmark report (no number for {key})"
            ));
        }
    }
    for (key, _) in METRICS {
        if v.get(key).is_none() {
            return Err(format!("{what}: not a benchmark report (no {key})"));
        }
    }
    Ok(())
}

fn usage() -> String {
    "usage: xcomm-bench run --tool PATH --kind xcomm|minicom|picocom|tio \
     [--size-mib N] [--runs N] [--samples N] [--strace]\n       \
     xcomm-bench compare --base FILE --new FILE [--threshold PERCENT]\n       \
     xcomm-bench ab --base PATH --new PATH --kind KIND [--size-mib N] [--runs N] \
     [--samples N] [--strace] [--threshold PERCENT] [--out DIR]"
        .into()
}

fn value(args: &mut std::slice::Iter<'_, String>, name: &str) -> Result<String, String> {
    args.next().cloned().ok_or(format!("{name} needs a value"))
}

fn number(s: &str, name: &str) -> Result<usize, String> {
    s.parse::<usize>()
        .ok()
        .filter(|n| *n > 0)
        .ok_or(format!("{name}: expected a positive integer, got '{s}'"))
}

fn main_inner() -> Result<ExitCode, String> {
    if !cfg!(target_os = "linux") {
        return Err(
            "xcomm-bench measures through Linux /proc and strace; it runs on Linux only".into(),
        );
    }
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut it = args.iter();
    match it.next().map(String::as_str) {
        Some("run") => {
            let mut o = Opts {
                tool: String::new(),
                kind: Kind::Xcomm,
                size_mib: 64,
                runs: 5,
                samples: 10_000,
                strace: false,
            };
            let mut kind = None;
            while let Some(a) = it.next() {
                match a.as_str() {
                    "--tool" => o.tool = value(&mut it, a)?,
                    "--kind" => kind = Some(Kind::parse(&value(&mut it, a)?)?),
                    "--size-mib" => o.size_mib = number(&value(&mut it, a)?, a)?,
                    "--runs" => o.runs = number(&value(&mut it, a)?, a)?,
                    "--samples" => o.samples = number(&value(&mut it, a)?, a)?,
                    "--strace" => o.strace = true,
                    _ => return Err(format!("unknown option '{a}'\n{}", usage())),
                }
            }
            if o.tool.is_empty() {
                return Err(format!("--tool is required\n{}", usage()));
            }
            o.kind = kind.ok_or(format!("--kind is required\n{}", usage()))?;
            let report = run(&o)?;
            println!(
                "{}",
                serde_json::to_string_pretty(&report).map_err(|e| e.to_string())?
            );
            Ok(ExitCode::SUCCESS)
        }
        Some("ab") => {
            let mut o = Opts {
                tool: String::new(),
                kind: Kind::Xcomm,
                size_mib: 64,
                runs: 9,
                samples: 10_000,
                strace: false,
            };
            let (mut base, mut new, mut threshold, mut out) = (None, None, 5.0, None);
            while let Some(a) = it.next() {
                match a.as_str() {
                    "--base" => base = Some(value(&mut it, a)?),
                    "--new" => new = Some(value(&mut it, a)?),
                    "--kind" => o.kind = Kind::parse(&value(&mut it, a)?)?,
                    "--size-mib" => o.size_mib = number(&value(&mut it, a)?, a)?,
                    "--runs" => o.runs = number(&value(&mut it, a)?, a)?,
                    "--samples" => o.samples = number(&value(&mut it, a)?, a)?,
                    "--strace" => o.strace = true,
                    "--threshold" => threshold = parse_threshold(&value(&mut it, a)?)?,
                    "--out" => out = Some(PathBuf::from(value(&mut it, a)?)),
                    _ => return Err(format!("unknown option '{a}'\n{}", usage())),
                }
            }
            let base_opts = Opts {
                tool: base.ok_or(format!("--base is required\n{}", usage()))?,
                ..o
            };
            let new_opts = Opts {
                tool: new.ok_or(format!("--new is required\n{}", usage()))?,
                ..o
            };
            // Before the runs: a bad --out must not cost a whole benchmark.
            if let Some(dir) = &out {
                std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
            }
            let (b, n) = ab(&base_opts, &new_opts)?;
            let worse = compare(&b, &n, threshold);
            if let Some(dir) = out {
                for (name, v) in [("base.json", &b), ("new.json", &n)] {
                    let path = dir.join(name);
                    let text = serde_json::to_string_pretty(v).map_err(|e| e.to_string())?;
                    std::fs::write(&path, text).map_err(|e| format!("{}: {e}", path.display()))?;
                }
            }
            if worse.is_empty() {
                Ok(ExitCode::SUCCESS)
            } else {
                eprintln!("xcomm-bench: worse than {threshold}%: {}", worse.join(", "));
                Ok(ExitCode::FAILURE)
            }
        }
        Some("compare") => {
            let (mut base, mut new, mut threshold) = (None, None, 5.0);
            while let Some(a) = it.next() {
                match a.as_str() {
                    "--base" => base = Some(value(&mut it, a)?),
                    "--new" => new = Some(value(&mut it, a)?),
                    "--threshold" => threshold = parse_threshold(&value(&mut it, a)?)?,
                    _ => return Err(format!("unknown option '{a}'\n{}", usage())),
                }
            }
            let load = |p: Option<String>, what: &str| -> Result<Value, String> {
                let p = p.ok_or(format!("{what} is required\n{}", usage()))?;
                let text = std::fs::read_to_string(&p).map_err(|e| format!("{p}: {e}"))?;
                serde_json::from_str(&text).map_err(|e| format!("{p}: {e}"))
            };
            let (b, n) = (load(base, "--base")?, load(new, "--new")?);
            check_report(&b, "--base")?;
            check_report(&n, "--new")?;
            let worse = compare(&b, &n, threshold);
            if worse.is_empty() {
                Ok(ExitCode::SUCCESS)
            } else {
                eprintln!("xcomm-bench: worse than {threshold}%: {}", worse.join(", "));
                Ok(ExitCode::FAILURE)
            }
        }
        _ => Err(usage()),
    }
}

fn main() -> ExitCode {
    match main_inner() {
        Ok(code) => code,
        Err(e) => {
            eprintln!("xcomm-bench: {e}");
            ExitCode::from(2)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report(text: f64, cpu: f64, p99: f64) -> Value {
        json!({
            "throughput_text_mib_s": text,
            "throughput_bin_mib_s": text,
            "cpu_ms_per_mib": cpu,
            "peak_rss_kib": 2000.0,
            "latency_p50_us": 50.0,
            "latency_p99_us": p99,
            "syscalls_per_mib": null,
        })
    }

    #[test]
    fn equal_reports_pass() {
        assert!(compare(&report(100.0, 10.0, 80.0), &report(100.0, 10.0, 80.0), 5.0).is_empty());
    }

    #[test]
    fn lower_throughput_beyond_threshold_fails() {
        let w = compare(&report(100.0, 10.0, 80.0), &report(94.0, 10.0, 80.0), 5.0);
        assert!(
            w.iter().any(|m| m.starts_with("throughput_text_mib_s")),
            "{w:?}"
        );
    }

    #[test]
    fn higher_latency_beyond_threshold_fails() {
        let w = compare(&report(100.0, 10.0, 80.0), &report(100.0, 10.0, 85.0), 5.0);
        assert_eq!(w.len(), 1, "{w:?}");
    }

    #[test]
    fn improvements_pass() {
        assert!(compare(&report(100.0, 10.0, 80.0), &report(120.0, 8.0, 60.0), 5.0).is_empty());
    }

    #[test]
    fn a_metric_on_one_side_only_fails() {
        let mut new = report(100.0, 10.0, 80.0);
        new["syscalls_per_mib"] = json!(1000.0);
        assert!(!compare(&report(100.0, 10.0, 80.0), &new, 5.0).is_empty());
    }

    #[test]
    fn a_zero_base_is_compared_not_called_missing() {
        let mut base = report(100.0, 10.0, 80.0);
        let mut new = report(100.0, 10.0, 80.0);
        base["peak_rss_kib"] = json!(0.0);
        new["peak_rss_kib"] = json!(0.0);
        assert!(compare(&base, &new, 5.0).is_empty());
        new["peak_rss_kib"] = json!(10.0);
        let w = compare(&base, &new, 5.0);
        assert!(w.iter().any(|m| m.starts_with("peak_rss_kib")), "{w:?}");
    }

    #[test]
    fn non_reports_and_bad_thresholds_are_rejected() {
        assert!(check_report(&json!({}), "x").is_err());
        assert!(check_report(&report(100.0, 10.0, 80.0), "x").is_ok());
        for bad in ["nan", "inf", "-1", "x"] {
            assert!(parse_threshold(bad).is_err(), "{bad}");
        }
        assert_eq!(parse_threshold("5"), Ok(5.0));
    }

    #[test]
    fn payloads_have_the_requested_size() {
        assert_eq!(binary_payload(1000).len(), 1000);
        let t = text_payload(1000);
        assert!(t.ends_with(b"XCOMM-BENCH-END\r\n"));
    }

    #[test]
    fn strace_summary_call_count() {
        let report = "% time     seconds  usecs/call     calls    errors syscall\n\
                      ------ ----------- ----------- --------- --------- ----------------\n\
                       60.00    0.000300           3       100           write\n\
                       40.00    0.000200           2       100         5 read\n\
                      ------ ----------- ----------- --------- --------- ----------------\n\
                      100.00    0.000500                   200         5 total\n";
        assert_eq!(calls_in_summary(report), Some(200.0));
        // Total line with usecs/call filled in and no errors column value.
        let report = "% time     seconds  usecs/call     calls    errors syscall\n\
                      ------ ----------- ----------- --------- --------- ------------------\n\
                      100.00    0.009832           3      3204           total\n";
        assert_eq!(calls_in_summary(report), Some(3204.0));
    }

    #[test]
    fn percentile_picks_from_sorted_samples() {
        let v: Vec<f64> = (1..=100).map(f64::from).collect();
        assert_eq!(percentile(&v, 0.5), 51.0);
        assert_eq!(percentile(&v, 0.99), 99.0);
    }
}
