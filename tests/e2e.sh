#!/usr/bin/env bash
# Drives a real zsh in an isolated tmux server and prints the prompt line
# after each step. Usage: tests/e2e.sh [big-repo-dir]
set -u
B=${ASHELL_BIN:-$(cd "$(dirname "$0")/.." && pwd)/target/release/ashell}
BIG=${1:-}
WORK=$(mktemp -d "${TMPDIR:-/tmp}/ashell-e2e.XXXX")
LOGD=$(mktemp -d "${TMPDIR:-/tmp}/ashell-e2e-log.XXXX")
ZLOG=$LOGD/zsh.log
t() { tmux -L ashell-test "$@"; }
line() { t capture-pane -p -t e2e | grep -v '^$' | tail -1; }
step() { t send-keys -t e2e "$1" Enter; sleep "$2"; printf '%-34s | %s\n' "$3" "$(line)"; }
cleanup() { t kill-server 2>/dev/null; "$B" stop 2>/dev/null; rm -rf "$WORK" "$LOGD"; }
trap cleanup EXIT

git -C "$WORK" init -q && touch "$WORK/a" && git -C "$WORK" add a &&
  git -C "$WORK" -c user.name=t -c user.email=t@t commit -qm init

"$B" stop 2>/dev/null
t new-session -d -s e2e -x 110 -y 30 "zsh -f"
sleep 0.5
t send-keys -t e2e "eval \"\$($B init zsh)\"" Enter
sleep 0.5
# Timestamp prompts and applied frames zsh-side: polling tmux is too coarse.
t send-keys -t e2e "_l() { print -r -- \"\$EPOCHREALTIME \$1 \$_ashell_left\" >> $ZLOG }; functions[_of]=\$functions[_ashell_on_frame]; _ashell_on_frame() { _of \"\$@\"; _l frame }; _lp() { _l prompt }; precmd_functions+=(_lp); clear" Enter
sleep 0.3
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
pid1=$(pgrep -f "ashell daemon")
touch "$B"; sleep 1.5
step "true" 0.4 "binary replaced -> fallback"
step "true" 0.4 "new daemon"
pid2=$(pgrep -f "ashell daemon")
printf '%-34s | %s\n' "daemon pid before/after upgrade" "$pid1 -> $pid2"

# Big repo: time until the async git status patch replaces the placeholder.
waitpatch() {
  local start=$(perl -MTime::HiRes=time -e 'printf "%.3f", time')
  t send-keys -t e2e "$1" Enter
  sleep 0.01; printf '%-34s | %s\n' "$2, +10ms" "$(line)"
  case "$(line)" in *…*) ;; *) printf '%-34s | %s\n' "$2: instant from cache" ""; return ;; esac
  for _ in $(seq 250); do
    case "$(line)" in *…*) sleep 0.02 ;; *) break ;; esac
  done
  local end=$(perl -MTime::HiRes=time -e 'printf "%.3f", time')
  printf '%-34s | %s\n' "$2, patched after $(echo "($end-$start)*1000/1" | bc)ms" "$(line)"
}
# Runs `cmd`, waits for the prompt to contain `pattern`, and reports when
# zsh showed it relative to the prompt (from the zsh-side log).
waitfor() {
  : > "$ZLOG"
  t send-keys -t e2e "$1" Enter
  for _ in $(seq 100); do
    case "$(line)" in *"$2"*) break ;; *) sleep 0.03 ;; esac
  done
  local when=$(sed -E 's/\x1b\[[0-9;]*m//g; s/%[{}]//g' "$ZLOG" | awk -v pat="$2" '
    $2 == "prompt" { t0 = $1; first = (index($0, pat) > 0); if (first) { print "in first frame"; exit } next }
    t0 && index($0, pat) { printf "+%.1fms after prompt", ($1 - t0) * 1000; exit }')
  printf '%-34s | %s\n' "$3: ${when:-never}" "$(line)"
}
if [ -n "$BIG" ]; then
  waitpatch "cd $BIG" "big repo cold"
  waitpatch "cd - >/dev/null; cd $BIG" "big repo revisit"
  # While the native cache warms up (aports: ~1.5s of readdir), git answers.
  waitfor "touch .ashell-e2e-probe" '?1 ' "big repo new file (warming up)"
  waitfor "rm .ashell-e2e-probe" 'master ❯' "big repo file removed (warming up)"
  sleep 4
  # Warm: watcher-driven incremental native status.
  for i in 1 2 3; do
    waitfor "touch .ashell-e2e-probe" '?1 ' "big repo new file #$i"
    waitfor "rm .ashell-e2e-probe" 'master ❯' "big repo file removed #$i"
  done
  waitfor "echo >> README.md" '!1 ' "big repo edit tracked"
  waitfor "git add README.md" '+1 ' "big repo git add"
  waitfor "git reset -q --hard" 'master ❯' "big repo reset --hard"
fi
