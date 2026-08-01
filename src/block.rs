//! block: `lib/lz4.c` — the core block codec, in safe Rust on slices.
//!
//! Entry points live in `crate::ffi`. Nothing here may use `unsafe`; pointer
//! handling stays in the FFI shim so the port's unsafe surface is small and
//! countable.
//!
//! ## Two deliberate departures from the C, neither of which changes a byte
//!
//! 1. **No wildcopy.** `LZ4_wildCopy8`/`32` (lz4.c:466, :531) overwrite past
//!    the logical end of the data because the caller reserved slack. In Rust
//!    that is a panic, so we write exactly the bytes that belong there. The
//!    limit *constants* and every comparison against them are ported verbatim,
//!    because those decide the parse and therefore the output.
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
}

// ===========================================================================
// Compression
// ===========================================================================

#[derive(Clone, Copy, PartialEq, Eq)]
enum TableType {
    U16,
    U32,
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
            TableType::U16 => Table::U16(vec![0u16; U16_ENTRIES].into_boxed_slice().try_into().unwrap()),
            TableType::U32 => Table::U32(vec![0u32; U32_ENTRIES].into_boxed_slice().try_into().unwrap()),
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
    let hash_log = if tt == TableType::U16 { HASHLOG + 1 } else { HASHLOG };
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

/// lz4.c:689 — counts matching bytes. C reads a word at a time; the naive loop
/// produces the identical count.
#[inline]
fn count(input: &Input, buf: &[u8], mut p_in: usize, mut p_match: usize, limit: usize) -> usize {
    let start = p_in;
    while p_in < limit && input.byte(buf, p_in) == input.byte(buf, p_match) {
        p_in += 1;
        p_match += 1;
    }
    p_in - start
}

/// `LZ4_compress_generic` (lz4.c:1353) + `_validated` (lz4.c:939), for
/// `noDict`/`noDictIssue` — the one-shot, non-streaming case.
///
/// `limited` selects `limitedOutput` vs `notLimited`: C picks `notLimited`
/// when `dstCapacity >= LZ4_compressBound(srcSize)` (lz4.c:1397) and then
/// skips every output bounds check, so this flag must be derived the same way.
fn compress_generic(
    buf: &mut [u8],
    dst: Range<usize>,
    input: &Input,
    tt: TableType,
    limited: bool,
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

    let mut table = Table::new(tt);

    let iend = input_size;
    let mut op = dst.start;
    let olimit = dst.end;
    let mut anchor = 0usize;

    // lz4.c:1011 — too small to compress; everything becomes literals.
    if input_size < LZ4_MIN_LENGTH {
        return last_literals(buf, input, anchor, iend, op, olimit, limited, dst.start);
    }

    // Only computed past the guard above: below LZ4_MIN_LENGTH (13) both of
    // these underflow. C forms out-of-range pointers that the main loop never
    // dereferences; in Rust the subtraction itself is the bug.
    let matchlimit = iend - LASTLITERALS;
    let mflimit_plus_one = iend - MFLIMIT + 1;

    // lz4.c:1013-1020 — first byte, then advance one and prime forwardH.
    let mut ip = 0usize;
    let h = hash_position(input, buf, ip, tt);
    table.put(h, 0);
    ip += 1;
    let mut forward_h = hash_position(input, buf, ip, tt);

    'main: loop {
        let mut match_idx: usize;

        // --- Find a match (lz4.c:1049-1110, the byU32/byU16 arm) ---
        {
            let mut forward_ip = ip;
            let mut step = 1usize;
            let mut search_match_nb: u32 = (acceleration as u32) << LZ4_SKIP_TRIGGER;
            loop {
                let h = forward_h;
                let current = forward_ip;
                let mi = table.get(h) as usize;
                ip = forward_ip;
                forward_ip += step;
                // Post-increment: the *old* counter picks the step (lz4.c:1062).
                step = (search_match_nb >> LZ4_SKIP_TRIGGER) as usize;
                search_match_nb += 1;

                if forward_ip > mflimit_plus_one {
                    return last_literals(buf, input, anchor, iend, op, olimit, limited, dst.start);
                }

                forward_h = hash_position(input, buf, forward_ip, tt);
                table.put(h, current as u32);

                // lz4.c:1099 — with LZ4_DISTANCE_MAX == LZ4_DISTANCE_ABSOLUTE_MAX
                // the whole guard is dead for byU16, so it must not be applied
                // there: a u16 table cannot express a distance that far anyway.
                if (tt != TableType::U16 || LZ4_DISTANCE_MAX < LZ4_DISTANCE_ABSOLUTE_MAX)
                    && mi + LZ4_DISTANCE_MAX < current
                {
                    continue;
                }

                if input.u32_ne(buf, mi) == input.u32_ne(buf, ip) {
                    match_idx = mi;
                    break;
                }
            }
        }

        // --- Catch up (lz4.c:1113-1118) ---
        // `lowLimit` is the start of the input in the noDict case.
        if match_idx > 0 && input.byte(buf, ip - 1) == input.byte(buf, match_idx - 1) {
            loop {
                ip -= 1;
                match_idx -= 1;
                if !(ip > anchor
                    && match_idx > 0
                    && input.byte(buf, ip - 1) == input.byte(buf, match_idx - 1))
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
                && op + lit_length + (2 + 1 + LASTLITERALS) + (lit_length / 255) > olimit
            {
                return Err(Error::OutputTooSmall);
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
            // --- Encode offset (lz4.c:1163-1172). Always little-endian. ---
            let offset = (ip - match_idx) as u16;
            buf[op..op + 2].copy_from_slice(&offset.to_le_bytes());
            op += 2;

            // --- Encode match length (lz4.c:1174-1235) ---
            {
                let mut match_code =
                    count(input, buf, ip + MINMATCH, match_idx + MINMATCH, matchlimit);
                ip += match_code + MINMATCH;

                if limited && op + (1 + LASTLITERALS) + (match_code + 240) / 255 > olimit {
                    return Err(Error::OutputTooSmall);
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
            table.put(h, (ip - 2) as u32);

            // --- Test next position (lz4.c:1262-1303) ---
            let h = hash_position(input, buf, ip, tt);
            let current = ip;
            let mi = table.get(h) as usize;
            table.put(h, current as u32);

            let near_enough = if tt == TableType::U16 && LZ4_DISTANCE_MAX == LZ4_DISTANCE_ABSOLUTE_MAX
            {
                true
            } else {
                mi + LZ4_DISTANCE_MAX >= current
            };
            if near_enough && input.u32_ne(buf, mi) == input.u32_ne(buf, ip) {
                token = op;
                buf[token] = 0;
                op += 1;
                match_idx = mi;
                continue; // goto _next_match
            }

            // --- Prepare next loop (lz4.c:1307) ---
            ip += 1;
            forward_h = hash_position(input, buf, ip, tt);
            break;
        }
    }

    last_literals(buf, input, anchor, iend, op, olimit, limited, dst.start)
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
    dst_start: usize,
) -> Result<usize, Error> {
    let last_run = iend - anchor;
    if limited && op + last_run + 1 + ((last_run + 255 - RUN_MASK as usize) / 255) > olimit {
        return Err(Error::OutputTooSmall);
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

    compress_generic(buf, dst, input, tt, limited, acceleration)
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
    let src_size = input.len();
    let dst_start = dst.start;

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

    // lz4.c:2050-2051 — the shortcut's margins.
    let shortiend = iend.saturating_sub(14 /*maxLL*/ + 2 /*offset*/);
    let shortoend = oend.saturating_sub(14 /*maxLL*/ + 18 /*maxML*/);

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
        if length != RUN_MASK as usize && ip < shortiend && op <= shortoend {
            input.copy_to(buf, ip, op, length);
            op += length;
            ip += length;

            match_len = (token & ML_MASK) as usize;
            offset = input.u16_le(buf, ip) as usize;
            ip += 2;

            // Stage 2: only for matches that need no length extension and
            // cannot overlap (offset >= 8), and that stay inside the output.
            if match_len != ML_MASK as usize && offset >= 8 && offset <= op - dst_start {
                copy_match(buf, op, op - offset, match_len + MINMATCH);
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
            if cpy > oend.saturating_sub(MFLIMIT)
                || ip + length > iend.saturating_sub(2 + 1 + LASTLITERALS)
            {
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
                input.copy_to(buf, ip, op, length);
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

        // The match may not reach behind the start of the output (lz4.c:2375,
        // `match + dictSize < lowPrefix` with dictSize == 0).
        //
        // Note what is NOT rejected here: `offset == 0`. It is malformed, but
        // C does not error on it -- `match == op` passes the check above -- and
        // it is reachable from corrupt input. Rejecting it is a real
        // divergence; a differential run caught C returning 13 where we
        // returned -8.
        if offset > op - dst_start {
            return Err(Error::Malformed { consumed: ip });
        }
        let match_pos = op - offset;

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
        } else {
            copy_match(buf, op, match_pos, length);
        }
        op = cpy;
    }

    Ok(op - dst_start)
}

/// The overlapping match copy — **byte at a time, on purpose**.
///
/// Nothing requires `offset >= length`. When `offset < length` the source and
/// destination overlap and that overlap is load-bearing: `offset=1, length=50`
/// means "repeat the previous byte 50 times", so the copy must read bytes it
/// is writing during the copy. `copy_from_slice`/`copy_within` have memcpy or
/// memmove semantics and would produce the wrong bytes here.
///
/// Isolated in one function so that dictionary support — where a match can
/// start before the output buffer and straddle the boundary (lz4.c:2384-2401)
/// — becomes a branch here rather than a change to every caller.
#[inline]
fn copy_match(buf: &mut [u8], dst_at: usize, match_at: usize, len: usize) {
    for i in 0..len {
        buf[dst_at + i] = buf[match_at + i];
    }
}
