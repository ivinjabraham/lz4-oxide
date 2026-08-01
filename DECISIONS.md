# DECISIONS.md — LZ4 (C) → Rust

Port Mortem Hackathon 2026, Track A (C → Rust).

Everything below records *why* the port is shaped the way it is. Where a claim
is checkable, the command that checks it is included.

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

The third row is the one that matters. Linking proves only that symbol names
resolved; it does not prove the C harness reaches Rust code. Running the
resulting binary does — the original `fuzzer` aborts inside our
`unimplemented!()` stub:

```
thread '<unnamed>' panicked at src/ffi.rs:858:5:
not implemented: LZ4_versionString
```

That is lz4's own, unedited test program calling into this port.

**Not yet true:** no function is implemented. Test *pass* rate is currently
zero by construction. The skeleton is proven; the port is not written.

---

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

**Why not the CLI.** `programs/` is another 6,982 lines, which would push the
project past the ceiling. Leaving `lz4cli.c` / `lz4io.c` in C is also
*positively* useful: the C CLI can be linked against our Rust library, so the
`tests/test-lz4-*.sh` shell tests exercise the port end to end.

That coverage is **not automatic** — it has to be wired deliberately, and
getting it wrong yields shell tests that pass while testing the C library. See
§4.1.

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

## 3. Architecture: a Rust staticlib wearing liblz4's ABI

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
`extern "C"` skeleton, and cross-checks it against that list — it fails loudly
if any exported symbol has no stub. Regenerate with `make gen-ffi`.

---

## 4. Running the original test suite with zero edits to `tests/`

This needed care, because the obvious approach silently does nothing.

**The trap.** lz4's tests do *not* link `liblz4.a`. `tests/Makefile:122` builds
`fuzzer` from `lz4.o lz4hc.o xxhash.o fuzzer.o`, and
`build/make/multiconf.make:148` resolves those through
`vpath %.c $(C_SRCDIRS)`, where `C_SRCDIRS = ../lib ../programs .`. A Rust
`liblz4.a` dropped into `lib/` would be **bypassed entirely** — the C sources
would still be compiled, the tests would pass, and they would be testing
nothing. Verify with `make -C tests --dry-run fuzzer`.

**The hook.** Two ordinary make variables, overridden on the command line:

```sh
make -C tests test \
  C_SRCDIRS="$LZ4_OXIDE/cstub $LZ4_SRC/programs ." \
  LDLIBS="$LZ4_OXIDE/target/release/liblz4_rs.a $(rustc --print native-static-libs ...)"
```

* `C_SRCDIRS` drops `../lib` and substitutes `cstub/`, which holds five
  intentionally-empty translation units (`lz4.c`, `lz4hc.c`, `lz4frame.c`,
  `lz4file.c`, `xxhash.c`). The object names the makefile expects still get
  produced; the C implementation is never compiled.
* `LDLIBS` is appended to every link line by `multiconf.make:222`, so our Rust
  archive resolves the symbols.

**No file under `tests/` is modified.** Not the C sources, not the shell
scripts, not `tests/Makefile`. This is wrapped up as `make test`.

### 4.1 The same trap, one level up: the CLI

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

### On "no source-language runtime linking"

The rule forbids leaning on the *source language's runtime* (the cited example
is Python→Rust calling the Python interpreter). C has no such runtime, and we
link none. What remains in C is the **test harness itself**, which is the thing
we are required to keep unmodified — plus the CLI, by choice (§1).

Auditable claim: every `LZ4_*` / `LZ4F_*` symbol in the test binaries resolves
into Rust, and no `lib/*.c` object participates in the link. `make abi-check` diffs the Rust archive's exports against the original's.

---

## 5. Caller-allocated state, and why sizes are probed not hardcoded

lz4's API is caller-allocated. The C tests declare state **on their own stack**
and hand us a pointer — e.g. `LZ4_stream_t stream;` in `fuzzer.c`, and
`XXH64_state_t xxh64;` at `frametest.c:1202`. Consequences:

1. Our Rust types must be `repr(C)` and match the C size and alignment exactly.
   No `Box`, no `Vec`, no `String` in these structs — they must be
   initialisable in place through a raw pointer.
2. The sizes **cannot be hardcoded**. `LZ4_STREAM_MINSIZE` is
   `(1UL << LZ4_MEMORY_USAGE) + 32`, and `tests/Makefile:214-215` deliberately
   rebuilds the suite with `LZ4_MEMORY_USAGE` at both its minimum and maximum.

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

**Decision: port it too, backed by the `xxhash-rust` crate.**

We could have let the genuine C `xxhash.c` keep compiling (simply by omitting it
from our stub directory). We chose not to: it would leave C objects in the
shipped binaries and muddy the "no C implementation remains" claim.

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

This keeps the unsafe surface small and, more importantly, *countable* for the
benchmark/write-up.

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

## 9. Open items

- [ ] Fill in `unimplemented!()` stubs (see §3); order of work in `README.md`
- [ ] Differential fuzz harness (C reference vs Rust, valid **and** malformed input)
- [ ] Benchmark report: p99, RSS, startup, with methodology
- [ ] Paste organisers' eligibility ruling (§2)
- [x] Confirm `LZ4F_compressOptions_t` / `LZ4F_decompressOptions_t` layouts
      against the probe — done, both asserted in `src/types.rs` (§5)
