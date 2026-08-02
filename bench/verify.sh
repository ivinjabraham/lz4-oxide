#!/usr/bin/env bash
# Byte-identity check: every fuzz/ harness, compiled against the pinned C
# library and against the Rust port, over a spread of sizes/compressibilities.
# Any divergence here is a parse difference the round-trip tests cannot see.
set -u
cd /home/ivin/lz4-oxide
SP="$(cd "$(dirname "$0")" && pwd)"
LIBS="-lgcc_s -lutil -lrt -lpthread -lm -ldl -lc"

make -C upstream/lib liblz4.a >/dev/null 2>&1
make -C upstream/tests datagen >/dev/null 2>&1

for h in difftest stream_difftest framediff; do
  gcc -O2 -I upstream/lib fuzz/$h.c upstream/lib/liblz4.a           -o $SP/$h-c  || exit 1
  gcc -O2 -I upstream/lib fuzz/$h.c target/release/liblz4_rs.a $LIBS -o $SP/$h-rs || exit 1
done

fail=0; pass=0
run() { # name, stdin-file, args...
  local name=$1 infile=$2; shift 2
  # Each invocation gets its OWN redirect. Sharing one stdin lets the first
  # binary drain the file and hands the second an empty input, which compares
  # equal and proves nothing.
  "$SP/$BIN-c"  "$@" < "$infile" > "$SP/o-c"  2>/dev/null
  "$SP/$BIN-rs" "$@" < "$infile" > "$SP/o-rs" 2>/dev/null
  if cmp -s "$SP/o-c" "$SP/o-rs"; then pass=$((pass+1));
  else fail=$((fail+1)); echo "  DIVERGED: $name"; fi
}

# difftest: block codec, all modes, across table types and compressibilities.
BIN=difftest
for size in 1K 60K 100K 1M 4M; do
  for p in 10 50 90; do
    upstream/tests/datagen -g$size -P$p > "$SP/in" 2>/dev/null
    for m in c i r d p x q; do
      run "difftest -$m  g$size P$p" "$SP/in" "$m"
    done
  done
done

# stream_difftest: streaming + dictionary transcripts, incl. internal state.
BIN=stream_difftest
run "stream_difftest" /dev/null

# framediff: frame format, all 8 preference combinations.
BIN=framediff
for size in 1K 64K 200K 1M 4M; do
  for p in 10 50 90; do
    upstream/tests/datagen -g$size -P$p > "$SP/in" 2>/dev/null
    for m in 0 1 2 3 4 5 6 7; do
      run "framediff mode$m g$size P$p" "$SP/in" "$m"
    done
  done
done

echo "byte-identical: $pass    diverged: $fail"
exit $((fail > 0))
