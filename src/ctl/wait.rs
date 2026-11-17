//! `xcomm ctl wait` and `xcomm ctl run`, run by the xcomm-wait binary
//! that `xcomm` hands them to: the regex engine they need stays out of the
//! xcomm binary, so out of every session's memory.

use std::io::{BufRead, BufReader, Read};
use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};

use super::client::{
    self, Conn, Dirs, Failed, ending_bytes, print_device_output, print_json, print_out,
    send_after,
};
use super::code;
use super::matcher::{Matcher, Outcome, Which};
use super::proto::{self, Request};
use crate::cli::{CtlArgs, CtlCmd, WaitOpts};

/// Runs `args` (a wait or a run) and returns the exit status (spec 4.4).
pub fn run(args: &CtlArgs, env: &dyn Fn(&str) -> Option<String>) -> u8 {
    match run_inner(args, env) {
        Ok(status) => status,
        Err((status, msg)) => {
            crate::out::report(&msg);
            status
        }
    }
}

fn run_inner(args: &CtlArgs, env: &dyn Fn(&str) -> Option<String>) -> Result<u8, Failed> {
    // A bad pattern is found before anything is sent.
    let (opts, keep) = match &args.cmd {
        CtlCmd::Wait { opts, .. } => (opts, false),
        CtlCmd::Run { opts, .. } => (opts, true),
        _ => return Err((code::USAGE, "only ctl wait and ctl run come here".into())),
    };
    let mut matcher = matcher(opts, keep)?;
    let dirs = Dirs::find(env);
    let mut conn = Conn::open_for(&dirs, args)?;
    let status = conn.request(&Request::Status)?;
    let path = conn.path.clone();
    let still_there = || session_runs(&path, &dirs, &status["pid"]);
    match &args.cmd {
        CtlCmd::Wait { since, .. } => {
            let started = Instant::now();
            let reply = conn.request(&Request::Subscribe {
                since: *since,
                from_oldest: false,
            })?;
            let start = reply["start"]
                .as_u64()
                .ok_or((code::FAILED, "the session sent a bad reply".to_string()))?;
            let outcome =
                watch(&mut conn, &mut matcher, start, opts, None, &still_there)?;
            finish(args.json, args.quiet, &outcome, started, start, None)
        }
        CtlCmd::Run {
            text,
            ending,
            max_bytes,
            ..
        } => {
            let mut bytes = text.as_bytes().to_vec();
            bytes.extend_from_slice(ending_bytes(*ending));
            let started = Instant::now();
            let offset = send_after(&mut conn, &bytes, &status)?["offset"]
                .as_u64()
                .ok_or((code::FAILED, "the session sent a bad reply".to_string()))?;
            // From the offset of the send: what the device answers cannot
            // come before it, so none of it is missed.
            conn.request(&Request::Subscribe {
                since: Some(offset),
                from_oldest: false,
            })?;
            let limit = usize::try_from(*max_bytes).unwrap_or(usize::MAX);
            let outcome = watch(
                &mut conn,
                &mut matcher,
                offset,
                opts,
                Some(limit),
                &still_there,
            )?;
            let output = matcher.output(text.as_bytes());
            finish(
                args.json,
                args.quiet,
                &outcome,
                started,
                offset,
                Some(output),
            )
        }
        _ => unreachable!("checked above"),
    }
}

fn matcher(opts: &WaitOpts, keep: bool) -> Result<Matcher, Failed> {
    Matcher::new(
        opts.until.as_deref(),
        &opts.fail_on,
        opts.raw,
        keep,
        opts.unicode,
    )
    .map_err(|e| (code::USAGE, format!("invalid pattern: {e}")))
}

/// How long a match that reaches the end of the output so far waits for
/// more: the rest of a number, say, that the adapter sent a little later.
const SETTLE: Duration = Duration::from_millis(50);

/// Reads the device bytes a subscription carries, from raw offset `start`,
/// until a pattern matches or the wait ends. `limit` caps the text held;
/// `still_there` tells, at a close, whether the session goes on.
fn watch(
    conn: &mut Conn,
    matcher: &mut Matcher,
    start: u64,
    opts: &WaitOpts,
    limit: Option<usize>,
    still_there: &dyn Fn() -> bool,
) -> Result<Outcome, Failed> {
    let deadline = opts.timeout.and_then(|t| Instant::now().checked_add(t));
    let mut heard = Instant::now();
    let mut at = start;
    let mut buf = vec![0u8; 64 << 10];
    // Read, not yet given to the matcher: what may be the start of the
    // session's last words, cut short by the read.
    let mut held: Vec<u8> = Vec::new();
    loop {
        let now = Instant::now();
        let waiting = matcher.tentative() || !held.is_empty();
        let settle = waiting.then(|| heard + SETTLE);
        let quiet = opts.idle.and_then(|d| heard.checked_add(d));
        let due = |t: Option<Instant>| t.is_some_and(|t| t <= now);
        if due(settle) || (waiting && (due(quiet) || due(deadline))) {
            // Nothing came after it: it is the device's.
            if let Some(found) = matcher.feed(at, &held) {
                return Ok(found);
            }
            at += held.len() as u64;
            held.clear();
            if let Some(found) = matcher.settle() {
                return Ok(found);
            }
            continue;
        }
        if quiet.is_some_and(|q| q <= now && deadline.is_none_or(|d| q <= d)) {
            return Ok(Outcome::Idle { end: at });
        }
        if due(deadline) {
            return Ok(Outcome::Timeout { end: at });
        }
        let wake = [deadline, quiet, settle].into_iter().flatten().min();
        if !readable(&conn.reader, wake.map(|w| w - now))? {
            continue;
        }
        let n = match conn.reader.read(&mut buf) {
            Ok(0) => {
                if let Some(found) = matcher.feed(at, &held).or_else(|| matcher.settle())
                {
                    return Ok(found);
                }
                at += held.len() as u64;
                // Its last words did not fit, or the session ended.
                return Err(if still_there() {
                    (code::DROPPED, format!("subscriber too slow at offset {at}"))
                } else {
                    (
                        code::NO_SESSION,
                        format!("the session ended at offset {at}"),
                    )
                });
            }
            Ok(n) => n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => {
                return Err((code::FAILED, format!("reading from the session: {e}")));
            }
        };
        heard = Instant::now();
        held.extend_from_slice(&buf[..n]);
        // The session's last words to a subscriber that fell behind, or
        // device output that reads the same: only the first is followed by
        // the close.
        if let Some((len, dropped)) = proto::dropped_at(&held)
            && closed_next(&mut conn.reader)?
        {
            if let Some(found) =
                matcher.feed(at, &held[..len]).or_else(|| matcher.settle())
            {
                return Ok(found);
            }
            return Err((
                code::DROPPED,
                format!("subscriber too slow at offset {dropped}"),
            ));
        }
        let ready = held.len() - proto::dropped_start(&held);
        let found = matcher.feed(at, &held[..ready]);
        at += ready as u64;
        held.drain(..ready);
        if let Some(found) = found {
            return Ok(found);
        }
        if let Some(limit) = limit
            && matcher.held() > limit
        {
            return Err((
                code::USAGE,
                format!(
                    "more than {limit} bytes of output without a match (--max-bytes)"
                ),
            ));
        }
    }
}

/// Whether the session with process id `pid`, first found on `path`, goes
/// on: its socket still takes connections (even with a prompt open or all
/// client places taken), or it answers on another one, having renamed it
/// for a device that came back under another name. A session that ends
/// removes its socket.
fn session_runs(path: &std::path::Path, dirs: &Dirs, pid: &serde_json::Value) -> bool {
    UnixStream::connect(path).is_ok()
        || client::socket_files(&dirs.ok)
            .iter()
            .filter(|s| s.as_path() != path)
            .any(|s| client::status_of(s).is_some_and(|st| st["pid"] == *pid))
}

/// Whether there is something to read (or the end) within `wait`; None
/// waits as long as it takes. Not a socket timeout: macOS refuses to set
/// one once the peer has closed.
fn readable(
    reader: &BufReader<UnixStream>,
    wait: Option<Duration>,
) -> Result<bool, Failed> {
    use rustix::event::{PollFd, PollFlags, Timespec, poll};
    if !reader.buffer().is_empty() {
        return Ok(true);
    }
    let wait = wait.map(|w| {
        Timespec::try_from(w.clamp(Duration::from_millis(1), Duration::from_secs(86_400)))
            .expect("a day fits")
    });
    let mut fds = [PollFd::new(reader.get_ref(), PollFlags::IN)];
    match poll(&mut fds, wait.as_ref()) {
        Ok(n) => Ok(n > 0),
        Err(rustix::io::Errno::INTR) => Ok(false),
        Err(e) => Err((code::FAILED, format!("waiting on the session: {e}"))),
    }
}

/// Whether the session closes the connection now, with nothing after
/// what has been read.
fn closed_next(reader: &mut BufReader<UnixStream>) -> Result<bool, Failed> {
    if !reader.buffer().is_empty() {
        return Ok(false);
    }
    // It closes right after its last words.
    if !readable(reader, Some(Duration::from_secs(1)))? {
        return Ok(false);
    }
    Ok(matches!(reader.fill_buf(), Ok([])))
}

/// Prints how a wait ended and returns its exit status.
fn finish(
    json: bool,
    quiet: bool,
    outcome: &Outcome,
    started: Instant,
    offset: u64,
    output: Option<&[u8]>,
) -> Result<u8, Failed> {
    let elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    let (which, status, end) = match outcome {
        Outcome::Matched {
            which: Which::Until,
            end,
            ..
        } => ("until", code::OK, *end),
        Outcome::Matched {
            which: Which::FailOn(_),
            end,
            ..
        } => ("fail_on", code::FAIL_ON, *end),
        Outcome::Idle { end } => ("idle", code::OK, *end),
        Outcome::Timeout { end } => ("timeout", code::FAILED, *end),
    };
    if json {
        let mut v = serde_json::json!({
            "which": which,
            "pattern": null,
            "start": null,
            "end": end,
            "text": null,
            "groups": [],
            "offset": offset,
            "elapsed_ms": elapsed_ms,
        });
        if let Outcome::Matched {
            pattern,
            start,
            text,
            groups,
            ..
        } = outcome
        {
            v["pattern"] = pattern.as_str().into();
            v["start"] = (*start).into();
            v["text"] = text.as_str().into();
            v["groups"] = serde_json::json!(groups);
        }
        if let Some(output) = output {
            v["output"] = String::from_utf8_lossy(output).into();
        }
        print_json(&v)?;
    } else {
        match (output, outcome) {
            (Some(output), _) => {
                // The offset line follows, or a message saying why it ended.
                let message = matches!(
                    outcome,
                    Outcome::Timeout { .. }
                        | Outcome::Matched {
                            which: Which::FailOn(_),
                            ..
                        }
                );
                print_device_output(output, !quiet || message)?
            }
            (None, Outcome::Matched { text, .. }) => {
                print_out(format!("{text}\n").as_bytes())?
            }
            (None, _) => {}
        }
    }
    match outcome {
        Outcome::Matched {
            which: Which::FailOn(_),
            pattern,
            text,
            ..
        } => crate::out::report(&format!("--fail-on '{pattern}' matched: {text}")),
        Outcome::Timeout { .. } => crate::out::report(&format!(
            "timed out after {:.1} s",
            started.elapsed().as_secs_f64()
        )),
        _ => {}
    }
    if !json && !quiet {
        eprintln!("end={end}");
    }
    Ok(status)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::path::Path;

    fn opts() -> WaitOpts {
        WaitOpts {
            until: Some("never".into()),
            fail_on: vec!["code".into()],
            timeout: Some(Duration::from_secs(5)),
            idle: None,
            raw: false,
            unicode: false,
        }
    }

    /// `watch` over a connection whose other end runs `peer`.
    fn watch_peer(
        peer: impl FnOnce(UnixStream) + Send + 'static,
    ) -> Result<Outcome, Failed> {
        watch_peer_with(opts(), peer)
    }

    fn watch_peer_with(
        o: WaitOpts,
        peer: impl FnOnce(UnixStream) + Send + 'static,
    ) -> Result<Outcome, Failed> {
        let (ours, theirs) = UnixStream::pair().unwrap();
        let t = std::thread::spawn(move || peer(theirs));
        // No socket there: a close the session's last words do not explain
        // reads as the session gone.
        let mut conn = Conn::new(Path::new("/nonexistent/x.sock"), ours, None).unwrap();
        let mut m =
            Matcher::new(o.until.as_deref(), &o.fail_on, false, false, false).unwrap();
        let got = watch(&mut conn, &mut m, 0, &o, None, &|| false);
        t.join().unwrap();
        got
    }

    /// The groups of a match on `ip (\d+)` when "ip 12" comes, then "34"
    /// after `gap`.
    fn ip_after(gap: Duration) -> Vec<Option<String>> {
        let o = WaitOpts {
            until: Some(r"ip (\d+)".into()),
            fail_on: vec![],
            ..opts()
        };
        let got = watch_peer_with(o, move |mut s| {
            s.write_all(b"ip 12").unwrap();
            std::thread::sleep(gap);
            s.write_all(b"34\r\n").unwrap();
            std::thread::sleep(Duration::from_millis(100));
        });
        let Ok(Outcome::Matched { groups, .. }) = got else {
            panic!("{got:?}");
        };
        groups
    }

    #[test]
    fn a_match_at_the_end_waits_settle_for_the_rest() {
        assert_eq!(ip_after(Duration::from_millis(5)), [Some("1234".into())]);
    }

    #[test]
    fn a_match_at_the_end_is_taken_once_settle_has_passed() {
        assert_eq!(ip_after(Duration::from_millis(200)), [Some("12".into())]);
    }

    #[test]
    fn last_words_cut_by_a_read_are_not_device_output() {
        let line = proto::dropped_line(7);
        let got = watch_peer(move |mut s| {
            let (a, b) = line.as_bytes().split_at(20);
            s.write_all(&[b"abcdefg", a].concat()).unwrap();
            std::thread::sleep(Duration::from_millis(5));
            s.write_all(b).unwrap();
        });
        assert_eq!(
            got,
            Err((code::DROPPED, "subscriber too slow at offset 7".into()))
        );
    }

    #[test]
    fn a_held_start_with_nothing_after_it_is_device_output() {
        let line = proto::dropped_line(7);
        let got = watch_peer(move |mut s| {
            s.write_all(&line.as_bytes()[..20]).unwrap();
            // Longer than SETTLE: the read has long since ended.
            std::thread::sleep(Duration::from_millis(300));
            s.write_all(b"never").unwrap();
            std::thread::sleep(Duration::from_millis(200));
        });
        let Ok(Outcome::Matched { which, .. }) = got else {
            panic!("{got:?}");
        };
        assert_eq!(which, Which::FailOn(0));
    }

    #[test]
    fn the_same_words_from_the_device_are_device_output() {
        let line = proto::dropped_line(7);
        let code_at = line.find("code").unwrap() as u64;
        let got = watch_peer(move |mut s| {
            s.write_all(line.as_bytes()).unwrap();
            std::thread::sleep(Duration::from_millis(100));
            s.write_all(b"never").unwrap();
            std::thread::sleep(Duration::from_millis(200));
        });
        let Ok(Outcome::Matched { which, start, .. }) = got else {
            panic!("{got:?}");
        };
        assert_eq!(which, Which::FailOn(0));
        assert_eq!(start, code_at);
    }
}
