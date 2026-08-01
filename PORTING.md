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
  `make test` pulls in the 6GB/3GB huge-file cases and is not a development loop.
- If the fuzzer gets *further* but you cannot tell why it stopped, raise the
  display level: `./upstream/tests/fuzzer -i1 -v`.

> ⚠️ **The binary at `upstream/tests/fuzzer` is not always the Rust one.**
> `make test-reference` builds the C version onto the same path, and whichever
> build ran last wins. Run it, then `./upstream/tests/fuzzer -i1`, and you are
> exercising **C** — it sails past everything and tells you nothing.
>
> There is a worse version of this. `multiconf.make` keys its object cache on
> the compiler and link flags but **not** on `C_SRCDIRS` — the variable that
> does the whole substitution. So an `lz4.o` compiled earlier from the real
> `lib/lz4.c` can be silently relinked into a binary built with our overrides,
> with no duplicate-symbol error, because `LDLIBS` comes last on the link line
> and archive members are only pulled in for *undefined* symbols.
>
> One command settles it:
>
> ```sh
> make provenance-check
> ```
>
> It reads each binary's cached `.d` file, which records the path the compiler
> actually resolved, and fails if any of them came from `lib/` rather than
> `cstub/`. `make link-check` runs it for you. When it fails, the fix is
> `rm -rf upstream/{tests,programs}/cachedObjs && make link-check`.

---

## 2. The boundary

One rule: **`src/ffi.rs` converts raw pointers to slices and delegates
immediately. Everything else is safe Rust on slices.** The implementation
modules carry `#![forbid(unsafe_code)]`, so the compiler enforces this, and
`make unsafe-count` fails the build if anything leaks out.

### ⚠️ Read this before you design the internal API

**`src` and `dst` may point into the same allocation.** The obvious signature —
two slices — is undefined behaviour against the real test suite, and it is the
first thing you would otherwise write.

`fuzzer.c:1207-1218` compresses in place:

```c
char* const startInput = testCompressed + startInputIndex;
memcpy(startInput, testInput, sampleSize);          /* input at END of buffer */
cSize = LZ4_compress_default(startInput, testCompressed, sampleSize, maxCSize);
```

`src` and `dst` are the same buffer. That is deliberate and supported — it is
what `LZ4_COMPRESS_INPLACE_BUFFER_SIZE` and `LZ4_DECOMPRESS_INPLACE_MARGIN`
(`lz4.h:672-680`) exist for, and `LZ4_decompress_safe` gets the same treatment
at `fuzzer.c:1240-1247`. It runs on `fuzzer -i1`.

Holding `&[u8]` and `&mut [u8]` over overlapping memory violates Rust's aliasing
rules. This is not pedantry: `Cargo.toml` sets `lto = true` and
`codegen-units = 1`, which is exactly where a `noalias` violation miscompiles
rather than merely being theoretically wrong. And no safe signature can express
it — you cannot fix this later without redesigning every function's parameters.

**So the internal API takes one buffer and index ranges, not two slices:**

```rust
// src/block.rs — no unsafe, ever
//
// `buf` covers the whole caller allocation; `src` and `dst` are ranges within
// it and may overlap. When the caller's buffers are genuinely separate, ffi.rs
// still has two allocations — see `compress_split` below.
pub fn compress_in_buffer(
    buf: &mut [u8], src: Range<usize>, dst: Range<usize>,
) -> Result<usize, Error> { ... }
```

Decide this before anyone writes a match loop. Changing it afterwards means
touching every function in the module.

### Return conventions — they are not uniform

This is the single most common source of "it works but the test fails."
`Result` lives inside; the translation happens once, per function, at the
boundary. Five families cover essentially everything:

| Family | Success | Failure | Notes |
|---|---|---|---|
| `LZ4_compress_*` | bytes written (`>0`) | **`0`** | never negative |
| `LZ4_decompress_safe*` | bytes written (**`>=0`**) | **negative** | `0` is success — see below |
| `LZ4_decompress_safe_partial` | bytes written (`>=0`) | negative | see the corruption trap below |
| `LZ4F_*` (frame) | `size_t`, often a *hint* | error code | test with `LZ4F_isError(r)`, never `r < 0` |
| `XXH*_reset/update` | `XXH_OK` (0) | `XXH_ERROR` (1) | `0` means success here — inverted vs the above |

Four traps in that table:

- **`0` from `LZ4_decompress_safe` is success, not failure.** Decompressing the
  single-byte block `0x00` legitimately yields zero bytes. A boundary that maps
  `0` to `Err` breaks every empty round-trip.
- **Empty input compresses to `1` byte, not `0`** — and `src` may be `NULL`
  while doing it. `fuzzer.c:1187-1195` calls
  `LZ4_compress_default(NULL, testCompressed, 0, maxCSize)` and requires the
  result to be `1` with `dst[0] == 0`. A blanket `if src.is_null() { return 0 }`
  guard — the obvious defensive reflex — fails this test. The *next* case,
  `fuzzer.c:1198-1201`, returns `0`, but because `dstCapacity == 0`, not
  because `dst` is `NULL`.
- **`LZ4F_*` returns `size_t`.** A "negative" error is a huge unsigned number,
  so `if (r < 0)` is always false and compiles cleanly. Use `LZ4F_isError`.
- **`LZ4_decompress_safe_partial` can corrupt silently.** Per `lz4.h:305-309`,
  if `srcSize` is larger than the block's true compressed size, then
  `targetOutputSize` **must** be no greater than the real decompressed size, or
  you get silent corruption rather than an error.

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
// WRONG — when len > offset this indexes past `before` and panics;
// even where it doesn't, the semantics are wrong.
let (before, after) = out.split_at_mut(pos);
after[..len].copy_from_slice(&before[pos - offset..][..len]);
```

`copy_from_slice` has memcpy semantics: it reads the whole source, then writes.
That is not what LZ4 means. The correct translation is the boring one:

```rust
// RIGHT — byte at a time, reads what previous iterations just wrote.
// Correct only when offset <= pos; see the dictionary case below.
for i in 0..len {
    out[pos + i] = out[pos - offset + i];
}
```

**`offset > pos` is legal.** With a dictionary or a ring buffer in play
(`LZ4_decompress_safe_usingDict`, `LZ4_setStreamDecode`), the match starts
*before* the output buffer, in the dictionary — so `pos - offset` underflows and
panics. Worse, a single match can **straddle** the boundary, requiring a split
copy: `lz4.c:2173-2202` handles exactly this, taking `lowPrefix - match` bytes
from `dictEnd` and the remainder from the output. All three streaming decode
tests reach it.

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

### 3.3 `tableType` decides everything — read this before you write a hash

There is no single "the hash". `LZ4_compress_generic` is instantiated per
**`tableType_t`** (`lz4.c:726`), one of `byPtr`, `byU32`, `byU16`, and the type
selects the hash function, the shift, the table's element width, and what the
stored values mean. Get this wrong and nothing else in this section saves you.

Which type you get is chosen by **input size**, at `lz4.c:1396-1403`:

```c
if (inputSize < LZ4_64Klimit) {                      /* 64 KB + 11 */
    ... byU16 ...
} else {
    const tableType_t tableType =
        ((sizeof(void*)==4) && ((uptrval)source > LZ4_DISTANCE_MAX)) ? byPtr : byU32;
```

So on x86-64, **anything under 64 KB is `byU16`** — which is most of what the
fuzzer feeds you.

Now the dispatch, `LZ4_hashPosition` (`lz4.c:808`):

```c
if ((sizeof(reg_t)==8) && (tableType != byU16)) return LZ4_hash5(LZ4_read_ARCH(p), tableType);
return LZ4_hash4(LZ4_read32(p), tableType);   /* native-endian read */
```

`byU16` is excluded from `hash5` **even on 64-bit**. And both hashes shift
differently for it (`lz4.c:786-804`):

```c
LZ4_hash4:  tableType == byU16 ? (seq * 2654435761U) >> ((MINMATCH*8)-(LZ4_HASHLOG+1))
                               : (seq * 2654435761U) >> ((MINMATCH*8)- LZ4_HASHLOG)
LZ4_hash5:  hashLog = (tableType == byU16) ? LZ4_HASHLOG+1 : LZ4_HASHLOG
            little-endian: ((seq << 24) * 889523592379ULL)   >> (64 - hashLog)
            big-endian:    ((seq >> 24) * 11400714785074694791ULL) >> (64 - hashLog)
```

**The trap:** write only the `hash5` path — the obvious reading of "64-bit uses
hash5" — and every input under 64 KB silently takes a hash you never wrote.
Round-trips still pass. Every compressed byte differs from C.

Transcribe all the branches. Do not swap in a "better" hash, do not round the
constants, do not collapse the `byU16` cases. A better hash still produces
*valid* LZ4 that decompresses correctly, so every round-trip test passes and it
diverges from C on the first differential fuzz run.

Same rule for tie-breaking, search order, and — for `lz4hc.c` specifically —
`nbSearches`/`targetLength` in `k_clTable` (`lz4hc.c:92`).

**`LZ4_HASHLOG` is not a constant.** It is `LZ4_MEMORY_USAGE - 2` (`lz4.h:697`),
which upstream permits to range 10..20 (`lz4.h:162-164`), so the hashlog ranges
8..18 and the table size with it. `build.rs` probes the real value and emits
`LZ4_MEMORY_USAGE_PROBED`, reachable as `crate::types::LZ4_MEMORY_USAGE_PROBED`.
Derive from that; never hardcode 12. And remember `byU16` adds one to it.

### 3.4 Compressed output is platform-dependent — on purpose

Four build-time properties change the compressed bytes:

1. **Word size** — 64-bit uses `hash5`, 32-bit uses `hash4` — *except* for
   `byU16`, which is always `hash4` (§3.3).
2. **Endianness** — `LZ4_read_ARCH`/`LZ4_read32` are *native-endian* reads
   (`lz4.c:381`, `:396`), and `hash5` branches on endianness internally.
3. **`LZ4_MEMORY_USAGE`** — per §3.3.
4. **`LZ4_DISTANCE_MAX`** (`lz4.c:257`) — a build knob that directly changes
   which matches are accepted.

On 32-bit there is a fifth, and it isn't a build flag at all: `lz4.c:1401`
selects `byPtr` vs `byU32` from `(uptrval)source > LZ4_DISTANCE_MAX`, so the
output depends on the **runtime address of the input buffer**.

None of this is a divergence — the C original behaves the same way. It means
"byte-identical to C" is a claim about the same platform and build flags, and
that you must port the endianness branches rather than assuming little-endian.
Reading with `u32::from_le_bytes` where C used a native read is a real bug that
is invisible on x86-64.

Note the asymmetry that catches people: hash reads are **native**-endian, but
the match offset written into the output is **always** little-endian
(`LZ4_writeLE16`, `lz4.c:452`). Both appear within a few hundred lines of each
other.

(`LZ4_STATIC_LINKING_ONLY_ENDIANNESS_INDEPENDENT_OUTPUT` only affects the
`hash4` path — line 808 returns `hash5` before the `#ifdef` is reached, and
`hash5` keeps its own endian branch. We do not define it; neither do the tests.)

---

## 4. Proving you got it right, without an agent

Round-trip tests do not detect divergence: wrong-but-valid output decompresses
fine. You need to compare bytes against C. Both libraries build side by side —
**This is committed as [`fuzz/difftest.c`](fuzz/difftest.c).** The compile and
link steps below were run on 2026-08-01 and work as written; the `cmp` cannot
have produced `BYTE-IDENTICAL` yet, because no function is implemented. It is
also the seed of the real differential harness, so it is not throwaway work:

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

# compare on real data (link-check does NOT build datagen — build it here)
make -C upstream/tests datagen
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

## 4a. Five more things you will hit on day one

Not traps exactly — just things that are load-bearing, easy to miss, and
annoying to retrofit.

**The compression state is index-based, not pointer-based, and the indices
drift on purpose.** `lz4.c:917-923`:

```c
if (cctx->currentOffset != 0 && tableType == byU32) cctx->currentOffset += 64 KB;
```

Table entries are stored relative to `currentOffset`, not to the buffer, and
that deliberate gap is what makes stale entries from a previous block
detectable. Reproduce it exactly — it changes which matches are found.

**The skip heuristic is second only to the hash in deciding the output.**
`lz4.c:1031-1037`, with `LZ4_skipTrigger = 6` (`lz4.c:720`):

```c
int searchMatchNb = acceleration << LZ4_skipTrigger;
step = (searchMatchNb++ >> LZ4_skipTrigger);
```

On a failed match the scanner accelerates. Also clamp as C does:
`acceleration < 1` becomes `1`, and anything above `LZ4_ACCELERATION_MAX`
(65537) is clamped (`lz4.c:52-58`).

**`LZ4_compressBound` has an exact formula** — `(isize) + ((isize)/255) + 16`,
returning `0` when `isize > LZ4_MAX_INPUT_SIZE` (`0x7E000000`), per
`lz4.h:214-215`. It is one of the first functions you will write; don't
approximate it.

**The block format has hard terminating rules**, and they are *format*
requirements rather than buffer arithmetic (`doc/lz4_Block_format.md:112-126`):
the last 5 bytes of a block are always literals, and the last match must start
at least 12 bytes before the end. `LZ4_minLength` is `MFLIMIT+1 = 13`
(`lz4.c:250`) — anything shorter is emitted as pure literals. This is why
`LASTLITERALS` and `MFLIMIT` must not be relaxed even after you stop
reproducing the wildcopy (§3.2): break them and the C decoder rejects your
blocks.

**`LZ4_compress_destSize` is a shape the return-convention table doesn't
cover.** It writes back through `srcSizePtr` — a second output — and always
uses `byU16` for small inputs (`lz4.c:1499`). It's in the 141-symbol ABI, so it
has to be dealt with eventually.

---

## 5. Where to start in each file

| File | Start with | Then |
|---|---|---|
| `src/block.rs` | `LZ4_versionString`/`LZ4_versionNumber`, `LZ4_compressBound` — trivial, and the first three the fuzzer demands | `LZ4_compress_default` → `LZ4_decompress_safe` → streaming/dict |
| `src/xxh.rs` | `XXH32`/`XXH64` one-shot | streaming state (layout is fixed by `xxhash.h:264-285` — see DECISIONS.md §6) |
| `src/frame.rs` | `LZ4F_compressBegin/Update/End` | decompression state machine, then dictionaries |
| `src/hc.rs` | levels ≤2 (`lz4mid`, its own hashes + **two** tables) then 3–9 (`lz4hc` hash chain) | levels 10–12 (`lz4opt` optimal parser) — **not** optional, see PLAN.md §6.1 |
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
