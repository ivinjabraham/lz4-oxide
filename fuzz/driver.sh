#!/usr/bin/env bash
# Differential harnesses: compile each twice, against the pinned C library and
# against the Rust port, and compare the bytes.
#
# `set -e` on the build steps only. The comparison loops must keep going after a
# divergence so one failure does not hide the rest, so they count instead --
# and the script exits non-zero at the end if anything diverged. A harness that
# cannot fail is not a harness.
set -u
cd "$(dirname "${BASH_SOURCE[0]}")" || exit 1
LIBS="-lgcc_s -lutil -lrt -lpthread -lm -ldl -lc"
diverged=0

build() { # name
    gcc -I ../upstream/lib "$1.c" ../upstream/lib/liblz4.a       -o "/tmp/$1-c"  || exit 1
    gcc -I ../upstream/lib "$1.c" ../target/release/liblz4_rs.a $LIBS -o "/tmp/$1-rs" || exit 1
}

check() { # label, a, b
    if cmp -s "$2" "$3"; then echo "$1 BYTE-IDENTICAL"
    else echo "$1 DIVERGED"; diverged=$((diverged + 1)); fi
}

# `make -C ..`, not a bare `make`: there is no fuzz/Makefile, so a bare make
# fails, the `&&` short-circuits, /tmp/framediff-rs is never built, and every
# comparison below silently prints nothing instead of failing.
make -C ../upstream/lib liblz4.a || exit 1
make -C .. || exit 1
make -C ../upstream/tests datagen || exit 1

echo "========= Running framediff fuzzer =========="
build framediff
../upstream/tests/datagen -g1M -P50 > /tmp/in
for m in 0 1 2 3 4 5 6 7; do
    /tmp/framediff-c  "$m" < /tmp/in > /tmp/o-c
    /tmp/framediff-rs "$m" < /tmp/in > /tmp/o-rs
    check "mode $m" /tmp/o-c /tmp/o-rs
done

echo "========= Running hc_difftest =========="
# Exercise all three HC strategies: lz4mid, greedy hash chain, and optimal.
build hc_difftest
for lvl in 1 2 3 4 5 6 7 8 9 10 11 12; do
    for sz in 20K 300K 1M; do
        for p in 10 50 90; do
            ../upstream/tests/datagen -g"$sz" -P"$p" > /tmp/hcin
            /tmp/hc_difftest-c  "$lvl" < /tmp/hcin > /tmp/hco-c
            /tmp/hc_difftest-rs "$lvl" < /tmp/hcin > /tmp/hco-rs
            check "level $lvl -g$sz -P$p" /tmp/hco-c /tmp/hco-rs
        done
    done
done

echo "diverged: $diverged"
exit $((diverged > 0))
