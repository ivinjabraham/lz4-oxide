# lz4-oxide

A Rust port of [lz4](https://github.com/lz4/lz4)'s compression library, built
for the Port Mortem hackathon 2026 (Track A, C → Rust).

The port is **ABI-compatible with liblz4**. It builds to a static library that
exports the same 141 symbols as the original, which means lz4's own C test
suite runs against it **unmodified** — no test file is edited, patched, or
skipped. That is the whole proof strategy. See [DECISIONS.md](DECISIONS.md).

Upstream pinned at [`0774d055`](https://github.com/lz4/lz4/commit/0774d05537f9762f838f7ab541b7765f1a729cb5)
(`v1.9.2-1552-g0774d055`).

> **State:** a work in progress. The test machinery is proven — the original C
> suite builds, links and runs against the Rust archive — and the library
> functions are being filled in behind it, so expect failures until that is
> done. `make test` is the honest answer at any moment.
> [PLAN.md](PLAN.md) tracks what is left; [PORTING.md](PORTING.md) records what
> breaks when translating *this* C into Rust.

---

## Build and test

You need a Rust toolchain (`curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh`)
and a C compiler.

```sh
git clone --recursive <this repo>
cd lz4-oxide
make test
```

The C sources come from `upstream/`, a submodule pinned at the commit above —
hence `--recursive`. If you cloned without it, `git submodule update --init
--recursive`.

To build against an lz4 checkout you already have:

```sh
make LZ4_SRC=/path/to/lz4 test
```

`LZ4_SRC` overrides `upstream/` for both `make` and `build.rs`. Pointing it at a
different tree invalidates `make kickoff-verify` and `make abi-check`, which are
claims about the pinned commit specifically.

### Targets

| Command | What it does |
|---|---|
| `make` | Build `liblz4_rs.a` |
| `make link-check` | Prove the original C tests *link* against the port |
| `make test` | Run lz4's original test suite against the port |
| `make test-reference` | Run the same suite against the untouched C library |
| `make test-quick` | `fuzzer` + `frametest` only — the edit/run loop |
| `make abi-check` | Diff our exported symbols against the recorded original ABI |
| `make provenance-check` | Prove each built test binary came from `cstub/`, not `lib/` |
| `make kickoff-verify` | Prove the original tests are byte-identical to kickoff |
| `make unsafe-count` | `unsafe` count and ratio; fails if any escapes `ffi.rs` |
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
`LDLIBS`) rather than by editing anything. DECISIONS.md §3 explains it, and §3.1
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

Every unimplemented function is `unimplemented!("LZ4_...")`, so anything not yet
written fails loudly with its own name rather than silently returning garbage.
That makes the panic message the work queue.

The sequenced breakdown — which step unlocks which tests, and why the optimal
parser is not the cheap cut it looks like — is [PLAN.md §6](PLAN.md).

---

## Behavioural equivalence

Compressed output matches the C implementation **byte for byte** wherever the
original is deterministic: same hash functions, same tie-breaking, same table
sizing. Divergence there is invisible to round-trip tests and shows up only
under differential fuzzing, which is why the port follows the original's search
loops rather than improving on them.

For the decoder the property is stronger than "valid input round-trips":
**malformed input must be rejected identically**. Upstream's four most recent
commits are all decode-bounds fixes, so that is where bugs live.

## License

[BSD 2-Clause](LICENSE), matching upstream lz4's `lib/`. The port is a
derivative work, so the original copyright notices are retained.
