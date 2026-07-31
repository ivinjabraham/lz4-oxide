/* Intentionally empty. The real implementation of lz4frame lives in the Rust
   staticlib (liblz4_rs.a). This placeholder exists only so that the original
   tests/Makefile can still produce a lz4frame.o without compiling lib/lz4frame.c.
   See DECISIONS.md. */
typedef int lz4_rs_lz4frame_translation_unit_not_empty;
