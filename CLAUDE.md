# lz4-oxide

Rust port of lz4's C library, ABI-compatible with liblz4, for a hackathon
(deadline 2026-08-03 18:00 UTC). **Read [PLAN.md](PLAN.md) first** — status,
roadmap and task ownership. Rationale lives in [DECISIONS.md](DECISIONS.md);
the porting traps and the byte-identity check are in [PORTING.md](PORTING.md).

## The loop

**The porting loop is finished.** Every exported symbol has a body, no
`unimplemented!()` remains, and `make test` passes end to end. `fuzzer -i1` no
longer names anything to write.

What replaces it, for any change from here:

```sh
make difftest     # byte-identity AND rejection parity vs the pinned C library
make test         # the score — the full unmodified upstream suite
```

`make difftest` is the one that matters while editing. The upstream suite is
round-trips and CRCs, so it cannot see compressed output that is wrong but
*valid*, nor an error returned at the wrong offset — and both have happened
here (DECISIONS.md sections 6 and 7). Run it before you trust a green suite.

**Do not record which functions are done** — not here, not in PLAN.md. That is
a doc edit per commit and it goes stale between them. The commands above are
the status.

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
  **C, not Rust** (see DECISIONS.md section 2). The `C_SRCDIRS` / `LDLIBS` /
  `-o lz4` machinery in `Makefile` handles both; do not simplify it. Use
  `make provenance-check` to confirm each binary's `lz4.o` uses `cstub/`.

## Commands

`make link-check` (build + prove linkage) · `make test` (full suite) ·
`make test-reference` (C baseline) · `make abi-check` (141-symbol diff)
