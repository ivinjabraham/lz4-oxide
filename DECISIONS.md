# Decision Log: LZ4 C to Rust

This file records the decisions that define the port: its scope, ABI strategy,
safety boundary, compatibility requirements, and known divergences. Current
work belongs in [PLAN.md](PLAN.md), porting traps belong in
[PORTING.md](PORTING.md), and generated performance numbers belong in
[`bench/results.json`](bench/results.json).

The reference implementation is upstream LZ4 commit
`0774d05537f9762f838f7ab541b7765f1a729cb5`.

## 1. Scope

### Port the library, not the CLI

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

The library itself creates no threads and holds no locks. Its concurrency
contract is therefore the absence of hidden mutable global state and the
ability to share immutable objects such as `LZ4_CDict`. The implementation uses
caller-owned or context-owned state and no mutable globals. The upstream CLI's
threaded tests exercise this design on covered paths, but they are not a race
detector.

### This is not a dependency wrapper

`lz4_flex` is an independent Rust implementation of the LZ4 format. It does not
provide liblz4's complete C ABI, caller-allocated state layouts, frame surface,
or all HC strategies. This project ports the pinned repository's API and
behavior and does not depend on `lz4_flex`.

## 2. ABI and Build Architecture

### Export the original library ABI

The crate builds as `staticlib`, `cdylib`, and `rlib`. The C-facing artifacts
export the same 141 `LZ4_*`, `LZ4F_*`, and namespaced `LZ4_XXH*` symbols as the
pinned `liblz4.a`.

The symbol contract was derived from the compiled C archive rather than only
from headers. `make abi-check` compares that committed contract with the Rust
archive. `tools/gen_ffi.py` was used to bootstrap `src/ffi.rs`; running
`make gen-ffi` now overwrites implemented bodies and must not be part of normal
development.

### Run the unmodified upstream tests against Rust

Upstream tests compile `lib/*.c` directly instead of linking a prebuilt
`liblz4.a`. The project redirects that build through two ordinary Make
variables:

- `C_SRCDIRS` replaces upstream library sources with empty translation units in
  `cstub/`.
- `LDLIBS` links `target/release/liblz4_rs.a` and Rust's native static-library
  dependencies.

The test Makefile clears `MAKEFLAGS` while building the CLI, so the CLI is built
separately with the same overrides and marked complete when the suite runs.
Without that step, shell tests can pass while exercising C rather than Rust.

Upstream's object cache does not include `C_SRCDIRS` in its cache key. A stale C
`lz4.o` can therefore be linked into an apparently Rust-backed test binary.
`make provenance-check` inspects each binary's `lz4.d` and requires its primary
`lz4.o` to come from `cstub/`. It catches the known stale-object failure mode;
it is not a complete audit of every object in the binary.

`test-amalgamation` is the one deliberate exception to the statement that
upstream C implementation sources are not used by test binaries. It compiles an
amalgamated C source as a standards-conformance check but does not link that
object into a tested executable.

## 3. Layout and Ownership

### Probe caller-allocated layouts

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

## 4. xxHash

The vendored xxHash implementation is ported in-tree rather than linked from C
or delegated to a crate.

The public `XXH32_state_t` and `XXH64_state_t` layouts are fixed by upstream
headers and are allocated by C callers. A crate's private state cannot satisfy
that ABI, and using a crate only for one-shot hashes would create two
implementations of the same algorithm in one library.

`src/xxh.rs` therefore implements one-shot and streaming XXH32/XXH64 over the
public layouts. `xxhash-rust` is a development-only oracle used by Rust tests;
it is not linked into the shipped library.

## 5. Safety and Errors

### Confine unsafe code to the FFI boundary

`src/ffi.rs` is the only module allowed to use `unsafe`. It validates C
arguments, forms slices or owned handles, and delegates to safe implementation
modules. `src/block.rs`, `src/hc.rs`, `src/frame.rs`, `src/file.rs`, and
`src/xxh.rs` use `#![forbid(unsafe_code)]`.

`make unsafe-count` enforces the location policy. The number of occurrences is
reported as evidence, not used as a manually maintained limit. The important
invariant is that no unsafe operation enters codec logic.

### Preserve overlapping-buffer semantics

Supported APIs permit source and destination ranges to overlap. Constructing
separate `&[u8]` and `&mut [u8]` values over those ranges would violate Rust's
aliasing rules. The FFI layer detects overlap and represents it as ranges within
one mutable slice. Safe codec functions operate on that slice and its indices.

### Keep C error encodings at the boundary

liblz4 uses several incompatible C conventions: compression returns zero on
failure, safe decompression returns position-encoded negative values, frame
functions return encoded `size_t` errors, and xxHash returns status enums.

Block, frame, and file implementation paths use Rust error types internally,
with `src/ffi.rs` translating them to the required C representation. Some HC
internals retain `(written, consumed)` and zero-valued failure sentinels because
their fill-output behavior has two observable results. Regardless of internal
representation, only the original C conventions cross the ABI.

## 6. Behavioral Equivalence

### Require byte-identical deterministic output

Round-trip tests cannot detect a compressor that emits valid but different
bytes. Deterministic compression must therefore preserve upstream hash
functions, table sizing, search order, skip heuristics, tie-breaking, and output
limits exactly.

Platform-dependent upstream behavior remains platform-dependent in the port.
Hash reads use native endianness where C does, encoded offsets use little
endianness, and table selection follows pointer width and probed configuration.

### Compare rejection positions, not only valid output

Safe decompression encodes the input position of a failure in its negative
return value. Differential tests compare both produced bytes and exact return
values on malformed and truncated input. This catches bounds decisions that
ordinary round trips cannot observe.

`make difftest` currently covers:

- Fast block compression across table types, capacities, and input patterns.
- Streaming and dictionary state transitions.
- HC levels 1 through 12, including fill-output behavior and state transcripts.
- Frame compression for eight preference combinations: linked and independent
  blocks, checksums, block sizes, declared content size, and fast acceleration.
- Block decompression rejection parity around the decoder's fast-path margins.

The command does not currently cover malformed frame decompression or HC levels
3 and above through the frame API. A zero-divergence result applies only to the
matrix above.

### Keep verification claims reproducible

`make test` runs the unmodified upstream suite. `make provenance-check` checks
the primary `lz4.o` provenance needed to attribute that result to the port.
`make difftest` checks output and rejection behavior that the suite cannot see.
None of those commands alone establishes all three properties.

## 7. Compatibility-Critical Implementation Choices

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

## 8. Known Divergences and Unverified Areas

| Area | Current behavior | Consequence |
|---|---|---|
| Frame compression levels 3 and above | Frame compression routes through the fast block compressor rather than HC. | Output is valid but can differ from C and may be larger. |
| Malformed frame input | Differential rejection testing covers block decoding, not frame decoding. | Exact frame error parity is unverified. |
| Frame custom allocators | Hooks own contexts but not internal `Vec` buffers. | Allocator ownership and `LZ4F_cctx_size` semantics differ from C. |
| Debug HC calls | A null `prefixStart` can reach `slice::from_raw_parts` in `src/ffi.rs`. | Debug builds abort before the fuzzer can exercise assertions; release behavior is unaffected. |
| Decoder fast loop | The safe decoder ports the two-stage shortcut but not upstream's full `LZ4_FAST_DEC_LOOP`. | Correctness is covered for the current loop; many-short-sequence throughput remains lower. |
| Release fail-loudness | `common_bytes` clamps invalid indices after a debug assertion. | A broken internal precondition can become wrong output rather than a release panic. |
| Build-time memory configuration | Cargo and C must use the same probed `LZ4_MEMORY_USAGE`. | Independently overriding only the C side can corrupt caller-allocated state. |

The benchmark runner records the frame-HC divergence alongside its generated
results. Performance measurements, environment details, and methodology live in
[`bench/results.json`](bench/results.json) and
[`bench/methodology.md`](bench/methodology.md), not in this log.

## 9. Upstream Finding

`LZ4_compress_destSize_extState` is declared in `lz4.h` without `LZ4LIB_API` or
`LZ4LIB_STATIC_API`, although the symbol is present in `liblz4.a`. ELF static
builds still export it, but a Windows DLL build can omit it because the missing
macro normally supplies `__declspec(dllexport)`. The ABI generator found the
discrepancy by comparing header declarations with symbols in the compiled
archive.

## 10. Verification

Use these commands for current evidence rather than copying volatile counts or
performance values into this file:

```sh
make difftest          # byte identity and covered rejection parity
make test              # complete unmodified upstream suite
make abi-check         # exported symbol contract
make provenance-check  # each binary's lz4.o comes from cstub/
make kickoff-verify    # pinned upstream and unmodified tests
make unsafe-count      # unsafe remains confined to src/ffi.rs
cargo test             # Rust unit tests and xxHash oracle checks
bench/bench.py         # regenerate benchmark results and environment metadata
```
