echo "========= Running framediff fuzzer =========="
make -C ../upstream/lib liblz4.a && gcc -I ../upstream/lib framediff.c ../upstream/lib/liblz4.a -o /tmp/fd-c
make && gcc -I ../upstream/lib framediff.c ../target/release/liblz4_rs.a -lgcc_s -lutil -lrt -lpthread -lm -ldl -lc -o /tmp/fd-rs

make -C ../upstream/tests datagen 
../upstream/tests/datagen -g1M -P50 > /tmp/in

for m in 0 1 2 3 4 5 6 7; do
    /tmp/fd-c  $m < /tmp/in > /tmp/o-c
    /tmp/fd-rs $m < /tmp/in > /tmp/o-rs
    cmp /tmp/o-c /tmp/o-rs && echo "mode $m BYTE-IDENTICAL"
done

echo "========= Running hc_difftest =========="
# Only levels 1-2 are compared: they select C's lz4mid strategy, the one this
# port implements. Levels 3-12 use the hash-chain and optimal parsers, which are
# not ported, so their output legitimately differs -- see DECISIONS.md 8.2.
gcc -I ../upstream/lib hc_difftest.c ../upstream/lib/liblz4.a -o /tmp/hc-c
gcc -I ../upstream/lib hc_difftest.c ../target/release/liblz4_rs.a \
    -lgcc_s -lutil -lrt -lpthread -lm -ldl -lc -o /tmp/hc-rs

for lvl in 1 2; do
    for sz in 20K 300K 1M; do
        for p in 10 50 90; do
            ../upstream/tests/datagen -g$sz -P$p > /tmp/hcin
            /tmp/hc-c  $lvl < /tmp/hcin > /tmp/hco-c
            /tmp/hc-rs $lvl < /tmp/hcin > /tmp/hco-rs
            cmp /tmp/hco-c /tmp/hco-rs \
                && echo "level $lvl -g$sz -P$p BYTE-IDENTICAL"
        done
    done
done
