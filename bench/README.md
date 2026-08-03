# bench/

Throughput measurement against the C library. Differential byte-identity and
rejection-parity checks live in [`../fuzz/driver.sh`](../fuzz/driver.sh).

- **Methodology:** [`methodology.md`](methodology.md)
- **Structured results:** [`results.json`](results.json)
- **Analysis and limitations:** [DECISIONS.md §8.4](../DECISIONS.md)

This directory contains the reproducible method, machine-readable data, and
tooling; the linked reports explain the conclusions and limitations.

| script | what it does |
|---|---|
| `rebuild.sh` | Rebuild `fullbench` against the port. Forces the relink and runs `provenance-check` — `tests/Makefile` does not list our archive as a prerequisite, so without this you benchmark a stale binary, and a cached C object relinks silently. |
| `bench.sh` | `fullbench` C vs Rust on selected algorithms. `bench.sh <file> "<algos>" [reps]`. |

```sh
bench/rebuild.sh                                    # build fullbench against the port
upstream/tests/datagen -g8M -P50 > /tmp/lz4-oxide-bench/d50.bin
bench/bench.sh /tmp/lz4-oxide-bench/d50.bin "c1 d4 d6"
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
