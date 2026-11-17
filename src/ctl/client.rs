//! `xcomm ctl`: one request to a running session, or a few for a large
//! send, and the reply printed.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::Duration;

use super::proto::{self, Request};
use super::{code, path};
use crate::cli::{ColorMode, CtlArgs, CtlCmd, LineEnding, SendData};

/// How long a reply may take. A session busy in a prompt the user has
/// open does not answer until it is closed. A send's reply comes once the
/// device has taken all but the session's queue of it, so a send gets the
/// time its bytes take at the line's rate on top (twice that, for slack).
const REPLY_TIMEOUT: Duration = Duration::from_secs(10);
/// Raw bytes per `send` request: base64 of this stays under the line limit.
const SEND_CHUNK: usize = 512 << 10;

/// Runs `args` and returns the exit status (spec 4.4). Messages go to
/// stderr here; the caller only exits.
pub fn run(args: &CtlArgs, env: &dyn Fn(&str) -> Option<String>) -> u8 {
    match run_inner(args, env) {
        Ok(status) => status,
        Err((status, msg)) => {
            crate::out::report(&msg);
            status
        }
    }
}

pub(super) type Failed = (u8, String);

fn run_inner(args: &CtlArgs, env: &dyn Fn(&str) -> Option<String>) -> Result<u8, Failed> {
    let dirs = Dirs::find(env);
    match &args.cmd {
        CtlCmd::Sessions => return sessions(&dirs, args.json),
        CtlCmd::PaneFromContext => {
            let pane = context_pane(env).ok_or((
                code::FAILED,
                "no pane in HERDR_PLUGIN_CONTEXT_JSON (run from a herdr plugin action)"
                    .to_string(),
            ))?;
            print_out(format!("{pane}\n").as_bytes())?;
            return Ok(code::OK);
        }
        _ => {}
    }
    let mut conn = Conn::open_for(&dirs, args)?;
    match &args.cmd {
        CtlCmd::Sessions | CtlCmd::PaneFromContext => unreachable!("handled above"),
        CtlCmd::Status { field } => {
            let reply = conn.request(&Request::Status)?;
            if args.json {
                print_json(&reply)?;
            } else if let Some(key) = field {
                print_out(format!("{}\n", status_value(&reply, key)).as_bytes())?;
            } else {
                print_status(&reply)?;
            }
        }
        CtlCmd::Send { data, ending } => {
            let mut bytes = match data {
                SendData::Text(t) => t.as_bytes().to_vec(),
                SendData::Hex(b) => b.clone(),
                SendData::File(p) => std::fs::read(p).map_err(|e| {
                    (code::FAILED, format!("--file {}: {e}", p.display()))
                })?,
            };
            bytes.extend_from_slice(ending_bytes(*ending));
            let reply = send(&mut conn, &bytes)?;
            if args.json {
                print_json(&reply)?;
            } else {
                print_out(format!("{}\n", reply["offset"]).as_bytes())?;
            }
        }
        CtlCmd::Wait { .. } | CtlCmd::Run { .. } => {
            return Err((
                code::USAGE,
                format!("ctl wait and ctl run are {}'s", super::WAIT_HELPER),
            ));
        }
        CtlCmd::Read {
            since,
            last,
            max_bytes,
            from_oldest,
            color,
        } => {
            let reply = conn.request(&Request::Read {
                since: *since,
                last: *last,
                max_bytes: *max_bytes,
                from_oldest: *from_oldest,
            })?;
            if args.json {
                print_json(&reply)?;
            } else {
                let data = reply["data_b64"]
                    .as_str()
                    .and_then(proto::base64_decode)
                    .ok_or((code::FAILED, "the session sent a bad reply".to_string()))?;
                let keep = match color {
                    ColorMode::Always => true,
                    ColorMode::Never => false,
                    ColorMode::Auto => rustix::termios::isatty(std::io::stdout()),
                };
                if keep {
                    print_out(&data)?;
                } else {
                    let mut plain = Vec::with_capacity(data.len());
                    crate::ansi::EscapeStripper::new().strip(&data, &mut plain);
                    print_out(&plain)?;
                }
                eprintln!("next={}", reply["next"]);
            }
        }
        CtlCmd::Break { ms } => {
            conn.timeout = action_timeout(&mut conn, *ms)?;
            let reply = conn.request(&Request::Break { ms: *ms })?;
            if args.json {
                print_json(&reply)?;
            }
        }
        CtlCmd::Modem { line, op, ms } => {
            let hold = if *op == proto::ModemOp::Pulse { *ms } else { 0 };
            conn.timeout = action_timeout(&mut conn, hold)?;
            let reply = conn.request(&Request::Modem {
                line: *line,
                op: *op,
                ms: *ms,
            })?;
            if args.json {
                print_json(&reply)?;
            } else {
                let on = if reply["on"] == true { "on" } else { "off" };
                print_out(format!("{} {on}\n", line.name()).as_bytes())?;
            }
        }
        CtlCmd::Set { patch } => {
            conn.timeout = action_timeout(&mut conn, 0)?;
            let reply = conn.request(&Request::Set {
                patch: patch.clone(),
            })?;
            if args.json {
                print_json(&reply)?;
            } else {
                if reply["exact"] != true {
                    crate::out::report("the driver did not apply exactly what was asked");
                }
                print_out(format!("{}\n", settings_text(&reply["settings"])).as_bytes())?;
            }
        }
        CtlCmd::CaptureStart(file) => {
            let path = absolute(file)?;
            conn.timeout = action_timeout(&mut conn, 0)?;
            let reply = conn.request(&Request::CaptureStart {
                path: path_text(&path)?,
            })?;
            if args.json {
                print_json(&reply)?;
            } else {
                print_out(format!("{}\n", path.display()).as_bytes())?;
            }
        }
        CtlCmd::CaptureStop => {
            conn.timeout = action_timeout(&mut conn, 0)?;
            let reply = conn.request(&Request::CaptureStop)?;
            if args.json {
                print_json(&reply)?;
            } else {
                print_out(
                    format!(
                        "{} ({} bytes)\n",
                        reply["path"].as_str().unwrap_or("?"),
                        reply["bytes"]
                    )
                    .as_bytes(),
                )?;
            }
        }
        CtlCmd::Xfer { dir, proto, files } => {
            let cwd = std::env::current_dir()
                .map_err(|e| (code::FAILED, format!("current directory: {e}")))?;
            let files = files
                .iter()
                .map(|f| absolute(f).and_then(|p| path_text(&p)))
                .collect::<Result<Vec<_>, _>>()?;
            // As long as it takes; Ctrl-C here closes the connection, which
            // aborts it.
            conn.timeout = None;
            let reply = conn.request(&Request::Xfer {
                dir: *dir,
                proto: *proto,
                files,
                cwd: path_text(&cwd)?,
            })?;
            if args.json {
                print_json(&reply)?;
            } else {
                print_out(
                    format!("{}\n", reply["result"].as_str().unwrap_or("done"))
                        .as_bytes(),
                )?;
            }
        }
    }
    Ok(code::OK)
}

pub(super) fn ending_bytes(ending: LineEnding) -> &'static [u8] {
    match ending {
        LineEnding::None => b"",
        LineEnding::Cr => b"\r",
        LineEnding::Lf => b"\n",
        LineEnding::CrLf => b"\r\n",
    }
}

/// Sends `bytes`, in as many requests as it takes, and returns the first
/// reply: its offset is the one at send time.
pub(super) fn send(conn: &mut Conn, bytes: &[u8]) -> Result<serde_json::Value, Failed> {
    // Even a short send waits while other clients fill the queue. A status
    // that fails ends it here: a late reply left on the connection would
    // be read as the send's.
    let status = conn.request(&Request::Status)?;
    send_after(conn, bytes, &status)
}

/// `send`, with a status reply already had.
pub(super) fn send_after(
    conn: &mut Conn,
    bytes: &[u8],
    status: &serde_json::Value,
) -> Result<serde_json::Value, Failed> {
    let mut first = None;
    // In order on one connection: the session serves a client's requests
    // one after another.
    let chunks: Vec<&[u8]> = if bytes.is_empty() {
        vec![&[]]
    } else {
        bytes.chunks(SEND_CHUNK).collect()
    };
    let rate = bits_per_second(status);
    for chunk in chunks {
        let wire = rate.map_or(Duration::ZERO, |(bits, per_sec)| {
            Duration::from_secs_f64(2.0 * (chunk.len() as f64) * bits / per_sec)
        });
        conn.timeout = Some(REPLY_TIMEOUT + wire);
        let reply = conn
            .request(&Request::Send {
                data_b64: proto::base64_encode(chunk),
            })
            .map_err(|(status, msg)| match first {
                // Closing cancels the send: what the session had not yet
                // taken goes nowhere.
                Some(_) => (status, format!("{msg}; the rest was not sent")),
                None => (status, msg),
            })?;
        first.get_or_insert(reply);
    }
    conn.timeout = Some(REPLY_TIMEOUT);
    Ok(first.expect("at least one request"))
}

/// How long an action's reply may take: it waits for what the session
/// has queued for the device (its queue at the line's rate, twice for the
/// driver's buffer and slack, or STALL if the line takes nothing), then
/// holds the line for `hold_ms`.
fn action_timeout(conn: &mut Conn, hold_ms: u32) -> Result<Option<Duration>, Failed> {
    let queued = bits_per_second(&conn.request(&Request::Status)?).map_or(
        Duration::ZERO,
        |(bits, per_sec)| {
            Duration::from_secs_f64(
                2.0 * crate::session::QUEUE_SIZE as f64 * bits / per_sec,
            )
        },
    );
    Ok(Some(
        REPLY_TIMEOUT + queued + Duration::from_millis(hold_ms.into()),
    ))
}

/// The pane a herdr plugin action was invoked on.
fn context_pane(env: &dyn Fn(&str) -> Option<String>) -> Option<String> {
    let json = env("HERDR_PLUGIN_CONTEXT_JSON")?;
    let v: serde_json::Value = serde_json::from_str(&json).ok()?;
    v["focused_pane_id"]
        .as_str()
        .filter(|p| !p.is_empty())
        .map(str::to_string)
}

/// `file` from this client's directory: the session's is another.
fn absolute(file: &Path) -> Result<std::path::PathBuf, Failed> {
    std::path::absolute(file)
        .map_err(|e| (code::FAILED, format!("{}: {e}", file.display())))
}

/// JSON carries text: a name that is not UTF-8 cannot be sent.
fn path_text(path: &Path) -> Result<String, Failed> {
    path.to_str()
        .map(str::to_string)
        .ok_or_else(|| (code::USAGE, format!("{}: not valid UTF-8", path.display())))
}

/// Bits a character takes on the line, and the line's bits per second,
/// from a status reply; None while disconnected.
fn bits_per_second(status: &serde_json::Value) -> Option<(f64, f64)> {
    let s = &status["settings"];
    let baud = s["baud"].as_f64().filter(|&b| b > 0.0)?;
    let parity = if s["parity"] == "none" { 0.0 } else { 1.0 };
    let bits = 1.0 + s["data_bits"].as_f64()? + parity + s["stop_bits"].as_f64()?;
    Some((bits, baud))
}

/// The socket directories a client may trust, and why any other was not.
pub(super) struct Dirs {
    pub(super) ok: Vec<std::path::PathBuf>,
    skipped: Vec<String>,
}

impl Dirs {
    pub(super) fn find(env: &dyn Fn(&str) -> Option<String>) -> Dirs {
        let mut dirs = Dirs {
            ok: Vec::new(),
            skipped: Vec::new(),
        };
        for dir in path::socket_dirs(env) {
            if std::fs::symlink_metadata(&dir).is_err() {
                continue;
            }
            // In /tmp anyone could have made it, with a socket of theirs
            // inside to hear what is sent.
            match path::check_dir(&dir) {
                Ok(()) => dirs.ok.push(dir),
                Err(e) => dirs.skipped.push(e.0),
            }
        }
        dirs
    }

    /// "no session for X", with any directory passed over.
    fn none(&self, device: &str) -> Failed {
        let mut msg = format!("no session for {device}");
        for why in &self.skipped {
            msg.push_str(&format!(" (not looked in: {why})"));
        }
        (code::NO_SESSION, msg)
    }
}

pub(super) struct Conn {
    pub(super) path: std::path::PathBuf,
    pub(super) reader: BufReader<UnixStream>,
    writer: UnixStream,
    /// For the next reply; None waits as long as it takes.
    timeout: Option<Duration>,
}

impl Conn {
    pub(super) fn open(dirs: &Dirs, device: &str) -> Result<Conn, Failed> {
        let mut socks: Vec<std::path::PathBuf> = match path::tty_name(device) {
            Some(tty) => dirs
                .ok
                .iter()
                .map(|dir| path::socket_path(dir, &tty))
                .collect(),
            None => Vec::new(),
        };
        // A link that went with its device (udev removes by-id links on an
        // unplug) names nothing now; the session waiting on it still knows
        // the name it was started with. A name that leads to a device is
        // that device's, whichever session once had it.
        if std::fs::metadata(crate::session::device_path(device)).is_err() {
            socks.extend(find_by_name(&dirs.ok, device));
        }
        let mut failed = None;
        for sock in &socks {
            match UnixStream::connect(sock) {
                Ok(stream) => {
                    return Conn::new(sock, stream, Some(REPLY_TIMEOUT))
                        .map_err(|e| (code::FAILED, e.to_string()));
                }
                // None there, or one a killed session left: look on.
                Err(e)
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::NotFound
                            | std::io::ErrorKind::ConnectionRefused
                    ) => {}
                Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
                    failed.get_or_insert((
                        code::DENIED,
                        format!("{}: {e}", sock.display()),
                    ));
                }
                Err(e) => {
                    failed.get_or_insert((
                        code::FAILED,
                        format!("{}: {e}", sock.display()),
                    ));
                }
            }
        }
        Err(failed.unwrap_or_else(|| dirs.none(device)))
    }

    /// The session `args` names: by device, or by herdr pane.
    pub(super) fn open_for(dirs: &Dirs, args: &CtlArgs) -> Result<Conn, Failed> {
        match (&args.device, &args.pane) {
            (Some(device), None) => Conn::open(dirs, device),
            (None, Some(pane)) => Conn::open_pane(dirs, pane),
            _ => Err((code::USAGE, "a device or --pane is needed".into())),
        }
    }

    /// The session whose status names herdr pane `pane`. Two in one pane
    /// (a second started there by hand) is an error: which one an action
    /// reached would depend on how their sockets sort.
    fn open_pane(dirs: &Dirs, pane: &str) -> Result<Conn, Failed> {
        let found: Vec<(std::path::PathBuf, String)> = socket_files(&dirs.ok)
            .into_iter()
            .filter_map(|sock| {
                let s = status_of(&sock)?;
                (s["herdr_pane"] == pane)
                    .then(|| (sock, s["device"].as_str().unwrap_or("?").to_string()))
            })
            .collect();
        if found.len() > 1 {
            let devices: Vec<&str> = found.iter().map(|(_, d)| d.as_str()).collect();
            return Err((
                code::FAILED,
                format!(
                    "{} xcomm sessions in pane {pane} ({}): name the device instead",
                    found.len(),
                    devices.join(", ")
                ),
            ));
        }
        if let Some((sock, _)) = found.first()
            && let Ok(stream) = UnixStream::connect(sock)
        {
            return Conn::new(sock, stream, Some(REPLY_TIMEOUT))
                .map_err(|e| (code::FAILED, e.to_string()));
        }
        let mut msg = format!("no xcomm session in pane {pane}");
        for why in &dirs.skipped {
            msg.push_str(&format!(" (not looked in: {why})"));
        }
        Err((code::NO_SESSION, msg))
    }

    pub(super) fn new(
        path: &Path,
        stream: UnixStream,
        timeout: Option<Duration>,
    ) -> std::io::Result<Conn> {
        let writer = stream.try_clone()?;
        Ok(Conn {
            path: path.to_path_buf(),
            reader: BufReader::new(stream),
            writer,
            timeout,
        })
    }

    /// Sends `request` and returns the reply's fields; a failed reply is
    /// an error with its code.
    pub(super) fn request(
        &mut self,
        request: &Request,
    ) -> Result<serde_json::Value, Failed> {
        // The session does not run it once this client has given up.
        let envelope = proto::Envelope {
            request: request.clone(),
            deadline_ms: self.timeout.map(|t| {
                proto::now_ms()
                    .saturating_add(u64::try_from(t.as_millis()).unwrap_or(u64::MAX))
            }),
        };
        let mut line = serde_json::to_vec(&envelope).expect("a request serializes");
        line.push(b'\n');
        self.writer
            .write_all(&line)
            .map_err(|e| (code::FAILED, format!("sending to the session: {e}")))?;
        self.reader
            .get_ref()
            .set_read_timeout(self.timeout)
            .map_err(|e| (code::FAILED, e.to_string()))?;
        read_reply(&mut self.reader, self.timeout)
    }
}

/// The socket of the session started with `device` (or reopening the
/// device by that name), asking each session in `dir`.
fn find_by_name(dirs: &[std::path::PathBuf], device: &str) -> Option<std::path::PathBuf> {
    let want = crate::session::device_path(device).display().to_string();
    socket_files(dirs).into_iter().find(|sock| {
        status_of(sock)
            .is_some_and(|s| s["device"] == want.as_str() || s["stable"] == want.as_str())
    })
}

/// The `*.sock` files in `dirs`, each directory's sorted.
pub(super) fn socket_files(dirs: &[std::path::PathBuf]) -> Vec<std::path::PathBuf> {
    let mut all = Vec::new();
    for dir in dirs {
        let Ok(entries) = std::fs::read_dir(dir) else {
            continue;
        };
        let mut socks: Vec<_> = entries
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.extension().is_some_and(|x| x == "sock"))
            .collect();
        socks.sort();
        all.extend(socks);
    }
    all
}

/// What the session on `sock` says of itself; None when nothing answers
/// there (gone, refusing, not answering).
pub(super) fn status_of(sock: &Path) -> Option<serde_json::Value> {
    let stream = UnixStream::connect(sock).ok()?;
    let mut conn = Conn::new(sock, stream, Some(Duration::from_secs(2))).ok()?;
    conn.request(&Request::Status).ok()
}

fn read_reply(
    reader: &mut impl BufRead,
    timeout: Option<Duration>,
) -> Result<serde_json::Value, Failed> {
    let mut line = Vec::new();
    match reader.read_until(b'\n', &mut line) {
        Ok(0) => {
            return Err((
                code::FAILED,
                "the session closed the connection without a reply".into(),
            ));
        }
        Ok(_) => {}
        Err(e)
            if matches!(
                e.kind(),
                std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
            ) =>
        {
            return Err((
                code::FAILED,
                format!(
                    "no reply from the session in {} s",
                    timeout.unwrap_or_default().as_secs()
                ),
            ));
        }
        Err(e) => return Err((code::FAILED, format!("reading the reply: {e}"))),
    }
    let reply: serde_json::Value = serde_json::from_slice(&line)
        .map_err(|e| (code::FAILED, format!("the session sent a bad reply: {e}")))?;
    if reply["ok"] == true {
        return Ok(reply);
    }
    let status = reply["code"]
        .as_u64()
        .and_then(|c| u8::try_from(c).ok())
        .filter(|&c| c != 0)
        .unwrap_or(code::FAILED);
    let msg = reply["error"].as_str().unwrap_or("the request failed");
    Err((status, msg.to_string()))
}

pub(super) fn print_out(bytes: &[u8]) -> Result<(), Failed> {
    crate::out::stdout(bytes)
        .map_err(|e| (code::FAILED, format!("cannot write to stdout: {e}")))
}

pub(super) fn print_json(v: &serde_json::Value) -> Result<(), Failed> {
    let mut line = serde_json::to_vec(v).expect("a JSON value serializes");
    line.push(b'\n');
    print_out(&line)
}

/// The fields of a status reply as `ctl status` prints them: the JSON key,
/// which `ctl status <field>` takes, and the row's label.
pub const STATUS_ROWS: [(&str, &str); 13] = [
    ("device", "device"),
    ("tty", "tty"),
    ("stable", "stable"),
    ("connected", "connected"),
    ("settings", "settings"),
    ("modem", "modem"),
    ("offset", "offset"),
    ("oldest", "oldest"),
    ("capture", "capture"),
    ("transfer", "transfer"),
    ("ctl", "ctl"),
    ("herdr_pane", "herdr pane"),
    ("pid", "pid"),
];

fn status_value(v: &serde_json::Value, key: &str) -> String {
    match (key, &v[key]) {
        ("settings", s) => settings_text(s),
        ("modem", serde_json::Value::Object(m)) => {
            ["dtr", "rts", "cts", "dsr", "dcd", "ri"]
                .iter()
                .map(|k| {
                    format!("{}={}", k.to_uppercase(), if m[*k] == true { 1 } else { 0 })
                })
                .collect::<Vec<_>>()
                .join(" ")
        }
        (_, serde_json::Value::String(s)) => s.clone(),
        (_, serde_json::Value::Null) => "-".into(),
        (_, other) => other.to_string(),
    }
}

fn print_status(v: &serde_json::Value) -> Result<(), Failed> {
    let out: String = STATUS_ROWS
        .iter()
        .map(|(key, label)| format!("{label:<10} {}\n", status_value(v, key)))
        .collect();
    print_out(out.as_bytes())
}

/// `115200 8N1 flow none`, or `-` for none.
fn settings_text(s: &serde_json::Value) -> String {
    match s {
        serde_json::Value::Object(s) => format!(
            "{} {}{}{} flow {}",
            s["baud"],
            s["data_bits"],
            s["parity"].as_str().map_or("?", |p| &p[..1]).to_uppercase(),
            s["stop_bits"],
            s["flow"].as_str().unwrap_or("?")
        ),
        _ => "-".into(),
    }
}

/// Every session that answers on a socket in `dirs`.
fn sessions(dirs: &Dirs, json: bool) -> Result<u8, Failed> {
    for why in &dirs.skipped {
        crate::out::report(&format!("not looked in: {why}"));
    }
    let mut found: Vec<serde_json::Value> = Vec::new();
    for sock in socket_files(&dirs.ok) {
        if let Some(mut status) = status_of(&sock) {
            if let serde_json::Value::Object(map) = &mut status {
                map.remove("ok");
                map.remove("code");
            }
            found.push(status);
        }
    }
    if json {
        print_json(&serde_json::Value::Array(found))?;
        return Ok(code::OK);
    }
    let mut out = String::new();
    for s in &found {
        let state = if s["connected"] == true {
            "connected"
        } else {
            "disconnected"
        };
        out.push_str(&format!(
            "{}\t{}\tpid {}\t{state}\toffset {}\n",
            s["tty"].as_str().unwrap_or("?"),
            s["device"].as_str().unwrap_or("?"),
            s["pid"],
            s["offset"],
        ));
    }
    print_out(out.as_bytes())?;
    Ok(code::OK)
}
