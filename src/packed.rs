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

#[inline]
pub fn pack9(s: &[u8]) -> Option<u64> {
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
}

impl IdMap {
    pub fn with_capacity(n: usize) -> IdMap {
        IdMap { packed: crate::keys::KeyMap::with_capacity(n), other: new_map(0) }
    }
    #[inline]
    pub fn get(&self, s: &[u8]) -> Option<u32> {
        match pack9(s) {
            Some(k) if !s.is_empty() => self.packed.get_mixed(fmix(k), k).map(|v| v as u32),
            Some(_) => None,
            None => self.other.get(s).copied(),
        }
    }
    pub fn contains_key(&self, s: &[u8]) -> bool {
        self.get(s).is_some()
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
