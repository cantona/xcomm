#!/bin/sh
# SPDX-FileCopyrightText: 2026 Su Kang Yin <paradyse@gmail.com>
# SPDX-License-Identifier: GPL-2.0-only
# picker-gif-capture.sh XCOMM OUT: the two sessions of the README's picker
# GIF, captured into OUT/s0.ansi and OUT/s1.ansi for scripts/picker-gif.py.
#
# Each is a real xcomm on a board played by scripts/demo-board.py, run in a
# private tmux server (200 columns, so nothing wraps; the GIF wraps to the
# pane) with HERDR_ENV=1, as in a herdr pane. Keys are typed as a person
# would. The boards' pty paths are then written as the by-id paths the
# picker opens (examples/screens.rs), and the pids as the lock holders it
# lists, which is all that is changed.
set -eu
# Both as the caller names them, before the cd to the repository.
case $1 in
*/*) xcomm=$(CDPATH='' cd -- "$(dirname -- "$1")" && pwd)/$(basename -- "$1") ;;
*) xcomm=$(command -v -- "$1") || { echo "picker-gif-capture: no $1 on PATH" >&2; exit 1; } ;;
esac
mkdir -p "$2"
out=$(CDPATH='' cd -- "$2" && pwd)
CDPATH='' cd -- "$(dirname -- "$0")/.."
tmp=$(mktemp -d)
sock=xcomm-gif-$$
b0='' b1=''
trap 'tmux -L "$sock" kill-server 2>/dev/null; kill $b0 $b1 2>/dev/null; rm -rf "$tmp"' EXIT

python3 scripts/demo-board.py "$tmp/b0" >/dev/null 2>&1 &
b0=$!
python3 scripts/demo-board.py "$tmp/b1" >/dev/null 2>&1 &
b1=$!
i=0
while [ ! -e "$tmp/b0" ] || [ ! -e "$tmp/b1" ]; do
	i=$((i + 1))
	[ $i -lt 50 ] || { echo "picker-gif-capture: boards did not start" >&2; exit 1; }
	sleep 0.1
done

# A session as in a herdr pane, with nothing of the caller's herdr or tmux.
session() {
	echo "env -u HERDR_PANE_ID -u HERDR_SOCKET_PATH -u HERDR_WORKSPACE_ID -u HERDR_TAB_ID" \
		"-u HC_TERMINAL_ID -u XCOMM_PREFIX -u TMUX HERDR_ENV=1" \
		"'$xcomm' --no-lock -b 115200 '$1'; sleep 60"
}
keys() {
	tmux -L "$sock" send-keys -t "$@"
}
# Until the window's screen holds TEXT, or 15 s.
until_shown() {
	n=0
	until tmux -L "$sock" capture-pane -p -t "$1" | grep -qF -- "$2"; do
		n=$((n + 1))
		[ $n -lt 150 ] || { echo "picker-gif-capture: no '$2' in $1" >&2; exit 1; }
		sleep 0.1
	done
}

tmux -L "$sock" -f /dev/null new-session -d -s g -x 200 -y 31 "$(session "$tmp/b0")"
until_shown g:0 'Alt-A Z for help'
# The board boots on the first line and waits 3 s for a key at autoboot.
keys g:0 Enter
until_shown g:0 'Hit any key'
keys g:0 Space
until_shown g:0 '=>'
type_line() {
	keys g:0 -l "$1"
	keys g:0 Enter
	until_shown g:0 "$2"
}
type_line boot 'login:'
type_line root 'Password:'
type_line secret 'Last login'
type_line 'uname -a' 'GNU/Linux'
type_line 'ip -4 addr show eth0' 'scope global eth0'
tmux -L "$sock" capture-pane -e -p -t g:0 |
	sed -e "s|$tmp/b0|/dev/serial/by-id/usb-FTDI_FT232R_USB_UART_A10K1234-if00-port0|g" \
		-e 's/pid [0-9]*;/pid 48213;/' >"$out/s0.ansi"

tmux -L "$sock" new-window -t g "$(session "$tmp/b1")"
until_shown g:1 'Alt-A Z for help'
keys g:1 Enter
until_shown g:1 'Hit any key'
tmux -L "$sock" capture-pane -e -p -t g:1 |
	sed -e "s|$tmp/b1|/dev/serial/by-id/usb-1a86_USB_Serial-if00-port0|g" \
		-e 's/pid [0-9]*;/pid 52144;/' >"$out/s1.ansi"

if grep -q "$tmp" "$out/s0.ansi" "$out/s1.ansi"; then
	echo "picker-gif-capture: a board path is left in the capture" >&2
	exit 1
fi
