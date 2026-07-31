/* Intentionally empty. The real implementation of xxhash lives in the Rust
   staticlib (liblz4_rs.a). This placeholder exists only so that the original
   tests/Makefile can still produce a xxhash.o without compiling lib/xxhash.c.
   See DECISIONS.md. */
typedef int lz4_rs_xxhash_translation_unit_not_empty;
