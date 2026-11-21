//! Night-05: lookup tables keyed by packed IDs.
//!
//! An ID of at most 9 bytes, each in 1..=127, packs left-aligned at 7 bits per byte into
//! a u64. Packing is injective and integer order equals byte-wise order (a shorter
//! prefix pads with 0, which sorts before every allowed byte). Lookups on packed keys
//! hash and compare one integer instead of dereferencing a byte slice in the input
//! mapping. Every other ID goes through an exact byte-string table; a packable and an
//! unpackable ID are never equal, so the two tables never disagree.

use crate::{Map, new_map};

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
        Some(k) => st.hash_one(k),
        None => st.hash_one(s),
    }
}

pub struct PMap<V> {
    packed: Map<u64, V>,
    other: Map<S, V>,
}

impl<V> PMap<V> {
    pub fn with_capacity(n: usize) -> PMap<V> {
        PMap { packed: new_map(n), other: new_map(0) }
    }
    #[inline]
    pub fn get(&self, s: &[u8]) -> Option<&V> {
        match pack9(s) {
            Some(k) => self.packed.get(&k),
            None => self.other.get(s),
        }
    }
    pub fn contains_key(&self, s: &[u8]) -> bool {
        self.get(s).is_some()
    }
    /// Value of `s`, inserting `f()` first if absent.
    #[inline]
    pub fn get_or_insert_with(&mut self, s: S, f: impl FnOnce() -> V) -> &V {
        match pack9(s) {
            Some(k) => self.packed.entry(k).or_insert_with(f),
            None => self.other.entry(s).or_insert_with(f),
        }
    }
    /// Insert unless the key is present (first occurrence wins).
    pub fn insert_first(&mut self, s: S, v: V) {
        match pack9(s) {
            Some(k) => {
                self.packed.entry(k).or_insert(v);
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
