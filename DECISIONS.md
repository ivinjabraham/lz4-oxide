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
the 692-line vendored xxHash dependency gives 6,284 C SLOC — under the
hackathon's 8,000-line ceiling. Porting `programs/` (the CLI, benchmark tool,
and their support code) as well would push the total past that ceiling, so it
stays in C.

That turns out to help rather than just avoid a limit: since the CLI links
against this port's library instead of the original, every one of upstream's
CLI integration tests exercises the Rust implementation end to end, without
the CLI itself needing to be ported at all.

# 2. Find caller-allocated layouts with build.rs 

Some structs in this port are never allocated by our own code at all. The C
test suite declares them itself — `LZ4_stream_t stream;`, sitting on its own
stack and just hands us a pointer into memory it already owns. For that to
work, our Rust version of the struct has to have the exact same size and byte
layout as C's, because C's code reads and writes specific offsets inside it no
matter what Rust thinks the layout should be.

Those sizes can't just be typed in once and trusted, because they aren't
fixed. `LZ4_STREAM_MINSIZE` depends on `LZ4_MEMORY_USAGE`, a build-time knob
upstream lets range from 10 to 20 and alignment isn't uniform either:
`XXH32_state_t` only needs 4-byte alignment where the rest of these types need
8. So instead of guessing, `build.rs` compiles a small C program against the
real headers and asks the compiler directly how big and how aligned each
struct actually is, and `src/types.rs` asserts the Rust definitions against
those measured numbers at compile time. 

`LZ4F_CustomMem` needs the same care for a different reason: it's passed **by
value**, not by pointer, into `LZ4F_createCDict_advanced` and
`LZ4F_createCompressionContext_advanced`. 

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

Some callers don't want the system allocator at all so the `_advanced` create functions accept an
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
and addressing them by offset instead of owning them separately as `Vec`s.

# 4. xxHash

LZ4 uses xxHash for checksums, and vendors its own copy of it rather than
depending on it externally — so this port needs an xxHash implementation too.
The obvious move is to pull in an existing Rust crate instead of hand-writing
a hash function.

That doesn't work here, for the same reason structs in section 2 have to be
probed rather than guessed: `XXH32_state_t` and `XXH64_state_t` are
caller-allocated too, their layout fixed by the upstream headers, and no
crate exposes its internal hash state in a way that matches that exact
layout. Using a crate only for the one-shot `XXH32()`/`XXH64()` calls and
hand-writing just the streaming state would also leave two separate
implementations of the same algorithm living in one library.

So `src/xxh.rs` implements both the one-shot and streaming versions directly
over the public layouts. `xxhash-rust` still appears in the tree, but only as
a test oracle: Rust tests hash the same input with both implementations and
compare, and it is never linked into the shipped library.

# 5. Confine Unsafe Code to the FFI Boundary

Every function this library exports takes raw C pointers — there's no way
around that, since C is the one calling us. Turning a raw pointer into
something Rust can safely work with, a slice or an owned value, is
inherently an `unsafe` operation: the compiler can't prove the pointer is
valid, it can only trust that the code checked.

The strategy is to do that conversion in exactly one place. `src/ffi.rs` is
the only module allowed to use `unsafe` at all — it validates whatever C
handed us, turns it into a slice or an owned Rust value, and immediately
calls into ordinary safe code. Every other module — `src/block.rs`,
`src/hc.rs`, `src/frame.rs`, `src/file.rs`, `src/xxh.rs` — carries
`#![forbid(unsafe_code)]`, so the compiler itself refuses to build an
`unsafe` block anywhere in the actual codec logic.

`make unsafe-count` checks this automatically and reports how many `unsafe`
blocks exist in total. That number is evidence of how small the boundary is,
not a budget to hit — what matters is that the count outside `src/ffi.rs` is
zero.

# 6. Overlapping-Buffer Semantics

Some of LZ4's APIs are explicitly designed to let source and destination
overlap — compressing or decompressing a buffer in place, writing over data
it's still reading from, so the caller doesn't need a second allocation. The
natural Rust translation of "read from src, write to dst" is two separate
references: an immutable `&[u8]` for the source and a mutable `&mut [u8]` for
the destination. That's fine as long as they never overlap — but Rust's
aliasing rules forbid a mutable and an immutable reference into the same
memory at the same time, so the natural translation is illegal exactly when
the API needs it most.

Instead, the FFI layer represents both source and destination as ranges
inside one single mutable slice. There's only ever one reference into the
memory, so there's nothing for the aliasing rules to object to; the safe
codec functions underneath just index into that one slice using the source
and destination ranges instead of holding two references.

# 7. Match Copies Preserve LZ4 Overlap Behavior

LZ4 encodes repeated data as a match: "copy this many bytes from this far
back in the output." Nothing requires the copy length to be shorter than
that distance — an offset of 1 and a length of 50 means "repeat the last
byte 50 times," which only works if the copy reads bytes that earlier steps
of the very same copy just wrote. A plain `memcpy` assumes source and
destination don't overlap and is free to read the whole source before
writing any of it, which gives the wrong output here. The decoder instead
uses a copy that's aware of this: fixed-width copies for short matches, and
a doubling copy — copy what exists so far, then double the copied region,
repeat — for long repeated runs, so each step only ever reads bytes an
earlier step already wrote.

C's decoder goes further for speed: it deliberately overshoots, writing a
few bytes past where the match logically ends, because the buffer always has
slack there and overwriting a little extra is cheaper than checking a length
on every step. This port reproduces that same overshoot, but only where the
very same margins C relies on prove the extra bytes are safe to write.

Below about 32 bytes, though, the fixed-width approach loses to a plain
`memcpy`/`memmove` call — a threshold C doesn't need, because its wildcopy
has no per-call overhead to pay off. Skip the cutoff and literal-heavy input
loses roughly a third of its decode throughput to `memcpy` calls that are
each only moving a handful of bytes.

# 8. Pointer-Style Bounds Comparisons Are Expressed Additively

C frequently writes bounds checks as pointer subtraction, like
`if (op <= oend - 32)`. That works in C even when the buffer is smaller than
the margin: `oend - 32` just becomes a pointer value that happens to sit
before the start of the buffer, and comparing `op` against it is still
meaningful. Rust has no such trick — a buffer's remaining length is an
unsigned integer, and `oend - 32` on a small buffer would try to go
negative. The obvious fix, `usize::saturating_sub`, clamps that result to
zero instead of panicking, but zero is not the number C's negative pointer
difference represents, so the comparison can silently take the wrong branch
on small inputs — exactly the branch C's version would have rejected.

The decoder instead writes these checks with the addition on the other side,
like `op + 32 <= oend`. That can never underflow, and it gives the identical
true/false answer C's pointer subtraction does for every input size,
including the small ones where the naive Rust translation gets it wrong.

# 9. Prefix History Is Addressed Lazily

When a caller decodes with a prefix — history that sits immediately before
the destination buffer in memory — the decoder is allowed to have matches
reach back into that history. The tempting Rust translation is to copy the
whole prefix into a working buffer up front, so the decoder only ever deals
with one owned region. That's wrong here: a caller can legally hand over a
prefix that's addressable in principle but not actually backed by real
memory beyond what the compressed data will ever reference — C's own decoder
never touches those bytes unless an actual match points at them, so nothing
goes wrong. Reading the whole prefix eagerly would touch memory the caller
never promised was there.

So the FFI layer instead forms one indexed region spanning both the prefix
and the output, without copying anything into it, and the decoder only reads
a byte of history at the moment some match's offset actually reaches back
that far — exactly as lazily as C does.

# 10. Frame Decode History Is Content-Owned

Frame-format decoding can span many blocks, and later blocks' matches can
reach back into up to 64 KiB of data decoded by earlier blocks — the decoder
has to remember that history somewhere. C is careful about *where*:
sometimes the history is literally still sitting in the caller's own output
buffer, in which case nothing needs to move, and C tracks that case
specifically to skip a copy that isn't necessary.

This port doesn't make that distinction. It always keeps its own owned
64 KiB copy of the history, regardless of where the caller's buffer happens
to be. Decoding only ever depends on what bytes the history contains, never
on their address, so this can't change the decoded output — it only costs
one extra copy each time the history advances that C would sometimes have
skipped.

The same simplification is not safe on the compression side, and this port
doesn't apply it there. There, which physical arrangement is in play —
`withPrefix64k` versus `usingExtDict` — changes which matches the *search*
finds, which is a real difference in output, not just bookkeeping.
Collapsing the two paths the same way once did change the compressed bytes:
it cost 6 bytes per multi-block frame, a difference invisible to a
round-trip test and caught only by comparing bytes directly against C.

# 11. Known Divergences and Unverified Areas

A few places where this port knowingly doesn't match C, or hasn't been
checked as thoroughly as the rest:

**Frame compression at levels 3 and above** routes through the fast block
compressor instead of the real HC compressor. The output is still valid
LZ4 — it decodes correctly — but it isn't the same bytes C would produce,
and it can be larger.

**Malformed frame input** isn't covered by the differential rejection
testing that block decoding gets. Block-level malformed input is checked
byte-for-byte and position-for-position against C's rejection behavior; the
frame decoder's rejection behavior on bad input hasn't been verified the
same way.

**Custom allocators on the frame path** only own the context struct, not its
internal working buffers (section 3), so `LZ4F_cctx_size` reports a smaller
number than a real C build would for the same context.

**A null `prefixStart` in the HC path can reach `slice::from_raw_parts` in
`src/ffi.rs`.** In a debug build this triggers Rust's own
undefined-behavior check and aborts before the fuzzer gets to run any of its
own assertions. Release builds are unaffected — the debug check simply isn't
compiled in.

**The decoder's `LZ4_FAST_DEC_LOOP` was actually attempted, not skipped.**
It was implemented, verified byte-identical to C, and measured — then
reverted, because it was slower than the existing decode loop on every input
tried, by as much as 22% in the worst case. C's version is fast because it
jumps directly into the middle of another loop with a `goto`; Rust has no
equivalent, so the port had to do that loop's bail-out work up front on
every single sequence instead, which cost more than it saved. The two-stage
shortcut this port already had stays as the fast path, and
many-short-sequence throughput remains lower than C's.

**`common_bytes` clamps an invalid index rather than panicking on it in
release builds**, after already asserting against it in debug. A broken
internal precondition — one that should never happen — would therefore show
up as quietly wrong output in release, rather than a hard crash.

**Cargo and C must be built against the same `LZ4_MEMORY_USAGE` value.**
Nothing propagates a test-side override of that setting into the Cargo
build, so overriding only the C side would make the two sides disagree about
a caller-allocated struct's size (section 2).

Performance measurements, environment details, and methodology for all of
this live in [`bench/results.json`](bench/results.json) and
[`bench/methodology.md`](bench/methodology.md), not here.

# 12. Upstream Finding

Every function liblz4 exports is normally declared with a macro —
`LZ4LIB_API` or `LZ4LIB_STATIC_API` — that expands to whatever the platform
needs to make a symbol visible outside the library. On Windows that's
`__declspec(dllexport)`; without it, a function simply isn't part of the
DLL's exported surface. `LZ4_compress_destSize_extState` is missing that
macro in `lz4.h`, even though every neighboring declaration has one.

On this port's own build (ELF, Linux) that's invisible — ELF doesn't need
the macro to export a symbol, so the function shows up in `liblz4.a` either
way. It would only actually break a Windows DLL build, where the function
would silently fail to be exported. This was found because the tool that
generates this port's FFI surface cross-checks every header declaration
against the real compiled archive's symbol table, rather than trusting the
headers alone, and flagged the one declaration that didn't match the
pattern.
