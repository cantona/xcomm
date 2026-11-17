// SPDX-FileCopyrightText: 2026 Su Kang Yin <paradyse@gmail.com>
// SPDX-License-Identifier: GPL-2.0-only

//! The README and the man page keep up with the binary: every long option
//! `xcomm --help` names, and every exit code of `xcomm ctl`.

use std::collections::BTreeSet;
use std::process::Command;

fn read(name: &str) -> String {
    std::fs::read_to_string(format!("{}/{name}", env!("CARGO_MANIFEST_DIR"))).unwrap()
}

/// The man page as text: roff's `\-` is a `-`, font changes go.
fn man_text() -> String {
    let t = read("xcomm.1").replace("\\-", "-");
    ["\\fB", "\\fI", "\\fR", "\\fP", "\\(em", "\\e"]
        .iter()
        .fold(t, |t, esc| t.replace(esc, ""))
}

fn long_options() -> BTreeSet<String> {
    let o = Command::new(env!("CARGO_BIN_EXE_xcomm"))
        .arg("--help")
        .env_remove("XCOMM_PREFIX")
        .output()
        .unwrap();
    let help = String::from_utf8(o.stdout).unwrap();
    let re = regex::Regex::new(r"--[a-z][a-z-]*[a-z]").unwrap();
    let found: BTreeSet<String> = re
        .find_iter(&help)
        .map(|m| m.as_str().to_string())
        .collect();
    // The help names them all; a parser that found few would pass anything.
    assert!(found.len() >= 30, "{found:?}");
    found
}

#[test]
fn every_long_option_is_documented() {
    let (readme, man) = (read("README.md"), man_text());
    for opt in long_options() {
        // As a whole option: --prefix is not found inside --prefix-time,
        // nor --color inside --colors.
        let word =
            regex::Regex::new(&format!(r"{}(?:[^A-Za-z0-9-]|$)", regex::escape(&opt)))
                .unwrap();
        assert!(word.is_match(&readme), "README.md lacks {opt}");
        assert!(word.is_match(&man), "xcomm.1 lacks {opt}");
    }
}

/// The exit codes `xcomm ctl` has: every `pub const` in src/ctl/mod.rs's
/// `code` module, so a new one has to be documented too.
fn ctl_codes() -> BTreeSet<u8> {
    let src = read("src/ctl/mod.rs");
    let module = src
        .split_once("pub mod code {")
        .expect("src/ctl/mod.rs has the code module")
        .1
        .split_once("\n}")
        .unwrap()
        .0;
    let re = regex::Regex::new(r"pub const [A-Z_]+: u8 = ([0-9]+);").unwrap();
    let codes: BTreeSet<u8> = re
        .captures_iter(module)
        .map(|c| c[1].parse().unwrap())
        .collect();
    assert!(codes.len() >= 10, "{codes:?}");
    codes
}

#[test]
fn every_ctl_exit_code_is_documented() {
    let codes = ctl_codes();
    let readme = read("README.md");
    let table: BTreeSet<u8> = readme
        .split_once("## Exit codes")
        .expect("README.md has an Exit codes section")
        .1
        .lines()
        .take_while(|l| !l.starts_with("## "))
        .filter_map(|l| {
            l.strip_prefix("| ")?
                .split_once(" |")?
                .0
                .trim()
                .parse()
                .ok()
        })
        .collect();
    let man = man_text();
    let section = man
        .split_once(".SH EXIT STATUS")
        .expect("xcomm.1 has EXIT STATUS")
        .1
        .split(".SH ")
        .next()
        .unwrap();
    let listed: BTreeSet<u8> = section
        .lines()
        .filter_map(|l| l.strip_prefix(".B ")?.trim().parse().ok())
        .collect();
    for &c in &codes {
        assert!(table.contains(&c), "README.md's exit codes lack {c}");
        assert!(listed.contains(&c), "xcomm.1's EXIT STATUS lacks {c}");
    }
    assert_eq!(table.len(), codes.len(), "README.md: {table:?}");
    assert_eq!(listed.len(), codes.len(), "xcomm.1: {listed:?}");
}
