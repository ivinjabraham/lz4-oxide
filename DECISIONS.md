# DECISIONS.md — LZ4 (C) → Rust

Port Mortem Hackathon 2026, Track A (C → Rust).

Everything below records *why* the port is shaped the way it is. Where a claim
is checkable, the command that checks it is included.

Two parts, because they answer different questions. **Part I** is about the
entry: what we chose to port, why the repository qualifies, and how lz4's own
test suite runs against Rust without a single edit. **Part II** is about the
code: the artefact's shape, the layouts it must match, and the policies that
govern what goes inside it. §0 is the evidence table for both.

---

## 0. Verification status

Last verified 2026-08-01 on x86_64-unknown-linux-gnu, rustc 1.97.1, gcc 16.1.1,
against upstream lz4 pinned at `0774d055`.

| Claim | Command | Result |
|---|---|---|
| Rust archive exports exactly the original ABI | `make abi-check` | **141/141, zero diff** |
| Original C tests link against the port | `make link-check` | **pass** (`fuzzer`, `frametest`) |
| The linked binaries really call Rust | `upstream/tests/fuzzer -i1` | **panics in `src/ffi.rs`** |
| Upstream tree unmodified | `git -C upstream status --short` | **empty** |
| Original test files match their kickoff hashes | `make kickoff-verify` | **42/42** |
| `unsafe` confined to `src/ffi.rs` | `make unsafe-count` | **0 occurrences so far** |
| Every built test binary came from `cstub/`, not `lib/` | `make provenance-check` | **pass** |
| C reference suite is green *on this host* | `make test-reference` | **exit 0** ([tests/README.md](tests/README.md#the-c-baseline--our-denominator)) |

Two of those rows carry the weight. **Row 3** proves the C harness reaches Rust:
linking only shows that symbol *names* resolved, whereas running the binary
shows the original `fuzzer` aborting inside our `unimplemented!()` stub.
**Row 5** proves the binary it ran was built from our stub directory rather than
from a cached object compiled out of `lib/` — `multiconf.make` keys its object
cache on the compiler and link flags but not on `C_SRCDIRS`, so without that
check a stale C object can be relinked silently and every other row here stays
green. It has caught this for real once.

```
thread '<unnamed>' panicked at src/ffi.rs:858:5:
not implemented: LZ4_versionString
```

That is lz4's own, unedited test program calling into this port.

**Not yet true:** no function is implemented. Test *pass* rate is currently
zero by construction. The skeleton is proven; the port is not written.

The last row is the **denominator**, and it is worth having before writing any
code: the original suite passes 100% for C on this machine, so every failure the
port shows from here is attributable to the port. Without that baseline, hours
get lost debugging "failures" that were never ours.

---

# Part I — The entry: scope, eligibility, and the proof strategy

What we took on, why it qualifies under the rules, and how the original test
suite ends up testing Rust.

## 1. Scope

We port the **library** — `lib/` — and leave the command-line tool in C.

| File | SLOC (non-blank, non-comment) |
|---|---|
| `lib/lz4.c` | 2,036 |
| `lib/lz4hc.c` | 1,791 |
| `lib/lz4frame.c` | 1,519 |
| `lib/lz4file.c` | 246 |
| **Ported total** | **5,592** |
| `lib/xxhash.c` (vendored dependency, see §6) | 692 |
| Total including xxhash | 6,284 |

Counted with:

```sh
for f in lib/lz4.c lib/lz4hc.c lib/lz4frame.c lib/lz4file.c lib/xxhash.c; do
  n=$(grep -v '^\s*$' $f | grep -v '^\s*[/*]' | wc -l); echo "$n  $f"
done
```

Both figures sit inside the 1,000–8,000 line eligibility window, so the port
qualifies whether or not the vendored dependency is counted.

**Why not the CLI.** `programs/` is another **4,511 SLOC** (`lz4io.c` 2,907 raw
lines, `lz4cli.c` 896, `bench.c` 865, `threadpool.c` 430, plus support). Porting
it too would put the project at 10,795 SLOC — past the 8,000 ceiling, which the
organisers set precisely because larger entries do not finish.

*(An earlier revision of this section said 6,982 lines. That was `programs/*.c`
plus `*.h` counted raw, compared against a non-blank/non-comment figure for
`lib/` — apples to oranges, and it flattered the argument. Both numbers here are
now SLOC on the same basis. The conclusion is unchanged: still over the
ceiling.)*

Leaving `lz4cli.c` / `lz4io.c` in C is also *positively* useful: the C CLI links
against our Rust library, so the `tests/test-lz4-*.sh` shell tests exercise the
port end to end.

### 1.1 Concurrency: there is none in `lib/`, and that is the point

The organisers' framing calls out "same concurrency semantics." Worth stating
plainly where concurrency actually lives in this codebase, because the answer
shapes what we owe:

```sh
grep -rlniE 'pthread|thread|mutex' lib/*.c     # lz4frame.c — comments only
grep -rlniE 'pthread|thread|mutex' programs/*.c # threadpool.c, lz4io.c, lz4cli.c, util.c
```

`lib/` spawns no threads and holds no locks. Its only concurrency surface is a
**contract**: `LZ4_CDict` "can be created once and shared by multiple threads
concurrently, since its usage is read-only" (`lz4frame.h:596`), and every other
API is caller-allocated with no global mutable state. Reproducing that contract
means our port must have no hidden `static mut`, no lazily-initialised global
table, no interior mutability behind a shared reference — which Rust enforces
for us far more strictly than C did.

This is not a gap dodged, but be precise about its status: it is a claim we
have **set up to be checked, not yet checked**. The C CLI's threadpool drives
the library multi-threaded, so once functions exist, `make test` exercises our
Rust concurrently and any hidden global mutable state would corrupt output or
trip a race. The `Using 6 threads for compression` line quoted from the baseline
run came from the **C** library, not from this port — no function is implemented
yet. Re-state this section with real evidence once the suite runs green.

That coverage is **not automatic** — it has to be wired deliberately, and
getting it wrong yields shell tests that pass while testing the C library. See
§3.1.

---

## 2. Eligibility: no pre-existing port

The rules bar porting a repository that has already been ported to the target
language. A pure-Rust LZ4 implementation (`lz4_flex`) does exist, and we want
to be explicit about why it does not disqualify this entry.

`lz4_flex` is an **independent reimplementation of the LZ4 wire format**, not a
port of this repository. It does not expose liblz4's C ABI, does not implement
the LZ4HC optimal parser, and covers only part of the frame API. What we are
porting is *this codebase* — its API surface, its 141 exported symbols, its
semantics, and its test suite. We did not read or depend on `lz4_flex`.

> **TODO (team):** paste the organisers' green-light ruling here verbatim,
> with the date and where it was given.

---

## 3. Running the original test suite with zero edits to `tests/`

This needed care, because the obvious approach silently does nothing.

**The trap:** lz4's tests do *not* link against a prebuilt library, The Makefile instead builds its own test binaries by compiling the c source files:

`tests/Makefile:122` builds
`fuzzer` from `lz4.o lz4hc.o xxhash.o fuzzer.o`, and
`build/make/multiconf.make:148` resolves those through
`vpath %.c $(C_SRCDIRS)`, where `C_SRCDIRS = ../lib ../programs .`.

A Rust library dropped into `lib/` would be **bypassed entirely** as the C sources
would still be compiled, the tests would pass, and they would be testing
nothing.

**The solution:.** Two ordinary make variables, overridden on the command line:

```sh
make -C tests test \
  C_SRCDIRS="$LZ4_OXIDE/cstub $LZ4_SRC/programs ." \
  LDLIBS="$LZ4_OXIDE/target/release/liblz4_rs.a $(rustc --print native-static-libs ...)"
```

* `C_SRCDIRS` drops the upstream `../lib` and substitutes `cstub/`, which holds five
  intentionally-empty translation units (`lz4.c`, `lz4hc.c`, `lz4frame.c`,
  `lz4file.c`, `xxhash.c`). The object names the makefile expects still get
  produced; no C implementation object is linked into any test binary.
* `LDLIBS` is appended to every link line by `multiconf.make:222`, so our Rust
  archive resolves the symbols.

**No file under `tests/` is modified.** Not the C sources, not the shell
scripts, not `tests/Makefile`. This is wrapped up as `make test`.

**One honest caveat.** `make test` includes `test-amalgamation`, whose rule
(`tests/Makefile:207`) is `cat ../lib/lz4.c ../lib/lz4hc.c ../lib/lz4frame.c >
lz4_all.c` and then compiles the result under `-std=c90 -Werror`. It names the
paths literally, so `C_SRCDIRS` cannot redirect it, and the object is real: 116
defined symbols. It is **never linked into anything** — it is upstream's
standards-conformance check on its own sources — so no test binary contains C.
But it means "no `lib/*.c` is ever compiled" would be false, and we do not say
it. It also means one green test in the suite (`test-amalgamation`) would pass
identically if this port did not exist.

### 3.1 The same trap, one level up: the CLI

`tests/Makefile:68-71` builds the CLI via a sub-make:

```make
lz4: MAKEFLAGS=
lz4:
	$(MAKE) -C $(PRGDIR) $@ CFLAGS="$(CFLAGS)"
```

Command-line variable overrides reach sub-makes through `MAKEFLAGS`. Clearing
it means `C_SRCDIRS` and `LDLIBS` **do not propagate into `programs/`**. Left
alone, `make -C tests test` therefore builds the CLI from the C library, and
every `tests/test-lz4-*.sh` passes while proving nothing about the port.

`programs/Makefile:68` fortunately uses the same mechanism
(`C_SRCDIRS = $(LIBLZ4DIR) .` plus `multiconf.make`), so the identical hook
works there — it just has to be invoked explicitly. our `Makefile` builds the
CLI as its own step with its own overrides, then runs the suite with `-o lz4`
so the sub-make does not relink the CLI against C behind our back.

A pleasant side effect: `multiconf.make` keys its object cache on a hash of the
build flags, so the C reference build and the Rust build occupy different cache
directories and can coexist. That is what the differential harness compares.

### 3.2 On "no source-language runtime linking"

The rule forbids leaning on the *source language's runtime* (the cited example
is Python→Rust calling the Python interpreter). C has no such runtime, and we
link none. What remains in C is the **test harness itself**, which is the thing
we are required to keep unmodified — plus the CLI, by choice (§1).

Auditable claim: every `LZ4_*` / `LZ4F_*` symbol in the test binaries resolves
into Rust, and no `lib/*.c` object participates in the link. `make abi-check` diffs the Rust archive's exports against the original's.

---

# Part II — Engineering decisions

How the port is built: the shape of the artefact, the types it must match, and
the policies (`unsafe`, error handling) that govern the code inside it.

## 4. Architecture: a Rust staticlib wearing liblz4's ABI

The single most important constraint is that lz4's test suite is written in
**C**. `tests/fuzzer.c`, `tests/frametest.c`, `tests/roundTripTest.c` and
friends call the library directly. To run those tests unmodified, our Rust code
has to be callable *as C*.

So the crate builds as `crate-type = ["staticlib", "cdylib", "rlib"]`,
producing `liblz4_rs.a`, and exports one `#[no_mangle] extern "C"` function per
symbol the original archive exports.

The symbol contract was taken from the real artefact rather than from the
headers, so nothing is missed:

```sh
make -C lib liblz4.a
nm --defined-only --extern-only lib/liblz4.a | awk '$2 ~ /^[TDBR]$/ {print $3}' | sort -u
```

**141 symbols**: 51 core block codec, 32 high-compression, 39 frame/file, 19
namespaced xxHash. `tools/gen_ffi.py` parses `lib/*.h`, generates the
`extern "C"` skeleton, and cross-checks it against that list — it exits
non-zero if any exported symbol has no stub. Regenerate with `make gen-ffi`.

---

## 5. Caller-allocated state, and why sizes are probed not hardcoded

lz4's API is caller-allocated. The C tests declare state **on their own stack**
and hand us a pointer — e.g. `LZ4_stream_t stream;` in `fuzzer.c`, and
`XXH64_state_t xxh64;` at `frametest.c:1202`. Consequences:

1. Our Rust types must be `repr(C)` and match the C size and alignment exactly.
   No `Box`, no `Vec`, no `String` in these structs — they must be
   initialisable in place through a raw pointer.
2. The sizes **cannot be hardcoded**. `LZ4_STREAM_MINSIZE` is
   `(1UL << LZ4_MEMORY_USAGE) + 32`, and upstream permits `LZ4_MEMORY_USAGE` to
   range 10..20 (`lz4.h:162-164`), which moves the struct size by three orders
   of magnitude.

   *Scope correction:* an earlier revision claimed `tests/Makefile:214-215`
   rebuilds the suite at both extremes. That target
   (`test-compile-with-lz4-memory-usage`) only **compiles**, and is not a
   prerequisite of `test:` — so `make test` never exercises either extreme.
   Worse, nothing currently plumbs `LZ4_MEMORY_USAGE` from the C build into
   cargo, so running that target with our overrides would leave the Rust side
   at the probed 14 while C allocated a different size: silent stack
   corruption, not a caught assert. Probing rather than hardcoding is still
   right; the justification above is the honest one.

So `build.rs` compiles and runs a small C probe against the real headers
and emits the true numbers as Rust constants. At the default
`LZ4_MEMORY_USAGE=14`:

| Type | size | align |
|---|---|---|
| `LZ4_stream_t` | 16416 | 8 |
| `LZ4_streamHC_t` | 262200 | 8 |
| `LZ4_streamDecode_t` | 32 | 8 |
| `XXH32_state_t` | 48 | **4** |
| `XXH64_state_t` | 88 | 8 |
| `LZ4F_preferences_t` | 56 | 8 |
| `LZ4F_frameInfo_t` | 32 | 8 |
| `LZ4F_CustomMem` | 32 | 8 |
| `LZ4F_compressOptions_t` | 16 | 4 |
| `LZ4F_decompressOptions_t` | 16 | 4 |

`src/types.rs` asserts every one of these at compile time. The probe
already earned its keep: `XXH32_state_t` is 4-aligned (all `uint32_t` members)
where every other type is 8-aligned, so a blanket `repr(align(8))` — our first
attempt — was wrong.

`LZ4F_CustomMem` is on the list for a different reason: it is passed **by
value** to `LZ4F_createCDict_advanced` and
`LZ4F_createCompressionContext_advanced`. A by-value struct with a wrong layout
does not fail to link — it silently corrupts. (It lives behind
`LZ4F_STATIC_LINKING_ONLY`, `lib/lz4frame.h:641`, which `programs/Makefile:146`
enables, so it is genuinely part of the shipped ABI.)

---

## 6. xxHash

`lib/xxhash.c` is **vendored**, not lz4 source: it is a copy of
[Cyan4973/xxHash](https://github.com/Cyan4973/xxHash), as its own header states.
Its history in this repo is 19 commits, almost all of the form "updated xxhash
to latest version"; `lib/lz4.c` has 440. It is a dependency that happens to be
checked in.

**Decision: port it too, by hand, with no crate dependency.**

We could have let the genuine C `xxhash.c` keep compiling (simply by omitting it
from our stub directory). We chose not to: it would leave C objects in the
shipped binaries and muddy the "no C implementation remains" claim.

**Reversal (2026-08-01): we no longer use the `xxhash-rust` crate.** The first
version of this section specified it. That is not implementable here, for the
§5 reason: `XXH32_state_t` and `XXH64_state_t` are **fully specified in
`lib/xxhash.h:264-285`** and the C tests declare them *on their own stack* —
`XXH64_state_t xxh64;` at `frametest.c:1202`. The layout is therefore fixed by
the C header:

```c
struct XXH32_state_s {          struct XXH64_state_s {
   uint32_t total_len_32;          uint64_t total_len;
   uint32_t large_len;             uint64_t v1, v2, v3, v4;
   uint32_t v1, v2, v3, v4;        uint64_t mem64[4];
   uint32_t mem32[4];              uint32_t memsize;
   uint32_t memsize;               uint32_t reserved[2];
   uint32_t reserved;           };  /* 88 bytes, align 8 */
};  /* 48 bytes, align 4 */
```

No crate's private state type can match that, and `xxhash-rust` does not expose
its internals, so we cannot even convert between the two at the boundary — the
crate could only ever serve the one-shot `XXH32()`/`XXH64()` entry points, not
the streaming ones. Splitting the two would mean *two* implementations of the
same algorithm in one library, which is worse than either alone.

So `src/xxh.rs` implements both hashes directly over a `repr(C)` state that
mirrors the structs above. This is mechanical, spec-defined code (~250 lines);
byte-exactness is not at risk, and it is covered by the `XXH32_canonical_*` /
`XXH64_canonical_*` round-trips the suite already exercises.

The crate does stay in the tree, as a **`[dev-dependencies]` test oracle**: unit
tests hash the same buffers with both implementations, so ours is checked
against a second implementation rather than only against itself. It is not
linked into `liblz4_rs.a` — `cargo tree --edges normal` shows no dependencies.

Scope note — the surface is larger than lz4 itself needs. LZ4's frame format
only uses XXH32, but the **test harness** uses XXH64 heavily (~20 call sites in
`frametest.c` alone, plus `fuzzer.c`, `roundTripTest.c`, `fullbench.c`). All 19
symbols are exported under the `LZ4_XXH*` prefix because the suite is built with
`-DXXH_NAMESPACE=LZ4_` (`tests/Makefile:43`), so our shim must match those exact
names.

---

## 7. `unsafe` policy

Raw pointers are converted to slices immediately at the boundary and never
propagate. Concretely:

* `src/ffi.rs` — the only module permitted to use `unsafe`. Thin
  entry points: validate, convert, delegate.
* `src/{block,hc,frame,file,xxh}.rs` — each carries
  `#![forbid(unsafe_code)]`. All real logic lives here, on slices.

This keeps the unsafe surface small and, more importantly, *countable*:

```sh
make unsafe-count
```

reports the raw occurrence count, the ported C SLOC it is measured against, the
ratio per 1000 C SLOC, and **fails the build if any `unsafe` appears outside
`src/ffi.rs`**. That last part is the real control; the number is the evidence.
Budget: `unsafe` is permitted only in `ffi.rs`, and there only for
`slice::from_raw_parts{,_mut}` and in-place initialisation of caller-allocated
state. Any other use is a decision that belongs in this file.

### 7.1 Error handling: `Result` inside, C codes only at the boundary

An explicit exit criterion is *"handles error paths idiomatically — `Result`,
not errno translated"*. That is in genuine tension with an ABI-compatible port,
because liblz4's callers — including the unmodified C tests — require the
original integer conventions, and those conventions are not even uniform:
`LZ4_compress_default` returns `0` on failure, `LZ4_decompress_safe` returns a
*negative* value, and the frame API returns a `size_t` that must be fed to
`LZ4F_isError`.

We resolve it by **separating the two representations**, rather than picking one:

* `src/{block,hc,frame,file,xxh}.rs` — the actual implementation — is written
  in idiomatic Rust and returns `Result<_, Error>`, where `Error` is a real
  enum (`MalformedInput`, `OutputTooSmall`, …). No integer sentinels, no
  errno-style out-params, no `-1` propagating through internal call graphs.
* `src/ffi.rs` translates, once, at the outermost frame: `Result` in, C
  integer convention out — per function, because the conventions differ.

So the error *logic* is idiomatic and the error *encoding* is compatible. The
translation is the boundary's job, which is what a boundary is for. A judge
reading `src/block.rs` should see Rust, not transliterated C; a judge running
`tests/fuzzer` should see lz4's exact return values.

The one thing this does **not** do is invent richer errors than the original
reports. Where C collapses several failure modes into a single `0`, we still
return `0` — behavioural equivalence outranks expressiveness at the boundary,
and the differential fuzzer checks exactly that.

---

## 8. Observations about the original

Recorded as we go; candidates for the Bug Catcher category.

1. **`LZ4_compress_destSize_extState` is declared without an export macro.**
   `lib/lz4.h:619` declares it as a bare `int LZ4_compress_destSize_extState(...)`,
   while every neighbouring declaration carries `LZ4LIB_API` or
   `LZ4LIB_STATIC_API`. The symbol *is* exported from `liblz4.a`, so on ELF
   builds nobody notices — but on a Windows DLL build, `LZ4LIB_API` expands to
   `__declspec(dllexport)`, and without it this function would not be exported.
   Found because our generator cross-checks headers against the real archive's
   symbol table and flagged the mismatch.

> **TODO:** the last four upstream commits (`fix_read_oob`, `read_variable_length`
> ilimit bounds, `ip` reaching `iend` in both decode loops) indicate the decode
> bounds logic is freshly subtle. Point the differential fuzzer at truncated and
> malformed input and compare *rejection* behaviour, not just agreement on valid
> input.

---

## 8.1 Frame format: two documented divergences

Both are in `src/frame.rs`, neither changes a compressed byte, and both are
verified against the C library rather than asserted.

**The custom allocator is honoured for the context, not for its buffers.**
`LZ4F_create*_advanced` takes an `LZ4F_CustomMem` — the caller's own
alloc/calloc/free. We route the *context struct* through those hooks
(`Owned<T>` in `src/ffi.rs`), so a caller that supplies an arena gets its
context from that arena and freed back to it. The working buffers inside the
context (`tmpBuff`, `tmpIn`, `tmpOutBuffer`, the history) are still `Vec<u8>`,
i.e. Rust's allocator.

The visible consequence is exactly one assertion:
`frametest.c:1095-1115` installs counting hooks and requires
`LZ4F_cctx_size(cc) == live_alloc_total_space`. Our reported size counts the
buffers; the hooks never saw them, so the numbers differ and `unitTests` fails
there. Everything after that assertion in the same test — including the whole
randomized `fuzzerTests` run, which is the part that exercises the format — is
green:

```sh
./upstream/tests/frametest -i25 -t1    # All tests completed
```

Closing it properly means carving all five buffers out of one hook-allocated
block and tracking them as offsets, which is what C does. That is a real change
to how `Cctx`/`Dctx` address their memory, not a patch, so it is recorded here
rather than half-done. **This is the one unit-test assertion the frame port does
not satisfy.**

**`LZ4F_updateDict` is collapsed.** C's version (lz4frame.c:1558) is a
five-branch juggle over whether the decoder's history currently lives in the
caller's `dst` or in `tmpOutBuffer`, and whether the two happen to be adjacent
in memory — all of it to avoid a copy. We keep an owned 64 KB history and copy
into it, which makes every branch the same branch. The decoded bytes cannot
differ: only the history's *content* feeds the decoder, never its address. The
cost is one memcpy per block.

The same reasoning does **not** license simplifying the compression side, and
we did not: `withPrefix64k` vs `usingExtDict` there changes which matches are
found. Picking the wrong one cost 6 bytes per multi-block frame and was caught
only by byte-comparison against C, never by a round trip — see the note in
`Cctx::make_block`.

### How byte-identity was checked

`fuzz/framediff.c` compresses stdin as a frame and is compiled twice, against
`upstream/lib/liblz4.a` and against `target/release/liblz4_rs.a`. Run over
`datagen` output at 1 KB / 64 KB / 200 KB / 1 MB / 4 MB, each at `-P10/50/90`,
across eight preference combinations (default, independent blocks, content
checksum, block checksum, 256 KB and 4 MB block sizes, declared content size,
and fast level -3): **120 comparisons, all byte-identical.**

---

## 8.2 HC: `lz4mid` is ported, the two parsers above it are not

`lz4hc.c:420-436` picks one of three match finders by level, and `src/hc.rs`
implements the first of them:

| Level | C strategy | Ported | Byte-identical to C |
|---|---|---|---|
| 1-2 | `lz4mid` | ✅ yes | ✅ **yes**, verified |
| 3-9 | `lz4hc` (hash chain) | ❌ no | ❌ no — routed to `lz4mid` |
| 10-12 | `lz4opt` (optimal parser) | ❌ no | ❌ no — routed to `lz4mid` |

This is the **"degrade, don't delete"** fallback named in PLAN.md §6.1, applied
in the other direction than that section anticipated: rather than routing 10-12
down to the level-9 hash chain, every level above 2 currently lands on `lz4mid`.
The consequence is the same in kind — the output is well-formed LZ4 that
round-trips and passes every CRC check in the suite, so `fuzzer` and `frametest`
are green at all 13 levels — but a caller asking for level 9 gets level-2
compression. Concretely, on 120 KB of 4-symbol noise: C emits 49,277 bytes at
level 9 and 46,773 at level 11, where we emit 56,424 at every level.

**What this costs.** Behavioural equivalence is 30% of the score, and levels 3-12
are 11 of the 13 reachable values. `fuzzer.c:386` draws the level once per cycle
and reuses it across ~15 HC call sites, so most cycles compress through a
strategy whose *bytes* we do not reproduce — they are merely valid. Nothing in
the upstream suite detects this, which is exactly why it is written down here:
round-trip tests cannot see it, and `fuzz/hc_difftest.c` deliberately compares
only levels 1-2 rather than reporting a failure it cannot fix.

**The one thing not to do** is treat the green suite as done. Finishing this
means porting `LZ4HC_compress_hashChain` (with `LZ4HC_InsertAndGetWiderMatch`,
the chain-swap and pattern-analysis paths) and `LZ4HC_compress_optimal`, plus the
`chainTable` maintenance in `LZ4HC_Insert` — which `set_external_dict` and
`LZ4_loadDictHC` also skip today, since no chain exists to maintain. Both skips
are marked at their call sites in `src/ffi.rs` and `src/hc.rs`.

### What *is* verified for levels 1-2

The trap in porting `lz4mid` is that `LZ4HC_CCtx_internal` describes positions
twice over: `prefixStart .. end` is one **contiguous** buffer holding the history
followed by the current block, while `dictLimit`/`lowLimit` give the same bytes
rising absolute indices. `LZ4_count` walks straight across the history/block
seam and the catch-back loop reads backwards through it, so representing the two
as separate slices changes which matches are found — invisibly, since the result
still round-trips. `src/hc.rs` therefore takes one `SrcView::base` slice plus the
block's offset within it, and the module header says so.

Two smaller places where following C exactly matters, both commented in place:

- The "fill table with beginning of match" writes use the `ipIndex` from the top
  of the loop while `ip` itself has already moved (the `ip+1` peek and the
  catch-back), so the stored index can disagree with the position hashed.
  Faithful, and load-bearing.
- `LZ4HC_compress_generic`'s dictCtx dispatch reads `position` **before**
  `ctx->end += *srcSizePtr`. Computing it afterwards silently selects the wrong
  arm; that bug survived a 12,000-case round-trip sweep and was caught only by
  byte-comparison against C.

### How byte-identity was checked

`fuzz/hc_difftest.c` compiles twice — against `upstream/lib/liblz4.a` and
against `target/release/liblz4_rs.a` — and emits a binary transcript of the
whole HC surface: one-shot at generous/exact/one-byte-short capacity, external
state fresh and fast-reset, streaming with a loaded dictionary, `saveDictHC`,
both `destSize` (fillOutput) entry points across a range of capacities, and
compression against an attached dictionary context. Failures and the
`srcSizePtr` written back by the fillOutput calls are emitted too, so the
comparison covers **rejection parity and how much input was consumed**, not just
agreement on valid output. The `dictLimit`/`lowLimit`/`nextToUpdate`/`dirty`
bookkeeping is emitted after each streaming call; pointers are not, being
addresses rather than behaviour.

Run over `datagen` output at 20 KB / 300 KB / 1 MB, each at `-P10/50/90`, at
levels 1 and 2: **18 transcripts, all byte-identical** (`fuzz/driver.sh`).

---

## 8.3 The prefix decode path: history must not be *read*, only addressed

`LZ4_decompress_safe_withPrefix64k` and the prefix arm of
`LZ4_decompress_safe{,_partial}_usingDict` segfaulted — `fullbench`
decompressors #5, #6 and #8 — because `decompress_safe_histories` staged the
decode through a scratch `Vec`: allocate `prefix_size + output_size`, **copy the
prefix in**, decode, copy the result back out.

The copy is the bug. C's decoder never reads the history up front; it reads a
byte of it only when a match offset actually reaches back that far. The two
differ whenever the prefix is nominally addressable but not really there, and
that is not a corner case the suite stumbled into by accident —
`fullbench.c:320` calls

```c
LZ4_decompress_safe_withPrefix64k(in, out, inSize, outSize);   /* out == orig_buff */
```

with `out` the head of a `malloc(benchedSize)`. The 64 KB "prefix" before it is
unmapped. C is fine, because `fullbench` compressed that data with no
dictionary, so no offset points below `out`. We touched all 65,536 bytes and
died. Note that C is *also* reading a pointer it has no right to form here —
this is arguably a latent bug in the harness — but the contract we owe is
behavioural equivalence, and C's laziness makes it unobservable.

The fix is to stop materialising the prefix and address it in place. A prefix is
contiguous with `dst` **by definition** — it is the mode C selects exactly when
`dictStart + dictSize == dst` — so one slice spans `[dst - prefix_size,
dst + output_size)` with the block at offset `prefix_size`, and `low_prefix`
arithmetic in `decompress_dict` already handles the rest unchanged. No scratch
buffer, no copy back, and one fewer allocation per call on the hot path.

The overlap case still needs care: `src` may lie *inside* the output allocation
(in-place decompression, `LZ4_DECOMPRESS_INPLACE_BUFFER_SIZE`), and holding
`&[u8]` and `&mut [u8]` over the same bytes is UB. That branch spans both
regions into one slice and indexes in, the same trick `with_buffers` uses.

**How it was checked.** `fuzzer -i3000`, `frametest -i50`, and all 13 `fullbench`
decompressors (which CRC-check their output) are green, and `make test` passes
end to end. For the paths this touches, a differential harness compiled twice —
against `upstream/lib/liblz4.a` and against `target/release/liblz4_rs.a` —
compares prefix mode, `withPrefix64k`, partial-in-prefix mode, external dict,
in-place-with-prefix, and four malformed inputs, over six PRNG seeds and four
sizes: **270 transcript lines, byte-identical, including the negative return
values.** Round-trip alone would not have caught the original defect (it
crashed rather than mis-decoded), but rejection parity is the half that silently
drifts, so the return codes are in the transcript too.

---

## 8.4 The block codec copies in bulk, and one of the two stated departures was wrong

`src/block.rs` opens by recording two deliberate departures from the C. The
first — **no wildcopy** — was justified on the grounds that `LZ4_wildCopy8`/`32`
(lz4.c:466, :531) overwrite past the logical end of the data, and "in Rust that
is a panic." **That premise is false**, and it cost real throughput.

Every wildcopy call site in `safe_decode` is guarded so the overshoot lands
*inside* the buffer: at lz4.c:2350 the guard is `cpy <= oend-MFLIMIT`, and
`MFLIMIT` is 12 against a 7-byte overshoot; at lz4.c:2444 the copy stops at
`oCopyLimit = oend-7`. C reserves the slack deliberately, and reading the guards
rather than the comment above the function shows it. Overshooting was legal all
along.

What we ship now, and it is **still safe Rust** — `block.rs` keeps
`#![forbid(unsafe_code)]` and `make unsafe-count` still reports every occurrence
confined to `src/ffi.rs`:

* `copy_match` — `copy_within` for disjoint regions; for an overlapping match,
  one period materialised and then doubled. Each step reads only bytes an
  earlier step finalised, so the pattern-repeat semantics that forbid a plain
  `memmove` survive.
* `copy_match_wild` / `Input::wild_copy_to` — fixed 8-byte steps that may write
  up to 7 bytes past the run, used **only** where C's own guard proves the room.
  `short_offset_prologue` ports the `inc32table`/`dec64table` trick
  (lz4.c:2425), which repositions the source 8 bytes back so a sub-8 offset can
  then be copied a word at a time. Its first four bytes stay sequential, because
  below offset 4 each byte read there was written by the previous one — that
  self-reference *is* the repeat the format encodes.
* `common_bytes` — `LZ4_count`'s word compare plus a bit scan, with the scan
  direction chosen by host endianness as `LZ4_NbCommonBytes` does.
* `Input::as_slice` — the enum is matched once per scan, not once per byte.

**`WILD_COPY_CUTOFF` is ours, not C's.** Below 32 bytes the fixed-width stores
win, because calling `memcpy` for a handful of bytes costs more than the copy.
Above it `memcpy` wins by a wide margin. C needs no such split: its wildcopy has
no per-step bounds check to amortise. Removing the cutoff cost about a third of
the decode throughput on literal-heavy input. The value is not sensitive —
measured at 16/32/64/128/512, the spread sits inside the host's ~13%
run-to-run noise.

### What this measured, and what it ruled out

`upstream/tests/fullbench`, built twice from the same `tests/Makefile` and
confirmed by `make provenance-check` to link no `lib/*.c` object. 8 MB inputs,
best-of-N, because single runs cannot resolve anything under ~13% here.
`bench/bench.sh` and `bench/verify.sh` reproduce all of it.

| vs C | `-P20` | `-P50` | `-P90` | 8 MB zeroes |
|---|---|---|---|---|
| `LZ4_decompress_safe`, before | 0.91x | 0.46x | 0.21x | — |
| `LZ4_decompress_safe`, now | **0.97x** | **0.73x** | **0.48x** | **2.50x** |
| `LZ4_decompress_safe_usingDict`, now | 0.97x | 0.66x | 0.56x | 2.55x |
| `LZ4_compress_default`, now | 0.37x | 0.36x | 0.54x | 0.56x |

The discriminating variable is **sequence density, not compressibility**.
`datagen -P<n>` sets the probability of emitting a match rather than a literal
run (`datagen.c:131`), so `-P90` means *many short* matches, not few long ones.
Reading the P50→P90 drop as "long matches" is the natural mistake and it is
wrong — C slows down on `-P90` too (7729 → 5664 MB/s), which it would not if the
matches were getting longer. The cost was per-sequence: one call into `memcpy`
per sequence. On 8 MB of zeroes, which really is one enormous match, we run
**2.5x faster than C**, because C is still stepping 8 bytes at a time where the
doubling loop hands whole megabytes to `memmove`.

The second stated departure — **one decode loop**, i.e. no `LZ4_FAST_DEC_LOOP` —
is **untested**. It is the one aimed squarely at `-P90`-shaped data, and it is
the largest remaining decode lever. Treat it as an open question, not a settled
decision, and note that the shortcut inside `safe_decode` is already documented
as *not* behaviour-preserving, so assume the fast loop changes what is accepted
until a rejection-parity run proves otherwise.

Compression sits at 0.36x and the copies were never its bottleneck: the
word-at-a-time `common_bytes` bought about 18% and that was most of what was
available there. What remains is per-scanned-position cost in the match search —
`hash_position` reads through `Input::window`, and `Table::get`/`put` dispatch on
the `U16`/`U32` enum, on every position examined. C pays none of it, because
`LZ4_FORCE_INLINE` instantiates the compressor per `dict_directive` and table
type. Monomorphising ours the same way is the next lever, and it is a refactor
rather than an algorithm change.

### The margin comparisons must be additive, not `saturating_sub`

Reproducing the overcopy made a latent translation error load-bearing, and it is
worth recording because the shape recurs everywhere in this decoder.

C computes `oend - MFLIMIT`, `oend - 32` and `iend - 16` as **pointers**. On a
block shorter than the margin those land before the buffer, so `op <= oend-32`
is false and `cpy > oend-MFLIMIT` is true — either way the guarded fast path is
unavailable and the careful path runs. `saturating_sub` clamps to `0` instead,
and for the first sequence of a block written at offset 0 both comparisons flip
the wrong way. Two bugs followed, both reachable only through a tiny output
buffer, i.e. `LZ4_decompress_safe_partial` with a small `targetOutputSize`:

* The two-stage shortcut ran on buffers smaller than its 32-byte margin, `op`
  walked past `oend`, and `length = oend - op` underflowed. `fuzzer -i2000
  -s7354` died on a 7-byte buffer. `tests/Makefile:327` seeds the fuzzer
  randomly, so `test-fuzzer` was failing about **1 run in 12** — a suite that
  is green on a lucky seed is not green.
* The literal parsing restriction let a zero-length run take the unrestricted
  branch. Harmless while that branch held an exact `memcpy`; once it became a
  wildcopy that always moves 8 bytes, it wrote past a 1-byte buffer.

Both are now written additively (`op + 32 <= oend`, `cpy + MFLIMIT > oend`),
which cannot underflow and is exactly C's comparison. The lesson is in
[PORTING.md §3.2](PORTING.md).

### Rejection parity is what found it

None of this is visible to a round trip: the inputs are corrupt or the capacity
is far below the output size, so there is nothing to compare against. What
discriminates is **the return code** — LZ4 position-encodes decode errors
(`-(ip-src)-1`, lz4.c:2462), so comparing the integer compares *where* each
implementation decided the block was malformed, not merely that it did.

`bench/verify.sh` sweeps `LZ4_decompress_safe` and
`LZ4_decompress_safe_partial` at capacities 1..4096 across the 32-byte margin,
over clean, header-corrupted, mid-corrupted, tail-corrupted and truncated
input, comparing that return code and a hash of the bytes actually produced:
**1261/1261 identical.**

It was 1234/1261 before this section's work — and **27 of those divergences
predate the copy work entirely**. They were a pre-existing rejection-parity gap
in `LZ4_decompress_safe` on corrupt input at small capacities, invisible to
every test the project had, and the same additive guards fix them.

### One regression in fail-loudness, recorded deliberately

`common_bytes` clamps its length against both slices, so a caller that violates
C's precondition gets a short count instead of an out-of-bounds index. The
byte-at-a-time loop it replaced *panicked* in that situation. This is a real
loss: the stale-`active_hist` bug fixed in §8.2's work drove the match index to
~4.29e9, which the old loop reported as "index out of bounds" and this one turns
into wrong output — `fuzzer -i60 -s9` still catches it at cycle 54, but by
comparing output rather than by crashing.

A `debug_assert!` restores the loud failure in debug builds (and debug catches
this particular case earlier still, via the `u32` overflow check in
`Hist::dict_at`). **In release the clamp remains, and that class of bug remains
quiet.** Dropping the clamp would restore the panic at the cost of crashing
where C merely reads a word past the match end, which is why it is still there.

---

## 9. Open items

- [ ] **Build the Dockerfile once.** It was written on a host without Docker,
      so it is unverified — an untested one-step build is worse than none.
- [ ] Push to a public GitHub repo; correct the URL in `.port-mortem.toml`.
- [ ] Fill in `unimplemented!()` stubs (see §4); order of work in `PLAN.md` §6
- [ ] **Frame contexts: move the working buffers into the caller's allocator**
      (§8.1). One `frametest` unit assertion depends on it.
- [x] **Segfault in the prefix decode path — fixed (2026-08-02), see §8.3.**
      Killed decompressors #5/#6/#8 in `fullbench` and stopped `make test` at
      `test-fullbench`. `make test` now exits 0 end to end.
- [ ] Differential fuzz harness (C reference vs Rust, valid **and** malformed input)
      — `fuzz/hc_difftest.c` covers the HC surface (§8.2); `bench/verify.sh`
      covers the block and frame codecs on valid input and the block decoder on
      malformed input, **including rejection parity** (§8.4, 1261 comparisons).
      Missing: the frame decoder on malformed input, and a generative loop
      rather than a fixed sweep
- [ ] Benchmark report: p99, RSS, startup, with methodology — throughput vs C
      is done and reproducible (§8.4, `bench/`); p99, RSS and startup are not
- [ ] **Port `LZ4_FAST_DEC_LOOP`** (§8.4). The second of `block.rs`'s two stated
      departures, still untested, and the largest remaining decode lever on
      many-short-sequence data. Needs rejection parity, not just round-trips.
- [ ] **Monomorphise the hot loops over `Input` / `DictDirective` / table type**
      (§8.4). C gets this from `LZ4_FORCE_INLINE`; we dispatch at run time on
      every scanned position, which is most of what keeps compression at 0.36x.
- [ ] Paste organisers' eligibility ruling (§2)
- [x] Confirm `LZ4F_compressOptions_t` / `LZ4F_decompressOptions_t` layouts
      against the probe — done, both asserted in `src/types.rs` (§5)
