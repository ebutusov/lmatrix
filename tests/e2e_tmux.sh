#!/usr/bin/env bash
# End-to-end check (specs.md 12.3): runs lmatrix in a detached tmux session
# and inspects the screen with `tmux capture-pane -p`.
set -u
cd "$(dirname "$0")/.."
cargo build -q --bin lmatrix --example panic_restore || exit 1
BIN="$PWD/target/debug/lmatrix"
PANIC_BIN="$PWD/target/debug/examples/panic_restore"
SESSION="lmatrix-e2e-$$"
WORK="$(mktemp -d)"
trap 'tmux kill-session -t "$SESSION" 2>/dev/null; rm -rf "$WORK"' EXIT
failures=0

pass() { printf 'ok    %s\n' "$1"; }
fail() { printf 'FAIL  %s\n' "$1"; failures=$((failures + 1)); }
screen() { tmux capture-pane -t "$SESSION" -p; }
row() { screen | sed -n "${1}p"; }
check() { if eval "$2"; then pass "$1"; else fail "$1"; screen | sed 's/^/      | /' | head -30; fi; }
restored() {
	[ "$(tmux display -p -t "$SESSION" '#{alternate_on}')" = 0 ] &&
		[ "$(tmux display -p -t "$SESSION" '#{cursor_flag}')" = 1 ]
}
shell_usable() {
	tmux send-keys -t "$SESSION" "clear; echo e2e-prompt-\$((40 + 2))" Enter
	sleep 0.5
	screen | grep -q 'e2e-prompt-42'
}
start() {
	tmux send-keys -t "$SESSION" "clear; $1" Enter
	sleep "${2:-2}"
}

tmux new-session -d -s "$SESSION" -x 80 -y 24 "env -i HOME=\"$HOME\" PATH=\"$PATH\" TERM=xterm-256color bash --norc --noprofile"
tmux resize-window -t "$SESSION" -x 80 -y 24 2>/dev/null
sleep 0.5

# Demo: help bar on the first row, backlog window on the last two.
start "$BIN --demo" 2.5
check "demo shows help bar on row 1" '[[ "$(row 1)" == *"[enter] tail"*"[q] quit"* ]]'
check "demo shows backlog window on rows 23-24" '[[ "$(row 23)" =~ ^\ [0-9]{2}:[0-9]{2}:[0-9]{2}\  && "$(row 24)" =~ ^\ [0-9]{2}:[0-9]{2}:[0-9]{2}\  ]]'
check "rain area is not empty" '[ "$(screen | sed -n 2,22p | tr -d " \n" | wc -c)" -gt 20 ]'

# Enter switches to tail mode and back.
tmux send-keys -t "$SESSION" Enter; sleep 0.5
check "Enter switches to tail mode" '[[ "$(row 1)" == " TAIL"* ]]'
tmux send-keys -t "$SESSION" Enter; sleep 0.5
check "Enter switches back to rain mode" '[[ "$(row 1)" == " [enter] tail"* ]]'

# q restores the terminal.
tmux send-keys -t "$SESSION" q; sleep 0.7
check "q restores the terminal" 'restored'
check "shell usable after q" 'shell_usable'

# An ack rule matched by a command source.
cat > "$WORK/ack.toml" <<'TOML'
[[source]]
type = "command"
run = "echo 'kernel: Out of memory: Killed process 4312 (java)'; sleep 3600"
restart = false

[[rule]]
name = "oom"
pattern = "(?i)out of memory"
color = "#ff463c"
hold = "ack"
TOML
start "$BIN --config $WORK/ack.toml" 2.5
check "ack rule shows 'ack 1'" '[[ "$(row 1)" == *"ack 1" ]]'
check "caught line decoded on screen" 'screen | grep -q "Out of memory: Killed process 4312 (java) \[a\]\|Out of memory: Killed process 4312 (java)"'
tmux send-keys -t "$SESSION" a; sleep 0.5
check "'a' clears the ack count" '[[ "$(row 1)" != *"ack 1"* ]]'
tmux send-keys -t "$SESSION" q; sleep 0.7
check "child command terminated on quit" '! pgrep -f "sleep 3600" >/dev/null'

# A remote journald source, with a fake ssh that runs the remote command
# locally through sh (so the quoting is checked too). Needs a readable
# journal and logger; skipped otherwise.
if command -v logger >/dev/null && journalctl -q -n 0 >/dev/null 2>&1; then
	mkdir -p "$WORK/bin"
	cat > "$WORK/bin/ssh" <<SHIM
#!/bin/sh
printf '%s\n' "\$*" >> "$WORK/ssh-calls"
while [ \$# -gt 0 ] && [ "\$1" != "--" ]; do shift; done
shift 2
exec sh -c "\$1"
SHIM
	chmod +x "$WORK/bin/ssh"
	MARK="e2e-remote-$$"
	cat > "$WORK/remote.toml" <<TOML
[[source]]
type = "journald"
host = "e2e-host"

[[rule]]
name = "marker"
pattern = "$MARK-(old|new)"
color = "#ff463c"
hold = "ack"
TOML
	logger -t lmatrix-e2e "$MARK-old"; sleep 1
	start "PATH=$WORK/bin:\$PATH $BIN --config $WORK/remote.toml" 3
	check "remote: backlog line is not decoded" '[[ "$(row 1)" != *"ack 1"* ]]'
	logger -t lmatrix-e2e "$MARK-new"; sleep 2
	check "remote: live line is decoded" '[[ "$(row 1)" == *"ack 1" ]]'
	check "remote: backlog read first, then followed" '[[ "$(sed -n 1p "$WORK/ssh-calls")" == *"-- e2e-host journalctl '"'--no-pager'"'"* && "$(sed -n 2p "$WORK/ssh-calls")" == *"-- e2e-host journalctl '"'-f'"'"* ]]'
	tmux send-keys -t "$SESSION" Enter; sleep 0.3
	tmux send-keys -t "$SESSION" / "$MARK" Enter; sleep 0.5
	check "remote: both lines in tail mode" 'screen | grep -q "lmatrix-e2e.*$MARK-old" && screen | grep -q "lmatrix-e2e.*$MARK-new"'
	tmux send-keys -t "$SESSION" q; sleep 0.7
	check "remote: journalctl stopped on quit" '! pgrep -f "journalctl .-f. .-o. .json." >/dev/null'
else
	echo "skip  remote journald (no readable journal or no logger)"
fi

# SIGTERM restores the terminal.
start "$BIN --demo" 1.5
pkill -TERM -x lmatrix; sleep 0.7
check "SIGTERM restores the terminal" 'restored'
check "shell usable after SIGTERM" 'shell_usable'

# SIGHUP restores the terminal.
start "$BIN --demo" 1.5
pkill -HUP -x lmatrix; sleep 0.7
check "SIGHUP restores the terminal" 'restored'

# Ctrl-C restores the terminal.
start "$BIN --demo" 1.5
tmux send-keys -t "$SESSION" C-c; sleep 0.7
check "Ctrl-C restores the terminal" 'restored'
check "shell usable after Ctrl-C" 'shell_usable'

# A panic restores the terminal.
start "$PANIC_BIN 2>$WORK/panic.txt" 1.5
check "panic restores the terminal" 'restored && grep -q "deliberate panic" "$WORK/panic.txt" && ! grep -q "worker" "$WORK/panic.txt"'
check "shell usable after panic" 'shell_usable'

# Escape sequences in log lines cannot reach the terminal.
cat > "$WORK/esc.toml" <<'TOML'
[[source]]
type = "command"
run = "printf 'evil \\033]0;HACKED\\007 \\033[?1049l \\033[2J title\\n'; sleep 3600"
restart = false
TOML
start "$BIN --config $WORK/esc.toml" 2
tmux send-keys -t "$SESSION" Enter; sleep 0.5
check "escape sequences are stripped" 'screen | grep -q "evil    title" && [ "$(tmux display -p -t "$SESSION" "#{pane_title}")" != HACKED ] && [ "$(tmux display -p -t "$SESSION" "#{alternate_on}")" = 1 ]'
tmux send-keys -t "$SESSION" q; sleep 0.5

# Too small, then resize back.
start "$BIN --demo" 1.5
tmux resize-window -t "$SESSION" -x 19 -y 10; sleep 0.5
check "too-small message" 'screen | grep -q "terminal too small\|too small"'
tmux resize-window -t "$SESSION" -x 100 -y 30; sleep 0.8
check "redraws after growing" '[[ "$(row 1)" == *"[enter] tail"* ]] && [[ "$(row 30)" =~ ^\ [0-9]{2}: ]]'
tmux send-keys -t "$SESSION" q; sleep 0.5

# Closing the terminal under lmatrix (dropped SSH session, killed pane)
# must end the process instead of leaving it spinning.
GONE="$SESSION-gone"
tmux new-session -d -s "$GONE" -x 80 -y 24 "$BIN --demo"
sleep 1.5
GONE_PID="$(tmux display -p -t "$GONE" '#{pane_pid}')"
tmux kill-session -t "$GONE"
for _ in $(seq 1 20); do kill -0 "$GONE_PID" 2>/dev/null || break; sleep 0.1; done
check "exits when its terminal is closed" '! kill -0 "$GONE_PID" 2>/dev/null || { kill -KILL "$GONE_PID"; false; }'

if [ "$failures" -eq 0 ]; then echo "all e2e checks passed"; else echo "$failures e2e check(s) failed"; fi
exit $(( failures > 0 ))
