//! A small, deterministic, seedable hasher for byte-string keys.
//!
//! Replaces ahash, whose `hash_one` returned 0 for every key in this build (every map
//! degenerated into a linear scan). Owning the hash also keeps runs reproducible:
//! partition assignment never depends on a random seed.

use std::hash::{BuildHasher, Hasher};

const M1: u64 = 0x9E37_79B9_7F4A_7C15;
const M2: u64 = 0xBF58_476D_1CE4_E5B9;
const M3: u64 = 0x94D0_49BB_1331_11EB;

#[inline]
fn mix(a: u64, b: u64) -> u64 {
    let r = (a as u128).wrapping_mul(b as u128);
    (r as u64) ^ ((r >> 64) as u64)
}

#[derive(Clone, Copy)]
pub struct Seeded(pub u64);

pub struct H {
    h: u64,
}

impl Hasher for H {
    #[inline]
    fn write(&mut self, bytes: &[u8]) {
        let mut h = self.h;
        let mut c = bytes.chunks_exact(8);
        for w in &mut c {
            let v = u64::from_le_bytes(w.try_into().unwrap());
            h = mix(h ^ v, M2);
        }
        let r = c.remainder();
        if !r.is_empty() {
            let mut buf = [0u8; 8];
            buf[..r.len()].copy_from_slice(r);
            h = mix(h ^ u64::from_le_bytes(buf) ^ ((r.len() as u64) << 59), M3);
        }
        self.h = h;
    }
    #[inline]
    fn write_u8(&mut self, i: u8) {
        self.h = mix(self.h ^ i as u64, M2);
    }
    #[inline]
    fn write_u32(&mut self, i: u32) {
        self.h = mix(self.h ^ i as u64, M2);
    }
    #[inline]
    fn write_u64(&mut self, i: u64) {
        self.h = mix(self.h ^ i, M2);
    }
    #[inline]
    fn write_usize(&mut self, i: usize) {
        self.h = mix(self.h ^ i as u64, M3);
    }
    #[inline]
    fn write_i32(&mut self, i: i32) {
        self.write_u32(i as u32)
    }
    #[inline]
    fn finish(&self) -> u64 {
        // final avalanche (splitmix64 finalizer) so high and low bits are both good
        let mut z = self.h;
        z = (z ^ (z >> 30)).wrapping_mul(M2);
        z = (z ^ (z >> 27)).wrapping_mul(M3);
        z ^ (z >> 31)
    }
}

impl BuildHasher for Seeded {
    type Hasher = H;
    #[inline]
    fn build_hasher(&self) -> H {
        H { h: self.0.wrapping_mul(M1) ^ M3 }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spreads() {
        let s = Seeded(7);
        let mut seen = std::collections::HashSet::new();
        for i in 0..100_000u32 {
            let k = format!("u{i}");
            seen.insert(s.hash_one(k.as_bytes()));
        }
        assert_eq!(seen.len(), 100_000);
        // partition bits are well spread
        let mut buckets = [0u32; 512];
        for i in 0..512_000u32 {
            let h = s.hash_one(format!("e{i:x}").as_bytes());
            buckets[(h >> 20) as usize & 511] += 1;
        }
        let (mn, mx) = (buckets.iter().min().unwrap(), buckets.iter().max().unwrap());
        assert!(*mn > 800 && *mx < 1200, "{mn} {mx}");
        assert_ne!(Seeded(1).hash_one(b"x"), Seeded(2).hash_one(b"x"));
    }
}
