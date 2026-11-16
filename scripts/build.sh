#!/bin/sh
# build.sh: what the herdr plugin runs (target/release) and what agents and
# the shell run (xcomm and xcomm-wait in ~/.local/bin).
set -eu
CDPATH='' cd -- "$(dirname -- "$0")/.."
cargo build --release --locked
cargo install --path . --locked --root "$HOME/.local" --target-dir target --force
# The agent skill beside it. Refused where a dotfiles repository may own
# ~/.agents (it says so): the build still stands.
"$HOME/.local/bin/xcomm" skill install ||
	echo "build.sh: skill not installed; xcomm skill install --dir <dir> puts it elsewhere" >&2
