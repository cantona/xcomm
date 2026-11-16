//! Writes the README's picker and `xcomm list` screens from made-up ports
//! (list.ansi is the README's `xcomm list` example, as text),
//! through the same code that draws the real ones, so the README shows
//! nobody's hardware:
//!
//! ```sh
//! cargo run --example screens -- /tmp/screens
//! scripts/ansi2svg.py --title 'herdr popup: xcomm pick' --cols 88 \
//!     < /tmp/screens/picker.ansi > assets/picker.svg
//! ```
//!
//! It also writes the picker frames of assets/picker.gif (`gif-*.ansi`, in
//! a popup the size herdr gives it), for scripts/picker-gif.py.

use std::path::PathBuf;

use xcomm::list::PortInfo;
use xcomm::pick::{PickAction, PickKey, PickState, render};

fn port(
    tty: &str,
    vid_pid: &str,
    product: &str,
    by_id: &str,
    lock: Option<i32>,
    pane: Option<&str>,
) -> PortInfo {
    PortInfo {
        tty: tty.into(),
        path: format!("/dev/{tty}"),
        by_id: Some(format!("/dev/serial/by-id/{by_id}")),
        vid_pid: Some(vid_pid.into()),
        product: Some(product.into()),
        serial: None,
        lock_pid: lock,
        lock_error: None,
        herdr_pane: pane.map(str::to_string),
    }
}

fn main() {
    let dir = PathBuf::from(std::env::args().nth(1).expect("usage: screens <dir>"));
    std::fs::create_dir_all(&dir).unwrap();
    let ports = vec![
        port(
            "ttyACM0",
            "2e8a:000a",
            "Pico",
            "usb-Raspberry_Pi_Pico_E6614C311B4A1234-if00",
            None,
            None,
        ),
        port(
            "ttyUSB0",
            "0403:6001",
            "FT232R USB UART",
            "usb-FTDI_FT232R_USB_UART_A10K1234-if00-port0",
            Some(48213),
            Some("w1:p3"),
        ),
        port(
            "ttyUSB1",
            "1a86:7523",
            "USB Serial",
            "usb-1a86_USB_Serial-if00-port0",
            None,
            None,
        ),
        port(
            "ttyUSB2",
            "0403:6015",
            "FT231X USB UART",
            "usb-FTDI_FT231X_USB_UART_DN01ABCD-if00-port0",
            Some(51877),
            None,
        ),
    ];
    std::fs::write(dir.join("list.ansi"), xcomm::list::table(&ports)).unwrap();
    let shot = |state: &PickState, (cols, rows): (u16, u16), name: &str| {
        let mut screen = Vec::new();
        render(state, cols, rows, &mut screen);
        std::fs::write(dir.join(name), screen).unwrap();
    };
    let mut state = PickState::new(ports.clone());
    state.selected = 2;
    shot(&state, (88, 12), "picker.ansi");

    // The GIF: Enter on a port open in a pane goes to that pane, on a free
    // port opens one, and on a port another program holds says so. Each
    // step is checked against what the picker does with the key.
    const POPUP: (u16, u16) = (88, 19);
    let mut state = PickState::new(ports.clone());
    for (i, name) in ["gif-a-0.ansi", "gif-a-1.ansi"].iter().enumerate() {
        state.selected = i;
        shot(&state, POPUP, name);
    }
    assert_eq!(
        state.clone().apply(PickKey::Enter),
        PickAction::Focus("w1:p3".into())
    );
    state.selected = 2;
    shot(&state, POPUP, "gif-b-2.ansi");
    assert_eq!(state.clone().apply(PickKey::Enter), PickAction::Open);
    // ttyUSB1 is now open in the pane that opened.
    let mut opened = ports;
    opened[2].lock_pid = Some(52144);
    opened[2].herdr_pane = Some("w1:p4".into());
    let mut state = PickState::new(opened);
    for i in 0..4 {
        state.selected = i;
        shot(&state, POPUP, &format!("gif-c-{i}.ansi"));
    }
    assert_eq!(state.apply(PickKey::Enter), PickAction::Redraw);
    shot(&state, POPUP, "gif-c-in-use.ansi");
}
