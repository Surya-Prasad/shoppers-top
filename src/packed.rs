//! Night-05: lookup tables keyed by packed IDs.
//!
//! An ID of at most 9 bytes, each in 1..=127, packs left-aligned at 7 bits per byte into
//! a u64. Packing is injective and integer order equals byte-wise order (a shorter
//! prefix pads with 0, which sorts before every allowed byte). Lookups on packed keys
//! hash and compare one integer instead of dereferencing a byte slice in the input
//! mapping. Every other ID goes through an exact byte-string table; a packable and an
//! unpackable ID are never equal, so the two tables never disagree.

use std::collections::HashMap;
use std::hash::{BuildHasher, Hasher};

use crate::{Map, new_map};

/// Night-14: murmur3's 64-bit finalizer. It is a bijection on u64, so a table keyed by
/// `fmix(k)` is keyed exactly by `k`, and its output is already well mixed: tables keyed
/// by it use the identity hasher and partitions take bits 40.. of it.
#[inline(always)]
pub fn fmix(mut k: u64) -> u64 {
    k ^= k >> 33;
    k = k.wrapping_mul(0xff51_afd7_ed55_8ccd);
    k ^= k >> 33;
    k = k.wrapping_mul(0xc4ce_b9fe_1a85_ec53);
    k ^ (k >> 33)
}

/// Identity hasher for keys that are already `fmix`ed u64s.
#[derive(Clone, Copy, Default)]
pub struct Mixed;
pub struct MixedH(u64);
impl Hasher for MixedH {
    #[inline]
    fn write(&mut self, _: &[u8]) {
        unreachable!("Mixed hashes u64 keys only")
    }
    #[inline]
    fn write_u64(&mut self, i: u64) {
        self.0 = i;
    }
    #[inline]
    fn finish(&self) -> u64 {
        self.0
    }
}
impl BuildHasher for Mixed {
    type Hasher = MixedH;
    #[inline]
    fn build_hasher(&self) -> MixedH {
        MixedH(0)
    }
}
pub type MixedMap<V> = HashMap<u64, V, Mixed>;

type S = &'static [u8];

/// RW20: SWAR packing. The first 8 bytes are byte-swapped (first character highest) and
/// their eight 7-bit lanes compacted with three mask-and-shift steps; a 9th character goes
/// in the low 7 bits. Every real byte must be in 1..=127 (exact lane masks). Same results
/// as `pack9_ref` (tested).
#[inline(always)]
pub fn pack9(s: &[u8]) -> Option<u64> {
    let n = s.len();
    if n > 9 {
        return None;
    }
    let w = if n >= 8 {
        u64::from_le_bytes(s[..8].try_into().unwrap())
    } else {
        let mut b = [0u8; 8];
        b[..n].copy_from_slice(s);
        u64::from_le_bytes(b)
    };
    let lanes: u64 = if n >= 8 { !0 } else { (1u64 << (8 * n)) - 1 } & 0x8080_8080_8080_8080;
    const LO7: u64 = 0x7f7f_7f7f_7f7f_7f7f;
    let nonzero = ((w & LO7).wrapping_add(LO7) | w) & 0x8080_8080_8080_8080;
    // every real lane: no high bit, and non-zero
    if w & lanes != 0 || nonzero & lanes != lanes {
        return None;
    }
    let mut x = w.swap_bytes() & LO7;
    x = (x & 0x007f_007f_007f_007f) | ((x & 0x7f00_7f00_7f00_7f00) >> 1);
    x = (x & 0x0000_3fff_0000_3fff) | ((x & 0x3fff_0000_3fff_0000) >> 2);
    x = (x & 0x0000_0000_0fff_ffff) | ((x & 0x0fff_ffff_0000_0000) >> 4);
    let mut v = x << 7;
    if n == 9 {
        let c = s[8];
        if c == 0 || c > 127 {
            return None;
        }
        v |= c as u64;
    }
    Some(v)
}

/// The original byte-at-a-time packing (reference for tests).
#[cfg(test)]
pub fn pack9_ref(s: &[u8]) -> Option<u64> {
    if s.len() > 9 {
        return None;
    }
    let mut v = 0u64;
    let mut ok = true;
    for (i, &c) in s.iter().enumerate() {
        ok &= c.wrapping_sub(1) < 127;
        v |= ((c & 0x7f) as u64) << (56 - 7 * i);
    }
    ok.then_some(v)
}

/// Hash of an ID for partitioning: of its packed form when it packs, else of its bytes.
/// Equal IDs always get the same hash.
#[inline]
pub fn id_hash(st: &crate::RandomState, s: &[u8]) -> u64 {
    use std::hash::BuildHasher;
    match pack9(s) {
        Some(k) => fmix(k),
        None => st.hash_one(s),
    }
}

/// RW11: u32-valued ID map: packed IDs in a flat `KeyMap`, all others in an exact byte-string
/// table. Lookups never intern (an unknown long ID just misses).
pub struct IdMap {
    packed: crate::keys::KeyMap,
    other: Map<S, u32>,
    // v2: IDs of the canonical form `prefix` + a number without leading zeros (1..=7 digits,
    // below DMAX) live only in `dense` (value + 1 at index number; 0 = absent); every other
    // ID only in `packed`/`other`. The two key sets are disjoint, so lookups route by form.
    dprefix: u8,
    dense: Vec<u32>,
}

/// v2: numbers of canonical IDs kept in the dense table are below this.
pub const DMAX: u32 = 1 << 22;

/// v2: the number of a canonical ID `prefix` + `[1-9][0-9]{0,6}` below DMAX, else None.
#[inline]
pub fn canon(s: &[u8], prefix: u8) -> Option<u32> {
    if s.len() < 2 || s.len() > 8 || s[0] != prefix || prefix == 0 || !(b'1'..=b'9').contains(&s[1]) {
        return None;
    }
    let mut n = 0u32;
    for &c in &s[1..] {
        if !c.is_ascii_digit() {
            return None;
        }
        n = n * 10 + (c - b'0') as u32;
    }
    (n < DMAX).then_some(n)
}

impl IdMap {
    pub fn with_capacity(n: usize) -> IdMap {
        IdMap { packed: crate::keys::KeyMap::with_capacity(n), other: new_map(0), dprefix: 0, dense: Vec::new() }
    }
    /// v2: with a dense table for canonical `prefix` + number IDs.
    pub fn with_dense(n: usize, prefix: u8) -> IdMap {
        IdMap { dprefix: prefix, ..IdMap::with_capacity(n) }
    }
    pub fn dense_prefix(&self) -> u8 {
        self.dprefix
    }
    /// v2: lookup of a canonical ID by its number (`canon`); only valid for numbers below DMAX.
    #[inline(always)]
    pub fn get_dense(&self, n: u32) -> Option<u32> {
        match self.dense.get(n as usize) {
            Some(&v) if v != 0 => Some(v - 1),
            _ => None,
        }
    }
    #[inline]
    pub fn get(&self, s: &[u8]) -> Option<u32> {
        if let Some(n) = canon(s, self.dprefix) {
            return self.get_dense(n);
        }
        match pack9(s) {
            Some(k) if !s.is_empty() => self.packed.get_mixed(fmix(k), k).map(|v| v as u32),
            Some(_) => None,
            None => self.other.get(s).copied(),
        }
    }
    pub fn contains_key(&self, s: &[u8]) -> bool {
        self.get(s).is_some()
    }
    /// RW15: prefetch the slot of a packed key's mix `m`.
    #[inline(always)]
    pub fn prefetch_packed(&self, m: u64) {
        self.packed.prefetch(m);
    }
    /// RW15: lookup of a packed (non-empty) key `k` with mix `m`. v2: callers route canonical
    /// IDs (`canon`) to `get_dense` first; this table never holds them.
    #[inline(always)]
    pub fn get_packed(&self, m: u64, k: u64) -> Option<u32> {
        self.packed.get_mixed(m, k).map(|v| v as u32)
    }
    /// Value of `s`, inserting `f()` first if absent (`s` non-empty).
    #[inline]
    pub fn get_or_insert_with(&mut self, s: S, f: impl FnOnce() -> u32) -> u32 {
        match pack9(s) {
            Some(k) if !s.is_empty() => self.packed.get_or_insert_with(k, || f() as u64) as u32,
            _ => *self.other.entry(s).or_insert_with(f),
        }
    }
    /// Insert unless the key is present (first occurrence wins; `s` non-empty).
    pub fn insert_first(&mut self, s: S, v: u32) {
        if let Some(n) = canon(s, self.dprefix) {
            if self.dense.len() <= n as usize {
                self.dense.resize(n as usize + 1, 0);
            }
            if self.dense[n as usize] == 0 {
                self.dense[n as usize] = v + 1;
            }
            return;
        }
        match pack9(s) {
            Some(k) if !s.is_empty() => self.packed.insert_first(k, v as u64),
            _ => {
                self.other.entry(s).or_insert(v);
            }
        }
    }
}

pub struct PMap<V> {
    packed: MixedMap<V>, // keyed by fmix(pack9(id))
    other: Map<S, V>,
}

impl<V> PMap<V> {
    pub fn with_capacity(n: usize) -> PMap<V> {
        PMap { packed: MixedMap::with_capacity_and_hasher(n, Mixed), other: new_map(0) }
    }
    #[inline]
    pub fn get(&self, s: &[u8]) -> Option<&V> {
        match pack9(s) {
            Some(k) => self.packed.get(&fmix(k)),
            None => self.other.get(s),
        }
    }
    /// Lookup by an already mixed packed key (`fmix(pack9(id))`).
    #[inline]
    pub fn get_mixed(&self, m: u64) -> Option<&V> {
        self.packed.get(&m)
    }
    /// Lookup of an unpackable ID.
    #[inline]
    pub fn get_other(&self, s: &[u8]) -> Option<&V> {
        self.other.get(s)
    }
    pub fn contains_key(&self, s: &[u8]) -> bool {
        self.get(s).is_some()
    }
    /// Value of `s`, inserting `f()` first if absent.
    #[inline]
    pub fn get_or_insert_with(&mut self, s: S, f: impl FnOnce() -> V) -> &V {
        match pack9(s) {
            Some(k) => self.packed.entry(fmix(k)).or_insert_with(f),
            None => self.other.entry(s).or_insert_with(f),
        }
    }
    /// Insert unless the key is present (first occurrence wins).
    pub fn insert_first(&mut self, s: S, v: V) {
        match pack9(s) {
            Some(k) => {
                self.packed.entry(fmix(k)).or_insert(v);
            }
            None => {
                self.other.entry(s).or_insert(v);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn lcg(x: &mut u64) -> u64 {
        *x = x.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        *x >> 33
    }
    /// RW20: the SWAR pack9 equals the reference on random bytes and lengths 0..=12.
    #[test]
    fn swar_matches_ref() {
        let mut x = 9u64;
        let alpha: [u8; 12] = [0, 1, b'0', b'a', b'z', b'Z', b' ', b'\r', 0x7e, 0x7f, 0x80, 0xff];
        for _ in 0..500_000 {
            let len = (lcg(&mut x) % 13) as usize;
            let v: Vec<u8> = (0..len)
                .map(|_| if lcg(&mut x) % 3 == 0 { alpha[(lcg(&mut x) as usize) % 12] } else { (lcg(&mut x) % 256) as u8 })
                .collect();
            assert_eq!(pack9(&v), pack9_ref(&v), "{v:?}");
        }
        for s in [&b""[..], b"a", b"abcdefgh", b"abcdefghi", b"abcdefghij", b"\x7f\x7f\x7f\x7f\x7f\x7f\x7f\x7f\x7f", b"\x01"] {
            assert_eq!(pack9(s), pack9_ref(s), "{s:?}");
        }
    }

    /// pack9 is injective and order-preserving on its domain and rejects the rest.
    #[test]
    fn order_and_domain() {
        let mut x = 1u64;
        let alpha: &[u8] = b"\x01 !\"09AZaz_~\x7f";
        let mut v: Vec<Vec<u8>> = vec![vec![]];
        for _ in 0..50_000 {
            let len = (lcg(&mut x) % 10) as usize;
            v.push((0..len).map(|_| alpha[(lcg(&mut x) as usize) % alpha.len()]).collect());
        }
        for w in v.windows(2) {
            let (a, b) = (&w[0], &w[1]);
            assert_eq!(pack9(a).unwrap().cmp(&pack9(b).unwrap()), a.cmp(b), "{a:?} {b:?}");
            assert_eq!(pack9(a) == pack9(b), a == b);
        }
        assert!(pack9(b"a").unwrap() < pack9(b"a\x01").unwrap());
        assert_eq!(pack9(b"abcdefghi").map(|p| p >> 63), Some(0));
        assert_eq!(pack9(b"abcdefghij"), None);
        assert_eq!(pack9(b"a\0"), None);
        assert_eq!(pack9(b"a\x80"), None);
        assert_eq!(pack9("é".as_bytes()), None);
    }
    /// v2: the dense table routes by form and stays exact against a plain map.
    #[test]
    fn idmap_dense_exact() {
        let mut x = 4u64;
        let mut m = IdMap::with_dense(16, b'p');
        let mut want: HashMap<Vec<u8>, u32> = HashMap::new();
        let forms = |x: &mut u64| -> Vec<u8> {
            let n = lcg(x) % 5_000_000;
            match lcg(x) % 9 {
                0 => format!("p0{n}"),
                1 => format!("P{n}"),
                2 => format!("p{n} "),
                3 => format!("p{}", n * 1000),
                4 => format!("q{n}"),
                5 => "p".to_string(),
                6 => format!("p{n}x"),
                _ => format!("p{}", n % 60_000),
            }
            .into_bytes()
        };
        for i in 0..200_000u32 {
            let k = forms(&mut x);
            let ks: S = Box::leak(k.clone().into_boxed_slice());
            m.insert_first(ks, i);
            want.entry(k).or_insert(i);
        }
        for _ in 0..200_000 {
            let k = forms(&mut x);
            assert_eq!(m.get(&k), want.get(&k).copied(), "{:?}", String::from_utf8_lossy(&k));
        }
        for (k, v) in &want {
            assert_eq!(m.get(k), Some(*v));
        }
        for (s, c) in [(&b"p1"[..], Some(1)), (b"p0", None), (b"p01", None), (b"p4194303", Some(4_194_303)), (b"p4194304", None), (b"p1234567", Some(1_234_567)), (b"p12345678", None)] {
            assert_eq!(canon(s, b'p'), c, "{:?}", s);
        }
    }

    /// Exact semantics: case and space variants, long and non-ASCII keys, first wins.
    #[test]
    fn pmap_exact() {
        let mut m: PMap<u32> = PMap::with_capacity(8);
        let keys: [&'static [u8]; 6] = [b"p1", b"P1", b" p1", b"p1 ", b"product_long_id_123", "pé".as_bytes()];
        for (i, k) in keys.iter().enumerate() {
            m.insert_first(k, i as u32);
        }
        m.insert_first(b"p1", 99);
        m.insert_first(b"product_long_id_123", 99);
        for (i, k) in keys.iter().enumerate() {
            assert_eq!(m.get(k), Some(&(i as u32)));
        }
        for k in [&b"p2"[..], b"p", b"p1\r", b"product_long_id_12", b""] {
            assert_eq!(m.get(k), None);
        }
    }
    /// Night-14: fmix is a bijection on a sample (no collisions) and its bits 40..46 and
    /// low bits spread packed user-like IDs evenly.
    #[test]
    fn fmix_spread() {
        let mut seen = std::collections::HashSet::new();
        let (mut hi, mut lo) = ([0u32; 64], [0u32; 1024]);
        for i in 0..640_000u32 {
            let m = fmix(pack9(format!("u{i:x}").as_bytes()).unwrap());
            assert!(seen.insert(m));
            hi[(m >> 40) as usize & 63] += 1;
            lo[m as usize & 1023] += 1;
        }
        let (a, b) = (*hi.iter().min().unwrap(), *hi.iter().max().unwrap());
        assert!(a > 9_500 && b < 10_500, "{a} {b}");
        let (a, b) = (*lo.iter().min().unwrap(), *lo.iter().max().unwrap());
        assert!(a > 450 && b < 800, "{a} {b}");
        // top 7 bits (hashbrown's tag) are used too
        let mut tag = [0u32; 128];
        for i in 0..128_000u32 {
            tag[(fmix(pack9(format!("p{i}").as_bytes()).unwrap()) >> 57) as usize] += 1;
        }
        assert!(*tag.iter().min().unwrap() > 800 && *tag.iter().max().unwrap() < 1200);
    }
    /// Table hash spreads packed keys (hasher used on u64 keys).
    #[test]
    fn packed_spread() {
        use std::hash::BuildHasher;
        let st = crate::table_state();
        let mut b = [0u32; 512];
        for i in 0..512_000u32 {
            b[(st.hash_one(pack9(format!("p{i}").as_bytes()).unwrap()) >> 50) as usize & 511] += 1;
        }
        let (mn, mx) = (*b.iter().min().unwrap(), *b.iter().max().unwrap());
        assert!(mn > 800 && mx < 1200, "{mn} {mx}");
    }
}
