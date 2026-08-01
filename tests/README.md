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
| `port/` | tests we added — not a substitute for the above |

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

`make test` and `make test-reference` both include `test-lz4-basic`, which
generates 6GB and 3GB files. Budget tens of minutes and ~10GB of scratch space.
Use `make test-quick` while implementing.
