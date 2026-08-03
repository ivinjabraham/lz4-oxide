# DECISIONS.md

> The reference implementation is upstream LZ4 commit `0774d05537f9762f838f7ab541b7765f1a729cb5`.

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

## 3. Probe Caller-Allocated Layouts

Several public state types are allocated by C callers on their own stack. Their
size and alignment are ABI, not implementation details. They cannot contain
`Box`, `Vec`, `String`, or any other Rust-owned indirection.

`build.rs` compiles a C probe against the pinned headers and emits the measured
sizes and alignments. `src/types.rs` asserts the Rust representations against
those values at compile time. This is necessary because values such as
`LZ4_STREAM_MINSIZE` depend on `LZ4_MEMORY_USAGE`, and because types such as
`XXH32_state_t` have less alignment than the other public states.

The current build does not propagate arbitrary test-side `LZ4_MEMORY_USAGE`
overrides into Cargo. A C caller and Rust library built with different values
would disagree about state size. Supported builds must therefore use the same
headers and configuration for both sides.

`LZ4F_CustomMem` is probed for a different reason: it crosses the boundary **by
value**, not by pointer, into `LZ4F_createCDict_advanced` and
`LZ4F_createCompressionContext_advanced`. A wrong layout on a by-value struct
does not fail to link — it silently corrupts, with no pointer mismatch to
catch it.

## 4. Ownership

### Keep opaque handles owned by Rust

Objects created and freed through opaque C APIs, such as frame contexts and
file handles, are Rust-owned allocations represented by raw pointers at the
boundary. `src/ffi.rs` is responsible for converting between those pointers and
their Rust owners.

### Honor custom allocators with a documented limit

`LZ4F_create*_advanced` allocates the context object through the caller's
`LZ4F_CustomMem` hooks and returns it through the matching free hook. Internal
working buffers remain Rust `Vec` allocations rather than allocations from the
custom hooks.

`LZ4F_cctx_size` reports the bytes observed by the caller's allocator, so the
upstream hook-accounting assertion passes. Unlike C, that value does not include
the Rust-heap working buffers. Full custom-allocator ownership would require
placing the context and its working storage in hook-allocated blocks and
tracking buffer offsets instead of `Vec`s.

## 5. xxHash

The vendored xxHash implementation is ported in-tree rather than linked from C
or delegated to a crate.

The public `XXH32_state_t` and `XXH64_state_t` layouts are fixed by upstream
headers and are allocated by C callers. A crate's private state cannot satisfy
that ABI, and using a crate only for one-shot hashes would create two
implementations of the same algorithm in one library.

`src/xxh.rs` therefore implements one-shot and streaming XXH32/XXH64 over the
public layouts. `xxhash-rust` is a development-only oracle used by Rust tests;
it is not linked into the shipped library.

## 6. Confine Unsafe Code to the FFI Boundary

`src/ffi.rs` is the only module allowed to use `unsafe`. It validates C
arguments, forms slices or owned handles, and delegates to safe implementation
modules. `src/block.rs`, `src/hc.rs`, `src/frame.rs`, `src/file.rs`, and
`src/xxh.rs` use `#![forbid(unsafe_code)]`.

`make unsafe-count` enforces the location policy. The number of occurrences is
reported as evidence, not used as a manually maintained limit. The important
invariant is that no unsafe operation enters codec logic.

## 7. Preserve Overlapping-Buffer Semantics

Supported APIs permit source and destination ranges to overlap. Constructing
separate `&[u8]` and `&mut [u8]` values over those ranges would violate Rust's
aliasing rules. The FFI layer detects overlap and represents it as ranges within
one mutable slice. Safe codec functions operate on that slice and its indices.

## 8. Compatibility-Critical Implementation Choices

### Compression tables and indices follow C

Fast compression selects `byU16` for small inputs and `byU32` for larger or
streaming inputs. The table width changes both storage and hash shift. Streaming
indices are relative to `currentOffset`, including the deliberate 64 KiB offset
advance used to make stale entries distinguishable.

HC state similarly uses absolute index space over prefix, external dictionary,
and current input. `SrcView` keeps contiguous prefix/current data together so
forward counting and backward catch-up cross the same boundaries as C.

### Match copies preserve LZ4 overlap behavior

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

### Pointer-style bounds comparisons are expressed additively

Upstream often compares against pointers such as `oend - 32`. On a buffer
smaller than the margin, those conceptual pointers lie before the buffer.
`usize::saturating_sub` does not preserve that ordering and can incorrectly
enable a fast path at offset zero.

The Rust decoder writes equivalent checks additively, for example
`op + 32 <= oend` and `cpy + MFLIMIT > oend`. This avoids underflow while
preserving C's branch decision. Detailed examples are in
[PORTING.md](PORTING.md).

### Prefix history is addressed lazily

Prefix decode APIs describe history contiguous with the destination. The
decoder must not copy or eagerly read all nominal history: callers can provide
an addressable prefix that no encoded match actually references. The FFI layer
forms one indexed region spanning history and output, and the decoder reads
history only when a match requires it.

### Frame decode history is content-owned

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

## 9. Known Divergences and Unverified Areas

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

## 10. Upstream Finding

`LZ4_compress_destSize_extState` is declared in `lz4.h` without `LZ4LIB_API` or
`LZ4LIB_STATIC_API`, although the symbol is present in `liblz4.a`. ELF static
builds still export it, but a Windows DLL build can omit it because the missing
macro normally supplies `__declspec(dllexport)`. The ABI generator found the
discrepancy by comparing header declarations with symbols in the compiled
archive.
