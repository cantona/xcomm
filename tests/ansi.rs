use xcomm::ansi::{
    EscapeStripper, SGR_HELD_MAX, SgrFilter, color_enabled, device_color_enabled,
    ends_inside, term_state,
};
use xcomm::cli::ColorMode;

fn sgr(chunks: &[&[u8]]) -> Vec<u8> {
    let mut f = SgrFilter::new();
    let mut out = Vec::new();
    for c in chunks {
        f.filter(c, &mut out);
    }
    out
}

fn strip(chunks: &[&[u8]]) -> Vec<u8> {
    let mut f = EscapeStripper::new();
    let mut out = Vec::new();
    for c in chunks {
        f.strip(c, &mut out);
    }
    out
}

/// Every way of cutting `input` in two gives the same result as one call.
fn same_when_cut(input: &[u8], run: fn(&[&[u8]]) -> Vec<u8>) {
    let whole = run(&[input]);
    for i in 0..=input.len() {
        assert_eq!(run(&[&input[..i], &input[i..]]), whole, "cut at {i}");
    }
}

#[test]
fn sgr_color_removed_attributes_and_cursor_kept() {
    assert_eq!(
        sgr(&[b"\x1b[1;32mok\x1b[0m \x1b[2J\x1b[H"]),
        b"\x1b[1mok\x1b[0m \x1b[2J\x1b[H"
    );
    assert_eq!(sgr(&[b"\x1b[mA\x1b[38:2::1:2:3mB"]), b"\x1b[mAB");
}

#[test]
fn only_color_parameters_are_dropped() {
    for (input, want) in [
        // A menu highlight survives.
        (&b"\x1b[7mX"[..], &b"\x1b[7mX"[..]),
        (b"\x1b[0;7;31mX", b"\x1b[0;7mX"),
        (b"\x1b[31;7mX", b"\x1b[7mX"),
        // 256-color and truecolor, with ; or : arguments.
        (b"\x1b[38;5;196;1mX", b"\x1b[1mX"),
        (b"\x1b[1;48;2;1;2;3;4mX", b"\x1b[1;4mX"),
        (b"\x1b[38:5:196;4:3mX", b"\x1b[4:3mX"),
        // Bright colors, defaults, underline color.
        (b"\x1b[91;102mX", b"X"),
        (b"\x1b[39;49mX", b"X"),
        (b"\x1b[58;2;1;2;3;4mX", b"\x1b[4mX"),
        (b"\x1b[59mX", b"X"),
        // An empty parameter is a reset, kept.
        (b"\x1b[;31mX", b"\x1b[mX"),
        // Arguments missing at the end.
        (b"\x1b[38mX", b"X"),
        (b"\x1b[1;38;5mX", b"\x1b[1mX"),
    ] {
        assert_eq!(sgr(&[input]), want, "{input:?}");
    }
}

#[test]
fn sgr_split_across_calls() {
    assert_eq!(sgr(&[b"a\x1b[3", b"1mb"]), b"ab");
    assert_eq!(sgr(&[b"a\x1b", b"[31mb"]), b"ab");
}

#[test]
fn non_sgr_csi_split_across_calls_is_kept_whole() {
    assert_eq!(sgr(&[b"\x1b[1", b"0;5H"]), b"\x1b[10;5H");
}

#[test]
fn private_and_intermediate_m_sequences_are_kept() {
    // xterm modifyOtherKeys and DECSET-style sequences ending in m.
    for s in [&b"\x1b[>4;1m"[..], b"\x1b[?1m", b"\x1b[1 m"] {
        assert_eq!(sgr(&[s]), s, "{s:?}");
    }
}

#[test]
fn other_escapes_and_aborted_csi_pass_the_sgr_filter() {
    let s = b"\x1b]0;t\x07\x1b7\x1b(B\x1b[1\x18x\x1b";
    // The CSI cut by CAN is passed as it is; the trailing ESC waits.
    assert_eq!(sgr(&[s]), &s[..s.len() - 1]);
    assert_eq!(sgr(&[s, b"c"]), [&s[..], b"c"].concat());
}

#[test]
fn controls_inside_a_csi_do_not_end_it() {
    // A terminal carries out the CR and ignores DEL, then applies SGR 31.
    assert_eq!(sgr(&[b"\x1b[31\rmX"]), b"\rX");
    assert_eq!(sgr(&[b"\x1b[3\x7f1mX"]), b"X");
    assert_eq!(sgr(&[b"\x1b[1\n0;5H"]), b"\n\x1b[10;5H");
    assert_eq!(strip(&[b"\x1b[31\rmX"]), b"\rX");
    assert_eq!(strip(&[b"\x1b[3\x7f1mX"]), b"X");
    assert_eq!(strip(&[b"\x1b\n7X\x1bO\rPY"]), b"\nX\rY");
}

#[test]
fn sub_aborts_strings_as_can_does() {
    // After the abort the BEL is plain text's BEL.
    assert_eq!(strip(&[b"\x1b]0;t\x1aabc\x07Z"]), b"abc\x07Z");
    assert_eq!(strip(&[b"\x1bPq\x1aabc\x1b\\Z"]), b"abcZ");
    assert_eq!(strip(&[b"\x1b]0;t\x18abc\x07Z"]), b"abc\x07Z");
}

#[test]
fn a_removed_sgr_does_not_join_its_neighbours_into_a_sequence() {
    // A lone ESC before it: ESC ESC [31m XYZ must not become ESC X (SOS).
    assert_eq!(sgr(&[b"\x1b\x1b[31mXYZ"]), b"\x1b\x18XYZ");
    // A CSI cut off by its ESC: not ESC[31X (erase), cursor up, or a mode.
    assert_eq!(sgr(&[b"\x1b[31\x1b[32mX"]), b"\x1b[31\x18X");
    assert_eq!(sgr(&[b"\x1b[1;31\x1b[32mA"]), b"\x1b[1;31\x18A");
    assert_eq!(sgr(&[b"\x1b[?25\x1b[32ml"]), b"\x1b[?25\x18l");
    // An SGR that keeps an attribute is a sequence of its own anyway.
    assert_eq!(sgr(&[b"\x1b[31\x1b[1;32mX"]), b"\x1b[31\x1b[1mX");
    // An OSC or DCS cut off by it is ended too, not left open.
    assert_eq!(sgr(&[b"\x1b]0;t\x1b[31mX"]), b"\x1b]0;t\x18X");
    assert_eq!(sgr(&[b"\x1bPq\x1b[31mX"]), b"\x1bPq\x18X");
    assert_eq!(sgr(&[b"\x1b(\x1b[31mX"]), b"\x1b(\x18X");
    // Ended properly first: nothing added.
    assert_eq!(sgr(&[b"\x1b]0;t\x1b\\\x1b[31mX"]), b"\x1b]0;t\x1b\\X");
    assert_eq!(sgr(&[b"\x1b]0;t\x07\x1b[31mX"]), b"\x1b]0;t\x07X");
    // An SGR-looking sequence inside an OSC is the OSC's text... until the
    // ESC, which ends the OSC: that is a real SGR, removed.
    assert_eq!(sgr(&[b"\x1b]2;a[31mb\x07"]), b"\x1b]2;a[31mb\x07");
}

#[test]
fn output_stays_within_twice_the_input() {
    // Each ESC here cuts off the sequence before it and adds a CAN.
    let noise: Vec<u8> = b"\x1b]\x1b\nx".repeat(800);
    let mut out = Vec::with_capacity(2 * noise.len() + SGR_HELD_MAX);
    let cap = out.capacity();
    SgrFilter::new().filter(&noise, &mut out);
    assert!(out.len() > noise.len());
    assert_eq!(out.capacity(), cap);
}

#[test]
fn ss3_takes_its_byte_in_both() {
    assert_eq!(sgr(&[b"\x1bOA\x1b[31mB"]), b"\x1bOAB");
    assert_eq!(sgr(&[b"\x1bO\x1b[31mA"]), b"\x1bO\x18A");
    assert_eq!(strip(&[b"\x1bO\x18A"]), b"A");
}

#[test]
fn truecolor_sgr_is_removed() {
    assert_eq!(
        sgr(&[b"\x1b[0;1;3;4;38;2;255;255;255;48;2;255;255;255mX"]),
        b"\x1b[0;1;3;4mX"
    );
    assert_eq!(sgr(&[b"\x1b[38;2;255;255;255;48;2;255;255;255mX"]), b"X");
}

#[test]
fn a_control_after_esc_does_not_end_it() {
    assert_eq!(sgr(&[b"\x1b\n[31mX"]), b"\nX");
    assert_eq!(strip(&[b"\x1b\n[31mX"]), b"\nX");
}

/// Filtering removes color only: the visible text (all sequences stripped)
/// is the same before and after, however the input is cut, and filtering
/// again changes nothing.
#[test]
fn filtering_keeps_the_visible_text() {
    let alphabet: &[u8] =
        b"\x1b\x1b\x1b[[]P\\m;31(B?\x07\x18\x1a\nxHNO\x7f\x80\xc3\x9b:$ X^_\r>";
    let mut seed: u64 = 0x2545_f491_4f6c_dd1d;
    let mut next = move |n: usize| {
        seed = seed
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        (seed >> 33) as usize % n
    };
    for _ in 0..50_000 {
        let len = next(30);
        let input: Vec<u8> = (0..len).map(|_| alphabet[next(alphabet.len())]).collect();
        let cut = next(len + 1);
        let mut f = SgrFilter::new();
        let mut filtered = Vec::new();
        for part in [&input[..cut], &input[cut..]] {
            let before = filtered.len();
            f.filter(part, &mut filtered);
            assert!(
                filtered.len() - before <= 2 * part.len() + SGR_HELD_MAX,
                "{input:?} cut {cut}: output past the documented bound"
            );
        }
        assert_eq!(strip(&[&filtered]), strip(&[&input]), "{input:?} cut {cut}");
        // Nothing left to remove. CAN ends what a filter would still hold
        // at the end of its input.
        let ended = [&filtered[..], b"\x18"].concat();
        assert_eq!(sgr(&[&ended]), ended, "{input:?} cut {cut}");
    }
}

#[test]
fn an_overlong_csi_passes_unfiltered() {
    let mut s = b"\x1b[".to_vec();
    s.extend(std::iter::repeat_n(b'1', SGR_HELD_MAX + 5));
    s.extend_from_slice(b"mz");
    assert_eq!(sgr(&[&s]), s);
}

#[test]
fn sgr_filter_cut_anywhere_gives_the_same_result() {
    same_when_cut(
        b"a\x1b[1;32mb\x1b[2Jc\x1b[>4;1md\x1b]0;x\x07e\x1b[0m\x1b[3\r1\x7fmf",
        sgr,
    );
}

#[test]
fn sgr_output_fits_input_plus_held() {
    let mut f = SgrFilter::new();
    let mut s = b"\x1b[".to_vec();
    s.extend(std::iter::repeat_n(b'1', SGR_HELD_MAX - 2));
    let mut out = Vec::new();
    f.filter(&s, &mut out);
    assert!(out.is_empty());
    let next = b"Hx";
    let mut out = Vec::with_capacity(next.len() + SGR_HELD_MAX);
    let cap = out.capacity();
    f.filter(next, &mut out);
    assert_eq!(out.len(), SGR_HELD_MAX + next.len());
    assert_eq!(out.capacity(), cap);
}

#[test]
fn stripper_removes_everything() {
    assert_eq!(
        strip(&[b"\x1b]0;title\x07A\x1b[31mB\x1bPdata\x1b\\C\x1bOPD\x1b7E"]),
        b"ABCDE"
    );
    assert_eq!(
        strip(&[b"\x1b(BF\x1b_apc\x1b\\G\x1b]8;;u\x1b\\H\x1b[?25lI"]),
        b"FGHI"
    );
}

#[test]
fn stripper_split_osc() {
    assert_eq!(strip(&[b"\x1b]0;ti", b"tle\x1b\\x"]), b"x");
}

#[test]
fn stripper_keeps_controls_that_cut_a_sequence() {
    // CAN aborts; a newline inside CSI is kept and the CSI goes on; ESC
    // inside OSC starts anew.
    assert_eq!(
        strip(&[b"\x1b[12\x18a\x1b[1\nmb\x1b]0;x\x1b[31mc"]),
        b"a\nbc"
    );
}

#[test]
fn stripper_cut_anywhere_gives_the_same_result() {
    same_when_cut(
        b"a\x1b]0;t\x07b\x1b[1;2Hc\x1bP1$r\x1b\\d\x1bOAe\x1b(0f\x1b]8;;x\x1b\\g\x1b[3\r1mh\x1b]0;\x1ai",
        strip,
    );
}

#[test]
fn plain_text_passes_both_unchanged() {
    let mut seed: u64 = 1;
    let text: Vec<u8> = (0..1 << 20)
        .map(|_| {
            seed = seed.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
            match (seed >> 33) % 100 {
                0 => b'\n',
                1 => b'\r',
                2 => b'\t',
                n => b' ' + (n % 95) as u8,
            }
        })
        .collect();
    assert_eq!(sgr(&[&text]), text);
    assert_eq!(strip(&[&text]), text);
}

#[test]
fn color_enabled_table() {
    let none = |_: &str| None;
    let no_color = |k: &str| (k == "NO_COLOR").then(|| "1".to_string());
    let no_color_empty = |k: &str| (k == "NO_COLOR").then(String::new);
    let dumb = |k: &str| (k == "TERM").then(|| "dumb".to_string());
    let xterm = |k: &str| (k == "TERM").then(|| "xterm-256color".to_string());
    assert!(color_enabled(ColorMode::Always, false, &no_color));
    assert!(!color_enabled(ColorMode::Never, true, &none));
    assert!(color_enabled(ColorMode::Auto, true, &none));
    assert!(color_enabled(ColorMode::Auto, true, &xterm));
    assert!(!color_enabled(ColorMode::Auto, false, &none));
    assert!(!color_enabled(ColorMode::Auto, true, &no_color));
    assert!(!color_enabled(ColorMode::Auto, true, &dumb));
    assert!(color_enabled(ColorMode::Auto, true, &no_color_empty));
}

#[test]
fn device_color_ignores_no_color() {
    let none = |_: &str| None;
    let no_color = |k: &str| (k == "NO_COLOR").then(|| "1".to_string());
    let dumb = |k: &str| (k == "TERM").then(|| "dumb".to_string());
    assert!(device_color_enabled(ColorMode::Auto, true, &none));
    assert!(device_color_enabled(ColorMode::Auto, true, &no_color));
    assert!(!device_color_enabled(ColorMode::Auto, true, &dumb));
    assert!(!device_color_enabled(ColorMode::Auto, false, &none));
    assert!(device_color_enabled(ColorMode::Always, false, &dumb));
    assert!(!device_color_enabled(ColorMode::Never, true, &none));
}

#[test]
fn ends_inside_sequences_and_characters() {
    for t in [
        &b"text\x1b"[..],
        b"text\x1b[",
        b"text\x1b[1;3",
        b"\x1b]0;title",
        b"\x1b]0;title\x1b",
        b"\x1bP1$r",
        "caf\u{e9}".as_bytes().split_last().unwrap().1,
        &"\u{65e5}".as_bytes()[..2],
        &"\u{1f600}".as_bytes()[..3],
    ] {
        assert!(ends_inside(t), "{t:?}");
    }
    for t in [
        &b""[..],
        b"plain",
        b"\x1b[1;31mred",
        b"\x1b[0m",
        b"\x1b]0;title\x07",
        b"\x1b]0;title\x1b\\",
        b"\x1b(B",
        "caf\u{e9}".as_bytes(),
        "\u{1f600}".as_bytes(),
        b"\x1b[31\x18",
    ] {
        assert!(!ends_inside(t), "{t:?}");
    }
}

#[test]
fn term_state_gives_the_sgr_since_the_last_reset() {
    let st = term_state(b"\x1b[31mred\x1b[0mplain\x1b[1mbold\x1b[4;32mgreen");
    assert_eq!(st.sgr, b"\x1b[1m\x1b[4;32m");
    assert!(!st.inside);
    // A reset with more after it is kept: it sets those.
    assert_eq!(term_state(b"\x1b[31mx\x1b[0;1my").sgr, b"\x1b[0;1m");
    assert_eq!(term_state(b"\x1b[31mx\x1b[;7my").sgr, b"\x1b[;7m");
    assert_eq!(term_state(b"\x1b[31mx\x1b[my").sgr, b"");
    assert_eq!(term_state(b"\x1b[31mx\x1b[00my").sgr, b"");
    // Not SGR: private, with intermediates, other finals; RIS resets.
    assert_eq!(
        term_state(b"\x1b[1m\x1b[?25m\x1b[1 m\x1b[2J").sgr,
        b"\x1b[1m"
    );
    assert_eq!(term_state(b"\x1b[1m\x1bcx").sgr, b"");
    // So does DECSTR.
    assert_eq!(term_state(b"\x1b[1m\x1b[!p\x1b[4mx").sgr, b"\x1b[4m");
    // An unfinished one is not kept, and is reported.
    let st = term_state(b"\x1b[1m\x1b[3");
    assert_eq!((st.sgr.as_slice(), st.inside), (&b"\x1b[1m"[..], true));
    // Only the latest whole sequences, up to a bound.
    let many = b"\x1b[1m".repeat(5000);
    let st = term_state(&many);
    assert!(
        st.sgr.len() <= 16 << 10 && st.sgr.len() > (16 << 10) - 4,
        "{}",
        st.sgr.len()
    );
    assert!(st.sgr.starts_with(b"\x1b[1m"));
}
