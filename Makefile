# Build the Rust port and run lz4's ORIGINAL, UNMODIFIED test suite against it.
#
# The key trick lives in TEST_OVERRIDES below. lz4's tests do not link
# liblz4.a -- tests/Makefile builds fuzzer/frametest from lz4.o, lz4hc.o etc.,
# which build/make/multiconf.make resolves through `vpath %.c $(C_SRCDIRS)`.
# So we override two ordinary make variables on the command line:
#
#   C_SRCDIRS  drop ../lib, substitute our empty stub translation units, so
#              the C implementation is never compiled at all.
#   LDLIBS     multiconf.make:222 appends this to every link line, so our
#              Rust staticlib satisfies the symbols the C tests reference.
#
# No file under tests/ is modified. See DECISIONS.md.

# ---------------------------------------------------------------------------
# Where are the original lz4 C sources?
#
# This port is its own repository, so upstream is external. Override with:
#   make LZ4_SRC=/path/to/lz4 test
# Default order matches build.rs: a submodule at ./upstream, else a sibling
# checkout at ../lz4.
# ---------------------------------------------------------------------------
ifeq ($(origin LZ4_SRC), undefined)
  ifneq ($(wildcard $(CURDIR)/upstream/lib/lz4.h),)
    LZ4_SRC := $(CURDIR)/upstream
  else
    LZ4_SRC := $(abspath $(CURDIR)/../lz4)
  endif
endif
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
	  echo "Point at your lz4 checkout, either way:"; \
	  echo "  make LZ4_SRC=/path/to/lz4 $(or $(MAKECMDGOALS),all)"; \
	  echo "  git submodule add https://github.com/lz4/lz4 upstream"; \
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

# Just prove the skeleton links -- the hour-one milestone.
.PHONY: link-check
link-check: $(RUST_LIB)
	$(MAKE) -C $(ROOT)/tests fuzzer frametest $(TEST_OVERRIDES)
	@echo "OK: original C tests link against the Rust port."

# Reference build: the untouched C implementation, for differential comparison.
.PHONY: test-reference
test-reference:
	$(MAKE) -C $(ROOT)/tests test

# Confirm no C implementation leaked into the binary: every LZ4_* symbol the
# tests resolve must come from the Rust archive.
.PHONY: abi-check
abi-check: $(RUST_LIB) $(ABI_TXT)
	@nm --defined-only --extern-only $(RUST_LIB) \
	  | awk '$$2 == "T" {print $$3}' | grep -E '^(LZ4_|LZ4F_)' | sort -u > $(CURDIR)/abi.rust.txt
	@echo "original: $$(wc -l < $(ABI_TXT))  rust: $$(wc -l < $(CURDIR)/abi.rust.txt)"
	@diff -u $(ABI_TXT) $(CURDIR)/abi.rust.txt \
	  && echo "OK: Rust archive exports exactly the original ABI." \
	  || { echo "MISMATCH: see diff above."; exit 1; }

.PHONY: clean
clean:
	cargo clean
	$(RM) $(CURDIR)/abi.rust.txt
