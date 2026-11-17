// SPDX-FileCopyrightText: 2026 Su Kang Yin <paradyse@gmail.com>
// SPDX-License-Identifier: GPL-2.0-only

//! `xcomm list`: the serial ports there are, with their USB identity, who
//! holds their lock and the herdr pane of a session on them. Nothing is
//! opened but sysfs files, lock files and session sockets.

use std::io::{self, BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Serialize;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PortInfo {
    /// The name in /dev (`ttyUSB0`, `cu.usbserial-1410`).
    pub tty: String,
    pub path: String,
    /// The /dev/serial/by-id link to it.
    pub by_id: Option<String>,
    /// `0403:6001`, from sysfs.
    pub vid_pid: Option<String>,
    pub product: Option<String>,
    pub serial: Option<String>,
    /// The live process holding its UUCP lock.
    pub lock_pid: Option<i32>,
    /// Why the lock could not be read, when it could not.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lock_error: Option<String>,
    /// The herdr pane of the xcomm session on it.
    pub herdr_pane: Option<String>,
}

/// How long a session gets to say which herdr pane it is in.
const STATUS_TIMEOUT: Duration = Duration::from_millis(100);
/// USB attributes are on an ancestor of the tty's device directory: the
/// interface's parent for most adapters.
const USB_DEPTH: usize = 4;

/// The ports on this machine.
pub fn enumerate(env: &dyn Fn(&str) -> Option<String>) -> io::Result<Vec<PortInfo>> {
    enumerate_in(Path::new("/"), env)
}

/// The ports under `root` (a fake tree in tests): `dev/ttyUSB*`,
/// `dev/ttyACM*` and `dev/cu.*`, and what `dev/serial/by-id` links to.
pub fn enumerate_in(
    root: &Path,
    env: &dyn Fn(&str) -> Option<String>,
) -> io::Result<Vec<PortInfo>> {
    let dev = root.join("dev");
    let mut ttys: Vec<String> = Vec::new();
    for entry in std::fs::read_dir(&dev)? {
        let name = entry?.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if ["ttyUSB", "ttyACM", "cu."]
            .iter()
            .any(|p| name.starts_with(p) && name.len() > p.len())
        {
            ttys.push(name.to_string());
        }
    }
    let by_id = by_id_links(&dev)?;
    for tty in by_id.iter().map(|(_, tty)| tty) {
        if !ttys.contains(tty) && dev.join(tty).exists() {
            ttys.push(tty.clone());
        }
    }
    ttys.sort_by(|a, b| natural_cmp(a, b));
    let lock_dirs = crate::lock::lock_dirs_to_read(env);
    let socket_dirs: Vec<PathBuf> = crate::ctl::path::socket_dirs(env)
        .into_iter()
        .filter(|d| crate::ctl::path::check_dir(d).is_ok())
        .collect();
    Ok(ttys
        .into_iter()
        .map(|tty| {
            let usb = usb_attrs(root, &tty);
            let (lock_pid, lock_error) = lock_holder(&lock_dirs, &tty);
            PortInfo {
                path: format!("/dev/{tty}"),
                by_id: by_id
                    .iter()
                    .find(|(_, t)| *t == tty)
                    .map(|(link, _)| format!("/dev/serial/by-id/{link}")),
                vid_pid: usb.vid_pid,
                product: usb.product,
                serial: usb.serial,
                lock_pid,
                lock_error,
                herdr_pane: herdr_pane(&socket_dirs, &tty),
                tty,
            }
        })
        .collect())
}

/// The live holder of `tty`'s lock in any of `dirs`, or why that is not
/// known.
fn lock_holder(dirs: &[PathBuf], tty: &str) -> (Option<i32>, Option<String>) {
    if dirs.is_empty() {
        return (None, Some("no lock directory".into()));
    }
    let mut error = None;
    for dir in dirs {
        match crate::lock::holder(dir, tty) {
            Ok(Some(pid)) => return (Some(pid), None),
            Ok(None) => {}
            Err(e) => {
                error.get_or_insert(e.to_string());
            }
        }
    }
    (None, error)
}

/// The /dev/serial/by-id link to `tty`, if there is one.
pub fn by_id_of(tty: &str) -> Option<String> {
    by_id_links(Path::new("/dev"))
        .ok()?
        .into_iter()
        .find(|(_, t)| t == tty)
        .map(|(link, _)| format!("/dev/serial/by-id/{link}"))
}

/// (link name, tty) for each link in `dev/serial/by-id` that leads to a
/// name directly in `dev`, sorted by link name.
fn by_id_links(dev: &Path) -> io::Result<Vec<(String, String)>> {
    let dir = dev.join("serial/by-id");
    let entries = match std::fs::read_dir(&dir) {
        Ok(entries) => entries,
        // No USB serial device plugged in: udev has not made it.
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    let mut links = Vec::new();
    for entry in entries {
        let entry = entry?;
        let (Some(link), Ok(target)) = (
            entry.file_name().to_str().map(str::to_string),
            std::fs::read_link(entry.path()),
        ) else {
            continue;
        };
        // udev makes them relative: ../../ttyUSB0.
        let Some(tty) = target.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        let resolved = dir.join(&target);
        if resolved.parent().and_then(|p| p.canonicalize().ok())
            == dev.canonicalize().ok()
        {
            links.push((link, tty.to_string()));
        }
    }
    links.sort();
    Ok(links)
}

#[derive(Default)]
struct Usb {
    vid_pid: Option<String>,
    product: Option<String>,
    serial: Option<String>,
}

/// The USB device's attributes, from the first of the tty device's
/// ancestors that has `idVendor`.
fn usb_attrs(root: &Path, tty: &str) -> Usb {
    let Ok(mut dir) = root
        .join("sys/class/tty")
        .join(tty)
        .join("device")
        .canonicalize()
    else {
        return Usb::default();
    };
    for _ in 0..=USB_DEPTH {
        if dir.join("idVendor").is_file() {
            let read = |name: &str| {
                std::fs::read_to_string(dir.join(name))
                    .ok()
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
            };
            return Usb {
                vid_pid: read("idVendor")
                    .zip(read("idProduct"))
                    .map(|(v, p)| format!("{v}:{p}")),
                product: read("product"),
                serial: read("serial"),
            };
        }
        if !dir.pop() {
            break;
        }
    }
    Usb::default()
}

/// The herdr pane of the session on `tty`, asked over its socket. A
/// session waiting for its device to come back keeps the socket of the
/// name it had, which may be another device's now: only one connected to
/// `tty` counts.
fn herdr_pane(dirs: &[PathBuf], tty: &str) -> Option<String> {
    session_pane(dirs, tty, true)
}

/// The herdr pane of the session that has `tty`'s socket; with
/// `connected`, only one connected to it now (one waiting for its device
/// keeps the socket of the name it had, which may be another device's).
pub fn session_pane(dirs: &[PathBuf], tty: &str, connected: bool) -> Option<String> {
    dirs.iter().find_map(|dir| {
        let sock = crate::ctl::path::socket_path(dir, tty);
        let stream = crate::sock::connect_now(&sock).ok()?;
        stream.set_read_timeout(Some(STATUS_TIMEOUT)).ok()?;
        stream.set_write_timeout(Some(STATUS_TIMEOUT)).ok()?;
        (&stream).write_all(b"{\"cmd\":\"status\"}\n").ok()?;
        let mut line = Vec::new();
        BufReader::new(&stream).read_until(b'\n', &mut line).ok()?;
        let v: serde_json::Value = serde_json::from_slice(&line).ok()?;
        if (connected && v["connected"] != true) || v["tty"] != tty {
            return None;
        }
        v["herdr_pane"].as_str().map(str::to_string)
    })
}

/// `s` with control characters (from a device's own descriptors) as `?`.
fn printable(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_control() { '?' } else { c })
        .collect()
}

/// Text order with digit runs compared as numbers: ttyUSB2 before ttyUSB10.
pub fn natural_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    let (mut a, mut b) = (a.as_bytes(), b.as_bytes());
    loop {
        match (a.first(), b.first()) {
            (None, None) => return std::cmp::Ordering::Equal,
            (None, _) => return std::cmp::Ordering::Less,
            (_, None) => return std::cmp::Ordering::Greater,
            (Some(x), Some(y)) if x.is_ascii_digit() && y.is_ascii_digit() => {
                let da = a.iter().take_while(|c| c.is_ascii_digit()).count();
                let db = b.iter().take_while(|c| c.is_ascii_digit()).count();
                let (na, nb) = (&a[..da], &b[..db]);
                // Without leading zeros, a longer run is a bigger number.
                let strip =
                    |n: &[u8]| -> usize { n.iter().take_while(|&&c| c == b'0').count() };
                let (ta, tb) = (&na[strip(na)..], &nb[strip(nb)..]);
                let ord = ta.len().cmp(&tb.len()).then(ta.cmp(tb)).then(da.cmp(&db));
                if ord.is_ne() {
                    return ord;
                }
                a = &a[da..];
                b = &b[db..];
            }
            (Some(x), Some(y)) => {
                if x != y {
                    return x.cmp(y);
                }
                a = &a[1..];
                b = &b[1..];
            }
        }
    }
}

/// The ports as `xcomm list` prints them: aligned columns, `-` for none.
pub fn table(ports: &[PortInfo]) -> String {
    if ports.is_empty() {
        return String::new();
    }
    let head = ["TTY", "VID:PID", "PRODUCT", "LOCK", "HERDR", "BY-ID"].map(String::from);
    let rows: Vec<[String; 6]> = std::iter::once(head)
        .chain(ports.iter().map(|p| {
            let or =
                |v: &Option<String>| v.as_deref().map_or_else(|| "-".into(), printable);
            [
                p.tty.clone(),
                or(&p.vid_pid),
                or(&p.product),
                match (p.lock_pid, &p.lock_error) {
                    (Some(pid), _) => format!("locked by {pid}"),
                    (None, Some(e)) => format!("lock unreadable: {e}"),
                    (None, None) => "free".into(),
                },
                p.herdr_pane
                    .as_ref()
                    .map_or_else(|| "-".into(), |pane| format!("pane {pane}")),
                or(&p.by_id),
            ]
        }))
        .collect();
    let mut width = [0usize; 6];
    for row in &rows {
        for (w, cell) in width.iter_mut().zip(row) {
            *w = (*w).max(cell.chars().count());
        }
    }
    let mut out = String::new();
    for row in &rows {
        let mut line = String::new();
        for (i, cell) in row.iter().enumerate() {
            if i + 1 == row.len() {
                line.push_str(cell);
            } else {
                line.push_str(&format!("{cell:<w$}  ", w = width[i]));
            }
        }
        out.push_str(line.trim_end());
        out.push('\n');
    }
    out
}
