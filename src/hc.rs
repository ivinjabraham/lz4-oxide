//! hc: safe-Rust port of `lz4hc.c`. Entry points live in `crate::ffi`.
//!
//! Nothing here may use `unsafe`; pointer handling stays in the FFI shim so
//! the port's unsafe surface is small and countable.
//!
//! # Scope
//!
//! This module implements the **`lz4mid` strategy** (`lz4hc.c:553-806`) for
//! levels 1-2, the greedy **hash-chain parser** for levels 3-9, and the
//! **optimal parser** for levels 10-12. All three use the same state layout as
//! C, and the latter two share C's hash-chain matcher.
//!
//! # How the C memory model maps onto slices
//!
//! `LZ4HC_CCtx_internal` describes positions in two ways at once, and getting
//! this wrong is the trap in this file:
//!
//! * **Pointers** — `prefixStart .. end` is one *contiguous* buffer holding the
//!   history *followed by* the block being compressed. `LZ4_count` walks
//!   straight across that seam, and the catch-back loop reads backwards through
//!   it. So the port takes a single [`SrcView::base`] slice plus the offset of
//!   the current block within it, never two separate slices.
//! * **Indices** — `dictLimit` is the absolute index of `prefixStart`, so
//!   `ipIndex = (ip - prefixStart) + dictLimit`. Because `ip` is a plain offset
//!   into `base` here, that is simply `ip + prefix_idx`. Indices keep rising
//!   across blocks, which is how stale hash-table entries are rejected.
//!
//! The external dictionary (`dictStart`, covering indices
//! `[lowLimit, dictLimit)`) is a genuinely separate buffer, and is passed as
//! its own slice.
#![forbid(unsafe_code)]

// --- Constants (lz4hc.h, lz4.h) --------------------------------------------

const MINMATCH: usize = 4;
const LASTLITERALS: usize = 5;
const MFLIMIT: usize = 12;
const ML_BITS: usize = 4;
const ML_MASK: usize = (1 << ML_BITS) - 1; // 15
const RUN_MASK: usize = (1 << (8 - ML_BITS)) - 1; // 15
const LZ4_DISTANCE_MAX: u32 = 65535;
const LZ4_MAX_INPUT_SIZE: u32 = 0x7E00_0000;

/// `LZ4_minLength` (lz4.c:250) — below this, everything is literals.
const LZ4_MIN_LENGTH: usize = MFLIMIT + 1;

pub const LZ4HC_CLEVEL_MIN: i32 = 2;
pub const LZ4HC_CLEVEL_DEFAULT: i32 = 9;
pub const LZ4HC_CLEVEL_OPT_MIN: i32 = 10;
pub const LZ4HC_CLEVEL_MAX: i32 = 12;

const LZ4HC_HASH_LOG: u32 = 15;
const LZ4HC_HASHTABLESIZE: usize = 1 << LZ4HC_HASH_LOG; // 32768
const LZ4HC_MAXD: usize = 1 << 16; // 65536
const LZ4_OPT_NUM: usize = 1 << 12;
const TRAILING_LITERALS: usize = 3;
const OPTIMAL_ML: usize = (ML_MASK - 1) + MINMATCH;

const LZ4MID_HASHSIZE: usize = 8;
const LZ4MID_HASHLOG: u32 = LZ4HC_HASH_LOG - 1; // 14
pub const LZ4MID_HASHTABLESIZE: usize = 1 << LZ4MID_HASHLOG; // 16384

/// Starting index offset assigned by `LZ4HC_init_internal` (lz4hc.c:253).
pub const START_OFFSET: u32 = 64 * 1024;

/// `LZ4HC_getCLevelParams` clamping (lz4hc.c:108-117).
pub fn clamp_level(level: i32) -> i32 {
    if level < 1 {
        LZ4HC_CLEVEL_DEFAULT
    } else {
        level.min(LZ4HC_CLEVEL_MAX)
    }
}

/// Whether level `level` selects the `lz4mid` strategy in C (`k_clTable`,
/// lz4hc.c:420-436). Used by `isStateCompatible` (lz4hc.c:1479).
pub fn is_mid_level(level: i32) -> bool {
    clamp_level(level) <= 2
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Strategy {
    Mid,
    HashChain,
    Optimal,
}

#[derive(Clone, Copy)]
struct CompressionParams {
    strategy: Strategy,
    searches: usize,
    target_length: usize,
}

fn compression_params(level: i32) -> CompressionParams {
    const PARAMS: [CompressionParams; 13] = [
        CompressionParams {
            strategy: Strategy::Mid,
            searches: 2,
            target_length: 16,
        },
        CompressionParams {
            strategy: Strategy::Mid,
            searches: 2,
            target_length: 16,
        },
        CompressionParams {
            strategy: Strategy::Mid,
            searches: 2,
            target_length: 16,
        },
        CompressionParams {
            strategy: Strategy::HashChain,
            searches: 4,
            target_length: 16,
        },
        CompressionParams {
            strategy: Strategy::HashChain,
            searches: 8,
            target_length: 16,
        },
        CompressionParams {
            strategy: Strategy::HashChain,
            searches: 16,
            target_length: 16,
        },
        CompressionParams {
            strategy: Strategy::HashChain,
            searches: 32,
            target_length: 16,
        },
        CompressionParams {
            strategy: Strategy::HashChain,
            searches: 64,
            target_length: 16,
        },
        CompressionParams {
            strategy: Strategy::HashChain,
            searches: 128,
            target_length: 16,
        },
        CompressionParams {
            strategy: Strategy::HashChain,
            searches: 256,
            target_length: 16,
        },
        CompressionParams {
            strategy: Strategy::Optimal,
            searches: 96,
            target_length: 64,
        },
        CompressionParams {
            strategy: Strategy::Optimal,
            searches: 512,
            target_length: 128,
        },
        CompressionParams {
            strategy: Strategy::Optimal,
            searches: 16384,
            target_length: LZ4_OPT_NUM,
        },
    ];
    PARAMS[clamp_level(level) as usize]
}

// --- Limit directive (matches limitedOutput_directive in lz4.c) ------------

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Limit {
    NotLimited,
    LimitedOutput,
    FillOutput,
}

impl Limit {
    /// C spells this `if (limit && ...)`, relying on `notLimited == 0`.
    #[inline]
    fn is_limited(self) -> bool {
        self != Limit::NotLimited
    }
}

// --- ABI state (mirrors LZ4HC_CCtx_internal exactly) -----------------------
//
// C offsets verified with offsetof() on the real headers (lz4hc.h:242-257):
//   hashTable   : 0
//   chainTable  : 131072
//   end         : 262144
//   prefixStart : 262152
//   dictStart   : 262160
//   dictLimit   : 262168
//   lowLimit    : 262172
//   nextToUpdate: 262176
//   compressionLevel: 262180
//   favorDecSpeed: 262182
//   dirty       : 262183
//   dictCtx     : 262184
//   sizeof(LZ4HC_CCtx_internal) = 262192
//   sizeof(LZ4_streamHC_t)      = 262200  (8 bytes of union padding)
//
// The three pointer fields are held as `usize` rather than raw pointers: this
// module is `forbid(unsafe_code)`, so it may only ever compare and offset them.
// `ffi` is what turns them back into slices.

#[repr(C)]
pub struct HcState {
    pub hash_table: [u32; LZ4HC_HASHTABLESIZE], // 131072 bytes
    pub chain_table: [u16; LZ4HC_MAXD],         // 131072 bytes
    pub end: usize,
    pub prefix_start: usize,
    pub dict_start: usize,
    pub dict_limit: u32,
    pub low_limit: u32,
    pub next_to_update: u32,
    pub compression_level: i16,
    pub favor_dec_speed: i8,
    pub dirty: i8,
    pub dict_ctx: usize,
}

// Layout assertions (LZ4_STREAMHC_SIZE = 262200, LZ4_STREAMHC_ALIGN = 8)
const _: () = {
    assert!(core::mem::size_of::<HcState>() + 8 == crate::types::LZ4_STREAMHC_SIZE);
    assert!(core::mem::align_of::<HcState>() == crate::types::LZ4_STREAMHC_ALIGN);
};

impl HcState {
    /// `LZ4HC_clearTables` (lz4hc.c:237).
    pub fn clear_tables(&mut self) {
        self.hash_table.fill(0);
        // C: MEM_INIT(chainTable, 0xFF, ...) — every *byte* becomes 0xFF.
        self.chain_table.fill(0xFFFF);
    }

    /// `LZ4HC_init_internal` (lz4hc.c:242). `start` is the address of the first
    /// byte of the buffer that will become the prefix.
    pub fn init_internal(&mut self, start: usize) {
        let buffer_size = self.end.wrapping_sub(self.prefix_start);
        let mut new_starting_offset = buffer_size.wrapping_add(self.dict_limit as usize);
        if new_starting_offset > 1024 * 1024 * 1024 {
            self.clear_tables();
            new_starting_offset = 0;
        }
        new_starting_offset += START_OFFSET as usize;
        self.next_to_update = new_starting_offset as u32;
        self.prefix_start = start;
        self.end = start;
        self.dict_start = start;
        self.dict_limit = new_starting_offset as u32;
        self.low_limit = new_starting_offset as u32;
    }

    /// `LZ4_initStreamHC` (lz4hc.c:1622): zero everything, then default level.
    pub fn init_stream(&mut self) {
        self.hash_table.fill(0);
        self.chain_table.fill(0);
        self.end = 0;
        self.prefix_start = 0;
        self.dict_start = 0;
        self.dict_limit = 0;
        self.low_limit = 0;
        self.next_to_update = 0;
        self.compression_level = 0;
        self.favor_dec_speed = 0;
        self.dirty = 0;
        self.dict_ctx = 0;
        self.set_level(LZ4HC_CLEVEL_DEFAULT);
    }

    /// `LZ4_setCompressionLevel` (lz4hc.c:1660).
    pub fn set_level(&mut self, level: i32) {
        self.compression_level = clamp_level(level) as i16;
    }

    /// `LZ4_favorDecompressionSpeed` (lz4hc.c:1668).
    pub fn set_favor_dec_speed(&mut self, favor: bool) {
        self.favor_dec_speed = favor as i8;
    }

    /// `LZ4_resetStreamHC` (lz4hc.c:1638).
    pub fn reset_stream(&mut self, level: i32) {
        self.init_stream();
        self.set_level(level);
    }

    /// `LZ4_resetStreamHC_fast` (lz4hc.c:1644). Note the index bookkeeping: the
    /// tables are *not* cleared, so `dictLimit` must absorb the retired prefix
    /// to keep old entries out of range.
    pub fn reset_stream_fast(&mut self, level: i32) {
        if self.dirty != 0 {
            self.init_stream();
        } else {
            // C asserts end >= prefixStart.
            self.dict_limit = self
                .dict_limit
                .wrapping_add(self.end.wrapping_sub(self.prefix_start) as u32);
            self.prefix_start = 0; // NULL
            self.end = 0; // NULL
            self.dict_ctx = 0; // NULL
        }
        self.set_level(level);
    }

    /// `LZ4HC_setExternalDict` (lz4hc.c:1709), minus the `LZ4HC_Insert` call
    /// that C makes only for the hash-chain strategies — see the module note on
    /// scope.
    pub fn set_external_dict(&mut self, new_block: usize) {
        // Only one memory segment for extDict, so any previous one is lost here.
        self.low_limit = self.dict_limit;
        self.dict_start = self.prefix_start;
        self.dict_limit = self
            .dict_limit
            .wrapping_add(self.end.wrapping_sub(self.prefix_start) as u32);
        self.prefix_start = new_block;
        self.end = new_block;
        self.next_to_update = self.dict_limit;
        self.dict_ctx = 0; // cannot reference an extDict and a dictCtx at once
    }

    /// `LZ4_memcpy(ctx, ctx->dictCtx, sizeof(LZ4HC_CCtx_internal))`
    /// (lz4hc.c:1505) — the whole context, tables included.
    pub fn copy_from_dict_ctx(&mut self, d: &MidDictCtx) {
        self.hash_table.copy_from_slice(d.hash_table);
        self.chain_table.copy_from_slice(d.chain_table);
        self.end = d.end;
        self.prefix_start = d.prefix_start;
        self.dict_start = d.dict_start;
        self.dict_limit = d.dict_limit;
        self.low_limit = d.low_limit;
        self.next_to_update = d.next_to_update;
        self.compression_level = d.compression_level;
        self.favor_dec_speed = d.favor_dec_speed;
        self.dirty = d.dirty;
        self.dict_ctx = d.dict_ctx;
    }
}

/// A borrowed view of the *dictionary* context attached by
/// `LZ4_attach_HC_dictionary`. `ffi` builds this by reading the pointer fields
/// out of the other `LZ4_streamHC_t`.
pub struct MidDictCtx<'a> {
    pub hash_table: &'a [u32; LZ4HC_HASHTABLESIZE],
    pub chain_table: &'a [u16; LZ4HC_MAXD],
    pub end: usize,
    pub prefix_start: usize,
    pub dict_start: usize,
    pub dict_limit: u32,
    pub low_limit: u32,
    pub next_to_update: u32,
    pub compression_level: i16,
    pub favor_dec_speed: i8,
    pub dirty: i8,
    pub dict_ctx: usize,
    /// The dictionary's own prefix bytes, i.e. `[prefixStart, end)`.
    pub prefix: &'a [u8],
}

impl MidDictCtx<'_> {
    /// `lDictEndIndex` (lz4hc.c:449).
    fn l_dict_end_index(&self) -> usize {
        (self.end - self.prefix_start) + self.dict_limit as usize
    }
}

// --- Reads and hashes -------------------------------------------------------

#[inline(always)]
pub fn read_u32_le(src: &[u8], pos: usize) -> u32 {
    u32::from_le_bytes(src[pos..pos + 4].try_into().unwrap())
}

#[inline(always)]
pub fn read_u64_le(src: &[u8], pos: usize) -> u64 {
    u64::from_le_bytes(src[pos..pos + 8].try_into().unwrap())
}

/// `LZ4MID_hash4` (lz4hc.c:145).
#[inline(always)]
pub fn mid_hash4(v: u32) -> usize {
    (v.wrapping_mul(2654435761) >> (32 - LZ4MID_HASHLOG)) as usize
}

/// `LZ4MID_hash7` (lz4hc.c:149) — hashes the low 56 bits of a little-endian
/// 64-bit read.
#[inline(always)]
pub fn mid_hash7(v: u64) -> usize {
    ((v << (64 - 56)).wrapping_mul(58295818150454627) >> (64 - LZ4MID_HASHLOG)) as usize
}

#[inline(always)]
fn mid_hash4_at(buf: &[u8], pos: usize) -> usize {
    mid_hash4(read_u32_le(buf, pos))
}

#[inline(always)]
fn mid_hash8_at(buf: &[u8], pos: usize) -> usize {
    mid_hash7(read_u64_le(buf, pos))
}

#[inline(always)]
fn hc_hash_at(buf: &[u8], pos: usize) -> usize {
    let value = u32::from_ne_bytes(buf[pos..pos + MINMATCH].try_into().unwrap());
    (value.wrapping_mul(2654435761) >> (32 - LZ4HC_HASH_LOG)) as usize
}

fn insert_chain(
    hash_table: &mut [u32; LZ4HC_HASHTABLESIZE],
    chain_table: &mut [u16; LZ4HC_MAXD],
    next_to_update: &mut u32,
    base: &[u8],
    prefix_idx: u32,
    target_pos: usize,
) {
    let target = prefix_idx.wrapping_add(target_pos as u32);
    let mut idx = *next_to_update;
    while idx < target {
        let pos = idx.wrapping_sub(prefix_idx) as usize;
        if pos + MINMATCH > base.len() {
            break;
        }
        let hash = hc_hash_at(base, pos);
        let delta = idx.wrapping_sub(hash_table[hash]).min(LZ4_DISTANCE_MAX);
        chain_table[idx as u16 as usize] = delta as u16;
        hash_table[hash] = idx;
        idx = idx.wrapping_add(1);
    }
    *next_to_update = target;
}

pub fn fill_chain_table(state: &mut HcState, base: &[u8], target_pos: usize) {
    insert_chain(
        &mut state.hash_table,
        &mut state.chain_table,
        &mut state.next_to_update,
        base,
        state.dict_limit,
        target_pos,
    );
}

/// `LZ4_count` (lz4.c:1909): common bytes between `a[ai..]` and `b[bi..]`,
/// reading `a` no further than `a_limit`.
///
/// C may read past the logical end of the `b` side (it is always earlier in the
/// same allocation, or bounded by a `safeLen` the caller computed); zipping the
/// iterators reproduces the bounded behaviour without the over-read.
#[inline(always)]
fn count_match(a: &[u8], ai: usize, b: &[u8], bi: usize, a_limit: usize) -> usize {
    // PROBE B: word-at-a-time like LZ4_count (lz4.c:696), first-mismatch via
    // bit scan. Count is identical to the byte loop; only speed differs.
    if ai >= a_limit || bi >= b.len() {
        return 0;
    }
    let n = (a_limit - ai).min(b.len() - bi);
    let mut k = 0;
    while k + 8 <= n {
        let x = u64::from_ne_bytes(a[ai + k..ai + k + 8].try_into().unwrap());
        let y = u64::from_ne_bytes(b[bi + k..bi + k + 8].try_into().unwrap());
        if x != y {
            let diff = x ^ y;
            let nb = if cfg!(target_endian = "little") {
                diff.trailing_zeros() >> 3
            } else {
                diff.leading_zeros() >> 3
            };
            return k + nb as usize;
        }
        k += 8;
    }
    while k < n && a[ai + k] == b[bi + k] {
        k += 1;
    }
    k
}

// --- Sequence encoding ------------------------------------------------------

/// `LZ4HC_encodeSequence` (lz4hc.c:292). On success advances `op`, `ip` and
/// `anchor`; on failure `op` is left dangling and the caller restores it, just
/// as C does with `saved_op`.
///
/// Returns `false` for C's "1 == buffer issue detected".
#[allow(clippy::too_many_arguments)]
fn encode_sequence(
    base: &[u8],
    dst: &mut [u8],
    op: &mut usize,
    anchor: &mut usize,
    ip: &mut usize,
    match_length: usize,
    offset: usize,
    limit: Limit,
    oend: usize,
) -> bool {
    let token = *op;
    *op += 1;

    // --- Literal length ---
    let lit_len = *ip - *anchor;
    if limit.is_limited() && *op + (lit_len / 255) + lit_len + (2 + 1 + LASTLITERALS) > oend {
        return false;
    }
    // A well-behaved notLimited caller guarantees the room; bail rather than
    // panic if one lied (C would corrupt memory here instead).
    if token >= dst.len() || *op + lit_len + 2 > dst.len() {
        return false;
    }
    if lit_len >= RUN_MASK {
        dst[token] = (RUN_MASK << ML_BITS) as u8;
        let mut len = lit_len - RUN_MASK;
        while len >= 255 {
            if *op >= dst.len() {
                return false;
            }
            dst[*op] = 255;
            *op += 1;
            len -= 255;
        }
        if *op >= dst.len() {
            return false;
        }
        dst[*op] = len as u8;
        *op += 1;
    } else {
        dst[token] = (lit_len << ML_BITS) as u8;
    }

    // Copy literals. C uses LZ4_wildCopy8, which may write up to 7 bytes past
    // the run; the extra bytes are always overwritten, so an exact copy is
    // equivalent for the emitted stream.
    if *op + lit_len + 2 > dst.len() {
        return false;
    }
    dst[*op..*op + lit_len].copy_from_slice(&base[*anchor..*anchor + lit_len]);
    *op += lit_len;

    // --- Offset (16-bit LE) ---
    debug_assert!(offset > 0 && offset <= LZ4_DISTANCE_MAX as usize);
    dst[*op] = offset as u8;
    dst[*op + 1] = (offset >> 8) as u8;
    *op += 2;

    // --- Match length ---
    let mut ml_code = match_length - MINMATCH;
    if limit.is_limited() && *op + (ml_code / 255) + (1 + LASTLITERALS) > oend {
        return false;
    }
    if ml_code >= ML_MASK {
        dst[token] += ML_MASK as u8;
        ml_code -= ML_MASK;
        while ml_code >= 510 {
            if *op + 2 > dst.len() {
                return false;
            }
            dst[*op] = 255;
            dst[*op + 1] = 255;
            *op += 2;
            ml_code -= 510;
        }
        if ml_code >= 255 {
            if *op >= dst.len() {
                return false;
            }
            ml_code -= 255;
            dst[*op] = 255;
            *op += 1;
        }
        if *op >= dst.len() {
            return false;
        }
        dst[*op] = ml_code as u8;
        *op += 1;
    } else {
        dst[token] += ml_code as u8;
    }

    *ip += match_length;
    *anchor = *ip;
    true
}

// --- Dictionary-context search ---------------------------------------------

#[derive(Clone, Copy, Default)]
struct Match {
    len: usize,
    off: usize,
    back: isize,
}

fn count_back(
    base: &[u8],
    ip: usize,
    match_pos: usize,
    input_low: usize,
    match_low: usize,
) -> isize {
    let limit = (ip - input_low).min(match_pos - match_low);
    let mut back = 0usize;
    while back < limit && base[ip - back - 1] == base[match_pos - back - 1] {
        back += 1;
    }
    -(back as isize)
}

fn count_back_ext(
    base: &[u8],
    ip: usize,
    dict: &[u8],
    match_pos: usize,
    input_low: usize,
) -> isize {
    let limit = (ip - input_low).min(match_pos);
    let mut back = 0usize;
    while back < limit && base[ip - back - 1] == dict[match_pos - back - 1] {
        back += 1;
    }
    -(back as isize)
}

fn count_pattern(buf: &[u8], start: usize, end: usize, pattern: u32) -> usize {
    let bytes = pattern.to_ne_bytes();
    let mut pos = start;
    while pos < end && buf[pos] == bytes[(pos - start) & 3] {
        pos += 1;
    }
    pos - start
}

fn reverse_count_pattern(buf: &[u8], start: usize, low: usize, pattern: u32) -> usize {
    let bytes = pattern.to_ne_bytes();
    let mut pos = start;
    let mut count = 0usize;
    while pos > low && buf[pos - 1] == bytes[3 - (count & 3)] {
        pos -= 1;
        count += 1;
    }
    count
}

#[inline(always)]
fn rotate_pattern(rotate: usize, pattern: u32) -> u32 {
    pattern.rotate_left(((rotate & 3) * 8) as u32)
}

#[inline(always)]
fn protects_dict_end(dict_limit: u32, match_index: u32) -> bool {
    dict_limit.wrapping_sub(1).wrapping_sub(match_index) >= 3
}

#[allow(clippy::too_many_arguments)]
fn insert_and_get_wider_match(
    state: &mut HcState,
    v: &SrcView,
    ip: usize,
    input_low: usize,
    input_high: usize,
    mut longest: usize,
    max_attempts: usize,
    pattern_analysis: bool,
    chain_swap: bool,
    dict_ctx: Option<&MidDictCtx>,
    favor_dec_speed: bool,
) -> Match {
    insert_chain(
        &mut state.hash_table,
        &mut state.chain_table,
        &mut state.next_to_update,
        v.base,
        v.prefix_idx,
        ip,
    );

    let ip_index = v.prefix_idx.wrapping_add(ip as u32);
    let lowest_match_index =
        if (state.low_limit as u64) + (LZ4_DISTANCE_MAX as u64) + 1 > ip_index as u64 {
            state.low_limit
        } else {
            ip_index - LZ4_DISTANCE_MAX
        };
    let within_start_distance = lowest_match_index == state.low_limit;
    let look_back_length = ip - input_low;
    let pattern = u32::from_ne_bytes(v.base[ip..ip + MINMATCH].try_into().unwrap());
    let mut match_index = state.hash_table[hc_hash_at(v.base, ip)];
    let mut attempts = max_attempts;
    let mut chain_pos = 0u32;
    let mut repeat = None;
    let mut source_pattern_length = 0usize;
    let mut best = Match::default();

    while match_index >= lowest_match_index && attempts > 0 {
        attempts -= 1;
        if match_index >= ip_index {
            break;
        }
        let mut match_length = 0usize;

        if !(favor_dec_speed && ip_index - match_index < 8) {
            if match_index >= v.prefix_idx {
                let match_pos = (match_index - v.prefix_idx) as usize;
                let source_check = input_low + longest - 1;
                let match_check =
                    match_pos as isize - look_back_length as isize + longest as isize - 1;
                if match_check >= 0
                    && source_check + 2 <= v.base.len()
                    && match_check as usize + 2 <= v.base.len()
                    && v.base[source_check..source_check + 2]
                        == v.base[match_check as usize..match_check as usize + 2]
                    && match_pos + MINMATCH <= v.base.len()
                    && u32::from_ne_bytes(
                        v.base[match_pos..match_pos + MINMATCH].try_into().unwrap(),
                    ) == pattern
                {
                    let back = if look_back_length == 0 {
                        0
                    } else {
                        count_back(v.base, ip, match_pos, input_low, 0)
                    };
                    let forward = MINMATCH
                        + count_match(
                            v.base,
                            ip + MINMATCH,
                            v.base,
                            match_pos + MINMATCH,
                            input_high,
                        );
                    match_length = (forward as isize - back) as usize;
                    if match_length > longest {
                        longest = match_length;
                        best = Match {
                            len: match_length,
                            off: (ip_index - match_index) as usize,
                            back,
                        };
                    }
                }
            } else if match_index >= v.dict_idx && match_index <= v.prefix_idx.wrapping_sub(4) {
                let match_pos = (match_index - v.dict_idx) as usize;
                if match_pos + MINMATCH <= v.dict.len()
                    && u32::from_ne_bytes(
                        v.dict[match_pos..match_pos + MINMATCH].try_into().unwrap(),
                    ) == pattern
                {
                    let virtual_limit = input_high.min(ip + (v.prefix_idx - match_index) as usize);
                    let mut forward = MINMATCH
                        + count_match(
                            v.base,
                            ip + MINMATCH,
                            v.dict,
                            match_pos + MINMATCH,
                            virtual_limit,
                        );
                    if ip + forward == virtual_limit && virtual_limit < input_high {
                        forward += count_match(v.base, ip + forward, v.base, 0, input_high);
                    }
                    let back = if look_back_length == 0 {
                        0
                    } else {
                        count_back_ext(v.base, ip, v.dict, match_pos, input_low)
                    };
                    match_length = (forward as isize - back) as usize;
                    if match_length > longest {
                        longest = match_length;
                        best = Match {
                            len: match_length,
                            off: (ip_index - match_index) as usize,
                            back,
                        };
                    }
                }
            }
        }

        if chain_swap && match_length == longest && look_back_length == 0 {
            if match_index.wrapping_add(longest as u32) <= ip_index {
                let mut distance_to_next = 1u32;
                let mut accel = 1usize << 4;
                let end = longest.saturating_sub(MINMATCH) + 1;
                let mut pos = 0usize;
                while pos < end {
                    let candidate = state.chain_table
                        [match_index.wrapping_add(pos as u32) as u16 as usize]
                        as u32;
                    let step = accel >> 4;
                    accel += 1;
                    if candidate > distance_to_next {
                        distance_to_next = candidate;
                        chain_pos = pos as u32;
                        accel = 1usize << 4;
                    }
                    pos += step;
                }
                if distance_to_next > 1 {
                    if distance_to_next > match_index {
                        break;
                    }
                    match_index -= distance_to_next;
                    continue;
                }
            }
        }

        let next = state.chain_table[match_index as u16 as usize] as u32;
        if pattern_analysis && next == 1 && chain_pos == 0 && match_index > 0 {
            let candidate_index = match_index - 1;
            let is_repeat = *repeat.get_or_insert_with(|| {
                let confirmed =
                    (pattern & 0xFFFF) == (pattern >> 16) && (pattern & 0xFF) == (pattern >> 24);
                if confirmed {
                    source_pattern_length =
                        MINMATCH + count_pattern(v.base, ip + MINMATCH, input_high, pattern);
                }
                confirmed
            });
            if is_repeat
                && candidate_index >= lowest_match_index
                && protects_dict_end(v.prefix_idx, candidate_index)
            {
                let external = candidate_index < v.prefix_idx;
                let (candidate, candidate_pos, candidate_limit) = if external {
                    let Some(pos) = candidate_index.checked_sub(v.dict_idx) else {
                        break;
                    };
                    (v.dict, pos as usize, v.dict.len())
                } else {
                    (
                        v.base,
                        (candidate_index - v.prefix_idx) as usize,
                        input_high,
                    )
                };
                if candidate_pos + MINMATCH <= candidate.len()
                    && u32::from_ne_bytes(
                        candidate[candidate_pos..candidate_pos + MINMATCH]
                            .try_into()
                            .unwrap(),
                    ) == pattern
                {
                    let mut forward_length = MINMATCH
                        + count_pattern(
                            candidate,
                            candidate_pos + MINMATCH,
                            candidate_limit,
                            pattern,
                        );
                    if external && candidate_pos + forward_length == candidate_limit {
                        forward_length += count_pattern(
                            v.base,
                            0,
                            input_high,
                            rotate_pattern(forward_length, pattern),
                        );
                    }
                    let mut back_length =
                        reverse_count_pattern(candidate, candidate_pos, 0, pattern);
                    if !external && candidate_pos == back_length && v.dict_idx < v.prefix_idx {
                        back_length += reverse_count_pattern(
                            v.dict,
                            v.dict.len(),
                            0,
                            rotate_pattern(0usize.wrapping_sub(back_length), pattern),
                        );
                    }
                    back_length = back_length.min((candidate_index - lowest_match_index) as usize);
                    let segment_length = back_length + forward_length;
                    if segment_length >= source_pattern_length
                        && forward_length <= source_pattern_length
                    {
                        let new_index = candidate_index
                            .wrapping_add(forward_length as u32)
                            .wrapping_sub(source_pattern_length as u32);
                        match_index = if protects_dict_end(v.prefix_idx, new_index) {
                            new_index
                        } else {
                            v.prefix_idx
                        };
                    } else {
                        let new_index = candidate_index.wrapping_sub(back_length as u32);
                        match_index = if protects_dict_end(v.prefix_idx, new_index) {
                            new_index
                        } else {
                            v.prefix_idx
                        };
                        if look_back_length == 0 {
                            let max_length = segment_length.min(source_pattern_length);
                            if longest < max_length {
                                if ip_index.wrapping_sub(match_index) > LZ4_DISTANCE_MAX {
                                    break;
                                }
                                longest = max_length;
                                best = Match {
                                    len: max_length,
                                    off: ip_index.wrapping_sub(match_index) as usize,
                                    back: 0,
                                };
                            }
                            let pattern_next =
                                state.chain_table[match_index as u16 as usize] as u32;
                            if pattern_next == 0 || pattern_next > match_index {
                                break;
                            }
                            match_index -= pattern_next;
                        }
                    }
                    continue;
                }
            }
        }

        let chain_index = match_index.wrapping_add(chain_pos) as u16 as usize;
        let next = state.chain_table[chain_index] as u32;
        if next == 0 || next > match_index {
            break;
        }
        match_index -= next;
    }

    if let Some(dict) = dict_ctx {
        if attempts > 0 && within_start_distance {
            let dict_end_index = dict.l_dict_end_index() as u32;
            let mut dict_match_index = dict.hash_table[hc_hash_at(v.base, ip)];
            let Some(mut relative_match_index) = dict_match_index
                .checked_add(lowest_match_index)
                .and_then(|value| value.checked_sub(dict_end_index))
            else {
                return best;
            };

            while attempts > 0 && ip_index.wrapping_sub(relative_match_index) <= LZ4_DISTANCE_MAX {
                attempts -= 1;
                let Some(match_pos) = dict_match_index.checked_sub(dict.dict_limit) else {
                    break;
                };
                let match_pos = match_pos as usize;
                if match_pos + MINMATCH <= dict.prefix.len()
                    && u32::from_ne_bytes(
                        dict.prefix[match_pos..match_pos + MINMATCH]
                            .try_into()
                            .unwrap(),
                    ) == pattern
                {
                    let virtual_limit =
                        input_high.min(ip + (dict_end_index - dict_match_index) as usize);
                    let forward = MINMATCH
                        + count_match(
                            v.base,
                            ip + MINMATCH,
                            dict.prefix,
                            match_pos + MINMATCH,
                            virtual_limit,
                        );
                    let back = if look_back_length == 0 {
                        0
                    } else {
                        count_back_ext(v.base, ip, dict.prefix, match_pos, input_low)
                    };
                    let length = (forward as isize - back) as usize;
                    if length > longest {
                        longest = length;
                        best = Match {
                            len: length,
                            off: ip_index.wrapping_sub(relative_match_index) as usize,
                            back,
                        };
                    }
                }
                let next = dict.chain_table[dict_match_index as u16 as usize] as u32;
                if next == 0 || next > dict_match_index || next > relative_match_index {
                    break;
                }
                dict_match_index -= next;
                relative_match_index -= next;
            }
        }
    }

    best
}

#[allow(clippy::too_many_arguments)]
fn insert_and_find_best_match(
    state: &mut HcState,
    v: &SrcView,
    ip: usize,
    input_high: usize,
    max_attempts: usize,
    pattern_analysis: bool,
    dict_ctx: Option<&MidDictCtx>,
) -> Match {
    insert_and_get_wider_match(
        state,
        v,
        ip,
        ip,
        input_high,
        MINMATCH - 1,
        max_attempts,
        pattern_analysis,
        false,
        dict_ctx,
        false,
    )
}

/// `LZ4MID_searchExtDict` (lz4hc.c:446).
fn mid_search_ext_dict(
    base: &[u8],
    ip: usize,
    ip_index: u32,
    i_high_limit: usize,
    d: &MidDictCtx,
    g_dict_end_index: u32,
) -> Option<Match> {
    let l_dict_end_index = d.l_dict_end_index();
    let (hash4_table, hash8_table) = d.hash_table.split_at(LZ4MID_HASHTABLESIZE);

    // Long match first, then short: the order decides which one wins.
    for (table, hash) in [
        (hash8_table, mid_hash8_at(base, ip)),
        (hash4_table, mid_hash4_at(base, ip)),
    ] {
        let l_dict_match_index = table[hash];
        let m_index = l_dict_match_index
            .wrapping_add(g_dict_end_index)
            .wrapping_sub(l_dict_end_index as u32);
        if ip_index.wrapping_sub(m_index) > LZ4_DISTANCE_MAX {
            continue;
        }
        // matchPtr = dictCtx->prefixStart - dictCtx->dictLimit + lDictMatchIndex
        let Some(match_off) = l_dict_match_index.checked_sub(d.dict_limit) else {
            // C would form a pointer below the dictionary buffer and read it.
            // Refuse instead; see PORTING.md on divergences we take on purpose.
            continue;
        };
        let match_off = match_off as usize;
        if match_off >= d.prefix.len() || l_dict_match_index as usize > l_dict_end_index {
            continue;
        }
        let safe_len =
            (l_dict_end_index - l_dict_match_index as usize).min(i_high_limit.saturating_sub(ip));
        let mlt = count_match(base, ip, d.prefix, match_off, ip + safe_len);
        if mlt >= MINMATCH {
            return Some(Match {
                len: mlt,
                off: ip_index.wrapping_sub(m_index) as usize,
                back: 0,
            });
        }
    }
    None
}

// --- LZ4MID compression -----------------------------------------------------

/// The buffers `LZ4MID_compress` works over. See the module docs for why the
/// history and the current block share one slice.
pub struct SrcView<'a> {
    /// C's `[prefixStart, end)`: history bytes followed by the current block.
    pub base: &'a [u8],
    /// Offset of the current block within `base` (C's `src - prefixStart`).
    pub src_off: usize,
    /// `ctx->dictLimit` — the absolute index of `base[0]`.
    pub prefix_idx: u32,
    /// External dictionary, a separate allocation (C's `dictStart`).
    pub dict: &'a [u8],
    /// `ctx->lowLimit` — the absolute index of `dict[0]`.
    pub dict_idx: u32,
}

/// `LZ4MID_compress` (lz4hc.c:553).
///
/// Preconditions, as in C: `1 <= src len <= LZ4_MAX_INPUT_SIZE` and
/// `dst` non-empty.
///
/// Returns `(bytes written, bytes of the block consumed)`. A written count of 0
/// is C's compression failure.
pub fn compress_mid(
    hash4_table: &mut [u32],
    hash8_table: &mut [u32],
    v: &SrcView,
    dst: &mut [u8],
    limit: Limit,
    dict_ctx: Option<&MidDictCtx>,
) -> (usize, usize) {
    let base = v.base;
    let prefix_idx = v.prefix_idx;
    let dict_idx = v.dict_idx;
    let g_dict_end_index = v.dict_idx;

    let iend = base.len();
    let src_size = iend - v.src_off;
    let mflimit = iend.saturating_sub(MFLIMIT);
    let matchlimit = iend.saturating_sub(LASTLITERALS);
    let ilimit = iend.saturating_sub(LZ4MID_HASHSIZE);
    let ilimit_idx = ilimit as u32 + prefix_idx;

    let mut ip = v.src_off;
    let mut anchor = ip;
    let mut op = 0usize;
    // "Hack for support LZ4 format restriction" (lz4hc.c:594).
    let mut oend = dst.len();
    if limit == Limit::FillOutput {
        oend = oend.saturating_sub(LASTLITERALS);
    }

    if src_size < LZ4_MIN_LENGTH {
        return last_literals(base, dst, anchor, iend, op, oend, limit, v.src_off);
    }

    while ip <= mflimit {
        let ip_index = ip as u32 + prefix_idx;

        // The match we settle on, if any: (length, distance, encode position).
        // C reaches the encoder by `goto`; this holds the same three values.
        let mut found: Option<(usize, usize)> = None;

        // --- search long match (hash8) ---
        {
            let h8 = mid_hash8_at(base, ip);
            let pos8 = hash8_table[h8];
            hash8_table[h8] = ip_index;
            if ip_index.wrapping_sub(pos8) <= LZ4_DISTANCE_MAX {
                if pos8 >= prefix_idx {
                    let match_off = (pos8 - prefix_idx) as usize;
                    let ml = count_match(base, ip, base, match_off, matchlimit);
                    if ml >= MINMATCH {
                        found = Some((ml, (ip_index - pos8) as usize));
                    }
                } else if pos8 >= dict_idx {
                    let match_off = (pos8 - dict_idx) as usize;
                    let safe_len =
                        ((prefix_idx - pos8) as usize).min(matchlimit.saturating_sub(ip));
                    let ml = count_match(base, ip, v.dict, match_off, ip + safe_len);
                    if ml >= MINMATCH {
                        found = Some((ml, (ip_index - pos8) as usize));
                    }
                }
            }
        }

        // --- search short match (hash4) ---
        if found.is_none() {
            let h4 = mid_hash4_at(base, ip);
            let pos4 = hash4_table[h4];
            hash4_table[h4] = ip_index;
            if ip_index.wrapping_sub(pos4) <= LZ4_DISTANCE_MAX {
                if pos4 >= prefix_idx {
                    let match_off = (pos4 - prefix_idx) as usize;
                    let mut ml = count_match(base, ip, base, match_off, matchlimit);
                    if ml >= MINMATCH {
                        // Short match found; check ip+1 for a longer one.
                        let mut match_distance = (ip_index - pos4) as usize;
                        let h8 = mid_hash8_at(base, ip + 1);
                        let pos8 = hash8_table[h8];
                        let m2_distance = ip_index.wrapping_add(1).wrapping_sub(pos8);
                        if m2_distance <= LZ4_DISTANCE_MAX && pos8 >= prefix_idx && ip < mflimit {
                            let m2_off = (pos8 - prefix_idx) as usize;
                            let ml2 = count_match(base, ip + 1, base, m2_off, matchlimit);
                            if ml2 > ml {
                                hash8_table[h8] = ip_index + 1;
                                ip += 1;
                                ml = ml2;
                                match_distance = m2_distance as usize;
                            }
                        }
                        found = Some((ml, match_distance));
                    }
                } else if pos4 >= dict_idx {
                    let match_off = (pos4 - dict_idx) as usize;
                    let safe_len =
                        ((prefix_idx - pos4) as usize).min(matchlimit.saturating_sub(ip));
                    let ml = count_match(base, ip, v.dict, match_off, ip + safe_len);
                    if ml >= MINMATCH {
                        found = Some((ml, (ip_index - pos4) as usize));
                    }
                }
            }
        }

        // --- search a match into the attached dictionary context ---
        if found.is_none() {
            if let Some(d) = dict_ctx {
                if ip_index.wrapping_sub(g_dict_end_index) < LZ4_DISTANCE_MAX - 8 {
                    if let Some(m) =
                        mid_search_ext_dict(base, ip, ip_index, matchlimit, d, g_dict_end_index)
                    {
                        found = Some((m.len, m.off));
                    }
                }
            }
        }

        let Some((mut match_length, match_distance)) = found else {
            // Skip faster over incompressible data.
            ip += 1 + ((ip - anchor) >> 9);
            continue;
        };

        // --- catch back ---
        // C: (U32)(ip - prefixPtr) > matchDistance, i.e. the extended match
        // must not reach back before prefixStart.
        while ip > anchor && ip > match_distance && base[ip - 1] == base[ip - match_distance - 1] {
            ip -= 1;
            match_length += 1;
        }

        // --- fill table with beginning of match ---
        // Note: `ip` has just moved (peek and/or catch-back) but `ip_index` is
        // deliberately the value from the top of the loop. C does the same, so
        // the indices stored here can disagree with the positions hashed. Do
        // not "fix" this — it changes the emitted stream.
        add_pos8(hash8_table, base, ip + 1, ip_index + 1);
        add_pos8(hash8_table, base, ip + 2, ip_index + 2);
        add_pos4(hash4_table, base, ip + 1, ip_index + 1);

        // --- encode ---
        let saved_op = op;
        if !encode_sequence(
            base,
            dst,
            &mut op,
            &mut anchor,
            &mut ip,
            match_length,
            match_distance,
            limit,
            oend,
        ) {
            op = saved_op;
            return dest_overflow(
                base,
                dst,
                &mut anchor,
                &mut ip,
                iend,
                op,
                oend,
                match_length,
                match_distance,
                limit,
                v.src_off,
            );
        }

        // --- fill table with end of match ---
        let end_match_idx = ip as u32 + prefix_idx;
        if end_match_idx.wrapping_sub(2) < ilimit_idx {
            if ip > 5 {
                add_pos8(hash8_table, base, ip - 5, end_match_idx - 5);
            }
            add_pos8(hash8_table, base, ip - 3, end_match_idx - 3);
            add_pos8(hash8_table, base, ip - 2, end_match_idx - 2);
            add_pos4(hash4_table, base, ip - 2, end_match_idx - 2);
            add_pos4(hash4_table, base, ip - 1, end_match_idx - 1);
        }
    }

    last_literals(base, dst, anchor, iend, op, oend, limit, v.src_off)
}

/// `ADDPOS8` (lz4hc.c:508).
#[inline(always)]
fn add_pos8(hash8_table: &mut [u32], base: &[u8], pos: usize, index: u32) {
    // The call sites are all provably in range for C; the guard keeps a
    // mis-sized buffer from panicking rather than silently over-reading.
    if pos + LZ4MID_HASHSIZE <= base.len() {
        hash8_table[mid_hash8_at(base, pos)] = index;
    }
}

/// `ADDPOS4` (lz4hc.c:509).
#[inline(always)]
fn add_pos4(hash4_table: &mut [u32], base: &[u8], pos: usize, index: u32) {
    if pos + MINMATCH <= base.len() {
        hash4_table[mid_hash4_at(base, pos)] = index;
    }
}

/// `_lz4mid_last_literals` (lz4hc.c:737).
#[allow(clippy::too_many_arguments)]
fn last_literals(
    base: &[u8],
    dst: &mut [u8],
    anchor: usize,
    iend: usize,
    mut op: usize,
    oend: usize,
    limit: Limit,
    src_off: usize,
) -> (usize, usize) {
    let mut last_run_size = iend - anchor;
    let ll_add = (last_run_size + 255 - RUN_MASK) / 255;
    let total_size = 1 + ll_add + last_run_size;
    // Restore the correct value hidden by the fillOutput hack.
    let oend = if limit == Limit::FillOutput {
        dst.len()
    } else {
        oend
    };

    if limit.is_limited() && op + total_size > oend {
        if limit == Limit::LimitedOutput {
            return (0, 0); // not enough space in dst
        }
        // Adapt lastRunSize to fill dst exactly.
        let avail = oend.saturating_sub(op);
        if avail == 0 {
            return (0, 0);
        }
        last_run_size = avail - 1 /*token*/;
        let ll_add = (last_run_size + 256 - RUN_MASK) / 256;
        last_run_size = last_run_size.saturating_sub(ll_add);
    }

    // `ip` can be != iend when limit == fillOutput.
    let ip = anchor + last_run_size;

    if last_run_size >= RUN_MASK {
        let mut accumulator = last_run_size - RUN_MASK;
        dst[op] = (RUN_MASK << ML_BITS) as u8;
        op += 1;
        while accumulator >= 255 {
            dst[op] = 255;
            op += 1;
            accumulator -= 255;
        }
        dst[op] = accumulator as u8;
        op += 1;
    } else {
        dst[op] = (last_run_size << ML_BITS) as u8;
        op += 1;
    }
    debug_assert!(last_run_size <= oend - op);
    dst[op..op + last_run_size].copy_from_slice(&base[anchor..anchor + last_run_size]);
    op += last_run_size;

    (op, ip - src_off)
}

/// `_lz4mid_dest_overflow` (lz4hc.c:777).
#[allow(clippy::too_many_arguments)]
fn dest_overflow(
    base: &[u8],
    dst: &mut [u8],
    anchor: &mut usize,
    ip: &mut usize,
    iend: usize,
    mut op: usize,
    oend: usize,
    mut match_length: usize,
    match_distance: usize,
    limit: Limit,
    src_off: usize,
) -> (usize, usize) {
    if limit != Limit::FillOutput {
        return (0, 0); // compression failed
    }
    let ll = *ip - *anchor;
    let ll_addbytes = (ll + 240) / 255;
    let ll_total_cost = 1 + ll_addbytes + ll;
    let max_lit_pos = oend.saturating_sub(3); // 2 for offset, 1 for token
    if op + ll_total_cost <= max_lit_pos {
        // ll validated; now adjust the match length to what is left.
        let bytes_left_for_ml = max_lit_pos - (op + ll_total_cost);
        let max_ml_size = MINMATCH + (ML_MASK - 1) + bytes_left_for_ml * 255;
        match_length = match_length.min(max_ml_size);
        // C: (oend + LASTLITERALS) - (op + ll_totalCost + 2) - 1 + matchLength >= MFLIMIT
        let lhs = (oend + LASTLITERALS) as isize - (op + ll_total_cost + 2) as isize - 1
            + match_length as isize;
        if lhs >= MFLIMIT as isize {
            encode_sequence(
                base,
                dst,
                &mut op,
                anchor,
                ip,
                match_length,
                match_distance,
                Limit::NotLimited,
                oend,
            );
        }
    }
    last_literals(base, dst, *anchor, iend, op, oend, limit, src_off)
}

/// `LZ4MID_fillHTable` (lz4hc.c:513) — index the loaded dictionary.
pub fn mid_fill_htable(
    hash4_table: &mut [u32],
    hash8_table: &mut [u32],
    dict: &[u8],
    prefix_idx: u32,
    next_to_update: &mut u32,
) {
    let size = dict.len();
    if size <= LZ4MID_HASHSIZE {
        return;
    }
    let target = prefix_idx + size as u32 - LZ4MID_HASHSIZE as u32;

    let mut idx = *next_to_update;
    while idx < target {
        let p = (idx - prefix_idx) as usize;
        add_pos4(hash4_table, dict, p, idx);
        add_pos8(hash8_table, dict, p + 1, idx + 1);
        idx += 3;
    }

    let mut idx = if size > 32 * 1024 + LZ4MID_HASHSIZE {
        target - 32 * 1024
    } else {
        *next_to_update
    };
    while idx < target {
        add_pos8(hash8_table, dict, (idx - prefix_idx) as usize, idx);
        idx += 1;
    }

    *next_to_update = target;
}

#[derive(Clone, Copy, Default)]
struct Optimal {
    price: i32,
    off: usize,
    mlen: usize,
    litlen: usize,
}

#[inline(always)]
fn literals_price(litlen: usize) -> i32 {
    let mut price = litlen;
    if litlen >= RUN_MASK {
        price += 1 + ((litlen - RUN_MASK) / 255);
    }
    price as i32
}

#[inline(always)]
fn sequence_price(litlen: usize, match_length: usize) -> i32 {
    let mut price = 3 + literals_price(litlen);
    if match_length >= ML_MASK + MINMATCH {
        price += (1 + ((match_length - (ML_MASK + MINMATCH)) / 255)) as i32;
    }
    price
}

#[allow(clippy::too_many_arguments)]
fn find_longer_match(
    state: &mut HcState,
    v: &SrcView,
    ip: usize,
    input_high: usize,
    min_length: usize,
    searches: usize,
    dict_ctx: Option<&MidDictCtx>,
    favor_dec_speed: bool,
) -> Match {
    let mut found = insert_and_get_wider_match(
        state,
        v,
        ip,
        ip,
        input_high,
        min_length,
        searches,
        true,
        true,
        dict_ctx,
        favor_dec_speed,
    );
    debug_assert_eq!(found.back, 0);
    if found.len <= min_length {
        return Match::default();
    }
    if favor_dec_speed && found.len > 18 && found.len <= 36 {
        found.len = 18;
    }
    found
}

#[allow(clippy::too_many_arguments)]
fn compress_hash_chain(
    state: &mut HcState,
    v: &SrcView,
    dst: &mut [u8],
    limit: Limit,
    max_attempts: usize,
    dict_ctx: Option<&MidDictCtx>,
) -> (usize, usize) {
    let base = v.base;
    let iend = base.len();
    let src_size = iend - v.src_off;
    let mflimit = iend.saturating_sub(MFLIMIT);
    let matchlimit = iend.saturating_sub(LASTLITERALS);
    let pattern_analysis = max_attempts > 128;
    let mut ip = v.src_off;
    let mut anchor = ip;
    let mut op = 0usize;
    let mut oend = dst.len();
    if limit == Limit::FillOutput {
        oend = oend.saturating_sub(LASTLITERALS);
    }

    if src_size < LZ4_MIN_LENGTH {
        return last_literals(base, dst, anchor, iend, op, oend, limit, v.src_off);
    }

    'main: while ip <= mflimit {
        let mut m1 = insert_and_find_best_match(
            state,
            v,
            ip,
            matchlimit,
            max_attempts,
            pattern_analysis,
            dict_ctx,
        );
        if m1.len < MINMATCH {
            ip += 1;
            continue;
        }

        let mut start0 = ip;
        let mut m0 = m1;

        'search2: loop {
            let (mut start2, mut m2) = if ip + m1.len <= mflimit {
                let search_start = ip + m1.len - 2;
                let found = insert_and_get_wider_match(
                    state,
                    v,
                    search_start,
                    ip,
                    matchlimit,
                    m1.len,
                    max_attempts,
                    pattern_analysis,
                    false,
                    dict_ctx,
                    false,
                );
                ((search_start as isize + found.back) as usize, found)
            } else {
                (0, Match::default())
            };

            if m2.len <= m1.len {
                let saved_op = op;
                if !encode_sequence(
                    base,
                    dst,
                    &mut op,
                    &mut anchor,
                    &mut ip,
                    m1.len,
                    m1.off,
                    limit,
                    oend,
                ) {
                    op = saved_op;
                    return dest_overflow(
                        base,
                        dst,
                        &mut anchor,
                        &mut ip,
                        iend,
                        op,
                        oend,
                        m1.len,
                        m1.off,
                        limit,
                        v.src_off,
                    );
                }
                continue 'main;
            }

            if start0 < ip && start2 < ip + m0.len {
                ip = start0;
                m1 = m0;
            }

            if start2 - ip < 3 {
                ip = start2;
                m1 = m2;
                continue 'search2;
            }

            'search3: loop {
                if start2 - ip < OPTIMAL_ML {
                    let mut new_match_length = m1.len.min(OPTIMAL_ML);
                    if ip + new_match_length > start2 + m2.len - MINMATCH {
                        new_match_length = start2 - ip + m2.len - MINMATCH;
                    }
                    let correction = new_match_length as isize - (start2 - ip) as isize;
                    if correction > 0 {
                        start2 += correction as usize;
                        m2.len -= correction as usize;
                    }
                }

                let (start3, m3) = if start2 + m2.len <= mflimit {
                    let search_start = start2 + m2.len - 3;
                    let found = insert_and_get_wider_match(
                        state,
                        v,
                        search_start,
                        start2,
                        matchlimit,
                        m2.len,
                        max_attempts,
                        pattern_analysis,
                        false,
                        dict_ctx,
                        false,
                    );
                    ((search_start as isize + found.back) as usize, found)
                } else {
                    (0, Match::default())
                };

                if m3.len <= m2.len {
                    if start2 < ip + m1.len {
                        m1.len = start2 - ip;
                    }
                    let saved_op = op;
                    if !encode_sequence(
                        base,
                        dst,
                        &mut op,
                        &mut anchor,
                        &mut ip,
                        m1.len,
                        m1.off,
                        limit,
                        oend,
                    ) {
                        op = saved_op;
                        return dest_overflow(
                            base,
                            dst,
                            &mut anchor,
                            &mut ip,
                            iend,
                            op,
                            oend,
                            m1.len,
                            m1.off,
                            limit,
                            v.src_off,
                        );
                    }
                    ip = start2;
                    let saved_op = op;
                    if !encode_sequence(
                        base,
                        dst,
                        &mut op,
                        &mut anchor,
                        &mut ip,
                        m2.len,
                        m2.off,
                        limit,
                        oend,
                    ) {
                        op = saved_op;
                        return dest_overflow(
                            base,
                            dst,
                            &mut anchor,
                            &mut ip,
                            iend,
                            op,
                            oend,
                            m2.len,
                            m2.off,
                            limit,
                            v.src_off,
                        );
                    }
                    continue 'main;
                }

                if start3 < ip + m1.len + 3 {
                    if start3 >= ip + m1.len {
                        if start2 < ip + m1.len {
                            let correction = ip + m1.len - start2;
                            start2 += correction;
                            m2.len -= correction;
                            if m2.len < MINMATCH {
                                start2 = start3;
                                m2 = m3;
                            }
                        }

                        let saved_op = op;
                        if !encode_sequence(
                            base,
                            dst,
                            &mut op,
                            &mut anchor,
                            &mut ip,
                            m1.len,
                            m1.off,
                            limit,
                            oend,
                        ) {
                            op = saved_op;
                            return dest_overflow(
                                base,
                                dst,
                                &mut anchor,
                                &mut ip,
                                iend,
                                op,
                                oend,
                                m1.len,
                                m1.off,
                                limit,
                                v.src_off,
                            );
                        }
                        ip = start3;
                        m1 = m3;
                        start0 = start2;
                        m0 = m2;
                        continue 'search2;
                    }

                    start2 = start3;
                    m2 = m3;
                    continue 'search3;
                }

                if start2 < ip + m1.len {
                    if start2 - ip < OPTIMAL_ML {
                        m1.len = m1.len.min(OPTIMAL_ML);
                        if ip + m1.len > start2 + m2.len - MINMATCH {
                            m1.len = start2 - ip + m2.len - MINMATCH;
                        }
                        let correction = m1.len as isize - (start2 - ip) as isize;
                        if correction > 0 {
                            start2 += correction as usize;
                            m2.len -= correction as usize;
                        }
                    } else {
                        m1.len = start2 - ip;
                    }
                }

                let saved_op = op;
                if !encode_sequence(
                    base,
                    dst,
                    &mut op,
                    &mut anchor,
                    &mut ip,
                    m1.len,
                    m1.off,
                    limit,
                    oend,
                ) {
                    op = saved_op;
                    return dest_overflow(
                        base,
                        dst,
                        &mut anchor,
                        &mut ip,
                        iend,
                        op,
                        oend,
                        m1.len,
                        m1.off,
                        limit,
                        v.src_off,
                    );
                }

                ip = start2;
                m1 = m2;
                start2 = start3;
                m2 = m3;
            }
        }
    }

    last_literals(base, dst, anchor, iend, op, oend, limit, v.src_off)
}

fn compress_optimal(
    state: &mut HcState,
    v: &SrcView,
    dst: &mut [u8],
    limit: Limit,
    searches: usize,
    sufficient_length: usize,
    full_update: bool,
    dict_ctx: Option<&MidDictCtx>,
) -> (usize, usize) {
    let base = v.base;
    let iend = base.len();
    let src_size = iend - v.src_off;
    let mflimit = iend.saturating_sub(MFLIMIT);
    let matchlimit = iend.saturating_sub(LASTLITERALS);
    let favor_dec_speed = state.favor_dec_speed != 0;
    let sufficient_length = sufficient_length.min(LZ4_OPT_NUM - 1);
    let mut opt = vec![Optimal::default(); LZ4_OPT_NUM + TRAILING_LITERALS];
    let mut ip = v.src_off;
    let mut anchor = ip;
    let mut op = 0usize;
    let mut oend = dst.len();
    if limit == Limit::FillOutput {
        oend = oend.saturating_sub(LASTLITERALS);
    }

    if src_size < LZ4_MIN_LENGTH {
        return last_literals(base, dst, anchor, iend, op, oend, limit, v.src_off);
    }

    while ip <= mflimit {
        let literal_length = ip - anchor;
        let first_match = find_longer_match(
            state,
            v,
            ip,
            matchlimit,
            MINMATCH - 1,
            searches,
            dict_ctx,
            favor_dec_speed,
        );
        if first_match.len == 0 {
            ip += 1;
            continue;
        }

        if first_match.len > sufficient_length {
            let saved_op = op;
            if !encode_sequence(
                base,
                dst,
                &mut op,
                &mut anchor,
                &mut ip,
                first_match.len,
                first_match.off,
                limit,
                oend,
            ) {
                op = saved_op;
                return dest_overflow(
                    base,
                    dst,
                    &mut anchor,
                    &mut ip,
                    iend,
                    op,
                    oend,
                    first_match.len,
                    first_match.off,
                    limit,
                    v.src_off,
                );
            }
            continue;
        }

        for relative_pos in 0..MINMATCH {
            opt[relative_pos] = Optimal {
                price: literals_price(literal_length + relative_pos),
                off: 0,
                mlen: 1,
                litlen: literal_length + relative_pos,
            };
        }
        for match_length in MINMATCH..=first_match.len {
            opt[match_length] = Optimal {
                price: sequence_price(literal_length, match_length),
                off: first_match.off,
                mlen: match_length,
                litlen: literal_length,
            };
        }

        let mut last_match_pos = first_match.len;
        for add_literal in 1..=TRAILING_LITERALS {
            opt[last_match_pos + add_literal] = Optimal {
                price: opt[last_match_pos].price + literals_price(add_literal),
                off: 0,
                mlen: 1,
                litlen: add_literal,
            };
        }

        let mut immediate = None;
        let mut frontier = 1usize;
        while frontier < last_match_pos {
            let current = frontier;
            frontier += 1;
            let current_ptr = ip + current;
            if current_ptr > mflimit {
                break;
            }
            if full_update {
                if opt[current + 1].price <= opt[current].price
                    && opt[current + MINMATCH].price < opt[current].price + 3
                {
                    continue;
                }
            } else if opt[current + 1].price <= opt[current].price {
                continue;
            }

            let minimum = if full_update {
                MINMATCH - 1
            } else {
                last_match_pos - current
            };
            let new_match = find_longer_match(
                state,
                v,
                current_ptr,
                matchlimit,
                minimum,
                searches,
                dict_ctx,
                favor_dec_speed,
            );
            if new_match.len == 0 {
                continue;
            }
            if new_match.len > sufficient_length || new_match.len + current >= LZ4_OPT_NUM {
                immediate = Some((current, current + 1, new_match.len, new_match.off));
                break;
            }

            let base_literal_length = opt[current].litlen;
            for literal_length in 1..MINMATCH {
                let price = opt[current].price - literals_price(base_literal_length)
                    + literals_price(base_literal_length + literal_length);
                let pos = current + literal_length;
                if price < opt[pos].price {
                    opt[pos] = Optimal {
                        price,
                        off: 0,
                        mlen: 1,
                        litlen: base_literal_length + literal_length,
                    };
                }
            }

            for match_length in MINMATCH..=new_match.len {
                let pos = current + match_length;
                let (literal_length, price) = if opt[current].mlen == 1 {
                    let literal_length = opt[current].litlen;
                    let prior_price = if current > literal_length {
                        opt[current - literal_length].price
                    } else {
                        0
                    };
                    (
                        literal_length,
                        prior_price + sequence_price(literal_length, match_length),
                    )
                } else {
                    (0, opt[current].price + sequence_price(0, match_length))
                };
                let replace = pos > last_match_pos + TRAILING_LITERALS
                    || price <= opt[pos].price - i32::from(favor_dec_speed);
                if replace {
                    if match_length == new_match.len && last_match_pos < pos {
                        last_match_pos = pos;
                    }
                    opt[pos] = Optimal {
                        price,
                        off: new_match.off,
                        mlen: match_length,
                        litlen: literal_length,
                    };
                }
            }

            for add_literal in 1..=TRAILING_LITERALS {
                opt[last_match_pos + add_literal] = Optimal {
                    price: opt[last_match_pos].price + literals_price(add_literal),
                    off: 0,
                    mlen: 1,
                    litlen: add_literal,
                };
            }
        }

        let (mut current, selected_end, mut selected_length, mut selected_offset) =
            if let Some(values) = immediate {
                values
            } else {
                let selected = opt[last_match_pos];
                (
                    last_match_pos - selected.mlen,
                    last_match_pos,
                    selected.mlen,
                    selected.off,
                )
            };

        loop {
            let next_length = opt[current].mlen;
            let next_offset = opt[current].off;
            opt[current].mlen = selected_length;
            opt[current].off = selected_offset;
            selected_length = next_length;
            selected_offset = next_offset;
            if next_length > current {
                break;
            }
            current -= next_length;
        }

        let mut relative_pos = 0usize;
        while relative_pos < selected_end {
            let selected = opt[relative_pos];
            if selected.mlen == 1 {
                ip += 1;
                relative_pos += 1;
                continue;
            }
            relative_pos += selected.mlen;
            let saved_op = op;
            if !encode_sequence(
                base,
                dst,
                &mut op,
                &mut anchor,
                &mut ip,
                selected.mlen,
                selected.off,
                limit,
                oend,
            ) {
                op = saved_op;
                return dest_overflow(
                    base,
                    dst,
                    &mut anchor,
                    &mut ip,
                    iend,
                    op,
                    oend,
                    selected.mlen,
                    selected.off,
                    limit,
                    v.src_off,
                );
            }
        }
    }

    last_literals(base, dst, anchor, iend, op, oend, limit, v.src_off)
}

// --- Strategy dispatch ------------------------------------------------------

/// `LZ4HC_compress_generic_internal` (lz4hc.c:1417).
///
/// `ffi` has already resolved the context's pointers into `v`. Returns
/// `(written, consumed)`; `written == 0` is failure, and the caller marks the
/// context dirty exactly as C does.
pub fn compress_generic_internal(
    state: &mut HcState,
    v: &SrcView,
    dst: &mut [u8],
    level: i32,
    limit: Limit,
    dict_ctx: Option<&MidDictCtx>,
) -> (usize, usize) {
    let src_size = v.base.len() - v.src_off;

    // Input sanitization (lz4hc.c:1431-1437).
    if src_size as u32 > LZ4_MAX_INPUT_SIZE {
        return (0, 0);
    }
    if dst.is_empty() {
        return (0, 0);
    }
    if src_size == 0 {
        dst[0] = 0;
        return (1, 0);
    }

    let params = compression_params(level);
    let result = match params.strategy {
        Strategy::Mid => {
            let (hash4_table, hash8_table) = state.hash_table.split_at_mut(LZ4MID_HASHTABLESIZE);
            compress_mid(hash4_table, hash8_table, v, dst, limit, dict_ctx)
        }
        Strategy::HashChain => compress_hash_chain(state, v, dst, limit, params.searches, dict_ctx),
        Strategy::Optimal => compress_optimal(
            state,
            v,
            dst,
            limit,
            params.searches,
            params.target_length,
            level >= LZ4HC_CLEVEL_MAX,
            dict_ctx,
        ),
    };
    if result.0 == 0 {
        state.dirty = 1;
    }
    result
}

/// `LZ4_compressBound` mirror, kept here so `ffi` can reach it via either
/// module.
pub fn compress_bound(src_size: i32) -> i32 {
    crate::block::compress_bound(src_size)
}
