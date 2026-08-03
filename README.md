# lz4-oxide

A Rust port of [lz4](https://github.com/lz4/lz4)'s compression library, built
for the Port Mortem hackathon 2026 (Track A, C → Rust).

The port is **ABI-compatible with liblz4**. It builds to a static library that
exports the same 141 symbols as the original, which means lz4's own C test
suite runs against it **unmodified**, no test file is edited, patched, or
skipped. That is the whole proof strategy. See [DECISIONS.md](DECISIONS.md).

Upstream pinned at [`0774d055`](https://github.com/lz4/lz4/commit/0774d05537f9762f838f7ab541b7765f1a729cb5)
(`v1.9.2-1552-g0774d055`). The port covers upstream's `lib/` (6,284 C SLOC),
not `programs/`, [DECISIONS.md §1](DECISIONS.md) explains the scope.

## Results at a glance

Last verified 2026-08-03 on x86_64 Linux with rustc 1.97.1 and gcc 14.3.0.

| Claim | Result | Reproduce |
|---|---:|---|
| Original lz4 suite against Rust | exit 0, end to end | `make test` |
| Original test files matching kickoff hashes | 42/42 | `make kickoff-verify` |
| `unsafe` occurrences | 312, all in `ffi.rs` | `make unsafe-count` |
| Exported C ABI | 141/141 symbols | `make abi-check` |
| Differential identity and rejection parity (block, stream, frame, HC) | 1369/1369 | `make difftest` |

The full evidence, methodology, and remaining limitations are recorded in
[DECISIONS.md §0](DECISIONS.md).

## Build and verify

You need a Rust toolchain and a C compiler.

```sh
git clone --recursive https://github.com/ivinjabraham/lz4-oxide.git
cd lz4-oxide
make difftest
make test
```
 The C sources come from `upstream/`, a submodule pinned at the commit
above, hence `--recursive`.

`make difftest` checks byte identity and malformed-input rejection of the port against the
pinned C implementation while `make test` runs the complete, unmodified upstream suite on it.

### Docker

The Dockerfile can produce a verified library bundle or a runnable lz4 CLI
linked against the Rust port:

```sh
docker build --target verify .
docker build --target artifacts -o out .
docker build -t lz4-oxide .
docker run --rm lz4-oxide --version
```

### Targets

| Command | What it does |
|---|---|
| `make` | Build `liblz4_rs.a` |
| `make link-check` | Prove the original C tests *link* against the port |
| `make test` | Run lz4's original test suite against the port |
| `make difftest` | Check byte identity and rejection parity against pinned C |
| `make test-reference` | Run the same suite against the untouched C library |
| `make test-quick` | `fuzzer` + `frametest` only — the edit/run loop |
| `make abi-check` | Diff our exported symbols against the recorded original ABI |
| `make provenance-check` | Prove each built test binary came from `cstub/`, not `lib/` |
| `make kickoff-verify` | Prove the original tests are byte-identical to kickoff |
| `make unsafe-count` | `unsafe` count and ratio; fails if any escapes `ffi.rs` |

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

The one subtlety: lz4's tests don't link `liblz4.a` at all, they compile
`lib/*.c` directly. We redirect that with two make variables (`C_SRCDIRS` and
`LDLIBS`) rather than by editing anything. 

The upstream CLI itself remains C, but its library calls resolve to Rust, so the original shell tests exercise the
port end to end. DECISIONS.md §3 explains the test redirect, and §3.1 covers the
same trap in the CLI build.

### Layout

```
src/
  ffi.rs      141 implemented extern "C" entry points; the ONLY unsafe module
  types.rs    C-compatible types, sizes/alignments asserted against the headers
  block.rs    lz4.c       — core block codec
  hc.rs       lz4hc.c     — high compression
  frame.rs    lz4frame.c  — frame format
  file.rs     lz4file.c   — file API
  xxh.rs      xxhash      — checksums
fuzz/         differential drivers for block, stream, frame and HC APIs
bench/        parity checks and reproducible C-vs-Rust throughput scripts
cstub/        empty .c files that displace lib/*.c in the test build
tools/        gen_ffi.py, used to bootstrap the original FFI skeleton
build.rs      probes the C headers for struct sizes and alignments
```

Every implementation module carries `#![forbid(unsafe_code)]`. All pointer
handling lives in `ffi.rs`, so the port's unsafe surface stays confined to the FFI shim.

## Behavioural equivalence

Compressed output matches the C implementation **byte for byte** wherever the
original is deterministic: same hash functions, same tie-breaking, same table
sizing. Divergence there is invisible to round-trip tests and shows up only
under differential fuzzing, which is why the port follows the original's search
loops rather than improving on them.

For the decoder the property is stronger than "valid input round-trips":
**malformed input must be rejected identically**. Upstream's four most recent
commits are all decode-bounds fixes, so that is where bugs live.

Run both differential harnesses with:

```sh
make difftest
```

## Performance

Best-of-three throughput on an
8 MB `datagen -P50` input is 0.61x C for default compression, 0.47x for
streaming compression, 1.01x for fast decompression, and 0.76x for safe
decompression. Across the P10/P50/P90/zero corpus, default compression ranges
from 0.61x–0.73x C. The commands and methodology are in
[`bench/methodology.md`](bench/methodology.md), structured measurements are in
[`bench/results.json`](bench/results.json), and analysis is in
[DECISIONS.md §8.4](DECISIONS.md).

## License

[BSD 2-Clause](LICENSE), matching upstream lz4's `lib/`. The port is a
derivative work, so the original copyright notices are retained.
