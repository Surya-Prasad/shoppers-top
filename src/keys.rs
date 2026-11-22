//! RW1: every ID (event, click, user, person name) is one `u64` key from scan to output.
//!
//! * An ID of 1..=9 bytes, each byte in 1..=127, packs left-aligned at 7 bits per byte
//!   (`packed::pack9`). Packing is injective and integer order equals byte-wise order.
//!   Packed keys are below 2^63 and never 0 (the first byte is non-zero).
//! * Any other non-empty ID is interned exactly (byte equality) in a global table and
//!   gets `INTERNED | index`. Interned keys are only ever compared for equality, except
//!   for click-ID tie-breaks, which use `cmp_bytes` (byte order, exact).
//! * Key 0 means "empty".
//!
//! Generated data never needs the interner (every ID packs), so its lock is cold.

use std::sync::{Mutex, OnceLock};

use crate::packed::{fmix, pack9};
use crate::{Map, new_map};

type S = &'static [u8];

pub const INTERNED: u64 = 1 << 63;
/// v3: pointer keys, INTERNED | PTR_BIT | len << 47 | address: an ID kept as its location in
/// a still-mapped input file, for IDs that only need equality by bytes (`same_bytes`) or byte
/// order (`cmp_bytes`), never global identity: event IDs (dedup) and click IDs (tie-breaks).
/// Two pointer keys of equal IDs differ; compare them with `same_bytes`.
pub const PTR_BIT: u64 = 1 << 62;
const PTR_ADDR: u64 = (1 << 47) - 1;

/// The pointer key of `s` (callers keep its mapping alive while the key is used), or None
/// when the address or length does not fit (the caller then uses `key_of`).
#[inline]
pub fn ptr_key(s: S) -> Option<u64> {
    let a = s.as_ptr() as u64;
    (a <= PTR_ADDR && s.len() < (1 << 14) && !s.is_empty()).then_some(INTERNED | PTR_BIT | (s.len() as u64) << 47 | a)
}

#[inline(always)]
pub fn is_ptr(k: u64) -> bool {
    k & (INTERNED | PTR_BIT) == INTERNED | PTR_BIT
}

#[inline(always)]
fn ptr_bytes(k: u64) -> S {
    // SAFETY: ptr_key built k from a live 'static slice; its mapping is kept while k is used
    unsafe { std::slice::from_raw_parts((k & PTR_ADDR) as *const u8, ((k >> 47) & ((1 << 14) - 1)) as usize) }
}

/// v3: the key of an ID that needs only byte equality/order: packed when it packs, a pointer
/// key when it is longer than 9 bytes, else interned.
#[inline]
pub fn local_key(s: S) -> u64 {
    if s.is_empty() {
        return EMPTY;
    }
    match pack9(s) {
        Some(k) => k,
        None if s.len() > 9 => ptr_key(s).unwrap_or_else(|| intern(s)),
        None => intern(s),
    }
}

/// v3: a hash of the ID behind a key that equal IDs share whatever their key kind: the key
/// itself for packed/interned keys, a hash of the bytes for pointer keys (an ID has one kind:
/// longer than 9 bytes always gives a pointer key, unless it did not fit one, then interned).
#[inline]
pub fn id_hash_key(k: u64) -> u64 {
    if is_ptr(k) { byte_hash(ptr_bytes(k)) | 1 } else { k }
}

/// v3: equal IDs (any key kinds produced by `local_key` for the same input).
#[inline]
pub fn same_id(a: u64, b: u64) -> bool {
    a == b || (is_ptr(a) && is_ptr(b) && ptr_bytes(a) == ptr_bytes(b))
}
pub const EMPTY: u64 = 0;

// v3: the interner is sharded (no global lock) and arena-backed (no allocation per ID).
// Generated data since the 2026-11-22 generator update has 1 in 8 user, event and click IDs
// of 33 bytes (a prefix and 32 hex digits), so interning is hot: millions of lookups.
const SHARD_BITS: u32 = 10;
const SHARDS: usize = 1 << SHARD_BITS;
const BLOCK: usize = 1 << 20;

struct Shard {
    // v3: open addressing over 24-byte slots; a lookup touches one slot line (and, for
    // non-hex IDs, the stored bytes once). Hex-shaped IDs (one byte + 32 lowercase hex digits,
    // the generator's long IDs): (hi, lo, HEX | prefix << 32 | index + 1), matched by value.
    // Other IDs: (hash, bytes address, len << 32 | index + 1). c == 0 marks a free slot.
    slots: Vec<(u64, u64, u64)>,
    bytes: Vec<S>, // index -> bytes (in a leaked arena block), for bytes_of
    block: &'static mut [u8], // free tail of the current arena block
}

const HEX: u64 = 1 << 63;

impl Shard {
    fn new() -> Shard {
        Shard { slots: vec![(0, 0, 0); 16], bytes: Vec::new(), block: &mut [] }
    }
    fn copy(&mut self, s: S) -> S {
        if s.len() > BLOCK / 4 {
            return Box::leak(s.to_vec().into_boxed_slice());
        }
        if s.len() > self.block.len() {
            self.block = Box::leak(vec![0u8; BLOCK].into_boxed_slice());
        }
        let (dst, rest) = std::mem::take(&mut self.block).split_at_mut(s.len());
        dst.copy_from_slice(s);
        self.block = rest;
        dst
    }
    #[inline]
    fn slot_hash(e: &(u64, u64, u64)) -> u64 {
        if e.2 & HEX != 0 { hex_hash(e.0, e.1) } else { e.0 }
    }
    fn grow(&mut self) {
        let cap = self.slots.len() * 2;
        let old = std::mem::replace(&mut self.slots, vec![(0, 0, 0); cap]);
        for e in old {
            if e.2 != 0 {
                let mut j = (Self::slot_hash(&e) as usize) & (cap - 1);
                while self.slots[j].2 != 0 {
                    j = (j + 1) & (cap - 1);
                }
                self.slots[j] = e;
            }
        }
    }
    fn insert_at(&mut self, j: usize, s: S, mk: impl FnOnce(S, u64) -> (u64, u64, u64)) -> u32 {
        let idx = self.bytes.len() as u32;
        let c = self.copy(s);
        self.bytes.push(c);
        self.slots[j] = mk(c, idx as u64 + 1);
        if self.bytes.len() * 2 > self.slots.len() {
            self.grow();
        }
        idx
    }
    /// The index of the hex-shaped ID `s` = (prefix, hi, lo), inserting a copy if absent.
    fn get_or_insert_hex(&mut self, s: S, prefix: u8, hi: u64, lo: u64) -> u32 {
        let mask = self.slots.len() - 1;
        let mut j = (hex_hash(hi, lo) as usize) & mask;
        let tag = HEX | (prefix as u64) << 32;
        loop {
            let e = self.slots[j];
            if e.2 == 0 {
                break;
            }
            if e.0 == hi && e.1 == lo && e.2 & !0xffff_ffff == tag {
                return (e.2 as u32) - 1;
            }
            j = (j + 1) & mask;
        }
        self.insert_at(j, s, |_, i| (hi, lo, tag | i))
    }
    /// The index of `s` (hash `h`), inserting a copy if absent.
    fn get_or_insert(&mut self, s: S, h: u64) -> u32 {
        let mask = self.slots.len() - 1;
        let mut j = (h as usize) & mask;
        loop {
            let e = self.slots[j];
            if e.2 == 0 {
                break;
            }
            if e.2 & HEX == 0 && e.0 == h && (e.2 >> 32) as usize == s.len() {
                // SAFETY: e.1 is the address of a leaked arena copy of length e.2 >> 32
                let b = unsafe { std::slice::from_raw_parts(e.1 as *const u8, s.len()) };
                if b == s {
                    return (e.2 as u32) - 1;
                }
            }
            j = (j + 1) & mask;
        }
        self.insert_at(j, s, |c, i| (h, c.as_ptr() as u64, (c.len() as u64) << 32 | i))
    }
}

#[inline]
fn hex_hash(hi: u64, lo: u64) -> u64 {
    crate::packed::fmix(hi ^ lo.rotate_left(17))
}

/// `s` as (prefix, hi, lo) when it is one byte + 32 lowercase hex digits.
#[inline]
fn hex_shape(s: &[u8]) -> Option<(u8, u64, u64)> {
    if s.len() != 33 {
        return None;
    }
    #[cfg(all(target_arch = "x86_64", target_feature = "avx2"))]
    {
        // SAFETY: s has 33 bytes, so the 32-byte load at s[1] is in bounds
        unsafe { hex_shape_avx2(s) }
    }
    #[cfg(not(all(target_arch = "x86_64", target_feature = "avx2")))]
    hex_shape_scalar(s)
}

/// v3f: AVX2 form of `hex_shape_scalar` (same result; tested): nibbles by range checks,
/// pairs joined by one multiply-add, bytes packed and read big-endian.
#[cfg(all(target_arch = "x86_64", target_feature = "avx2"))]
#[inline(always)]
unsafe fn hex_shape_avx2(s: &[u8]) -> Option<(u8, u64, u64)> {
    use std::arch::x86_64::*;
    unsafe {
        let v = _mm256_loadu_si256(s.as_ptr().add(1) as *const __m256i);
        let d = _mm256_sub_epi8(v, _mm256_set1_epi8(b'0' as i8));
        let a = _mm256_sub_epi8(v, _mm256_set1_epi8(b'a' as i8));
        let is_d = _mm256_cmpeq_epi8(_mm256_min_epu8(d, _mm256_set1_epi8(9)), d);
        let is_a = _mm256_cmpeq_epi8(_mm256_min_epu8(a, _mm256_set1_epi8(5)), a);
        if _mm256_movemask_epi8(_mm256_or_si256(is_d, is_a)) != -1 {
            return None;
        }
        let nib = _mm256_blendv_epi8(_mm256_add_epi8(a, _mm256_set1_epi8(10)), d, is_d);
        // each 16-bit lane: first nibble * 16 + second nibble
        let pairs = _mm256_maddubs_epi16(nib, _mm256_set1_epi16(0x0110));
        let packed = _mm256_packus_epi16(pairs, pairs);
        let hi = _mm256_extract_epi64::<0>(packed) as u64;
        let lo = _mm256_extract_epi64::<2>(packed) as u64;
        Some((s[0], hi.swap_bytes(), lo.swap_bytes()))
    }
}

#[cfg_attr(all(target_arch = "x86_64", target_feature = "avx2"), allow(dead_code))]
#[inline]
fn hex_shape_scalar(s: &[u8]) -> Option<(u8, u64, u64)> {
    if s.len() != 33 {
        return None;
    }
    // branch-free: a table maps each byte to its nibble or 0x10 (not lowercase hex)
    static HEXV: [u8; 256] = {
        let mut t = [0x10u8; 256];
        let mut i = 0;
        while i < 10 {
            t[b'0' as usize + i] = i as u8;
            i += 1;
        }
        let mut i = 0;
        while i < 6 {
            t[b'a' as usize + i] = 10 + i as u8;
            i += 1;
        }
        t
    };
    let mut v = [0u64; 2];
    let mut bad = 0u8;
    for (w, part) in v.iter_mut().zip(s[1..].chunks_exact(16)) {
        let mut x = 0u64;
        for &c in part {
            let d = HEXV[c as usize];
            bad |= d;
            x = x << 4 | (d & 0xf) as u64;
        }
        *w = x;
    }
    (bad & 0x10 == 0).then_some((s[0], v[0], v[1]))
}

fn shards() -> &'static [Mutex<Shard>] {
    static I: OnceLock<Vec<Mutex<Shard>>> = OnceLock::new();
    I.get_or_init(|| (0..SHARDS).map(|_| Mutex::new(Shard::new())).collect())
}

/// Byte hash for the interner (shard = top bits, slot = low bits).
#[inline]
fn byte_hash(s: &[u8]) -> u64 {
    use std::hash::BuildHasher;
    crate::hashing::Seeded(0x2545_F491_4F6C_DD1D).hash_one(s)
}

/// The key of a non-empty ID (EMPTY for an empty one).
#[inline]
pub fn key_of(s: S) -> u64 {
    if s.is_empty() {
        return EMPTY;
    }
    match pack9(s) {
        Some(k) => k,
        None => intern(s),
    }
}

/// Key of an unpackable ID: INTERNED | shard << 32 | index in the shard.
#[cold]
pub fn intern(s: S) -> u64 {
    if let Some((prefix, hi, lo)) = hex_shape(s) {
        let sh = (crate::packed::fmix(hex_hash(hi, lo) ^ prefix as u64) >> (64 - SHARD_BITS)) as usize;
        let idx = shards()[sh].lock().unwrap().get_or_insert_hex(s, prefix, hi, lo);
        return INTERNED | (sh as u64) << 32 | idx as u64;
    }
    let h = byte_hash(s);
    let sh = (h >> (64 - SHARD_BITS)) as usize;
    let idx = shards()[sh].lock().unwrap().get_or_insert(s, h);
    INTERNED | (sh as u64) << 32 | idx as u64
}

/// The bytes of a key, into `buf` for packed keys.
pub fn bytes_of(k: u64, buf: &mut [u8; 9]) -> &[u8] {
    if is_ptr(k) {
        return ptr_bytes(k);
    }
    if k & INTERNED != 0 {
        let sh = ((k >> 32) as usize) & (SHARDS - 1);
        return shards()[sh].lock().unwrap().bytes[(k & 0xffff_ffff) as usize];
    }
    let mut n = 0;
    while n < 9 {
        let c = ((k >> (56 - 7 * n)) & 0x7f) as u8;
        if c == 0 {
            break;
        }
        buf[n] = c;
        n += 1;
    }
    &buf[..n]
}

/// Byte-wise order of the IDs behind two keys (packed keys compare as integers).
#[inline]
pub fn cmp_bytes(a: u64, b: u64) -> std::cmp::Ordering {
    if (a | b) & INTERNED == 0 {
        return a.cmp(&b);
    }
    let (mut x, mut y) = ([0u8; 9], [0u8; 9]);
    // copy out of the interner before comparing (bytes_of may hold its lock)
    let ax: Vec<u8> = bytes_of(a, &mut x).to_vec();
    let by: Vec<u8> = bytes_of(b, &mut y).to_vec();
    ax.cmp(&by)
}

/// Partition of a key: the top `bits` of its mix. Uses bits disjoint from `KeyMap`
/// slots (low bits of the same mix) so a partition's table is not clustered.
#[inline(always)]
pub fn part(k: u64, bits: u32) -> usize {
    (fmix(k) >> (64 - bits)) as usize
}

/// Software prefetch (a hint; never faults). x86-64: PREFETCHT0; aarch64: PRFM PLDL1KEEP.
#[inline(always)]
pub fn prefetch_read(p: *const u8) {
    #[cfg(target_arch = "x86_64")]
    unsafe {
        core::arch::x86_64::_mm_prefetch::<{ core::arch::x86_64::_MM_HINT_T0 }>(p as *const i8);
    }
    #[cfg(target_arch = "aarch64")]
    unsafe {
        core::arch::asm!("prfm pldl1keep, [{0}]", in(reg) p, options(nostack, readonly, preserves_flags));
    }
    #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
    let _ = p;
}

/// Flat open-addressing map u64 -> u64 (key EMPTY marks a free slot). One 16-byte slot
/// per probe, so a hit usually costs a single cache line.
pub struct KeyMap {
    slots: Vec<(u64, u64)>,
    mask: usize,
    len: usize,
}

impl KeyMap {
    pub fn with_capacity(n: usize) -> KeyMap {
        let cap = (n * 2).next_power_of_two().max(16);
        KeyMap { slots: vec![(EMPTY, 0); cap], mask: cap - 1, len: 0 }
    }
    pub fn len(&self) -> usize {
        self.len
    }
    /// RW11: double the table when it would pass half full (it never fills up).
    fn grow_if_needed(&mut self) {
        if (self.len + 1) * 2 <= self.slots.len() {
            return;
        }
        let bigger = vec![(EMPTY, 0); self.slots.len() * 2];
        let old = std::mem::replace(&mut self.slots, bigger);
        self.mask = self.slots.len() - 1;
        for (k, v) in old {
            if k != EMPTY {
                let mut i = fmix(k) as usize & self.mask;
                while self.slots[i].0 != EMPTY {
                    i = (i + 1) & self.mask;
                }
                self.slots[i] = (k, v);
            }
        }
    }
    /// The value of `k`, inserting `f()` first if absent.
    pub fn get_or_insert_with(&mut self, k: u64, f: impl FnOnce() -> u64) -> u64 {
        debug_assert!(k != EMPTY);
        self.grow_if_needed();
        let mut i = fmix(k) as usize & self.mask;
        loop {
            let s = &mut self.slots[i];
            if s.0 == EMPTY {
                let v = f();
                *s = (k, v);
                self.len += 1;
                return v;
            }
            if s.0 == k {
                return s.1;
            }
            i = (i + 1) & self.mask;
        }
    }
    /// Insert unless present (first value wins).
    pub fn insert_first(&mut self, k: u64, v: u64) {
        debug_assert!(k != EMPTY);
        self.grow_if_needed();
        let mut i = fmix(k) as usize & self.mask;
        loop {
            let s = &mut self.slots[i];
            if s.0 == EMPTY {
                *s = (k, v);
                self.len += 1;
                return;
            }
            if s.0 == k {
                return;
            }
            i = (i + 1) & self.mask;
        }
    }
    /// Hint the CPU to start loading the slot a lookup of mix `m` will read first.
    #[inline(always)]
    pub fn prefetch(&self, m: u64) {
        prefetch_read(self.slots.as_ptr().wrapping_add(m as usize & self.mask) as *const u8);
    }
    /// Lookup with the key's mix already computed.
    #[inline(always)]
    pub fn get_mixed(&self, m: u64, k: u64) -> Option<u64> {
        let mut i = m as usize & self.mask;
        loop {
            let s = self.slots[i];
            if s.0 == k {
                return Some(s.1);
            }
            if s.0 == EMPTY {
                return None;
            }
            i = (i + 1) & self.mask;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn leak(s: &str) -> S {
        Box::leak(s.as_bytes().to_vec().into_boxed_slice())
    }

    #[test]
    fn keys_roundtrip_and_order() {
        let ids = ["u1", "U1", " u1", "u1 ", "u01", "u1\r", "a\tb", "abcdefghi", "abcdefghij", "pé", "x"];
        let keys: Vec<u64> = ids.iter().map(|s| key_of(leak(s))).collect();
        for (i, a) in ids.iter().enumerate() {
            let mut b = [0u8; 9];
            assert_eq!(bytes_of(keys[i], &mut b), a.as_bytes(), "{a:?}");
            for (j, c) in ids.iter().enumerate() {
                assert_eq!(keys[i] == keys[j], a == c);
                assert_eq!(cmp_bytes(keys[i], keys[j]), a.as_bytes().cmp(c.as_bytes()), "{a:?} {c:?}");
            }
        }
        assert_eq!(key_of(b""), EMPTY);
        assert!(keys.iter().all(|&k| k != EMPTY));
        // the same long ID interns to the same key
        assert_eq!(key_of(leak("abcdefghij")), key_of(leak("abcdefghij")));
    }

    /// v3: the sharded interner from many threads: equal keys iff equal bytes, bytes round-trip,
    /// byte order via cmp_bytes, for 33-byte hex IDs, case variants, long and odd IDs.
    #[test]
    fn sharded_interner_exact_concurrent() {
        let ids: Vec<S> = (0..60_000u64)
            .map(|i| {
                let s = match i % 5 {
                    0 => format!("u{:016x}{:016x}", i.wrapping_mul(0x9E37_79B9_7F4A_7C15), i),
                    1 => format!("U{:016x}{:016x}", i.wrapping_mul(0x9E37_79B9_7F4A_7C15), i),
                    2 => format!("e{:032x}", i * 7),
                    3 => format!("id-{i}-é"),
                    _ => format!("k{}", i % 500), // packable, repeats
                };
                leak(&s)
            })
            .collect();
        let keys: Vec<Vec<u64>> = std::thread::scope(|sc| {
            let hs: Vec<_> = (0..8)
                .map(|t| {
                    let ids = &ids;
                    sc.spawn(move || {
                        // every thread interns all IDs, in a different order
                        let mut out = vec![0u64; ids.len()];
                        for j in 0..ids.len() {
                            let i = (j + t * 7919) % ids.len();
                            out[i] = key_of(ids[i]);
                        }
                        out
                    })
                })
                .collect();
            hs.into_iter().map(|h| h.join().unwrap()).collect()
        });
        for t in 1..8 {
            assert_eq!(keys[t], keys[0]);
        }
        let mut by_key: std::collections::HashMap<u64, S> = std::collections::HashMap::new();
        for (i, &k) in keys[0].iter().enumerate() {
            let mut b = [0u8; 9];
            assert_eq!(bytes_of(k, &mut b), ids[i]);
            if let Some(prev) = by_key.insert(k, ids[i]) {
                assert_eq!(prev, ids[i], "two IDs share a key");
            }
        }
        for w in keys[0].windows(2).take(5_000) {
            let (mut a, mut b) = ([0u8; 9], [0u8; 9]);
            let (x, y) = (bytes_of(w[0], &mut a).to_vec(), bytes_of(w[1], &mut b).to_vec());
            assert_eq!(cmp_bytes(w[0], w[1]), x.cmp(&y));
        }
    }

    #[test]
    fn keymap_basic_and_spread() {
        let mut m = KeyMap::with_capacity(100_000);
        for i in 1..=100_000u64 {
            m.insert_first(pack9(format!("u{i:x}").as_bytes()).unwrap(), i);
        }
        m.insert_first(pack9(b"u1").unwrap(), 999); // first wins
        // growth from a tiny table keeps every entry
        let mut g = KeyMap::with_capacity(1);
        for i in 1..=50_000u64 {
            assert_eq!(g.get_or_insert_with(pack9(format!("p{i}").as_bytes()).unwrap(), || i), i);
        }
        for i in 1..=50_000u64 {
            let k = pack9(format!("p{i}").as_bytes()).unwrap();
            assert_eq!(g.get_mixed(fmix(k), k), Some(i));
            assert_eq!(g.get_or_insert_with(k, || 0), i);
        }
        assert_eq!(m.len(), 100_000);
        for i in 1..=100_000u64 {
            let k = pack9(format!("u{i:x}").as_bytes()).unwrap();
            assert_eq!(m.get_mixed(fmix(k), k), Some(i));
        }
        let k = pack9(b"nope").unwrap();
        assert_eq!(m.get_mixed(fmix(k), k), None);
        // partitions spread evenly
        let mut b = [0u32; 256];
        for i in 0..512_000u32 {
            b[part(pack9(format!("e{i:x}").as_bytes()).unwrap(), 8)] += 1;
        }
        let (mn, mx) = (*b.iter().min().unwrap(), *b.iter().max().unwrap());
        assert!(mn > 1_700 && mx < 2_300, "{mn} {mx}");
    }
}

#[cfg(test)]
mod ptr_tests {
    use super::*;
    /// v3: pointer keys: equal bytes in different buffers are the same ID with the same hash
    /// key; different bytes are different; byte order and bytes_of follow the bytes.
    #[test]
    fn ptr_keys_exact() {
        let mk = |s: &str| -> S { Box::leak(s.as_bytes().to_vec().into_boxed_slice()) };
        let ids = ["e0123456789abcdef0123456789abcdef", "e0123456789abcdef0123456789abcdee", "E0123456789ABCDEF0123456789ABCDEF", "k_long_click_id_1", "short"];
        for a in ids {
            for b in ids {
                let (ka, kb) = (local_key(mk(a)), local_key(mk(b)));
                assert_eq!(same_id(ka, kb), a == b, "{a} {b}");
                if a == b {
                    assert_eq!(id_hash_key(ka), id_hash_key(kb));
                }
                assert_eq!(cmp_bytes(ka, kb), a.as_bytes().cmp(b.as_bytes()), "{a} {b}");
                let mut buf = [0u8; 9];
                assert_eq!(bytes_of(ka, &mut buf), a.as_bytes());
            }
        }
        assert!(is_ptr(local_key(mk(ids[0]))) && !is_ptr(local_key(mk("short"))));
        assert_ne!(id_hash_key(local_key(mk(ids[0]))), EMPTY);
    }
}

#[cfg(test)]
mod hex_tests {
    #[test]
    fn hex_shape_matches_scalar() {
        let mut x = 0x9E37_79B9_7F4A_7C15u64;
        let mut next = || { x ^= x << 13; x ^= x >> 7; x ^= x << 17; x };
        let alpha = b"0123456789abcdef";
        for i in 0..200_000 {
            let mut s = [0u8; 33];
            s[0] = (next() % 256) as u8;
            for c in s[1..].iter_mut() { *c = alpha[(next() % 16) as usize]; }
            if i % 2 == 1 {
                // one or more arbitrary bytes anywhere (all 256 values over the run)
                for _ in 0..(1 + next() % 3) { let k = 1 + (next() % 32) as usize; s[k] = (next() % 256) as u8; }
            }
            assert_eq!(super::hex_shape(&s), super::hex_shape_scalar(&s), "{:?}", s);
        }
        // every byte value at every position
        for k in 1..33 {
            for b in 0..=255u8 {
                let mut s = *b"u0123456789abcdef0123456789abcdef";
                s[k] = b;
                assert_eq!(super::hex_shape(&s), super::hex_shape_scalar(&s));
            }
        }
        assert_eq!(super::hex_shape(b"u0123456789abcdef0123456789abcdef"), Some((b'u', 0x0123_4567_89ab_cdef, 0x0123_4567_89ab_cdef)));
    }
}

#[cfg(test)]
mod intern_bench {
    #[test]
    #[ignore]
    fn intern_speed() {
        let ids: Vec<super::S> = (0..200_000u64).map(|i| &*Box::leak(format!("u{:016x}{:016x}", i.wrapping_mul(0x9E3779B97F4A7C15), i ^ 0x55).into_bytes().into_boxed_slice())).collect();
        let t = std::time::Instant::now();
        let mut acc = 0u64;
        for s in &ids { acc ^= super::intern(s); }
        eprintln!("insert {:.0} ns/op", t.elapsed().as_nanos() as f64 / ids.len() as f64);
        let t = std::time::Instant::now();
        for _ in 0..10 { for s in &ids { acc ^= super::intern(s); } }
        eprintln!("lookup {:.0} ns/op ({acc})", t.elapsed().as_nanos() as f64 / (10 * ids.len()) as f64);
        let t = std::time::Instant::now();
        for _ in 0..10 { for s in &ids { acc ^= super::byte_hash(s); } }
        eprintln!("hash {:.0} ns/op ({acc})", t.elapsed().as_nanos() as f64 / (10 * ids.len()) as f64);
        let t = std::time::Instant::now();
        for _ in 0..10 { for s in &ids { let h = super::byte_hash(s); let g = super::shards()[(h >> 54) as usize].lock().unwrap(); acc ^= g.bytes.len() as u64; } }
        eprintln!("hash+lock {:.0} ns/op ({acc})", t.elapsed().as_nanos() as f64 / (10 * ids.len()) as f64);
    }
}
