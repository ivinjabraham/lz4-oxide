# Benchmark Methodology

## Scope

This report compares the pinned C implementation of lz4 (`0774d055`) with the
Rust port. Both implementations use the same lz4 C harnesses and input corpus.
The Rust harnesses link `target/release/liblz4_rs.a`; `bench/rebuild.sh` forces
the relink and runs `make provenance-check` so cached C objects cannot be
mistakenly measured as Rust.

Machine-readable measured values are in [`results.json`](results.json).

## Environment

- Date: 2026-08-03
- Host: x86-64 Linux 6.12.86
- Upstream: lz4 `0774d05537f9762f838f7ab541b7765f1a729cb5`
- Port artifact: `target/release/liblz4_rs.a`
- Port provenance: all six checked test binaries compiled from `cstub/`

## Throughput

`fullbench` is compiled once against each implementation and run on the same
8 MB input. Reported throughput is the best of three runs, each with three
inner iterations. The primary corpus is `upstream/tests/datagen -g8M -P50`.
Additional corpus runs use `-P10`, `-P50`, `-P90`, and an 8 MB all-zero file.

Run-to-run spread on the development host was about 13%; differences below
15% are treated as noise. `datagen -P<n>` controls match probability rather
than compression ratio, so the all-zero file is included as the long-match
case.

```sh
bench/rebuild.sh
upstream/tests/datagen -g8M -P50 > /tmp/lz4-oxide-bench/d50.bin
bench/bench.sh /tmp/lz4-oxide-bench/d50.bin "c1 c4 d1 d4"
```

## Latency, RSS, and startup

Per-call latency uses 1 MB input and ten `fullbench` iterations. RSS is the
peak resident memory while each CLI compresses an 8 MB `-P50` input. Startup
uses 20 trials compressing `/dev/null`; results report min, p50, p99, and max.
