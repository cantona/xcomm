// SPDX-FileCopyrightText: 2026 Su Kang Yin <paradyse@gmail.com>
// SPDX-License-Identifier: GPL-2.0-only

use std::time::{Duration, Instant};

use xcomm::cli::{Modifier, PrefixKey};
use xcomm::keys::{KeyAction, KeyParser, NO_KEY};

#[derive(Debug, PartialEq)]
enum A {
    F(Vec<u8>),
    C(u8),
}

fn collect(v: &mut Vec<A>) -> impl FnMut(KeyAction<'_>) + '_ {
    move |a| match a {
        KeyAction::Forward(b) => match v.last_mut() {
            Some(A::F(prev)) => prev.extend_from_slice(b),
            _ => v.push(A::F(b.to_vec())),
        },
        KeyAction::Command(c) => v.push(A::C(c)),
    }
}

/// Feeds all of `input`, as the session does: again after each command.
fn run(p: &mut KeyParser, input: &[u8], t: Instant) -> Vec<A> {
    let mut v = Vec::new();
    let mut rest = input;
    loop {
        let used = p.feed(rest, t, &mut collect(&mut v));
        rest = &rest[used..];
        if rest.is_empty() {
            return v;
        }
    }
}

fn flush(p: &mut KeyParser, t: Instant) -> Vec<A> {
    let mut v = Vec::new();
    p.on_deadline(t, &mut collect(&mut v));
    v
}

const CTRL_A: PrefixKey = PrefixKey {
    modifier: Modifier::Ctrl,
    letter: b'a',
};
const ALT_A: PrefixKey = PrefixKey {
    modifier: Modifier::Alt,
    letter: b'a',
};

fn ms(n: u64) -> Duration {
    Duration::from_millis(n)
}

fn parser(k: PrefixKey) -> KeyParser {
    KeyParser::new(k, ms(25))
}

#[test]
fn plain_text_is_forwarded_in_one_piece() {
    let mut p = parser(CTRL_A);
    let t = Instant::now();
    assert_eq!(run(&mut p, b"ls -l\r", t), vec![A::F(b"ls -l\r".to_vec())]);
    assert_eq!(p.deadline(), None);
}

#[test]
fn ctrl_a_then_a_letter_is_a_command() {
    let mut p = parser(CTRL_A);
    let t = Instant::now();
    assert_eq!(
        run(&mut p, b"ab\x01xcd", t),
        vec![A::F(b"ab".to_vec()), A::C(b'x'), A::F(b"cd".to_vec())]
    );
}

#[test]
fn feed_stops_after_a_command() {
    let mut p = parser(CTRL_A);
    let t = Instant::now();
    let mut v = Vec::new();
    assert_eq!(p.feed(b"ab\x01xcd", t, &mut collect(&mut v)), 4);
    assert_eq!(v, vec![A::F(b"ab".to_vec()), A::C(b'x')]);
}

#[test]
fn ctrl_a_split_across_reads() {
    let mut p = parser(CTRL_A);
    let t = Instant::now();
    assert_eq!(run(&mut p, b"\x01", t), vec![]);
    assert_eq!(run(&mut p, b"z", t), vec![A::C(b'z')]);
}

#[test]
fn ctrl_a_twice_sends_one_ctrl_a() {
    let mut p = parser(CTRL_A);
    let t = Instant::now();
    assert_eq!(run(&mut p, b"\x01\x01", t), vec![A::F(vec![0x01])]);
}

#[test]
fn command_letters_are_lowercased() {
    let mut p = parser(CTRL_A);
    let t = Instant::now();
    assert_eq!(run(&mut p, b"\x01X", t), vec![A::C(b'x')]);
}

#[test]
fn other_ctrl_prefixes_work() {
    let mut p = parser(PrefixKey {
        modifier: Modifier::Ctrl,
        letter: b't',
    });
    let t = Instant::now();
    assert_eq!(
        run(&mut p, b"\x01\x14q", t),
        vec![A::F(vec![0x01]), A::C(b'q')]
    );
}

#[test]
fn alt_a_in_one_read_is_the_prefix() {
    let mut p = parser(ALT_A);
    let t = Instant::now();
    assert_eq!(run(&mut p, b"\x1baq", t), vec![A::C(b'q')]);
}

#[test]
fn alt_a_in_csi_u_form_is_the_prefix() {
    let mut p = parser(ALT_A);
    let t = Instant::now();
    assert_eq!(run(&mut p, b"\x1b[97;3uz", t), vec![A::C(b'z')]);
}

#[test]
fn alt_shift_a_is_not_the_prefix() {
    // ESC A is an application-mode arrow key on some terminals.
    let mut p = parser(ALT_A);
    let t = Instant::now();
    assert_eq!(run(&mut p, b"\x1bA", t), vec![A::F(b"\x1bA".to_vec())]);
}

#[test]
fn arrow_keys_pass_through_with_an_alt_prefix() {
    let mut p = parser(ALT_A);
    let t = Instant::now();
    assert_eq!(
        run(&mut p, b"\x1b[A\x1bOB", t),
        vec![A::F(b"\x1b[A\x1bOB".to_vec())]
    );
}

#[test]
fn a_lone_esc_is_held_then_forwarded_at_the_deadline() {
    let mut p = parser(ALT_A);
    let t = Instant::now();
    assert_eq!(run(&mut p, b"x\x1b", t), vec![A::F(b"x".to_vec())]);
    assert_eq!(p.deadline(), Some(t + ms(25)));
    assert_eq!(flush(&mut p, t + ms(10)), vec![], "not yet due");
    assert_eq!(flush(&mut p, t + ms(26)), vec![A::F(vec![0x1b])]);
    assert_eq!(p.deadline(), None);
}

#[test]
fn esc_then_the_letter_in_the_next_read_before_the_deadline_is_the_prefix() {
    let mut p = parser(ALT_A);
    let t = Instant::now();
    run(&mut p, b"\x1b", t);
    assert_eq!(run(&mut p, b"ax", t + ms(5)), vec![A::C(b'x')]);
}

#[test]
fn a_partial_csi_u_is_held_and_completed_by_the_next_read() {
    let mut p = parser(ALT_A);
    let t = Instant::now();
    assert_eq!(run(&mut p, b"\x1b[97", t), vec![]);
    assert_eq!(run(&mut p, b";3uq", t + ms(2)), vec![A::C(b'q')]);
}

#[test]
fn a_held_partial_sequence_that_turns_out_different_is_forwarded_whole() {
    let mut p = parser(ALT_A);
    let t = Instant::now();
    assert_eq!(run(&mut p, b"\x1b[9", t), vec![]);
    assert_eq!(
        run(&mut p, b"8;3u", t + ms(2)),
        vec![A::F(b"\x1b[98;3u".to_vec())]
    );
}

#[test]
fn alt_a_twice_sends_esc_a() {
    let mut p = parser(ALT_A);
    let t = Instant::now();
    assert_eq!(run(&mut p, b"\x1ba\x1ba", t), vec![A::F(b"\x1ba".to_vec())]);
}

#[test]
fn after_alt_a_an_alt_letter_counts_as_that_letter() {
    let mut p = parser(ALT_A);
    let t = Instant::now();
    assert_eq!(run(&mut p, b"\x1ba\x1bx", t), vec![A::C(b'x')]);
}

#[test]
fn after_the_prefix_an_arrow_key_is_one_key_with_no_byte() {
    let t = Instant::now();
    let mut p = parser(CTRL_A);
    assert_eq!(
        run(&mut p, b"\x01\x1b[Aend", t),
        vec![A::C(NO_KEY), A::F(b"end".to_vec())]
    );
    assert_eq!(
        run(&mut p, b"\x01\x1bOPend", t),
        vec![A::C(NO_KEY), A::F(b"end".to_vec())]
    );
    let mut p = parser(ALT_A);
    assert_eq!(
        run(&mut p, b"\x1ba\x1b[1;5Dend", t),
        vec![A::C(NO_KEY), A::F(b"end".to_vec())]
    );
}

#[test]
fn after_alt_a_a_csi_u_alt_letter_counts_as_that_letter() {
    let mut p = parser(ALT_A);
    let t = Instant::now();
    assert_eq!(run(&mut p, b"\x1ba\x1b[122;3u", t), vec![A::C(b'z')]);
}

#[test]
fn a_key_after_the_prefix_cut_across_reads_is_kept_whole() {
    let mut p = parser(ALT_A);
    let t = Instant::now();
    assert_eq!(run(&mut p, b"\x1ba", t), vec![]);
    assert_eq!(run(&mut p, b"\x1b[9", t), vec![]);
    assert_eq!(
        run(&mut p, b"8;3uend", t),
        vec![A::C(b'b'), A::F(b"end".to_vec())]
    );
}

#[test]
fn the_prefix_twice_in_csi_u_form_sends_the_prefix() {
    let t = Instant::now();
    let mut p = parser(CTRL_A);
    assert_eq!(run(&mut p, b"\x01\x1b[97;5u", t), vec![A::F(vec![0x01])]);
    let mut p = parser(ALT_A);
    assert_eq!(
        run(&mut p, b"\x1ba\x1b[97;3u", t),
        vec![A::F(b"\x1ba".to_vec())]
    );
}

#[test]
fn esc_after_ctrl_a_cancels_at_once() {
    let mut p = parser(CTRL_A);
    let t = Instant::now();
    assert_eq!(run(&mut p, b"\x01\x1b", t), vec![A::C(0x1b)]);
    assert_eq!(p.deadline(), None);
    assert_eq!(run(&mut p, b"x", t), vec![A::F(b"x".to_vec())]);
    // A second ESC is a key of its own.
    assert_eq!(
        run(&mut p, b"\x01\x1b\x1b", t),
        vec![A::C(0x1b), A::F(vec![0x1b])]
    );
}

#[test]
fn esc_after_alt_a_is_held_then_cancels() {
    let mut p = parser(ALT_A);
    let t = Instant::now();
    assert_eq!(run(&mut p, b"\x1ba\x1b", t), vec![]);
    assert_eq!(flush(&mut p, t + ms(26)), vec![A::C(0x1b)]);
    assert_eq!(run(&mut p, b"x", t), vec![A::F(b"x".to_vec())]);
}

#[test]
fn with_a_ctrl_prefix_esc_and_a_byte_are_two_keys() {
    let mut p = parser(CTRL_A);
    let t = Instant::now();
    // Esc, then Ctrl-A x, in one read: not Alt-Ctrl-A.
    assert_eq!(
        run(&mut p, b"\x01\x1b\x01x", t),
        vec![A::C(0x1b), A::C(b'x')]
    );
    assert_eq!(
        run(&mut p, b"\x1b\x01x", t),
        vec![A::F(vec![0x1b]), A::C(b'x')]
    );
}

#[test]
fn the_prefix_is_recognised_in_other_coded_forms() {
    let t = Instant::now();
    for (prefix, form) in [
        // kitty with Num Lock on (128) and with Caps Lock on (64)
        (CTRL_A, &b"\x1b[97;133u"[..]),
        (CTRL_A, b"\x1b[97;69u"),
        (ALT_A, b"\x1b[97;131u"),
        // modifyOtherKeys
        (CTRL_A, b"\x1b[27;5;97~"),
        (ALT_A, b"\x1b[27;3;97~"),
        // event type sub-parameter
        (CTRL_A, b"\x1b[97;5:1u"),
    ] {
        let mut p = parser(prefix);
        let mut input = form.to_vec();
        input.push(b'x');
        assert_eq!(run(&mut p, &input, t), vec![A::C(b'x')], "{form:?}");
        // And after the prefix it is the prefix again.
        let mut input = form.to_vec();
        input.extend_from_slice(form);
        let sent = run(&mut p, &input, t);
        assert!(matches!(&sent[..], [A::F(_)]), "{form:?}: {sent:?}");
    }
    // Ctrl-Shift-A is not the prefix.
    let mut p = parser(CTRL_A);
    assert_eq!(
        run(&mut p, b"\x1b[97;6u", t),
        vec![A::F(b"\x1b[97;6u".to_vec())]
    );
}

#[test]
fn kitty_key_releases_are_not_the_prefix() {
    let mut p = parser(CTRL_A);
    let t = Instant::now();
    // Press, release of the prefix, then the command key.
    assert_eq!(run(&mut p, b"\x1b[97;5u\x1b[97;5:3ux", t), vec![A::C(b'x')]);
    // A release alone is passed on.
    assert_eq!(
        run(&mut p, b"\x1b[97;5:3u", t),
        vec![A::F(b"\x1b[97;5:3u".to_vec())]
    );
    // A repeat counts as a press.
    assert_eq!(run(&mut p, b"\x1b[97;5:2uq", t), vec![A::C(b'q')]);
}

#[test]
fn coded_keys_give_their_bytes_to_a_line_reader() {
    use xcomm::keys::{EscKey, escape_key, plain_byte};
    let byte = |s: &[u8]| match escape_key(s) {
        EscKey::Done { key, .. } => plain_byte(key),
        EscKey::Partial => panic!("partial {s:?}"),
    };
    assert_eq!(byte(b"\x1b[13u"), Some(b'\r'));
    assert_eq!(byte(b"\x1b[57;129u"), Some(b'9'));
    assert_eq!(byte(b"\x1b[99;5u"), Some(0x03));
    assert_eq!(byte(b"\x1b[27;5;99~"), Some(0x03));
    assert_eq!(byte(b"\x1b[27u"), Some(0x1b));
    assert_eq!(byte(b"\x1b\x1b"), Some(0x1b));
    // Alt keys, releases and arrows have none.
    assert_eq!(byte(b"\x1b[57;3u"), None);
    assert_eq!(byte(b"\x1b[57;1:3u"), None);
    assert_eq!(byte(b"\x1bx"), None);
    assert_eq!(byte(b"\x1b[D"), None);
}

#[test]
fn a_control_byte_ends_an_ss3_or_console_key() {
    let t = Instant::now();
    // Esc, Shift-O, then the prefix and a command, in one read.
    let mut p = parser(CTRL_A);
    assert_eq!(
        run(&mut p, b"\x1bO\x01x", t),
        vec![A::F(b"\x1bO".to_vec()), A::C(b'x')]
    );
    let mut p = parser(ALT_A);
    assert_eq!(
        run(&mut p, b"\x1bO\x1bax", t),
        vec![A::F(b"\x1bO".to_vec()), A::C(b'x')]
    );
    let mut p = parser(CTRL_A);
    assert_eq!(
        run(&mut p, b"\x1b[[\x01x", t),
        vec![A::F(b"\x1b[[".to_vec()), A::C(b'x')]
    );
}

#[test]
fn after_a_ctrl_prefix_an_alt_letter_is_one_unknown_key() {
    let mut p = parser(CTRL_A);
    let t = Instant::now();
    assert_eq!(run(&mut p, b"\x01", t), vec![]);
    assert_eq!(
        run(&mut p, b"\x1bxend", t),
        vec![A::C(NO_KEY), A::F(b"end".to_vec())]
    );
}

#[test]
fn linux_console_function_keys_are_one_key() {
    let mut p = parser(CTRL_A);
    let t = Instant::now();
    assert_eq!(
        run(&mut p, b"\x01\x1b[[Aend", t),
        vec![A::C(NO_KEY), A::F(b"end".to_vec())]
    );
    let mut p = parser(ALT_A);
    assert_eq!(run(&mut p, b"\x1b[[B", t), vec![A::F(b"\x1b[[B".to_vec())]);
}

/// Feeding input whole or cut into pieces at any points gives the same
/// result, as long as no deadline passes in between.
#[test]
fn reads_cut_anywhere_give_the_same_result() {
    let alphabet: &[u8] = b"\x1b\x1b\x1b[[aa97;35ux\x01OAb12~:";
    let mut seed: u64 = 0x9e37_79b9_7f4a_7c15;
    let mut next = move |n: usize| {
        seed = seed
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        (seed >> 33) as usize % n
    };
    let t = Instant::now();
    for prefix in [CTRL_A, ALT_A] {
        for _ in 0..20_000 {
            let len = next(40);
            let input: Vec<u8> =
                (0..len).map(|_| alphabet[next(alphabet.len())]).collect();
            let mut whole = parser(prefix);
            let mut expect = run(&mut whole, &input, t);
            expect.extend(flush(&mut whole, t + ms(100)));
            let mut cut = parser(prefix);
            let mut got = Vec::new();
            let mut rest = &input[..];
            while !rest.is_empty() {
                let mut n = 1 + next(rest.len());
                // With a Ctrl prefix a lone ESC at the end of a read goes
                // out at once (a terminal writes CSI-u whole), so no cut
                // there.
                while prefix == CTRL_A && rest[n - 1] == 0x1b && n < rest.len() {
                    n += 1;
                }
                got.extend(run(&mut cut, &rest[..n], t));
                rest = &rest[n..];
            }
            got.extend(flush(&mut cut, t + ms(100)));
            let merge = |v: Vec<A>| {
                let mut out: Vec<A> = Vec::new();
                for a in v {
                    match (out.last_mut(), a) {
                        (Some(A::F(prev)), A::F(b)) => prev.extend(b),
                        (_, a) => out.push(a),
                    }
                }
                out
            };
            assert_eq!(merge(got), merge(expect), "{prefix:?} {input:?}");
        }
    }
}

#[test]
fn ctrl_a_in_csi_u_form_is_the_prefix() {
    let mut p = parser(CTRL_A);
    let t = Instant::now();
    assert_eq!(run(&mut p, b"\x1b[97;5uq", t), vec![A::C(b'q')]);
    // Cut after ESC [ it is held for the rest.
    assert_eq!(run(&mut p, b"\x1b[97", t), vec![]);
    assert_eq!(run(&mut p, b";5ux", t), vec![A::C(b'x')]);
    // Other CSI sequences pass.
    assert_eq!(
        run(&mut p, b"\x1b[A\x1b[98;5u", t),
        vec![A::F(b"\x1b[A\x1b[98;5u".to_vec())]
    );
}

#[test]
fn ctrl_prefix_does_not_hold_esc() {
    let mut p = parser(CTRL_A);
    let t = Instant::now();
    assert_eq!(run(&mut p, b"\x1b", t), vec![A::F(vec![0x1b])]);
    assert_eq!(p.deadline(), None);
}

#[test]
fn a_pending_prefix_has_no_deadline() {
    // Waiting for the command letter: no timeout, the next key decides.
    let mut p = parser(CTRL_A);
    let t = Instant::now();
    run(&mut p, b"\x01", t);
    assert_eq!(p.deadline(), None);
}
