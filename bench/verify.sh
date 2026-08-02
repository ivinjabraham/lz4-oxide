#!/usr/bin/env bash
# Byte-identity check: every fuzz/ harness, compiled against the pinned C
# library and against the Rust port, over a spread of sizes/compressibilities.
# Any divergence here is a parse difference the round-trip tests cannot see.
set -u
# Resolve our own location BEFORE cd-ing, and keep every artefact in $SP
# rather than the repo -- $0 is relative, so resolving it afterwards used to
# scatter test binaries and samples into whatever directory this ran from.
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SP="${LZ4_BENCH_WORK:-${TMPDIR:-/tmp}/lz4-oxide-bench}"
mkdir -p "$SP"
cd "$ROOT" || exit 1
LIBS="-lgcc_s -lutil -lrt -lpthread -lm -ldl -lc"

make -C upstream/lib liblz4.a >/dev/null 2>&1
make -C upstream/tests datagen >/dev/null 2>&1

for h in difftest stream_difftest framediff; do
  gcc -O2 -I upstream/lib "fuzz/$h.c" upstream/lib/liblz4.a           -o $SP/$h-c  || exit 1
  gcc -O2 -I upstream/lib "fuzz/$h.c" target/release/liblz4_rs.a $LIBS -o $SP/$h-rs || exit 1
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

# Rejection parity at tiny output capacities.
#
# The two-stage decode shortcut has a 32-byte output margin. C tests it as a
# pointer comparison that is simply false on a smaller block; expressing that
# with saturating_sub instead made it *true* at op == 0 and let the shortcut run
# on buffers too small for it. Round-trips cannot see this class of bug -- it
# only shows up when the capacity is near or below the margin -- so sweep the
# boundary directly and compare the exact return code (position-encoded on
# error, so it says *where* each side gave up) plus a hash of the bytes produced.
BIN=difftest
for size in 200 4K 60K; do
  upstream/tests/datagen -g$size -P50 > "$SP/in" 2>/dev/null
  "$SP/difftest-c" c < "$SP/in" > "$SP/cmp.bin" 2>/dev/null
  for variant in clean corrupt-hdr corrupt-mid corrupt-tail truncated; do
    python3 - "$SP/cmp.bin" "$SP/case.bin" "$variant" <<'EOF'
import sys
data = bytearray(open(sys.argv[1], 'rb').read())
which = sys.argv[3]
if data:
    if which == 'corrupt-hdr':  data[0] ^= 0xFF
    elif which == 'corrupt-mid': data[len(data)//2] ^= 0xFF
    elif which == 'corrupt-tail': data[-1] ^= 0xFF
    elif which == 'truncated':   data = data[:max(1, len(data)//2)]
open(sys.argv[2], 'wb').write(bytes(data))
EOF
    for cap in 1 2 3 4 5 6 7 8 11 12 15 16 17 18 19 20 24 31 32 33 40 64 4096; do
      run "partial  $variant g$size cap=$cap"        "$SP/case.bin" q "$cap" "$cap"
      run "partial  $variant g$size cap=$cap t=1"    "$SP/case.bin" q "$cap" 1
      run "safe     $variant g$size cap=$cap"        "$SP/case.bin" x "$cap"
    done
  done
done

echo "byte-identical: $pass    diverged: $fail"
exit $((fail > 0))
