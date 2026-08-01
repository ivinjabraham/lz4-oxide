# PORTING.md — writing the port by hand

For when no agent is available. This is not a Rust tutorial and not a C
tutorial; you can all write both. It is a list of **the things that break when
you translate *this* C into Rust**, each with the `lz4.c` line that causes it.

Read §1 and §2 once. Read §3 before you write a match loop. §4 is the thing you
will wish you had run at hour 60, so run it at hour 10.

---

## 1. The loop

```sh
make link-check
./upstream/tests/fuzzer -i1
#   thread panicked at src/ffi.rs:858: not implemented: LZ4_versionString
```

Implement whatever it names. Run it again. It names the next one. That is the
entire workflow — the stubs are a self-generating worklist, and you never have
to decide what to do next.

Two notes:

- Use `./upstream/tests/fuzzer -i1` for the first few hours. It is one
  iteration and fails in about a second. `make test-quick` is the next rung up;
  `make test` includes 6GB datagen cases and is not a development loop.
- If the fuzzer gets *further* but you cannot tell why it stopped, raise the
  display level: `./upstream/tests/fuzzer -i1 -v`.

> ⚠️ **`make test-reference` overwrites `upstream/tests/fuzzer` with the
> C-linked build.** The object files live in separate flag-hashed cache
> directories and coexist fine, but the final binary path is shared, so
> whichever you built last wins. Run `make test-reference`, then
> `./upstream/tests/fuzzer -i1`, and you are testing **C** — it will sail past
> everything and tell you nothing.
>
> Always `make link-check` (or `make test`) before running the binary by hand.
> The one-second check that you have the right one:
>
> ```sh
> strings upstream/tests/fuzzer | grep -c 'not implemented'   # >0 means Rust
> ```
>
> This is the same class of trap as PLAN.md §8, one level further out: not "the
> tests silently compile C" but "the binary on disk is silently the C one."

---

## 2. The boundary

One rule: **`src/ffi.rs` converts raw pointers to slices and delegates
immediately. Everything else is safe Rust on slices.** The implementation
modules carry `#![forbid(unsafe_code)]`, so the compiler enforces this, and
`make unsafe-count` fails the build if anything leaks out.

```rust
// src/ffi.rs — the only place `unsafe` is allowed
#[no_mangle]
pub unsafe extern "C" fn LZ4_compress_default(
    src: *const c_char, dst: *mut c_char, srcSize: c_int, dstCapacity: c_int,
) -> c_int {
    if src.is_null() || dst.is_null() || srcSize < 0 || dstCapacity < 0 {
        return 0;
    }
    let input  = unsafe { slice::from_raw_parts(src as *const u8, srcSize as usize) };
    let output = unsafe { slice::from_raw_parts_mut(dst as *mut u8, dstCapacity as usize) };
    crate::block::compress_default(input, output).unwrap_or(0) as c_int
}
```

```rust
// src/block.rs — no unsafe, ever
pub fn compress_default(src: &[u8], dst: &mut [u8]) -> Result<usize, Error> { ... }
```

### Return conventions — they are not uniform

This is the single most common source of "it works but the test fails."
`Result` lives inside; the translation happens once, per function, at the
boundary. Five families cover essentially everything:

| Family | Success | Failure | Notes |
|---|---|---|---|
| `LZ4_compress_*` | bytes written (`>0`) | **`0`** | never negative |
| `LZ4_decompress_safe*` | bytes written (`>0`) | **negative** | value is not specified; any negative is a fail |
| `LZ4_decompress_*_partial` | bytes written | negative | success may be < requested |
| `LZ4F_*` (frame) | `size_t`, often a *hint* | error code | test with `LZ4F_isError(r)`, never `r < 0` |
| `XXH*_reset/update` | `XXH_OK` (0) | `XXH_ERROR` (1) | note `0` means success here — inverted vs the above |

Two traps in that table:

- `LZ4F_*` returns `size_t`. A "negative" error is a huge unsigned number.
  `if (r < 0)` is always false and compiles fine. Use `LZ4F_isError`.
- Empty input is a *success* returning `1` byte for compression, not `0`.
  `fuzzer.c:1366` checks exactly this: compressing 0 bytes must produce a
  single `0` byte. Returning `0` reads as failure and fails the test.

When in doubt the authority is the doc comment in `upstream/lib/lz4.h` above
the declaration. It is accurate and it is per-function.

---

## 3. The four things that will actually bite you

### 3.1 Overlapping match copy — `copy_from_slice` is WRONG

**This is the one that will cost you a day if you get it wrong, because
round-trip tests still pass on small inputs.**

An LZ4 match says "copy `length` bytes from `offset` back." Nothing requires
`offset >= length`. When `offset < length` the source and destination
**overlap**, and the overlap is *load-bearing*: it is how LZ4 encodes runs.
`offset=1, length=50` means "repeat the previous byte 50 times." The bytes you
copy must include bytes you are writing *during the copy*.

So the natural Rust:

```rust
// WRONG — panics (overlapping borrow), or with split_at_mut, produces garbage
let (before, after) = out.split_at_mut(pos);
after[..len].copy_from_slice(&before[pos - offset..][..len]);
```

`copy_from_slice` has memcpy semantics: it reads the whole source, then writes.
That is not what LZ4 means. The correct translation is the boring one:

```rust
// RIGHT — byte at a time, reads what previous iterations just wrote
for i in 0..len {
    out[pos + i] = out[pos - offset + i];
}
```

Do not get clever here. C's `LZ4_memcpy_using_offset_base` (`lz4.c:492-511`)
with its `inc32table`/`dec64table` lookup is a *speed* optimisation for
`offset < 8` that produces identical bytes to the naive loop. Port the naive
loop first, get it correct, and only revisit if benchmarks demand it. The
output is byte-identical either way — this is an optimisation, not a
behaviour.

### 3.2 Wildcopy writes past the end — do not reproduce it

`LZ4_wildCopy8` (`lz4.c:466`) and `LZ4_wildCopy32` (`lz4.c:531`) deliberately
write **beyond** the logical end of the data, in fixed-size chunks, because
copying 8 or 32 bytes unconditionally is faster than checking a bound each
byte. C gets away with it because the caller guarantees slack in the buffer —
that is what `MFLIMIT`, `LASTLITERALS` and `MATCH_SAFEGUARD_DISTANCE`
(`lz4.c:246-248`) are reserving.

In Rust a slice bound at the logical end will panic instead. **Do not
reproduce the overcopy.** Write exactly the bytes that belong there:

```rust
out[pos..pos + len].copy_from_slice(&src[ip..ip + len]);   // literals: no overlap, this is fine
```

The output is identical — the extra bytes C writes are always overwritten or
discarded. What you *must* still port faithfully are the **limit constants and
the comparisons against them**, because those affect which parsing decisions
get made, and therefore the compressed bytes. Keep `MFLIMIT`, `LASTLITERALS`,
`MINMATCH`, `WILDCOPYLENGTH` and every `ip < ilimit`-style guard exactly as
written, even where the reason for a particular `-5` or `-12` is not obvious.

### 3.3 The hash is not yours to improve

```c
/* lz4.c:786 */
LZ4_hash4: (sequence * 2654435761U) >> ((MINMATCH*8) - LZ4_HASHLOG)
/* lz4.c:794 */
LZ4_hash5: ((sequence << 24) * 889523592379ULL) >> (64 - hashLog)
```

Transcribe these. Do not reason about them, do not swap in a "better" hash, do
not round the constants. The hash decides which match is found, which decides
the compressed bytes. A better hash still produces *valid* LZ4 that decompresses
correctly — so every round-trip test passes — and diverges from C on the first
differential fuzz run. That is 30% of the score, failing invisibly.

Same rule for tie-breaking, search order, and `nbSearches`/`targetLength` in
`k_clTable` (`lz4hc.c:92`).

**`LZ4_HASHLOG` is not a constant.** It is `LZ4_MEMORY_USAGE - 2`
(`lz4.h:697`), and `tests/Makefile:214-215` rebuilds the whole suite with
`LZ4_MEMORY_USAGE` at both its minimum (10) and maximum (20). So `LZ4_HASHLOG`
ranges 8..18 and the hash table size changes with it. `build.rs` probes the real
value and emits `LZ4_MEMORY_USAGE_PROBED`; derive the hashlog from that. Never
hardcode 12.

### 3.4 Compressed output is platform-dependent — on purpose

`LZ4_hashPosition` (`lz4.c:806`):

```c
if ((sizeof(reg_t)==8) && (tableType != byU16)) return LZ4_hash5(LZ4_read_ARCH(p), tableType);
return LZ4_hash4(LZ4_read32(p), tableType);     /* native-endian read */
```

Three build-time properties change the compressed bytes:

1. **Word size** — 64-bit builds use `hash5`, 32-bit builds use `hash4`.
2. **Endianness** — `LZ4_read_ARCH`/`LZ4_read32` are *native-endian* reads, and
   `hash5` has an explicit little/big-endian branch (`lz4.c:798-803`).
3. **`LZ4_MEMORY_USAGE`** — per §3.3.

This is true of the C original too, so it is not a divergence — but it means
"byte-identical to C" is a claim about *the same platform and build flags*, and
you must port the endianness branches rather than assuming little-endian.
Reading with `u32::from_le_bytes` where C used a native read is a real bug that
is invisible on x86-64.

(Upstream offers `LZ4_STATIC_LINKING_ONLY_ENDIANNESS_INDEPENDENT_OUTPUT` to
force the LE path. We do not define it, because the tests do not.)

---

## 4. Proving you got it right, without an agent

Round-trip tests do not detect divergence: wrong-but-valid output decompresses
fine. You need to compare bytes against C. Both libraries build side by side —
`multiconf.make` keys its object cache on a hash of the build flags, so the C
and Rust builds do not collide.

**This is committed as [`fuzz/difftest.c`](fuzz/difftest.c)** — the commands
below were run and verified on 2026-08-01, so if they fail for you it is your
build, not the recipe. It is also the seed of the real differential harness, so
it is not throwaway work:

```c
/* Compile twice — once against C, once against Rust — and cmp the output. */
#include <stdio.h>
#include <string.h>
#include "lz4.h"

int main(void) {
    char in[64 * 1024], out[LZ4_COMPRESSBOUND(64 * 1024)];
    size_t n = fread(in, 1, sizeof(in), stdin);
    int c = LZ4_compress_default(in, out, (int)n, (int)sizeof(out));
    fwrite(out, 1, (size_t)c, stdout);
    return c <= 0;
}
```

```sh
# reference: the untouched C library
make -C upstream/lib liblz4.a
gcc -I upstream/lib fuzz/difftest.c upstream/lib/liblz4.a -o /tmp/diff-c

# the port
make
gcc -I upstream/lib fuzz/difftest.c target/release/liblz4_rs.a \
    -lgcc_s -lutil -lrt -lpthread -lm -ldl -lc -o /tmp/diff-rs

# compare on real data
./upstream/tests/datagen -g64K > /tmp/sample
/tmp/diff-c  < /tmp/sample > /tmp/o-c
/tmp/diff-rs < /tmp/sample > /tmp/o-rs
cmp /tmp/o-c /tmp/o-rs && echo "BYTE-IDENTICAL"
```

Sanity check that this is wired up correctly: **before** `LZ4_compress_default`
is implemented, `/tmp/diff-rs` must abort with

```
thread '<unnamed>' panicked at src/ffi.rs:504:5:
not implemented: LZ4_compress_default
```

If it instead produces output, you have linked the C library by accident and
every comparison you run will trivially "pass."

Run that against several `datagen` seeds and compressibility settings
(`-g64K -P10`, `-P50`, `-P90`) the moment `LZ4_compress_default` returns
anything. If it says `BYTE-IDENTICAL` you are genuinely done with that
function; if round-trip passes but `cmp` differs, you have a §3.3 bug.

Other checks that need no agent:

```sh
make abi-check       # 141/141 symbols still exported
make kickoff-verify  # you have not accidentally touched the original tests
make unsafe-count    # nothing escaped ffi.rs
cargo test           # unit tests, incl. xxhash cross-checked against the oracle crate
```

---

## 5. Where to start in each file

| File | Start with | Then |
|---|---|---|
| `src/block.rs` | `LZ4_compressBound`, `LZ4_versionNumber` — trivial, unblock the fuzzer | `LZ4_compress_default` → `LZ4_decompress_safe` → streaming/dict |
| `src/xxh.rs` | `XXH32`/`XXH64` one-shot | streaming state (layout is fixed by `xxhash.h:264-285` — see DECISIONS.md §6) |
| `src/frame.rs` | `LZ4F_compressBegin/Update/End` | decompression state machine, then dictionaries |
| `src/hc.rs` | levels 3–9 (`LZ4HC_compress_hashChain`) | levels 10–12 optimal parser — **not** optional, see PLAN.md §6.1 |
| `src/file.rs` | thin layer over `frame.rs` | — |

Port structure first, cleverness never. A faithful, boring translation that
matches C byte for byte scores far better than idiomatic Rust that diverges.
The place to be idiomatic is error handling and the internal API shape
(DECISIONS.md §7.1) — not the search loops.

---

## 6. When you are unsure

- **Behaviour question** → the doc comment in `upstream/lib/lz4.h`. It is good.
- **Why is this constant here** → `upstream/doc/lz4_Block_format.md`.
- **Is this divergence acceptable** → if it changes compressed bytes, no.
  If it does not, write it down in DECISIONS.md and move on.
- **Something in the harness looks broken** → it is not; the C baseline is 100%
  green on our machine (tests/README.md). Suspect the port.
