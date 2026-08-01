# tests/

The submission template suggests `tests/original/` (the source repo's suite,
hashed at kickoff) and `tests/port/` (tests we add).

**We do not vendor a copy of the original suite here.** It is consumed as a git
submodule, `upstream/`, pinned at lz4 `0774d055`. Copying the files into
`tests/original/` would make "unmodified" a claim you have to take on trust;
a pinned submodule makes it structural — there is no second copy that could
drift, and any edit shows up as a dirty submodule.

Both properties are checkable in one command:

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
| `upstream/tests/` | the original suite itself (submodule, read-only by policy) |
| `port/` | tests we add — **does not exist yet**; create it when we add any |

`upstream/tests/cachedObjs/` is excluded from the manifest: it is build output,
and is gitignored by upstream (`upstream/.gitignore:27`).

**The check is falsifiable**, which matters more than it passing. Verified
2026-08-01 by running the comparison against a deliberately corrupted manifest
(one hash zeroed → exit 1) and against one naming a file that does not exist
(→ exit 1). A verification step that cannot fail is not evidence.

## Running them

| Command | Scope |
|---|---|
| `make test` | full original suite against the Rust port — **this is the score** |
| `make test-quick` | `fuzzer` + `frametest` only; the edit/run loop |
| `make test-reference` | same full suite against the untouched C library — the baseline |

`make test` and `make test-reference` both include huge-file cases that
pipe `datagen -g6GB` and `-g3G` through the CLI (`test-lz4-fast-hugefile.sh`).
Budget tens of minutes. Use `make test-quick` while implementing.

## The C baseline — our denominator

Run before writing any implementation code, so that "N tests fail" means
something. The original suite against the **untouched C library**, on the
machine we develop on:

| | |
|---|---|
| Date (UTC) | 2026-08-01 |
| Host | x86_64-unknown-linux-gnu, Linux 7.1.5-arch1-1 |
| Compiler | gcc (GCC) 16.1.1 20260728 |
| lz4 commit | `0774d055` |
| **Exit code** | **0 — full suite passed** |
| Wall time | ~35 min, dominated by the 6GB/3GB cases |

```
9522 /   9522  - all tests completed successfully   (frametest)
All tests completed                                 (fuzzer, 47840 items)
All unit tests completed successfully compressionLevel=9
All unit tests completed successfully compressionLevel=10
Basic tests completed
```

The suite is **100% green here for C**, so from this point every failure the
Rust port shows is attributable to the port and not to the environment. Without
that, hours go into debugging "failures" that were never ours.

Reproduce with:

```sh
mkdir -p bench && make test-reference 2>&1 | tee bench/reference.log
```

The raw log is ~1.9 MB of datagen progress bars and is gitignored; the numbers
above are the part worth keeping. Note `fuzzer` and `frametest` are **time**-
bounded (`-T90s`, `tests/Makefile:55`), so the item counts scale with machine
speed — the load-bearing figure is the exit code, not the totals.

*(Performance numbers are a different question and will live in `bench/` — see
PLAN.md §9, deliverable 06. This section is about correctness only.)*
