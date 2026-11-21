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
pub const EMPTY: u64 = 0;

struct Interner {
    map: Map<S, u64>,
    bytes: Vec<S>,
}

fn interner() -> &'static Mutex<Interner> {
    static I: OnceLock<Mutex<Interner>> = OnceLock::new();
    I.get_or_init(|| Mutex::new(Interner { map: new_map(16), bytes: Vec::new() }))
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

#[cold]
fn intern(s: S) -> u64 {
    let mut g = interner().lock().unwrap();
    if let Some(&k) = g.map.get(s) {
        return k;
    }
    let k = INTERNED | g.bytes.len() as u64;
    g.bytes.push(s);
    g.map.insert(s, k);
    k
}

/// The bytes of a key, into `buf` for packed keys.
pub fn bytes_of(k: u64, buf: &mut [u8; 9]) -> &[u8] {
    if k & INTERNED != 0 {
        let g = interner().lock().unwrap();
        return g.bytes[(k & !INTERNED) as usize];
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
