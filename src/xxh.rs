//! xxh: safe-Rust implementation. Entry points live in `crate::ffi`.
//!
//! Nothing here may use `unsafe`; pointer handling stays in the FFI shim so
//! the port's unsafe surface is small and countable.
#![forbid(unsafe_code)]

// ─── Constants ───────────────────────────────────────────────────────────────

const PRIME32_1: u32 = 2654435761;
const PRIME32_2: u32 = 2246822519;
const PRIME32_3: u32 = 3266489917;
const PRIME32_4: u32 = 668265263;
const PRIME32_5: u32 = 374761393;

const PRIME64_1: u64 = 11400714785074694791;
const PRIME64_2: u64 = 14029467366897019727;
const PRIME64_3: u64 = 1609587929392839161;
const PRIME64_4: u64 = 9650029242287828579;
const PRIME64_5: u64 = 2870177450012600261;

pub const VERSION_NUMBER: u32 = 605; // 0 *100*100 + 6*100 + 5

// ─── State structs ────────────────────────────────────────────────────────────
//
// Layout matches the C structs in xxhash.h exactly (repr(C), same field order
// and types).  ffi.rs casts *mut XXH32_state_t (opaque storage) to
// *mut Xxh32State; the build.rs assertions confirm the sizes agree.
//
// mem32/mem64 are used as byte buffers in C via (BYTE*)state->mem32; we model
// them as [u8; N] to avoid unsafe byte-reinterpretation here.  The repr(C)
// layout is identical on little-endian because:
//   - offset of the field is the same (all preceding fields are 4-/8-byte
//     aligned and the byte-array has align 1, so no extra padding is inserted)
//   - total struct size and alignment are unchanged (trailing fields are
//     already aligned without the array contributing to alignment).

#[repr(C)]
#[derive(Clone, Copy)]
pub(crate) struct Xxh32State {
    pub total_len_32: u32,
    pub large_len: u32,
    pub v1: u32,
    pub v2: u32,
    pub v3: u32,
    pub v4: u32,
    pub mem: [u8; 16], // was uint32_t mem32[4] in C
    pub memsize: u32,
    pub reserved: u32, // never read or written per C comment
}

#[repr(C)]
#[derive(Clone, Copy)]
pub(crate) struct Xxh64State {
    pub total_len: u64,
    pub v1: u64,
    pub v2: u64,
    pub v3: u64,
    pub v4: u64,
    pub mem: [u8; 32], // was uint64_t mem64[4] in C
    pub memsize: u32,
    pub reserved: [u32; 2], // never read or written per C comment
}

// ─── XXH32 core ──────────────────────────────────────────────────────────────

#[inline(always)]
fn xxh32_round(mut seed: u32, input: u32) -> u32 {
    seed = seed.wrapping_add(input.wrapping_mul(PRIME32_2));
    seed = seed.rotate_left(13);
    seed.wrapping_mul(PRIME32_1)
}

#[inline(always)]
fn xxh32_avalanche(mut h: u32) -> u32 {
    h ^= h >> 15;
    h = h.wrapping_mul(PRIME32_2);
    h ^= h >> 13;
    h = h.wrapping_mul(PRIME32_3);
    h ^= h >> 16;
    h
}

// data must be exactly `data.len() & 15` bytes (the tail, 0-15 bytes).
fn xxh32_finalize(mut h: u32, data: &[u8]) -> u32 {
    let n = data.len() & 15;
    let mut p = &data[..n];

    // n/4 four-byte rounds followed by n%4 one-byte rounds.
    for _ in 0..(n / 4) {
        let v = u32::from_le_bytes(p[..4].try_into().unwrap());
        h = h.wrapping_add(v.wrapping_mul(PRIME32_3));
        h = h.rotate_left(17).wrapping_mul(PRIME32_4);
        p = &p[4..];
    }
    for _ in 0..(n % 4) {
        h = h.wrapping_add((p[0] as u32).wrapping_mul(PRIME32_5));
        h = h.rotate_left(11).wrapping_mul(PRIME32_1);
        p = &p[1..];
    }
    xxh32_avalanche(h)
}

// ─── XXH32 public API ─────────────────────────────────────────────────────────

pub(crate) fn xxh32(data: &[u8], seed: u32) -> u32 {
    let mut p = data;
    let mut h: u32;

    if data.len() >= 16 {
        let mut v1 = seed.wrapping_add(PRIME32_1).wrapping_add(PRIME32_2);
        let mut v2 = seed.wrapping_add(PRIME32_2);
        let mut v3 = seed;
        let mut v4 = seed.wrapping_sub(PRIME32_1);

        while p.len() >= 16 {
            v1 = xxh32_round(v1, u32::from_le_bytes(p[0..4].try_into().unwrap()));
            v2 = xxh32_round(v2, u32::from_le_bytes(p[4..8].try_into().unwrap()));
            v3 = xxh32_round(v3, u32::from_le_bytes(p[8..12].try_into().unwrap()));
            v4 = xxh32_round(v4, u32::from_le_bytes(p[12..16].try_into().unwrap()));
            p = &p[16..];
        }

        h = v1
            .rotate_left(1)
            .wrapping_add(v2.rotate_left(7))
            .wrapping_add(v3.rotate_left(12))
            .wrapping_add(v4.rotate_left(18));
    } else {
        h = seed.wrapping_add(PRIME32_5);
    }

    h = h.wrapping_add(data.len() as u32);
    xxh32_finalize(h, p)
}

pub(crate) fn xxh32_reset(s: &mut Xxh32State, seed: u32) {
    s.total_len_32 = 0;
    s.large_len = 0;
    s.v1 = seed.wrapping_add(PRIME32_1).wrapping_add(PRIME32_2);
    s.v2 = seed.wrapping_add(PRIME32_2);
    s.v3 = seed;
    s.v4 = seed.wrapping_sub(PRIME32_1);
    s.mem = [0; 16];
    s.memsize = 0;
    // reserved intentionally left untouched
}

pub(crate) fn xxh32_update(s: &mut Xxh32State, input: &[u8]) {
    let len = input.len();
    s.total_len_32 = s.total_len_32.wrapping_add(len as u32);
    s.large_len |= ((len >= 16) as u32) | ((s.total_len_32 >= 16) as u32);

    let ms = s.memsize as usize;

    if ms + len < 16 {
        s.mem[ms..ms + len].copy_from_slice(input);
        s.memsize += len as u32;
        return;
    }

    let mut p = input;

    if ms > 0 {
        let fill = 16 - ms;
        s.mem[ms..16].copy_from_slice(&p[..fill]);
        s.v1 = xxh32_round(s.v1, u32::from_le_bytes(s.mem[0..4].try_into().unwrap()));
        s.v2 = xxh32_round(s.v2, u32::from_le_bytes(s.mem[4..8].try_into().unwrap()));
        s.v3 = xxh32_round(s.v3, u32::from_le_bytes(s.mem[8..12].try_into().unwrap()));
        s.v4 = xxh32_round(s.v4, u32::from_le_bytes(s.mem[12..16].try_into().unwrap()));
        p = &p[fill..];
        s.memsize = 0;
    }

    while p.len() >= 16 {
        s.v1 = xxh32_round(s.v1, u32::from_le_bytes(p[0..4].try_into().unwrap()));
        s.v2 = xxh32_round(s.v2, u32::from_le_bytes(p[4..8].try_into().unwrap()));
        s.v3 = xxh32_round(s.v3, u32::from_le_bytes(p[8..12].try_into().unwrap()));
        s.v4 = xxh32_round(s.v4, u32::from_le_bytes(p[12..16].try_into().unwrap()));
        p = &p[16..];
    }

    if !p.is_empty() {
        s.mem[..p.len()].copy_from_slice(p);
        s.memsize = p.len() as u32;
    }
}

pub(crate) fn xxh32_digest(s: &Xxh32State) -> u32 {
    let mut h: u32 = if s.large_len != 0 {
        s.v1.rotate_left(1)
            .wrapping_add(s.v2.rotate_left(7))
            .wrapping_add(s.v3.rotate_left(12))
            .wrapping_add(s.v4.rotate_left(18))
    } else {
        s.v3.wrapping_add(PRIME32_5)
    };
    h = h.wrapping_add(s.total_len_32);
    let ms = s.memsize as usize;
    xxh32_finalize(h, &s.mem[..ms])
}

pub(crate) fn xxh32_canonical_from_hash(hash: u32) -> [u8; 4] {
    hash.to_be_bytes()
}

pub(crate) fn xxh32_hash_from_canonical(src: &[u8; 4]) -> u32 {
    u32::from_be_bytes(*src)
}

// ─── XXH64 core ──────────────────────────────────────────────────────────────

#[inline(always)]
fn xxh64_round(mut acc: u64, input: u64) -> u64 {
    acc = acc.wrapping_add(input.wrapping_mul(PRIME64_2));
    acc = acc.rotate_left(31);
    acc.wrapping_mul(PRIME64_1)
}

#[inline(always)]
fn xxh64_merge_round(acc: u64, val: u64) -> u64 {
    let val = xxh64_round(0, val);
    let acc = acc ^ val;
    acc.wrapping_mul(PRIME64_1).wrapping_add(PRIME64_4)
}

#[inline(always)]
fn xxh64_avalanche(mut h: u64) -> u64 {
    h ^= h >> 33;
    h = h.wrapping_mul(PRIME64_2);
    h ^= h >> 29;
    h = h.wrapping_mul(PRIME64_3);
    h ^= h >> 32;
    h
}

// data is the tail, len & 31 bytes.
fn xxh64_finalize(mut h: u64, data: &[u8]) -> u64 {
    let n = data.len() & 31;
    let mut p = &data[..n];

    // n/8 eight-byte rounds, optional four-byte round, then n%4 one-byte rounds.
    for _ in 0..(n / 8) {
        let k1 = xxh64_round(0, u64::from_le_bytes(p[..8].try_into().unwrap()));
        h ^= k1;
        h = h
            .rotate_left(27)
            .wrapping_mul(PRIME64_1)
            .wrapping_add(PRIME64_4);
        p = &p[8..];
    }
    if n % 8 >= 4 {
        h ^= (u32::from_le_bytes(p[..4].try_into().unwrap()) as u64).wrapping_mul(PRIME64_1);
        h = h
            .rotate_left(23)
            .wrapping_mul(PRIME64_2)
            .wrapping_add(PRIME64_3);
        p = &p[4..];
    }
    for _ in 0..(n % 4) {
        h ^= (p[0] as u64).wrapping_mul(PRIME64_5);
        h = h.rotate_left(11).wrapping_mul(PRIME64_1);
        p = &p[1..];
    }
    xxh64_avalanche(h)
}

// ─── XXH64 public API ─────────────────────────────────────────────────────────

pub(crate) fn xxh64(data: &[u8], seed: u64) -> u64 {
    let mut p = data;
    let mut h: u64;

    if data.len() >= 32 {
        let mut v1 = seed.wrapping_add(PRIME64_1).wrapping_add(PRIME64_2);
        let mut v2 = seed.wrapping_add(PRIME64_2);
        let mut v3 = seed;
        let mut v4 = seed.wrapping_sub(PRIME64_1);

        while p.len() >= 32 {
            v1 = xxh64_round(v1, u64::from_le_bytes(p[0..8].try_into().unwrap()));
            v2 = xxh64_round(v2, u64::from_le_bytes(p[8..16].try_into().unwrap()));
            v3 = xxh64_round(v3, u64::from_le_bytes(p[16..24].try_into().unwrap()));
            v4 = xxh64_round(v4, u64::from_le_bytes(p[24..32].try_into().unwrap()));
            p = &p[32..];
        }

        h = v1
            .rotate_left(1)
            .wrapping_add(v2.rotate_left(7))
            .wrapping_add(v3.rotate_left(12))
            .wrapping_add(v4.rotate_left(18));
        h = xxh64_merge_round(h, v1);
        h = xxh64_merge_round(h, v2);
        h = xxh64_merge_round(h, v3);
        h = xxh64_merge_round(h, v4);
    } else {
        h = seed.wrapping_add(PRIME64_5);
    }

    h = h.wrapping_add(data.len() as u64);
    xxh64_finalize(h, p)
}

pub(crate) fn xxh64_reset(s: &mut Xxh64State, seed: u64) {
    s.total_len = 0;
    s.v1 = seed.wrapping_add(PRIME64_1).wrapping_add(PRIME64_2);
    s.v2 = seed.wrapping_add(PRIME64_2);
    s.v3 = seed;
    s.v4 = seed.wrapping_sub(PRIME64_1);
    s.mem = [0; 32];
    s.memsize = 0;
    // Quirk, preserved deliberately: C's XXH64_reset copies
    // `sizeof(state) - sizeof(state.reserved)` = 80 bytes, but `reserved`
    // starts at offset 76 — so reserved[0] IS zeroed (despite the "do not
    // write into reserved" comment) while reserved[1] is not. XXH32_reset's
    // arithmetic lands exactly on its reserved field, which stays untouched.
    s.reserved[0] = 0;
}

pub(crate) fn xxh64_update(s: &mut Xxh64State, input: &[u8]) {
    let len = input.len();
    s.total_len = s.total_len.wrapping_add(len as u64);

    let ms = s.memsize as usize;

    if ms + len < 32 {
        s.mem[ms..ms + len].copy_from_slice(input);
        s.memsize += len as u32;
        return;
    }

    let mut p = input;

    if ms > 0 {
        let fill = 32 - ms;
        s.mem[ms..32].copy_from_slice(&p[..fill]);
        s.v1 = xxh64_round(s.v1, u64::from_le_bytes(s.mem[0..8].try_into().unwrap()));
        s.v2 = xxh64_round(s.v2, u64::from_le_bytes(s.mem[8..16].try_into().unwrap()));
        s.v3 = xxh64_round(s.v3, u64::from_le_bytes(s.mem[16..24].try_into().unwrap()));
        s.v4 = xxh64_round(s.v4, u64::from_le_bytes(s.mem[24..32].try_into().unwrap()));
        p = &p[fill..];
        s.memsize = 0;
    }

    while p.len() >= 32 {
        s.v1 = xxh64_round(s.v1, u64::from_le_bytes(p[0..8].try_into().unwrap()));
        s.v2 = xxh64_round(s.v2, u64::from_le_bytes(p[8..16].try_into().unwrap()));
        s.v3 = xxh64_round(s.v3, u64::from_le_bytes(p[16..24].try_into().unwrap()));
        s.v4 = xxh64_round(s.v4, u64::from_le_bytes(p[24..32].try_into().unwrap()));
        p = &p[32..];
    }

    if !p.is_empty() {
        s.mem[..p.len()].copy_from_slice(p);
        s.memsize = p.len() as u32;
    }
}

pub(crate) fn xxh64_digest(s: &Xxh64State) -> u64 {
    let mut h: u64 = if s.total_len >= 32 {
        let mut h =
            s.v1.rotate_left(1)
                .wrapping_add(s.v2.rotate_left(7))
                .wrapping_add(s.v3.rotate_left(12))
                .wrapping_add(s.v4.rotate_left(18));
        h = xxh64_merge_round(h, s.v1);
        h = xxh64_merge_round(h, s.v2);
        h = xxh64_merge_round(h, s.v3);
        h = xxh64_merge_round(h, s.v4);
        h
    } else {
        s.v3.wrapping_add(PRIME64_5)
    };
    h = h.wrapping_add(s.total_len);
    let ms = s.memsize as usize;
    xxh64_finalize(h, &s.mem[..ms])
}

pub(crate) fn xxh64_canonical_from_hash(hash: u64) -> [u8; 8] {
    hash.to_be_bytes()
}

pub(crate) fn xxh64_hash_from_canonical(src: &[u8; 8]) -> u64 {
    u64::from_be_bytes(*src)
}
