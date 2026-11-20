#!/usr/bin/env bash
# SPDX-FileCopyrightText: 2026 Su Kang Yin <paradyse@gmail.com>
# SPDX-License-Identifier: GPL-2.0-only
# release.sh: the xcomm release, step by step, as its maintainer runs it.
#
#   release.sh prepare  <ver> <date>       bump versions, date the CHANGELOG and man page
#   release.sh check                       privacy scan, check.sh here and on the Mac, publish dry run
#   release.sh commit   <ver> <msg-file>   release commit and annotated tag
#   release.sh push     <ver>              privacy scan, then push master and the tag (asks)
#   release.sh wait-ci                     wait for CI on the pushed master head
#   release.sh publish  <ver>              cargo publish from the tagged tree (asks)
#   release.sh gh-release <ver> <notes-file>   GitHub release                 (asks)
#   release.sh binaries <ver>              wait for the static binaries, check them
#   release.sh brew-bump  <ver>            tap branch, formula bump, PR        (asks)
#   release.sh brew-publish <pr>           wait for bottles, run brew pr-pull (asks)
#
# <date> is YYYY-MM-DD. Each step stops at the first problem. YES=1 answers
# the questions. Commits and the tag are made with this repository's git
# identity (user.name, user.email), in the tap too.
#
# Environment: MAC_HOST (mac; empty skips the Mac, with a warning), MAC_DIR
# (build/xcomm-test, under the Mac's HOME), TAP_DIR (~/build/homebrew-tap),
# XCOMM_PRIVACY_PATTERNS (a file of extended regexes, one a line, that must
# not appear in what is pushed or published; required), VERIFY_MESSAGE (a
# commit message checker for the release commit, run if the file is there).
set -euo pipefail

repo=$(git -C "$(dirname -- "$0")/.." rev-parse --show-toplevel)
cd "$repo"
MAC_HOST=${MAC_HOST-mac}
MAC_DIR=${MAC_DIR:-build/xcomm-test}
TAP_DIR=${TAP_DIR:-$HOME/build/homebrew-tap}
XCOMM_PRIVACY_PATTERNS=${XCOMM_PRIVACY_PATTERNS:-$HOME/.config/xcomm/privacy-patterns}
VERIFY_MESSAGE=${VERIFY_MESSAGE:-$HOME/.agents/skills/git-commit-composer/scripts/verify-message.sh}
GH_REPO=cantona/xcomm
TAP_REPO=cantona/homebrew-tap
RELEASE_FILES=(CHANGELOG.md Cargo.lock Cargo.toml herdr-plugin.toml xcomm.1)

die() { echo "release.sh: $*" >&2; exit 1; }
say() { echo "== $*"; }
confirm() {
	[ "${YES:-}" = 1 ] && return 0
	printf '%s [y/N] ' "$1" > /dev/tty
	read -r answer < /dev/tty
	[ "$answer" = y ] || die "stopped"
}
version_ok() { [[ $1 =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || die "bad version '$1'"; }
clean_master() {
	[ "$(git branch --show-current)" = master ] || die "not on master"
	[ -z "$(git status --porcelain --untracked-files=no)" ] || die "the tree has changes"
}
current_version() { sed -n 's/^version = "\(.*\)"$/\1/p' Cargo.toml | head -1; }
identity() {
	NAME=$(git config --local user.name) || die "no user.name in this repository's own git config (git config --local)"
	EMAIL=$(git config --local user.email) || die "no user.email in this repository's own git config (git config --local)"
}

# Fails when stdin holds a private pattern, and when the scan cannot run:
# no patterns file, an empty one, or a pattern grep refuses.
privacy_scan() {
	local what=$1 rc=0
	[ -s "$XCOMM_PRIVACY_PATTERNS" ] ||
		die "no privacy patterns in $XCOMM_PRIVACY_PATTERNS (a file of regexes, one a line)"
	grep -niE -f "$XCOMM_PRIVACY_PATTERNS" || rc=$?
	case $rc in
	0) die "a private pattern is in $what (above)" ;;
	1) ;;
	*) die "the privacy scan of $what failed (grep exit $rc)" ;;
	esac
}

cmd_prepare() {
	local ver=$1 date=$2 old
	version_ok "$ver"
	[[ $date =~ ^[0-9]{4}-[0-9]{2}-[0-9]{2}$ ]] || die "bad date '$date'"
	clean_master
	old=$(current_version)
	[ "$old" != "$ver" ] || die "already at $ver"
	grep -q '^## \[Unreleased\]$' CHANGELOG.md || die "no Unreleased section"
	python3 - "$old" "$ver" "$date" <<'EOF'
import re, sys
old, ver, date = sys.argv[1:]
def edit(path, pairs):
    s = open(path).read()
    for a, b in pairs:
        if s.count(a) != 1:
            sys.exit(f"{path}: expected one '{a}', found {s.count(a)}")
        s = s.replace(a, b)
    open(path, "w").write(s)
edit("Cargo.toml", [(f'version = "{old}"\n', f'version = "{ver}"\n')])
edit("herdr-plugin.toml", [(f'version = "{old}"\n', f'version = "{ver}"\n')])
th = re.search(r'^\.TH XCOMM 1 "[0-9-]+" "xcomm [0-9.]+"', open("xcomm.1").read(), re.M)
if not th:
    sys.exit("xcomm.1: no .TH XCOMM 1 line")
edit("xcomm.1", [(th.group(0), f'.TH XCOMM 1 "{date}" "xcomm {ver}"')])
s = open("CHANGELOG.md").read()
a = s.index("## [Unreleased]\n") + len("## [Unreleased]\n")
b = s.index("\n## [", a)
if not s[a:b].strip():
    sys.exit("CHANGELOG.md: nothing under Unreleased")
edit("CHANGELOG.md", [
    ("## [Unreleased]\n", f"## [Unreleased]\n\n## [{ver}] - {date}\n"),
    (f"[Unreleased]: https://github.com/cantona/xcomm/compare/v{old}...HEAD\n",
     f"[Unreleased]: https://github.com/cantona/xcomm/compare/v{ver}...HEAD\n"
     f"[{ver}]: https://github.com/cantona/xcomm/compare/v{old}...v{ver}\n"),
])
EOF
	cargo check -q --locked 2>/dev/null || cargo check -q
	git diff --stat
	say "prepared $old -> $ver; read the diff, then: release.sh check"
}

cmd_check() {
	local log=${TMPDIR:-/tmp}/xcomm-release-check.log rc=0
	say "privacy: what origin/master does not have yet, and the working tree's changes"
	{ git log -p --format=fuller origin/master..HEAD; git diff HEAD; } | privacy_scan "what would be pushed"
	say "check.sh here"
	sh scripts/check.sh > "$log" 2>&1 || { tail -30 "$log"; die "check.sh failed here"; }
	if [ -n "$MAC_HOST" ]; then
		say "check.sh on $MAC_HOST (~/$MAC_DIR)"
		# A plain path under the Mac's HOME, and a copy this script made: the
		# copy is emptied (all but its build directory) before each run.
		[[ $MAC_DIR =~ ^[A-Za-z0-9_][A-Za-z0-9_./-]*$ && ! $MAC_DIR =~ (^|/)\.\.?(/|$) ]] ||
			die "MAC_DIR '$MAC_DIR' is not a plain relative path"
		# Exactly the tracked files, with the working changes.
		# shellcheck disable=SC2029 # MAC_DIR is meant to expand here.
		git ls-files | tar -cf - -T - | ssh "$MAC_HOST" "d=\$HOME/'$MAC_DIR'
			if [ -e \"\$d\" ] && [ ! -f \"\$d/.xcomm-release-copy\" ] && [ -n \"\$(ls -A \"\$d\")\" ]; then
				echo \"\$d is not a copy release.sh made (no .xcomm-release-copy): refusing to empty it\" >&2; exit 3
			fi
			mkdir -p \"\$d\" &&
			find \"\$d\" -mindepth 1 -maxdepth 1 ! -name target -exec rm -rf {} + &&
			touch \"\$d/.xcomm-release-copy\" &&
			tar -xf - -C \"\$d\"" || die "could not copy the tree to $MAC_HOST:~/$MAC_DIR"
		# shellcheck disable=SC2029
		ssh "$MAC_HOST" "cd ~/'$MAC_DIR' && PATH=\$HOME/.cargo/bin:/opt/homebrew/bin:\$PATH sh scripts/check.sh > /tmp/xcomm-release-check.log 2>&1" ||
			{ ssh "$MAC_HOST" 'grep -E "panicked|FAILED" -A4 /tmp/xcomm-release-check.log | head -30' || true
			  die "check.sh failed on $MAC_HOST"; }
	else
		echo "release.sh: MAC_HOST is empty: the Mac check is skipped" >&2
	fi
	say "cargo publish --dry-run"
	cargo publish --dry-run --allow-dirty -q > "$log" 2>&1 || rc=$?
	rm -rf target/package
	[ "$rc" = 0 ] || { tail -30 "$log"; die "cargo publish --dry-run failed"; }
	say "checks passed"
}

cmd_commit() {
	local ver=$1 msg=$2 extra
	version_ok "$ver"
	[ -s "$msg" ] || die "no message in '$msg'"
	[ "$(current_version)" = "$ver" ] || die "Cargo.toml is not at $ver: release.sh prepare first"
	git rev-parse -q --verify "refs/tags/v$ver" > /dev/null && die "tag v$ver exists"
	{ head -1 "$msg" | grep -qx "chore(release): $ver"; } || die "the message must start 'chore(release): $ver'"
	privacy_scan "the message" < "$msg"
	identity
	git add -- "${RELEASE_FILES[@]}"
	extra=$({ git diff --name-only; git diff --cached --name-only; } | grep -vxF -f <(printf '%s\n' "${RELEASE_FILES[@]}") | sort -u || true)
	[ -z "$extra" ] || die "changes outside the release files: $(echo "$extra" | tr '\n' ' ')"
	GIT_AUTHOR_NAME=$NAME GIT_AUTHOR_EMAIL=$EMAIL GIT_COMMITTER_NAME=$NAME GIT_COMMITTER_EMAIL=$EMAIL \
		git commit -q -F "$msg"
	if [ -f "$VERIFY_MESSAGE" ] && ! sh "$VERIFY_MESSAGE" HEAD; then
		# Undone, not left for an amend: fix the message file and run again.
		git reset -q --soft HEAD~1
		die "the message failed $VERIFY_MESSAGE: fix $msg and run release.sh commit again"
	fi
	GIT_COMMITTER_NAME=$NAME GIT_COMMITTER_EMAIL=$EMAIL git tag -a "v$ver" -m "xcomm $ver"
	git log -1 --format='%h %an <%ae> %s'
	say "tagged v$ver; next: release.sh push $ver"
}

cmd_push() {
	local ver=$1
	version_ok "$ver"; clean_master
	[ "$(git rev-parse "v$ver^{commit}")" = "$(git rev-parse HEAD)" ] || die "v$ver is not HEAD"
	say "privacy: the commits and the tag to push"
	{ git log -p --format=fuller origin/master..HEAD; git cat-file tag "v$ver"; } | privacy_scan "what would be pushed"
	git log --format='%h %an <%ae> %s' origin/master..HEAD
	confirm "push these and v$ver to origin?"
	git push origin master "v$ver"
}

cmd_wait_ci() {
	local sha id=''
	sha=$(git rev-parse origin/master)
	for _ in $(seq 30); do
		id=$(gh run list -R "$GH_REPO" --commit "$sha" --event push --json databaseId --jq '.[0].databaseId' 2>/dev/null || true)
		[ -n "$id" ] && [ "$id" != null ] && break
		id=''
		sleep 10
	done
	[ -n "$id" ] || die "no CI run for $sha"
	say "CI run $id on ${sha:0:7}"
	gh run watch -R "$GH_REPO" "$id" --exit-status --interval 30 > /dev/null || die "CI failed: gh run view -R $GH_REPO $id --log-failed"
	say "CI passed"
}

cmd_publish() {
	local ver=$1
	version_ok "$ver"; clean_master
	[ "$(git rev-parse "v$ver^{commit}")" = "$(git rev-parse HEAD)" ] || die "v$ver is not HEAD"
	[ "$(git rev-parse HEAD)" = "$(git rev-parse origin/master)" ] || die "HEAD is not pushed"
	confirm "cargo publish xcomm $ver to crates.io (cannot be undone)?"
	cargo publish --locked
}

cmd_gh_release() {
	local ver=$1 notes=$2
	version_ok "$ver"
	[ -s "$notes" ] || die "no notes in '$notes'"
	git ls-remote --exit-code --tags origin "refs/tags/v$ver" > /dev/null || die "v$ver is not pushed"
	privacy_scan "the release notes" < "$notes"
	confirm "create the GitHub release xcomm $ver?"
	gh release create "v$ver" -R "$GH_REPO" --title "xcomm $ver" --notes-file "$notes" --verify-tag
}

# The static Linux binaries the release workflow attaches (.github/workflows/
# release.yml, started by gh-release): waits for its run, then checks every
# tarball against its checksum, that the binaries in it are static, and that
# the one this machine can run gives the version.
cmd_binaries() {
	local ver=$1 tag id='' dir t name b kind sum arch
	version_ok "$ver"
	tag=v$ver
	say "waiting for the static binaries of $tag"
	# The newest run for the tag, started by the release or by hand (its
	# run-name carries the tag either way). gh failing stops the step.
	for _ in $(seq 30); do
		id=$(gh run list -R "$GH_REPO" --workflow release.yml --limit 20 --json databaseId,displayTitle \
			--jq "[.[] | select(.displayTitle == \"Release binaries $tag\")] | .[0].databaseId // empty") ||
			die "gh run list failed (above)"
		[ -n "$id" ] && break
		sleep 10
	done
	[ -n "$id" ] || die "no release.yml run for $tag: is the release published?"
	gh run watch -R "$GH_REPO" "$id" --exit-status --interval 30 > /dev/null ||
		die "the release binaries failed: gh run view -R $GH_REPO $id --log-failed (a run just started by hand may not be listed yet: when gh run list -R $GH_REPO --workflow release.yml shows it, run binaries again)"
	dir=$(mktemp -d)
	# Expanded now: the local is gone by the time the script exits.
	trap "rm -rf -- $(printf '%q' "$dir")" EXIT
	for t in x86_64-unknown-linux-musl aarch64-unknown-linux-musl; do
		name=xcomm-$ver-$t
		gh release download "$tag" -R "$GH_REPO" -D "$dir" -p "$name.tar.gz" -p "$name.tar.gz.sha256" ||
			die "$name is not attached to $tag"
		# Compared whole: sha256sum -c would check whichever file the line names.
		sum=$(cd "$dir" && sha256sum "$name.tar.gz") || die "sha256sum $name.tar.gz failed"
		[ "$sum" = "$(cat "$dir/$name.tar.gz.sha256")" ] || die "$name.tar.gz does not match its checksum"
		case $t in
		x86_64-*) arch=x86-64 ;;
		aarch64-*) arch='ARM aarch64' ;;
		esac
		tar -xzf "$dir/$name.tar.gz" -C "$dir"
		for b in xcomm xcomm-wait; do
			[ -x "$dir/$name/$b" ] || die "$name has no $b"
			# Said positively: a file that cannot run, or cannot tell, fails.
			kind=$(file -b "$dir/$name/$b") || die "file $name/$b failed"
			[[ $kind == *"statically linked"* || $kind == *"static-pie linked"* ]] ||
				die "$name/$b is not static: $kind"
			[[ $kind == *", $arch,"* ]] || die "$name/$b is not built for $arch: $kind"
		done
		if [ "$(uname -s)" = Linux ] && [ "$t" = "$(uname -m)-unknown-linux-musl" ]; then
			[ "$("$dir/$name/xcomm" --version)" = "xcomm $ver" ] || die "$name/xcomm is not $ver"
		else
			say "$name: not run here ($(uname -sm)): its version is not checked"
		fi
		say "$name: checksum, static, $arch, ok"
	done
}

# The formula at stdin with the new tarball, and without the old bottles.
formula_bump() {
	python3 -c '
import re, sys
ver, sha = sys.argv[1:]
s = sys.stdin.read()
s, n = re.subn(r"archive/refs/tags/v[0-9.]+\.tar\.gz", f"archive/refs/tags/v{ver}.tar.gz", s)
assert n == 1, "url"
s, n = re.subn(r"(\n  url .*\n  sha256 \")[0-9a-f]{64}\"", f"\\g<1>{sha}\"", s)
assert n == 1, "sha256"
s, n = re.subn(r"\n  bottle do\n.*?\n  end\n", "", s, flags=re.S)
assert n <= 1, "bottle"
sys.stdout.write(s)
' "$1" "$2"
}

cmd_brew_bump() {
	local ver=$1 url sha sha2 branch pr
	version_ok "$ver"
	identity
	url="https://github.com/$GH_REPO/archive/refs/tags/v$ver.tar.gz"
	sha=$(curl -fsSL "$url" | sha256sum | cut -d' ' -f1)
	sha2=$(curl -fsSL "$url" | sha256sum | cut -d' ' -f1)
	[ "$sha" = "$sha2" ] || die "the tarball changed between two downloads"
	branch=xcomm-$ver
	cd "$TAP_DIR"
	[ -z "$(git status --porcelain --untracked-files=no)" ] || die "$TAP_DIR has changes"
	git fetch -q origin
	git checkout -q -B "$branch" origin/main
	formula_bump "$ver" "$sha" < Formula/xcomm.rb > Formula/xcomm.rb.new
	mv Formula/xcomm.rb.new Formula/xcomm.rb
	git diff
	confirm "commit, push $branch and open the tap PR?"
	git add Formula/xcomm.rb
	GIT_AUTHOR_NAME=$NAME GIT_AUTHOR_EMAIL=$EMAIL GIT_COMMITTER_NAME=$NAME GIT_COMMITTER_EMAIL=$EMAIL \
		git commit -q -m "xcomm $ver"
	git push -q -u origin "$branch"
	pr=$(gh pr create -R "$TAP_REPO" --base main --head "$branch" --title "xcomm $ver" \
		--body "Bump xcomm to $ver: https://github.com/$GH_REPO/releases/tag/v$ver")
	say "$pr; next: release.sh brew-publish ${pr##*/}"
}

cmd_brew_publish() {
	local pr=$1 head id='' ver since checks=''
	[[ $pr =~ ^[0-9]+$ ]] || die "bad PR '$pr'"
	ver=$(gh pr view "$pr" -R "$TAP_REPO" --json title --jq .title | sed -n 's/^xcomm //p')
	version_ok "$ver"
	say "waiting for the bottles of PR $pr (xcomm $ver)"
	# The tap's workflow attaches its checks a little after the PR opens.
	for _ in $(seq 30); do
		checks=$(gh pr view "$pr" -R "$TAP_REPO" --json statusCheckRollup --jq '.statusCheckRollup | length')
		[ "$checks" -gt 0 ] && break
		sleep 10
	done
	[ "${checks:-0}" -gt 0 ] || die "PR $pr has no checks after 5 minutes"
	gh pr checks "$pr" -R "$TAP_REPO" --watch --interval 30 > /dev/null || die "the PR's checks failed: gh pr checks $pr -R $TAP_REPO"
	head=$(gh pr view "$pr" -R "$TAP_REPO" --json headRefOid --jq .headRefOid)
	confirm "publish the bottles of PR $pr (xcomm $ver, head ${head:0:7}) with brew pr-pull?"
	since=$(date -u +%FT%TZ)
	gh workflow run publish.yml -R "$TAP_REPO" -f pull_request="$pr" -f head_sha="$head"
	# The run this dispatch started: the first one created since.
	for _ in $(seq 30); do
		id=$(gh run list -R "$TAP_REPO" --workflow publish.yml --event workflow_dispatch --limit 5 \
			--json databaseId,createdAt --jq "[.[] | select(.createdAt >= \"$since\")] | last | .databaseId // empty")
		[ -n "$id" ] && break
		sleep 5
	done
	[ -n "$id" ] || die "no publish.yml run started since $since"
	gh run watch -R "$TAP_REPO" "$id" --exit-status --interval 30 > /dev/null || die "pr-pull failed: gh run view -R $TAP_REPO $id --log-failed"
	gh api "repos/$TAP_REPO/contents/Formula/xcomm.rb" --jq .content | base64 -d |
		grep -q "releases/download/xcomm-$ver\"" || die "main has no $ver bottles"
	say "xcomm $ver bottles are on main: brew update && brew upgrade xcomm"
}

[ $# -ge 1 ] || { sed -n '4,16p' "$0"; exit 2; }
step=$1; shift
case $step in
prepare) [ $# = 2 ] || die "prepare <ver> <date>"; cmd_prepare "$@" ;;
check) cmd_check ;;
commit) [ $# = 2 ] || die "commit <ver> <msg-file>"; cmd_commit "$@" ;;
push) [ $# = 1 ] || die "push <ver>"; cmd_push "$@" ;;
wait-ci) cmd_wait_ci ;;
publish) [ $# = 1 ] || die "publish <ver>"; cmd_publish "$@" ;;
gh-release) [ $# = 2 ] || die "gh-release <ver> <notes-file>"; cmd_gh_release "$@" ;;
binaries) [ $# = 1 ] || die "binaries <ver>"; cmd_binaries "$@" ;;
brew-bump) [ $# = 1 ] || die "brew-bump <ver>"; cmd_brew_bump "$@" ;;
brew-publish) [ $# = 1 ] || die "brew-publish <pr>"; cmd_brew_publish "$@" ;;
*) die "unknown step '$step'" ;;
esac
