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
#[inline]
pub fn parse(b: &[u8]) -> Option<i64> {
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
