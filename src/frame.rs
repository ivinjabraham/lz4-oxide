//! frame: `lib/lz4frame.c` — the LZ4 frame format, in safe Rust on slices.
//!
//! Entry points live in `crate::ffi`. Nothing here may use `unsafe`; pointer
//! handling stays in the FFI shim so the port's unsafe surface is small and
//! countable.
//!
//! ## What a frame is
//!
//! ```text
//! magic(4) | frame descriptor(3..15) | block | block | ... | endmark(4) | checksum(0..4)
//! ```
//!
//! Per `doc/lz4_Frame_format.md`. Each block is a 4-byte little-endian header —
//! whose top bit means "stored uncompressed" — then the data, then an optional
//! 4-byte xxh32 of *the block as written*.
//!
//! ## The error convention is the trap here
//!
//! `LZ4F_*` returns `size_t`. An error is `(size_t)-(ptrdiff_t)code`, i.e. a
//! huge unsigned number, so `if (r < 0)` is always false and compiles cleanly.
//! Everything internal returns `Result`; the translation to that encoding
//! happens once, in [`Error::to_code`], and nowhere else.
//!
//! ## Two states, and why they are shaped differently to C's
//!
//! C's `LZ4F_cctx`/`LZ4F_dctx` hold raw `BYTE*` cursors into their own
//! malloc'd buffers (`tmpIn`, `tmpOut`, `dict`), and much of `lz4frame.c` is
//! pointer arithmetic comparing them. Those are all *indices* here — `Vec<u8>`
//! plus `usize` — because a pointer into one's own allocation is exactly what
//! safe Rust cannot express. The arithmetic is otherwise transcribed as
//! written, since it decides how much gets buffered and therefore where the
//! block boundaries fall, and block boundaries change the compressed bytes.
//!
//! Both are heap types reached only through an opaque handle that C obtains
//! from our own create/free functions, so unlike `LZ4_stream_t` they are free
//! to contain `Vec`.
#![forbid(unsafe_code)]

use crate::block::{self, Input, StreamState};
use crate::xxh::{self, Xxh32State};

// --- lz4frame.h:281-294, lz4frame.c:245-257 --------------------------------
pub const LZ4F_VERSION: u32 = 100;
pub const HEADER_SIZE_MIN: usize = 7;
pub const HEADER_SIZE_MAX: usize = 19;
/// Block header: 4 bytes, little-endian, top bit = "uncompressed".
pub const BH_SIZE: usize = 4;
/// Block footer: the optional per-block checksum.
pub const BF_SIZE: usize = 4;
pub const MAGICNUMBER: u32 = 0x184D_2204;
pub const MAGIC_SKIPPABLE_START: u32 = 0x184D_2A50;
pub const MIN_SIZE_TO_KNOW_HEADER_LENGTH: usize = 5;
const BLOCK_UNCOMPRESSED_FLAG: u32 = 0x8000_0000;
/// `LZ4F_BLOCKSIZEID_DEFAULT` (lz4frame.c:252) — `LZ4F_max64KB`.
const BLOCKSIZEID_DEFAULT: i32 = 4;

/// `LZ4HC_CLEVEL_MAX` (lz4hc.h), returned by `LZ4F_compressionLevel_max`.
pub const LZ4HC_CLEVEL_MAX: i32 = 12;

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// `LZ4F_errorCodes` (lz4frame.h:668-693). The discriminants are the *order of
/// the X-macro list*, and `LZ4F_getErrorName` indexes an array of strings with
/// them, so neither the order nor the names may be rearranged.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(i32)]
pub enum Error {
    Generic = 1,
    MaxBlockSizeInvalid = 2,
    BlockModeInvalid = 3,
    ParameterInvalid = 4,
    CompressionLevelInvalid = 5,
    HeaderVersionWrong = 6,
    BlockChecksumInvalid = 7,
    ReservedFlagSet = 8,
    AllocationFailed = 9,
    SrcSizeTooLarge = 10,
    DstMaxSizeTooSmall = 11,
    FrameHeaderIncomplete = 12,
    FrameTypeUnknown = 13,
    FrameSizeWrong = 14,
    SrcPtrWrong = 15,
    DecompressionFailed = 16,
    HeaderChecksumInvalid = 17,
    ContentChecksumInvalid = 18,
    FrameDecodingAlreadyStarted = 19,
    CompressionStateUninitialized = 20,
    ParameterNull = 21,
    IoWrite = 22,
    IoRead = 23,
}

/// `LZ4F_ERROR_maxCode` — one past the last real code. `LZ4F_isError` compares
/// against it, so it must track the enum above.
pub const ERROR_MAX_CODE: i32 = 24;

impl Error {
    /// `LZ4F_returnErrorCode` (lz4frame.c:313): `(size_t)-(ptrdiff_t)code`.
    ///
    /// This wrapping negation *is* the ABI. Callers test it with
    /// `LZ4F_isError`, never with `r < 0`.
    pub fn to_code(self) -> usize {
        (self as isize).wrapping_neg() as usize
    }
}

/// `LZ4F_isError` (lz4frame.c:295).
pub fn is_error(code: usize) -> bool {
    code > (-(ERROR_MAX_CODE as isize)) as usize
}

/// `LZ4F_getErrorCode` (lz4frame.c:307).
pub fn error_code(result: usize) -> i32 {
    if !is_error(result) {
        return 0;
    }
    (result as isize).wrapping_neg() as i32
}

/// The strings `LZ4F_getErrorName` returns, in enum order, NUL-terminated
/// because C hands them out as `const char*`. Index 0 is the no-error slot.
pub static ERROR_STRINGS: [&[u8]; ERROR_MAX_CODE as usize + 1] = [
    b"OK_NoError\0",
    b"ERROR_GENERIC\0",
    b"ERROR_maxBlockSize_invalid\0",
    b"ERROR_blockMode_invalid\0",
    b"ERROR_parameter_invalid\0",
    b"ERROR_compressionLevel_invalid\0",
    b"ERROR_headerVersion_wrong\0",
    b"ERROR_blockChecksum_invalid\0",
    b"ERROR_reservedFlag_set\0",
    b"ERROR_allocation_failed\0",
    b"ERROR_srcSize_tooLarge\0",
    b"ERROR_dstMaxSize_tooSmall\0",
    b"ERROR_frameHeader_incomplete\0",
    b"ERROR_frameType_unknown\0",
    b"ERROR_frameSize_wrong\0",
    b"ERROR_srcPtr_wrong\0",
    b"ERROR_decompressionFailed\0",
    b"ERROR_headerChecksum_invalid\0",
    b"ERROR_contentChecksum_invalid\0",
    b"ERROR_frameDecoding_alreadyStarted\0",
    b"ERROR_compressionState_uninitialized\0",
    b"ERROR_parameter_null\0",
    b"ERROR_io_write\0",
    b"ERROR_io_read\0",
    b"ERROR_maxCode\0",
];

pub type Res<T> = Result<T, Error>;

// ---------------------------------------------------------------------------
// Preferences — the plain-data mirror of LZ4F_preferences_t
// ---------------------------------------------------------------------------

/// A copy of `LZ4F_frameInfo_t`, decoupled from the `#[repr(C)]` ABI struct so
/// that the state machine works in ordinary Rust types.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FrameInfo {
    pub block_size_id: i32,
    /// 0 = linked, 1 = independent.
    pub block_mode: i32,
    pub content_checksum: bool,
    /// 0 = normal frame, 1 = skippable.
    pub frame_type: i32,
    /// 0 means "unknown", which is why it is not an `Option`: the flag in the
    /// header is literally `contentSize > 0` (lz4frame.c:807).
    pub content_size: u64,
    pub dict_id: u32,
    pub block_checksum: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Preferences {
    pub frame_info: FrameInfo,
    pub compression_level: i32,
    pub auto_flush: bool,
    pub favor_dec_speed: bool,
}

/// `LZ4F_getBlockSize` (lz4frame.c:335).
pub fn get_block_size(block_size_id: i32) -> Res<usize> {
    const BLOCK_SIZES: [usize; 4] = [64 * 1024, 256 * 1024, 1024 * 1024, 4 * 1024 * 1024];
    let id = if block_size_id == 0 {
        BLOCKSIZEID_DEFAULT
    } else {
        block_size_id
    };
    if id < 4 || id > 7 {
        return Err(Error::MaxBlockSizeInvalid);
    }
    Ok(BLOCK_SIZES[(id - 4) as usize])
}

/// `LZ4F_optimalBSID` (lz4frame.c:361) — shrink the block size when the whole
/// input fits in a smaller one. Changes where blocks break, so it is not a
/// cosmetic choice.
fn optimal_bsid(requested: i32, src_size: usize) -> i32 {
    let mut proposed = 4;
    let mut max_block_size = 64 * 1024usize;
    while requested > proposed {
        if src_size <= max_block_size {
            return proposed;
        }
        proposed += 1;
        max_block_size <<= 2;
    }
    requested
}

/// `LZ4F_compressBound_internal` (lz4frame.c:381).
///
/// `prefs == None` means "worst case", which C builds by enabling *both*
/// checksums (lz4frame.c:386-387) — not by zeroing the struct.
fn compress_bound_internal(
    src_size: usize,
    prefs: Option<&Preferences>,
    already_buffered: usize,
) -> usize {
    let worst = Preferences {
        frame_info: FrameInfo {
            block_size_id: 4,
            content_checksum: true,
            block_checksum: true,
            ..FrameInfo::default()
        },
        ..Preferences::default()
    };
    let p = prefs.unwrap_or(&worst);

    let flush = p.auto_flush || src_size == 0;
    let block_size = get_block_size(p.frame_info.block_size_id).unwrap_or(64 * 1024);
    let max_buffered = block_size - 1;
    let buffered = already_buffered.min(max_buffered);
    let max_src_size = src_size + buffered;
    let nb_full_blocks = max_src_size / block_size;
    let partial_block_size = max_src_size & (block_size - 1);
    let last_block_size = if flush { partial_block_size } else { 0 };
    let nb_blocks = nb_full_blocks + usize::from(last_block_size > 0);

    let block_crc_size = BF_SIZE * usize::from(p.frame_info.block_checksum);
    let frame_end = BH_SIZE + usize::from(p.frame_info.content_checksum) * BF_SIZE;

    ((BH_SIZE + block_crc_size) * nb_blocks)
        + (block_size * nb_full_blocks)
        + last_block_size
        + frame_end
}

/// `LZ4F_compressBound` (lz4frame.c:884).
///
/// Note the asymmetry, which is not a typo: with `autoFlush` off, C passes
/// `(size_t)-1` as `alreadyBuffered`, letting the clamp inside pick
/// `blockSize - 1` — the worst case where the internal buffer is nearly full.
pub fn compress_bound(src_size: usize, prefs: Option<&Preferences>) -> usize {
    match prefs {
        Some(p) if p.auto_flush => compress_bound_internal(src_size, Some(p), 0),
        _ => compress_bound_internal(src_size, prefs, usize::MAX),
    }
}

/// `LZ4F_compressFrameBound` (lz4frame.c:408).
pub fn compress_frame_bound(src_size: usize, prefs: Option<&Preferences>) -> usize {
    let mut p = prefs.copied().unwrap_or_default();
    p.auto_flush = true;
    HEADER_SIZE_MAX + compress_bound_internal(src_size, Some(&p), 0)
}

// ---------------------------------------------------------------------------
// Little-endian helpers (lz4frame.c:189-233)
// ---------------------------------------------------------------------------

#[inline]
fn read_le32(b: &[u8]) -> u32 {
    u32::from_le_bytes(b[..4].try_into().unwrap())
}

#[inline]
fn read_le64(b: &[u8]) -> u64 {
    u64::from_le_bytes(b[..8].try_into().unwrap())
}

#[inline]
fn write_le32(b: &mut [u8], v: u32) {
    b[..4].copy_from_slice(&v.to_le_bytes());
}

/// A freshly reset xxh32 state. `Xxh32State` deliberately has no `Default`: its
/// zero value is not a valid hashing state (the `v1..v4` seeds are derived from
/// the seed), so it is only ever created through a reset.
fn new_xxh32() -> Xxh32State {
    let mut s = Xxh32State {
        total_len_32: 0,
        large_len: 0,
        v1: 0,
        v2: 0,
        v3: 0,
        v4: 0,
        mem: [0; 16],
        memsize: 0,
        reserved: 0,
    };
    xxh::xxh32_reset(&mut s, 0);
    s
}

/// `LZ4F_headerChecksum` (lz4frame.c:351) — the *second* byte of the xxh32, not
/// the low one.
#[inline]
fn header_checksum(header: &[u8]) -> u8 {
    (xxh::xxh32(header, 0) >> 8) as u8
}

// ---------------------------------------------------------------------------
// Compression context
// ---------------------------------------------------------------------------

/// `LZ4F_BlockCompressMode_e` (lz4frame.c:264).
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum BlockCompressMode {
    Compressed,
    Uncompressed,
}

/// A digested dictionary — `LZ4F_CDict` (lz4frame.c:533).
///
/// C precomputes both a fast and an HC table so the cost is paid once. We keep
/// the content and re-prime per use; the HC table arrives with `src/hc.rs`.
pub struct CDict {
    pub content: Vec<u8>,
}

impl CDict {
    /// `LZ4F_createCDict_advanced` (lz4frame.c:541). Only the last 64 KB of a
    /// longer dictionary is useful, and C truncates to it *before* digesting, so
    /// the retained bytes must match or the tables would disagree.
    pub fn new(dict: &[u8]) -> CDict {
        let start = dict.len().saturating_sub(64 * 1024);
        CDict {
            content: dict[start..].to_vec(),
        }
    }
}

/// `LZ4F_cctx` (lz4frame.c:267).
pub struct Cctx {
    pub version: u32,
    /// `cStage`: 0 = uninitialized, 1 = header written and accepting input.
    stage: u32,
    prefs: Preferences,
    max_block_size: usize,
    /// The staging buffer for data not yet forming a whole block — C's
    /// `tmpBuff`. C also keeps a `tmpIn` cursor into it so that saved history
    /// can sit in front of the pending input; we hold history separately in
    /// `dict`, so the pending input always starts at 0 here.
    tmp: Vec<u8>,
    tmp_in_size: usize,
    total_in_size: u64,
    xxh: Xxh32State,
    /// The block codec's table and index bookkeeping, held across blocks —
    /// which is what makes linked blocks linked.
    stream: StreamState,
    /// History for the next block: the tail of the previous one, or a loaded
    /// dictionary. Owned, where C points into either the caller's buffer or its
    /// own `tmpBuff` depending on `stableSrc`.
    dict: Vec<u8>,
    /// The frame's dictionary, kept separately so independent blocks can
    /// re-prime from it per block while linked blocks prime once per frame.
    frame_dict: Vec<u8>,
    block_compress_mode: BlockCompressMode,
}

impl Default for Cctx {
    fn default() -> Self {
        Self::new()
    }
}

impl Cctx {
    pub fn new() -> Cctx {
        Cctx {
            version: LZ4F_VERSION,
            stage: 0,
            prefs: Preferences::default(),
            max_block_size: 0,
            tmp: Vec::new(),
            tmp_in_size: 0,
            total_in_size: 0,
            xxh: new_xxh32(),
            stream: StreamState::new(),
            dict: Vec::new(),
            frame_dict: Vec::new(),
            block_compress_mode: BlockCompressMode::Compressed,
        }
    }

    /// The heap this context holds *besides* the struct itself — C's
    /// `maxBufferSize` term in `LZ4F_cctx_size` (lz4frame.c:700).
    ///
    /// The struct term is added by `ffi.rs`, which knows the real block size.
    pub fn heap_size(&self) -> usize {
        self.tmp.capacity() + self.dict.capacity() + self.frame_dict.capacity()
    }

    fn linked(&self) -> bool {
        self.prefs.frame_info.block_mode == 0
    }

    /// `LZ4F_compressBegin_internal` (lz4frame.c:706) — validate preferences,
    /// size the buffers, reset the checksum, and write the frame header.
    pub fn begin(
        &mut self,
        dst: &mut [u8],
        dict: Option<&[u8]>,
        cdict: Option<&CDict>,
        prefs: Option<&Preferences>,
    ) -> Res<usize> {
        if dst.len() < HEADER_SIZE_MAX {
            return Err(Error::DstMaxSizeTooSmall);
        }
        self.prefs = prefs.copied().unwrap_or_default();

        // Buffer management (lz4frame.c:754-771).
        if self.prefs.frame_info.block_size_id == 0 {
            self.prefs.frame_info.block_size_id = BLOCKSIZEID_DEFAULT;
        }
        self.max_block_size = get_block_size(self.prefs.frame_info.block_size_id)?;
        if self.tmp.len() < self.max_block_size {
            self.tmp.resize(self.max_block_size, 0);
        }
        self.tmp_in_size = 0;
        self.total_in_size = 0;
        xxh::xxh32_reset(&mut self.xxh, 0);
        self.block_compress_mode = BlockCompressMode::Compressed;

        // Context init (lz4frame.c:774-794). For linked blocks the stream is
        // primed once per frame; independent blocks re-prime per block, which is
        // why the dictionary is kept in `frame_dict` as well.
        self.dict.clear();
        self.frame_dict.clear();
        self.stream.reset();
        let loaded: Option<&[u8]> = match (dict, cdict) {
            (Some(d), None) => Some(d),
            (None, Some(c)) => Some(&c.content),
            _ => None,
        };
        if let Some(d) = loaded {
            self.frame_dict.extend_from_slice(d);
            // Keep only the tail the codec actually indexes, so the content and
            // the table's index space agree.
            let kept = self.stream.load_dict(&self.frame_dict, cdict.is_some());
            let drop = self.frame_dict.len() - kept;
            self.frame_dict.drain(..drop);
            self.dict.extend_from_slice(&self.frame_dict);
        }

        // --- Stage 2: write the frame header (lz4frame.c:796-826) ---
        let mut n = 0;
        write_le32(&mut dst[n..], MAGICNUMBER);
        n += 4;
        let header_start = n;

        // FLG byte. Version is '01' in the top two bits.
        dst[n] = ((1 & 0x03) << 6)
            | (((self.prefs.frame_info.block_mode & 0x01) as u8) << 5)
            | (u8::from(self.prefs.frame_info.block_checksum) << 4)
            | (u8::from(self.prefs.frame_info.content_size > 0) << 3)
            | (u8::from(self.prefs.frame_info.content_checksum) << 2)
            | u8::from(self.prefs.frame_info.dict_id > 0);
        n += 1;
        // BD byte.
        dst[n] = ((self.prefs.frame_info.block_size_id & 0x07) as u8) << 4;
        n += 1;

        if self.prefs.frame_info.content_size != 0 {
            dst[n..n + 8].copy_from_slice(&self.prefs.frame_info.content_size.to_le_bytes());
            n += 8;
            self.total_in_size = 0;
        }
        if self.prefs.frame_info.dict_id != 0 {
            write_le32(&mut dst[n..], self.prefs.frame_info.dict_id);
            n += 4;
        }
        dst[n] = header_checksum(&dst[header_start..n]);
        n += 1;

        self.stage = 1;
        Ok(n)
    }

    /// Acceleration from the level, per `LZ4F_compressBlock` (lz4frame.c:931):
    /// negative levels mean "fast acceleration".
    fn acceleration(&self) -> i32 {
        let level = self.prefs.compression_level;
        if level < 0 {
            -level + 1
        } else {
            1
        }
    }

    /// `LZ4F_makeBlock` (lz4frame.c:900) — compress one block, write its 4-byte
    /// header, and append the optional checksum. Returns bytes written.
    ///
    /// The `dstCapacity` handed to the codec is `srcSize - 1`, so "compressed"
    /// is only accepted when it comes out *strictly* smaller. A block that fails
    /// to shrink is stored verbatim with the top header bit set — which is why
    /// incompressible data still round-trips.
    fn make_block(
        &mut self,
        dst: &mut [u8],
        dst_at: usize,
        src: &[u8],
        mode: BlockCompressMode,
    ) -> usize {
        let src_size = src.len();
        let dst_capacity = if src_size > 1 { src_size - 1 } else { 1 };
        let body = dst_at + BH_SIZE;
        let accel = self.acceleration();

        let c_size = match mode {
            BlockCompressMode::Uncompressed => 0,
            BlockCompressMode::Compressed => {
                let end = (body + dst_capacity).min(dst.len());
                if body >= end {
                    0
                } else if self.linked() {
                    // `LZ4F_compressBlock_continue` (lz4frame.c:941): one
                    // continuous stream across the whole frame, so the history
                    // and the table both carry over.
                    //
                    // **Prefix**, not ext-dict. C reaches this with
                    // `dictEnd == source` — each block is followed immediately
                    // by the next in the caller's buffer, which is what
                    // `stableSrc` pledges (lz4frame.c:1091-1092) — so
                    // `LZ4_compress_fast_continue` takes its `withPrefix64k`
                    // branch (lz4.c:1755). Choosing ext-dict here instead costs
                    // a handful of bytes on every multi-block frame: it makes
                    // the history exactly one block long, where the prefix path
                    // reaches back over the whole accumulated window. Caught by
                    // a differential run against C, not by any round-trip test.
                    let dict = core::mem::take(&mut self.dict);
                    let r = block::compress_continue(
                        dst,
                        body..end,
                        &Input::Separate(src),
                        &mut self.stream,
                        &dict,
                        true,
                        None,
                        accel,
                    );
                    self.dict = dict;
                    r.unwrap_or(0)
                } else if self.frame_dict.is_empty() {
                    // `LZ4F_compressBlock` (lz4frame.c:929) with no dictionary:
                    // a plain one-shot, which resets its own table.
                    block::compress_fast(dst, body..end, &Input::Separate(src), accel).unwrap_or(0)
                } else {
                    // Independent blocks *with* a dictionary: every block starts
                    // from that same dictionary, so the stream is re-primed
                    // rather than continued.
                    let dict = core::mem::take(&mut self.frame_dict);
                    self.stream.reset();
                    self.stream.load_dict(&dict, true);
                    let r = block::compress_continue(
                        dst,
                        body..end,
                        &Input::Separate(src),
                        &mut self.stream,
                        &dict,
                        false,
                        None,
                        accel,
                    );
                    self.frame_dict = dict;
                    r.unwrap_or(0)
                }
            }
        };

        let c_size = if c_size == 0 || c_size >= src_size {
            // Stored uncompressed.
            write_le32(
                &mut dst[dst_at..],
                src_size as u32 | BLOCK_UNCOMPRESSED_FLAG,
            );
            dst[body..body + src_size].copy_from_slice(src);
            src_size
        } else {
            write_le32(&mut dst[dst_at..], c_size as u32);
            c_size
        };

        if self.prefs.frame_info.block_checksum {
            // Checksum of the block *as written*, compressed or not.
            let crc = xxh::xxh32(&dst[body..body + c_size], 0);
            write_le32(&mut dst[body + c_size..], crc);
        }

        BH_SIZE
            + c_size
            + if self.prefs.frame_info.block_checksum {
                BF_SIZE
            } else {
                0
            }
    }

    /// `LZ4F_localSaveDict` (lz4frame.c:983) — retain the last 64 KB of history
    /// so the next block can still reference it once the caller's buffer is
    /// gone. Only linked blocks have history to keep.
    fn set_history(&mut self, block: &[u8]) {
        if !self.linked() {
            return;
        }
        const MAX: usize = 64 * 1024;
        if block.len() >= MAX {
            self.dict.clear();
            self.dict.extend_from_slice(&block[block.len() - MAX..]);
        } else {
            self.dict.extend_from_slice(block);
            if self.dict.len() > MAX {
                let drop = self.dict.len() - MAX;
                self.dict.drain(..drop);
            }
        }
        // Tell the codec how much history survived. `compress_generic` grew
        // `dict_size` by the whole block; anything just dropped has to come back
        // off, or the dictionary's index space no longer lines up with
        // `self.dict` and its matches silently stop being found — a ratio-only
        // regression that round-trip tests cannot see.
        self.stream.save_dict(self.dict.len());
    }

    /// `LZ4F_compressUpdateImpl` (lz4frame.c:1008).
    ///
    /// Always consumes all of `src` on success: whatever does not fill a whole
    /// block is buffered for next time. The return is the number of bytes
    /// written to `dst`, which may legitimately be zero — that means "buffered".
    pub fn update(&mut self, dst: &mut [u8], src: &[u8], mode: BlockCompressMode) -> Res<usize> {
        let block_size = self.max_block_size;
        if self.stage != 1 {
            return Err(Error::CompressionStateUninitialized);
        }
        if dst.len() < compress_bound_internal(src.len(), Some(&self.prefs), self.tmp_in_size) {
            return Err(Error::DstMaxSizeTooSmall);
        }
        if mode == BlockCompressMode::Uncompressed && dst.len() < src.len() {
            return Err(Error::DstMaxSizeTooSmall);
        }

        let mut op = 0usize;

        // Switching block mode mid-frame flushes what is buffered first, so the
        // two kinds never share a block (lz4frame.c:1031-1036).
        if self.block_compress_mode != mode {
            op += self.flush(dst)?;
            self.block_compress_mode = mode;
        }

        let mut sp = 0usize;
        let src_end = src.len();

        // --- Complete a partially filled buffer (lz4frame.c:1040-1063) ---
        if self.tmp_in_size > 0 {
            let size_to_copy = block_size - self.tmp_in_size;
            if size_to_copy > src_end {
                // Not even enough to fill one block: buffer it and stop.
                let at = self.tmp_in_size;
                self.tmp[at..at + src_end].copy_from_slice(src);
                sp = src_end;
                self.tmp_in_size += src_end;
            } else {
                let at = self.tmp_in_size;
                self.tmp[at..at + size_to_copy].copy_from_slice(&src[..size_to_copy]);
                sp += size_to_copy;

                let block = self.tmp[..block_size].to_vec();
                op += self.make_block(dst, op, &block, mode);
                self.set_history(&block);
                self.tmp_in_size = 0;
            }
        }

        // --- Whole blocks straight from src (lz4frame.c:1065-1074) ---
        while src_end - sp >= block_size {
            let block = src[sp..sp + block_size].to_vec();
            op += self.make_block(dst, op, &block, mode);
            self.set_history(&block);
            sp += block_size;
        }

        // --- autoFlush: emit the tail as a short block (lz4frame.c:1076-1085) ---
        if self.prefs.auto_flush && sp < src_end {
            let block = src[sp..src_end].to_vec();
            op += self.make_block(dst, op, &block, mode);
            self.set_history(&block);
            sp = src_end;
        }

        // --- Buffer whatever is left, necessarily < blockSize ---
        if sp < src_end {
            let size_to_copy = src_end - sp;
            self.tmp[..size_to_copy].copy_from_slice(&src[sp..src_end]);
            self.tmp_in_size = size_to_copy;
        }

        if self.prefs.frame_info.content_checksum {
            xxh::xxh32_update(&mut self.xxh, src);
        }
        self.total_in_size += src.len() as u64;
        Ok(op)
    }

    /// `LZ4F_flush` (lz4frame.c:1175) — compress whatever is buffered, without
    /// waiting for a full block. Zero buffered bytes is success, not an error.
    pub fn flush(&mut self, dst: &mut [u8]) -> Res<usize> {
        if self.tmp_in_size == 0 {
            return Ok(0);
        }
        if self.stage != 1 {
            return Err(Error::CompressionStateUninitialized);
        }
        if dst.len() < self.tmp_in_size + BH_SIZE + BF_SIZE {
            return Err(Error::DstMaxSizeTooSmall);
        }

        let mode = self.block_compress_mode;
        let block = self.tmp[..self.tmp_in_size].to_vec();
        let n = self.make_block(dst, 0, &block, mode);
        self.set_history(&block);
        self.tmp_in_size = 0;
        Ok(n)
    }

    /// `LZ4F_compressEnd` (lz4frame.c:1224) — flush, write the endmark, and
    /// append the content checksum if one was requested.
    pub fn end(&mut self, dst: &mut [u8]) -> Res<usize> {
        let flushed = self.flush(dst)?;
        let mut n = flushed;
        let remaining = dst.len() - flushed;

        if remaining < 4 {
            return Err(Error::DstMaxSizeTooSmall);
        }
        write_le32(&mut dst[n..], 0);
        n += 4;

        if self.prefs.frame_info.content_checksum {
            if remaining < 8 {
                return Err(Error::DstMaxSizeTooSmall);
            }
            let xxh = xxh::xxh32_digest(&self.xxh);
            write_le32(&mut dst[n..], xxh);
            n += 4;
        }

        self.stage = 0;

        // A declared content size that disagrees with what was actually fed in
        // is reported *after* the frame has been written, as C does.
        if self.prefs.frame_info.content_size != 0
            && self.prefs.frame_info.content_size != self.total_in_size
        {
            return Err(Error::FrameSizeWrong);
        }
        Ok(n)
    }
}

/// `LZ4F_compressFrame_usingCDict` (lz4frame.c:430) — a whole frame in one call.
///
/// The preference fixups here are load-bearing: `autoFlush` is forced on, the
/// block size is shrunk to fit the input, and a single-block input is switched
/// to independent mode. Each one changes the bytes emitted.
pub fn compress_frame(
    cctx: &mut Cctx,
    dst: &mut [u8],
    src: &[u8],
    cdict: Option<&CDict>,
    prefs: Option<&Preferences>,
) -> Res<usize> {
    let mut p = prefs.copied().unwrap_or_default();
    if p.frame_info.content_size != 0 {
        p.frame_info.content_size = src.len() as u64;
    }
    p.frame_info.block_size_id = optimal_bsid(p.frame_info.block_size_id, src.len());
    p.auto_flush = true;
    if src.len() <= get_block_size(p.frame_info.block_size_id)? {
        // One block => nothing to link to.
        p.frame_info.block_mode = 1;
    }

    if dst.len() < compress_frame_bound(src.len(), Some(&p)) {
        return Err(Error::DstMaxSizeTooSmall);
    }

    let mut n = cctx.begin(dst, None, cdict, Some(&p))?;
    n += cctx.update(&mut dst[n..], src, BlockCompressMode::Compressed)?;
    n += cctx.end(&mut dst[n..])?;
    Ok(n)
}

// ---------------------------------------------------------------------------
// Decompression
// ---------------------------------------------------------------------------

/// `dStage_t` (lz4frame.c:1266).
///
/// `LZ4F_freeDecompressionContext` returns the raw discriminant, so the
/// *numbering* is observable to the C caller and must match C exactly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum DStage {
    GetFrameHeader = 0,
    StoreFrameHeader = 1,
    Init = 2,
    GetBlockHeader = 3,
    StoreBlockHeader = 4,
    CopyDirect = 5,
    GetBlockChecksum = 6,
    GetCBlock = 7,
    StoreCBlock = 8,
    FlushOut = 9,
    GetSuffix = 10,
    StoreSuffix = 11,
    GetSFrameSize = 12,
    StoreSFrameSize = 13,
    SkipSkippable = 14,
}

/// `LZ4F_dctx` (lz4frame.c:1278).
pub struct Dctx {
    pub version: u32,
    pub stage: DStage,
    pub frame_info: FrameInfo,
    frame_remaining_size: u64,
    max_block_size: usize,
    max_buffer_size: usize,
    /// C's `tmpIn`, sized `maxBlockSize + BFSize`: one compressed block that
    /// arrived split across calls.
    tmp_in: Vec<u8>,
    tmp_in_size: usize,
    tmp_in_target: usize,
    /// C's `tmpOutBuffer` — where a block is decoded when `dst` is too small to
    /// take a whole one. `tmp_out_start` is how much has been handed over.
    tmp_out_buffer: Vec<u8>,
    tmp_out_size: usize,
    tmp_out_start: usize,
    /// The decoder's history. C tracks it as a `(pointer, size)` that may aim at
    /// either the caller's `dst` or at `tmpOutBuffer`, and most of
    /// `LZ4F_updateDict` exists to juggle those cases. We keep an owned copy:
    /// one memcpy per block, no aliasing, and the decoded bytes are identical
    /// because only the history's *content* can affect them.
    dict: Vec<u8>,
    xxh: Xxh32State,
    block_checksum: Xxh32State,
    skip_checksum: bool,
    header: [u8; HEADER_SIZE_MAX],
}

impl Default for Dctx {
    fn default() -> Self {
        Self::new()
    }
}

impl Dctx {
    pub fn new() -> Dctx {
        Dctx {
            version: LZ4F_VERSION,
            stage: DStage::GetFrameHeader,
            frame_info: FrameInfo::default(),
            frame_remaining_size: 0,
            max_block_size: 0,
            max_buffer_size: 0,
            tmp_in: Vec::new(),
            tmp_in_size: 0,
            tmp_in_target: 0,
            tmp_out_buffer: Vec::new(),
            tmp_out_size: 0,
            tmp_out_start: 0,
            dict: Vec::new(),
            xxh: new_xxh32(),
            block_checksum: new_xxh32(),
            skip_checksum: false,
            header: [0; HEADER_SIZE_MAX],
        }
    }

    /// The heap besides the struct — C's `tmpIn`/`tmpOutBuffer` terms in
    /// `LZ4F_dctx_size` (lz4frame.c:1347-1349). See `Cctx::heap_size`.
    pub fn heap_size(&self) -> usize {
        self.tmp_in.capacity() + self.tmp_out_buffer.capacity() + self.dict.capacity()
    }

    /// `LZ4F_resetDecompressionContext` (lz4frame.c:1354).
    pub fn reset(&mut self) {
        self.stage = DStage::GetFrameHeader;
        self.dict.clear();
        self.skip_checksum = false;
        self.frame_remaining_size = 0;
    }

    /// `LZ4F_decodeHeader` (lz4frame.c:1373) — parse magic + descriptor, size
    /// the buffers, and advance the stage. Returns bytes consumed.
    ///
    /// `from_header` distinguishes the two callers, because C compares
    /// `src == dctx->header` to decide whether a partially buffered skippable
    /// frame is being re-entered rather than started.
    fn decode_header(&mut self, src: &[u8], from_header: bool) -> Res<usize> {
        if src.len() < HEADER_SIZE_MIN {
            return Err(Error::FrameHeaderIncomplete);
        }
        self.frame_info = FrameInfo::default();

        // Skippable frames (lz4frame.c:1384-1395).
        if (read_le32(src) & 0xFFFF_FFF0) == MAGIC_SKIPPABLE_START {
            self.frame_info.frame_type = 1;
            if from_header {
                self.tmp_in_size = src.len();
                self.tmp_in_target = 8;
                self.stage = DStage::StoreSFrameSize;
                return Ok(src.len());
            }
            self.stage = DStage::GetSFrameSize;
            return Ok(4);
        }

        if read_le32(src) != MAGICNUMBER {
            return Err(Error::FrameTypeUnknown);
        }
        self.frame_info.frame_type = 0;

        // FLG byte (lz4frame.c:1406-1417).
        let flg = src[4];
        let version = (flg >> 6) & 0x03;
        let block_checksum = (flg >> 4) & 1;
        let block_mode = (flg >> 5) & 1;
        let content_size_flag = (flg >> 3) & 1;
        let content_checksum = (flg >> 2) & 1;
        let dict_id_flag = flg & 1;
        if (flg >> 1) & 1 != 0 {
            return Err(Error::ReservedFlagSet);
        }
        if version != 1 {
            return Err(Error::HeaderVersionWrong);
        }

        let frame_header_size = HEADER_SIZE_MIN
            + if content_size_flag != 0 { 8 } else { 0 }
            + if dict_id_flag != 0 { 4 } else { 0 };

        if src.len() < frame_header_size {
            // Not enough to finish: stash what we have and ask for more.
            if !from_header {
                self.header[..src.len()].copy_from_slice(src);
            }
            self.tmp_in_size = src.len();
            self.tmp_in_target = frame_header_size;
            self.stage = DStage::StoreFrameHeader;
            return Ok(src.len());
        }

        // BD byte (lz4frame.c:1433-1439).
        let bd = src[5];
        let block_size_id = ((bd >> 4) & 0x07) as i32;
        if (bd >> 7) & 1 != 0 {
            return Err(Error::ReservedFlagSet);
        }
        if block_size_id < 4 {
            return Err(Error::MaxBlockSizeInvalid);
        }
        if bd & 0x0F != 0 {
            return Err(Error::ReservedFlagSet);
        }

        // The header checksum covers the descriptor: from byte 4 up to, but not
        // including, the checksum byte itself.
        let hc = header_checksum(&src[4..frame_header_size - 1]);
        if hc != src[frame_header_size - 1] {
            return Err(Error::HeaderChecksumInvalid);
        }

        self.frame_info.block_mode = block_mode as i32;
        self.frame_info.block_checksum = block_checksum != 0;
        self.frame_info.content_checksum = content_checksum != 0;
        self.frame_info.block_size_id = block_size_id;
        self.max_block_size = get_block_size(block_size_id)?;
        if content_size_flag != 0 {
            self.frame_info.content_size = read_le64(&src[6..]);
            self.frame_remaining_size = self.frame_info.content_size;
        }
        if dict_id_flag != 0 {
            self.frame_info.dict_id = read_le32(&src[frame_header_size - 5..]);
        }

        self.stage = DStage::Init;
        Ok(frame_header_size)
    }
}

/// `LZ4F_headerSize` (lz4frame.c:1471).
pub fn header_size(src: &[u8]) -> Res<usize> {
    if src.len() < MIN_SIZE_TO_KNOW_HEADER_LENGTH {
        return Err(Error::FrameHeaderIncomplete);
    }
    if (read_le32(src) & 0xFFFF_FFF0) == MAGIC_SKIPPABLE_START {
        return Ok(8);
    }
    if read_le32(src) != MAGICNUMBER {
        return Err(Error::FrameTypeUnknown);
    }
    let flg = src[4];
    let content_size_flag = (flg >> 3) & 1;
    let dict_id_flag = flg & 1;
    Ok(HEADER_SIZE_MIN
        + if content_size_flag != 0 { 8 } else { 0 }
        + if dict_id_flag != 0 { 4 } else { 0 })
}

/// What a decompress call reports back.
pub struct Progress {
    /// Bytes consumed from `src`.
    pub src_consumed: usize,
    /// Bytes written to `dst`.
    pub dst_written: usize,
    /// The hint: how many source bytes the next call would like. `0` means the
    /// frame is complete — it is *not* an error.
    pub hint: usize,
}

/// `LZ4F_getFrameInfo` (lz4frame.c:1512).
///
/// Returns the frame parameters, how much of `src` was consumed, and the hint.
/// It only consumes anything when it is the call that decodes the header.
pub fn get_frame_info(dctx: &mut Dctx, src: &[u8]) -> Res<(FrameInfo, usize, usize)> {
    if dctx.stage as u32 > DStage::StoreFrameHeader as u32 {
        // Already decoded: report from the context, consuming nothing, and
        // return the hint a zero-length decompress call would give.
        let p = decompress(dctx, &mut [], &[], None, false)?;
        return Ok((dctx.frame_info, 0, p.hint));
    }
    if dctx.stage == DStage::StoreFrameHeader {
        // Mid-header: C cannot restart from here and fails outright.
        return Err(Error::FrameDecodingAlreadyStarted);
    }
    let h_size = header_size(src)?;
    if src.len() < h_size {
        return Err(Error::FrameHeaderIncomplete);
    }
    let consumed = dctx.decode_header(&src[..h_size], false)?;
    Ok((dctx.frame_info, consumed, BH_SIZE))
}

/// `LZ4F_decompress` (lz4frame.c:1644) — the streaming decoder, as a state
/// machine over `DStage`.
///
/// Neither buffer has to be complete: any stage may run out of input or output
/// and stop, leaving enough in the context to resume from the same point on the
/// next call. That resumability is the entire reason the `Store*` stages exist,
/// and it is what `frametest`'s random chunk sizes exercise.
///
/// `dict` is the external dictionary for `LZ4F_decompress_usingDict`.
pub fn decompress(
    dctx: &mut Dctx,
    dst: &mut [u8],
    src: &[u8],
    dict: Option<&[u8]>,
    skip_checksums: bool,
) -> Res<Progress> {
    let mut sp = 0usize;
    let mut op = 0usize;
    let src_end = src.len();
    let dst_end = dst.len();
    let mut hint = 1usize;
    let mut another = true;
    dctx.skip_checksum |= skip_checksums;

    if let Some(d) = dict {
        // `LZ4F_decompress_usingDict` seeds the history before the frame's first
        // block (lz4frame.c:2160-2163).
        if dctx.stage as u32 <= DStage::Init as u32 {
            dctx.dict.clear();
            dctx.dict.extend_from_slice(d);
        }
    }

    while another {
        match dctx.stage {
            DStage::GetFrameHeader => {
                if src_end - sp >= HEADER_SIZE_MAX {
                    // Enough for any header: decode in place.
                    let n = dctx.decode_header(&src[sp..], false)?;
                    sp += n;
                    continue;
                }
                dctx.tmp_in_size = 0;
                if src_end - sp == 0 {
                    // A zero-length call: this is how `getFrameInfo` asks for a
                    // hint without offering data.
                    return Ok(Progress {
                        src_consumed: 0,
                        dst_written: 0,
                        hint: HEADER_SIZE_MIN,
                    });
                }
                dctx.tmp_in_target = HEADER_SIZE_MIN;
                dctx.stage = DStage::StoreFrameHeader;
            }

            DStage::StoreFrameHeader => {
                let size_to_copy = (dctx.tmp_in_target - dctx.tmp_in_size).min(src_end - sp);
                let at = dctx.tmp_in_size;
                dctx.header[at..at + size_to_copy].copy_from_slice(&src[sp..sp + size_to_copy]);
                dctx.tmp_in_size += size_to_copy;
                sp += size_to_copy;
                if dctx.tmp_in_size < dctx.tmp_in_target {
                    // Rest of the header, plus the block header behind it.
                    hint = (dctx.tmp_in_target - dctx.tmp_in_size) + BH_SIZE;
                    another = false;
                    continue;
                }
                let target = dctx.tmp_in_target;
                let hdr = dctx.header;
                dctx.decode_header(&hdr[..target], true)?;
            }

            DStage::Init => {
                if dctx.frame_info.content_checksum {
                    xxh::xxh32_reset(&mut dctx.xxh, 0);
                }
                let buffer_needed = dctx.max_block_size
                    + if dctx.frame_info.block_mode == 0 {
                        128 * 1024
                    } else {
                        0
                    };
                if buffer_needed > dctx.max_buffer_size {
                    dctx.max_buffer_size = 0;
                    dctx.tmp_in.clear();
                    dctx.tmp_in.resize(dctx.max_block_size + BF_SIZE, 0);
                    dctx.tmp_out_buffer.clear();
                    dctx.tmp_out_buffer.resize(buffer_needed, 0);
                    dctx.max_buffer_size = buffer_needed;
                }
                dctx.tmp_in_size = 0;
                dctx.tmp_in_target = 0;
                dctx.tmp_out_start = 0;
                dctx.tmp_out_size = 0;
                dctx.stage = DStage::GetBlockHeader;
            }

            DStage::GetBlockHeader => {
                if src_end - sp >= BH_SIZE {
                    let hdr = read_le32(&src[sp..]);
                    sp += BH_SIZE;
                    decode_block_header(
                        dctx,
                        hdr,
                        op,
                        dst_end,
                        sp,
                        src_end,
                        &mut hint,
                        &mut another,
                    )?;
                } else {
                    dctx.tmp_in_size = 0;
                    dctx.stage = DStage::StoreBlockHeader;
                }
            }

            DStage::StoreBlockHeader => {
                let wanted = BH_SIZE - dctx.tmp_in_size;
                let size_to_copy = wanted.min(src_end - sp);
                let at = dctx.tmp_in_size;
                dctx.tmp_in[at..at + size_to_copy].copy_from_slice(&src[sp..sp + size_to_copy]);
                sp += size_to_copy;
                dctx.tmp_in_size += size_to_copy;
                if dctx.tmp_in_size < BH_SIZE {
                    hint = BH_SIZE - dctx.tmp_in_size;
                    another = false;
                    continue;
                }
                let hdr = read_le32(&dctx.tmp_in[..BH_SIZE]);
                decode_block_header(dctx, hdr, op, dst_end, sp, src_end, &mut hint, &mut another)?;
            }

            DStage::CopyDirect => {
                // An uncompressed block: copy straight through, checksumming as
                // we go, and stop when either buffer runs out.
                let min_buff = (src_end - sp).min(dst_end - op);
                let size_to_copy = dctx.tmp_in_target.min(min_buff);
                dst[op..op + size_to_copy].copy_from_slice(&src[sp..sp + size_to_copy]);
                if !dctx.skip_checksum {
                    if dctx.frame_info.block_checksum {
                        xxh::xxh32_update(&mut dctx.block_checksum, &src[sp..sp + size_to_copy]);
                    }
                    if dctx.frame_info.content_checksum {
                        xxh::xxh32_update(&mut dctx.xxh, &src[sp..sp + size_to_copy]);
                    }
                }
                if dctx.frame_info.content_size != 0 {
                    dctx.frame_remaining_size =
                        dctx.frame_remaining_size.wrapping_sub(size_to_copy as u64);
                }
                if dctx.frame_info.block_mode == 0 {
                    let block = dst[op..op + size_to_copy].to_vec();
                    update_dict(dctx, &block);
                }
                sp += size_to_copy;
                op += size_to_copy;

                if size_to_copy == dctx.tmp_in_target {
                    if dctx.frame_info.block_checksum {
                        dctx.tmp_in_size = 0;
                        dctx.stage = DStage::GetBlockChecksum;
                    } else {
                        dctx.stage = DStage::GetBlockHeader;
                    }
                    continue;
                }
                // Only part of the block moved; the rest waits for more room.
                dctx.tmp_in_target -= size_to_copy;
                hint = dctx.tmp_in_target
                    + if dctx.frame_info.block_checksum {
                        BF_SIZE
                    } else {
                        0
                    }
                    + BH_SIZE;
                another = false;
            }

            DStage::GetBlockChecksum => {
                let crc_bytes: [u8; 4];
                if src_end - sp >= 4 && dctx.tmp_in_size == 0 {
                    crc_bytes = src[sp..sp + 4].try_into().unwrap();
                    sp += 4;
                } else {
                    let still = 4 - dctx.tmp_in_size;
                    let size_to_copy = still.min(src_end - sp);
                    let at = dctx.tmp_in_size;
                    dctx.header[at..at + size_to_copy].copy_from_slice(&src[sp..sp + size_to_copy]);
                    dctx.tmp_in_size += size_to_copy;
                    sp += size_to_copy;
                    if dctx.tmp_in_size < 4 {
                        another = false;
                        continue;
                    }
                    crc_bytes = dctx.header[..4].try_into().unwrap();
                }
                if !dctx.skip_checksum {
                    let read_crc = u32::from_le_bytes(crc_bytes);
                    let calc_crc = xxh::xxh32_digest(&dctx.block_checksum);
                    if read_crc != calc_crc {
                        return Err(Error::BlockChecksumInvalid);
                    }
                }
                dctx.stage = DStage::GetBlockHeader;
            }

            DStage::GetCBlock => {
                if src_end - sp < dctx.tmp_in_target {
                    dctx.tmp_in_size = 0;
                    dctx.stage = DStage::StoreCBlock;
                    continue;
                }
                let block = src[sp..sp + dctx.tmp_in_target].to_vec();
                sp += dctx.tmp_in_target;
                decode_cblock(dctx, dst, &block, &mut op)?;
            }

            DStage::StoreCBlock => {
                let wanted = dctx.tmp_in_target - dctx.tmp_in_size;
                let size_to_copy = wanted.min(src_end - sp);
                let at = dctx.tmp_in_size;
                dctx.tmp_in[at..at + size_to_copy].copy_from_slice(&src[sp..sp + size_to_copy]);
                dctx.tmp_in_size += size_to_copy;
                sp += size_to_copy;
                if dctx.tmp_in_size < dctx.tmp_in_target {
                    hint = (dctx.tmp_in_target - dctx.tmp_in_size)
                        + if dctx.frame_info.block_checksum {
                            BF_SIZE
                        } else {
                            0
                        }
                        + BH_SIZE;
                    another = false;
                    continue;
                }
                let block = dctx.tmp_in[..dctx.tmp_in_target].to_vec();
                decode_cblock(dctx, dst, &block, &mut op)?;
            }

            DStage::FlushOut => {
                let size_to_copy = (dctx.tmp_out_size - dctx.tmp_out_start).min(dst_end - op);
                let from = dctx.tmp_out_start;
                dst[op..op + size_to_copy]
                    .copy_from_slice(&dctx.tmp_out_buffer[from..from + size_to_copy]);
                if dctx.frame_info.block_mode == 0 {
                    let block = dctx.tmp_out_buffer[from..from + size_to_copy].to_vec();
                    update_dict(dctx, &block);
                }
                dctx.tmp_out_start += size_to_copy;
                op += size_to_copy;

                if dctx.tmp_out_start == dctx.tmp_out_size {
                    dctx.stage = DStage::GetBlockHeader;
                    continue;
                }
                // Output full: the caller must come back for the rest.
                another = false;
                hint = BH_SIZE;
            }

            DStage::GetSuffix => {
                if dctx.frame_remaining_size != 0 {
                    return Err(Error::FrameSizeWrong);
                }
                if !dctx.frame_info.content_checksum {
                    // No checksum: the frame ends here.
                    hint = 0;
                    dctx.reset();
                    another = false;
                    continue;
                }
                if src_end - sp < 4 {
                    dctx.tmp_in_size = 0;
                    dctx.stage = DStage::StoreSuffix;
                    continue;
                }
                let read_crc = read_le32(&src[sp..]);
                sp += 4;
                check_content_crc(dctx, read_crc)?;
                hint = 0;
                dctx.reset();
                another = false;
            }

            DStage::StoreSuffix => {
                let wanted = 4 - dctx.tmp_in_size;
                let size_to_copy = wanted.min(src_end - sp);
                let at = dctx.tmp_in_size;
                dctx.tmp_in[at..at + size_to_copy].copy_from_slice(&src[sp..sp + size_to_copy]);
                sp += size_to_copy;
                dctx.tmp_in_size += size_to_copy;
                if dctx.tmp_in_size < 4 {
                    hint = 4 - dctx.tmp_in_size;
                    another = false;
                    continue;
                }
                let read_crc = read_le32(&dctx.tmp_in[..4]);
                check_content_crc(dctx, read_crc)?;
                hint = 0;
                dctx.reset();
                another = false;
            }

            DStage::GetSFrameSize => {
                if src_end - sp >= 4 {
                    let s_frame_size = read_le32(&src[sp..]);
                    sp += 4;
                    dctx.frame_info.content_size = s_frame_size as u64;
                    dctx.tmp_in_target = s_frame_size as usize;
                    dctx.stage = DStage::SkipSkippable;
                    continue;
                }
                // C has already consumed the 4 magic bytes into `header`, hence
                // a start of 4 and a target of 8 (lz4frame.c:2069-2070).
                dctx.tmp_in_size = 4;
                dctx.tmp_in_target = 8;
                dctx.stage = DStage::StoreSFrameSize;
            }

            DStage::StoreSFrameSize => {
                let size_to_copy = (dctx.tmp_in_target - dctx.tmp_in_size).min(src_end - sp);
                let at = dctx.tmp_in_size;
                dctx.header[at..at + size_to_copy].copy_from_slice(&src[sp..sp + size_to_copy]);
                sp += size_to_copy;
                dctx.tmp_in_size += size_to_copy;
                if dctx.tmp_in_size < dctx.tmp_in_target {
                    hint = dctx.tmp_in_target - dctx.tmp_in_size;
                    another = false;
                    continue;
                }
                let s_frame_size = read_le32(&dctx.header[4..]);
                dctx.frame_info.content_size = s_frame_size as u64;
                dctx.tmp_in_target = s_frame_size as usize;
                dctx.stage = DStage::SkipSkippable;
            }

            DStage::SkipSkippable => {
                let skip_size = dctx.tmp_in_target.min(src_end - sp);
                sp += skip_size;
                dctx.tmp_in_target -= skip_size;
                another = false;
                hint = dctx.tmp_in_target;
                if hint == 0 {
                    // Fully skipped: ready for whatever frame follows.
                    dctx.reset();
                }
            }
        }
    }

    Ok(Progress {
        src_consumed: sp,
        dst_written: op,
        hint,
    })
}

/// The block-header decode shared by the direct and buffered entries
/// (lz4frame.c:1759-1789).
#[allow(clippy::too_many_arguments)]
fn decode_block_header(
    dctx: &mut Dctx,
    block_header: u32,
    op: usize,
    dst_end: usize,
    sp: usize,
    src_end: usize,
    hint: &mut usize,
    another: &mut bool,
) -> Res<()> {
    let next_c_block_size = (block_header & 0x7FFF_FFFF) as usize;
    let crc_size = if dctx.frame_info.block_checksum {
        BF_SIZE
    } else {
        0
    };

    if block_header == 0 {
        // The endmark: this frame has no more blocks.
        dctx.stage = DStage::GetSuffix;
        return Ok(());
    }
    if next_c_block_size > dctx.max_block_size {
        return Err(Error::MaxBlockSizeInvalid);
    }
    if block_header & BLOCK_UNCOMPRESSED_FLAG != 0 {
        dctx.tmp_in_target = next_c_block_size;
        if dctx.frame_info.block_checksum {
            xxh::xxh32_reset(&mut dctx.block_checksum, 0);
        }
        dctx.stage = DStage::CopyDirect;
        return Ok(());
    }
    dctx.tmp_in_target = next_c_block_size + crc_size;
    dctx.stage = DStage::GetCBlock;
    // Nothing can be done with this block without room to put it or bytes to
    // read, so stop and tell the caller exactly how much input to bring.
    if op == dst_end || sp == src_end {
        *hint = BH_SIZE + next_c_block_size + crc_size;
        *another = false;
    }
    Ok(())
}

/// Decode one compressed block: verify its checksum, then decompress it either
/// straight into `dst` or into `tmpOut` when `dst` cannot take a whole block
/// (lz4frame.c:1899-1989).
fn decode_cblock(dctx: &mut Dctx, dst: &mut [u8], block: &[u8], op: &mut usize) -> Res<()> {
    // The block checksum covers the compressed bytes, so it is stripped from
    // the length before decoding (lz4frame.c:1902-1914).
    let mut body_len = dctx.tmp_in_target;
    if dctx.frame_info.block_checksum {
        if body_len < 4 {
            return Err(Error::BlockChecksumInvalid);
        }
        body_len -= 4;
        dctx.tmp_in_target = body_len;
        if !dctx.skip_checksum {
            let read_crc = read_le32(&block[body_len..]);
            let calc_crc = xxh::xxh32(&block[..body_len], 0);
            if read_crc != calc_crc {
                return Err(Error::BlockChecksumInvalid);
            }
        }
    }
    let cblock = &block[..body_len];

    let dst_room = dst.len() - *op;
    if dst_room >= dctx.max_block_size {
        // Enough room to decode straight into the caller's buffer.
        let decoded = block::decompress_dict(
            dst,
            *op..*op + dctx.max_block_size,
            &Input::Separate(cblock),
            false,
            0,
            &dctx.dict,
            0,
        )
        .map_err(|_| Error::DecompressionFailed)?;

        if dctx.frame_info.content_checksum && !dctx.skip_checksum {
            let bytes = dst[*op..*op + decoded].to_vec();
            xxh::xxh32_update(&mut dctx.xxh, &bytes);
        }
        if dctx.frame_info.content_size != 0 {
            dctx.frame_remaining_size = dctx.frame_remaining_size.wrapping_sub(decoded as u64);
        }
        if dctx.frame_info.block_mode == 0 {
            let bytes = dst[*op..*op + decoded].to_vec();
            update_dict(dctx, &bytes);
        }
        *op += decoded;
        dctx.stage = DStage::GetBlockHeader;
        return Ok(());
    }

    // Not enough room: decode into tmpOut and hand it over in pieces.
    let mut scratch = core::mem::take(&mut dctx.tmp_out_buffer);
    let cap = dctx.max_block_size.min(scratch.len());
    let decoded = block::decompress_dict(
        &mut scratch,
        0..cap,
        &Input::Separate(cblock),
        false,
        0,
        &dctx.dict,
        0,
    );
    dctx.tmp_out_buffer = scratch;
    let decoded = decoded.map_err(|_| Error::DecompressionFailed)?;

    if dctx.frame_info.content_checksum && !dctx.skip_checksum {
        let bytes = dctx.tmp_out_buffer[..decoded].to_vec();
        xxh::xxh32_update(&mut dctx.xxh, &bytes);
    }
    if dctx.frame_info.content_size != 0 {
        dctx.frame_remaining_size = dctx.frame_remaining_size.wrapping_sub(decoded as u64);
    }
    dctx.tmp_out_size = decoded;
    dctx.tmp_out_start = 0;
    dctx.stage = DStage::FlushOut;
    Ok(())
}

/// `LZ4F_updateDict` (lz4frame.c:1558), reduced to what it means rather than how
/// C achieves it.
///
/// C's version is a five-branch juggle over whether the history currently lives
/// in the caller's `dst` or in `tmpOutBuffer`, and whether the two happen to be
/// adjacent — all of it to avoid a copy. Owning the history collapses every
/// branch into "keep the last 64 KB", and the decoded bytes are the same because
/// only the content can affect them.
fn update_dict(dctx: &mut Dctx, just_written: &[u8]) {
    const MAX: usize = 64 * 1024;
    if just_written.len() >= MAX {
        dctx.dict.clear();
        dctx.dict
            .extend_from_slice(&just_written[just_written.len() - MAX..]);
        return;
    }
    dctx.dict.extend_from_slice(just_written);
    if dctx.dict.len() > MAX {
        let drop = dctx.dict.len() - MAX;
        dctx.dict.drain(..drop);
    }
}

fn check_content_crc(dctx: &Dctx, read_crc: u32) -> Res<()> {
    if dctx.skip_checksum {
        return Ok(());
    }
    let result_crc = xxh::xxh32_digest(&dctx.xxh);
    if read_crc != result_crc {
        return Err(Error::ContentChecksumInvalid);
    }
    Ok(())
}
