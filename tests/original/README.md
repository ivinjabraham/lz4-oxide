# Original lz4 Test Suite

`upstream/tests/` is the authoritative original test suite. It is a Git
submodule pinned to lz4 commit `0774d05537f9762f838f7ab541b7765f1a729cb5`, not
a vendored copy under this directory.

Keeping one authoritative checkout avoids a second copy that could drift or be
silently edited. This `README` exists solely to map the Port Mortem suggested
`tests/original/` layout to the actual source of truth.

---

Verify the provenance and integrity of every original test file with:

```sh
make kickoff-verify
```

The command verifies that the submodule is at the pinned commit with a clean
working tree, and that all 42 tracked files under `upstream/tests/` match the
SHA-256 values in [`../KICKOFF.sha256`](../KICKOFF.sha256).

Run the unmodified suite against the Rust port with:

```sh
make test
```
