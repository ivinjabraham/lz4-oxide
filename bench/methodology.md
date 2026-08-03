# Benchmark Methodology

This directory contains the reproducible method, machine-readable data, and
benchmark tooling for throughput measurements against the pinned C library.
Differential byte-identity and rejection-parity checks live in
[`../fuzz/driver.sh`](../fuzz/driver.sh); analysis and limitations live in
[DECISIONS.md §8.4](../DECISIONS.md).

Machine-readable measured values are in [`results.json`](results.json).

## Scope

This report compares the pinned C implementation of lz4 (`0774d055`) with the
Rust port. Both implementations use the same lz4 C harnesses and input corpus.
The Rust harnesses link `target/release/liblz4_rs.a`; `bench/rebuild.sh` forces
the relink and runs `make provenance-check` so cached C objects cannot be
mistakenly measured as Rust.

## Tooling

| script | what it does |
|---|---|
| `rebuild.sh` | Rebuild `fullbench` against the port. Forces the relink and runs `provenance-check` — `tests/Makefile` does not list our archive as a prerequisite, so without this you benchmark a stale binary, and a cached C object relinks silently. |
| `bench.sh` | `fullbench` C vs Rust on selected algorithms. `bench.sh <file> "<algos>" [reps]`. |

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

```sh
bench/rebuild.sh
upstream/tests/datagen -g8M -P50 > /tmp/lz4-oxide-bench/d50.bin
bench/bench.sh /tmp/lz4-oxide-bench/d50.bin "c1 c4 d1 d4"
```

Artefacts go to `$LZ4_BENCH_WORK` (default `${TMPDIR:-/tmp}/lz4-oxide-bench`),
never into the repo.

## Two things that will mislead you

**Run-to-run spread on the dev host is ~13%**, measured by repeating one binary
five times. A single run cannot resolve anything smaller, so `bench.sh` reports
best-of-N. A `WILD_COPY_CUTOFF` sweep run before that was measured showed a
clean-looking trend that was entirely noise.

**`datagen -P<n>` is not a compression ratio.** It is the probability of
emitting a match rather than a literal run (`datagen.c:131`), so `-P90` means
*many short* matches, not few long ones — which is why C also gets slower as it
rises. Sequence density, not compressibility, is what separates these inputs.
Construct the long-match case explicitly (`head -c 8M /dev/zero`) if that is
what you want to measure.

## Latency, RSS, and startup

Per-call latency uses 1 MB input and ten `fullbench` iterations. RSS is the
peak resident memory while each CLI compresses an 8 MB `-P50` input. Startup
uses 20 trials compressing `/dev/null`; results report min, p50, p99, and max.
