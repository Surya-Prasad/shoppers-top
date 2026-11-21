//! Timestamps: F1 `YYYY-MM-DDTHH:MM:SSZ`, F2 `YYYY-MM-DDTHH:MM:SS±HH:MM`, F3 epoch-ms
//! digits. Everything else is bad. Digits are ASCII `0`-`9` only.

pub const SECOND: i64 = 1_000;
pub const MINUTE: i64 = 60 * SECOND;
pub const DAY: i64 = 86_400_000;

/// Sentinel for "bad / outside window". No valid in-window value can equal it.
pub const BAD: i64 = i64::MIN;

#[inline]
fn d2(b: &[u8], i: usize) -> Option<i64> {
    let (a, c) = (b[i].wrapping_sub(b'0'), b[i + 1].wrapping_sub(b'0'));
    if a < 10 && c < 10 { Some((a * 10 + c) as i64) } else { None }
}

pub fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * ((m + 9) % 12) + 2) / 5 + d - 1;
    era * 146_097 + yoe * 365 + yoe / 4 - yoe / 100 + doy - 719_468
}

fn days_in_month(y: i64, m: i64) -> i64 {
    match m {
        2 => {
            if y % 4 == 0 && (y % 100 != 0 || y % 400 == 0) { 29 } else { 28 }
        }
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

/// Parse any of the three forms to epoch ms. F3 values too large for i64 return None
/// (they are "just a number" outside every window, so the caller treats them as bad).
///
/// Night-08: a SWAR check of the F1/F2 layout; same results as the
/// original byte-by-byte parser (`parse_ref`, kept for the equivalence test).
#[inline]
pub fn parse(b: &[u8]) -> Option<i64> {
    let n = b.len();
    if (n == 20 || n == 25) && b[4] == b'-' {
        return parse_dt(b);
    }
    if n == 0 || !b.iter().all(u8::is_ascii_digit) {
        return None; // not F3, and not the F1/F2 layout
    }
    // F3: digits only, any length, leading zeros allowed
    let first = b.iter().position(|&c| c != b'0').unwrap_or(n);
    let digits = &b[first..];
    if digits.len() > 19 {
        return None;
    }
    let mut v: u64 = 0;
    for &c in digits {
        v = v * 10 + (c - b'0') as u64; // 19 digits always fit in u64
    }
    i64::try_from(v).ok()
}

/// SWAR helpers: bytes of `x` (already XOR 0x30) that are ASCII digits, as a 0x80 mask.
#[inline(always)]
fn nondigit_mask(x: u64) -> u64 {
    // byte < 10 iff no high bit in (byte & 0x7f) + 0x76 and no high bit in byte
    (((x & 0x7f7f_7f7f_7f7f_7f7f) + 0x7676_7676_7676_7676) | x) & 0x8080_8080_8080_8080
}

/// F1 (len 20, `Z`) or F2 (len 25, `±HH:MM`); `b[4] == '-'` is known.
#[inline]
fn parse_dt(b: &[u8]) -> Option<i64> {
    let n = b.len();
    let offset = if n == 20 {
        if b[19] != b'Z' {
            return None;
        }
        0
    } else {
        if !((b[19] == b'+' || b[19] == b'-') && b[22] == b':') {
            return None;
        }
        let oh = d2(b, 20)?;
        let om = d2(b, 23)?;
        if oh > 23 || om > 59 {
            return None;
        }
        let o = (oh * 60 + om) * MINUTE;
        if b[19] == b'+' { o } else { -o }
    };
    // bytes 0..8 "YYYY-MM-", 8..16 "DDTHH:MM", 16..19 ":SS"
    let w0 = u64::from_le_bytes(b[0..8].try_into().unwrap());
    let w1 = u64::from_le_bytes(b[8..16].try_into().unwrap());
    let w2 = u32::from_le_bytes(b[16..20].try_into().unwrap()) as u64 & 0x00ff_ffff;
    // separators: '-' at 4 and 7, 'T' at 10, ':' at 13 and 16
    const S0: u64 = 0xff00_00ff_0000_0000;
    const E0: u64 = (b'-' as u64) << 56 | (b'-' as u64) << 32;
    const S1: u64 = 0x0000_ff00_00ff_0000;
    const E1: u64 = (b':' as u64) << 40 | (b'T' as u64) << 16;
    if w0 & S0 != E0 || w1 & S1 != E1 || w2 & 0xff != b':' as u64 {
        return None;
    }
    let (x0, x1, x2) = (w0 ^ 0x3030_3030_3030_3030, w1 ^ 0x3030_3030_3030_3030, w2 ^ 0x0030_3030);
    const D0: u64 = 0x0080_8000_8080_8080; // digits at 0,1,2,3,5,6
    const D1: u64 = 0x8080_0080_8000_8080; // digits at 8,9,11,12,14,15
    const D2: u64 = 0x0000_0000_0080_8000; // digits at 17,18
    if nondigit_mask(x0) & D0 != 0 || nondigit_mask(x1) & D1 != 0 || nondigit_mask(x2) & D2 != 0 {
        return None;
    }
    let by = |x: u64, i: u32| ((x >> (8 * i)) & 0xff) as i64;
    let y = by(x0, 0) * 1000 + by(x0, 1) * 100 + by(x0, 2) * 10 + by(x0, 3);
    let mo = by(x0, 5) * 10 + by(x0, 6);
    let d = by(x1, 0) * 10 + by(x1, 1);
    let h = by(x1, 3) * 10 + by(x1, 4);
    let mi = by(x1, 6) * 10 + by(x1, 7);
    let s = by(x2, 1) * 10 + by(x2, 2);
    if !(1..=12).contains(&mo) || d < 1 || d > days_in_month(y, mo) || h > 23 || mi > 59 || s > 59 {
        return None;
    }
    Some(days_from_civil(y, mo, d) * DAY + ((h * 60 + mi) * 60 + s) * SECOND - offset)
}

/// Parse any of the three forms to epoch ms. F3 values too large for i64 return None
/// (they are "just a number" outside every window, so the caller treats them as bad).
#[cfg(test)]
pub fn parse_ref(b: &[u8]) -> Option<i64> {
    let n = b.len();
    if n == 0 {
        return None;
    }
    if b.iter().all(u8::is_ascii_digit) {
        // F3: digits only, any length, leading zeros allowed
        let first = b.iter().position(|&c| c != b'0').unwrap_or(n);
        let digits = &b[first..];
        if digits.len() > 19 {
            return None;
        }
        let mut v: u64 = 0;
        for &c in digits {
            v = v * 10 + (c - b'0') as u64; // 19 digits always fit in u64
        }
        return i64::try_from(v).ok();
    }
    let offset = match n {
        20 if b[19] == b'Z' => 0,
        25 if (b[19] == b'+' || b[19] == b'-') && b[22] == b':' => {
            let oh = d2(b, 20)?;
            let om = d2(b, 23)?;
            if oh > 23 || om > 59 {
                return None;
            }
            let o = (oh * 60 + om) * MINUTE;
            if b[19] == b'+' { o } else { -o }
        }
        _ => return None,
    };
    if b[4] != b'-' || b[7] != b'-' || b[10] != b'T' || b[13] != b':' || b[16] != b':' {
        return None;
    }
    let y = d2(b, 0)? * 100 + d2(b, 2)?;
    let mo = d2(b, 5)?;
    let d = d2(b, 8)?;
    let h = d2(b, 11)?;
    let mi = d2(b, 14)?;
    let s = d2(b, 17)?;
    if !(1..=12).contains(&mo) || d < 1 || d > days_in_month(y, mo) || h > 23 || mi > 59 || s > 59 {
        return None;
    }
    Some(days_from_civil(y, mo, d) * DAY + ((h * 60 + mi) * 60 + s) * SECOND - offset)
}

/// Parse and check a half-open window `[lo, hi)`; BAD if either fails.
#[inline]
pub fn parse_in(b: &[u8], lo: i64, hi: i64) -> i64 {
    match parse(b) {
        Some(v) if v >= lo && v < hi => v,
        _ => BAD,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lcg(x: &mut u64) -> u64 {
        *x = x.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        *x >> 33
    }

    /// Night-08: the fast parser equals the original on random mutations of every form.
    #[test]
    fn fast_equals_reference() {
        let seeds: [&[u8]; 8] = [
            b"2026-10-03T23:58:10Z", b"2026-10-03T19:58:10-04:00", b"1791071890000", b"0001791071890000",
            b"2028-02-29T00:00:00+12:00", b"9223372036854775807", b"00000000000000000001", b"2027-12-31T23:59:59Z",
        ];
        let alpha: &[u8] = b"0123456789-+:TZtz .\xd9\x0f\x3a\x2f";
        let mut x = 99u64;
        for i in 0..2_000_000u32 {
            let mut v = seeds[(i as usize) % seeds.len()].to_vec();
            for _ in 0..(lcg(&mut x) % 3) {
                let p = (lcg(&mut x) as usize) % v.len();
                match lcg(&mut x) % 4 {
                    0 if v.len() > 1 => { v.remove(p); }
                    1 => v.insert(p, alpha[(lcg(&mut x) as usize) % alpha.len()]),
                    _ => v[p] = alpha[(lcg(&mut x) as usize) % alpha.len()],
                }
            }
            assert_eq!(parse(&v), parse_ref(&v), "{:?}", String::from_utf8_lossy(&v));
        }
        // every calendar day 2024..2029 at a few times, both forms
        for day in 0..(6 * 366) {
            let ms = days_from_civil(2024, 1, 1) * DAY + day as i64 * DAY + 3_723_000;
            let s = format!("{ms}");
            assert_eq!(parse(s.as_bytes()), Some(ms));
        }
        for (y, m, d) in [(2028, 2, 29), (2027, 2, 29), (2026, 4, 31), (2026, 12, 31), (2100, 2, 29), (2000, 2, 29)] {
            for t in [format!("{y:04}-{m:02}-{d:02}T00:00:00Z"), format!("{y:04}-{m:02}-{d:02}T23:59:59+00:30")] {
                assert_eq!(parse(t.as_bytes()), parse_ref(t.as_bytes()), "{t}");
            }
        }
    }

    #[test]
    fn forms() {
        let f1 = parse(b"2026-10-03T23:58:10Z").unwrap();
        assert_eq!(f1, 1_791_071_890_000);
        assert_eq!(parse(b"2026-10-03T19:58:10-04:00"), Some(f1));
        assert_eq!(parse(b"1791071890000"), Some(f1));
        assert_eq!(parse(b"0001791071890000"), Some(f1));
        assert_eq!(parse(b"2026-10-04T01:30:00+02:00"), parse(b"2026-10-03T23:30:00Z"));
        assert_eq!(parse(b"2026-10-03T10:00:00-00:00"), parse(b"2026-10-03T10:00:00Z"));
        assert_eq!(parse(b"2026-10-03T10:00:00+23:59").map(|v| v + 1439 * MINUTE), parse(b"2026-10-03T10:00:00Z"));
        assert_eq!(parse(b"2028-02-29T00:00:00Z").is_some(), true);
        assert_eq!(parse(b"20260101000000000000"), None); // 20 significant digits > i64
        assert_eq!(parse(b"9223372036854775807"), Some(i64::MAX));
        assert_eq!(parse(b"9223372036854775808"), None);
        assert_eq!(parse(b"00000000000000000001"), Some(1)); // 20 digits, leading zeros
        for bad in [
            &b""[..], b"2026-10-03T23:58:10.000Z", b"2026-10-03T23:58:10z", b"2026-10-03t23:58:10Z",
            b"2026-10-03 23:58:10Z", b"2026-10-03T23:58:10", b"1791071890000.0", b"1.79e12", b"-5",
            b"2026-13-03T23:58:10Z", b"2026-02-30T00:00:00Z", b"2027-02-29T00:00:00Z",
            b"2026-10-03T24:00:00Z", b"2026-10-03T23:59:60Z", b"2026-10-03T10:00:00+24:00",
            b"2026-10-03T10:00:00+05:60", b"2026-10-03T10:00:00+0200", b"2026-00-03T10:00:00Z",
            b"2026-10-00T10:00:00Z", "２026-10-03T10:00:00Z".as_bytes(), "١٧٩١٠٧١٨٩٠٠٠٠".as_bytes(),
            b"179102160000000000000", b" 2026-10-03T10:00:00Z", b"2026-10-03T10:00:00Z ",
            b"2026-10-03T10:00:00Z\r", b"+1791071890000", b"2026-1-03T10:00:00Z",
        ] {
            assert!(parse(bad).is_none(), "{:?} should be bad", String::from_utf8_lossy(bad));
        }
    }
}
