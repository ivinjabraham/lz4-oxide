# One step from a clean checkout to a runnable artifact:
#
#   docker build -t lz4-oxide .
#   docker run --rm lz4-oxide            # runs the original C test suite
#   docker run --rm lz4-oxide make abi-check kickoff-verify unsafe-count
#
# !! UNVERIFIED: written on a host without Docker installed; not yet built.
#    Build it once before relying on it. Tracked in PLAN.md §9.
#
# The image deliberately keeps a C toolchain. That is not the port leaning on
# C -- no lib/*.c is ever compiled (see DECISIONS.md §4) -- it is because the
# *test suite* is C and must stay that way for the suite to be the original.

FROM rust:1-bookworm

# build-essential: gcc + make, to compile lz4's own C test harness.
# binutils     : nm, for `make abi-check`.
# git          : `make kickoff-verify` asks the submodule if it is clean.
# python3      : tools/gen_ffi.py (bootstrap only; not on the build path).
RUN apt-get update && apt-get install -y --no-install-recommends \
        build-essential binutils git python3 \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /src

# The upstream submodule must be populated in the build context:
#   git clone --recursive ...   (or: git submodule update --init)
# There is no `git submodule` step here on purpose -- the image must not be
# able to silently fetch a *different* commit than the one hashed at kickoff.
COPY . .

# Fail early and legibly if the context was built from a non-recursive clone.
RUN test -f upstream/lib/lz4.h || { \
      echo "ERROR: upstream/ is empty -- the build context lacks the submodule."; \
      echo "       git submodule update --init, then rebuild."; exit 1; }

RUN make

# Prove the artifact is what it claims to be, at build time.
RUN make abi-check && make kickoff-verify && make unsafe-count

CMD ["make", "test"]
