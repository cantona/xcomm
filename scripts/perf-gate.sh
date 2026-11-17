#!/bin/sh
# SPDX-FileCopyrightText: 2026 Su Kang Yin <paradyse@gmail.com>
# SPDX-License-Identifier: GPL-2.0-only
# perf-gate.sh BASE-REV
# Benchmarks xcomm at BASE-REV and in the working tree on the same machine
# and inputs, and fails if any metric is more than THRESHOLD percent worse.
set -eu
# Run inside herdr, the benchmarked sessions would rename this pane.
unset HERDR_SOCKET_PATH HERDR_PANE_ID HERDR_ENV
CDPATH= cd -- "$(dirname -- "$0")/.."

if [ $# -ne 1 ]; then
	echo "usage: perf-gate.sh BASE-REV" >&2
	exit 2
fi
base=$1
# Syscalls per MiB is one of the gated metrics, so strace is required.
if ! command -v strace >/dev/null 2>&1; then
	echo "perf-gate: strace is required (syscalls per MiB is gated)" >&2
	exit 1
fi
size=${PERF_SIZE_MIB:-32}
runs=${PERF_RUNS:-15}
samples=${PERF_SAMPLES:-10000}
threshold=${PERF_THRESHOLD:-5}

root=$PWD
# Outside the repository (cargo would take a checkout under it for part of
# this workspace) and outside $TMPDIR (/tmp may be a small quota-limited
# tmpfs).
cache=${XDG_CACHE_HOME:-$HOME/.cache}
mkdir -p "$cache"
# One directory per run: concurrent runs must not delete each other's.
work=$(mktemp -d "$cache/xcomm-perf-gate.XXXXXX")
cleanup() {
	git -C "$root" worktree remove --force "$work/base" 2>/dev/null || true
	rm -rf "$work"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM
trap 'exit 129' HUP

# No `git worktree prune`: it would also drop the user's own worktrees
# whose directories are only missing for now. Each run uses a fresh path, so
# a registration left by a killed run does not get in the way.
git -C "$root" worktree add --detach "$work/base" "$base" >/dev/null
(cd "$work/base" && CARGO_TARGET_DIR="$work/base-target" cargo build --locked -q --release -p xcomm)
# Both xcomm binaries are built the same way, `-p xcomm`: a workspace build
# would unify in the harness's extra dependency features and give a
# different binary for the same source. Built where this script reads them,
# whatever CARGO_TARGET_DIR or build.target-dir say.
CARGO_TARGET_DIR="$root/target" cargo build --locked -q --release -p xcomm
CARGO_TARGET_DIR="$root/target" cargo build --locked -q --release -p xcomm-bench

bench=$root/target/release/xcomm-bench
echo "perf-gate: $base -> working tree ($size MiB, $runs alternating runs each, $samples latency samples)"
"$bench" ab --kind xcomm --base "$work/base-target/release/xcomm" --new "$root/target/release/xcomm" \
	--size-mib "$size" --runs "$runs" --samples "$samples" --threshold "$threshold" \
	--out "$work" --strace || status=$?
# Only this run's reports: a stale pair must not pass for this run's.
rm -rf "$root/target/perf-gate-last"
mkdir -p "$root/target/perf-gate-last"
cp "$work/base.json" "$work/new.json" "$root/target/perf-gate-last/" 2>/dev/null || true
if [ "${status:-0}" -ne 0 ]; then
	exit "$status"
fi
cargo test --locked -q --test alloc
echo "perf-gate: ok"
