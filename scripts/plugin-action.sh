#!/bin/sh
# plugin-action.sh <action>: an action of the xcomm herdr plugin. Pane
# actions act on the xcomm session in the pane they were invoked on; the
# outcome, or the error, is shown as a herdr notification.
set -u
xcomm=target/release/xcomm
herdr=${HERDR_BIN_PATH:-herdr}

say() {
	"$herdr" notification show "xcomm: $1" --body "$2" >/dev/null 2>&1
}

pane() {
	if ! p=$("$xcomm" ctl --pane-from-context 2>&1); then
		say "$1" "$p"
		exit 1
	fi
	echo "$p"
}

# A popup asking for what the action needs, which then runs it.
prompt() {
	run "$herdr" plugin pane open --plugin "$HERDR_PLUGIN_ID" --entrypoint prompt \
		--env "XCOMM_PROMPT=$1" --env "XCOMM_PANE=$2"
}

# After a --json status in $out: a read-only session would refuse the
# request, so say so now rather than after a prompt has been answered.
read_only() {
	case $out in
	*'"ctl":"ro"'*)
		say "$action" "xcomm: the control socket is read-only (--ctl ro)"
		exit 1
		;;
	esac
}

run() {
	if ! out=$("$@" 2>&1); then
		say "$action" "$out"
		exit 1
	fi
}

action=${1:?usage: plugin-action.sh <action>}
case $action in
open)
	run "$herdr" plugin pane open --plugin "$HERDR_PLUGIN_ID" --entrypoint picker
	;;
break)
	p=$(pane "$action") || exit 1
	run "$xcomm" ctl --pane "$p" break
	say "$action" "break sent"
	;;
dtr | rts)
	p=$(pane "$action") || exit 1
	run "$xcomm" ctl --pane "$p" "$action" toggle
	say "$action" "$out"
	;;
capture)
	p=$(pane "$action") || exit 1
	run "$xcomm" ctl --pane "$p" --json status
	read_only
	case $out in
	*'"capture":null'*) prompt capture "$p" ;;
	*)
		run "$xcomm" ctl --pane "$p" capture stop
		say "capture stopped" "$out"
		;;
	esac
	;;
baud | send-file | recv-file)
	p=$(pane "$action") || exit 1
	# Before the prompt, or a pane with no session (or a read-only one) would
	# say so only after the answer has been typed.
	run "$xcomm" ctl --pane "$p" --json status
	read_only
	prompt "$action" "$p"
	;;
watch-start | watch-stop | watch-status)
	run "$xcomm" watch "${action#watch-}"
	say "hotplug watcher" "$out"
	;;
*)
	say "$action" "unknown action"
	exit 2
	;;
esac
