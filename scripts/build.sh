#!/bin/sh
# SPDX-FileCopyrightText: 2026 Su Kang Yin <paradyse@gmail.com>
# SPDX-License-Identifier: GPL-2.0-only
# build.sh: what the herdr plugin runs (target/release) and what agents and
# the shell run (xcomm and xcomm-wait in ~/.local/bin).
set -eu
CDPATH='' cd -- "$(dirname -- "$0")/.."
cargo build --release --locked
cargo install --path . --locked --root "$HOME/.local" --target-dir target --force
# The man page where man looks beside ~/.local/bin (cargo installs none):
# never through a link, nor over another program's page. Like the skill,
# the build stands without it.
man="$HOME/.local/share/man/man1/xcomm.1"
# The deepest of its directories that is there, as it really is: a link
# below $HOME on the way (stow folds directories into links) leads into
# someone else's tree.
dir=${man%/*}
while [ ! -d "$dir" ] && [ "$dir" != "$HOME" ]; do dir=${dir%/*}; done
# One that cannot be entered cannot be looked into: no page then.
real=$(cd "$dir" 2>/dev/null && pwd -P) || real=
want=$(cd "$HOME" 2>/dev/null && pwd -P) || want=
want=$want${dir#"$HOME"}
if [ -z "$real" ]; then
	echo "build.sh: cannot look in $dir; man page not installed" >&2
elif [ -L "$man" ] || [ "$real" != "$want" ] ||
	{ [ -e "$man" ] && ! grep -q '^\.TH XCOMM 1 ' "$man"; }; then
	echo "build.sh: $man is not xcomm's, or a link leads there; left alone" >&2
elif ! { mkdir -p "${man%/*}" && cp xcomm.1 "$man"; }; then
	echo "build.sh: man page not installed" >&2
fi
# The agent skill beside it. Refused where a dotfiles repository may own
# ~/.agents (it says so): the build still stands.
"$HOME/.local/bin/xcomm" skill install ||
	echo "build.sh: skill not installed; xcomm skill install --dir <dir> puts it elsewhere" >&2
