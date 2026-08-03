//! block: `lib/lz4.c` — the core block codec, in safe Rust on slices.
//!
//! Entry points live in `crate::ffi`. Nothing here may use `unsafe`; pointer
//! handling stays in the FFI shim so the port's unsafe surface is small and
//! countable.
//!
//! ## Two deliberate departures from the C, neither of which changes a byte
//!
//! 1. **No wildcopy — but the bulk copies are here.** `LZ4_wildCopy8`/`32`
//!    (lz4.c:466, :531) copy in 8- and 32-byte steps and overwrite past the
//!    logical end of the data, which the caller has reserved slack for. We do
//!    not overshoot; we reach the same throughput with copies that write
//!    exactly the bytes that belong there — `copy_within` for disjoint regions,
//!    a doubling loop for overlapping ones (`copy_match`), and word-at-a-time
//!    comparison in the match search (`common_bytes`). The limit *constants*
//!    and every comparison against them are ported verbatim, because those
//!    decide the parse and therefore the output.
//!
//!    An earlier revision of this note gave the reason as "in Rust that is a
//!    panic" and left the copies byte-at-a-time. The premise is wrong: every
//!    wildcopy call site in `safe_decode` is guarded so the overshoot lands
//!    inside the buffer (`cpy <= oend-MFLIMIT` with `MFLIMIT` 12 against a
//!    7-byte overshoot at lz4.c:2350; `oCopyLimit = oend-7` at :2444), so
//!    overshooting would have been legal after all. It simply is not necessary
//!    — and skipping it keeps the module inside `forbid(unsafe_code)`.
//! 2. **One decode loop.** C has `LZ4_FAST_DEC_LOOP` in front of
//!    `safe_decode`; we port `safe_decode`. For well-formed input both emit
//!    the same bytes, since the format determines them.
//!
//!    The two-stage shortcut inside `safe_decode` (lz4.c:2241-2272) is a
//!    different matter and **is** ported, because it is *not* equivalent to
//!    the general path it precedes. Its guard is `ip < iend-16`, which is
//!    weaker than the general path's parsing restriction
//!    (`ip + length > iend - 8`), so it accepts sequences the general path
//!    rejects. Omitting it made us reject a corrupt block that C decoded to
//!    29643 bytes. Treat "this is only an optimisation" as a claim to verify,
//!    not to assume.
//!
//! What is *not* optional is in `PORTING.md` §3: the per-`tableType` hashes,
//! the skip heuristic, and the byte-at-a-time overlapping match copy.
#![forbid(unsafe_code)]

use crate::types::LZ4_MEMORY_USAGE_PROBED;
use core::ops::Range;

// --- lz4.c:243-264, lz4.h:214 ----------------------------------------------
const MINMATCH: usize = 4;
const LASTLITERALS: usize = 5;
const MFLIMIT: usize = 12;
const MATCH_SAFEGUARD_DISTANCE: usize = (2 * 8) - MINMATCH;
/// `LZ4_minLength` (lz4.c:250). Shorter inputs are emitted as pure literals.
const LZ4_MIN_LENGTH: usize = MFLIMIT + 1;
const ML_BITS: u32 = 4;
const ML_MASK: u32 = (1 << ML_BITS) - 1;
const RUN_BITS: u32 = 8 - ML_BITS;
const RUN_MASK: u32 = (1 << RUN_BITS) - 1;

pub const LZ4_MAX_INPUT_SIZE: u32 = 0x7E00_0000;
const LZ4_DISTANCE_MAX: usize = 65535;
const LZ4_DISTANCE_ABSOLUTE_MAX: usize = 65535;
/// lz4.c:718 — `(64 KB) + (MFLIMIT-1)`. Below this, `byU16` is used.
const LZ4_64K_LIMIT: usize = (64 * 1024) + (MFLIMIT - 1);
const LZ4_SKIP_TRIGGER: u32 = 6;
pub const LZ4_ACCELERATION_DEFAULT: i32 = 1;
pub const LZ4_ACCELERATION_MAX: i32 = 65537;

/// lz4.h:697 — `LZ4_MEMORY_USAGE - 2`, probed from the real header because the
/// suite rebuilds itself with both the MIN and MAX permitted memory usage.
const HASHLOG: u32 = LZ4_MEMORY_USAGE_PROBED as u32 - 2;
const U32_ENTRIES: usize = 1 << HASHLOG;
/// `byU16` gets one extra bit of hash (lz4.c:789, :796).
const U16_ENTRIES: usize = 1 << (HASHLOG + 1);
const ABI_STREAM_PADDING_WORDS: usize = if usize::BITS == 32 { 3 } else { 1 };

/// lz4.h:215 — exact formula; do not approximate.
///
/// The `as u32` reproduces C's unsigned comparison, which is what makes a
/// negative `isize` return 0 rather than a huge bound.
pub fn compress_bound(isize_: i32) -> i32 {
    if (isize_ as u32) > LZ4_MAX_INPUT_SIZE {
        0
    } else {
        isize_ + (isize_ / 255) + 16
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    /// The output budget was exhausted. Compressors report this as `0`.
    OutputTooSmall,
    /// Malformed input, detected after consuming `consumed` bytes of the
    /// source. C encodes the position in the return value itself:
    /// `return (int)(-(ip - src)) - 1` (lz4.c:2462), so the boundary needs the
    /// offset, not just the fact of failure.
    Malformed { consumed: usize },
}

/// Where a codec reads its input from.
///
/// `src` and `dst` may live in the **same allocation**: `fuzzer.c:1207-1218`
/// compresses in place and `:1240-1247` decompresses in place, both on
/// `fuzzer -i1`. Holding `&[u8]` and `&mut [u8]` over overlapping memory is
/// undefined behaviour — and with `lto = true` and `codegen-units = 1` it is
/// the kind that miscompiles rather than merely being wrong on paper. So the
/// overlapping case is expressed as a range within the output buffer, and no
/// aliasing pair is ever formed.
pub enum Input<'a> {
    /// A genuinely separate allocation.
    Separate(&'a [u8]),
    /// A range inside the same buffer the output is written to.
    Within(Range<usize>),
}

impl<'a> Input<'a> {
    #[inline]
    fn len(&self) -> usize {
        match self {
            Input::Separate(s) => s.len(),
            Input::Within(r) => r.end - r.start,
        }
    }

    #[inline]
    fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The whole input as one slice.
    ///
    /// `window`/`byte` re-dispatch on the enum and re-bounds-check on every
    /// access, which is invisible at a call site and ruinous inside a loop that
    /// runs once per input byte. Loops that scan take the slice once, up front.
    #[inline]
    fn as_slice<'b>(&'b self, buf: &'b [u8]) -> &'b [u8] {
        match self {
            Input::Separate(s) => s,
            Input::Within(r) => &buf[r.start..r.end],
        }
    }

    #[inline]
    fn window<'b>(&'b self, buf: &'b [u8], at: usize, n: usize) -> &'b [u8] {
        match self {
            Input::Separate(s) => &s[at..at + n],
            Input::Within(r) => &buf[r.start + at..r.start + at + n],
        }
    }

    #[inline]
    fn byte(&self, buf: &[u8], at: usize) -> u8 {
        self.window(buf, at, 1)[0]
    }

    /// **Native**-endian, matching `LZ4_read32` (lz4.c:408). Using
    /// `from_le_bytes` here would be a real bug, invisible on x86-64.
    #[inline]
    fn u32_ne(&self, buf: &[u8], at: usize) -> u32 {
        u32::from_ne_bytes(self.window(buf, at, 4).try_into().unwrap())
    }

    /// **Native**-endian, matching `LZ4_read_ARCH` (lz4.c:413).
    #[inline]
    fn u64_ne(&self, buf: &[u8], at: usize) -> u64 {
        u64::from_ne_bytes(self.window(buf, at, 8).try_into().unwrap())
    }

    /// Always little-endian, matching `LZ4_readLE16` (lz4.c:431) — the format
    /// fixes this regardless of host endianness.
    #[inline]
    fn u16_le(&self, buf: &[u8], at: usize) -> u16 {
        u16::from_le_bytes(self.window(buf, at, 2).try_into().unwrap())
    }

    /// Copy `n` input bytes to `buf[dst_at..]`. Uses `copy_within` (memmove)
    /// for the overlapping case, matching `LZ4_memmove` at lz4.c:2338.
    #[inline]
    fn copy_to(&self, buf: &mut [u8], src_at: usize, dst_at: usize, n: usize) {
        match self {
            Input::Separate(s) => buf[dst_at..dst_at + n].copy_from_slice(&s[src_at..src_at + n]),
            Input::Within(r) => buf.copy_within(r.start + src_at..r.start + src_at + n, dst_at),
        }
    }

    /// `copy_to` for call sites that have proven spare bytes on both sides:
    /// this may write up to 7 bytes past `dst_at + n` and read up to 7 past
    /// `src_at + n`, and always moves at least 8.
    ///
    /// `LZ4_wildCopy8` (lz4.c:2350). Both decode call sites sit behind the
    /// literal parsing restrictions, which reserve `MFLIMIT` (12) output bytes
    /// and `2+1+LASTLITERALS` (8) input bytes, so neither overshoot leaves the
    /// buffer. The win is not the copy itself — it is not calling `memcpy` for
    /// a handful of bytes, which is what a length-dispatched copy costs on the
    /// short literal runs that dominate real data.
    ///
    /// Long runs fall back to `copy_to`; see `WILD_COPY_CUTOFF`.
    #[inline]
    fn wild_copy_to(&self, buf: &mut [u8], src_at: usize, dst_at: usize, n: usize) {
        if n >= WILD_COPY_CUTOFF {
            self.copy_to(buf, src_at, dst_at, n);
            return;
        }
        let end = dst_at + n;
        match self {
            Input::Separate(s) => {
                let (mut d, mut k) = (dst_at, src_at);
                loop {
                    let mut word = [0u8; 8];
                    word.copy_from_slice(&s[k..k + 8]);
                    buf[d..d + 8].copy_from_slice(&word);
                    d += 8;
                    k += 8;
                    if d >= end {
                        return;
                    }
                }
            }
            Input::Within(r) => {
                let src = r.start + src_at;
                // Copying forward in 8-byte chunks reproduces `memmove` only
                // while the source is at or ahead of the destination — which is
                // exactly how in-place decompression lays the buffer out
                // (`LZ4_DECOMPRESS_INPLACE_MARGIN`, lz4.h:672). The other
                // direction would read bytes an earlier chunk had overwritten,
                // so it takes the exact path instead.
                if src < dst_at {
                    self.copy_to(buf, src_at, dst_at, n);
                    return;
                }
                let (mut d, mut k) = (dst_at, src);
                loop {
                    copy8(buf, d, k);
                    d += 8;
                    k += 8;
                    if d >= end {
                        return;
                    }
                }
            }
        }
    }
}

// ===========================================================================
// Compression
// ===========================================================================

#[derive(Clone, Copy, PartialEq, Eq)]
enum TableType {
    U16,
    U32,
}

/// `dict_directive` (lz4.c:751), minus `usingDictCtx`.
///
/// C instantiates the whole compressor once per directive via
/// `LZ4_FORCE_INLINE`; we branch at run time instead. The branches themselves
/// are transcribed verbatim, because each one changes which matches are found
/// and therefore the compressed bytes.
///
/// `usingDictCtx` is deliberately absent: it is an *optimisation* for attached
/// `LZ4F_CDict`s, and C only selects it for inputs ≤ 4 KB (lz4.c:1771-1780).
/// Above that it `memcpy`s the dictionary context into the working stream and
/// uses `usingExtDict`. Frame-level `CDict` support takes that same path
/// unconditionally — see `frame::cdict_as_ext`.
#[derive(Clone, Copy, PartialEq, Eq)]
enum DictDirective {
    /// No preceding content.
    NoDict,
    /// The dictionary immediately precedes `src` in memory: one contiguous
    /// segment, so a match may reach back past the block start.
    WithPrefix64k,
    /// The dictionary lives in a separate allocation.
    UsingExtDict,
    /// Hash candidates come from an attached dictionary stream until this
    /// stream has populated its own table (lz4.c:1071-1084).
    UsingDictCtx,
}

/// `dictIssue_directive` (lz4.c:752). `DictSmall` means the dictionary is
/// shorter than 64 KB, so table entries below `prefixIdxLimit` point at content
/// that no longer exists and must be skipped (lz4.c:1097).
#[derive(Clone, Copy, PartialEq, Eq)]
enum DictIssue {
    NoDictIssue,
    DictSmall,
}

/// The compressor's view of the bytes preceding this block, in **index space**.
///
/// C addresses history with pointers into one virtual byte stream: `base =
/// source - currentOffset`, so a table entry `matchIndex` is `base +
/// matchIndex`, which may land in the dictionary or in the current block. We
/// keep the same index space but resolve it explicitly, because the dictionary
/// is a genuinely separate slice here (it cannot be part of `buf`, which the
/// caller may also be compressing in place).
///
/// Index layout, for `start_index = currentOffset` at entry:
///
/// ```text
///   dictionary bytes            current block
///   [start_index - dict_size, start_index) [start_index, start_index + len)
/// ```
///
/// `WithPrefix64k` and `UsingExtDict` differ in C by whether those two regions
/// are physically adjacent. They are always separate slices here, and the
/// two-part match count below reproduces the contiguous count exactly, so the
/// emitted bytes are identical either way. What the directive still decides —
/// and what is therefore ported literally — is the `low_limit` used for
/// catch-up and the `DictSmall` candidate filter.
struct Hist<'a> {
    content: &'a [u8],
    start_index: u32,
    dict_size: u32,
}

impl<'a> Hist<'a> {
    /// First valid index of the dictionary region.
    #[inline]
    fn dict_base_index(&self) -> u32 {
        self.start_index - self.dict_size
    }

    /// Offset of index `mi` within `content`. Only valid when `mi <
    /// start_index`.
    #[inline]
    fn dict_at(&self, mi: u32) -> usize {
        (mi - self.dict_base_index()) as usize
    }

    /// `LZ4_read32(base + matchIndex)`.
    ///
    /// `None` where C would read past the end of the dictionary buffer. That is
    /// unreachable for tables built by this crate: `load_dict` only indexes
    /// positions up to `dictEnd - 8`, and the main loop never indexes past
    /// `mflimitPlusOne`, which is 11 bytes short of the block end — hence C's
    /// `assert(startIndex - matchIndex >= MINMATCH)` (lz4.c:1082). Treating it
    /// as "no match" keeps a corrupt state from panicking.
    #[inline]
    fn read32(&self, input: &Input, buf: &[u8], mi: u32) -> Option<u32> {
        if mi >= self.start_index {
            let at = (mi - self.start_index) as usize;
            (at + 4 <= input.len()).then(|| input.u32_ne(buf, at))
        } else {
            let at = self.dict_at(mi);
            (at + 4 <= self.content.len())
                .then(|| u32::from_ne_bytes(self.content[at..at + 4].try_into().unwrap()))
        }
    }

    /// A single byte at index `mi`, for the catch-up loop.
    #[inline]
    fn byte(&self, input: &Input, buf: &[u8], mi: u32) -> u8 {
        if mi >= self.start_index {
            input.byte(buf, (mi - self.start_index) as usize)
        } else {
            self.content[self.dict_at(mi)]
        }
    }
}

/// The hash table plus the streaming state C keeps beside it. Held across
/// blocks by `frame`, and by the block-level streaming API.
pub struct StreamState {
    table: Table,
    /// `cctx->currentOffset` (lz4.c:955). Indices in `table` are relative to
    /// this, so it must survive between blocks.
    current_offset: u32,
    /// `cctx->dictSize` — how much history precedes the current block.
    dict_size: u32,
    tt: TableType,
}

/// Exact allocation-free mirror of `LZ4_stream_t_internal`.
#[derive(Clone)]
#[repr(C)]
pub struct AbiStreamState {
    pub hash_table: [u32; U32_ENTRIES],
    pub dictionary: usize,
    pub dict_ctx: usize,
    pub current_offset: u32,
    pub table_type: u32,
    pub dict_size: u32,
    pub padding: [u32; ABI_STREAM_PADDING_WORDS],
}

/// Exact allocation-free mirror of `LZ4_streamDecode_t_internal`.
#[derive(Clone, Copy, Default)]
#[repr(C)]
pub struct AbiDecodeState {
    pub external_dict: usize,
    pub prefix_end: usize,
    pub ext_dict_size: usize,
    pub prefix_size: usize,
}

const _: () = {
    assert!(core::mem::size_of::<AbiStreamState>() == crate::types::LZ4_STREAM_SIZE);
    assert!(core::mem::align_of::<AbiStreamState>() == crate::types::LZ4_STREAM_ALIGN);
    assert!(core::mem::size_of::<AbiDecodeState>() == crate::types::LZ4_STREAMDECODE_SIZE);
    assert!(core::mem::align_of::<AbiDecodeState>() == crate::types::LZ4_STREAMDECODE_ALIGN);
};

impl StreamState {
    pub fn new() -> Self {
        StreamState {
            table: Table::new(TableType::U32),
            current_offset: 0,
            dict_size: 0,
            tt: TableType::U32,
        }
    }

    /// `LZ4_resetStream_fast` (lz4.c:1553) — drop the history but keep the
    /// table, which stays valid because indices are offset-relative.
    pub fn reset_fast(&mut self) {
        self.dict_size = 0;
        // C keeps currentOffset, so stale entries remain distinguishable.
    }

    /// `LZ4_resetStream` — a full reset, table included.
    pub fn reset(&mut self) {
        self.table.clear();
        self.current_offset = 0;
        self.dict_size = 0;
        self.tt = TableType::U32;
    }

    /// `LZ4_saveDict` (lz4.c:1823) — declare how much history is actually being
    /// retained, after the caller has copied that many trailing bytes somewhere
    /// safe.
    ///
    /// This must be called between blocks whenever the history is shortened,
    /// and getting it wrong is silent: `dict_size` is what places the dictionary
    /// in the compressor's index space, so a value larger than the retained
    /// slice makes every dictionary index point at the wrong byte. Matches are
    /// then rejected by the `read32` comparison instead of found, which costs
    /// compression ratio without ever producing invalid output — exactly the
    /// class of bug round-trip tests cannot see.
    pub fn save_dict(&mut self, retained: usize) {
        let retained = retained.min(64 * 1024) as u32;
        self.dict_size = retained.min(self.dict_size);
    }

    /// `LZ4_loadDict_internal` (lz4.c:1596). `slow` is C's `_ld_slow`, which
    /// adds a second indexing pass that favours positions near the *start* of
    /// the dictionary; `LZ4F_createCDict` uses it (lz4frame.c:569).
    pub fn load_dict(&mut self, dict: &[u8], slow: bool) -> usize {
        const HASH_UNIT: usize = 8; // sizeof(reg_t) on 64-bit
        self.reset();

        // Always advance a whole window, even for a shorter dictionary: that is
        // what lets `compress_continue` use `NoDictIssue` regardless of the
        // dictionary's real length (lz4.c:1614-1620).
        self.current_offset += 64 * 1024;

        if dict.len() < HASH_UNIT {
            return 0;
        }

        let start = dict.len().saturating_sub(64 * 1024);
        let d = &dict[start..];
        self.dict_size = d.len() as u32;
        self.tt = TableType::U32;

        let input = Input::Separate(d);
        let scratch: [u8; 0] = [];
        let mut idx32 = self.current_offset - self.dict_size;
        let mut p = 0usize;
        while p + HASH_UNIT <= d.len() {
            let h = hash_position(&input, &scratch, p, TableType::U32);
            // Overwriting => favours positions at the end of the dictionary.
            self.table.put(h, idx32);
            p += 3;
            idx32 += 3;
        }

        if slow {
            let limit = self.current_offset - 64 * 1024;
            let mut idx32 = self.current_offset - self.dict_size;
            let mut p = 0usize;
            while p + HASH_UNIT <= d.len() {
                let h = hash_position(&input, &scratch, p, TableType::U32);
                // Not overwriting => favours positions at the beginning.
                if self.table.get(h) <= limit {
                    self.table.put(h, idx32);
                }
                p += 1;
                idx32 += 1;
            }
        }

        self.dict_size as usize
    }
}

impl AbiStreamState {
    pub fn reset(&mut self) {
        self.hash_table.fill(0);
        self.dictionary = 0;
        self.dict_ctx = 0;
        self.current_offset = 0;
        self.table_type = 0;
        self.dict_size = 0;
        self.padding.fill(0);
    }

    pub fn reset_fast(&mut self) {
        if self.table_type != 0 && (self.table_type != 2 || self.current_offset > (1 << 30)) {
            self.hash_table.fill(0);
            self.current_offset = 0;
            self.table_type = 0;
        }
        if self.current_offset != 0 {
            self.current_offset = self.current_offset.wrapping_add(64 * 1024);
        }
        self.dictionary = 0;
        self.dict_ctx = 0;
        self.dict_size = 0;
    }

    pub fn renormalize(&mut self, next_size: usize) {
        if self.current_offset as usize + next_size > 0x8000_0000 {
            let delta = self.current_offset - 64 * 1024;
            let dict_end = self.dictionary.saturating_add(self.dict_size as usize);
            for entry in &mut self.hash_table {
                *entry = if *entry < delta { 0 } else { *entry - delta };
            }
            self.current_offset = 64 * 1024;
            self.dict_size = self.dict_size.min(64 * 1024);
            self.dictionary = dict_end.saturating_sub(self.dict_size as usize);
        }
    }

    pub fn load_dict(&mut self, dict: &[u8], slow: bool) -> usize {
        self.reset();
        let mut state = StreamState::new();
        let result = state.load_dict(dict, slow);
        self.store(&state);
        result
    }

    fn working(&self) -> StreamState {
        let tt = if self.table_type == 3 {
            TableType::U16
        } else {
            TableType::U32
        };
        StreamState {
            table: Table::from_abi(&self.hash_table, tt),
            current_offset: self.current_offset,
            dict_size: self.dict_size,
            tt,
        }
    }

    fn store(&mut self, state: &StreamState) {
        state.table.store_abi(&mut self.hash_table);
        self.current_offset = state.current_offset;
        self.dict_size = state.dict_size;
        self.table_type = if state.tt == TableType::U16 { 3 } else { 2 };
    }

    fn prepare(&mut self, input_size: usize, tt: TableType) {
        let table_type = if tt == TableType::U16 { 3 } else { 2 };
        if self.table_type != 0
            && (self.table_type != table_type
                || (tt == TableType::U16
                    && self.current_offset as usize + input_size >= u16::MAX as usize)
                || (tt == TableType::U32 && self.current_offset > (1 << 30))
                || input_size >= 4 * 1024)
        {
            self.hash_table.fill(0);
            self.current_offset = 0;
            self.table_type = 0;
        }
        if self.current_offset != 0 && tt == TableType::U32 {
            self.current_offset = self.current_offset.wrapping_add(64 * 1024);
        }
        self.dictionary = 0;
        self.dict_ctx = 0;
        self.dict_size = 0;
    }
}

impl Default for StreamState {
    fn default() -> Self {
        Self::new()
    }
}

/// The hash table. Element width and hash shift both follow `tableType`
/// (PORTING.md §3.3) — this is the single most output-relevant choice in the
/// compressor, so the two cases stay separate rather than being unified.
enum Table {
    U16(Box<[u16; U16_ENTRIES]>),
    U32(Box<[u32; U32_ENTRIES]>),
}

impl Table {
    fn new(tt: TableType) -> Self {
        // Boxed: LZ4_MEMORY_USAGE may be probed as high as 20, which would put
        // a 2 MB array on the stack. C puts `LZ4_stream_t` on the stack
        // (lz4.c:1469) but its callers control the size; ours is a library.
        match tt {
            TableType::U16 => Table::U16(
                vec![0u16; U16_ENTRIES]
                    .into_boxed_slice()
                    .try_into()
                    .unwrap(),
            ),
            TableType::U32 => Table::U32(
                vec![0u32; U32_ENTRIES]
                    .into_boxed_slice()
                    .try_into()
                    .unwrap(),
            ),
        }
    }

    #[inline]
    fn get(&self, h: u32) -> u32 {
        match self {
            Table::U16(t) => t[h as usize] as u32,
            Table::U32(t) => t[h as usize],
        }
    }

    #[inline]
    fn put(&mut self, h: u32, idx: u32) {
        match self {
            Table::U16(t) => t[h as usize] = idx as u16,
            Table::U32(t) => t[h as usize] = idx,
        }
    }

    fn clear(&mut self) {
        match self {
            Table::U16(t) => t.fill(0),
            Table::U32(t) => t.fill(0),
        }
    }

    fn from_abi(words: &[u32; U32_ENTRIES], tt: TableType) -> Self {
        match tt {
            TableType::U32 => Table::U32(Box::new(*words)),
            TableType::U16 => {
                let mut entries = Box::new([0u16; U16_ENTRIES]);
                for (i, word) in words.iter().enumerate() {
                    let bytes = word.to_ne_bytes();
                    entries[2 * i] = u16::from_ne_bytes([bytes[0], bytes[1]]);
                    entries[2 * i + 1] = u16::from_ne_bytes([bytes[2], bytes[3]]);
                }
                Table::U16(entries)
            }
        }
    }

    fn store_abi(&self, words: &mut [u32; U32_ENTRIES]) {
        match self {
            Table::U32(entries) => words.copy_from_slice(entries.as_slice()),
            Table::U16(entries) => {
                for (i, word) in words.iter_mut().enumerate() {
                    let lo = entries[2 * i].to_ne_bytes();
                    let hi = entries[2 * i + 1].to_ne_bytes();
                    *word = u32::from_ne_bytes([lo[0], lo[1], hi[0], hi[1]]);
                }
            }
        }
    }
}

/// lz4.c:786.
#[inline]
fn hash4(sequence: u32, tt: TableType) -> u32 {
    const PRIME: u32 = 2654435761;
    if tt == TableType::U16 {
        sequence.wrapping_mul(PRIME) >> ((MINMATCH as u32 * 8) - (HASHLOG + 1))
    } else {
        sequence.wrapping_mul(PRIME) >> ((MINMATCH as u32 * 8) - HASHLOG)
    }
}

/// lz4.c:794. Branches on host endianness internally, as C does.
#[inline]
fn hash5(sequence: u64, tt: TableType) -> u32 {
    let hash_log = if tt == TableType::U16 {
        HASHLOG + 1
    } else {
        HASHLOG
    };
    if cfg!(target_endian = "little") {
        const PRIME5BYTES: u64 = 889523592379;
        ((sequence << 24).wrapping_mul(PRIME5BYTES) >> (64 - hash_log)) as u32
    } else {
        const PRIME8BYTES: u64 = 11400714785074694791;
        ((sequence >> 24).wrapping_mul(PRIME8BYTES) >> (64 - hash_log)) as u32
    }
}

/// lz4.c:806. `byU16` is excluded from `hash5` **even on 64-bit** — writing
/// only the `hash5` path silently mis-hashes every input under 64 KB, which is
/// most of what the fuzzer feeds us, while still round-tripping.
#[inline]
fn hash_position(input: &Input, buf: &[u8], at: usize, tt: TableType) -> u32 {
    if usize::BITS == 64 && tt != TableType::U16 {
        hash5(input.u64_ne(buf, at), tt)
    } else {
        hash4(input.u32_ne(buf, at), tt)
    }
}

/// `LZ4_NbCommonBytes` (lz4.c:600) — how many leading bytes of `diff` are zero.
///
/// `diff` is the XOR of two **native-endian** word loads, so "leading" means
/// *earlier in memory*, and which end of the register that is depends on the
/// host. On a little-endian host the first byte in memory is the low byte, so
/// the scan runs from the bottom; on big-endian it runs from the top. Getting
/// this backwards is invisible on x86-64 and silently miscounts matches
/// everywhere else — the same trap as `u32_ne` above.
#[inline]
fn nb_common_bytes(diff: u64) -> usize {
    if cfg!(target_endian = "little") {
        (diff.trailing_zeros() >> 3) as usize
    } else {
        (diff.leading_zeros() >> 3) as usize
    }
}

/// Leading bytes shared by `a[i..]` and `b[j..]`, capped at `n`.
///
/// This is `LZ4_count`'s inner loop (lz4.c:689-737): compare a 64-bit word at a
/// time and locate the first differing byte with a bit scan, instead of walking
/// one byte per iteration. The count is identical either way — the word compare
/// is only a faster way to find the same first mismatch — so the parse, and
/// therefore every compressed byte, is unchanged.
///
/// C bounds only the `a` side, because its caller guarantees the `b` side is
/// backed by at least as many readable bytes. We cap against both lengths as
/// well: every caller here satisfies C's precondition, so the extra caps never
/// bind, and if one ever stopped satisfying it this returns a short count
/// rather than reading out of bounds.
#[inline]
fn common_bytes(a: &[u8], i: usize, b: &[u8], j: usize, n: usize) -> usize {
    const STEP: usize = 8;
    // The clamps below are a guard, not an expected outcome: reaching them
    // means a caller broke C's precondition, and the honest signal for that is
    // a crash, not a short count. The byte-at-a-time loop this replaced gave
    // one for free -- it indexed out of bounds and panicked. Clamping instead
    // turns the same bug into wrong output, which is strictly harder to find,
    // so assert in debug and keep the guard in release.
    //
    // Not hypothetical: the stale-`active_hist` bug fixed in the lz4hc work
    // drove `j` to ~4.29e9 here. Against the old byte loop that panicked with
    // "index out of bounds"; against this function it surfaced only as
    // `fuzzer -i60 -s9` reporting a different-sized output at cycle 54.
    debug_assert!(i <= a.len() && j <= b.len(), "match index out of range");
    let n = n
        .min(a.len().saturating_sub(i))
        .min(b.len().saturating_sub(j));
    let mut k = 0;
    while k + STEP <= n {
        let x = u64::from_ne_bytes(a[i + k..i + k + STEP].try_into().unwrap());
        let y = u64::from_ne_bytes(b[j + k..j + k + STEP].try_into().unwrap());
        if x != y {
            return k + nb_common_bytes(x ^ y);
        }
        k += STEP;
    }
    while k < n && a[i + k] == b[j + k] {
        k += 1;
    }
    k
}

/// lz4.c:689 — counts matching bytes, both sides inside the current block.
#[inline]
fn count(input: &Input, buf: &[u8], p_in: usize, p_match: usize, limit: usize) -> usize {
    let s = input.as_slice(buf);
    common_bytes(s, p_in, s, p_match, limit.saturating_sub(p_in))
}

/// `LZ4_count` where the match side is in *index* space and so may start in the
/// dictionary and continue into the current block.
///
/// In C this is the same `LZ4_count` as above: the prefix and the block are one
/// contiguous buffer, so walking off the end of one lands in the other. Here
/// they are separate slices, so the crossing is explicit. `limit` bounds the
/// *input* side, as in C.
#[inline]
fn count_hist(
    input: &Input,
    buf: &[u8],
    hist: &Hist,
    p_in: usize,
    m_idx: u32,
    limit: usize,
) -> usize {
    let s = input.as_slice(buf);
    let n = limit.saturating_sub(p_in);

    if m_idx >= hist.start_index {
        // The match is already inside the current block: one contiguous run.
        return common_bytes(s, p_in, s, (m_idx - hist.start_index) as usize, n);
    }

    // The match starts in the dictionary. It may run to the dictionary's end
    // and continue at the block's first byte — which is contiguous in C and a
    // different slice here — so count each segment separately and stop at the
    // first mismatch, exactly as walking one byte at a time would.
    let at = hist.dict_at(m_idx);
    let in_dict = (hist.start_index - m_idx) as usize;
    let head = n.min(in_dict);
    let matched = common_bytes(s, p_in, hist.content, at, head);
    if matched < head {
        return matched;
    }
    matched + common_bytes(s, p_in + matched, s, 0, n - matched)
}

/// `LZ4_compress_generic` (lz4.c:1353) + `_validated` (lz4.c:939).
///
/// `limited` selects `limitedOutput` vs `notLimited`: C picks `notLimited`
/// when `dstCapacity >= LZ4_compressBound(srcSize)` (lz4.c:1397) and then
/// skips every output bounds check, so this flag must be derived the same way.
///
/// `state` carries the table and index bookkeeping. For a one-shot compression
/// it is freshly constructed and discarded; for linked blocks the caller holds
/// it across calls, which is the whole of what makes them "linked".
#[allow(clippy::too_many_arguments)]
fn compress_generic(
    buf: &mut [u8],
    dst: Range<usize>,
    input: &Input,
    state: &mut StreamState,
    dict_content: &[u8],
    dict_context: Option<&StreamState>,
    directive: DictDirective,
    dict_issue: DictIssue,
    tt: TableType,
    limited: bool,
    fill: bool,
    consumed: &mut usize,
    acceleration: i32,
) -> Result<usize, Error> {
    let input_size = input.len();

    // lz4.c:1369-1381 — the `src == NULL, srcSize == 0` case, and the reason a
    // blanket null guard fails: empty input compresses to *one* byte.
    if input_size as u32 > LZ4_MAX_INPUT_SIZE {
        return Err(Error::OutputTooSmall);
    }
    if input_size == 0 {
        if limited && dst.is_empty() {
            return Err(Error::OutputTooSmall);
        }
        buf[dst.start] = 0;
        return Ok(1);
    }

    // lz4.c:955-1009 — the index space, then the state update that happens
    // *before* the parse and is therefore visible to the next block.
    let start_index = state.current_offset;
    let hist = Hist {
        content: dict_content,
        start_index,
        dict_size: state.dict_size,
    };
    let context_hist = dict_context.map(|context| Hist {
        content: dict_content,
        start_index,
        dict_size: context.dict_size,
    });
    // `prefixIdxLimit` (lz4.c:968): with DictSmall, candidates below this point
    // at content that is no longer present.
    let prefix_idx_limit = start_index - state.dict_size;
    let maybe_ext_mem = matches!(
        directive,
        DictDirective::UsingExtDict | DictDirective::UsingDictCtx
    );

    state.dict_size += input_size as u32;
    state.current_offset += input_size as u32;
    state.tt = tt;

    let table = &mut state.table;
    let iend = input_size;
    let mut op = dst.start;
    let olimit = dst.end;
    let mut anchor = 0usize;

    // lz4.c:1011 — too small to compress; everything becomes literals.
    if input_size < LZ4_MIN_LENGTH {
        return last_literals(
            buf, input, anchor, iend, op, olimit, limited, fill, consumed, dst.start,
        );
    }

    // Only computed past the guard above: below LZ4_MIN_LENGTH (13) both of
    // these underflow. C forms out-of-range pointers that the main loop never
    // dereferences; in Rust the subtraction itself is the bug.
    let matchlimit = iend - LASTLITERALS;
    let mflimit_plus_one = iend - MFLIMIT + 1;

    // lz4.c:1013-1020 — first byte, then advance one and prime forwardH.
    // The table stores *indices*, so the first position is `startIndex`, not 0.
    let mut ip = 0usize;
    let h = hash_position(input, buf, ip, tt);
    table.put(h, start_index);
    ip += 1;
    let mut forward_h = hash_position(input, buf, ip, tt);

    // C's `offset`, only meaningful when `maybe_extMem` (lz4.c:985). For an
    // ext-dict match `ip - match` is not the distance, because the two regions
    // are not adjacent in our address space — the index difference is.
    let mut ext_offset: u32 = 0;

    'main: loop {
        // The match position, in index space.
        let mut match_idx: u32;
        // C's `lowLimit`: `dictionary` when the match is in the ext dict,
        // `source` otherwise. Drives catch-up and the match-length count.
        let mut match_in_dict: bool;

        // --- Find a match (lz4.c:1049-1110, the byU32/byU16 arm) ---
        {
            let mut forward_ip = ip;
            let mut step = 1usize;
            let mut search_match_nb: u32 = (acceleration as u32) << LZ4_SKIP_TRIGGER;
            loop {
                let h = forward_h;
                let current = (forward_ip + start_index as usize) as u32;
                let mut mi = table.get(h);
                ip = forward_ip;
                forward_ip += step;
                // Post-increment: the *old* counter picks the step (lz4.c:1062).
                step = (search_match_nb >> LZ4_SKIP_TRIGGER) as usize;
                search_match_nb += 1;

                if forward_ip > mflimit_plus_one {
                    return last_literals(
                        buf, input, anchor, iend, op, olimit, limited, fill, consumed, dst.start,
                    );
                }

                forward_h = hash_position(input, buf, forward_ip, tt);
                table.put(h, current);

                match_in_dict = matches!(
                    directive,
                    DictDirective::UsingExtDict | DictDirective::UsingDictCtx
                ) && mi < start_index;
                if directive == DictDirective::UsingDictCtx && match_in_dict {
                    let context = dict_context.expect("dictionary context");
                    mi = context
                        .table
                        .get(h)
                        .wrapping_add(start_index.wrapping_sub(context.current_offset));
                }

                // lz4.c:1097 — a candidate pointing into content that the
                // (short) dictionary no longer covers.
                if dict_issue == DictIssue::DictSmall && mi < prefix_idx_limit {
                    continue;
                }
                // lz4.c:1099 — with LZ4_DISTANCE_MAX == LZ4_DISTANCE_ABSOLUTE_MAX
                // the whole guard is dead for byU16, so it must not be applied
                // there: a u16 table cannot express a distance that far anyway.
                if (tt != TableType::U16 || LZ4_DISTANCE_MAX < LZ4_DISTANCE_ABSOLUTE_MAX)
                    && (mi as usize) + LZ4_DISTANCE_MAX < current as usize
                {
                    continue;
                }

                // An index below the dictionary's own start is a stale entry
                // from an earlier block whose content is gone. C cannot see
                // this — its `base + matchIndex` still forms a readable
                // address inside the old buffer, and the `LZ4_DISTANCE_MAX`
                // test above is what rejects it. Here the same entry would
                // index outside `content`, so it is filtered explicitly.
                let active_hist = if directive == DictDirective::UsingDictCtx && match_in_dict {
                    context_hist.as_ref().expect("dictionary context")
                } else {
                    &hist
                };
                if mi < active_hist.dict_base_index() {
                    continue;
                }

                if active_hist.read32(input, buf, mi) == Some(input.u32_ne(buf, ip)) {
                    if maybe_ext_mem {
                        ext_offset = current - mi;
                    }
                    match_idx = mi;
                    break;
                }
            }
        }

        // --- Catch up (lz4.c:1113-1118) ---
        //
        // `lowLimit` is the dictionary start for an ext-dict match and the
        // block start otherwise, so the walk-back stops at a different place in
        // each case. It never crosses between the two regions, which is why a
        // single index comparison suffices.
        let filled_ip = ip;
        let pick_hist = |in_dict: bool| -> &Hist {
            if directive == DictDirective::UsingDictCtx && in_dict {
                context_hist.as_ref().expect("dictionary context")
            } else {
                &hist
            }
        };
        let active_hist = pick_hist(match_in_dict);
        let low_limit: u32 = if match_in_dict {
            active_hist.dict_base_index()
        } else if directive == DictDirective::WithPrefix64k {
            start_index - hist.dict_size
        } else {
            start_index
        };
        let ip_index = |ip: usize| (ip + start_index as usize) as u32;
        if match_idx > low_limit
            && input.byte(buf, ip - 1) == active_hist.byte(input, buf, match_idx - 1)
        {
            loop {
                ip -= 1;
                match_idx -= 1;
                if !(ip_index(ip) > anchor as u32 + start_index
                    && match_idx > low_limit
                    && input.byte(buf, ip - 1) == active_hist.byte(input, buf, match_idx - 1))
                {
                    break;
                }
            }
        }

        // --- Encode literals (lz4.c:1120-1145) ---
        let mut token = op;
        {
            let lit_length = ip - anchor;
            op += 1;
            if limited
                && !fill
                && op + lit_length + (2 + 1 + LASTLITERALS) + (lit_length / 255) > olimit
            {
                return Err(Error::OutputTooSmall);
            }
            if fill
                && op + (lit_length + 240) / 255 + lit_length + 2 + 1 + MFLIMIT - MINMATCH > olimit
            {
                return last_literals(
                    buf, input, anchor, iend, token, olimit, true, true, consumed, dst.start,
                );
            }
            if lit_length >= RUN_MASK as usize {
                let mut len = lit_length - RUN_MASK as usize;
                buf[token] = (RUN_MASK << ML_BITS) as u8;
                while len >= 255 {
                    buf[op] = 255;
                    op += 1;
                    len -= 255;
                }
                buf[op] = len as u8;
                op += 1;
            } else {
                buf[token] = ((lit_length as u32) << ML_BITS) as u8;
            }
            input.copy_to(buf, anchor, op, lit_length);
            op += lit_length;
        }

        // `_next_match` (lz4.c:1147). Re-entered without re-encoding literals
        // when the very next position also matches.
        loop {
            // Re-derived every iteration, not hoisted: the "test next position"
            // tail below reassigns `match_in_dict`, and under `UsingDictCtx` the
            // two arms are *different* histories. C re-points `lowLimit` at the
            // same moment (lz4.c:1272,1276) and comments that it is "required
            // for match length counter". Reading a dictCtx match against `hist`
            // — whose `dict_size` is 0 in that mode — wraps `dict_at` into a
            // ~4 G index. Reached from `fuzzer -i60 -s9`, cycle 54.
            let active_hist = pick_hist(match_in_dict);
            if fill && op + 2 + 1 + MFLIMIT - MINMATCH > olimit {
                return last_literals(
                    buf, input, anchor, iend, token, olimit, true, true, consumed, dst.start,
                );
            }
            // --- Encode offset (lz4.c:1163-1172). Always little-endian. ---
            //
            // For an ext-dict match the distance is the index difference, which
            // `ext_offset` already holds; `ip - match` would be meaningless
            // because the dictionary is a separate allocation here.
            let offset = if maybe_ext_mem {
                ext_offset as u16
            } else {
                (ip_index(ip) - match_idx) as u16
            };
            buf[op..op + 2].copy_from_slice(&offset.to_le_bytes());
            op += 2;

            // --- Encode match length (lz4.c:1174-1235) ---
            {
                let mut match_code = if match_in_dict {
                    // lz4.c:1177-1189 — an ext-dict match. The dictionary is a
                    // separate segment, so the count stops at `dictEnd` and then
                    // resumes at the *block start*, not at the following byte.
                    // One straight count would run past the dictionary into
                    // whatever follows it and emit a match C never emits.
                    let mut limit = ip + (start_index - match_idx) as usize;
                    if limit > matchlimit {
                        limit = matchlimit;
                    }
                    let mut mc = count_hist(
                        input,
                        buf,
                        active_hist,
                        ip + MINMATCH,
                        match_idx + MINMATCH as u32,
                        limit,
                    );
                    ip += mc + MINMATCH;
                    if ip == limit {
                        let more = count(input, buf, limit, 0, matchlimit);
                        mc += more;
                        ip += more;
                    }
                    mc
                } else {
                    // Prefix mode reaches here with `match_idx < start_index`:
                    // the prefix and the block are one contiguous segment in C,
                    // so the count simply walks across the boundary.
                    let mc = count_hist(
                        input,
                        buf,
                        active_hist,
                        ip + MINMATCH,
                        match_idx + MINMATCH as u32,
                        matchlimit,
                    );
                    ip += mc + MINMATCH;
                    mc
                };

                if limited && op + (1 + LASTLITERALS) + (match_code + 240) / 255 > olimit {
                    if !fill {
                        return Err(Error::OutputTooSmall);
                    }
                    let new_match_code = 14 + (olimit - op - 1 - LASTLITERALS) * 255;
                    ip -= match_code - new_match_code;
                    match_code = new_match_code;
                    if ip <= filled_ip {
                        for position in ip..=filled_ip {
                            if position + 8 <= input.len() {
                                table.put(hash_position(input, buf, position, tt), 0);
                            }
                        }
                    }
                }
                if match_code >= ML_MASK as usize {
                    buf[token] += ML_MASK as u8;
                    match_code -= ML_MASK as usize;
                    while match_code >= 255 {
                        buf[op] = 255;
                        op += 1;
                        match_code -= 255;
                    }
                    buf[op] = match_code as u8;
                    op += 1;
                } else {
                    buf[token] += match_code as u8;
                }
            }

            anchor = ip;

            // --- Test end of chunk (lz4.c:1242) ---
            if ip >= mflimit_plus_one {
                break 'main;
            }

            // --- Fill table (lz4.c:1244-1251) ---
            let h = hash_position(input, buf, ip - 2, tt);
            table.put(h, ip_index(ip - 2));

            // --- Test next position (lz4.c:1262-1303) ---
            let h = hash_position(input, buf, ip, tt);
            let current = ip_index(ip);
            let mut mi = table.get(h);
            table.put(h, current);

            let in_dict = matches!(
                directive,
                DictDirective::UsingExtDict | DictDirective::UsingDictCtx
            ) && mi < start_index;
            if directive == DictDirective::UsingDictCtx && in_dict {
                let context = dict_context.expect("dictionary context");
                mi = context
                    .table
                    .get(h)
                    .wrapping_add(start_index.wrapping_sub(context.current_offset));
            }
            let next_hist = pick_hist(in_dict);
            let near_enough =
                if tt == TableType::U16 && LZ4_DISTANCE_MAX == LZ4_DISTANCE_ABSOLUTE_MAX {
                    true
                } else {
                    (mi as usize) + LZ4_DISTANCE_MAX >= current as usize
                };
            let dict_ok = dict_issue != DictIssue::DictSmall || mi >= prefix_idx_limit;
            if dict_ok
                && near_enough
                && mi >= next_hist.dict_base_index()
                && next_hist.read32(input, buf, mi) == Some(input.u32_ne(buf, ip))
            {
                token = op;
                buf[token] = 0;
                op += 1;
                if maybe_ext_mem {
                    ext_offset = current - mi;
                }
                match_idx = mi;
                match_in_dict = in_dict;
                continue; // goto _next_match
            }

            // --- Prepare next loop (lz4.c:1307) ---
            ip += 1;
            forward_h = hash_position(input, buf, ip, tt);
            break;
        }
    }

    last_literals(
        buf, input, anchor, iend, op, olimit, limited, fill, consumed, dst.start,
    )
}

/// `_last_literals` (lz4.c:1311-1338).
#[allow(clippy::too_many_arguments)]
fn last_literals(
    buf: &mut [u8],
    input: &Input,
    anchor: usize,
    iend: usize,
    mut op: usize,
    olimit: usize,
    limited: bool,
    fill: bool,
    consumed: &mut usize,
    dst_start: usize,
) -> Result<usize, Error> {
    let mut last_run = iend - anchor;
    if limited && op + last_run + 1 + ((last_run + 255 - RUN_MASK as usize) / 255) > olimit {
        if !fill || olimit <= op {
            return Err(Error::OutputTooSmall);
        }
        last_run = olimit - op - 1;
        last_run -= (last_run + 256 - RUN_MASK as usize) / 256;
    }
    if last_run >= RUN_MASK as usize {
        let mut accumulator = last_run - RUN_MASK as usize;
        buf[op] = (RUN_MASK << ML_BITS) as u8;
        op += 1;
        while accumulator >= 255 {
            buf[op] = 255;
            op += 1;
            accumulator -= 255;
        }
        buf[op] = accumulator as u8;
        op += 1;
    } else {
        buf[op] = ((last_run as u32) << ML_BITS) as u8;
        op += 1;
    }
    input.copy_to(buf, anchor, op, last_run);
    op += last_run;
    if fill {
        *consumed = anchor + last_run;
    }
    Ok(op - dst_start)
}

/// `LZ4_compress_fast` (lz4.c:1462) — the whole one-shot compression entry.
///
/// The `tableType` choice is `lz4.c:1398-1402`. On 64-bit the `byPtr` arm is
/// unreachable: it is guarded by `sizeof(void*) == 4`.
pub fn compress_fast(
    buf: &mut [u8],
    dst: Range<usize>,
    input: &Input,
    acceleration: i32,
) -> Result<usize, Error> {
    let mut acceleration = acceleration;
    if acceleration < 1 {
        acceleration = LZ4_ACCELERATION_DEFAULT;
    }
    if acceleration > LZ4_ACCELERATION_MAX {
        acceleration = LZ4_ACCELERATION_MAX;
    }

    let src_size = input.len();
    let dst_capacity = dst.end - dst.start;
    let tt = if src_size < LZ4_64K_LIMIT {
        TableType::U16
    } else {
        TableType::U32
    };
    // lz4.c:1397 — `notLimited` when the destination provably cannot overflow.
    let limited = (dst_capacity as i64) < compress_bound(src_size as i32) as i64;

    let mut state = StreamState {
        table: Table::new(tt),
        current_offset: 0,
        dict_size: 0,
        tt,
    };
    let mut consumed = 0;
    compress_generic(
        buf,
        dst,
        input,
        &mut state,
        &[],
        None,
        DictDirective::NoDict,
        DictIssue::NoDictIssue,
        tt,
        limited,
        false,
        &mut consumed,
        acceleration,
    )
}

/// `LZ4_compress_destSize`: fill the output and report the consumed prefix.
pub fn compress_dest_size(
    buf: &mut [u8],
    dst: Range<usize>,
    input: &Input,
    acceleration: i32,
) -> Result<(usize, usize), Error> {
    if dst.is_empty() {
        return Err(Error::OutputTooSmall);
    }
    let tt = if input.len() < LZ4_64K_LIMIT {
        TableType::U16
    } else {
        TableType::U32
    };
    let mut state = StreamState {
        table: Table::new(tt),
        current_offset: 0,
        dict_size: 0,
        tt,
    };
    let mut consumed = 0;
    let written = compress_generic(
        buf,
        dst,
        input,
        &mut state,
        &[],
        None,
        DictDirective::NoDict,
        DictIssue::NoDictIssue,
        tt,
        true,
        true,
        &mut consumed,
        acceleration.clamp(LZ4_ACCELERATION_DEFAULT, LZ4_ACCELERATION_MAX),
    )?;
    Ok((written, consumed))
}

/// `LZ4_compress_fast_continue` (lz4.c:1716) — one block of a linked stream.
///
/// `dict` is the history preceding this block: the previous block's bytes, or a
/// loaded dictionary. `prefix` says whether those bytes sit immediately before
/// `src` in the caller's address space, which is what selects `withPrefix64k`
/// over `usingExtDict` in C. Both count matches across the boundary; the
/// directive additionally decides `lowLimit` and the offset encoding.
///
/// The stream's `dictSize`/`currentOffset` are updated by `compress_generic`, so
/// consecutive calls see a growing history exactly as C does.
pub fn compress_continue(
    buf: &mut [u8],
    dst: Range<usize>,
    input: &Input,
    state: &mut StreamState,
    dict: &[u8],
    prefix: bool,
    dict_context: Option<&StreamState>,
    acceleration: i32,
) -> Result<usize, Error> {
    let mut acceleration = acceleration;
    if acceleration < 1 {
        acceleration = LZ4_ACCELERATION_DEFAULT;
    }
    if acceleration > LZ4_ACCELERATION_MAX {
        acceleration = LZ4_ACCELERATION_MAX;
    }

    // lz4.c:1731-1742 — a dictionary too short to hash is dropped rather than
    // carried, so the faster prefix path can be used.
    if state.dict_size < 4 && !prefix && !input.is_empty() {
        state.dict_size = 0;
    }

    // lz4.c:1721 — streaming is always byU32, whatever the block size.
    let tt = TableType::U32;
    let dst_capacity = dst.end - dst.start;
    let _ = dst_capacity;

    let (directive, issue) = if dict_context.is_some() {
        (DictDirective::UsingDictCtx, DictIssue::NoDictIssue)
    } else if prefix {
        // lz4.c:1755-1759
        let issue =
            if (state.dict_size as usize) < 64 * 1024 && state.dict_size < state.current_offset {
                DictIssue::DictSmall
            } else {
                DictIssue::NoDictIssue
            };
        (DictDirective::WithPrefix64k, issue)
    } else {
        // lz4.c:1781-1786
        let issue =
            if (state.dict_size as usize) < 64 * 1024 && state.dict_size < state.current_offset {
                DictIssue::DictSmall
            } else {
                DictIssue::NoDictIssue
            };
        (DictDirective::UsingExtDict, issue)
    };

    // C always uses `limitedOutput` here (lz4.c:1757) — a streaming caller's
    // dstCapacity is never assumed to cover the bound.
    if directive == DictDirective::UsingDictCtx {
        state.dict_size = 0;
    }
    let mut consumed = 0;
    compress_generic(
        buf,
        dst,
        input,
        state,
        dict,
        dict_context,
        directive,
        issue,
        tt,
        true,
        false,
        &mut consumed,
        acceleration,
    )
}

/// Adapt caller-owned ABI state to the Phase 3 streaming compressor.
#[allow(clippy::too_many_arguments)]
pub fn compress_abi_continue(
    buf: &mut [u8],
    dst: Range<usize>,
    input: &Input,
    state: &mut AbiStreamState,
    dict: &[u8],
    prefix: bool,
    dict_context: Option<(&AbiStreamState, &[u8])>,
    acceleration: i32,
) -> Result<usize, Error> {
    let mut working = state.working();
    let context = dict_context.map(|(context, _)| context.working());
    let context_dict = dict_context.map_or(dict, |(_, content)| content);
    let result = compress_continue(
        buf,
        dst,
        input,
        &mut working,
        context_dict,
        prefix,
        context.as_ref(),
        acceleration,
    );
    state.store(&working);
    result
}

/// One-shot compression using caller-provided `LZ4_stream_t` storage.
pub fn compress_abi_ext_state(
    buf: &mut [u8],
    dst: Range<usize>,
    input: &Input,
    state: &mut AbiStreamState,
    acceleration: i32,
    fast_reset: bool,
) -> Result<usize, Error> {
    let tt = if input.len() < LZ4_64K_LIMIT {
        TableType::U16
    } else {
        TableType::U32
    };
    if fast_reset {
        state.prepare(input.len(), tt);
    } else {
        state.reset();
    }
    let mut working = StreamState {
        table: Table::from_abi(&state.hash_table, tt),
        current_offset: state.current_offset,
        dict_size: state.dict_size,
        tt,
    };
    let limited = dst.end - dst.start < compress_bound(input.len() as i32) as usize;
    let mut consumed = 0;
    let result = compress_generic(
        buf,
        dst,
        input,
        &mut working,
        &[],
        None,
        DictDirective::NoDict,
        DictIssue::NoDictIssue,
        tt,
        limited,
        false,
        &mut consumed,
        acceleration.clamp(LZ4_ACCELERATION_DEFAULT, LZ4_ACCELERATION_MAX),
    );
    state.store(&working);
    result
}

// ===========================================================================
// Decompression
// ===========================================================================

/// `read_variable_length` (lz4.c:1986). `None` is C's `rvl_error`.
#[inline]
fn read_variable_length(input: &Input, buf: &[u8], ip: &mut usize, ilimit: usize) -> Option<usize> {
    let mut length: usize = 0;
    if *ip >= ilimit {
        return None;
    }
    let mut s = input.byte(buf, *ip);
    *ip += 1;
    length += s as usize;
    if s != 255 {
        return Some(length);
    }
    loop {
        if *ip >= ilimit {
            return None;
        }
        s = input.byte(buf, *ip);
        *ip += 1;
        length = length.checked_add(s as usize)?;
        if s != 255 {
            break;
        }
    }
    Some(length)
}

/// `LZ4_decompress_generic` (lz4.c:2022), `safe_decode` path, `noDict`.
///
/// `partial` is C's `earlyEnd_directive`. `target_output_size` is only
/// consulted when `partial` is set.
pub fn decompress_generic(
    buf: &mut [u8],
    dst: Range<usize>,
    input: &Input,
    partial: bool,
    target_output_size: usize,
) -> Result<usize, Error> {
    decompress_dict(buf, dst, input, partial, target_output_size, &[], 0)
}

/// `LZ4_decompress_safe_usingDict` (lz4.c:2738) and the generic decoder behind
/// it, for all three dict directives.
///
/// `ext_dict` is the dictionary content when it lives in its own allocation
/// (`usingExtDict`). `prefix_size` is non-zero when the history instead sits
/// immediately *before* `dst.start` inside `buf` — the prefix case, where the
/// match simply reads earlier bytes of the same buffer.
///
/// The two are mutually exclusive, matching C's dispatch: `dictStart + dictSize
/// == dest` selects a prefix, anything else an external dictionary.
pub fn decompress_dict(
    buf: &mut [u8],
    dst: Range<usize>,
    input: &Input,
    partial: bool,
    target_output_size: usize,
    ext_dict: &[u8],
    prefix_size: usize,
) -> Result<usize, Error> {
    let src_size = input.len();
    let dst_start = dst.start;

    // C's `lowPrefix`: `dest - prefixSize`, the earliest byte a match may read
    // within the output buffer itself.
    let low_prefix = dst_start - prefix_size;
    let using_ext = !ext_dict.is_empty();
    // `dictSize` (lz4.c:2032) — the ext dict's length, or the prefix's, since
    // both are "history that precedes the block". Only the ext-dict case can
    // reach outside `buf`.
    let dict_size = ext_dict.len() + prefix_size;
    // lz4.c:2046 — with a full 64 KB of history the offset can't escape it, so
    // C skips the check entirely. Reproduced because it decides what is
    // *rejected*, and rejection parity is half of behavioural equivalence.
    let check_offset = dict_size < 64 * 1024;

    // With partial decoding the effective output end is the smaller of the
    // caller's capacity and what they asked for (lz4.c:2478-2485).
    let output_size = if partial {
        core::cmp::min(target_output_size, dst.end - dst.start)
    } else {
        dst.end - dst.start
    };
    let oend = dst_start + output_size;

    let iend = src_size;
    let mut ip = 0usize;
    let mut op = dst_start;

    // lz4.c:2063-2068 — special cases.
    if output_size == 0 {
        if partial {
            return Ok(0);
        }
        return if src_size == 1 && input.byte(buf, 0) == 0 {
            Ok(0)
        } else {
            Err(Error::Malformed { consumed: 0 })
        };
    }
    if src_size == 0 {
        return Err(Error::Malformed { consumed: 0 });
    }

    // lz4.c:2050-2051 — the shortcut's margins, as C compares them but written
    // additively.
    //
    // C forms `oend - 32` and `iend - 16` as *pointers* and tests `op <=
    // shortoend` / `ip < shortiend`. On a block smaller than the margin those
    // land before the buffer, so no `op` can satisfy the test and the shortcut
    // is simply unavailable. `saturating_sub` does not reproduce that: it
    // clamps to 0, and `op <= 0` is **true** for the first sequence of a block
    // written at offset 0 — so the shortcut ran on buffers far too small for
    // its 32-byte margin, `op` walked past `oend`, and `length = oend - op`
    // underflowed. `LZ4_decompress_safe_partial` with a small
    // `targetOutputSize` reached it: `fuzzer -i2000 -s7354` died with
    // "slice index starts at 21 but ends at 7" on a 7-byte output buffer.
    //
    // Adding to the left-hand side instead cannot underflow and needs no
    // clamp, so the comparison is exactly C's.
    const SHORT_IN_MARGIN: usize = 14 /*maxLL*/ + 2 /*offset*/;
    const SHORT_OUT_MARGIN: usize = 14 /*maxLL*/ + 18 /*maxML*/;

    loop {
        // --- token ---
        let token = input.byte(buf, ip) as u32;
        ip += 1;
        let mut length = (token >> ML_BITS) as usize;

        let offset: usize;
        let match_len: usize;

        // --- two-stage shortcut (lz4.c:2241-2272) ---
        // Entering it skips the parsing-restriction check below, which is a
        // real difference in what gets accepted, not just in speed.
        if length != RUN_MASK as usize
            && ip + SHORT_IN_MARGIN < iend
            && op + SHORT_OUT_MARGIN <= oend
        {
            // C copies a fixed 16 bytes here (lz4.c:2246); `length` is at most
            // 14 on this path, so the two 8-byte steps below cover the same
            // ground. The margins tested above reserved the room for both.
            input.wild_copy_to(buf, ip, op, length);
            op += length;
            ip += length;

            match_len = (token & ML_MASK) as usize;
            offset = input.u16_le(buf, ip) as usize;
            ip += 2;

            // Stage 2: only for matches that need no length extension and
            // cannot overlap (offset >= 8), and whose source is in the
            // contiguous history — `dict == withPrefix64k || match >= lowPrefix`
            // (lz4.c:2257-2259). With a prefix, `lowPrefix` is *below*
            // `dst.start`, so a match reaching before the block start is
            // legitimate here and this stage still applies.
            if match_len != ML_MASK as usize
                && offset >= 8
                && (prefix_size >= 64 * 1024
                    || op as isize - offset as isize >= low_prefix as isize)
            {
                // C copies a fixed **18** bytes here regardless of the match
                // length (lz4.c:2262-2264) — the largest a match can be on
                // this path — because three constant-size stores beat a copy
                // that has to branch on a length. `offset >= 8` is what makes
                // the second and third stores legal: each reads only bytes an
                // earlier store has already finalised.
                //
                // The room is guaranteed, not hoped for: entry required
                // `op + 32 <= oend`, and the literal copy above advanced `op`
                // by at most 14, so `op + 18 <= oend` holds here.
                let src = op - offset;
                copy8(buf, op, src);
                copy8(buf, op + 8, src + 8);
                let mut pair = [0u8; 2];
                pair.copy_from_slice(&buf[src + 16..src + 18]);
                buf[op + 16..op + 18].copy_from_slice(&pair);
                op += match_len + MINMATCH;
                continue;
            }
            // Stage 2 declined: fall through to the match copy with `offset`
            // and `match_len` already decoded (C's `goto _copy_match`).
        } else {
            // --- decode literal length (lz4.c:2279-2287) ---
            if length == RUN_MASK as usize {
                // `saturating_sub`, not `-`: for a block shorter than the
                // limit C forms an out-of-range pointer whose comparison still
                // yields `rvl_error`. Plain `usize` subtraction wraps to a
                // huge limit instead, reading a byte it must not (and panics
                // in debug). Saturating to 0 makes `ip >= ilimit` true, which
                // is the behaviour C ends up with.
                let addl = match read_variable_length(
                    input,
                    buf,
                    &mut ip,
                    iend.saturating_sub(RUN_MASK as usize),
                ) {
                    Some(v) => v,
                    None => return Err(Error::Malformed { consumed: ip }),
                };
                length = match length.checked_add(addl) {
                    Some(v) => v,
                    None => return Err(Error::Malformed { consumed: ip }),
                };
            }

            // --- copy literals (lz4.c:2289-2352) ---
            let mut cpy = op + length;
            // Additive, for the reason given at `SHORT_OUT_MARGIN`: C compares
            // `cpy > oend - MFLIMIT` as pointers, and on a block shorter than
            // `MFLIMIT` that bound sits before `dst`, so the test is *true* and
            // the restricted branch below runs. `oend.saturating_sub(MFLIMIT)`
            // clamps to 0 instead, and `cpy > 0` is false for a zero-length
            // literal run — which sent a 1-byte output buffer down the
            // unrestricted branch. That was harmless while the branch held an
            // exact `memcpy`; once it became a wildcopy that always moves 8
            // bytes, it wrote past a 1-byte buffer. Found by rejection-parity
            // sweep: `difftest q 1 1` on a block whose first byte was corrupted
            // to 0x00, where C returns -4 and we panicked.
            if cpy + MFLIMIT > oend || ip + length + (2 + 1 + LASTLITERALS) > iend {
                // Either the input or the output parsing restriction was hit.
                // For a well-formed full block this must be the final sequence.
                if partial {
                    if ip + length > iend {
                        length = iend - ip;
                        cpy = op + length;
                    }
                    if cpy > oend {
                        cpy = oend;
                        length = oend - op;
                    }
                } else if ip + length != iend || cpy > oend {
                    return Err(Error::Malformed { consumed: ip });
                }
                input.copy_to(buf, ip, op, length);
                ip += length;
                op += length;
                if !partial || cpy == oend || ip >= iend.saturating_sub(2) {
                    break;
                }
                // NOT `continue`. In C the `break` above sits *inside* this
                // branch (lz4.c:2346) and control otherwise falls through to
                // the offset read below — the sequence still has a match to
                // decode. Restarting the loop instead re-reads the two offset
                // bytes as a fresh token, which silently truncates the output
                // in partial mode (caught by a differential run: C returned 13
                // bytes where we returned 9).
            } else {
                // Neither parsing restriction was hit, so 12 output and 8
                // input bytes are spare — C's `LZ4_wildCopy8` arm (lz4.c:2350).
                input.wild_copy_to(buf, ip, op, length);
                ip += length;
                op = cpy;
            }

            // --- offset + match length (lz4.c:2354-2360) ---
            offset = input.u16_le(buf, ip) as usize;
            ip += 2;
            match_len = (token & ML_MASK) as usize;
        }

        // `_copy_match` (lz4.c:2362) — reached from either path above.
        let mut length = match_len;
        if length == ML_MASK as usize {
            let addl = match read_variable_length(
                input,
                buf,
                &mut ip,
                iend.saturating_sub(1 + LASTLITERALS),
            ) {
                Some(v) => v,
                None => return Err(Error::Malformed { consumed: ip }),
            };
            length = match length.checked_add(addl) {
                Some(v) => v,
                None => return Err(Error::Malformed { consumed: ip }),
            };
        }
        length += MINMATCH;

        // How far back a match may legally reach (lz4.c:2375). `low_prefix` is
        // the start of the contiguous history inside `buf`: `dst.start` with no
        // prefix, earlier when the caller placed history immediately before the
        // output. A match may additionally reach into `ext_dict`, which is a
        // separate allocation, hence the `+ dict_size` slack.
        //
        // Signed arithmetic: `op - offset` legitimately lands *before*
        // `dst.start` in both dict modes, so this cannot be done in `usize`.
        //
        // Note what is NOT rejected here: `offset == 0`. It is malformed, but
        // C does not error on it -- `match == op` passes the check above -- and
        // it is reachable from corrupt input. Rejecting it is a real
        // divergence; a differential run caught C returning 13 where we
        // returned -8.
        let match_i = op as isize - offset as isize;
        if check_offset && match_i + (dict_size as isize) < (low_prefix as isize) {
            return Err(Error::Malformed { consumed: ip });
        }

        // --- match starting within the external dictionary (lz4.c:2377-2403) ---
        if using_ext && match_i < low_prefix as isize {
            if op + length > oend.saturating_sub(LASTLITERALS) {
                if partial {
                    length = core::cmp::min(length, oend - op);
                } else {
                    return Err(Error::Malformed { consumed: ip });
                }
            }
            // Distance from the match to the end of the dictionary.
            let from_dict = (low_prefix as isize - match_i) as usize;
            if length <= from_dict {
                // Entirely inside the dictionary.
                let at = ext_dict.len() - from_dict;
                buf[op..op + length].copy_from_slice(&ext_dict[at..at + length]);
                op += length;
            } else {
                // Straddles the boundary: the tail continues at `low_prefix`.
                // lz4.c:2388-2401 — and the tail may itself overlap what this
                // copy is writing, so that half stays byte-at-a-time.
                let rest = length - from_dict;
                let at = ext_dict.len() - from_dict;
                buf[op..op + from_dict].copy_from_slice(&ext_dict[at..at + from_dict]);
                op += from_dict;
                if rest > op - low_prefix {
                    copy_match(buf, op, low_prefix, rest);
                } else {
                    buf.copy_within(low_prefix..low_prefix + rest, op);
                }
                op += rest;
            }
            continue;
        }

        let match_pos = match_i as usize;

        // --- copy match (lz4.c:2406-2453) ---
        let cpy = op + length;
        if partial && cpy > oend.saturating_sub(MATCH_SAFEGUARD_DISTANCE) {
            let mlen = core::cmp::min(length, oend - op);
            copy_match(buf, op, match_pos, mlen);
            op += mlen;
            if op == oend {
                break;
            }
            continue;
        }
        if cpy > oend.saturating_sub(LASTLITERALS) {
            // The last 5 bytes of a block must be literals — a match may not
            // reach into them. This is a *format* rule, not buffer arithmetic.
            return Err(Error::Malformed { consumed: ip });
        }
        if offset == 0 {
            // Malformed but accepted (see above). C reaches lz4.c:2425 with
            // `offset < 8`, whose first act is `LZ4_write32(op, 0)`; every
            // subsequent byte of the match is then copied from those zeros,
            // because inc32table[0] and dec64table[0] are both 0. So the whole
            // match reads back as zeros. The naive copy below would instead
            // leave the destination untouched.
            buf[op..cpy].fill(0);
        } else if cpy + MATCH_SAFEGUARD_DISTANCE <= oend {
            // C's fast arm (lz4.c:2450). The same margin it checks is what
            // makes the overshooting copy legal here.
            copy_match_wild(buf, op, match_pos, length);
        } else {
            // Too close to the end to overshoot — C drops to a byte tail at
            // lz4.c:2447 for the same reason.
            copy_match(buf, op, match_pos, length);
        }
        op = cpy;
    }

    Ok(op - dst_start)
}

/// Legacy output-size-driven decoder. The source has no declared length, so
/// the FFI layer supplies a byte reader rather than manufacturing a slice.
pub fn decompress_fast_with(
    mut byte: impl FnMut(usize) -> u8,
    output: &mut [u8],
    external: &[u8],
    prefix: &[u8],
) -> Result<usize, Error> {
    let mut ip = 0usize;
    let mut op = 0usize;
    loop {
        let token = byte(ip) as usize;
        ip += 1;
        let mut literal_length = token >> ML_BITS;
        if literal_length == RUN_MASK as usize {
            loop {
                let extension = byte(ip) as usize;
                ip += 1;
                literal_length += extension;
                if extension != 255 {
                    break;
                }
            }
        }
        if output.len() - op < literal_length {
            return Err(Error::Malformed { consumed: ip });
        }
        for index in 0..literal_length {
            output[op + index] = byte(ip + index);
        }
        ip += literal_length;
        op += literal_length;
        if output.len() - op < MFLIMIT {
            return if op == output.len() {
                Ok(ip)
            } else {
                Err(Error::Malformed { consumed: ip })
            };
        }

        let offset = u16::from_le_bytes([byte(ip), byte(ip + 1)]) as usize;
        ip += 2;
        let mut match_length = token & ML_MASK as usize;
        if match_length == ML_MASK as usize {
            loop {
                let extension = byte(ip) as usize;
                ip += 1;
                match_length += extension;
                if extension != 255 {
                    break;
                }
            }
        }
        match_length += MINMATCH;
        let history_len = external.len() + prefix.len();
        if output.len() - op < match_length || offset > history_len + op {
            return Err(Error::Malformed { consumed: ip });
        }
        if offset == 0 {
            output[op..op + match_length].fill(0);
        } else {
            for index in 0..match_length {
                let source = history_len + op - offset + index;
                output[op + index] = if source < external.len() {
                    external[source]
                } else if source < history_len {
                    prefix[source - external.len()]
                } else {
                    output[source - history_len]
                };
            }
        }
        op += match_length;
        if output.len() - op < LASTLITERALS {
            return Err(Error::Malformed { consumed: ip });
        }
    }
}

/// The match copy, in bulk.
///
/// Nothing requires `offset >= len`. When `offset < len` the source and
/// destination overlap and that overlap is load-bearing: `offset=1, len=50`
/// means "repeat the previous byte 50 times", so a plain `copy_within` — which
/// has memmove semantics — would produce the wrong bytes. That is why this was
/// originally a byte loop, and why it dominated decode time: it is the one cost
/// on the decode path that scales with match length, and it showed up as
/// `LZ4_decompress_safe` running at 0.21x of C on long-match data.
///
/// Both cases are still bulk copies, without reading a byte the byte loop would
/// not have read:
///
/// * `offset >= len` — the regions are disjoint, so a single `copy_within`.
/// * `offset < len` — the match is the `offset`-byte pattern at `match_at`
///   repeated. Materialise one period, then keep doubling the region already
///   written. Each `copy_within` reads only bytes finalised by an earlier step,
///   so no step overlaps itself and every one is an honest memmove.
///
/// C reaches the same place differently: `LZ4_wildCopy8` plus the
/// `inc32table`/`dec64table` fixups (lz4.c:490-510) copy 8 bytes at a time and
/// deliberately overwrite up to 8 bytes past the end, which the caller has
/// reserved. The doubling loop needs no such slack, so it stays inside safe
/// Rust — and the bytes are the format's, identical either way.
///
/// Isolated in one function so that dictionary support — where a match can
/// start before the output buffer and straddle the boundary (lz4.c:2384-2401)
/// — becomes a branch here rather than a change to every caller.
/// Copy a fixed 8 bytes within `buf`. The size is a constant, so this is a
/// load/store pair rather than a call into `memmove` — which is the whole point
/// (see `copy_match_wild`).
#[inline(always)]
fn copy8(buf: &mut [u8], dst_at: usize, src_at: usize) {
    let mut word = [0u8; 8];
    word.copy_from_slice(&buf[src_at..src_at + 8]);
    buf[dst_at..dst_at + 8].copy_from_slice(&word);
}

/// `inc32table` / `dec64table` (lz4.c:475-476), indexed by an offset below 8.
const INC32_TABLE: [usize; 8] = [0, 1, 2, 1, 0, 4, 4, 4];
const DEC64_TABLE: [isize; 8] = [0, 0, 0, -1, -4, 1, 2, 3];

/// Writes the first 8 bytes of a match whose offset is 1..=7, and returns the
/// source index the rest of the copy should continue from.
///
/// This is lz4.c:2425-2436. The trick is the two tables: after these 8 bytes
/// the returned source sits **at least 8 bytes** behind the write position, so
/// everything after it can proceed a word at a time without reading bytes it is
/// still writing — which a sub-8 offset otherwise forbids.
///
/// The first four bytes must go one at a time, exactly as C writes them: with
/// an offset below 4, each byte read here was written by the previous
/// iteration, and that self-reference *is* the repeat the format encodes.
/// The second four are a bulk copy, because the adjusted source is disjoint
/// from them for every offset in range.
#[inline]
fn short_offset_prologue(buf: &mut [u8], dst_at: usize, match_at: usize, offset: usize) -> usize {
    for i in 0..4 {
        buf[dst_at + i] = buf[match_at + i];
    }
    let src = match_at + INC32_TABLE[offset];
    let mut word = [0u8; 4];
    word.copy_from_slice(&buf[src..src + 4]);
    buf[dst_at + 4..dst_at + 8].copy_from_slice(&word);
    (src as isize - DEC64_TABLE[offset]) as usize
}

/// Below this length a run is copied in fixed 8-byte steps; at or above it, by
/// `memcpy`/`memmove`. Applies to both literal runs and matches.
///
/// Both directions are worth having, and the measurements say so. On the few
/// bytes a typical sequence moves, the call into `memcpy` costs more than the
/// copy itself and C's fixed-width stores win — paying for that call per
/// sequence is what left decompression at 0.2–0.4x of C. On long runs the
/// reverse holds by a wide margin: `memcpy` moves far more than 8 bytes per
/// step and amortises its dispatch. On 8 MB of zeroes — one enormous offset-1
/// match — `LZ4_decompress_safe` reaches 2.7x the C library, because C is
/// still stepping 8 bytes at a time where we hand whole megabytes to `memmove`.
///
/// The exact value is not sensitive: measured at 16, 32, 64, 128 and 512 over
/// four inputs, the spread sits inside this host's ~13% run-to-run noise.
/// Only removing the cutoff is clearly wrong — it cost about a third of the
/// decode throughput on literal-heavy data. C needs no such split, because its
/// `LZ4_wildCopy8` has no per-step bounds check to amortise.
const WILD_COPY_CUTOFF: usize = 32;

/// `copy_match` for call sites that have proven at least
/// `MATCH_SAFEGUARD_DISTANCE` writable bytes past `dst_at + len`.
///
/// It may write up to 7 bytes beyond the match — C's `LZ4_wildCopy8` bargain
/// (lz4.c:466): trading a few dead bytes for a copy loop with no length
/// dispatch and no call. Every byte that *belongs* to the match is identical to
/// what `copy_match` would have written; the slack bytes are overwritten by
/// whatever the decoder emits next.
#[inline]
fn copy_match_wild(buf: &mut [u8], dst_at: usize, match_at: usize, len: usize) {
    let offset = dst_at - match_at;
    if offset == 0 || len == 0 {
        return;
    }
    if len >= WILD_COPY_CUTOFF {
        copy_match(buf, dst_at, match_at, len);
        return;
    }
    let end = dst_at + len;
    let (mut d, mut s) = (dst_at, match_at);
    if offset < 8 {
        s = short_offset_prologue(buf, dst_at, match_at, offset);
        d = dst_at + 8;
        if d >= end {
            return;
        }
    }
    loop {
        copy8(buf, d, s);
        d += 8;
        s += 8;
        if d >= end {
            return;
        }
    }
}

#[inline]
fn copy_match(buf: &mut [u8], dst_at: usize, match_at: usize, len: usize) {
    let offset = dst_at - match_at;
    // `offset == 0` is malformed but reachable, and C does not reject it (see
    // `decompress_dict`). The byte loop assigned each byte to itself; keep that
    // exact no-op, and keep the doubling loop below from never advancing.
    if offset == 0 || len == 0 {
        return;
    }
    if offset >= len {
        buf.copy_within(match_at..match_at + len, dst_at);
        return;
    }
    buf.copy_within(match_at..match_at + offset, dst_at);
    let mut filled = offset;
    while filled < len {
        let n = core::cmp::min(filled, len - filled);
        buf.copy_within(dst_at..dst_at + n, dst_at + filled);
        filled += n;
    }
}

#[cfg(test)]
mod streaming_tests {
    use super::*;

    fn abi_state() -> AbiStreamState {
        AbiStreamState {
            hash_table: [0; U32_ENTRIES],
            dictionary: 0,
            dict_ctx: 0,
            current_offset: 0,
            table_type: 0,
            dict_size: 0,
            padding: [0; ABI_STREAM_PADDING_WORDS],
        }
    }

    #[test]
    fn abi_stream_layouts_match_c() {
        assert_eq!(
            core::mem::size_of::<AbiStreamState>(),
            crate::types::LZ4_STREAM_SIZE
        );
        assert_eq!(
            core::mem::align_of::<AbiStreamState>(),
            crate::types::LZ4_STREAM_ALIGN
        );
        assert_eq!(
            core::mem::size_of::<AbiDecodeState>(),
            crate::types::LZ4_STREAMDECODE_SIZE
        );
        assert_eq!(
            core::mem::align_of::<AbiDecodeState>(),
            crate::types::LZ4_STREAMDECODE_ALIGN
        );
    }

    #[test]
    fn abi_stream_renormalization_rescales_indices() {
        let mut state = abi_state();
        state.current_offset = 0x7fff_ffff;
        state.dictionary = 100_000;
        state.dict_size = 70_000;
        let delta = state.current_offset - 64 * 1024;
        state.hash_table[..3].copy_from_slice(&[delta - 1, delta, state.current_offset - 1]);
        state.renormalize(2);
        assert_eq!(state.current_offset, 64 * 1024);
        assert_eq!(state.dict_size, 64 * 1024);
        assert_eq!(state.dictionary, 104_464);
        assert_eq!(&state.hash_table[..3], &[0, 0, 65_535]);
    }
}
