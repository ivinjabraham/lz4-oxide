//! hc: safe-Rust port of `lz4hc.c`. Entry points live in `crate::ffi`.
//!
//! Nothing here may use `unsafe`; pointer handling stays in the FFI shim so
//! the port's unsafe surface is small and countable.
//!
//! # Scope
//!
//! This module implements the **`lz4mid` strategy** (`lz4hc.c:553-806`), which
//! C selects for compression levels 1 and 2. The hash-chain parser (levels 3-9)
//! and the optimal parser (levels 10-12) are not ported yet; `compress_generic`
//! routes every level to `lz4mid`. That is the "degrade, don't delete" fallback
//! from PLAN.md §6.1: the output is valid LZ4 that round-trips and CRC-checks,
//! but it is only *byte-identical* to C for levels 1-2.
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

/// `LZ4_count` (lz4.c:1909): common bytes between `a[ai..]` and `b[bi..]`,
/// reading `a` no further than `a_limit`.
///
/// C may read past the logical end of the `b` side (it is always earlier in the
/// same allocation, or bounded by a `safeLen` the caller computed); zipping the
/// iterators reproduces the bounded behaviour without the over-read.
#[inline(always)]
fn count_match(a: &[u8], ai: usize, b: &[u8], bi: usize, a_limit: usize) -> usize {
    if ai >= a_limit || bi >= b.len() {
        return 0;
    }
    a[ai..a_limit]
        .iter()
        .zip(b[bi..].iter())
        .take_while(|(x, y)| x == y)
        .count()
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

struct Match {
    len: usize,
    off: usize,
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
        oend -= LASTLITERALS;
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
        while ip > anchor
            && ip > match_distance
            && base[ip - 1] == base[ip - match_distance - 1]
        {
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
        oend + LASTLITERALS
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

// --- Strategy dispatch ------------------------------------------------------

/// `LZ4HC_compress_generic_internal` (lz4hc.c:1417), restricted to the
/// `lz4mid` strategy (see the module note on scope).
///
/// `ffi` has already resolved the context's pointers into `v`. Returns
/// `(written, consumed)`; `written == 0` is failure, and the caller marks the
/// context dirty exactly as C does.
pub fn compress_generic_internal(
    state: &mut HcState,
    v: &SrcView,
    dst: &mut [u8],
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

    let (hash4_table, hash8_table) = state.hash_table.split_at_mut(LZ4MID_HASHTABLESIZE);
    let result = compress_mid(hash4_table, hash8_table, v, dst, limit, dict_ctx);
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
