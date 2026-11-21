# ashell zsh integration. Zero forks per prompt: talks to the daemon over a
# unix socket with zsh builtins, repaints asynchronously via zle -F.
# Usage: eval "$(ashell init zsh)"

[[ -o interactive ]] || return 0
zmodload zsh/net/socket zsh/system zsh/datetime 2>/dev/null || return 0
autoload -Uz add-zsh-hook

typeset -g _ashell_bin=@ASHELL_BIN@ _ashell_sock=@ASHELL_SOCK@
typeset -g _ashell_fd= _ashell_buf= _ashell_frame= _ashell_left= _ashell_right= _ashell_t0=
typeset -g _ashell_us=$'\x1f' _ashell_rs=$'\x1e'
typeset -gi _ashell_seq=0
typeset -ga _ashell_env

setopt prompt_subst
PROMPT='${_ashell_left}'
RPROMPT='${_ashell_right}'

_ashell_disconnect() {
  [[ -n $_ashell_fd ]] || return 0
  zle -F $_ashell_fd 2>/dev/null
  exec {_ashell_fd}<&-
  _ashell_fd= _ashell_buf=
}

# Pops one record into $_ashell_frame. $1: timeout in seconds.
# Returns sysread's status on failure (4 timeout, 5 eof).
_ashell_next_frame() {
  local chunk rc
  while [[ $_ashell_buf != *$_ashell_rs* ]]; do
    sysread -i $_ashell_fd -t $1 chunk || { rc=$?; return $(( rc ? rc : 1 )) }
    _ashell_buf+=$chunk
  done
  _ashell_frame=${_ashell_buf%%$_ashell_rs*}
  # Not ${buf#*$rs}: a leading-* shortest match is quadratic in zsh
  # (19µs for a short frame, 55µs for a long one).
  _ashell_buf=${_ashell_buf[${#_ashell_frame}+2,-1]}
}

_ashell_send() {
  setopt local_options local_traps
  trap '' PIPE
  syswrite -o $_ashell_fd -- "$1$_ashell_rs"  # fails silently (rc 2) on a dead daemon
}

_ashell_connect() {
  zsocket $_ashell_sock 2>/dev/null || return 1
  _ashell_fd=$REPLY _ashell_buf=
  _ashell_send "H${_ashell_us}1${_ashell_us}zsh" && _ashell_next_frame 1 || { _ashell_disconnect; return 1 }
  local -a p=("${(@ps:$_ashell_us:)_ashell_frame}")
  [[ $p[1] == H ]] || { _ashell_disconnect; return 1 }
  _ashell_env=("${(@)p[3,-1]}")
  zle -Fw $_ashell_fd _ashell_on_frame 2>/dev/null
  return 0
}

# Applies $_ashell_frame if it belongs to the current prompt.
_ashell_apply() {
  local -a p=("${(@ps:$_ashell_us:)_ashell_frame}")
  [[ $p[1] == F && $p[2] == $_ashell_seq ]] || return 1
  _ashell_left=$p[4] _ashell_right=$p[5]
}

_ashell_oneshot() {
  local out
  out=$("$_ashell_bin" prompt --shell zsh --status $1 ${2:+--duration $2} --keymap ${KEYMAP:-main} 2>/dev/null)
  _ashell_left=${out%%$_ashell_us*} _ashell_right=${out#*$_ashell_us}
  "$_ashell_bin" daemon --detach --socket $_ashell_sock &>/dev/null
}

_ashell_preexec() { _ashell_t0=$EPOCHREALTIME }

_ashell_precmd() {
  local -i st=$?
  local dur= v
  if [[ -n $_ashell_t0 ]]; then
    local -i ms=$(( (EPOCHREALTIME - _ashell_t0) * 1000 ))
    dur=$ms _ashell_t0=
  fi
  (( ++_ashell_seq ))
  if [[ -z $_ashell_fd ]] && ! _ashell_connect; then
    _ashell_oneshot $st $dur; return
  fi
  local req="P${_ashell_us}${_ashell_seq}${_ashell_us}${PWD}${_ashell_us}${COLUMNS}${_ashell_us}${st}${_ashell_us}${dur}${_ashell_us}${KEYMAP:-main}"
  for v in $_ashell_env; do req+="${_ashell_us}${(P)v}"; done
  if _ashell_send "$req"; then
    while _ashell_next_frame 0.5; do _ashell_apply && return; done
  fi
  _ashell_disconnect
  _ashell_oneshot $st $dur
}

# zle -F widget: patch frames from slow segments, or daemon hangup.
_ashell_on_frame() {
  [[ -z $2 ]] || { _ashell_disconnect; return }
  local -i changed=0 rc
  while true; do
    _ashell_next_frame 0; rc=$?
    (( rc == 0 )) || break
    _ashell_apply && changed=1
  done
  (( rc == 5 )) && _ashell_disconnect
  (( changed )) && zle reset-prompt
}
zle -N _ashell_on_frame

add-zsh-hook preexec _ashell_preexec
add-zsh-hook precmd _ashell_precmd
# end of ashell init: trailing text pasted after the eval lands in this comment
