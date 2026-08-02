//! C-compatible type definitions for the LZ4 ABI.
//!
//! Two categories live here, and the distinction matters:
//!
//! * **Opaque types** (`LZ4F_cctx`, `LZ4F_dctx`, ...) are only ever handled by
//!   the C side as pointers returned from our own create/free functions. We are
//!   free to lay these out however we like.
//!
//! * **Caller-allocated types** (`LZ4_stream_t`, `LZ4_streamHC_t`,
//!   `LZ4_streamDecode_t`, `XXH32_state_t`, `XXH64_state_t`) are declared *by
//!   the C test harness on its own stack* and handed to us as pointers. Their
//!   size and alignment are fixed by the C headers, so we represent them as
//!   correctly-sized opaque storage and assert the layout against values
//!   probed from the real headers at build time (see build.rs).
//!
//!   Concretely: `tests/fuzzer.c` does `LZ4_stream_t stream;` and
//!   `tests/frametest.c:1202` does `XXH64_state_t xxh64;`. If our size or
//!   alignment disagrees with theirs, we corrupt their stack.

#![allow(non_camel_case_types)]

use core::ffi::{c_char, c_int, c_uint, c_ulonglong, c_void};

// Sizes/alignments probed from the real C headers at build time.
include!(concat!(env!("OUT_DIR"), "/layout.rs"));

/// Compile-time assertion that a plain struct matches the probed C layout.
macro_rules! assert_layout {
    ($t:ty, $size:expr, $align:expr) => {
        const _: () = {
            assert!(core::mem::size_of::<$t>() == $size);
            assert!(core::mem::align_of::<$t>() == $align);
        };
    };
}

// ---------------------------------------------------------------------------
// Caller-allocated state
// ---------------------------------------------------------------------------

/// Define a type whose storage exactly reproduces a C type's size *and*
/// alignment.
///
/// Rust's `repr(align(N))` needs a literal, so we can't feed it the probed
/// constant directly. Instead we pick the storage word type (`u64` for
/// 8-aligned C types, `u32` for 4-aligned ones) and let the array give us the
/// alignment naturally. The asserts then verify against the probe, so if a
/// header change or a different target moves either number, the build fails
/// here rather than silently corrupting the C caller's stack.
macro_rules! caller_allocated {
    ($name:ident, $size:expr, $align:expr, $word:ty) => {
        #[repr(C)]
        pub struct $name {
            pub(crate) storage: [$word; $size / core::mem::size_of::<$word>()],
        }
        const _: () = {
            assert!($size % core::mem::size_of::<$word>() == 0);
            assert!(core::mem::size_of::<$name>() == $size);
            assert!(core::mem::align_of::<$name>() == $align);
        };
    };
}

caller_allocated!(LZ4_stream_t, LZ4_STREAM_SIZE, LZ4_STREAM_ALIGN, u64);
caller_allocated!(LZ4_streamHC_t, LZ4_STREAMHC_SIZE, LZ4_STREAMHC_ALIGN, u64);
caller_allocated!(
    LZ4_streamDecode_t,
    LZ4_STREAMDECODE_SIZE,
    LZ4_STREAMDECODE_ALIGN,
    u64
);
// Note: XXH32_state_t is 4-aligned in C (all uint32_t members), unlike the
// others. Over-aligning it here would be a latent bug.
caller_allocated!(XXH32_state_t, XXH32_STATE_SIZE, XXH32_STATE_ALIGN, u32);
caller_allocated!(XXH64_state_t, XXH64_STATE_SIZE, XXH64_STATE_ALIGN, u64);

// ---------------------------------------------------------------------------
// xxHash scalars and canonical forms
// ---------------------------------------------------------------------------

pub type XXH32_hash_t = u32;
pub type XXH64_hash_t = u64;

/// `typedef enum { XXH_OK = 0, XXH_ERROR } XXH_errorcode;` -- a C enum, so
/// `c_int` at the ABI boundary.
pub type XXH_errorcode = c_int;

pub const XXH_OK: XXH_errorcode = 0;
pub const XXH_ERROR: XXH_errorcode = 1;

#[repr(C)]
pub struct XXH32_canonical_t {
    pub digest: [u8; 4],
}

#[repr(C)]
pub struct XXH64_canonical_t {
    pub digest: [u8; 8],
}

// ---------------------------------------------------------------------------
// Frame format
// ---------------------------------------------------------------------------

pub type LZ4F_errorCode_t = usize;

/// The error *enum*, distinct from `LZ4F_errorCode_t` above. Generated in C by
/// the `LZ4F_LIST_ERRORS` X-macro (lib/lz4frame.h:698) as
/// `typedef enum { LZ4F_OK_NoError = 0, ... } LZ4F_errorCodes;`, so `c_int`.
/// Returned by `LZ4F_getErrorCode()`.
pub type LZ4F_errorCodes = c_int;

// C enums; all values are small non-negative ints, so `c_int` matches the ABI.
pub type LZ4F_blockSizeID_t = c_int;
pub type LZ4F_blockMode_t = c_int;
pub type LZ4F_contentChecksum_t = c_int;
pub type LZ4F_blockChecksum_t = c_int;
pub type LZ4F_frameType_t = c_int;

#[repr(C)]
pub struct LZ4F_frameInfo_t {
    pub blockSizeID: LZ4F_blockSizeID_t,
    pub blockMode: LZ4F_blockMode_t,
    pub contentChecksumFlag: LZ4F_contentChecksum_t,
    pub frameType: LZ4F_frameType_t,
    pub contentSize: c_ulonglong,
    pub dictID: c_uint,
    pub blockChecksumFlag: LZ4F_blockChecksum_t,
}
assert_layout!(LZ4F_frameInfo_t, LZ4F_FRAMEINFO_SIZE, LZ4F_FRAMEINFO_ALIGN);

#[repr(C)]
pub struct LZ4F_preferences_t {
    pub frameInfo: LZ4F_frameInfo_t,
    pub compressionLevel: c_int,
    pub autoFlush: c_uint,
    pub favorDecSpeed: c_uint,
    pub reserved: [c_uint; 3],
}
assert_layout!(
    LZ4F_preferences_t,
    LZ4F_PREFERENCES_SIZE,
    LZ4F_PREFERENCES_ALIGN
);

#[repr(C)]
pub struct LZ4F_compressOptions_t {
    pub stableSrc: c_uint,
    pub reserved: [c_uint; 3],
}
assert_layout!(
    LZ4F_compressOptions_t,
    LZ4F_COMPRESSOPTS_SIZE,
    LZ4F_COMPRESSOPTS_ALIGN
);

#[repr(C)]
pub struct LZ4F_decompressOptions_t {
    pub stableDst: c_uint,
    pub skipChecksums: c_uint,
    pub reserved1: c_uint,
    pub reserved0: c_uint,
}
assert_layout!(
    LZ4F_decompressOptions_t,
    LZ4F_DECOMPRESSOPTS_SIZE,
    LZ4F_DECOMPRESSOPTS_ALIGN
);

/// Passed **by value** to `LZ4F_createCDict_advanced` and
/// `LZ4F_createCompressionContext_advanced`. A by-value struct with the wrong
/// layout corrupts silently instead of failing to link, so this one is
/// asserted as carefully as the caller-allocated types.
///
/// `Copy` because C stores it by value too (`cctx->cmem = customMem`): four
/// pointers with no ownership semantics of their own.
#[derive(Clone, Copy)]
#[repr(C)]
pub struct LZ4F_CustomMem {
    pub customAlloc: *mut c_void,
    pub customCalloc: *mut c_void,
    pub customFree: *mut c_void,
    pub opaqueState: *mut c_void,
}
assert_layout!(LZ4F_CustomMem, LZ4F_CUSTOMMEM_SIZE, LZ4F_CUSTOMMEM_ALIGN);

// ---------------------------------------------------------------------------
// Opaque handles (we own the allocation, C only holds the pointer)
// ---------------------------------------------------------------------------

macro_rules! opaque {
    ($($name:ident),* $(,)?) => {$(
        #[repr(C)]
        pub struct $name {
            _private: [u8; 0],
        }
    )*};
}

opaque!(LZ4F_cctx, LZ4F_dctx, LZ4F_CDict, LZ4_readFile_t, LZ4_writeFile_t);

/// C `FILE`. Only ever passed through to the C stdio the caller owns.
#[repr(C)]
pub struct FILE {
    _private: [u8; 0],
}

/// Unused placeholder so `c_char` stays referenced if no signature needs it.
#[allow(dead_code)]
pub(crate) type _CharAlias = c_char;
