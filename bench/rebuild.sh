#!/usr/bin/env bash
set -e
source "$HOME/.cargo/env"
cd "$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SP="${LZ4_BENCH_WORK:-${TMPDIR:-/tmp}/lz4-oxide-bench}"
mkdir -p "$SP"
cargo build --release >/dev/null
rm -f upstream/tests/cachedObjs/*/fullbench upstream/tests/fullbench
make -C upstream/tests fullbench \
  C_SRCDIRS="$PWD/cstub $PWD/upstream/programs ." \
  LDLIBS="$PWD/target/release/liblz4_rs.a -lgcc_s -lutil -lrt -lpthread -lm -ldl -lc" >/dev/null 2>&1
make provenance-check | tail -1
cp "$(readlink -f upstream/tests/fullbench)" "$SP/fullbench-rust"
