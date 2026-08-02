# Is the bottleneck the two decisions in `block.rs`?

Answer: **for decompression, largely yes — and it is fixed.** For compression,
only partly; most of that gap is elsewhere and is still open.

Branch `perf/block-bulk-copies`, on top of 24b2216. Two commits, no `unsafe`
added — `make unsafe-count` still reports all 268 occurrences confined to
`src/ffi.rs`, and `block.rs` keeps `#![forbid(unsafe_code)]`.

## Method

`upstream/tests/fullbench` (lz4's own speed analyzer) built twice from the same
`tests/Makefile`: once normally, once with our `C_SRCDIRS`/`LDLIBS` overrides.
`make provenance-check` confirms the Rust one links no `lib/*.c` object.

8 MB inputs. `fullbench -i3`, best of 3 runs — **run-to-run spread on this host
is ~13%**, measured by repeating one binary five times, so anything smaller
than that is not a result.

Byte-identity after every change: `verify.sh` runs `fuzz/difftest`,
`fuzz/stream_difftest` and `fuzz/framediff` compiled against both libraries
over five sizes × three compressibilities — **226/226 identical**, and the
divergence set is unchanged from the untouched tree.

## `LZ4_decompress_safe` vs C

| input | before | step 1 | step 2 |
|---|---|---|---|
| `datagen -P20` (literal-heavy) | 0.91x | 0.91x | **0.92x** |
| `datagen -P50` | 0.46x | 0.55x | **0.66x** |
| `datagen -P90` (short matches) | 0.21x | 0.30x | **0.53x** |
| 8 MB of zeroes (one huge match) | — | 2.73x | **2.60x** |
| 32 KB block × 256 | — | 1.07x | **1.12x** |

`LZ4_compress_default`: 0.30x → 0.34x (P50), 0.35x → 0.44x (P90).

(The `-P20` cell is a median of six runs. Best-of-3 put it at 0.97x, which is
the upward bias of best-of-N on whichever side got the luckier scheduler; it
would not have reproduced.)

## What the shape of the data said

The discriminating measurement was **sequence density**, not compressibility:

- 8 MB of zeroes — few, enormous matches — went to **2.7x faster than C**. The
  doubling loop hands whole megabytes to `memmove` where C's `LZ4_wildCopy8` is
  still stepping 8 bytes at a time.
- `-P20` — mostly literals, i.e. long `memcpy`s — was already at 0.9x before any
  change. Literal throughput was never the problem.
- `-P90` — many *short* matches — was the worst at 0.21x.

So the cost was per-sequence, not per-byte: a call into `memcpy`/`memmove` for
a handful of bytes. That is exactly what `LZ4_wildCopy8` exists to avoid, and
porting it is what moved `-P90` from 0.30x to 0.53x.

> Correction to an earlier reading in this file: I first took the P50→P90 drop
> as evidence of long matches, and it is not. **C slows down on P90 too**
> (7729 → 5664 MB/s); if P90 were long matches both would speed up. `-P90`
> produces *more, shorter* sequences. The conclusion the misreading motivated
> — bulk copies — was still right, but for the other reason.

## What changed

**Step 1 (83ca30f)** — bulk copies, no overshoot:
`copy_match` uses `copy_within` for disjoint regions and a doubling loop for
overlapping ones; `common_bytes` compares a 64-bit word at a time with a bit
scan as `LZ4_count` does; `Input::as_slice` takes the slice once per scan
instead of re-dispatching the enum per byte.

**Step 2 (bc27dd5)** — `LZ4_wildCopy8`, in safe Rust:
fixed 8-byte steps that may write up to 7 bytes past the run, used only where
C's own guard proves the room; the `inc32table`/`dec64table` prologue for
offsets below 8; the shortcut's fixed 18-byte copy. `WILD_COPY_CUTOFF` keeps
`memcpy` for long runs — without it, literal-heavy input lost a third of its
throughput.

## The wildcopy premise was wrong

`block.rs`'s module doc justified decision #1 with "in Rust that is a panic."
It is not: every wildcopy call site in `safe_decode` is guarded so the
overshoot lands inside the buffer — `cpy <= oend-MFLIMIT` (12) against a 7-byte
overshoot at lz4.c:2350, `oCopyLimit = oend-7` at lz4.c:2444. Overshooting was
legal all along. The doc is corrected in the commit.

## Still open

- **Compression, 0.34x.** `count` was worth ~18%, so the copies were *a*
  bottleneck, not *the* bottleneck. What is left is the match search:
  `hash_position` reads through `Input::window` (enum dispatch + bounds check)
  on every position, and the table get/put per position. Specialising the
  compressor on the `Input` variant and on `DictDirective` — which C gets free
  from `LZ4_FORCE_INLINE` per directive, and which this port does at run time
  by design (see the `DictDirective` doc) — is the next lever.
- **`LZ4_decompress_fast`, 0.27x — untouched.** It goes through
  `decompress_fast_with`, which takes `impl FnMut(usize) -> u8` and so pays a
  closure call *per byte*, plus a three-way branch per byte in its match copy.
  That is a separate defect from the two decisions and wants its own change.
- **The dictionary decode entry points allocate and copy per call — and this is
  now the largest single cost on that path.** It is not in `block.rs` at all.

  `ffi.rs:325-339` (`decompress_safe_histories`) does, on **every** call:
  `vec![0u8; prefix + dstCapacity]`, `copy_from_slice` the prefix in, decode,
  then `ptr::copy` the result back out to the caller's `dst`. For a 64 KB block
  with a 64 KB prefix that is a 128 KB zeroed allocation plus two 64 KB copies
  to produce 64 KB of output — roughly 4x the memory traffic of the decode.

  It shows up as a decode mode that did not respond to *any* of this work:

  | `rep.bin` | C | Rust | ratio |
  |---|---|---|---|
  | `decompress_safe` (decodes into the caller's `dst`) | 21353 | 23216 | **1.09x** |
  | `decompress_safe_withPrefix64k` (via `histories`) | 20731 | 7969 | 0.38x |
  | `decompress_safe_usingDict` (via `histories`) | 21872 | 7738 | 0.35x |

  Same input, same C speed for all three; the two that share
  `decompress_safe_histories` are 3x down, and they stayed at 0.38x across all
  three of my changes. `rep.bin` exposes it because the decode itself is fast
  enough (23 GB/s) that a fixed per-call cost dominates — on `-P50` all three
  read ~0.55x, because there the decode is slow enough to hide it.

  This is `ffi.rs`, i.e. person A's area, and it is the same "own the history
  and copy into it" shortcut already recorded for `LZ4F_updateDict` in
  DECISIONS.md §8.1. Closing it means decoding into the caller's buffer with
  the prefix addressed in place, as `LZ4_decompress_safe` already does through
  `with_buffers`.
- **Possible latent edge case, pre-existing.** `shortoend` is computed with
  `saturating_sub`, so on an output buffer smaller than 32 bytes with
  `dst.start == 0` the shortcut's guard `op <= shortoend` becomes `0 <= 0` and
  admits a sequence C's pointer comparison would reject. Rust's bounds checks
  make it a panic rather than a corruption, and step 2 added an explicit
  `op + 18 <= oend` fallback at that site, but the guard itself is unexamined.

## Verification gap

`make test-quick` **cannot pass on this branch**, and not because of these
changes: `upstream/tests/fuzzer` dies at `not implemented: LZ4_compress_HC`,
which is step 5/6 and not yet written. lz4's own suite has therefore *not* been
run against this work. The 226/226 differential result covers the block codec
and the frame format well, but it is not the same evidence.
