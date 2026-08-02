/* Compile against pinned C and Rust, then compare the binary transcripts.
 *
 * Covers the HC surface (`lz4hc.c`): one-shot, external-state, streaming with a
 * loaded dictionary, `saveDictHC`, the two `destSize` (fillOutput) entry points,
 * and compression against an attached dictionary context.
 *
 * Levels 1-2 select C's `lz4mid` strategy and must be **byte-identical**.
 * Levels 3+ select the hash-chain and optimal parsers, which are not ported;
 * this harness therefore only asserts identity for the levels passed on the
 * command line. See DECISIONS.md §8.2.
 *
 * Every result is emitted, including failures and the `srcSizePtr` written back
 * by the fillOutput calls: agreeing on *rejection* and on how much input was
 * consumed is the half of the comparison that finds bugs.
 *
 *   usage: hc_difftest <level> < input   (input is read from stdin)
 */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#define LZ4_HC_STATIC_LINKING_ONLY
#include "lz4hc.h"

#define DICT_SIZE 65536
#define BLOCK_SIZE 65536

static void emit_int(int value) { fwrite(&value, sizeof(value), 1, stdout); }
static void emit_block(const char* data, int size) {
    emit_int(size);
    if (size > 0) fwrite(data, 1, (size_t)size, stdout);
}
/* The index bookkeeping, which is where a streaming port goes wrong silently.
 * Pointers are deliberately not emitted: they are addresses, not behaviour. */
static void emit_state(const LZ4_streamHC_t* s) {
    emit_int((int)s->internal_donotuse.dictLimit);
    emit_int((int)s->internal_donotuse.lowLimit);
    emit_int((int)s->internal_donotuse.nextToUpdate);
    emit_int((int)s->internal_donotuse.compressionLevel);
    emit_int((int)s->internal_donotuse.dirty);
}

int main(int argc, char** argv) {
    int const level = (argc > 1) ? atoi(argv[1]) : 2;
    char* const dict = malloc(DICT_SIZE);
    char* const input = malloc(BLOCK_SIZE);
    int const bound = LZ4_compressBound(BLOCK_SIZE);
    char* const out = malloc((size_t)bound);
    char* const saved = malloc(DICT_SIZE);
    LZ4_streamHC_t* const stream = LZ4_createStreamHC();
    LZ4_streamHC_t* const dctx = LZ4_createStreamHC();
    void* const extState = malloc((size_t)LZ4_sizeofStateHC());
    int dictSize, srcSize, n, cap;

    if (!dict || !input || !out || !saved || !stream || !dctx || !extState) return 1;

    dictSize = (int)fread(dict, 1, DICT_SIZE, stdin);
    srcSize = (int)fread(input, 1, BLOCK_SIZE, stdin);
    if (srcSize <= 0) return 1;

    /* --- one-shot, at generous and at exact capacity --- */
    n = LZ4_compress_HC(input, out, srcSize, bound, level);
    emit_block(out, n);
    emit_block(out, LZ4_compress_HC(input, out, srcSize, n, level));
    /* one byte short: must fail, and must not have written past the buffer */
    emit_int(LZ4_compress_HC(input, out, srcSize, n - 1, level));

    /* --- external state, fresh and fast-reset (reusing the same state) --- */
    emit_block(out, LZ4_compress_HC_extStateHC(
        extState, input, out, srcSize, bound, level));
    emit_block(out, LZ4_compress_HC_extStateHC_fastReset(
        extState, input, out, srcSize, bound, level));
    emit_block(out, LZ4_compress_HC_extStateHC_fastReset(
        extState, input, out, srcSize, bound, level));

    /* --- streaming with a loaded dictionary --- */
    LZ4_resetStreamHC_fast(stream, level);
    emit_int(LZ4_loadDictHC(stream, dict, dictSize));
    n = LZ4_compress_HC_continue(stream, input, out, srcSize, bound);
    emit_block(out, n);
    emit_state(stream);

    /* exactly the right size, then one byte short */
    LZ4_resetStreamHC_fast(stream, level);
    LZ4_loadDictHC(stream, dict, dictSize);
    emit_block(out, LZ4_compress_HC_continue(stream, input, out, srcSize, n));
    emit_state(stream);
    LZ4_resetStreamHC_fast(stream, level);
    LZ4_loadDictHC(stream, dict, dictSize);
    emit_int(LZ4_compress_HC_continue(stream, input, out, srcSize, n - 1));
    emit_state(stream);

    /* --- saveDictHC after a stream --- */
    LZ4_resetStreamHC_fast(stream, level);
    LZ4_loadDictHC(stream, dict, dictSize);
    LZ4_compress_HC_continue(stream, input, out, srcSize, bound);
    n = LZ4_saveDictHC(stream, saved, DICT_SIZE);
    emit_block(saved, n);
    emit_state(stream);
    /* a NULL buffer must be accepted and save nothing */
    emit_int(LZ4_saveDictHC(stream, NULL, 0));

    /* --- fillOutput: both destSize entry points, at several capacities --- */
    for (cap = 5; cap < srcSize; cap = cap * 3 + 7) {
        int consumed = srcSize;
        LZ4_resetStreamHC_fast(stream, level);
        LZ4_loadDictHC(stream, dict, dictSize);
        emit_block(out, LZ4_compress_HC_continue_destSize(
            stream, input, out, &consumed, cap));
        emit_int(consumed);   /* how much input was eaten matters too */
        emit_state(stream);

        consumed = srcSize;
        emit_block(out, LZ4_compress_HC_destSize(
            extState, input, out, &consumed, cap, level));
        emit_int(consumed);
    }

    /* --- attached dictionary context --- */
    /* >4 KB with position==0 takes C's "copy the whole dictCtx" arm;
     * a small block takes the searchIntoDict arm. Both are exercised. */
    LZ4_resetStreamHC_fast(dctx, level);
    emit_int(LZ4_loadDictHC(dctx, dict, dictSize));
    LZ4_resetStreamHC_fast(stream, level);
    LZ4_attach_HC_dictionary(stream, dctx);
    emit_block(out, LZ4_compress_HC_continue(stream, input, out, srcSize, bound));
    emit_state(stream);

    LZ4_resetStreamHC_fast(stream, level);
    LZ4_attach_HC_dictionary(stream, dctx);
    n = srcSize < 2048 ? srcSize : 2048;
    emit_block(out, LZ4_compress_HC_continue(stream, input, out, n, bound));
    emit_state(stream);

    /* detaching must return to the plain path */
    LZ4_resetStreamHC_fast(stream, level);
    LZ4_attach_HC_dictionary(stream, NULL);
    emit_block(out, LZ4_compress_HC_continue(stream, input, out, srcSize, bound));
    emit_state(stream);

    /* --- level clamping: <1 becomes 9, >12 becomes 12 ---
     * Emitted as *self*-comparisons, not raw output: the levels these clamp to
     * (9 and 12) use strategies this port does not implement, so their bytes
     * legitimately differ from C. That clamping happens at all is still shared
     * behaviour, and that is what is compared. */
    {   char* const ref = malloc((size_t)bound);
        int r12, r9;
        r12 = LZ4_compress_HC(ref, ref, 0, bound, 12); /* silence unused warn */
        (void)r12;
        r12 = LZ4_compress_HC(input, ref, srcSize, bound, 12);
        n = LZ4_compress_HC(input, out, srcSize, bound, 13);
        emit_int(n == r12 && memcmp(out, ref, (size_t)(n > 0 ? n : 0)) == 0);
        r9 = LZ4_compress_HC(input, ref, srcSize, bound, 9);
        n = LZ4_compress_HC(input, out, srcSize, bound, 0);
        emit_int(n == r9 && memcmp(out, ref, (size_t)(n > 0 ? n : 0)) == 0);
        n = LZ4_compress_HC(input, out, srcSize, bound, -1);
        emit_int(n == r9 && memcmp(out, ref, (size_t)(n > 0 ? n : 0)) == 0);
        free(ref);
    }

    /* --- empty and degenerate inputs --- */
    emit_int(LZ4_compress_HC(input, out, 0, bound, level));
    emit_int(LZ4_compress_HC(input, out, 0, 0, level));
    emit_int(LZ4_compress_HC(NULL, NULL, 0, 0, level));
    emit_int(LZ4_compress_HC(input, out, srcSize, 0, level));
    emit_int(LZ4_compress_HC(input, out, srcSize, 1, level));

    LZ4_freeStreamHC(stream);
    LZ4_freeStreamHC(dctx);
    return 0;
}
