#!/bin/zsh -f
# Per-prompt cost as the shell sees it, measured inside zsh.
# Usage: bench/precmd.zsh <dir> [iterations]
zmodload zsh/datetime
dir=${1:?dir} n=${2:-1000}
ashell=${ASHELL_BIN:-${0:A:h:h}/target/release/ashell}
cd $dir || exit 1

bench() {  # name, iterations, code
  local -F t0=$EPOCHREALTIME
  repeat $2 eval $3
  printf '%-34s %9.1f µs/prompt\n' $1 $(( (EPOCHREALTIME - t0) * 1e6 / $2 ))
}

setopt interactive 2>/dev/null
eval "$($ashell init zsh)"
_ashell_precmd; "$ashell" daemon --detach --socket $_ashell_sock; sleep 0.3
_ashell_precmd; sleep 1; _ashell_precmd   # connected, git status cached

bench "ashell daemon (zsh socket)" $n '_ashell_precmd'
bench "fork baseline \$(true)" 200 'x=$(/usr/bin/true)'
bench "ashell one-shot \$(ashell prompt)" 20 'x=$($ashell prompt --shell zsh)'
(( $+commands[starship] )) && bench "starship \$(starship prompt)" 10 'x=$(starship prompt)'
