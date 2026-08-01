# C reference baseline

`make test-reference` — lz4's original suite against the untouched C library,
on the machine we develop on. This is the denominator: it establishes that the
suite is 100% green here for C, so any failure the Rust port shows is ours and
not an environment artefact.

| | |
|---|---|
| Date (UTC) | 2026-08-01 |
| Host | x86_64-unknown-linux-gnu, Linux 7.1.5-arch1-1 |
| Compiler | gcc (GCC) 16.1.1 20260728 |
| lz4 commit | 0774d055 |
| **Exit code** | **0 — full suite passed** |
| Wall time | ~35 min (dominated by the 6GB/3GB huge-file cases) |

Headline counters lifted from the run:

```
9522 /   9522   - all tests completed successfully
All unit tests completed successfully compressionLevel=10
All unit tests completed successfully compressionLevel=9
All tests completed   (fuzzer)
Basic tests completed
```

Full log is not committed (1.9 MB, mostly datagen progress bars). Reproduce with:

```sh
make test-reference 2>&1 | tee bench/reference.log
```
