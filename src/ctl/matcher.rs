// SPDX-FileCopyrightText: 2026 Su Kang Yin <paradyse@gmail.com>
// SPDX-License-Identifier: GPL-2.0-only

//! Pattern matching for `ctl wait` and `ctl run`, done in the client:
//! device output, without its escape sequences unless raw, searched as it
//! comes in.

use regex::bytes::{Regex, RegexBuilder};

use crate::ansi::EscapeStripper;

/// Longest match supported: each search starts this far before the new
/// text.
pub const WINDOW: usize = 64 << 10;
/// Bytes kept before a search's start, so `\b` and `^` see what precedes
/// it (a UTF-8 character is at most 4 bytes).
const CONTEXT: usize = 4;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Which {
    Until,
    FailOn(usize),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// `start` and `end` are raw offsets: `end` is just past the match's
    /// last byte.
    Matched {
        which: Which,
        pattern: String,
        start: u64,
        end: u64,
        text: String,
        groups: Vec<Option<String>>,
    },
    Idle {
        end: u64,
    },
    Timeout {
        end: u64,
    },
    /// The device went away at `end` with no match before it (a `run`).
    Gone {
        end: u64,
    },
}

pub struct Matcher {
    until: Option<Regex>,
    fail: Vec<Regex>,
    /// None when matching raw bytes.
    stripper: Option<EscapeStripper>,
    text: Vec<u8>,
    /// Text bytes dropped from the front of `text`.
    base: u64,
    /// (text position, raw offset) where each run of contiguous raw bytes
    /// starts in the text; positions count dropped bytes too.
    runs: Vec<(u64, u64)>,
    raw_end: u64,
    /// Keep all text (for `run`'s output) rather than the last WINDOW.
    keep: bool,
    /// Where in `text` the match starts, once there is one.
    found: Option<usize>,
    /// Where the last feed's search started.
    from: usize,
    /// The earliest match reaches the end of the text so far: more text
    /// may make it longer (`\d+`) or undo it (`$`, `\b`).
    tentative: bool,
}

impl Matcher {
    /// `unicode` makes `\w`, `\d`, `\s` and `(?i)` Unicode-aware; that
    /// needs the `unicode` feature. Without it they are ASCII, and UTF-8
    /// literals still match.
    pub fn new(
        until: Option<&str>,
        fail_on: &[String],
        raw: bool,
        keep: bool,
        unicode: bool,
    ) -> Result<Matcher, regex::Error> {
        let compile = |p: &str| RegexBuilder::new(p).unicode(unicode).build();
        Ok(Matcher {
            until: until.map(compile).transpose()?,
            fail: fail_on
                .iter()
                .map(|p| compile(p))
                .collect::<Result<_, _>>()?,
            stripper: (!raw).then(EscapeStripper::new),
            text: Vec::new(),
            base: 0,
            runs: Vec::new(),
            raw_end: 0,
            keep,
            found: None,
            from: 0,
            tentative: false,
        })
    }

    /// Takes the device bytes from raw offset `raw_start` on, and returns
    /// the earliest match of any pattern; a fail-on pattern wins a tie. A
    /// match that reaches the end of the text is held back (`tentative`).
    pub fn feed(&mut self, raw_start: u64, bytes: &[u8]) -> Option<Outcome> {
        if self.found.is_some() {
            return None;
        }
        let old = self.text.len();
        self.raw_end = raw_start + bytes.len() as u64;
        let (runs, base) = (&mut self.runs, self.base);
        let mut kept = |at: usize, pos: usize, len: usize| {
            if len == 0 {
                return;
            }
            let (pos, raw) = (base + pos as u64, raw_start + at as u64);
            if runs.last().is_none_or(|&(p, r)| r + (pos - p) != raw) {
                runs.push((pos, raw));
            }
        };
        match &mut self.stripper {
            Some(s) => s.strip_mapped(bytes, &mut self.text, kept),
            None => {
                kept(0, old, bytes.len());
                self.text.extend_from_slice(bytes);
            }
        }
        if self.text.len() == old {
            return None;
        }
        self.from = old.saturating_sub(WINDOW);
        self.search(false)
    }

    /// Whether a match is held back, waiting for what comes next.
    pub fn tentative(&self) -> bool {
        self.tentative
    }

    /// Nothing more has come: the match held back stands.
    pub fn settle(&mut self) -> Option<Outcome> {
        if !self.tentative {
            return None;
        }
        self.search(true)
    }

    fn search(&mut self, at_end: bool) -> Option<Outcome> {
        let from = self.from;
        let mut best: Option<(usize, usize, Which)> = None;
        let candidates = self
            .fail
            .iter()
            .enumerate()
            .map(|(i, re)| (re, Which::FailOn(i)))
            .chain(self.until.iter().map(|re| (re, Which::Until)));
        for (re, which) in candidates {
            if let Some(m) = re.find_at(&self.text, from)
                && best.is_none_or(|(start, _, _)| m.start() < start)
            {
                best = Some((m.start(), m.end(), which));
            }
        }
        let Some((start, end, which)) = best else {
            self.tentative = false;
            self.trim();
            return None;
        };
        self.tentative = end == self.text.len() && !at_end;
        if self.tentative {
            return None;
        }
        self.found = Some(start);
        let re = match which {
            Which::Until => self.until.as_ref().expect("until matched"),
            Which::FailOn(i) => &self.fail[i],
        };
        let lossy = |b: &[u8]| String::from_utf8_lossy(b).into_owned();
        let groups = re
            .captures_at(&self.text, from)
            .map(|c| {
                (1..c.len())
                    .map(|g| c.get(g).map(|m| lossy(m.as_bytes())))
                    .collect()
            })
            .unwrap_or_default();
        let raw_start = self.raw_of(start);
        Some(Outcome::Matched {
            which,
            pattern: re.as_str().to_string(),
            start: raw_start,
            end: if end > start {
                self.raw_of(end - 1) + 1
            } else {
                raw_start
            },
            text: lossy(&self.text[start..end]),
            groups,
        })
    }

    /// The raw offset just past every byte fed.
    pub fn end(&self) -> u64 {
        self.raw_end
    }

    /// Text bytes held.
    pub fn held(&self) -> usize {
        self.text.len()
    }

    /// `run`'s output: the text up to the match (all of it without one),
    /// less its first line if that is `sent` echoed back.
    pub fn output(&self, sent: &[u8]) -> &[u8] {
        let text = &self.text[..self.found.unwrap_or(self.text.len())];
        match text.iter().position(|&b| b == b'\r' || b == b'\n') {
            Some(p) if text[..p] == *sent => {
                let next = if text[p..].starts_with(b"\r\n") {
                    p + 2
                } else {
                    p + 1
                };
                &text[next..]
            }
            _ => text,
        }
    }

    /// The raw offset of the text byte at `at` in `text`.
    fn raw_of(&self, at: usize) -> u64 {
        let pos = self.base + at as u64;
        let run = self.runs.partition_point(|&(p, _)| p <= pos);
        let (p, r) = self.runs[run.saturating_sub(1)];
        r + (pos - p)
    }

    /// Drops text no later search can reach, now and then rather than at
    /// every feed.
    fn trim(&mut self) {
        if self.keep || self.text.len() <= 4 * WINDOW {
            return;
        }
        let cut = self.text.len() - WINDOW - CONTEXT;
        self.text.drain(..cut);
        self.base += cut as u64;
        let first = self.runs.partition_point(|&(p, _)| p <= self.base);
        self.runs.drain(..first.saturating_sub(1));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn matched(o: Option<Outcome>) -> (Which, u64, u64, String, Vec<Option<String>>) {
        match o {
            Some(Outcome::Matched {
                which,
                start,
                end,
                text,
                groups,
                ..
            }) => (which, start, end, text, groups),
            other => panic!("no match: {other:?}"),
        }
    }

    #[test]
    fn a_match_at_the_end_waits_for_more() {
        let mut m =
            Matcher::new(Some(r"inet (\d+\.\d+)"), &[], false, false, false).unwrap();
        assert_eq!(m.feed(0, b"inet 10.1"), None);
        assert!(m.tentative());
        let (_, _, end, text, groups) = matched(m.feed(9, b"2/24"));
        assert_eq!((end, text.as_str()), (10, "inet 10.12"));
        assert_eq!(groups, vec![Some("10.12".to_string())]);
        // `$` holds only while nothing follows.
        let mut m = Matcher::new(Some("# $"), &[], false, false, false).unwrap();
        assert_eq!(m.feed(0, b"# "), None);
        assert_eq!(m.feed(2, b"x"), None);
        assert!(!m.tentative());
        assert_eq!(m.settle(), None);
        assert_eq!(m.feed(3, b"\r\n# "), None);
        let (_, start, end, ..) = matched(m.settle());
        assert_eq!((start, end), (5, 7));
    }

    #[test]
    fn classes_are_ascii_unless_unicode() {
        let mut m = Matcher::new(Some(r"錯誤 (\d+)"), &[], false, false, false).unwrap();
        let (_, _, _, text, _) = matched(m.feed(0, "x 錯誤 42 ".as_bytes()));
        assert_eq!(text, "錯誤 42");
        let mut m = Matcher::new(Some(r"(?i)login"), &[], false, false, false).unwrap();
        matched(m.feed(0, b"LOGIN: "));
        let mut m = Matcher::new(Some(r"\w+"), &[], false, false, false).unwrap();
        assert_eq!(m.feed(0, "日本 ".as_bytes()), None);
        let unicode = Matcher::new(Some(r"\w+"), &[], false, false, true);
        if cfg!(feature = "unicode") {
            let (_, _, _, text, _) =
                matched(unicode.unwrap().feed(0, "日本 ".as_bytes()));
            assert_eq!(text, "日本");
        } else {
            assert!(unicode.is_err());
        }
    }

    #[test]
    fn a_tie_goes_to_fail_on() {
        let mut m =
            Matcher::new(Some("err"), &["e".into(), "er".into()], false, false, false)
                .unwrap();
        let (which, start, end, text, _) = matched(m.feed(0, b"xx error"));
        assert_eq!(which, Which::FailOn(0));
        assert_eq!((start, end, text.as_str()), (3, 4, "e"));
    }

    #[test]
    fn the_earliest_match_wins() {
        let mut m =
            Matcher::new(Some("ok"), &["fail".into()], false, false, false).unwrap();
        let (which, ..) = matched(m.feed(0, b"ok then fail"));
        assert_eq!(which, Which::Until);
    }

    #[test]
    fn groups_are_returned() {
        let mut m =
            Matcher::new(Some(r"ip (\d+)\.(\d+)(x)?"), &[], false, false, false).unwrap();
        let (_, _, _, text, groups) = matched(m.feed(10, b"my ip 10.2 here"));
        assert_eq!(text, "ip 10.2");
        assert_eq!(
            groups,
            vec![Some("10".to_string()), Some("2".to_string()), None]
        );
    }

    #[test]
    fn raw_offsets_map_back_through_stripped_sequences() {
        let mut m = Matcher::new(Some("login:"), &[], false, false, false).unwrap();
        // "ab" ESC[32m "log" | "in" ESC[0m ":"
        assert_eq!(m.feed(100, b"ab\x1b[32mlog"), None);
        assert_eq!(m.feed(110, b"in\x1b[0m:"), None);
        let (_, start, end, text, _) = matched(m.settle());
        assert_eq!(text, "login:");
        assert_eq!(start, 107);
        assert_eq!(end, 117);
        assert_eq!(m.end(), 117);
    }

    #[test]
    fn raw_does_not_strip() {
        let mut m = Matcher::new(Some("login:"), &[], true, false, false).unwrap();
        assert_eq!(m.feed(0, b"\x1b[32mlogin\x1b[0m:"), None);
        let mut m = Matcher::new(Some("32mlogin"), &[], true, false, false).unwrap();
        let (_, start, end, ..) = matched(m.feed(5, b"\x1b[32mlogin\x1b[0m:"));
        assert_eq!((start, end), (7, 15));
    }

    #[test]
    fn a_match_split_across_feeds() {
        let mut m = Matcher::new(Some("login:"), &[], false, false, false).unwrap();
        assert_eq!(m.feed(0, b"log"), None);
        let (_, start, end, ..) = matched(m.feed(3, b"in: "));
        assert_eq!((start, end), (0, 6));
    }

    #[test]
    fn carets_and_boundaries_see_trimmed_text_as_before() {
        let mut m = Matcher::new(Some(r"^x|\bword"), &[], false, false, false).unwrap();
        let filler = vec![b'a'; WINDOW];
        let mut at = 0;
        for _ in 0..6 {
            assert_eq!(m.feed(at, &filler), None);
            at += WINDOW as u64;
        }
        assert!(m.held() <= 3 * WINDOW, "{}", m.held());
        // "aword": no boundary before "word", and "x" is not at the start.
        assert_eq!(m.feed(at, b"x"), None);
        assert_eq!(m.feed(at + 1, b"aword"), None);
        assert_eq!(m.feed(at + 6, b" word"), None);
        let (_, start, ..) = matched(m.settle());
        assert_eq!(start, at + 7);
    }

    #[test]
    fn output_drops_the_echoed_line() {
        let mut m = Matcher::new(Some("# $"), &[], false, true, false).unwrap();
        assert_eq!(m.feed(0, b"uname -a\r\nLinux x 6.1\r\n# "), None);
        matched(m.settle());
        assert_eq!(m.output(b"uname -a"), b"Linux x 6.1\r\n");
        assert_eq!(m.output(b"other"), b"uname -a\r\nLinux x 6.1\r\n");
        let mut m = Matcher::new(None, &[], false, true, false).unwrap();
        assert_eq!(m.feed(0, b"ls\nfile\n"), None);
        assert_eq!(m.output(b"ls"), b"file\n");
    }
}
