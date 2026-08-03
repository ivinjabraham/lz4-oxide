/* Differential harness for the FRAME format: compile twice -- once against the
 * C library, once against the Rust port -- and compare the bytes.
 *
 * The block-level equivalent is difftest.c; this one covers lz4frame.c, where
 * the output depends on the preference set as well as on the input, so the mode
 * argument sweeps the combinations that change framing decisions.
 *
 *   make -C upstream/lib liblz4.a
 *   gcc -I upstream/lib fuzz/framediff.c upstream/lib/liblz4.a -o /tmp/fd-c
 *
 *   make
 *   gcc -I upstream/lib fuzz/framediff.c target/release/liblz4_rs.a \
 *       -lgcc_s -lutil -lrt -lpthread -lm -ldl -lc -o /tmp/fd-rs
 *
 *   make -C upstream/tests datagen
 *   ./upstream/tests/datagen -g1M -P50 > /tmp/in
 *   for m in 0 1 2 3 4 5 6 7; do
 *     /tmp/fd-c  $m < /tmp/in > /tmp/o-c
 *     /tmp/fd-rs $m < /tmp/in > /tmp/o-rs
 *     cmp /tmp/o-c /tmp/o-rs && echo "mode $m BYTE-IDENTICAL"
 *   done
 *
 * Round-trip tests cannot replace this: wrong-but-valid output decompresses
 * perfectly. Mode 0 vs mode 1 is the pair that matters most -- linked vs
 * independent blocks -- because linked blocks are where the compressor's
 * history handling can diverge while still producing decodable output. That is
 * exactly the bug documented at Cctx::make_block in src/frame.rs.
 */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include "lz4frame.h"

int main(int argc, char** argv)
{
    static char in[4 << 20];
    size_t const n = fread(in, 1, sizeof(in), stdin);
    int const mode = (argc > 1) ? atoi(argv[1]) : 0;

    LZ4F_preferences_t prefs;
    memset(&prefs, 0, sizeof(prefs));
    switch (mode) {
        case 0: break;   /* defaults: linked blocks, 64 KB, no checksums */
        case 1: prefs.frameInfo.blockMode = LZ4F_blockIndependent; break;
        case 2: prefs.frameInfo.contentChecksumFlag = 1; break;
        case 3: prefs.frameInfo.blockChecksumFlag = 1; break;
        case 4: prefs.frameInfo.blockSizeID = LZ4F_max256KB; break;
        case 5: prefs.frameInfo.blockSizeID = LZ4F_max4MB;
                prefs.frameInfo.contentChecksumFlag = 1; break;
        case 6: prefs.frameInfo.contentSize = n; break;  /* declared size in header */
        case 7: prefs.compressionLevel = -3; break;      /* "fast acceleration" */
        default: fprintf(stderr, "unknown mode %d\n", mode); return 2;
    }

    {   size_t const bound = LZ4F_compressFrameBound(n, &prefs);
        char* const out = (char*)malloc(bound);
        if (out == NULL) { fprintf(stderr, "malloc failed\n"); return 2; }

        /* LZ4F_* returns size_t: an error is a huge unsigned value, so this
         * must be tested with LZ4F_isError, never with `c < 0`. */
        {   size_t const c = LZ4F_compressFrame(out, bound, in, n, &prefs);
            if (LZ4F_isError(c)) {
                fprintf(stderr, "compress error: %s\n", LZ4F_getErrorName(c));
                free(out);
                return 1;
            }
            fwrite(out, 1, c, stdout);
        }
        free(out);
    }
    return 0;
}
