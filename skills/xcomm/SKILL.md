---
name: xcomm
description: "Drive a serial console (UART, /dev/ttyUSB*, /dev/ttyACM*, U-Boot, boot logs, board shells) through a running xcomm session: send commands, wait for prompts, read output, capture logs, transfer files with zmodem. Use when a task needs a device's serial console. Not for sessions started with --ctl ro or off."
---

# xcomm

xcomm is a serial terminal. A running session keeps the port open and has a
control socket: `xcomm ctl` sends to the device and reads what it said,
from another shell, while the user may be watching the same console.

`<dev>` below is the device as the session was started with it: a name in
/dev (`ttyUSB0`) or a path (`/dev/serial/by-id/usb-FTDI_...`).

## When to use it, and when not

- Use it to type into a board's console, wait for a prompt, read a boot
  log, or move a file over the line.
- Not for a session started with `--ctl ro` (read-only: `status`, `read`
  and `wait` only) or `--ctl off` (no socket at all). Never try to get round
  either: the user chose it.
- Not for opening a port the user did not name. A port in use by someone
  else is theirs.

## Before you start

List the running sessions, then look at the one you will use:

```sh
xcomm ctl sessions
xcomm ctl --json <dev> status
```

`status` gives the port's settings, whether it is connected, the output
offsets, a running capture or transfer, and `ctl` (`rw` or `ro`). Exit 3
means there is no session for that device. `status <field>` prints one
field's value alone (`status offset`, `status pid`), with no `jq`
needed. `--json` is still there on every `ctl` command: it gives the
whole reply as one object, for a JSON parser.

## Starting a session

Inside herdr, ask the user to run the plugin action "xcomm: open serial
port", or open it yourself:

```sh
herdr plugin action invoke --plugin cantona.herdr-xcomm open
```

Otherwise start a headless session (no terminal; it runs until it is
signalled). It prints its pid and its log: note both, since `%1`, `$!` and
`$log` do not survive into another shell. Its messages go to a temporary
log, not to your output, which would otherwise stay open while it runs.
The last line waits up to 5 s for a session on the port to answer, and
prints its pid:

```sh
log=$(mktemp)
xcomm --headless -b 115200 <dev> > "$log" 2>&1 & echo "pid $! log $log"
for i in $(seq 25); do xcomm ctl <dev> status pid 2>/dev/null && break; sleep 0.2; done
```

If that is another pid, or nothing came, your start failed (the log says
why: the port is someone else's, or the name is wrong) and any session
there is not yours: leave it alone. When you are done, stop yours with
`kill <pid>`, the pid you noted, and remove the log (`rm <log>`); remove
the log of a failed start once you have read it.

## Rules

- Always give `wait` and `run` a `--timeout` (seconds). The default is 10;
  a boot can take longer, a prompt much less.
- Add `--fail-on` for what means the command went wrong (`Unknown
  command`, `not found`, a kernel `panic`), so a failure ends the wait at
  once with exit 7 instead of a timeout.
- Read the output since your own command, not the whole buffer: take
  the offset with `xcomm ctl <dev> status offset` before you act, and pass it
  to `--since` (`wait` and `read` also give the next one, as `end=` and
  `next=` on stderr). `--since 0` reaches back to output from before, such as the
  previous boot.
- Ask the user before a break, a DTR/RTS change or a baud change on a
  session you did not start: each can reset or interrupt the board.
- Never type into a `ro` session. The socket refuses it (exit 8).
- What you send shows in the user's terminal as a `[ctl]` line, unless
  the session was started with `--no-ctl-marker` or `--headless`, or its
  output is not a terminal (piped to `tee`, say).

## Recipes

Run a command and get what it printed, up to the next prompt. The output
ends where the match starts, so start the pattern at the line break to
leave the prompt's line out (`\r?\n`, so a CR before it goes too).
Output that does not end in a line break (`printf abc`) is on the
prompt's line and goes with it: match just the prompt (`'\$ $'`) for
that, and cut the prompt off yourself.

```sh
xcomm ctl <dev> run 'uname -a' --until '\r?\n[^\n]*\$ $' --timeout 10
```

Wait for something from now on, and remember where it ended:

```sh
xcomm ctl <dev> wait 'login:' --timeout 120
```

Wait from an offset taken before you acted, so a line that came before
the wait started still counts:

```sh
off=$(xcomm ctl <dev> status offset)
xcomm ctl <dev> send 'reboot'
xcomm ctl <dev> wait 'Starting kernel' --since "$off" --timeout 60
```

Wait until the device has been quiet for a second (no prompt to match):

```sh
xcomm ctl <dev> wait --idle 1000 --timeout 30
```

Read what came since an offset (the next one goes to stderr as `next=`):

```sh
xcomm ctl <dev> read --since "$off"
```

Send bytes: a line with Enter (CR), no line ending, or raw hex:

```sh
xcomm ctl <dev> send 'reboot'
xcomm ctl <dev> send -n 'y'
xcomm ctl <dev> send --hex 03
```

## Exit codes

| Code | Meaning | What to do |
|---|---|---|
| 0 | ok | |
| 1 | `wait`/`run` timed out, or another error | read the output, then decide; do not just retry |
| 2 | usage error; or `run` had more than `--max-bytes` before the match; or xcomm-wait is missing beside xcomm | read the message |
| 3 | no session for the device (or one started with `--ctl off`, or another user's) | `xcomm ctl sessions` and `xcomm list` (who holds the port); ask before starting one |
| 4 | port disconnected | the board was unplugged or reset; wait and check `status` |
| 5 | busy (file transfer) | wait for the transfer to end |
| 6 | offset evicted | the output left the buffer; read from `oldest` in `status` |
| 7 | a `--fail-on` pattern matched | the command failed; read the match |
| 8 | permission denied: the session is `--ctl ro`, or its socket is not yours | stop; the session is not yours to drive |
| 9 | subscriber dropped (fell behind) | read again from the last offset |

## Examples

### Stop U-Boot autoboot

Take the offset, have the board reset (ask the user first), then catch
the countdown from that offset - U-Boot prints it once, and a wait from
now on misses it if it came first:

```sh
off=$(xcomm ctl <dev> status offset)
xcomm ctl <dev> wait 'Hit any key' --since "$off" --timeout 60 && xcomm ctl <dev> run -n ' ' --until '=> $' --timeout 5
```

`run` waits from the offset of its own send, so the prompt cannot come
before its wait starts.

### Log in

```sh
xcomm ctl <dev> run '' --until 'login: $' --timeout 10
xcomm ctl <dev> run 'root' --until 'Password: $' --timeout 10
xcomm ctl <dev> run 'secret' --until '# $' --fail-on 'Login incorrect' --timeout 10
```

### Run and pick out a value

`run` prints what came between the echoed command and the match, with
no line ending of its own: `\r?\n` starts the match at the CR, so the
value does not end in one. Take a value out of it with `sed`:

```sh
ver=$(xcomm ctl <dev> run 'cat /proc/version' --until '\r?\n[^\n]*# $' --timeout 10)
ip=$(xcomm ctl <dev> run 'ip -4 addr show eth0' --until '\r?\n[^\n]*# $' --timeout 10 | sed -n '/inet /{s/.*inet \([0-9.]*\)\/.*/\1/p;q;}')
echo "kernel: $ver, eth0: $ip"
```

`--json` gives the same as one object: `which` (`until`, `fail_on`,
`idle` or `timeout`), `text` and `groups` of the match, `end`, and
`output`.

### Capture a boot log

```sh
xcomm ctl <dev> capture start boot.log
xcomm ctl <dev> wait 'login:' --timeout 120
xcomm ctl <dev> capture stop
```

A relative path is from the directory you run `xcomm ctl` in. The
session appends to the file. One capture runs at a time: a session already
capturing (started with `-C`, or an earlier `capture start`) refuses with
exit 1 and names the file.

### Upload a file with zmodem

Start the receiver on the board, then send (lrzsz must be installed where
the session runs; the reply comes when the transfer ends):

```sh
xcomm ctl <dev> send 'rz'
xcomm ctl <dev> xfer send zmodem fw.bin
```

## Limits

- A match spans at most 64 KiB of output.
- `run` fails with exit 2, printing nothing, when more than `--max-bytes`
  (default 16 MiB) comes before the match: give a closer `--until`.
- The session keeps the last `--ring-size` of output (default 1 MiB); older
  offsets are evicted (exit 6).
- Escape sequences are removed before matching unless `--raw`.
