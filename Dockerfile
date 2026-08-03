# Native Linux images with upstream fetched at the kickoff commit.
#
#   docker build --target verify .       # required checks, not the full suite
#   docker build --target full-test .    # complete unmodified upstream suite
#   docker build --target artifacts -o out .
#   docker build -t lz4-oxide .          # full-tested small CLI (default target)

FROM debian:bookworm-slim AS upstream-source

RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates git \
    && rm -rf /var/lib/apt/lists/* \
    && git init /upstream \
    && git -C /upstream fetch --depth=1 \
        https://github.com/lz4/lz4.git \
        0774d05537f9762f838f7ab541b7765f1a729cb5 \
    && git -C /upstream checkout --detach FETCH_HEAD \
    && rm -rf /upstream/.git/hooks /upstream/.git/logs \
    && rm -f /upstream/.git/FETCH_HEAD /upstream/.git/ORIG_HEAD \
    && test "$(git -C /upstream rev-parse HEAD)" = \
        "0774d05537f9762f838f7ab541b7765f1a729cb5" \
    && test -z "$(git -C /upstream status --porcelain)" \
    && test -z "$(git -C /upstream config --get-regexp '^remote\..*\.url$' || true)"

FROM rust:1.97.1-bookworm AS builder

RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        binutils \
        build-essential \
        git \
        python3 \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /src
COPY . .
COPY --from=upstream-source /upstream/ upstream/

# The fetched checkout retains only the metadata needed by kickoff-verify.
RUN test -f upstream/lib/lz4.h \
    && test "$(git -C upstream rev-parse HEAD)" = "0774d05537f9762f838f7ab541b7765f1a729cb5" \
    && test -z "$(git -C upstream status --porcelain)"

# Never trust archives, objects, or multiconf caches copied from the host.
RUN rm -rf \
        target \
        upstream/lib/cachedObjs \
        upstream/programs/cachedObjs \
        upstream/tests/cachedObjs \
    && rm -f \
        upstream/lib/liblz4.a \
        upstream/lib/liblz4.so \
        upstream/lib/liblz4.so.* \
        upstream/programs/lz4 \
        upstream/programs/lz4c \
        upstream/programs/lz4cat \
        upstream/programs/unlz4 \
        upstream/tests/datagen \
        upstream/tests/decompress-partial \
        upstream/tests/decompress-partial-usingDict \
        upstream/tests/frametest \
        upstream/tests/fullbench \
        upstream/tests/fuzzer \
        upstream/tests/lz4_all.c \
        upstream/tests/lz4_all.o \
    && cargo build --locked --release \
    && make cli \
    && cli_dir="$(dirname "$(readlink -f upstream/programs/lz4)")" \
    && grep -q '/src/cstub/lz4.c' "$cli_dir/lz4.d"

# Prepare both export stages once. ldd supplies the native loader and shared
# libraries without installing a package manager or toolchain in the runtime.
RUN install -D -m 0644 target/release/liblz4_rs.a /artifacts/lib/liblz4_rs.a \
    && install -D -m 0755 target/release/liblz4_rs.so /artifacts/lib/liblz4_rs.so \
    && install -D -m 0644 abi.txt /artifacts/abi.txt \
    && install -D -m 0644 LICENSE /artifacts/licenses/lz4-oxide-LICENSE \
    && install -D -m 0644 upstream/LICENSE /artifacts/licenses/lz4-LICENSE \
    && mkdir -p /artifacts/include \
    && cp upstream/lib/*.h /artifacts/include/ \
    && install -D -m 0755 upstream/programs/lz4 /runtime-root/usr/local/bin/lz4 \
    && install -D -m 0644 LICENSE /runtime-root/usr/share/licenses/lz4-oxide/LICENSE \
    && install -D -m 0644 upstream/LICENSE /runtime-root/usr/share/licenses/lz4/LICENSE \
    && ldd upstream/programs/lz4 \
        | awk '/=> \/.* \(0x/ { print $3 } /^[[:space:]]*\/.* \(0x/ { print $1 }' \
        | sort -u \
        | xargs -r cp --parents -t /runtime-root

FROM builder AS verify

RUN cargo test --locked --release \
    && make kickoff-verify abi-check unsafe-count link-check \
    && make difftest

FROM scratch AS artifacts

COPY --from=verify /artifacts/ /

FROM scratch AS runtime-base

FROM runtime-base AS runtime

COPY --from=builder /runtime-root/ /

USER 65532:65532
ENTRYPOINT ["/usr/local/bin/lz4"]
CMD ["--version"]

FROM verify AS full-test

# This expensive target runs the complete unmodified upstream suite. Use the
# verify target during development when the full multi-gigabyte suite is not
# required.
RUN make test

# The default runnable image is emitted only after the full suite succeeds,
# while retaining only the runtime filesystem.
FROM runtime-base AS verified-runtime

COPY --from=full-test /runtime-root/ /

USER 65532:65532
ENTRYPOINT ["/usr/local/bin/lz4"]
CMD ["--version"]
