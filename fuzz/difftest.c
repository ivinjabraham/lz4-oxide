/* Differential harness: compile twice -- once against the C library, once
 * against the Rust port -- and compare.
 *
 *   gcc -I upstream/lib fuzz/difftest.c upstream/lib/liblz4.a -o /tmp/diff-c
 *   gcc -I upstream/lib fuzz/difftest.c target/release/liblz4_rs.a \
 *       -lgcc_s -lutil -lrt -lpthread -lm -ldl -lc -o /tmp/diff-rs
 *
 * Modes (argv[1]); all read the sample on stdin:
 *
 *   c   compress from a separate buffer; compressed bytes to stdout.  `cmp`
 *       the two outputs -- this is the byte-identity check that round-trip
 *       tests cannot make, because wrong-but-valid output decompresses fine.
 *   i   the same, but compressing IN PLACE, mirroring fuzzer.c:1206-1216.
 *       Exercises the overlapping src/dst path.
 *   r   round-trip: compress, then LZ4_decompress_safe, verify identity.
 *   d   round-trip with an IN-PLACE decompression, mirroring fuzzer.c:1240.
 *   p   LZ4_decompress_safe_partial against a truncated output budget.
 *
 * Feed it >64KB to reach the byU32 table type; <64KB stays on byU16.
 */
#define LZ4_STATIC_LINKING_ONLY   /* LZ4_COMPRESS_INPLACE_BUFFER_SIZE */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include "lz4.h"

#define MAXIN (8 << 20)
#define OUTCAP (MAXIN + (MAXIN / 255) + 16 + LZ4_COMPRESS_INPLACE_MARGIN)

static char in[MAXIN];
static char out[OUTCAP];
static char back[OUTCAP];

int main(int argc, char** argv)
{
    char const mode = (argc > 1) ? argv[1][0] : 'c';
    int const n = (int)fread(in, 1, MAXIN, stdin);

    if (mode == 'c') {
        int const c = LZ4_compress_default(in, out, n, (int)sizeof(out));
        if (c <= 0) { fprintf(stderr, "compress failed (%d)\n", c); return 1; }
        fwrite(out, 1, (size_t)c, stdout);
        return 0;
    }

    if (mode == 'i') {
        int const maxCSize = LZ4_COMPRESSBOUND(n);
        int const outSize = LZ4_COMPRESS_INPLACE_BUFFER_SIZE(maxCSize);
        int const startInputIndex = outSize - n;
        char* const startInput = out + startInputIndex;
        int c;
        if (outSize > (int)sizeof(out)) { fprintf(stderr, "sample too large\n"); return 1; }
        memcpy(startInput, in, (size_t)n);   /* input at END of buffer */
        c = LZ4_compress_default(startInput, out, n, maxCSize);
        if (c <= 0) { fprintf(stderr, "in-place compress failed (%d)\n", c); return 1; }
        fwrite(out, 1, (size_t)c, stdout);
        return 0;
    }

    if (mode == 'r') {
        int c = LZ4_compress_default(in, out, n, (int)sizeof(out));
        int d;
        if (c <= 0) { fprintf(stderr, "compress failed (%d)\n", c); return 1; }
        d = LZ4_decompress_safe(out, back, c, (int)sizeof(back));
        if (d != n) { fprintf(stderr, "FAIL decompress returned %d, want %d\n", d, n); return 1; }
        if (memcmp(in, back, (size_t)n) != 0) { fprintf(stderr, "FAIL round-trip mismatch\n"); return 1; }
        fprintf(stderr, "round-trip OK (%d -> %d -> %d)\n", n, c, d);
        return 0;
    }

    if (mode == 'd') {
        /* in-place decompression: compressed data sits at the end of the
         * buffer, plaintext is written over it from the start. */
        int const c = LZ4_compress_default(in, out, n, (int)sizeof(out));
        int const bufSize = LZ4_DECOMPRESS_INPLACE_BUFFER_SIZE(n);
        char* const startCompressed = back + bufSize - c;
        int d;
        if (c <= 0) { fprintf(stderr, "compress failed (%d)\n", c); return 1; }
        if (bufSize > (int)sizeof(back)) { fprintf(stderr, "sample too large\n"); return 1; }
        memcpy(startCompressed, out, (size_t)c);
        d = LZ4_decompress_safe(startCompressed, back, c, n);
        if (d != n) { fprintf(stderr, "FAIL in-place decompress returned %d, want %d\n", d, n); return 1; }
        if (memcmp(in, back, (size_t)n) != 0) { fprintf(stderr, "FAIL in-place mismatch\n"); return 1; }
        fprintf(stderr, "in-place decompress OK (%d -> %d -> %d)\n", n, c, d);
        return 0;
    }

    if (mode == 'p') {
        int const c = LZ4_compress_default(in, out, n, (int)sizeof(out));
        int const target = n / 3;
        int d;
        if (c <= 0) { fprintf(stderr, "compress failed (%d)\n", c); return 1; }
        d = LZ4_decompress_safe_partial(out, back, c, target, target);
        if (d < 0 || d > target) { fprintf(stderr, "FAIL partial returned %d (target %d)\n", d, target); return 1; }
        if (memcmp(in, back, (size_t)d) != 0) { fprintf(stderr, "FAIL partial prefix mismatch at %d\n", d); return 1; }
        fprintf(stderr, "partial OK (%d of %d bytes)\n", d, target);
        return 0;
    }

    if (mode == 'x') {
        /* Read ALREADY-COMPRESSED (and possibly corrupted) bytes and print the
         * exact return value of LZ4_decompress_safe. Errors are position
         * encoded (-(ip-src)-1, lz4.c:2462), so comparing the integer compares
         * *where* each implementation decided the block was malformed, not
         * merely that it did. dstCapacity comes from argv[2]. */
        int const cap = (argc > 2) ? atoi(argv[2]) : 65536;
        int const d = LZ4_decompress_safe(in, back, n, cap);
        printf("%d\n", d);
        return 0;
    }

    if (mode == 'q') {
        /* As 'x', but through LZ4_decompress_safe_partial, and comparing the
         * OUTPUT BYTES as well as the return value.
         *
         * The partial path handles offset==0 differently from the full one:
         * lz4.c:2411-2422 has no `LZ4_write32(op, 0)`, so `*op++ = *match++`
         * with match==op leaves the destination untouched, where the full path
         * (lz4.c:2426) fills zeros. A destination pre-filled with zeros would
         * therefore let a divergence pass unnoticed -- hence 0xA5. */
        int const cap = (argc > 2) ? atoi(argv[2]) : 65536;
        int const target = (argc > 3) ? atoi(argv[3]) : cap;
        unsigned h = 2166136261u;
        int d, i;
        memset(back, 0xA5, (size_t)cap);
        d = LZ4_decompress_safe_partial(in, back, n, target, cap);
        /* Hash only the bytes the call claims to have produced: beyond that,
         * C's wildcopy legitimately leaves debris we deliberately do not
         * reproduce (PORTING.md §3.2), so comparing it would be a false alarm. */
        for (i = 0; i < d && i < cap; i++) { h ^= (unsigned char)back[i]; h *= 16777619u; }
        printf("%d %08x", d, (d < 0) ? 0u : h);
        if (getenv("DIFFTEST_HEX")) {
            int const lim = (d > 0 && d < 32) ? d : (cap < 32 ? cap : 32);
            printf("  [");
            for (i = 0; i < lim; i++) printf("%02x", (unsigned char)back[i]);
            printf("]");
        }
        printf("\n");
        return 0;
    }

    fprintf(stderr, "unknown mode '%c'\n", mode);
    return 2;
}
