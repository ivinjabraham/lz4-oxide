/* Intentionally empty. The real implementation of lz4 lives in the Rust
   staticlib (liblz4_rs.a). This placeholder exists only so that the original
   tests/Makefile can still produce a lz4.o without compiling lib/lz4.c.
   See DECISIONS.md. */
typedef int lz4_rs_lz4_translation_unit_not_empty;
