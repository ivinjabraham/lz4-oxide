#!/usr/bin/env bash
# Differential verification: compile every harness twice, against the pinned C
# library and against the Rust port, then compare their output byte-for-byte.
#
# The matrix covers block modes, streaming/dictionary state, frame preferences,
# HC levels 1-12, and malformed-input rejection at boundary capacities.
set -u

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SP="${LZ4_BENCH_WORK:-${TMPDIR:-/tmp}/lz4-oxide-difftest}"
mkdir -p "$SP"
cd "$ROOT" || exit 1

LIBS="-lgcc_s -lutil -lrt -lpthread -lm -ldl -lc"
pass=0
fail=0

make -C upstream/lib liblz4.a >/dev/null 2>&1 || exit 1
make >/dev/null || exit 1
make -C upstream/tests datagen >/dev/null 2>&1 || exit 1

for harness in difftest stream_difftest framediff hc_difftest; do
    gcc -O2 -I upstream/lib "fuzz/$harness.c" upstream/lib/liblz4.a \
        -o "$SP/$harness-c" || exit 1
    gcc -O2 -I upstream/lib "fuzz/$harness.c" target/release/liblz4_rs.a $LIBS \
        -o "$SP/$harness-rs" || exit 1
done

run() { # label, harness, stdin file, arguments...
    local label=$1 harness=$2 infile=$3
    shift 3
    "$SP/$harness-c" "$@" < "$infile" > "$SP/o-c" 2>/dev/null
    "$SP/$harness-rs" "$@" < "$infile" > "$SP/o-rs" 2>/dev/null
    if cmp -s "$SP/o-c" "$SP/o-rs"; then
        pass=$((pass + 1))
    else
        fail=$((fail + 1))
        echo "DIVERGED: $label"
    fi
}

# Block codec: all modes, table types, and input compressibilities.
for size in 1K 60K 100K 1M 4M; do
    for probability in 10 50 90; do
        upstream/tests/datagen -g"$size" -P"$probability" > "$SP/in" 2>/dev/null
        for mode in c i r d p x q; do
            run "difftest mode=$mode size=$size P=$probability" difftest "$SP/in" "$mode"
        done
    done
done

# Streaming and dictionary transcripts, including internal index state.
run "stream_difftest" stream_difftest /dev/null

# Frame format: all preference combinations across the input matrix.
for size in 1K 64K 200K 1M 4M; do
    for probability in 10 50 90; do
        upstream/tests/datagen -g"$size" -P"$probability" > "$SP/in" 2>/dev/null
        for mode in 0 1 2 3 4 5 6 7; do
            run "framediff mode=$mode size=$size P=$probability" framediff "$SP/in" "$mode"
        done
    done
done

# HC levels exercise lz4mid, hash-chain, and optimal-parser strategies.
for level in {1..12}; do
    for size in 20K 300K 1M; do
        for probability in 10 50 90; do
            upstream/tests/datagen -g"$size" -P"$probability" > "$SP/in" 2>/dev/null
            run "hc level=$level size=$size P=$probability" hc_difftest "$SP/in" "$level"
        done
    done
done

# Rejection parity near the fast decoder's 32-byte output margin.
for size in 200 4K 60K; do
    upstream/tests/datagen -g"$size" -P50 > "$SP/in" 2>/dev/null
    "$SP/difftest-c" c < "$SP/in" > "$SP/cmp.bin" 2>/dev/null
    for variant in clean corrupt-hdr corrupt-mid corrupt-tail truncated; do
        python3 - "$SP/cmp.bin" "$SP/case.bin" "$variant" <<'EOF'
import sys

data = bytearray(open(sys.argv[1], "rb").read())
variant = sys.argv[3]
if data:
    if variant == "corrupt-hdr":
        data[0] ^= 0xFF
    elif variant == "corrupt-mid":
        data[len(data) // 2] ^= 0xFF
    elif variant == "corrupt-tail":
        data[-1] ^= 0xFF
    elif variant == "truncated":
        data = data[:max(1, len(data) // 2)]
open(sys.argv[2], "wb").write(data)
EOF
        for capacity in 1 2 3 4 5 6 7 8 11 12 15 16 17 18 19 20 24 31 32 33 40 64 4096; do
            run "partial $variant size=$size cap=$capacity" difftest "$SP/case.bin" q "$capacity" "$capacity"
            run "partial $variant size=$size cap=$capacity target=1" difftest "$SP/case.bin" q "$capacity" 1
            run "safe $variant size=$size cap=$capacity" difftest "$SP/case.bin" x "$capacity"
        done
    done
done

echo "byte-identical: $pass    diverged: $fail"
exit $((fail > 0))
