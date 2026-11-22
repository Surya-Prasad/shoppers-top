//! P1 (v2): shape-specialized line decoding.
//!
//! The fast path decodes one line straight from the input bytes: one structural mask pass
//! over the next 128 bytes (AVX2 when compiled for it, SSE2 otherwise on x86-64, scalar
//! elsewhere) finds the newline, the commas and the quotes; fields are cut with the
//! branch-free quote rule of RW16, and each field is decoded in place (packed IDs from a
//! single unaligned load, SWAR digits). It accepts a line only when the result is provably
//! the one the exact path (`dialect::split` + the per-table rules) computes; every other
//! line (longer than 127 bytes, wrong comma count, odd quotes, unusual shapes, invalid
//! values) returns `None` and the caller sends it through the exact path. So exactness
//! never depends on this module: it can only be faster, never different (unit tests below
//! check that on random and adversarial inputs).
//!
//! Callers guarantee `SLACK` readable bytes from the line start.

/// Readable bytes the fast path may touch from a line start (128 mask bytes + 16 for the
/// widest field load past a field that ends at the last mask byte, rounded up).
pub const SLACK: usize = 160;

/// Bitmasks (bit i = byte p+i) of `\n`, `,` and `"` over 128 bytes.
#[inline(always)]
unsafe fn masks128(p: *const u8) -> (u128, u128, u128) {
    #[cfg(all(target_arch = "x86_64", target_feature = "avx2"))]
    unsafe {
        use std::arch::x86_64::*;
        let (n, c, q) = (_mm256_set1_epi8(b'\n' as i8), _mm256_set1_epi8(b',' as i8), _mm256_set1_epi8(b'"' as i8));
        let mut out = (0u128, 0u128, 0u128);
        for k in 0..4 {
            let v = _mm256_loadu_si256(p.add(32 * k) as *const __m256i);
            out.0 |= (_mm256_movemask_epi8(_mm256_cmpeq_epi8(v, n)) as u32 as u128) << (32 * k);
            out.1 |= (_mm256_movemask_epi8(_mm256_cmpeq_epi8(v, c)) as u32 as u128) << (32 * k);
            out.2 |= (_mm256_movemask_epi8(_mm256_cmpeq_epi8(v, q)) as u32 as u128) << (32 * k);
        }
        out
    }
    #[cfg(all(target_arch = "x86_64", not(target_feature = "avx2")))]
    unsafe {
        use std::arch::x86_64::*;
        let (n, c, q) = (_mm_set1_epi8(b'\n' as i8), _mm_set1_epi8(b',' as i8), _mm_set1_epi8(b'"' as i8));
        let mut out = (0u128, 0u128, 0u128);
        for k in 0..8 {
            let v = _mm_loadu_si128(p.add(16 * k) as *const __m128i);
            out.0 |= (_mm_movemask_epi8(_mm_cmpeq_epi8(v, n)) as u32 as u128) << (16 * k);
            out.1 |= (_mm_movemask_epi8(_mm_cmpeq_epi8(v, c)) as u32 as u128) << (16 * k);
            out.2 |= (_mm_movemask_epi8(_mm_cmpeq_epi8(v, q)) as u32 as u128) << (16 * k);
        }
        out
    }
    #[cfg(not(target_arch = "x86_64"))]
    unsafe {
        let mut out = (0u128, 0u128, 0u128);
        for i in 0..128 {
            let b = *p.add(i);
            out.0 |= ((b == b'\n') as u128) << i;
            out.1 |= ((b == b',') as u128) << i;
            out.2 |= ((b == b'"') as u128) << i;
        }
        out
    }
}

/// Cut the line at `p` into N fields: `(line length, [(content start, content end); N])`,
/// offsets from `p`, quotes stripped. None unless the line ends within 128 bytes, has
/// exactly N-1 commas, and satisfies the quote rule (each field unquoted or wrapped in one
/// pair of quotes, no other quote anywhere), i.e. exactly when `dialect::split` would return
/// these fields. An empty line has 0 commas, so it is None for N >= 2.
#[inline(always)]
pub unsafe fn fields<const N: usize>(p: *const u8) -> Option<(usize, [(u32, u32); N])> {
    let (nl, c, q) = unsafe { masks128(p) };
    if nl == 0 {
        return None;
    }
    let e = nl.trailing_zeros();
    let lm = (1u128 << e) - 1;
    let mut cm = c & lm;
    if cm.count_ones() as usize != N - 1 {
        return None;
    }
    let qn = (q & lm).count_ones();
    let mut out = [(0u32, 0u32); N];
    let mut fs = 0u32;
    let mut qsum = 0u32;
    let mut ok = true;
    for (j, o) in out.iter_mut().enumerate() {
        let fe = if j + 1 < N {
            let t = cm.trailing_zeros();
            cm &= cm - 1;
            t
        } else {
            e
        };
        let l = fe - fs;
        let q0 = (l > 0 && unsafe { *p.add(fs as usize) } == b'"') as u32;
        let q1 = (l > 1 && unsafe { *p.add(fe as usize - 1) } == b'"') as u32;
        ok &= q0 == q1;
        qsum += q0 + q1;
        *o = (fs + q0, fe - q1);
        fs = fe + 1;
    }
    if !ok || qn != qsum {
        return None;
    }
    Some((e as usize, out))
}

#[inline(always)]
unsafe fn load64(p: *const u8) -> u64 {
    u64::from_le_bytes(unsafe { std::ptr::read_unaligned(p as *const [u8; 8]) })
}

/// `packed::pack9` of the `n` bytes at `p` (n in 1..=9), reading 9 bytes (callers
/// guarantee them readable). Same result as `pack9` (tested).
#[inline(always)]
pub unsafe fn pack_at(p: *const u8, n: usize) -> Option<u64> {
    debug_assert!((1..=9).contains(&n));
    let raw = unsafe { load64(p) };
    let lanes: u64 = if n >= 8 { !0 } else { (1u64 << (8 * n)) - 1 };
    let w = raw & lanes;
    const LO7: u64 = 0x7f7f_7f7f_7f7f_7f7f;
    const HI: u64 = 0x8080_8080_8080_8080;
    let nonzero = ((w & LO7).wrapping_add(LO7) | w) & HI;
    if w & lanes & HI != 0 || nonzero & lanes & HI != lanes & HI {
        return None;
    }
    let mut x = w.swap_bytes() & LO7;
    x = (x & 0x007f_007f_007f_007f) | ((x & 0x7f00_7f00_7f00_7f00) >> 1);
    x = (x & 0x0000_3fff_0000_3fff) | ((x & 0x3fff_0000_3fff_0000) >> 2);
    x = (x & 0x0000_0000_0fff_ffff) | ((x & 0x0fff_ffff_0000_0000) >> 4);
    let mut v = x << 7;
    if n == 9 {
        let c = unsafe { *p.add(8) };
        if c == 0 || c > 127 {
            return None;
        }
        v |= c as u64;
    }
    Some(v)
}

/// An all-digit field of 1..=8 bytes at `p` as a number (reads 8 bytes); None if any of
/// the n bytes is not an ASCII digit.
#[inline(always)]
pub unsafe fn digits8(p: *const u8, n: usize) -> Option<u32> {
    debug_assert!((1..=8).contains(&n));
    let x = (unsafe { load64(p) } ^ 0x3030_3030_3030_3030) << (8 * (8 - n));
    let bad = (x & 0xf0f0_f0f0_f0f0_f0f0) | (x.wrapping_add(0x0606_0606_0606_0606) & 0xf0f0_f0f0_f0f0_f0f0);
    if bad != 0 {
        return None;
    }
    let x = x.wrapping_mul(10).wrapping_add(x >> 8) & 0x00ff_00ff_00ff_00ff;
    let x = x.wrapping_mul(100).wrapping_add(x >> 16) & 0x0000_ffff_0000_ffff;
    Some((x.wrapping_mul(10000).wrapping_add(x >> 32) & 0xffff_ffff) as u32)
}

#[inline(always)]
unsafe fn slice<'a>(p: *const u8, f: (u32, u32)) -> &'a [u8] {
    unsafe { std::slice::from_raw_parts(p.add(f.0 as usize), (f.1 - f.0) as usize) }
}

/// A packed non-empty ID field of at most 9 bytes.
#[inline(always)]
unsafe fn id(p: *const u8, f: (u32, u32)) -> Option<u64> {
    let n = (f.1 - f.0) as usize;
    if n == 0 || n > 9 {
        return None;
    }
    unsafe { pack_at(p.add(f.0 as usize), n) }
}

/// v2: `ingest_ts` from an already parsed `ts` when both are F1 with the same date and hour
/// (`YYYY-MM-DDTHH:` equal): only `MM:SS` of `ingest_ts` is checked and parsed, so the result
/// equals `timefmt::parse(ingest_ts)`. None when the shortcut does not apply (the caller then
/// runs the full parser).
#[inline(always)]
unsafe fn same_hour_f1(p: *const u8, t: (u32, u32), i: (u32, u32), ts: i64) -> Option<i64> {
    if t.1 - t.0 != 20 || i.1 - i.0 != 20 {
        return None;
    }
    let (tp, ip) = unsafe { (p.add(t.0 as usize), p.add(i.0 as usize)) };
    // first 14 bytes equal: two overlapping 8-byte loads (bytes 0..8 and 6..14)
    if unsafe { load64(tp) != load64(ip) || load64(tp.add(6)) != load64(ip.add(6)) } {
        return None;
    }
    // ts must be F1 (a 20-byte ts can also be a 20-digit F3, which has no '-')
    if unsafe { *tp.add(4) != b'-' || *ip.add(16) != b':' || *ip.add(19) != b'Z' } {
        return None;
    }
    let d = |q: *const u8, k: usize| -> Option<i64> {
        let (a, b) = unsafe { ((*q.add(k)).wrapping_sub(b'0'), (*q.add(k + 1)).wrapping_sub(b'0')) };
        (a < 10 && b < 10).then_some((a * 10 + b) as i64)
    };
    let (im, is) = (d(ip, 14)?, d(ip, 17)?);
    if im > 59 || is > 59 {
        return None;
    }
    // ts is a valid F1 (parsed by the caller), so its MM and SS are valid digits
    let (tm, tsec) = (d(tp, 14)?, d(tp, 17)?);
    Some(ts + ((im - tm) * 60 + (is - tsec)) * 1000)
}

/// A fully valid event line in the documented column order, decoded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Ev {
    pub eid: u64,
    pub user: u64,
    pub ts: i64,
    pub ingest: i64,
    pub price: u32, // 0 for view/cart
    pub product: u64,
    /// v2: `packed::canon(product, pprefix)` or 0 (not canonical)
    pub pnum: u32,
}

/// Decode the events line at `p` (columns in the documented order). Some only for a line
/// whose kept copy would be valid under the exact rules: packable non-empty event, user and
/// product IDs, `ts` valid and in `[lo, hi)`, `ingest_ts` valid, exact event type, a valid
/// price for a purchase, not late. Every other line is for the exact path.
#[inline(always)]
pub unsafe fn event(p: *const u8, lo: i64, hi: i64, pprefix: u8) -> Option<(usize, Ev)> {
    let (len, f) = unsafe { fields::<7>(p) }?;
    let eid = unsafe { id(p, f[0]) }?;
    let user = unsafe { id(p, f[1]) }?;
    let ts = crate::timefmt::parse(unsafe { slice(p, f[2]) })?;
    if ts < lo || ts >= hi {
        return None;
    }
    let ingest = unsafe { same_hour_f1(p, f[2], f[3], ts) }.or_else(|| crate::timefmt::parse(unsafe { slice(p, f[3]) }))?;
    let tl = f[4].1 - f[4].0;
    let tp = unsafe { p.add(f[4].0 as usize) };
    let purchase = if tl == 4 {
        let w = u32::from_le_bytes(unsafe { std::ptr::read_unaligned(tp as *const [u8; 4]) });
        if w != u32::from_le_bytes(*b"view") && w != u32::from_le_bytes(*b"cart") {
            return None;
        }
        false
    } else if tl == 8 && unsafe { load64(tp) } == u64::from_le_bytes(*b"purchase") {
        true
    } else {
        return None;
    };
    let product = unsafe { id(p, f[5]) }?;
    // canonical product ID: prefix + 1..=7 digits without a leading zero (< DMAX = 2^22 holds
    // for up to 6 digits; 7-digit numbers are checked)
    let pn = (f[5].1 - f[5].0) as usize;
    let pq = unsafe { p.add(f[5].0 as usize) };
    let pnum = if (2..=8).contains(&pn) && pprefix != 0 && unsafe { *pq } == pprefix && unsafe { *pq.add(1) }.wrapping_sub(b'1') < 9 {
        match unsafe { digits8(pq.add(1), pn - 1) } {
            Some(v) if v < crate::packed::DMAX => v,
            _ => 0,
        }
    } else {
        0
    };
    let price = if purchase {
        let n = (f[6].1 - f[6].0) as usize;
        if n == 0 || n > 8 {
            return None;
        }
        let v = unsafe { digits8(p.add(f[6].0 as usize), n) }?;
        if !(1..=10_000_000).contains(&v) {
            return None;
        }
        v
    } else {
        0
    };
    if ingest.saturating_sub(ts) > crate::timefmt::DAY {
        return None;
    }
    Some((len, Ev { eid, user, ts, ingest, price, product, pnum }))
}

/// A fully valid click line in the documented column order, decoded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cl {
    pub cid: u64,
    pub user: u64,
    pub camp: u64, // packed campaign ID (the caller looks it up; a miss goes to the exact path)
    pub ts: i64,
}

/// Decode the clicks line at `p`: packable non-empty click, user and campaign IDs, `ts`
/// valid and in `[lo, hi)`.
#[inline(always)]
pub unsafe fn click(p: *const u8, lo: i64, hi: i64) -> Option<(usize, Cl)> {
    let (len, f) = unsafe { fields::<4>(p) }?;
    let cid = unsafe { id(p, f[0]) }?;
    let user = unsafe { id(p, f[1]) }?;
    let camp = unsafe { id(p, f[2]) }?;
    let ts = crate::timefmt::parse(unsafe { slice(p, f[3]) })?;
    if ts < lo || ts >= hi {
        return None;
    }
    Some((len, Cl { cid, user, camp, ts }))
}

/// Run `fast` on each line of `chunk` that has `SLACK` bytes after its start, and `slow`
/// on the line (without its `\n`, as `for_each_line` gives it) wherever `fast` declines
/// and on the chunk's tail. `fast` returns the line length (without `\n`) when it took the
/// line. Every line of the chunk is visited exactly once, in order.
#[inline(always)]
pub fn for_each_line_fast<'a>(
    chunk: &'a [u8],
    mut fast: impl FnMut(*const u8) -> Option<usize>,
    mut slow: impl FnMut(&'a [u8]),
) {
    let len = chunk.len();
    let base = chunk.as_ptr();
    let mut s = 0usize;
    while s + SLACK <= len {
        // SAFETY: s + SLACK <= len, so the fast path reads inside the chunk
        if let Some(l) = fast(unsafe { base.add(s) }) {
            s += l + 1;
            continue;
        }
        let e = memchr::memchr(b'\n', &chunk[s..]).map_or(len, |i| s + i);
        slow(&chunk[s..e]);
        s = e + 1;
    }
    while s < len {
        let e = memchr::memchr(b'\n', &chunk[s..]).map_or(len, |i| s + i);
        slow(&chunk[s..e]);
        s = e + 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dialect::split;
    use crate::packed::pack9;

    fn lcg(x: &mut u64) -> u64 {
        *x = x.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        *x >> 33
    }

    fn padded(line: &[u8]) -> Vec<u8> {
        let mut v = line.to_vec();
        v.resize(line.len() + SLACK + 64, b'x');
        v
    }

    #[test]
    fn pack_at_matches_pack9() {
        let mut x = 11u64;
        let alpha: [u8; 12] = [0, 1, b'0', b'a', b'z', b'Z', b' ', b'\r', 0x7e, 0x7f, 0x80, 0xff];
        for _ in 0..400_000 {
            let n = 1 + (lcg(&mut x) % 9) as usize;
            let mut v: Vec<u8> = (0..n + 16)
                .map(|_| if lcg(&mut x) % 3 == 0 { alpha[(lcg(&mut x) as usize) % 12] } else { (lcg(&mut x) % 256) as u8 })
                .collect();
            if lcg(&mut x) % 2 == 0 {
                for b in &mut v[..n] {
                    *b = b"0123456789abcdefghijklmnopqrstuvwxyz"[(lcg(&mut x) % 36) as usize];
                }
            }
            assert_eq!(unsafe { pack_at(v.as_ptr(), n) }, pack9(&v[..n]), "{v:?} {n}");
        }
    }

    /// The same-hour shortcut equals the full parser (random F1 pairs, mutated MM:SS, F3 ts).
    #[test]
    fn same_hour_matches_parse() {
        let mut x = 8u64;
        let alpha = b"0123456789:Zz \x7f";
        for _ in 0..300_000 {
            let base = 1_790_000_000_000i64 + (lcg(&mut x) as i64 % 2_000_000) * 1000;
            let t = crate::timefmt::tests_f1(base);
            let mut i = crate::timefmt::tests_f1(base + (lcg(&mut x) as i64 % 1200) * 1000).into_bytes();
            if lcg(&mut x) % 4 == 0 {
                let k = 11 + (lcg(&mut x) as usize) % 9;
                i[k] = alpha[(lcg(&mut x) as usize) % alpha.len()];
            }
            let mut buf = t.clone().into_bytes();
            buf.push(b',');
            buf.extend_from_slice(&i);
            buf.resize(buf.len() + 32, b'x');
            let ts = crate::timefmt::parse(t.as_bytes()).unwrap();
            if let Some(v) = unsafe { same_hour_f1(buf.as_ptr(), (0, 20), (21, 41), ts) } {
                assert_eq!(Some(v), crate::timefmt::parse(&i), "{t} {:?}", String::from_utf8_lossy(&i));
            }
        }
        // a 20-digit F3 ts with an ingest sharing its first 14 bytes
        let (t, i) = (b"00001791071890000000", b"00001791071890:00:0Z");
        let mut buf = t.to_vec();
        buf.push(b',');
        buf.extend_from_slice(i);
        buf.resize(buf.len() + 32, b'x');
        let ts = crate::timefmt::parse(t).unwrap();
        assert_eq!(unsafe { same_hour_f1(buf.as_ptr(), (0, 20), (21, 41), ts) }, None);
    }

    #[test]
    fn digits8_exact() {
        let mut x = 5u64;
        let alpha = b"0123456789/:a \x7f\xd9";
        for _ in 0..400_000 {
            let n = 1 + (lcg(&mut x) % 8) as usize;
            let mut v: Vec<u8> = (0..n + 8).map(|_| b'0' + (lcg(&mut x) % 10) as u8).collect();
            if lcg(&mut x) % 3 == 0 {
                let i = (lcg(&mut x) as usize) % (n + 8);
                v[i] = alpha[(lcg(&mut x) as usize) % alpha.len()];
            }
            let s = &v[..n];
            let want = s.iter().all(u8::is_ascii_digit).then(|| s.iter().fold(0u32, |a, &c| a * 10 + (c - b'0') as u32));
            assert_eq!(unsafe { digits8(v.as_ptr(), n) }, want, "{s:?}");
        }
    }

    /// `fields` agrees with `split` whenever it accepts, and accepts every short regular line.
    #[test]
    fn fields_match_split() {
        let mut x = 3u64;
        let alpha: &[u8] = b"ab,,\"\"\r 0z\n";
        for it in 0..300_000 {
            let mut line: Vec<u8> = Vec::new();
            let nf = 1 + lcg(&mut x) % 9;
            for i in 0..nf {
                if i > 0 {
                    line.push(b',');
                }
                let q = lcg(&mut x) % 3 == 0;
                if q {
                    line.push(b'"');
                }
                for _ in 0..(lcg(&mut x) % 20) {
                    line.push(if lcg(&mut x) % 8 == 0 { alpha[(lcg(&mut x) as usize) % alpha.len()] } else { b'a' + (lcg(&mut x) % 26) as u8 });
                }
                if q && lcg(&mut x) % 8 != 0 {
                    line.push(b'"');
                }
            }
            if it % 50 == 0 {
                line.clear();
            }
            // the line ends at its first '\n'
            let cut = line.iter().position(|&b| b == b'\n').unwrap_or(line.len());
            line.truncate(cut);
            let mut buf = line.clone();
            buf.push(b'\n');
            let buf = padded(&buf);
            fn chk<const N: usize>(line: &[u8], buf: &[u8]) {
                let want = split::<N>(line);
                let got = unsafe { fields::<N>(buf.as_ptr()) };
                match got {
                    Some((l, f)) => {
                        assert_eq!(l, line.len());
                        let w = want.expect("fast accepted a line split rejects");
                        for j in 0..N {
                            assert_eq!(&buf[f[j].0 as usize..f[j].1 as usize], w[j], "{:?}", String::from_utf8_lossy(line));
                        }
                    }
                    None => {
                        if line.len() < 128 {
                            assert!(want.is_none(), "fast declined a regular line {:?}", String::from_utf8_lossy(line));
                        }
                    }
                }
            }
            chk::<2>(&line, &buf);
            chk::<4>(&line, &buf);
            chk::<7>(&line, &buf);
        }
    }

    /// Adversarial event lines: the fast decoder agrees with the exact field rules whenever it
    /// accepts (the scan-level equivalence of whole records is tested in main.rs).
    #[test]
    fn event_matches_exact_rules() {
        let lo = crate::timefmt::parse(b"2026-10-01T00:00:00Z").unwrap();
        let hi = lo + 14 * crate::timefmt::DAY;
        let mut x = 21u64;
        let mut accepted = 0;
        for _ in 0..300_000 {
            let line = crate::fast::tests_support::random_event_line(&mut x, lo);
            let buf = padded(&[line.as_slice(), b"\n"].concat());
            let got = unsafe { event(buf.as_ptr(), lo, hi, b'p') };
            let f = split::<7>(&line);
            if let Some((l, ev)) = got {
                accepted += 1;
                assert_eq!(l, line.len());
                let f = f.expect("accepted a malformed line");
                assert_eq!(Some(ev.eid), pack9(f[0]).filter(|_| !f[0].is_empty()));
                assert_eq!(Some(ev.user), pack9(f[1]).filter(|_| !f[1].is_empty()));
                assert_eq!(Some(ev.product), pack9(f[5]).filter(|_| !f[5].is_empty()));
                assert_eq!(ev.pnum, crate::packed::canon(f[5], b'p').unwrap_or(0), "{:?}", String::from_utf8_lossy(f[5]));
                assert_eq!(Some(ev.ts), crate::timefmt::parse(f[2]));
                assert!(ev.ts >= lo && ev.ts < hi);
                assert_eq!(Some(ev.ingest), crate::timefmt::parse(f[3]));
                assert!(ev.ingest.saturating_sub(ev.ts) <= crate::timefmt::DAY);
                match f[4] {
                    b"purchase" => assert_eq!(ev.price, crate::parse_price(f[6])),
                    b"view" | b"cart" => assert_eq!(ev.price, 0),
                    _ => panic!("bad type accepted"),
                }
                assert!(f[4] != b"purchase" || ev.price > 0);
            }
        }
        assert!(accepted > 30_000, "{accepted}");
    }
}

#[cfg(test)]
pub mod tests_support {
    fn lcg(x: &mut u64) -> u64 {
        *x = x.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        *x >> 33
    }
    fn pick<'a>(x: &mut u64, v: &[&'a str]) -> &'a str {
        v[(lcg(x) as usize) % v.len()]
    }
    /// A generator-like event line with dirt mixed in (quotes, bad values, odd shapes).
    pub fn random_event_line(x: &mut u64, lo: i64) -> Vec<u8> {
        let b36 = |x: &mut u64, pre: &str| -> String {
            let mut s = pre.to_string();
            let n = 1 + lcg(x) % 9;
            for _ in 0..n {
                s.push(b"0123456789abcdefghijklmnopqrstuvwxyz"[(lcg(x) % 36) as usize] as char);
            }
            if lcg(x) % 40 == 0 {
                s = s.to_uppercase();
            }
            s
        };
        let ts_ms = lo - 2 * crate::timefmt::DAY + (lcg(x) as i64 * 7919) % (18 * crate::timefmt::DAY);
        let f1 = |ms: i64| -> String {
            let d = ms.div_euclid(crate::timefmt::DAY);
            let (y, m, dd) = civil(d);
            let s = ms.rem_euclid(crate::timefmt::DAY) / 1000;
            format!("{y:04}-{m:02}-{dd:02}T{:02}:{:02}:{:02}Z", s / 3600, s / 60 % 60, s % 60)
        };
        let ts = match lcg(x) % 8 {
            0..=3 => f1(ts_ms),
            4 => format!("{}", ts_ms),
            5 => format!("000{}", ts_ms),
            6 => {
                let mut t = f1(ts_ms + 3_600_000);
                t.pop();
                t + "+01:00"
            }
            _ => pick(x, &["", "2026-10-03T23:58:10.000Z", "2026-02-30T00:00:00Z", "1.79e12", "2026-10-03 23:58:10Z", "x"]).to_string(),
        };
        let ing_ms = ts_ms + [1000i64, 60_000, 86_400_000, 86_401_000, -5000, 300_000][(lcg(x) % 6) as usize];
        let ingest = if lcg(x) % 30 == 0 { format!("{ing_ms}") } else { f1(ing_ms) };
        let typ = pick(x, &["view", "view", "cart", "purchase", "purchase", "Purchase", "cart ", "", "views"]);
        let prod = if lcg(x) % 20 == 0 { pick(x, &["", "P12", "p1234567890", " p1"]).to_string() } else { format!("p{}", lcg(x) % 60000) };
        let price = pick(x, &["", "1000", "0250", "0", "10000000", "10000001", "12.5", "-5", "00000001", "123456789", "99"]);
        let eid = if lcg(x) % 50 == 0 { pick(x, &["", "e1234567890", "é1"]).to_string() } else { b36(x, "e") };
        let uid = if lcg(x) % 50 == 0 { pick(x, &["", " u1", "u1\r"]).to_string() } else { b36(x, "u") };
        let mut fields = vec![eid, uid, ts, ingest, typ.to_string(), prod, price.to_string()];
        if lcg(x) % 100 == 0 {
            fields.pop();
        }
        let mut out = String::new();
        for (i, f) in fields.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            match lcg(x) % 12 {
                0 | 1 => out.push_str(&format!("\"{f}\"")),
                2 if lcg(x) % 10 == 0 => out.push_str(&format!("\"{f}")),
                3 if lcg(x) % 10 == 0 => out.push_str(&format!("{f}\"x")),
                _ => out.push_str(f),
            }
        }
        if lcg(x) % 200 == 0 {
            out.push('\r');
        }
        out.into_bytes()
    }
    fn civil(days: i64) -> (i64, i64, i64) {
        let z = days + 719_468;
        let era = z.div_euclid(146_097);
        let doe = z - era * 146_097;
        let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
        let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
        let mp = (5 * doy + 2) / 153;
        let d = doy - (153 * mp + 2) / 5 + 1;
        let m = if mp < 10 { mp + 3 } else { mp - 9 };
        (yoe + era * 400 + (m <= 2) as i64, m, d)
    }
}
