// SPDX-FileCopyrightText: 2026 Su Kang Yin <paradyse@gmail.com>
// SPDX-License-Identifier: GPL-2.0-only

//! Output helpers that return write errors instead of panicking.
//!
//! `println!` and `eprintln!` panic when the write fails (a closed pipe), and
//! `panic = "abort"` turns that panic into SIGABRT.

use std::io::{self, Write};

pub fn stdout(bytes: &[u8]) -> io::Result<()> {
    let mut out = io::stdout().lock();
    out.write_all(bytes)?;
    out.flush()
}

/// Prints `xcomm: <msg>` to stderr in one write, so a line is not split by
/// another writer on the same terminal. A failure is dropped: with stderr
/// gone there is nowhere left to report it.
pub fn report(msg: &str) {
    let _ = io::stderr().write_all(format!("xcomm: {msg}\n").as_bytes());
}
