#!/usr/bin/env bash
# Drives a real zsh in an isolated tmux server and prints the prompt line
# after each step. Usage: tests/e2e.sh [big-repo-dir]
set -u
B=${ASHELL_BIN:-$(cd "$(dirname "$0")/.." && pwd)/target/release/ashell}
BIG=${1:-}
WORK=$(mktemp -d "${TMPDIR:-/tmp}/ashell-e2e.XXXX")
t() { tmux -L ashell-test "$@"; }
line() { t capture-pane -p -t e2e | grep -v '^$' | tail -1; }
step() { t send-keys -t e2e "$1" Enter; sleep "$2"; printf '%-34s | %s\n' "$3" "$(line)"; }
cleanup() { t kill-server 2>/dev/null; "$B" stop 2>/dev/null; rm -rf "$WORK"; }
trap cleanup EXIT

git -C "$WORK" init -q && touch "$WORK/a" && git -C "$WORK" add a &&
  git -C "$WORK" -c user.name=t -c user.email=t@t commit -qm init

"$B" stop 2>/dev/null
t new-session -d -s e2e -x 110 -y 30 "zsh -f"
sleep 0.5
t send-keys -t e2e "eval \"\$($B init zsh)\"" Enter
sleep 0.5
step "clear" 0.3 "first prompts (daemon spawned)"
step "cd $WORK" 0.3 "cd clean repo"
step "touch b; git add b" 0.4 "stage a file"
step "echo x >> a" 0.4 "modify tracked file"
step "touch c; false" 0.4 "untracked + exit 1"
step "sleep 2.1" 2.5 "duration"
step "git stash -q -u" 0.4 "stash"
pkill -f "ashell daemon" ; sleep 0.2
step "true" 0.4 "daemon killed -> fallback"
step "true" 0.4 "reconnected"
if [ -n "$BIG" ]; then
  t send-keys -t e2e "cd $BIG" Enter
  sleep 0.01; printf '%-34s | %s\n' "big repo, +10ms" "$(line)"
  sleep 0.5;  printf '%-34s | %s\n' "big repo, +500ms (patched)" "$(line)"
fi
