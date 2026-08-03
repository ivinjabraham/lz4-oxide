# DECISIONS.md

# 1. Scope

The port covers the public library implemented by:

| Upstream source | Purpose |
|---|---|
| `lib/lz4.c` | Block compression and decompression |
| `lib/lz4hc.c` | High-compression strategies |
| `lib/lz4frame.c` | Frame format |
| `lib/lz4file.c` | File helpers |
| `lib/xxhash.c` | Vendored checksum dependency |

The four LZ4 library files total 5,592 non-blank, non-comment lines. Including
the 692-line vendored xxHash dependency gives 6,284 C SLOC, below the
hackathon's 8,000-line ceiling. Porting `programs/` as well would exceed that
limit.

The C CLI remains unchanged and links against the Rust library. This keeps the
ported scope within the hackathon's source-line limit while making the original
CLI integration tests exercise the Rust implementation end to end.

# 2. Probe Caller-Allocated Layouts

Some structs in this port are never allocated by our own code at all. The C
test suite declares them itself — `LZ4_stream_t stream;`, sitting on its own
stack — and just hands us a pointer into memory it already owns. For that to
work, our Rust version of the struct has to have the exact same size and byte
layout as C's, because C's code reads and writes specific offsets inside it no
matter what Rust thinks the layout should be. Get this wrong and a C caller
writing what it believes is one field silently corrupts a different one.

Those sizes can't just be typed in once and trusted, because they aren't
fixed. `LZ4_STREAM_MINSIZE` depends on `LZ4_MEMORY_USAGE`, a build-time knob
upstream lets range from 10 to 20 — enough on its own to move a struct's size
by three orders of magnitude — and alignment isn't uniform either:
`XXH32_state_t` only needs 4-byte alignment where the rest of these types need
8. So instead of guessing, `build.rs` compiles a small C program against the
real headers and asks the compiler directly how big and how aligned each
struct actually is, and `src/types.rs` asserts the Rust definitions against
those measured numbers at compile time. If a header ever changes and the
numbers drift, this fails the build loudly instead of corrupting memory
quietly at runtime.

One consequence: Cargo has no way to know if a test build overrides
`LZ4_MEMORY_USAGE`, so both sides have to be built against the same value, or
the probed numbers and the real C struct will simply disagree.

`LZ4F_CustomMem` needs the same care for a different reason: it's passed **by
value**, not by pointer, into `LZ4F_createCDict_advanced` and
`LZ4F_createCompressionContext_advanced`. A wrong layout here wouldn't even
fail to link — there's no pointer-type mismatch to catch it — it would just
silently corrupt whatever ends up at the wrong offset.

# 3. Ownership

## Keep opaque handles owned by Rust

A handful of APIs work like create/free pairs —
`LZ4F_createCompressionContext` and its matching `LZ4F_free...` call, or the
file read/write handles. C never inspects what's inside these objects; it just
gets a pointer back from `create` and hands that same pointer to `free` later.
Because C treats them as opaque, we're free to represent them however Rust
normally would: allocate a real Rust value, hand C a raw pointer to it, and
when the matching `free` call comes back, turn that pointer back into an owned
value and let Rust drop it. `src/ffi.rs` is the only place this
pointer-to-owner conversion happens, in either direction.

## Honor custom allocators with a documented limit

Some callers don't want the system allocator at all — an embedded target with
its own arena, say — so the `_advanced` create functions accept an
`LZ4F_CustomMem`: a small struct of function pointers standing in for
`malloc`/`calloc`/`free`. Doing this completely faithfully would mean routing
everything the context ever allocates through those hooks, not just the
context struct itself but every scratch buffer it needs while compressing. We
only do the first half: the context struct goes through the caller's hooks,
but its internal working buffers stay as ordinary Rust `Vec`s, allocated from
Rust's own heap.

That's a real, measurable gap, not a free simplification. `LZ4F_cctx_size`
reports how much memory the context is using, and upstream's own test suite
checks that number against what the hooks actually saw allocated. Since our
`Vec` buffers never went through the hooks, they're invisible to that check,
so our reported size comes in smaller than a real C build's would. Closing
this properly would mean carving those buffers out of one hook-allocated block
and addressing them by offset instead of owning them separately as `Vec`s — a
real restructuring, which is why it's recorded here rather than silently left
as it is.

# 4. xxHash

The vendored xxHash implementation is ported in-tree rather than linked from C
or delegated to a crate.

The public `XXH32_state_t` and `XXH64_state_t` layouts are fixed by upstream
headers and are allocated by C callers. A crate's private state cannot satisfy
that ABI, and using a crate only for one-shot hashes would create two
implementations of the same algorithm in one library.

`src/xxh.rs` therefore implements one-shot and streaming XXH32/XXH64 over the
public layouts. `xxhash-rust` is a development-only oracle used by Rust tests;
it is not linked into the shipped library.

# 5. Confine Unsafe Code to the FFI Boundary

`src/ffi.rs` is the only module allowed to use `unsafe`. It validates C
arguments, forms slices or owned handles, and delegates to safe implementation
modules. `src/block.rs`, `src/hc.rs`, `src/frame.rs`, `src/file.rs`, and
`src/xxh.rs` use `#![forbid(unsafe_code)]`.

`make unsafe-count` enforces the location policy. The number of occurrences is
reported as evidence, not used as a manually maintained limit. The important
invariant is that no unsafe operation enters codec logic.

# 6. Overlapping-Buffer Semantics

Supported APIs permit source and destination ranges to overlap. Constructing
separate `&[u8]` and `&mut [u8]` values over those ranges would violate Rust's
aliasing rules. The FFI layer detects overlap and represents it as ranges within
one mutable slice. Safe codec functions operate on that slice and its indices.

# 7. Compression Tables and Indices Follow C

Fast compression selects `byU16` for small inputs and `byU32` for larger or
streaming inputs. The table width changes both storage and hash shift. Streaming
indices are relative to `currentOffset`, including the deliberate 64 KiB offset
advance used to make stale entries distinguishable.

HC state similarly uses absolute index space over prefix, external dictionary,
and current input. `SrcView` keeps contiguous prefix/current data together so
forward counting and backward catch-up cross the same boundaries as C.

# 8. Match Copies Preserve LZ4 Overlap Behavior

An LZ4 match may be longer than its offset. Its copy must read bytes produced by
earlier steps of the same copy; a plain `memcpy`-style operation is incorrect.
The block decoder uses safe fixed-width copies for short matches and a doubling
copy for long repeated patterns.

Wild copies are used only where upstream's margins prove that fixed-width
overreads and overwrites remain within allocated slices. Near a boundary, the
decoder uses exact copies.

Below that, a length cutoff (~32 bytes) switches between fixed-width copies and
`memcpy`/`memmove` — a threshold C has no equivalent of, since its wildcopy has
no per-step call to amortize. Calling `memcpy` for the few bytes a typical
sequence moves costs more than the copy; without the cutoff, literal-heavy
input loses roughly a third of its decode throughput.

# 9. Pointer-Style Bounds Comparisons Are Expressed Additively

Upstream often compares against pointers such as `oend - 32`. On a buffer
smaller than the margin, those conceptual pointers lie before the buffer.
`usize::saturating_sub` does not preserve that ordering and can incorrectly
enable a fast path at offset zero.

The Rust decoder writes equivalent checks additively, for example
`op + 32 <= oend` and `cpy + MFLIMIT > oend`. This avoids underflow while
preserving C's branch decision. Detailed examples are in
[PORTING.md](PORTING.md).

# 10. Prefix History Is Addressed Lazily

Prefix decode APIs describe history contiguous with the destination. The
decoder must not copy or eagerly read all nominal history: callers can provide
an addressable prefix that no encoded match actually references. The FFI layer
forms one indexed region spanning history and output, and the decoder reads
history only when a match requires it.

# 11. Frame Decode History Is Content-Owned

The C frame decoder tracks whether history remains contiguous with caller
output and avoids copies where possible. The Rust implementation keeps an owned
64 KiB history. This simplifies lifetime and aliasing behavior and preserves
decoded bytes because only history content affects decoding, but it costs a
copy as history advances.

This reasoning does **not** extend to compression. There, `withPrefix64k` vs.
`usingExtDict` changes which matches the search finds, so the two paths stay
distinct. Collapsing them the same way once cost 6 bytes per multi-block
frame — a difference invisible to round trips and caught only by
byte-comparison against C.

# 12. Known Divergences and Unverified Areas

| Area | Current behavior | Consequence |
|---|---|---|
| Frame compression levels 3 and above | Frame compression routes through the fast block compressor rather than HC. | Output is valid but can differ from C and may be larger. |
| Malformed frame input | Differential rejection testing covers block decoding, not frame decoding. | Exact frame error parity is unverified. |
| Frame custom allocators | Hooks own contexts but not internal `Vec` buffers. | Allocator ownership and `LZ4F_cctx_size` semantics differ from C. |
| Debug HC calls | A null `prefixStart` can reach `slice::from_raw_parts` in `src/ffi.rs`. | Debug builds abort before the fuzzer can exercise assertions; release behavior is unaffected. |
| Decoder fast loop | Implemented, verified byte-identical, measured, then **reverted**: slower than the general loop on every input tried (worst case -22%). Its speed in C comes from `goto`-ing into the middle of another loop, which Rust cannot express without doing the bail-out work up front on every sequence — so the port paid the cost C's control flow avoids. | The two-stage shortcut (ported) remains the fast path; many-short-sequence throughput is lower than C. |
| Release fail-loudness | `common_bytes` clamps invalid indices after a debug assertion. | A broken internal precondition can become wrong output rather than a release panic. |
| Build-time memory configuration | Cargo and C must use the same probed `LZ4_MEMORY_USAGE`. | Independently overriding only the C side can corrupt caller-allocated state. |

The benchmark runner records the frame-HC divergence alongside its generated
results. Performance measurements, environment details, and methodology live in
[`bench/results.json`](bench/results.json) and
[`bench/methodology.md`](bench/methodology.md), not in this log.

# 13. Upstream Finding

`LZ4_compress_destSize_extState` is declared in `lz4.h` without `LZ4LIB_API` or
`LZ4LIB_STATIC_API`, although the symbol is present in `liblz4.a`. ELF static
builds still export it, but a Windows DLL build can omit it because the missing
macro normally supplies `__declspec(dllexport)`. The ABI generator found the
discrepancy by comparing header declarations with symbols in the compiled
archive.
