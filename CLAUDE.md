# lz4-oxide

Rust port of lz4's C library, ABI-compatible with liblz4, for a hackathon
(deadline 2026-08-03). **Read [PLAN.md](PLAN.md) first** — status, roadmap and
task ownership. Rationale lives in [DECISIONS.md](DECISIONS.md).

## State

The scaffold is built and proven; **no library function is implemented yet**.
Test pass rate is zero by construction. Work proceeds by running a test, reading
the `not implemented: LZ4_xxx` panic, implementing that function, repeating.

## Rules

- **Never edit anything under `upstream/`.** `git -C upstream status --short`
  must stay empty — that our port passes lz4's *unmodified* tests is the entire
  claim of this project.
- **Compressed output must be byte-identical to the C implementation** where the
  original is deterministic. Do not "improve" hash functions, tie-breaking or
  table sizing; divergence is invisible in round-trip tests and fatal in
  differential fuzzing.
- `unsafe` belongs only in `src/ffi.rs`, which converts raw pointers to slices
  and delegates. Implementation modules carry `#![forbid(unsafe_code)]`.
- Caller-allocated structs (`LZ4_stream_t`, `XXH64_state_t`, …) are declared on
  the C caller's stack. Sizes are probed by `build.rs` and asserted — never
  hardcode them, and keep these types free of `Box`/`Vec`/`String`.
- `make gen-ffi` **overwrites `src/ffi.rs`**. It was a bootstrap tool; running it
  now destroys implemented bodies.

## Gotchas

- Rust may be installed but absent from an existing shell's PATH:
  `source "$HOME/.cargo/env"`.
- Two separate build traps can produce a green test suite that is exercising
  **C, not Rust** (see PLAN.md §8). The `C_SRCDIRS` / `LDLIBS` / `-o lz4`
  machinery in `Makefile` handles both — don't simplify it. To confirm the port
  is really under test: `./upstream/tests/fuzzer -i1` must panic in `src/ffi.rs`.

## Commands

`make link-check` (build + prove linkage) · `make test` (full suite) ·
`make test-reference` (C baseline) · `make abi-check` (141-symbol diff)
