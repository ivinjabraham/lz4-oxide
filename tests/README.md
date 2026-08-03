# tests/

The submission template suggests `tests/original/` (the source repo's suite,
hashed at kickoff) and `tests/port/` (tests we add).

**We do not vendor a copy of the original suite here.** It is consumed as a git
submodule, `upstream/`, pinned at lz4 `0774d055`.

The integrity of the upstream module can be checked with:

```sh
make kickoff-verify
```

which asserts

1. `git -C upstream status --short` is empty — no working-tree edit, and
2. all 42 git-tracked files under `upstream/tests/` still hash to the values
   recorded in [`KICKOFF.sha256`](KICKOFF.sha256).

| Path | What it is |
|---|---|
| `KICKOFF.sha256` | SHA-256 of every tracked file in `upstream/tests/`, plus the pinned commit and its date |
| `upstream/tests/` | The original suite itself (submodule, read-only by policy) |
| `port/` | Safe Rust integration tests for port helpers |

## Running them

| Command | Scope |
|---|---|
| `make test` | full original suite against the Rust port — **this is the score** |
| `make test-quick` | `fuzzer` + `frametest` only; the edit/run loop |
| `make test-reference` | same full suite against the untouched C library — the baseline |
| `cargo test --test port` | run Rust integration tests for helper functions |

`make test` and `make test-reference` both include huge-file cases that
pipe `datagen -g6GB` and `-g3G` through the CLI (`test-lz4-fast-hugefile.sh`).
Budget tens of minutes. Use `make test-quick` while implementing.
