# lz4-oxide

A Rust port of [lz4](https://github.com/lz4/lz4)'s compression library, built
for the Port Mortem hackathon 2026 (Track A, C → Rust).

The port is **ABI-compatible with liblz4**. It builds to a static library that
exports the same 141 symbols as the original, which means lz4's own C test
suite runs against it **unmodified**, no test file is edited, patched, or
skipped. That is the whole proof strategy. See [DECISIONS.md](DECISIONS.md).

Upstream pinned at [`0774d055`](https://github.com/lz4/lz4/commit/0774d05537f9762f838f7ab541b7765f1a729cb5)
(`v1.9.2-1552-g0774d055`). The port covers upstream's `lib/` (6,284 C SLOC),
not `programs/`; [DECISIONS.md section 1](DECISIONS.md) explains the scope.

## Results at a glance

The latest benchmark environment and artifact hash are recorded in
[`bench/results.json`](bench/results.json).

| Claim | Result | Reproduce |
|---|---:|---|
| Original lz4 suite against Rust | exit 0, end to end | `make test && make provenance-check` |
| Original test files matching kickoff hashes | 42/42 | `make kickoff-verify` |
| `unsafe` boundary | all occurrences confined to `ffi.rs` | `make unsafe-count` |
| Exported C ABI | 141/141 symbols | `make abi-check` |
| Covered differential matrix | 0 divergences | `make difftest` |

The verification scope and remaining limitations are recorded in
[DECISIONS.md sections 6 and 8](DECISIONS.md).

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

`make difftest` checks covered byte identity and block-decoder rejection parity
against the pinned C implementation. `make test` runs the complete, unmodified
upstream suite against the port.

### Docker

The Dockerfile is intended to produce a verified library bundle or a runnable
lz4 CLI linked against the Rust port. Its targets have not yet been exercised on
a host with Docker:

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
| `make provenance-check` | Check that each binary's primary `lz4.o` came from `cstub/` |
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

The upstream CLI itself remains C, but its library calls resolve to Rust, so the
original shell tests exercise the port end to end. [DECISIONS.md section
2](DECISIONS.md) explains the test and CLI build redirects.

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
bench/        one-command C-vs-Rust benchmark runner and structured results
cstub/        empty .c files that displace lib/*.c in the test build
tools/        gen_ffi.py, used to bootstrap the original FFI skeleton
build.rs      probes the C headers for struct sizes and alignments
```

Every implementation module carries `#![forbid(unsafe_code)]`. All pointer
handling lives in `ffi.rs`, so the port's unsafe surface stays confined to the FFI shim.

## Behavioural equivalence

The covered block, streaming, HC, and frame preference paths match the C
implementation **byte for byte**. Divergence is invisible to round-trip tests
and shows up only under differential testing, which is why the port follows the
original's search loops rather than improving on them. HC levels 3 and above
through the frame API remain a documented exception.

For the block decoder the property is stronger than "valid input round-trips":
covered malformed inputs must be rejected at the same offset. Malformed frame
rejection parity is not yet covered.

Run the differential matrix with:

```sh
make difftest
```

## Performance

Run `bench/bench.py` to rebuild independently linked C and Rust binaries,
execute the complete benchmark matrix, and atomically replace the structured
results.
The latest measurements are in [`bench/results.json`](bench/results.json), the
methodology is in [`bench/methodology.md`](bench/methodology.md), and known
limitations are in [DECISIONS.md sections 6 and 8](DECISIONS.md).
Keeping numeric results in the generated JSON avoids a stale second copy here.

## License

[BSD 2-Clause](LICENSE), matching upstream lz4's `lib/`. The port is a
derivative work, so the original copyright notices are retained.
