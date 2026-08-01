/* Compile twice -- once against C, once against Rust -- and cmp the output. */
#include <stdio.h>
#include <string.h>
#include "lz4.h"

int main(void) {
    char in[64 * 1024], out[LZ4_COMPRESSBOUND(64 * 1024)];
    size_t n = fread(in, 1, sizeof(in), stdin);
    int c = LZ4_compress_default(in, out, (int)n, (int)sizeof(out));
    fwrite(out, 1, (size_t)c, stdout);
    return c <= 0;
}
