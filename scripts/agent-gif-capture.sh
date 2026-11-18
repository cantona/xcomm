#!/bin/bash
# SPDX-FileCopyrightText: 2026 Su Kang Yin <paradyse@gmail.com>
# SPDX-License-Identifier: GPL-2.0-only
# agent-gif-capture.sh XCOMM OUT: the steps of the agent GIF, captured into
# OUT/NN-console.ansi and OUT/NN-agent.ansi for scripts/agent-gif.py.
#
# A real xcomm session on a board played by scripts/demo-board.py (left),
# and beside it a shell (right) where commands adapted from the agent skill's
# examples are typed and run, one per step: the commands an agent runs, not
# an agent. The board's pty path is written as /dev/ttyUSB0 and the session's
# pid as 48213, which is all that is changed; in the shell, a function gives
# `ttyUSB0` the pty's path.
set -euo pipefail
[ $# = 2 ] || { echo "usage: agent-gif-capture.sh XCOMM OUT" >&2; exit 2; }
case $1 in
*/*) xcomm=$(CDPATH='' cd -- "$(dirname -- "$1")" && pwd)/$(basename -- "$1") ;;
*) xcomm=$(command -v -- "$1") || { echo "agent-gif-capture: no $1 on PATH" >&2; exit 1; } ;;
esac
mkdir -p "$2"
out=$(CDPATH='' cd -- "$2" && pwd)
CDPATH='' cd -- "$(dirname -- "$0")/.."
tmp=$(mktemp -d)
sock=xcomm-agent-gif-$$
board=''
trap 'tmux -L "$sock" kill-server 2>/dev/null; [ -z "$board" ] || kill $board 2>/dev/null; rm -rf "$tmp"' EXIT

dev=$tmp/ttyUSB0
mkdir -m 700 "$tmp/run"
python3 scripts/demo-board.py "$dev" >/dev/null 2>&1 &
board=$!
i=0
while [ ! -e "$dev" ]; do
	i=$((i + 1))
	[ $i -lt 50 ] || { echo "agent-gif-capture: the board did not start" >&2; exit 1; }
	sleep 0.1
done

# Nothing of the caller's herdr, tmux or socket directory in either pane.
clean="env -u HERDR_ENV -u HERDR_PANE_ID -u HERDR_SOCKET_PATH -u HERDR_WORKSPACE_ID \
-u HERDR_TAB_ID -u HC_TERMINAL_ID -u XCOMM_PREFIX -u TMUX -u NO_COLOR \
XDG_RUNTIME_DIR=$tmp/run PATH=$(dirname -- "$xcomm"):/usr/bin:/bin"
tmux -L "$sock" -f /dev/null new-session -d -s g -x 208 -y 34 \
	"$clean '$xcomm' --no-lock -b 115200 '$dev'; sleep 60"
tmux -L "$sock" split-window -h -t g:0 "$clean DEV='$dev' bash --norc --noprofile"
con=g:0.0 agent=g:0.1

# The pane's whole history: earlier commands scroll off its screen.
screen() { tmux -L "$sock" capture-pane -p -J -S - -t "$1"; }
# Until pane $1's screen holds TEXT, or 20 s.
until_shown() {
	local n=0
	until screen "$1" | grep -qF -- "$2"; do
		n=$((n + 1))
		[ $n -lt 200 ] || { echo "agent-gif-capture: no '$2' in $1" >&2; exit 1; }
		sleep 0.1
	done
}
# Until the agent's shell prompts again after its Nth command.
until_prompt() {
	local n=0
	until [ "$(screen "$agent" | grep -c '^\$ [^ ]')" -ge "$1" ] &&
		[ "$(screen "$agent" | sed -e 's/ *$//' -e '/^$/d' | tail -1)" = '$' ]; do
		n=$((n + 1))
		[ $n -lt 200 ] || {
			echo "agent-gif-capture: no prompt after command $1" >&2
			screen "$agent" | sed '/^ *$/d' | tail -8 >&2
			exit 1
		}
		sleep 0.1
	done
	[ "$(cat "$tmp/rc")" = 0 ] || {
		echo "agent-gif-capture: command $1 exited $(cat "$tmp/rc")" >&2
		screen "$agent" | sed '/^ *$/d' | tail -8 >&2
		exit 1
	}
}
step=0
snap() {
	local f
	for f in console agent; do
		local pane=$con
		[ $f = agent ] && pane=$agent
		tmux -L "$sock" capture-pane -e -p -J -t "$pane" |
			sed -e "s|$dev|/dev/ttyUSB0|g" -e 's/pid [0-9]*;/pid 48213;/' \
				>"$out/$(printf %02d $step)-$f.ansi"
	done
	step=$((step + 1))
}

until_shown "$con" 'for help'
# The prompt writes each command's exit status to $tmp/rc, unseen.
tmux -L "$sock" send-keys -t "$agent" -l \
	"PS1='\$(echo \$? > $tmp/rc)\$ '; xcomm() { local a=() x; for x; do [ \"\$x\" = ttyUSB0 ] && x=\$DEV; a+=(\"\$x\"); done; command xcomm \"\${a[@]}\"; }; clear"
tmux -L "$sock" send-keys -t "$agent" Enter
n=0
# On its screen, not in its history: a clear may leave the setup line there.
until [ "$(tmux -L "$sock" capture-pane -p -J -t "$agent" | sed -e 's/ *$//' -e '/^$/d')" = '$' ]; do
	n=$((n + 1))
	[ $n -lt 100 ] || { echo "agent-gif-capture: the agent's shell did not clear" >&2; exit 1; }
	sleep 0.1
done
snap

cmds=0
run() {
	tmux -L "$sock" send-keys -t "$agent" -l "$1"
	tmux -L "$sock" send-keys -t "$agent" Enter
	cmds=$((cmds + 1))
}

# shellcheck disable=SC2016 # typed into the agent's shell as it is
run 'off=$(xcomm ctl ttyUSB0 status offset)'
until_prompt $cmds
snap
# The board boots on its first input and gives 3 s for a key at autoboot:
# the next command goes at once.
run "xcomm ctl ttyUSB0 send ''"
until_shown "$con" 'Hit any key'
until_prompt $cmds
snap
run "xcomm ctl ttyUSB0 wait 'Hit any key' --since \"\$off\" --timeout 60 && xcomm ctl ttyUSB0 run -n ' ' --until '=> \$' --timeout 5"
until_shown "$con" '=>'
until_prompt $cmds
snap
run "xcomm ctl ttyUSB0 run boot --until 'login: \$' --timeout 60"
until_prompt $cmds
snap
run "xcomm ctl ttyUSB0 run root --until 'Password: \$' --timeout 10"
until_prompt $cmds
run "xcomm ctl ttyUSB0 run secret --until '# \$' --fail-on 'Login incorrect' --timeout 10"
until_prompt $cmds
snap
run "xcomm ctl ttyUSB0 run 'uname -a' --until '\\r?\\n[^\\n]*# \$' --timeout 10"
until_prompt $cmds
snap
run "ip=\$(xcomm ctl ttyUSB0 run 'ip -4 addr show eth0' --until '\\r?\\n[^\\n]*# \$' --timeout 10 | sed -n '/inet /{s/.*inet \\([0-9.]*\\)\\/.*/\\1/p;q;}'); echo \"eth0: \$ip\""
until_shown "$agent" 'eth0: 192.168.7.2'
until_prompt $cmds
snap

if grep -qF "$tmp" "$out"/*.ansi; then
	echo "agent-gif-capture: a temporary path is left in the capture" >&2
	exit 1
fi
if grep -hE 'pid[ =]*[0-9]+' "$out"/*.ansi | grep -qv 'pid 48213;'; then
	echo "agent-gif-capture: a pid of this machine is left in the capture" >&2
	exit 1
fi
echo "agent-gif-capture: $step steps in $out"
