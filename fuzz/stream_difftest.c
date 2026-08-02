/* Compile against pinned C and Rust, then compare the binary transcripts. */
#include <stdio.h>
#include <string.h>
#define LZ4_STATIC_LINKING_ONLY
#include "lz4.h"

int LZ4_compress_forceExtDict(
    LZ4_stream_t* stream, const char* source, char* destination, int source_size);

static void emit_int(int value) { fwrite(&value, sizeof(value), 1, stdout); }
static void emit_block(const char* data, int size) {
    emit_int(size);
    if (size > 0) fwrite(data, 1, (size_t)size, stdout);
}
static void emit_state(const LZ4_stream_t* stream) {
    fwrite(stream->internal_donotuse.hashTable,
           sizeof(stream->internal_donotuse.hashTable), 1, stdout);
    fwrite(&stream->internal_donotuse.currentOffset,
           sizeof(stream->internal_donotuse.currentOffset), 1, stdout);
    fwrite(&stream->internal_donotuse.tableType,
           sizeof(stream->internal_donotuse.tableType), 1, stdout);
    fwrite(&stream->internal_donotuse.dictSize,
           sizeof(stream->internal_donotuse.dictSize), 1, stdout);
}

int main(void) {
    enum { DICT_SIZE = 70000, BLOCK_SIZE = 4096 };
    char dictionary[DICT_SIZE];
    char input[3 * BLOCK_SIZE];
    char compressed[3][LZ4_COMPRESSBOUND(BLOCK_SIZE)];
    char forced[LZ4_COMPRESSBOUND(BLOCK_SIZE)];
    char attached[LZ4_COMPRESSBOUND(2 * BLOCK_SIZE)];
    char saved[65536];
    char decoded[3 * BLOCK_SIZE];
    char ring[2 * BLOCK_SIZE];
    char prefix_buffer[65535 + BLOCK_SIZE];
    int compressed_size[3];
    int index;
    LZ4_stream_t stream_body;
    LZ4_stream_t dictionary_body;
    LZ4_stream_t working_body;
    LZ4_streamDecode_t decode_body;
    LZ4_stream_t* stream = LZ4_initStream(&stream_body, sizeof(stream_body));

    for (index = 0; index < DICT_SIZE; ++index)
        dictionary[index] = (char)((index * 17 + index / 13) % 251);
    for (index = 0; index < 3 * BLOCK_SIZE; ++index)
        input[index] = dictionary[DICT_SIZE - 3 * BLOCK_SIZE + index];

    emit_int(stream != NULL);
    emit_int(LZ4_loadDict(stream, dictionary, DICT_SIZE));
    for (index = 0; index < 3; ++index) {
        compressed_size[index] = LZ4_compress_fast_continue(
            stream, input + index * BLOCK_SIZE, compressed[index], BLOCK_SIZE,
            sizeof(compressed[index]), index + 1);
        emit_block(compressed[index], compressed_size[index]);
        emit_state(stream);
    }
    index = LZ4_saveDict(stream, saved, sizeof(saved));
    emit_int(index);
    fwrite(saved, 1, (size_t)index, stdout);
    emit_block(forced, LZ4_compress_forceExtDict(
        stream, input, forced, BLOCK_SIZE));

    memset(&decode_body, 0, sizeof(decode_body));
    emit_int(LZ4_setStreamDecode(&decode_body, dictionary, DICT_SIZE));
    for (index = 0; index < 3; ++index) {
        int result = LZ4_decompress_safe_continue(
            &decode_body, compressed[index], decoded + index * BLOCK_SIZE,
            compressed_size[index], BLOCK_SIZE);
        emit_int(result);
        if (result > 0) fwrite(decoded + index * BLOCK_SIZE, 1, (size_t)result, stdout);
    }

    memset(&decode_body, 0, sizeof(decode_body));
    emit_int(LZ4_setStreamDecode(&decode_body, dictionary, DICT_SIZE));
    emit_int(LZ4_decompress_safe_continue(
        &decode_body, compressed[0], ring + BLOCK_SIZE,
        compressed_size[0], BLOCK_SIZE));
    emit_int(LZ4_decompress_safe_continue(
        &decode_body, compressed[1], ring,
        compressed_size[1], BLOCK_SIZE));
    fwrite(ring, 1, sizeof(ring), stdout);

    memcpy(prefix_buffer, dictionary + DICT_SIZE - 65535, 65535);
    memset(&decode_body, 0, sizeof(decode_body));
    emit_int(LZ4_setStreamDecode(&decode_body, prefix_buffer, 65535));
    index = LZ4_decompress_safe_continue(
        &decode_body, compressed[0], prefix_buffer + 65535,
        compressed_size[0], BLOCK_SIZE);
    emit_int(index);
    if (index > 0) fwrite(prefix_buffer + 65535, 1, (size_t)index, stdout);

    LZ4_initStream(&dictionary_body, sizeof(dictionary_body));
    LZ4_initStream(&working_body, sizeof(working_body));
    emit_int(LZ4_loadDict(&dictionary_body, dictionary, DICT_SIZE));
    LZ4_attach_dictionary(&working_body, &dictionary_body);
    emit_block(forced, LZ4_compress_fast_continue(
        &working_body, input, forced, 2048, sizeof(forced), 1));
    LZ4_resetStream(&working_body);
    LZ4_attach_dictionary(&working_body, &dictionary_body);
    emit_block(attached, LZ4_compress_fast_continue(
        &working_body, input, attached, 2 * BLOCK_SIZE, sizeof(attached), 1));

    LZ4_resetStream(&dictionary_body);
    emit_int(LZ4_loadDict(&dictionary_body, dictionary, DICT_SIZE));
    LZ4_attach_dictionary(&dictionary_body, &dictionary_body);
    emit_block(forced, LZ4_compress_fast_continue(
        &dictionary_body, input, forced, 2048, sizeof(forced), 1));
    emit_state(&dictionary_body);
    LZ4_resetStream(&dictionary_body);
    emit_int(LZ4_loadDict(&dictionary_body, dictionary, DICT_SIZE));
    LZ4_attach_dictionary(&dictionary_body, &dictionary_body);
    emit_block(attached, LZ4_compress_fast_continue(
        &dictionary_body, input, attached, 2 * BLOCK_SIZE, sizeof(attached), 1));
    emit_state(&dictionary_body);

    LZ4_resetStream(&dictionary_body);
    LZ4_resetStream(&working_body);
    LZ4_attach_dictionary(&working_body, &dictionary_body);
    emit_state(&working_body);

    LZ4_resetStream(stream);
    emit_int(LZ4_loadDictSlow(stream, dictionary, DICT_SIZE));
    emit_block(compressed[0], LZ4_compress_fast_continue(
        stream, input, compressed[0], BLOCK_SIZE, sizeof(compressed[0]), 1));
    LZ4_resetStream(stream);
    emit_int(LZ4_loadDict(stream, dictionary, DICT_SIZE));
    emit_int(LZ4_compress_fast_continue(
        stream, input, compressed[0], BLOCK_SIZE, 1, 1));
    emit_state(stream);
    LZ4_resetStream(stream);
    emit_int(LZ4_loadDict(stream, dictionary, DICT_SIZE));
    emit_int(LZ4_compress_fast_continue(
        stream, input, compressed[0], BLOCK_SIZE, 0, 1));
    emit_state(stream);
    return 0;
}
