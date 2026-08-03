# Benchmark Methodology

This directory contains one reproducible runner and its machine-readable output
for comparisons against the pinned C library. Differential byte-identity and
rejection-parity coverage uses the same runner through `make difftest`; analysis
and limitations live in [DECISIONS.md §8.4](../DECISIONS.md).

## Run Everything

From the repository root:

```sh
bench/bench.py
```

That single command:

1. verifies the pinned upstream commit, clean tree, and kickoff hashes;
2. clears generated caches and creates the benchmark corpora;
3. builds independent C and Rust `fullbench` and CLI binaries;
4. verifies that the Rust CLI was compiled from `cstub/`;
5. runs `make difftest`;
6. measures throughput, latency, RSS, binary size, and startup;
7. atomically replaces [`results.json`](results.json).

## Build Provenance

Both implementations use the same unmodified `upstream/tests/fullbench.c`
harness. The C benchmark compiles directly with the pinned C sources. The Rust
benchmark compiles the harness directly against `target/release/liblz4_rs.a`,
so no cached `lz4.o` can enter that executable.

## Throughput

Throughput uses 8 MiB inputs and reports decimal MB/s, matching `fullbench`.
C and Rust samples alternate order across three runs, each with three inner
iterations, and the result records each implementation's best sample. The
hot-loop P50 matrix measures:

- `c1`: `LZ4_compress_default`
- `c9`: `LZ4_compress_fast_continue`
- `d1`: `LZ4_decompress_fast`
- `d4`: `LZ4_decompress_safe`

The corpus matrix additionally measures default compression and safe
decompression on `datagen -P10`, `-P50`, `-P90`, and an all-zero input.

## Latency, RSS, Size, and Startup

Latency alternates C and Rust across ten independent one-inner-iteration
`fullbench` samples on a 1 MiB P50 input. Each throughput sample is converted
to microseconds per 1 MiB call; the result records p50 and max.

RSS is the child process's Linux `wait4().ru_maxrss` while each CLI compresses
the 8 MiB P50 corpus. Binary size records the C CLI, unstripped Rust CLI, and a
stripped temporary Rust copy. Startup alternates C and Rust across 20
warm-filesystem trials compressing `/dev/null` and records min, p50, and max.

## Measurement Caveat

Run-to-run spread on the development host is about 13%. A single run cannot
resolve smaller changes, which is why the runner records best-of-N throughput.
