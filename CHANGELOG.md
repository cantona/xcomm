# Changelog

What changed in each release of xcomm.

## [Unreleased]

### Added
- `xcomm ctl read|wait|run -q` (`--quiet`): no `next=`/`end=` line on stderr,
  for output piped on to a program that shows it on the same terminal (`tee`,
  `less`).

### Changed
- Licensed under the GNU GPL, version 2 only (GPL-2.0-only); every source file
  carries an SPDX copyright and license header.

### Fixed
- Control socket: a client dropped between two looks at the clients (a
  subscriber left behind) no longer makes the clients after it act on each
  other's events for a pass.
- A break a client asks for that the driver refuses no longer leaves the port
  without its flow control (`--flow rts` or `xon`) for the rest of the session.
- `ctl run` and `ctl read`: the `end=`/`next=` line no longer runs on from
  output that does not end its line (`6.1+end=70`) where stderr goes to
  the same terminal or stream as stdout; stdout itself is unchanged.
- `NO_COLOR` takes the dim style off the `[ctl]` marker (xcomm's own text),
  and leaves the device's colors as they are.
- `xcomm watch`: each herdr server gets its own watcher. Two servers running
  at once (herdr sessions) shared one, so the second had no hotplug
  notifications. A watcher started by an older xcomm is recognised, so an
  upgrade does not start a second one.
- Control socket: a client that closed with a send the device never takes
  held its place forever, and 32 of them locked every client out until the
  session restarted. Such a client is dropped once the line has taken nothing
  for 30 s; a slow line, or a send waiting behind others, is not affected.

## [0.1.2] - 2026-11-17

### Added
- `xcomm ctl <dev> status <field>` prints one status field's value alone
  (`status offset`, `status pid`), so scripts and the agent skill need no
  `jq`. `--json` is unchanged.
- Man page: an EXAMPLES section, with `--json` examples.

### Changed
- Agent skill: reads the offset and the session's pid with `status <field>`;
  the run example takes values out with plain `run` and `sed`, and run
  patterns start at `\r?\n` so a value does not end in a CR. Its tests run
  without `jq`.

### Fixed
- The install tests pass where xcomm is also installed system-wide.

## [0.1.1] - 2026-11-17

### Changed
- Builds with Rust 1.93 (the cargo in Ubuntu 26.04) and libc 0.2.186 (the
  version in Debian unstable); was Rust 1.96 and libc 0.2.191.
- Code formatted at 90 columns, with a `rustfmt.toml`; CI on Linux and macOS
  for every push.

### Fixed
- herdr picker: after an error, the prompt no longer drops the Enter that
  closes it, and an arrow key whose bytes arrive split on a busy machine is no
  longer typed as text (it waits 100 ms for the rest of a key, from its latest
  bytes).

## [0.1.0] - 2026-11-17

The first release: a clean-room serial terminal for Linux and macOS with
minicom's options and keys, a control socket that scripts and AI coding agents
can drive while you watch, and a herdr plugin.

### Added
- `xcomm ctl`: send commands, wait for prompts, read output by offset, flip
  DTR/RTS, send breaks, change the baud and run zmodem transfers on a live
  session.
- `xcomm skill install`: an agent skill that teaches a coding agent to stop
  U-Boot, log in and run commands.
- Reopens the port after the adapter is unplugged or the board power-cycled.
- herdr plugin: a port picker that opens each port in its own pane, line
  control actions, hotplug notifications.
- minicom's options and its `Ctrl-A` keys (`Alt-A` inside a multiplexer).

[Unreleased]: https://github.com/cantona/xcomm/compare/v0.1.2...HEAD
[0.1.2]: https://github.com/cantona/xcomm/compare/v0.1.1...v0.1.2
[0.1.1]: https://github.com/cantona/xcomm/compare/v0.1.0...v0.1.1
[0.1.0]: https://github.com/cantona/xcomm/releases/tag/v0.1.0
