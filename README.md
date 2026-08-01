# lz4-oxide

A Rust port of [lz4](https://github.com/lz4/lz4)'s compression library, built
for the Port Mortem hackathon 2026 (Track A, C → Rust).

The port is **ABI-compatible with liblz4**. It builds to a static library that
exports the same 141 symbols as the original, which means lz4's own C test
suite runs against it **unmodified** — no test file is edited, patched, or
skipped. That is the whole proof strategy. See [DECISIONS.md](DECISIONS.md).

Upstream pinned at [`0774d055`](https://github.com/lz4/lz4/commit/0774d05537f9762f838f7ab541b7765f1a729cb5)
(`v1.9.2-1552-g0774d055`).

> **Working on this?** Start with [PLAN.md](PLAN.md) — current status, what's
> left, who owns what, and the traps to avoid. The port is an early scaffold:
> the test machinery is proven, but no library function is implemented yet.

---

## Build and test

You need a Rust toolchain (`curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh`)
and a C compiler.

```sh
git clone --recursive <this repo>
cd lz4-oxide
make test
```

If you already have an lz4 checkout and don't want the submodule, point at it:

```sh
make LZ4_SRC=/path/to/lz4 test
```

`LZ4_SRC` resolves in this order: the `LZ4_SRC` variable, then `./upstream`
(the submodule), then `../lz4` (a sibling checkout).

### Targets

| Command | What it does |
|---|---|
| `make` | Build `liblz4_rs.a` |
| `make link-check` | Prove the original C tests *link* against the port |
| `make test` | Run lz4's original test suite against the port |
| `make test-reference` | Run the same suite against the untouched C library |
| `make abi-check` | Diff our exported symbols against the original's |
| `make gen-ffi` | Regenerate the FFI skeleton from the C headers |

`make link-check` is the gate to clear first — see below.

---

## How it fits together

lz4's tests are C programs. For them to test Rust, the Rust has to be callable
as C:

```
   tests/fuzzer.c  ──calls──>  LZ4_compress_default(...)
                                       │
                                       │ resolved at link time by
                                       ▼
                            target/release/liblz4_rs.a
                                (our #[no_mangle] extern "C" fns)
```

The one subtlety: lz4's tests don't link `liblz4.a` at all — they compile
`lib/*.c` directly. We redirect that with two make variables (`C_SRCDIRS` and
`LDLIBS`) rather than by editing anything. DECISIONS.md §4 explains it, and §4.1
covers the same trap recurring in the CLI build.

### Layout

```
src/
  ffi.rs      141 extern "C" entry points. Generated; the ONLY unsafe module.
  types.rs    C-compatible types, sizes/alignments asserted against the headers
  block.rs    lz4.c       — core block codec
  hc.rs       lz4hc.c     — high compression
  frame.rs    lz4frame.c  — frame format
  file.rs     lz4file.c   — file API
  xxh.rs      xxhash      — checksums
cstub/        empty .c files that displace lib/*.c in the test build
tools/        gen_ffi.py, the FFI skeleton generator
build.rs      probes the C headers for struct sizes and alignments
```

Every implementation module carries `#![forbid(unsafe_code)]`. All pointer
handling lives in `ffi.rs`, so the port's unsafe surface stays small and
countable.

---

## Order of work

Sequenced so each step unlocks the most tests. Each unimplemented function is
`unimplemented!("LZ4_...")`, so anything not yet written fails loudly with its
own name rather than silently returning garbage.

| # | Step | Unlocks |
|---|---|---|
| 1 | **Skeleton links** (`make link-check`) | the gate — nothing else counts until this passes |
| 2 | Basic compress / decompress | most of `fuzzer` |
| 3 | Frame format | `frametest` + all `test-lz4-*.sh` shell tests |
| 4 | Streaming + dictionary | the rest of `fuzzer` |
| 5 | High compression, levels 1–9 | `test-lz4hc` |
| 6 | Optimal parser, levels 10–12 | hardest; cut this first if time runs short |

Steps 1–3 give a genuinely working lz4. Steps 4–6 buy score.

### Who owns what

| | Area | Files |
|---|---|---|
| **A** | Block codec — steps 2 and 4 | `src/block.rs` |
| **B** | Frame format + checksums — step 3 | `src/frame.rs`, `src/xxh.rs` |
| **C** | High compression — steps 5/6 — **plus** the fuzz harness, benchmarks and DECISIONS.md | `src/hc.rs` |

Role C is not a consolation prize. The differential fuzzer, the benchmark
report and DECISIONS.md are worth roughly a third of the total score, and they
are the classic thing teams leave until the last night.

---

## A note on behavioural equivalence

Compressed output must match the C implementation **byte for byte** where the
original is deterministic. So port the search loops faithfully — do not
"improve" hash functions, tie-breaking, or table sizing, however tempting.
Divergence there is invisible in round-trip tests and fatal in differential
fuzzing.

For the decoder, the interesting property is not just that valid input
round-trips, but that **malformed input is rejected identically**. Upstream's
four most recent commits are all decode-bounds fixes, so that is where bugs
live.

## License

BSD 2-Clause, matching upstream lz4.
