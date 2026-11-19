#!/usr/bin/env bash
# Compares native (gitoxide) vs `git status` counts on every repo found under
# the given roots. Read-only. Usage: tests/git-diff.sh <root>...
B=${ASHELL_BIN:-$(cd "$(dirname "$0")/.." && pwd)/target/release/ashell}
pass=0 fail=0
printf '%-40s %10s %10s  %s\n' repo native_ms cli_ms result
while read -r gitdir; do
  repo=$(dirname "$gitdir")
  n=$(cd "$repo" && "$B" git-status --backend native ${WARM:+--warm} 2>&1)
  c=$(cd "$repo" && "$B" git-status --backend cli 2>&1)
  nt=${n##*took_us=} ct=${c##*took_us=}
  if [ "${n% took_us=*}" = "${c% took_us=*}" ]; then r=ok; pass=$((pass+1)); else
    r="DIFF native[${n% took_us=*}] cli[${c% took_us=*}]"; fail=$((fail+1)); fi
  printf '%-40s %10.1f %10.1f  %s\n' "$(basename "$repo")" "$(echo "$nt/1000" | bc -l)" "$(echo "$ct/1000" | bc -l)" "$r"
done < <(for r in "$@"; do fd -HI -d 3 --prune '^\.git$' "$r"; done)
echo "pass=$pass fail=$fail"
