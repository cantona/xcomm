#!/bin/sh
# build.sh: what the herdr plugin runs (target/release) and what agents and
# the shell run (xcomm and xcomm-wait in ~/.local/bin).
set -eu
CDPATH='' cd -- "$(dirname -- "$0")/.."
cargo build --release --locked
cargo install --path . --locked --root "$HOME/.local" --target-dir target --force
