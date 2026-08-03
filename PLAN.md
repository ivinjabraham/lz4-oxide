# lz4-oxide — working plan

Team roadmap and handoff. If you are picking this up cold, read this file top
to bottom once; it should take about five minutes.

- **What & how to build** → [README.md](README.md)
- **How to write the port by hand** → [PORTING.md](PORTING.md) ← read before
  writing a match loop; it is the list of things that break when you translate
  *this* C into Rust
- **Why we made each call** (graded) → [DECISIONS.md](DECISIONS.md)
- **Status, what's next, who does what** → this file

---

## 1. Status at a glance

> **The port is written and the suite passes.** Every exported symbol has a
> body, `make test` exits 0 end to end, and all 13 HC levels plus the block and
> frame codecs are byte-identical to the C library.
>
> **Status is still derivable rather than recorded here:**
>
> ```sh
> make test        # the score — the full unmodified upstream suite
> make difftest    # byte-identity and rejection parity vs pinned C
> ```
>
> Deliberately no per-function checklist: it would need an edit per commit and
> would be wrong between them. What is *not* done is in DECISIONS.md §0 under
> "what is still not true", and §9.

| | State |
|---|---|
| Build system, one-command | ✅ done |
| 141-symbol C ABI surface | ✅ generated, exact match |
| Original C tests link against Rust | ✅ proven |
| C tests actually *call* Rust | ✅ proven |
| Upstream tree unmodified | ✅ empty `git status` |
| Struct layouts match C | ✅ probed + asserted at compile time |
| **Library functions** | 🟡 in progress — see the commands above |
| Differential fuzz harness | ❌ not started |
| Benchmark report | ❌ not started |
| Demo video | ❌ not started |
| Kickoff hash manifest, `.port-mortem.toml` | ✅ done |
| Dockerfile | 🟡 written, **never built** |

### The clock

| | UTC |
|---|---|
| Kickoff | 2026-07-31 18:00 |
| **Deadline** | **2026-08-03 18:00** |
| Stop implementing, start packaging | **2026-08-03 08:00** |

That third row is a decision, not a guess. The last ~10 hours go to the fuzz
log, the benchmark run, the demo video and a DECISIONS.md pass. **The video
cannot be made until tests actually pass live**, so it is hostage to everything
else finishing — do not let it slide past the pivot.

---

## 1.1 The brief, condensed

Port Mortem, Track A (C → Rust). The organisers' framing: *generating a port
that compiles is now trivial; producing one that behaves like the original —
same edge cases, same failure modes, original test suite untouched — is the
open problem.* They cite the Bun rewrite editing the original tests as the
anti-pattern.

**Scoring: 40% functionality & reliability · 30% behavioural equivalence ·
20% code quality · 10% innovation.**

Exit criteria they named:

| Criterion | Where we stand |
|---|---|
| Original C suite passes unmodified, ≥99% | ❌ 0% — the job |
| `unsafe` under a *documented threshold* vs source line count | 🟡 policy + `make unsafe-count`; needs a stated budget |
| ≥1 latent bug in the original found by differential fuzzing | ❌ harness not started |
| Error paths idiomatic — `Result`, not translated errno | 🟡 architecture does this; see DECISIONS.md §7.1 |

Automatic disqualifiers, all of which we avoid **by construction** — worth
knowing so nobody "simplifies" us into one:

- shelling out to the original binary → we link a Rust staticlib
- FFI-ing into the source language's runtime → C has none, and no `lib/*.c`
  object is ever linked into a test binary (DECISIONS.md §3)
- silently editing the original tests → `make kickoff-verify`
- cherry-picking happy-path tests → the suite is run whole
- repos over 8,000 source lines → 6,284 SLOC ported (DECISIONS.md §1)

Note the line ceiling is on **SLOC**: `lib/*.c` is 8,662 *raw* lines but 6,284
non-blank non-comment. Both counts are in DECISIONS.md §1 so the arithmetic is
visible rather than flattering.

---

## 2. Evidence

Everything in §1 marked ✅ is checkable. The full table with commands and
outputs is [DECISIONS.md §0](DECISIONS.md). The short version:

```sh
make abi-check                 # 141/141, zero diff
make link-check                # OK: original C tests link against the Rust port
./upstream/tests/fuzzer -i1    # panics: not implemented: <next symbol to write>
git -C upstream status --short # empty
```

The third command is the one that matters. Linking only proves symbol *names*
resolved. Running the binary proves the unmodified C harness reaches our Rust.

---

## 3. Setup from scratch

```sh
# 1. Rust toolchain
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
source "$HOME/.cargo/env"        # existing shells won't have it on PATH

# 2. Clone WITH the submodule (upstream lz4, pinned at 0774d055)
git clone --recursive <repo> lz4-oxide && cd lz4-oxide

# 3. Prove it works
make link-check
```

Already have an lz4 checkout and don't want the submodule?
`make LZ4_SRC=/path/to/lz4 link-check`.

`LZ4_SRC` resolves: the `LZ4_SRC` variable, else `./upstream` (the submodule).
Overriding it invalidates `kickoff-verify` and `abi-check` — both are claims
about the pinned commit.

---

## 4. How the proof strategy works

lz4's tests are **C programs**. For them to test Rust, the Rust must be callable
as C. So we build a `staticlib` that exports the same 141 symbols as `liblz4.a`,
and redirect lz4's build to link ours instead of compiling its own C.

```
   upstream/tests/fuzzer.c  ──calls──>  LZ4_compress_default(...)
                                                │  resolved at link time by
                                                ▼
                                  target/release/liblz4_rs.a
                                      (our extern "C" fns)
```

The redirect is two ordinary make variables — `C_SRCDIRS` and `LDLIBS` — set on
the command line. **No file under `upstream/` is edited.** Full mechanism in
[DECISIONS.md §3](DECISIONS.md).

---

## 5. The working loop

This is why the skeleton was built before any real code: **the stubs are a
self-generating worklist.**

```
  make link-check                    # build
  ./upstream/tests/fuzzer -i1        # run
    → "not implemented: LZ4_compressBound"
  ...implement LZ4_compressBound...
  repeat
```

Every unimplemented function is `unimplemented!("LZ4_xxx")`, so the test tells
you its own name when it reaches it. You never have to guess what to do next.
Work depth-first down whatever the panic says until the test gets further.

Once functions start passing, `make test` gives you the real score.

---

## 6. Work breakdown

Ordered so each step unlocks the most tests per hour.

| # | Step | Owner | Files | Unlocks |
|---|---|---|---|---|
| 1 | Skeleton links | — | — | ✅ **done** |
| 2 | Basic compress / decompress | **A** | `src/block.rs` | ✅ **done** |
| 3 | Frame format + checksums | **B** | `src/frame.rs`, `src/xxh.rs` | ✅ **done** |
| 4 | Streaming + dictionary | **A** | `src/block.rs` | ✅ **done** |
| 5 | HC, levels ≤2 (`lz4mid`) and 3–9 (`lz4hc` hash chain) | **C** | `src/hc.rs` | ✅ **done** |
| 6 | Optimal parser, levels 10–12 (`lz4opt`) | **C** | `src/hc.rs` | ✅ **done** |

All six are done and byte-identical to C. §6.1 below is kept because its
*reasoning* is what made step 6 get finished rather than cut — and because the
fallback it describes was in fact taken for a while, silently, with a green
suite the whole time (DECISIONS.md §8.2).

What remains is not implementation: see DECISIONS.md §9.

### 6.1 The optimal parser is not the cheap cut it looks like

An earlier revision of this file marked step 6 "cut this first if short on
time." **That was wrong**, and acting on it would have failed a lot of tests.

`lz4hc.c:92-106` selects one of three strategies by level: `lz4mid` (≤2),
`lz4hc` (3–9, `LZ4HC_compress_hashChain`), and `lz4opt` (10–12,
`LZ4HC_compress_optimal`). The last is a dynamic-programming parser: instead of
greedily taking the longest match, it prices each candidate in real output bytes
via `LZ4HC_literalsPrice` / `LZ4HC_sequencePrice` (`lz4hc.c:1826-1848`),
including the 255-extension bytes, and finds the cheapest parse over a 4096-byte
window (`LZ4_OPT_NUM`).

The reason it can't simply be dropped: `fuzzer.c:386` draws
`compressionLevel = FUZ_rand(...) % (LZ4HC_CLEVEL_MAX+1)` **once per cycle** and
then uses it across ~15 HC call sites on real data (`fuzzer.c:440-1043`).
Levels 10–12 are 3 of 13 outcomes, so roughly **a quarter of all fuzzer cycles
enter the optimal parser**. It is not a `test-lz4hc` side feature.

**The actual fallback, if time runs out:** route levels 10–12 to the level-9
hash chain rather than omitting them. That emits valid LZ4, so round-trip and
CRC checks still pass and `fuzzer` stays green. What you lose is byte-identity
with C — which costs behavioural equivalence (30% of score) and will show up in
our own differential fuzzer as a divergence. Degrade, don't delete. And if you
do this, say so in DECISIONS.md; an undocumented divergence is the thing the
organisers explicitly penalise.

**Person C also owns the fuzz harness, benchmarks and DECISIONS.md** — see §9.
That is not a consolation prize; it is roughly a third of the total score, and
it is the classic thing teams leave until the last night.

---

## 7. How to implement one function

The pattern, every time: **`ffi.rs` converts raw pointers to slices immediately
and delegates. All real logic lives in a safe module.**

Generated stub:

```rust
#[no_mangle]
pub extern "C" fn LZ4_compress_default(
    src: *const c_char, dst: *mut c_char, srcSize: c_int, dstCapacity: c_int,
) -> c_int {
    unimplemented!("LZ4_compress_default")
}
```

Implemented — `src/ffi.rs`:

```rust
#[no_mangle]
pub unsafe extern "C" fn LZ4_compress_default(
    src: *const c_char, dst: *mut c_char, srcSize: c_int, dstCapacity: c_int,
) -> c_int {
    if src.is_null() || dst.is_null() || srcSize < 0 || dstCapacity < 0 {
        return 0;                       // C returns 0 on failure here
    }
    let input = unsafe { slice::from_raw_parts(src as *const u8, srcSize as usize) };
    let output = unsafe { slice::from_raw_parts_mut(dst as *mut u8, dstCapacity as usize) };
    crate::block::compress_default(input, output).unwrap_or(0) as c_int
}
```

`src/block.rs` — no `unsafe` allowed here, the module has
`#![forbid(unsafe_code)]`:

```rust
/// Returns the compressed length, or None if it doesn't fit in `dst`.
pub fn compress_default(src: &[u8], dst: &mut [u8]) -> Option<usize> { ... }
```

Match the C return conventions exactly — they differ per function. Compression
returns `0` on failure; `LZ4_decompress_safe` returns a **negative** value.
Check the doc comment in `upstream/lib/lz4.h` for each one.

### ⚠️ `make gen-ffi` overwrites `src/ffi.rs`

It was a bootstrap tool. Once you start filling in bodies, **running it will
destroy your work.** Only re-run it if the upstream ABI changes, and diff the
result rather than accepting it wholesale.

---

## 8. Traps

Each of these costs hours if rediscovered the hard way.

**Tests that pass while testing nothing — twice.** lz4's tests don't link
`liblz4.a`; they compile `lib/*.c` directly. And `upstream/tests/Makefile:68-71`
clears `MAKEFLAGS`, so our overrides don't reach the CLI sub-make. Both produce
a green suite that exercises **C, not Rust**. Both are handled in our `Makefile`
— don't "simplify" the `C_SRCDIRS`/`LDLIBS`/`-o lz4` machinery without reading
[DECISIONS.md §3 and §3.1](DECISIONS.md). If you ever doubt it, run
`./upstream/tests/fuzzer -i1` and confirm it still dies in Rust.

**Never edit anything under `upstream/`.** `git -C upstream status --short` must
stay empty. That is the entire claim we make to judges.

**A stale test binary will lie to you.** `upstream/tests/Makefile` lists the
`.o` files as prerequisites of `fuzzer`, *not* our `liblz4_rs.a` — which reaches
the link only through `LDLIBS`. So implementing a function and re-running
`make link-check` can leave the old binary in place, panicking on the symbol you
just wrote. It looks like your code did nothing. Force the relink:

```sh
rm -f "$(readlink -f upstream/tests/fuzzer)" upstream/tests/fuzzer
```

(The binaries are git-ignored, so this does not dirty `upstream/`.) Related:
`panic = "abort"` discards buffered stdout, so the fuzzer's banner vanishes on
panic — use `stdbuf -oL` when you need to *see* what the port returned.

**Compressed output must be byte-identical to C** wherever the original is
deterministic. Port the search loops faithfully — do not "improve" hash
functions, tie-breaking, or table sizing. Divergence is invisible in round-trip
tests and fatal in differential fuzzing (30% of the score).

**Caller-allocated structs.** The C tests declare `LZ4_stream_t`,
`XXH64_state_t` etc. *on their own stack* and hand us pointers. Our types must
match C's size and alignment exactly — no `Box`, no `Vec`, no `String` in them.
Sizes are probed from the real headers by `build.rs` and asserted at compile
time, because `LZ4_STREAM_MINSIZE` varies with `LZ4_MEMORY_USAGE` and the suite
is deliberately built with both extremes. Don't hardcode them.

---

## 9. Deliverables checklist

Scoring is 40% functionality / 30% behavioural equivalence / 20% code quality /
10% innovation.

The organisers ask for seven things. Mapped to this repo:

| # | Deliverable | Where | Owner | State |
|---|---|---|---|---|
| 01 | Public GitHub repo with the port | — | — | 🟡 local only — **push it** |
| 02 | One-step build to a runnable artifact | `make` / `Dockerfile` | — | 🟡 `make` works; Dockerfile never built |
| 03 | Original suite, hashed at kickoff, passing | `tests/KICKOFF.sha256` | A, B, C | 🟡 hashed ✅ · passing ❌ 0% |
| 04 | Differential fuzz harness | `fuzz/` | **C** | ❌ not started |
| 05 | DECISIONS.md | `DECISIONS.md` | **C** | 🟡 written; needs eligibility ruling (§2) |
| 06 | Benchmark report | `bench/` | **C** | ❌ not started |
| 07 | 5-minute demo video | — | **C** | ❌ not started |

Plus `.port-mortem.toml` (track letter, source URL, kickoff hash) — ✅ done,
though the schema is our reading; conform it if a canonical one is published.

**We brought our own repo**, so two things the pooled entrants get, we don't:
a vetted line count (see §1.1) and the fuzz-harness template. The template is
no real loss — a differential harness is ~100 lines. It generates random *and*
malformed inputs, feeds the identical bytes to the C reference and to our port,
and asserts they agree on output **and on rejection**: same error code for the
same bad input, not merely agreement on valid data. Rejection parity is the
half that finds bugs, and it is the half a naive harness omits.

**Differential fuzzing note:** compare against the C reference built by
`make test-reference`. Feed both **valid and malformed/truncated** input, and
check they *reject* the same bytes — not just that they agree on valid data.
Upstream's four most recent commits are all decode-bounds fixes, so that is
where bugs live, and where the Bug Catcher prize is.

---

## 10. Commands

| Command | What it does |
|---|---|
| `make` | Build `liblz4_rs.a` |
| `make link-check` | Prove the original C tests link against the port |
| `make test` | Run lz4's original test suite against the port — **the score** |
| `make test-quick` | `fuzzer` + `frametest` only — the edit/run loop |
| `make test-reference` | Run the same suite against the untouched C library |
| `make abi-check` | Diff our exported symbols against the recorded original ABI |
| `make provenance-check` | Prove each built test binary came from `cstub/`, not `lib/` |
| `make kickoff-verify` | Prove the original tests are byte-identical to kickoff |
| `make unsafe-count` | `unsafe` occurrences, ratio vs C SLOC; fails if any escapes `ffi.rs` |
| `make gen-ffi` | ⚠️ Regenerate the FFI skeleton — **overwrites `src/ffi.rs`** |
| `make clean` | `cargo clean` + drop generated symbol lists |

Add `LZ4_SRC=/path/to/lz4` to any of them to use a checkout other than
`./upstream`.
