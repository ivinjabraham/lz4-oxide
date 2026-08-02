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

