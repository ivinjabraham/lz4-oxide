#!/usr/bin/env bash
# Compare fullbench-c vs fullbench-rust on selected block-codec algorithms.
# Reports best-of-N: run-to-run spread on this host is ~13%, so a single run
# cannot resolve anything smaller than that, and the max is the least
# noise-sensitive summary of a throughput measurement.
SP="$(cd "$(dirname "$0")" && pwd)"
FILE="${1:-$SP/d50.bin}"
ALGOS="${2:-c1 c9 d1 d4 d6}"
REPS="${3:-3}"

best() { # binary, algo, file
  local m best=0
  for _ in $(seq "$REPS"); do
    m=$(timeout 300 "$1" -i3 "-$2" "$3" 2>&1 | tr '\r' '\n' \
        | grep -oE '[0-9.]+ MB/s' | tail -1 | cut -d' ' -f1)
    [ -n "$m" ] && best=$(awk -v a="$best" -v b="$m" 'BEGIN{print (b>a)?b:a}')
  done
  echo "$best"
}

printf '%-38s %10s %10s %8s\n' "algorithm  ($(basename "$FILE"))" "C MB/s" "Rust MB/s" "ratio"
for a in $ALGOS; do
  name=$(timeout 300 "$SP/fullbench-c" -i1 "-$a" "$FILE" 2>&1 | tr '\r' '\n' \
         | grep 'MB/s' | tail -1 | sed 's/ *:.*//;s/^ *//')
  mc=$(best "$SP/fullbench-c"    "$a" "$FILE")
  mr=$(best "$SP/fullbench-rust" "$a" "$FILE")
  ratio=$(awk -v c="$mc" -v r="$mr" 'BEGIN{ if (c+0>0) printf "%.2fx", r/c; else print "n/a" }')
  printf '%-38s %10s %10s %8s\n' "$name" "$mc" "$mr" "$ratio"
done
