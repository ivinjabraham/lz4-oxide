# lz4-oxide Benchmark Report

**Date:** 2026-08-03  
**Host:** x86-64 Linux 6.12.86  
**Upstream pin:** `0774d055` (verified clean, 42 test files match kickoff hashes)  
**Rust port:** `target/release/liblz4_rs.a`, provenance-checked (all 6 test binaries compiled from `cstub/`)

---

## 1. Behavioral equivalence — differential fuzzing

The core claim of this port is byte-identical output where the C library is deterministic.  
All comparisons below compile the same C harness twice — once against `upstream/lib/liblz4.a`,
once against `target/release/liblz4_rs.a` — and `cmp` the output streams.

### 1.1 `bench/verify.sh` — block codec, streaming, frame format, rejection parity

```
byte-identical: 1261    diverged: 0
```

Coverage:
- **block codec** (`difftest.c`): 5 sizes × 3 compressibilities × 7 modes = 105 cases
- **streaming + dictionary** (`stream_difftest.c`): internal index state emitted and compared
- **frame format** (`framediff.c`): 5 sizes × 3 compressibilities × 8 preference combinations = 120 cases
- **rejection parity**: 3 sizes × 5 corpus variants × 23 output capacities × 3 entry points = 1035 cases (exact return code + output bytes compared at boundary conditions)

### 1.2 `fuzz/driver.sh` — HC levels 1–12, all strategies

```
framediff:   8/8 mode combinations BYTE-IDENTICAL
hc_difftest: 108/108 (12 levels × 3 sizes × 3 compressibilities) BYTE-IDENTICAL
diverged: 0
```

Covers `lz4mid` (levels 1–2), greedy hash chain (3–9), and optimal parser (10–12),
via `LZ4_compress_HC_continue`, `LZ4_compress_HC_continue_destSize`, and `LZ4_compress_HC_extStateHC_fastReset`.

### 1.3 CLI diff — frame format via `lz4` binary

Compressed each of 4 inputs (8 MB, P=10/50/90/zero) with both CLIs, compared bytes,
and cross-decoded (C decodes Rust output, Rust decodes C output):

| Input | Level 1 | Level 9 |
|---|---|---|
| 8M -P10 (few matches) | ✅ identical + cross-decode OK | ❌ see §1.4 |
| 8M -P50 (medium) | ✅ identical + cross-decode OK | ❌ see §1.4 |
| 8M -P90 (many short matches) | ✅ identical + cross-decode OK | ❌ see §1.4 |
| 8M zeros (maximally compressible) | ✅ identical + cross-decode OK | ✅ identical + cross-decode OK |

### 1.4 Known divergence: HC levels through the frame API

`-9` and above diverge when the frame API is used (i.e. the `lz4` CLI `-9` flag,
or `LZ4F_compressFrame`/`LZ4F_compressUpdate` with `compressionLevel ≥ 3`).

**What happens:** `Cctx` holds only a fast (`LZ4_stream_t`) compressor; when
`compression_level ≥ LZ4HC_CLEVEL_MIN` the block path should dispatch to `hc::compress`,
but it never does — all levels silently use fast compression.  
**Output is valid** (decompresses correctly with both C and Rust decoders) but larger:

| Input | C `-9` | Rust `-9` | Rust/C ratio |
|---|---|---|---|
| 8M -P10 | 7,617 KB | 7,861 KB | 1.03× |
| 8M -P50 | 3,821 KB | 4,945 KB | 1.29× |
| 8M -P90 | 1,131 KB | 1,913 KB | 1.69× |
| 8M zeros | 9 KB | 9 KB | 1.00× (trivially compressible) |

**Scope:** `LZ4F_compress*` with positive `compressionLevel`. Direct HC block APIs
(`LZ4_compress_HC`, `LZ4_compress_HC_continue`, `LZ4_compress_HC_extStateHC_fastReset`,
`LZ4_compress_HC_continue_destSize`) are all byte-identical to C.  
**Gap in test coverage:** `framediff.c` only tests `compressionLevel ≤ 0` (fast acceleration).
`hc_difftest.c` tests the HC block API directly, not via the frame wrapper.

---

## 2. Throughput

`fullbench` compiled against each library, best-of-3 × 3 inner iterations, 8 MB input.  
Run-to-run spread on this host is ~13%; differences below ~15% are noise.

### 2.1 Hot-loop throughput (8 MB, -P50)

| Algorithm | C (MB/s) | Rust (MB/s) | ratio |
|---|---|---|---|
| `LZ4_compress_default` | 1,220 | 462 | 0.38× |
| `LZ4_compress_fast_continue(0)` | 953 | 263 | 0.28× |
| `LZ4_decompress_fast` | 5,670 | 4,924 | 0.87× |
| `LZ4_decompress_safe` | 9,552 | 5,479 | 0.57× |

### 2.2 Compression across corpus variants (algo 1 and algo 4)

| Corpus | C compress (MB/s) | Rust compress (MB/s) | ratio |
|---|---|---|---|
| 8M -P10 | 3,276 | 1,512 | 0.46× |
| 8M -P50 | 1,755 | 680 | 0.39× |
| 8M -P90 | 1,251 | 659 | 0.53× |
| 8M zeros | 32,244 | 15,035 | 0.47× |

| Corpus | C decompress (MB/s) | Rust decompress (MB/s) | ratio |
|---|---|---|---|
| 8M -P10 | 32,614 | 29,927 | 0.92× |
| 8M -P50 | 11,023 | 6,669 | 0.61× |
| 8M -P90 | 8,099 | 4,032 | 0.50× |
| 8M zeros | 29,468 | 59,747 | 2.03× |

Rust decompress on zeros is 2× faster than C — the all-zero case hits a long-match
fast path in `decompress_fast` that bulk-copies via Rust's `copy_nonoverlapping`
rather than C's byte loop (see DECISIONS.md §perf for the `WILD_COPY_CUTOFF` notes).

---

## 3. Per-call latency (1 MB input, n=10 fullbench iterations)

| Operation | C p50 (µs) | C p99 (µs) | Rust p50 (µs) | Rust p99 (µs) |
|---|---|---|---|---|
| compress (LZ4_compress_default) | 546 | 556 | 1,446 | 1,446 |
| decompress (LZ4_decompress_safe) | 28 | 28 | 60 | 78 |

Latency tracks throughput: compress is ~2.6× slower than C, decompress ~2.2×.

---

## 4. RSS and binary size

### 4.1 Peak RSS — CLI compressing 8 MB (-P50)

| Binary | Peak RSS |
|---|---|
| `lz4` (C) | ~12 MB |
| `lz4` (Rust port) | ~23 MB |

The Rust binary statically links `libstd` and `libcore`, which contributes both
to RSS and to binary size. In a shared-library deployment the delta would shrink.

### 4.2 Binary size

| Binary | Total | `.text` | `.data+.bss` | Note |
|---|---|---|---|---|
| `lz4` (C) | 341 KB | 307 KB | 9 KB | dynamically links glibc |
| `lz4` (Rust, unstripped) | 5.2 MB | 1,108 KB | 45 KB | statically links libstd |
| `lz4` (Rust, stripped) | 1.2 MB | — | — | |
| `fullbench` (C) | 1.6 MB | — | — | |
| `fullbench` (Rust) | 5.1 MB | — | — | |

The `.text` section is 3.6× larger than C's. Part of this is Rust's standard library
(panic machinery, allocator, fmt), part is that the port has not gone through
dead-code elimination passes.

### 4.3 Startup time — compressing `/dev/null` (20 trials)

| Binary | min | p50 | p99 | max |
|---|---|---|---|---|
| `lz4` (C) | 0.2 ms | 0.3 ms | 0.7 ms | 0.7 ms |
| `lz4` (Rust) | 0.2 ms | 0.3 ms | 0.4 ms | 0.4 ms |

Startup is indistinguishable. The larger binary does not cost startup latency
on a warm filesystem.

---

## 5. ABI and provenance

```
original: 141  rust: 141
OK: Rust archive exports exactly the original ABI.

OK: all 6 built test binaries were compiled from cstub/, not from lib/.

OK: upstream at 0774d055, tree clean, 42 test files match their kickoff hashes.
```

`unsafe` is confined to `src/ffi.rs`: 313 occurrences in 1 of 8 source files,
49.81 per 1,000 C SLOC ported.

---

## Summary

| Claim | Result |
|---|---|
| Byte-identical output (block API) | ✅ 1,261/1,261 comparisons, 0 diverged |
| Byte-identical output (HC block API, all 12 levels) | ✅ 108/108 BYTE-IDENTICAL |
| Byte-identical output (frame API, fast levels) | ✅ CLI level 1 identical on all 4 inputs |
| Byte-identical output (frame API, HC levels ≥ 3) | ❌ valid but larger (§1.4) |
| Rejection parity (corrupt/truncated input) | ✅ exact return code + output bytes match |
| ABI completeness (141 symbols) | ✅ exact match |
| Upstream suite unmodified | ✅ kickoff hash verified |
| All `unsafe` confined to `src/ffi.rs` | ✅ 313 occurrences, 0 outside ffi.rs |
| Throughput — compress | 0.28–0.53× C (2–4× slower) |
| Throughput — decompress | 0.50–2.03× C (±2×) |
| Startup latency | ≈ identical (p50 0.3 ms both) |
| RSS overhead | ~11 MB (libstd + allocator) |
