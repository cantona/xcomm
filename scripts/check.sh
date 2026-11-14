#!/bin/sh
# check.sh          check the working tree
# check.sh --staged check exactly what is staged (the tree the next commit will contain)
set -eu
CDPATH= cd -- "$(dirname -- "$0")/.."

case $# in
0) ;;
1)
	if [ "$1" != "--staged" ]; then
		echo "usage: check.sh [--staged]" >&2
		exit 2
	fi
	# Export inside target/, not $TMPDIR: cargo, rustup, rustfmt and clippy look for
	# config in every parent directory, and a world-writable /tmp lets anyone plant one.
	mkdir -p target
	tmp=$(mktemp -d "$PWD/target/staged-src.XXXXXX")
	trap 'rm -rf "$tmp"' EXIT
	# dash skips the EXIT trap when a signal kills the shell; exiting from a trap runs it
	trap 'exit 130' INT
	trap 'exit 143' TERM
	trap 'exit 129' HUP
	git checkout-index --all --prefix="$tmp/"
	# A separate target dir: the exported copy has newer mtimes than the working
	# tree, and sharing target/ would make cargo treat working-tree edits as built.
	CARGO_TARGET_DIR="$PWD/target/staged"
	export CARGO_TARGET_DIR
	cd "$tmp"
	;;
*)
	echo "usage: check.sh [--staged]" >&2
	exit 2
	;;
esac

msrv=$(sed -n 's/^rust-version = "\(.*\)"$/\1/p' Cargo.toml)
case $msrv in
*.*.*) toolchain=$msrv ;;
*.*) toolchain=$msrv.0 ;;
*)
	echo "check.sh: unexpected rust-version '$msrv'" >&2
	exit 1
	;;
esac
if ! rustup run "$toolchain" rustc --version >/dev/null 2>&1; then
	echo "check.sh: MSRV toolchain $toolchain is not installed; run: rustup toolchain install $toolchain" >&2
	exit 1
fi

cargo fmt --all -- --check
cargo clippy --locked --workspace --all-targets -- -D warnings
cargo test --locked --workspace
cargo "+$toolchain" check --locked --workspace --all-targets
cargo build --locked --release
echo "check.sh: ok (msrv toolchain $toolchain)"
