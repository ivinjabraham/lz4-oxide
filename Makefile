# Build the Rust port and run lz4's ORIGINAL, UNMODIFIED test suite against it.
#
# The key trick lives in TEST_OVERRIDES below. lz4's tests do not link
# liblz4.a -- tests/Makefile builds fuzzer/frametest from lz4.o, lz4hc.o etc.,
# which build/make/multiconf.make resolves through `vpath %.c $(C_SRCDIRS)`.
# So we override two ordinary make variables on the command line:
#
#   C_SRCDIRS  drop ../lib, substitute our empty stub translation units, so
#              so no lib/*.c object is ever linked into a test binary.
#   LDLIBS     multiconf.make:222 appends this to every link line, so our
#              Rust staticlib satisfies the symbols the C tests reference.
#
# No file under tests/ is modified. See DECISIONS.md §3.

# ---------------------------------------------------------------------------
# Where are the original lz4 C sources?
#
# ./upstream, the submodule pinned at the commit this port is written against.
# Matches build.rs. Override with:
#   make LZ4_SRC=/path/to/lz4 test
# but pointing elsewhere invalidates kickoff-verify and abi-check -- both are
# claims about that specific commit.
# ---------------------------------------------------------------------------
LZ4_SRC ?= $(CURDIR)/upstream
export LZ4_SRC

ROOT      := $(LZ4_SRC)
PROFILE   ?= release
RUST_LIB  := $(CURDIR)/target/$(PROFILE)/liblz4_rs.a
CARGOFLAG := $(if $(filter release,$(PROFILE)),--release,)

# Rust's staticlib needs libstd's own dependencies at the final link.
# Authoritative list, from (rustc 1.97.1, x86_64-unknown-linux-gnu):
#   rustc --print native-static-libs --crate-type staticlib <any>.rs
# Re-run that on a different host/toolchain before assuming it still holds.
RUST_LINK_DEPS ?= -lgcc_s -lutil -lrt -lpthread -lm -ldl -lc

# tests/ and programs/ each set their own C_SRCDIRS, so the substitution is
# per-directory: tests/ needs ../programs on the path (for lorem.o, bench.o),
# programs/ does not.
TEST_OVERRIDES = C_SRCDIRS="$(CURDIR)/cstub $(ROOT)/programs ." \
                 LDLIBS="$(RUST_LIB) $(RUST_LINK_DEPS)"
PRG_OVERRIDES  = C_SRCDIRS="$(CURDIR)/cstub ." \
                 LDLIBS="$(RUST_LIB) $(RUST_LINK_DEPS)"

# Fail early and legibly if upstream isn't where we think it is, rather than
# letting cargo or cc produce a confusing "no such file" deep in a build.
.PHONY: check-src
check-src:
	@test -f "$(ROOT)/lib/lz4.h" || { \
	  echo "ERROR: lz4 C sources not found at: $(ROOT)"; \
	  echo "       (looked for \$$LZ4_SRC/lib/lz4.h)"; \
	  echo; \
	  echo "upstream/ is a submodule -- a plain 'git clone' leaves it empty:"; \
	  echo "  git submodule update --init --recursive"; \
	  echo; \
	  echo "Or point at an existing checkout:"; \
	  echo "  make LZ4_SRC=/path/to/lz4 $(or $(MAKECMDGOALS),all)"; \
	  exit 1; }
	@echo "using lz4 sources: $(ROOT)"

.PHONY: all
all: $(RUST_LIB)

.PHONY: $(RUST_LIB)
$(RUST_LIB): check-src
	cargo build $(CARGOFLAG)

# ---------------------------------------------------------------------------
# Regenerate the FFI skeleton from the C headers + the real archive's symbols.
# ---------------------------------------------------------------------------
ABI_TXT := $(CURDIR)/abi.txt

# The symbol contract, taken from the real artefact rather than the headers.
# Fails loudly on any non-text symbol: gen_ffi.py emits `extern "C" fn` for
# every entry, so a data symbol would be stubbed as a function -- which links
# cleanly and then misbehaves at runtime.
$(ABI_TXT):
	$(MAKE) -C $(ROOT)/lib liblz4.a
	@nm --defined-only --extern-only $(ROOT)/lib/liblz4.a \
	  | awk '$$2 ~ /^[DBR]$$/ {print "non-function exported symbol: " $$2 " " $$3; bad=1} \
	         END {exit bad+0}' \
	  || { echo "ERROR: see above; these need #[no_mangle] pub static, not fn."; exit 1; }
	nm --defined-only --extern-only $(ROOT)/lib/liblz4.a \
	  | awk '$$2 == "T" {print $$3}' | sort -u > $@

# Re-derive the committed ABI contract. Only needed if the upstream pin moves.
# This compiles the real C library, so run it deliberately, not as a habit.
.PHONY: abi-refresh
abi-refresh:
	$(RM) $(ABI_TXT)
	$(MAKE) $(ABI_TXT)
	@echo "abi.txt re-derived at $$(git -C $(ROOT) rev-parse --short HEAD). Commit it, and update tests/KICKOFF.sha256."

.PHONY: gen-ffi
gen-ffi: $(ABI_TXT)
	python3 $(CURDIR)/tools/gen_ffi.py $(ROOT) $(ABI_TXT) $(CURDIR)/src/ffi.rs

# ---------------------------------------------------------------------------
# Tests
# ---------------------------------------------------------------------------

# The lz4 CLI, linked against the Rust port.
#
# This MUST be built as its own step. tests/Makefile:68-71 declares
#     lz4: MAKEFLAGS=
#     lz4:  $(MAKE) -C $(PRGDIR) $@ CFLAGS="$(CFLAGS)"
# and clearing MAKEFLAGS stops command-line variable overrides propagating into
# the sub-make. Relying on `make -C tests test` to build the CLI therefore
# yields a CLI linked against the *C* library -- so every tests/test-lz4-*.sh
# would pass while proving nothing about the port.
.PHONY: cli
cli: $(RUST_LIB)
	$(MAKE) -C $(ROOT)/programs lz4 $(PRG_OVERRIDES)

# Original C test suite, linked against the Rust port.
#
# `-o lz4` marks the CLI target as already up to date, so the sub-make above
# does not run again (unoverridden) and relink the CLI against C.
.PHONY: test
test: $(RUST_LIB) cli
	$(MAKE) -C $(ROOT)/tests test -o lz4 $(TEST_OVERRIDES)

# Build the C-level test programs against the port, then PROVE the result
# really contains Rust.
#
# Building is not by itself evidence. multiconf.make keys its object cache on
# the compiler and link flags but NOT on C_SRCDIRS, so an lz4.o compiled
# earlier from the real ../lib/lz4.c can be silently relinked into a binary
# built with our overrides. Without provenance-check this target printed OK
# over a pure-C fuzzer.
.PHONY: link-check
link-check: $(RUST_LIB)
	$(MAKE) -C $(ROOT)/tests fuzzer frametest $(TEST_OVERRIDES)
	@$(MAKE) --no-print-directory provenance-check

# Reference build: the untouched C implementation, for differential comparison.
#
# NOTE: this is the FULL suite. tests/test-lz4-fast-hugefile.sh pipes
# `datagen -g6GB` and `-g3G` through the CLI, so budget tens of minutes.
# It also leaves C-linked binaries behind on the shared paths -- run
# `make provenance-check` before trusting any binary afterwards.
# For the edit/run loop use `make test-quick`.
.PHONY: test-reference
test-reference:
	$(MAKE) -C $(ROOT)/tests test

# The C-level tests only: fuzzer + frametest, no shell scripts, no huge files.
# This is the loop to run while implementing; `make test` is the score.
.PHONY: test-quick
test-quick: $(RUST_LIB)
	$(MAKE) -C $(ROOT)/tests test-fuzzer test-frametest $(TEST_OVERRIDES)

# ---------------------------------------------------------------------------
# Submission evidence
# ---------------------------------------------------------------------------

# Which lz4.c actually went into each built test binary?
#
# This is the only check that looks inside the artefacts. Everything else
# inspects our own archive or the upstream source tree, and would stay green
# while the binaries a judge runs were built from cached C objects.
#
# multiconf.make symlinks tests/<prog> into cachedObjs/<flag-hash>/, and each
# object's .d file records the path the compiler actually resolved through
# vpath. Ours must always be $(CURDIR)/cstub/lz4.c.
.PHONY: provenance-check
provenance-check:
	@fail=0; found=0; \
	 for b in $(ROOT)/tests/* $(ROOT)/programs/lz4; do \
	   test -L "$$b" || continue; \
	   t=$$(readlink -f "$$b" 2>/dev/null); \
	   test -n "$$t" && test -e "$$t" || continue; \
	   d=$$(dirname "$$t"); \
	   test -f "$$d/lz4.d" || continue; \
	   found=$$((found+1)); \
	   src=$$(tr ' \\' '\n\n' < "$$d/lz4.d" | grep -E 'lz4\.c$$' | head -1); \
	   case "$$src" in \
	     $(CURDIR)/cstub/*) printf '  rust   %s\n' "$$(basename $$b)";; \
	     *) printf '  C !!   %-16s lz4.o compiled from %s\n' "$$(basename $$b)" "$$src"; fail=1;; \
	   esac; \
	 done; \
	 test "$$found" -gt 0 \
	   || { echo "FAIL: no test binaries built yet. Run 'make link-check' or 'make test'."; exit 1; }; \
	 if [ "$$fail" = 0 ]; then \
	   echo "OK: all $$found built test binaries were compiled from cstub/, not from lib/."; \
	 else \
	   echo "FAIL: the binaries marked 'C !!' contain the C implementation."; \
	   echo "      Cause: a previous build cached an lz4.o from lib/ under the same flag hash."; \
	   echo "      Fix:   rm -rf $(ROOT)/tests/cachedObjs $(ROOT)/programs/cachedObjs && make link-check"; \
	   exit 1; \
	 fi

# The original test suite is untouched. Four independent checks, in order of
# how easily each is defeated on its own:
#   1. upstream really is a git checkout   (without this, 2 and 3 pass silently
#      -- git reports errors on stderr, so an unguarded $(shell) sees "")
#   2. it is at the commit pinned at kickoff
#   3. its working tree has no edits and no untracked files
#   4. every tracked file under tests/ still hashes to its kickoff value
#
# Known limit: a file matching upstream's own .gitignore (tmp*, fuzzer, ...)
# can be added under tests/ without any of these noticing. The suite's shell
# scripts legitimately create tmp* files, so flagging them is not possible
# without false positives on every run.
KICKOFF := $(CURDIR)/tests/KICKOFF.sha256

.PHONY: kickoff-verify
kickoff-verify:
	@git -C $(ROOT) rev-parse --is-inside-work-tree >/dev/null 2>&1 \
	  || { echo "FAIL: $(ROOT) is not a git checkout, so it cannot be shown unmodified."; exit 1; }
	@pinned=$$(awk '/^# Pinned commit/ {print $$5}' $(KICKOFF)); \
	 head=$$(git -C $(ROOT) rev-parse HEAD); \
	 test "$$pinned" = "$$head" \
	   || { echo "FAIL: upstream is at $$head; kickoff pinned $$pinned."; exit 1; }
	@dirty=$$(git -C $(ROOT) status --porcelain); \
	 test -z "$$dirty" \
	   || { echo "FAIL: upstream working tree is not clean:"; echo "$$dirty"; exit 1; }
	@want=$$(grep -vc '^#' $(KICKOFF)); \
	 have=$$(git -C $(ROOT) ls-files tests | wc -l); \
	 test "$$want" -eq "$$have" \
	   || { echo "FAIL: upstream/tests tracks $$have files; the manifest lists $$want."; exit 1; }
	@grep -v '^#' $(KICKOFF) | (cd $(ROOT) && sha256sum --quiet -c -) \
	  || { echo "FAIL: original test suite has been modified."; exit 1; }
	@echo "OK: upstream at $$(git -C $(ROOT) rev-parse --short HEAD), tree clean, $$(grep -vc '^#' $(KICKOFF)) test files match their kickoff hashes."

# `unsafe` budget. The exit criterion asks for a documented threshold against
# the source line count, so emit the number rather than just asserting a policy.
# Ported C SLOC (non-blank, non-comment) is measured, not hardcoded.
# Three things this has to get right, each of which it previously got wrong:
#   - find, not src/*.rs: a glob stops measuring the moment anyone splits a
#     module into a subdirectory, and reports OK for the rest of the project.
#   - the sed that removes `#![forbid(unsafe_code)]` is anchored to that exact
#     line. Deleting every line containing "unsafe_code" also deletes
#     `#[allow(unsafe_code)] unsafe fn ...`, which is precisely the line a
#     developer writes next to an unsafe they had to add.
#   - depend on check-src and assert the C SLOC is non-zero, or a wrong
#     LZ4_SRC yields a divide-by-zero and still prints OK.
.PHONY: unsafe-count
unsafe-count: check-src
	@code() { sed -e 's://.*::' -e '/^[[:space:]]*#!\[forbid(unsafe_code)\]/d' "$$@"; }; \
	 rs=$$(find $(CURDIR)/src -name '*.rs' | sort); \
	 test -n "$$rs" || { echo "FAIL: no .rs files found under src/."; exit 1; }; \
	 blocks=$$(code $$rs | grep -o '\bunsafe\b' | wc -l); \
	 outside=$$(for f in $$rs; do \
	              case $$f in */ffi.rs) continue;; esac; \
	              code $$f | grep -q '\bunsafe\b' && echo $$f; \
	            done); \
	 csloc=$$(cat $(ROOT)/lib/lz4.c $(ROOT)/lib/lz4hc.c $(ROOT)/lib/lz4frame.c \
	           $(ROOT)/lib/lz4file.c $(ROOT)/lib/xxhash.c \
	          | grep -v '^[[:space:]]*$$' | grep -v '^[[:space:]]*[/*]' | wc -l); \
	 test "$$csloc" -gt 0 \
	   || { echo "FAIL: measured 0 C SLOC -- LZ4_SRC does not point at an lz4 checkout."; exit 1; }; \
	 rsloc=$$(cat $$rs | grep -v '^[[:space:]]*$$' | grep -v '^[[:space:]]*//' | wc -l); \
	 echo "unsafe occurrences : $$blocks   (in $$(echo "$$rs" | wc -l) files under src/)"; \
	 echo "C SLOC ported      : $$csloc"; \
	 echo "Rust SLOC          : $$rsloc"; \
	 echo "ratio              : $$(awk "BEGIN{printf \"%.2f\", $$blocks*1000/$$csloc}") unsafe per 1000 C SLOC"; \
	 test -z "$$outside" \
	   || { echo "FAIL: unsafe outside ffi.rs:"; echo "$$outside"; exit 1; }; \
	 if [ "$$blocks" = 0 ]; then \
	   echo "OK: no unsafe anywhere yet."; \
	 else \
	   echo "OK: all $$blocks unsafe occurrences are confined to src/ffi.rs."; \
	 fi

# Does our archive export exactly the symbol set the original exports?
#
# Scope, precisely: this reads `nm` on target/release/liblz4_rs.a and diffs it
# against abi.txt. It does NOT open a test binary and cannot tell you whether a
# binary was linked from C -- that is what provenance-check is for. Keep the
# two claims apart; conflating them was how a fully C-linked fuzzer coexisted
# with "OK: Rust archive exports exactly the original ABI".
#
# abi.txt is a committed contract, derived once from the real liblz4.a at the
# pinned commit. It is deliberately not regenerated on every run: rebuilding
# the C library to re-derive it would compile the very sources this project
# claims never to link. Its authority therefore rests on kickoff-verify
# confirming upstream is still at that commit. Re-derive with `make abi-refresh`
# only if the pin ever moves.
.PHONY: abi-check
abi-check: $(RUST_LIB) $(ABI_TXT)
	@nm --defined-only --extern-only $(RUST_LIB) \
	  | awk '$$2 == "T" {print $$3}' | grep -E '^(LZ4_|LZ4F_)' | sort -u > $(CURDIR)/abi.rust.txt
	@echo "original: $$(wc -l < $(ABI_TXT))  rust: $$(wc -l < $(CURDIR)/abi.rust.txt)"
	@# Compare under a fixed collation. `abi.txt` was recorded with a
	@# locale-dependent sort, so without LC_ALL the two sides can disagree on
	@# case ordering alone and report a mismatch with identical symbol sets.
	@LC_ALL=C sort $(ABI_TXT) > $(CURDIR)/abi.orig.sorted.txt
	@LC_ALL=C sort $(CURDIR)/abi.rust.txt > $(CURDIR)/abi.rust.sorted.txt
	@diff -u $(CURDIR)/abi.orig.sorted.txt $(CURDIR)/abi.rust.sorted.txt \
	  && echo "OK: Rust archive exports exactly the original ABI." \
	  || { echo "MISMATCH: see diff above."; exit 1; }

.PHONY: clean
clean:
	cargo clean
	$(RM) $(CURDIR)/abi.rust.txt
	$(RM) $(CURDIR)/abi.rust.sorted.txt $(CURDIR)/abi.orig.sorted.txt
