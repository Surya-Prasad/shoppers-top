//! The CSV dialect of rules.md, hand-written so it matches exactly.
//!
//! * Only `\n` ends a line; `\r` is an ordinary byte of its field.
//! * Every comma separates fields (values never hold commas, quotes or newlines).
//! * A field may be wrapped in double quotes; the quotes are syntax. A quote anywhere
//!   else makes the whole line malformed. `""` is an empty field.
//! * An empty field is null (represented here by an empty slice).
//! * A wrong field count or an empty line is malformed. Malformed lines are dropped.
//! * Spaces are part of values; nothing is trimmed or normalized.

use std::fs::File;
use std::path::{Path, PathBuf};

use memmap2::Mmap;

/// One input file, memory-mapped read-only (the input mount is read-only, and the
/// page cache is shared, so this is the cheapest "scan" access path).
#[allow(dead_code)]
pub struct Source {
    pub path: PathBuf,
    map: Option<Mmap>,
}

impl Source {
    pub fn open(path: &Path) -> std::io::Result<Source> {
        let file = File::open(path)?;
        let len = file.metadata()?.len();
        // mmap of a zero-length file is an error on Linux; an empty file has no rows.
        let map = if len == 0 { None } else { Some(unsafe { Mmap::map(&file)? }) };
        if let Some(m) = &map {
            let _ = m.advise(memmap2::Advice::Sequential);
        }
        Ok(Source { path: path.to_path_buf(), map })
    }

    pub fn bytes(&self) -> &[u8] {
        self.map.as_deref().unwrap_or(&[])
    }
}

/// Every `*.csv` regular file directly inside `dir` (not recursive). Names starting
/// with `.` are skipped, as a shell glob `*.csv` would. A missing folder means no files.
pub fn csv_files(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else { return out };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with('.') || !name.ends_with(".csv") {
            continue;
        }
        let path = entry.path();
        if path.is_file() {
            out.push(path);
        }
    }
    out.sort();
    out
}

/// The data region of a file: everything after the header line. A file with no
/// newline at all is only a header (or empty) and has no data.
pub fn data_region(bytes: &[u8]) -> &[u8] {
    match memchr::memchr(b'\n', bytes) {
        Some(i) => &bytes[i + 1..],
        None => &[],
    }
}

/// Split a data region into chunks of roughly `target` bytes, each ending just after
/// a `\n` (or at the end of the file), so every line lies wholly in one chunk.
pub fn chunks(region: &[u8], target: usize) -> Vec<&[u8]> {
    let mut out = Vec::new();
    let mut start = 0;
    while start < region.len() {
        let mut end = (start + target).min(region.len());
        if end < region.len() {
            end = match memchr::memchr(b'\n', &region[end..]) {
                Some(i) => end + i + 1,
                None => region.len(),
            };
        }
        out.push(&region[start..end]);
        start = end;
    }
    out
}

/// Call `f` for every line of a chunk (without its `\n`). Empty lines are included;
/// the field splitter rejects them as malformed. A final segment with no `\n` is a
/// line only if it is non-empty (it cannot be empty: that would mean the chunk ended
/// with `\n`).
#[inline]
pub fn for_each_line<'a>(chunk: &'a [u8], mut f: impl FnMut(&'a [u8])) {
    let mut start = 0;
    for nl in memchr::memchr_iter(b'\n', chunk) {
        f(&chunk[start..nl]);
        start = nl + 1;
    }
    if start < chunk.len() {
        f(&chunk[start..]);
    }
}

/// Split a line into exactly N fields under the dialect. Returns None for a
/// malformed line. Quotes around a whole field are removed; an empty slice is null.
#[inline]
pub fn split<const N: usize>(line: &[u8]) -> Option<[&[u8]; N]> {
    if line.is_empty() {
        return None;
    }
    let mut out: [&[u8]; N] = [&[]; N];
    let mut field = 0usize;
    let mut start = 0usize;
    let len = line.len();
    loop {
        // find the end of this field
        let end = match memchr::memchr(b',', &line[start..]) {
            Some(i) => start + i,
            None => len,
        };
        if field >= N {
            return None; // too many fields
        }
        let raw = &line[start..end];
        out[field] = unquote(raw)?;
        field += 1;
        if end == len {
            break;
        }
        start = end + 1;
    }
    if field != N {
        return None;
    }
    Some(out)
}

#[inline]
fn unquote(raw: &[u8]) -> Option<&[u8]> {
    match memchr::memchr(b'"', raw) {
        None => Some(raw),
        Some(0) => {
            // must be "..." with no other quote inside
            if raw.len() >= 2 && raw[raw.len() - 1] == b'"' {
                let inner = &raw[1..raw.len() - 1];
                if memchr::memchr(b'"', inner).is_none() {
                    return Some(inner);
                }
            }
            None
        }
        Some(_) => None,
    }
}

/// Map header names to positions. If the header (parsed with the same dialect) is a
/// permutation of the expected names, use it; otherwise assume the documented order.
pub fn header_order<const N: usize>(bytes: &[u8], expected: [&str; N]) -> [usize; N] {
    let mut order = [0usize; N];
    for (i, o) in order.iter_mut().enumerate() {
        *o = i;
    }
    let line = match memchr::memchr(b'\n', bytes) {
        Some(i) => &bytes[..i],
        None => bytes,
    };
    let Some(names) = split::<N>(line) else { return order };
    let mut found = [usize::MAX; N];
    for (pos, name) in names.iter().enumerate() {
        if let Some(k) = expected.iter().position(|e| e.as_bytes() == *name) {
            if found[k] != usize::MAX {
                return order; // duplicate name: not a permutation
            }
            found[k] = pos;
        }
    }
    if found.iter().all(|&p| p != usize::MAX) { found } else { order }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dialect() {
        assert_eq!(split::<2>(b"a,b"), Some([&b"a"[..], b"b"]));
        assert_eq!(split::<2>(b"\"a\",\"\""), Some([&b"a"[..], b""]));
        assert_eq!(split::<2>(b" a,b "), Some([&b" a"[..], b"b "]));
        assert_eq!(split::<2>(b"a,b\r"), Some([&b"a"[..], b"b\r"]));
        assert_eq!(split::<2>(b""), None);
        assert_eq!(split::<2>(b"a"), None);
        assert_eq!(split::<2>(b"a,b,c"), None);
        assert_eq!(split::<2>(b"a,b\""), None);
        assert_eq!(split::<2>(b"\"a,b"), None);
        assert_eq!(split::<2>(b"\",b"), None);
        assert_eq!(split::<2>(b"\"\"\",b"), None);
        assert_eq!(split::<2>(b"a\"b\",c"), None);
        assert_eq!(split::<2>(b",,"), None);
        assert_eq!(split::<2>(b","), Some([&b""[..], b""]));
        assert_eq!(split::<1>(b"\"\""), Some([&b""[..]]));
    }

    #[test]
    fn lines_and_chunks() {
        let data = b"h1\na\n\nb\nc";
        let region = data_region(data);
        let mut got = Vec::new();
        for c in chunks(region, 1) {
            for_each_line(c, |l| got.push(l.to_vec()));
        }
        assert_eq!(got, vec![b"a".to_vec(), b"".to_vec(), b"b".to_vec(), b"c".to_vec()]);
        assert!(data_region(b"header only").is_empty());
        assert!(data_region(b"header\n").is_empty());
    }
}

// ====================================================================== night-04: fused scanner

/// Bitmasks of `\n`, `,` and `"` for 16 bytes at `p` (bit i = byte p+i).
#[inline(always)]
fn masks16(p: &[u8; 16]) -> (u32, u32, u32) {
    #[cfg(target_arch = "x86_64")]
    unsafe {
        use std::arch::x86_64::*;
        // SSE2 is part of the x86_64 baseline: no runtime detection needed.
        let v = _mm_loadu_si128(p.as_ptr() as *const __m128i);
        let nl = _mm_movemask_epi8(_mm_cmpeq_epi8(v, _mm_set1_epi8(b'\n' as i8))) as u32;
        let c = _mm_movemask_epi8(_mm_cmpeq_epi8(v, _mm_set1_epi8(b',' as i8))) as u32;
        let q = _mm_movemask_epi8(_mm_cmpeq_epi8(v, _mm_set1_epi8(b'"' as i8))) as u32;
        (nl, c, q)
    }
    #[cfg(not(target_arch = "x86_64"))]
    {
        let (mut nl, mut c, mut q) = (0u32, 0u32, 0u32);
        for (i, &b) in p.iter().enumerate() {
            nl |= ((b == b'\n') as u32) << i;
            c |= ((b == b',') as u32) << i;
            q |= ((b == b'"') as u32) << i;
        }
        (nl, c, q)
    }
}

/// One pass over a chunk: for every line (same lines as `for_each_line`), call `f` with
/// the line's N fields under the dialect (`split::<N>`), or None if it is malformed.
/// Lines up to 128 bytes take a mask-based fast path; longer ones fall back to `split`.
/// (Night-04 scanner; RW9 restored it after RW4's structural index measured slower on x86.)
#[inline]
pub fn for_each_record<'a, const N: usize>(chunk: &'a [u8], mut f: impl FnMut(Option<[&'a [u8]; N]>)) {
    let len = chunk.len();
    let mut start = 0usize;
    #[allow(unused_assignments)]
    let mut tail = [b'\n'; 16];
    while start < len {
        // gather masks block by block until the line's '\n' (or a long line)
        let (mut cm, mut qm) = (0u128, 0u128);
        let mut k = 0usize; // blocks read
        let line_len;
        loop {
            let p = start + 16 * k;
            let blk: &[u8; 16] = if p + 16 <= len {
                chunk[p..p + 16].try_into().unwrap()
            } else {
                // past the chunk: pad with '\n', which ends a final line without one
                tail = [b'\n'; 16];
                let n = len.saturating_sub(p);
                tail[..n].copy_from_slice(&chunk[p..p + n]);
                &tail
            };
            let (nl, c, q) = masks16(blk);
            if k < 8 {
                cm |= (c as u128) << (16 * k);
                qm |= (q as u128) << (16 * k);
            }
            if nl != 0 {
                line_len = 16 * k + nl.trailing_zeros() as usize;
                break;
            }
            k += 1;
        }
        let end = (start + line_len).min(len);
        let line = &chunk[start..end];
        start = start + line_len + 1;
        if line_len > 128 {
            f(split::<N>(line));
            continue;
        }
        if line_len == 0 {
            f(None);
            continue;
        }
        let keep = if line_len == 128 { u128::MAX } else { (1u128 << line_len) - 1 };
        let (mut cm, qm) = (cm & keep, qm & keep);
        if cm.count_ones() as usize != N - 1 {
            f(None);
            continue;
        }
        let mut out: [&'a [u8]; N] = [&[]; N];
        let mut s = 0usize;
        let mut ok = true;
        for slot in out.iter_mut() {
            let e = if cm == 0 { line_len } else { cm.trailing_zeros() as usize };
            cm &= cm.wrapping_sub(1);
            // field [s, e): no quote, or exactly one at s and one at e-1 (e-1 > s)
            let range = if e - s == 128 { u128::MAX } else { ((1u128 << (e - s)) - 1) << s };
            let fq = qm & range;
            if fq == 0 {
                *slot = &line[s..e];
            } else if e >= s + 2 && fq == ((1u128 << s) | (1u128 << (e - 1))) {
                *slot = &line[s + 1..e - 1];
            } else {
                ok = false;
                break;
            }
            s = e + 1;
        }
        f(if ok { Some(out) } else { None });
    }
}


// ====================================================================== RW4: structural index

/// Lines starting in a window of `WIN` bytes are parsed from masks computed once for the
/// window plus `OV` bytes of slack (a line longer than the slack takes the exact slow path).
const WIN: usize = 64 * 1024;
const OV: usize = 4096;
const MB: usize = (WIN + OV) / 64;

/// Bitmasks of `\n`, `,` and `"` for 64 bytes (bit i = byte i).
#[inline(always)]
fn masks64(p: &[u8; 64]) -> (u64, u64, u64) {
    // RW28: AVX2 when the build targets it (x86-64-v3, see .cargo/config.toml)
    #[cfg(all(target_arch = "x86_64", target_feature = "avx2"))]
    unsafe {
        use std::arch::x86_64::*;
        let (n, c, q) = (_mm256_set1_epi8(b'\n' as i8), _mm256_set1_epi8(b',' as i8), _mm256_set1_epi8(b'"' as i8));
        let lo = _mm256_loadu_si256(p.as_ptr() as *const __m256i);
        let hi = _mm256_loadu_si256(p.as_ptr().add(32) as *const __m256i);
        let m = |v: __m256i, d: __m256i| _mm256_movemask_epi8(_mm256_cmpeq_epi8(v, d)) as u32 as u64;
        return (m(lo, n) | m(hi, n) << 32, m(lo, c) | m(hi, c) << 32, m(lo, q) | m(hi, q) << 32);
    }
    #[cfg(all(target_arch = "x86_64", not(target_feature = "avx2")))]
    unsafe {
        use std::arch::x86_64::*;
        // SSE2 is in the x86_64 baseline: no runtime detection needed.
        let (n, c, q) = (_mm_set1_epi8(b'\n' as i8), _mm_set1_epi8(b',' as i8), _mm_set1_epi8(b'"' as i8));
        let (mut nl, mut cm, mut qm) = (0u64, 0u64, 0u64);
        for k in 0..4 {
            let v = _mm_loadu_si128(p.as_ptr().add(16 * k) as *const __m128i);
            nl |= (_mm_movemask_epi8(_mm_cmpeq_epi8(v, n)) as u16 as u64) << (16 * k);
            cm |= (_mm_movemask_epi8(_mm_cmpeq_epi8(v, c)) as u16 as u64) << (16 * k);
            qm |= (_mm_movemask_epi8(_mm_cmpeq_epi8(v, q)) as u16 as u64) << (16 * k);
        }
        (nl, cm, qm)
    }
    #[cfg(target_arch = "aarch64")]
    unsafe {
        use std::arch::aarch64::*;
        const W: [u8; 16] = [1, 2, 4, 8, 16, 32, 64, 128, 1, 2, 4, 8, 16, 32, 64, 128];
        let w = vld1q_u8(W.as_ptr());
        let v = [vld1q_u8(p.as_ptr()), vld1q_u8(p.as_ptr().add(16)), vld1q_u8(p.as_ptr().add(32)), vld1q_u8(p.as_ptr().add(48))];
        // 64 compare lanes -> 64-bit mask (pairwise-add reduction of bit-weighted lanes)
        let mm = |ch: u8| -> u64 {
            let d = vdupq_n_u8(ch);
            let t0 = vandq_u8(vceqq_u8(v[0], d), w);
            let t1 = vandq_u8(vceqq_u8(v[1], d), w);
            let t2 = vandq_u8(vceqq_u8(v[2], d), w);
            let t3 = vandq_u8(vceqq_u8(v[3], d), w);
            let s0 = vpaddq_u8(t0, t1);
            let s1 = vpaddq_u8(t2, t3);
            let s0 = vpaddq_u8(s0, s1);
            let s0 = vpaddq_u8(s0, s0);
            vgetq_lane_u64(vreinterpretq_u64_u8(s0), 0)
        };
        (mm(b'\n'), mm(b','), mm(b'"'))
    }
    #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
    {
        let (mut nl, mut c, mut q) = (0u64, 0u64, 0u64);
        for (i, &b) in p.iter().enumerate() {
            nl |= ((b == b'\n') as u64) << i;
            c |= ((b == b',') as u64) << i;
            q |= ((b == b'"') as u64) << i;
        }
        (nl, c, q)
    }
}

/// First set bit in `[from, to)` of a mask array, if any.
#[inline(always)]
fn next_bit(m: &[u64], from: usize, to: usize) -> Option<usize> {
    let mut b = from / 64;
    let mut x = m[b] & (!0u64 << (from % 64));
    loop {
        if x != 0 {
            let p = b * 64 + x.trailing_zeros() as usize;
            return if p < to { Some(p) } else { None };
        }
        b += 1;
        if b * 64 >= to {
            return None;
        }
        x = m[b];
    }
}

/// Bits of block `b` restricted to `[lo, hi)`.
#[inline(always)]
fn bits_in(m: &[u64], b: usize, lo: usize, hi: usize) -> u64 {
    let mut x = m[b];
    if b == lo / 64 {
        x &= !0u64 << (lo % 64);
    }
    let end = hi - b * 64;
    if end < 64 {
        x &= (1u64 << end) - 1;
    }
    x
}

/// RW4 structural-index scanner: masks per 64 KiB window, bit-scan line cutting. Same
/// results as `for_each_record` (tested), but measured ~9% slower end to end on x86, so it
/// is kept only for `SS_EXP` comparisons.
#[allow(dead_code)]
#[inline]
pub fn for_each_record_idx<'a, const N: usize>(chunk: &'a [u8], mut f: impl FnMut(Option<[&'a [u8]; N]>)) {
    let len = chunk.len();
    let mut nlm = [0u64; MB];
    let mut cmm = [0u64; MB];
    let mut qmm = [0u64; MB];
    let mut s = 0usize; // next line start
    while s < len {
        let w = s;
        let wend = (w + WIN + OV).min(len);
        let nblk = (wend - w).div_ceil(64);
        for b in 0..nblk {
            let p = w + 64 * b;
            let (a, c, q) = if p + 64 <= len {
                masks64(chunk[p..p + 64].try_into().unwrap())
            } else {
                let mut pad = [0u8; 64];
                pad[..len - p].copy_from_slice(&chunk[p..len]);
                masks64(&pad)
            };
            nlm[b] = a;
            cmm[b] = c;
            qmm[b] = q;
        }
        let limit = (w + WIN).min(len);
        while s < limit {
            let rs = s - w;
            let re = match next_bit(&nlm, rs, wend - w) {
                Some(re) => re,
                None if wend == len => len - w, // final line without '\n'
                None => {
                    // longer than the window slack: exact slow path, then a fresh window
                    let e = memchr::memchr(b'\n', &chunk[s..]).map_or(len, |i| s + i);
                    f(if e == s { None } else { split::<N>(&chunk[s..e]) });
                    s = e + 1;
                    break;
                }
            };
            let e = w + re;
            if re == rs {
                f(None); // empty line
            } else if (rs / 64..=(re - 1) / 64).any(|b| bits_in(&qmm, b, rs, re) != 0) {
                f(split::<N>(&chunk[s..e]));
            } else {
                let mut out: [&'a [u8]; N] = [&[]; N];
                let (mut k, mut fs) = (0usize, s);
                let mut ok = true;
                'blocks: for b in rs / 64..=(re - 1) / 64 {
                    let mut x = bits_in(&cmm, b, rs, re);
                    while x != 0 {
                        let p = w + b * 64 + x.trailing_zeros() as usize;
                        if k == N - 1 {
                            ok = false; // too many fields
                            break 'blocks;
                        }
                        out[k] = &chunk[fs..p];
                        k += 1;
                        fs = p + 1;
                        x &= x - 1;
                    }
                }
                if ok && k == N - 1 {
                    out[N - 1] = &chunk[fs..e];
                    f(Some(out));
                } else {
                    f(None);
                }
            }
            s = e + 1;
        }
    }
}


// ====================================================================== RW9: SWAR line splitter

const LO7: u64 = 0x7f7f_7f7f_7f7f_7f7f;

/// Exact per-byte equality mask: the high bit of byte i is set iff byte i of `w` == `c`.
/// (No false positives: ((t & 0x7f..) + 0x7f..) sets the high bit for every non-zero low
/// 7 bits, and `| t` covers bytes whose own high bit is set.)
#[inline(always)]
fn eq_mask(w: u64, c: u8) -> u64 {
    let t = w ^ (0x0101_0101_0101_0101u64 * c as u64);
    !(((t & LO7).wrapping_add(LO7)) | t | LO7)
}

/// Split one line (without its '\n') into N fields: commas found 8 bytes at a time.
/// A line containing any quote is re-split by the exact `split`.
#[inline(always)]
pub fn split_swar<const N: usize>(line: &[u8]) -> Option<[&[u8]; N]> {
    let n = line.len();
    if n == 0 {
        return None;
    }
    let mut out: [&[u8]; N] = [&[]; N];
    let (mut k, mut fs, mut i) = (0usize, 0usize, 0usize);
    let mut quotes = 0u64;
    while i + 8 <= n {
        let w = u64::from_le_bytes(line[i..i + 8].try_into().unwrap());
        quotes |= eq_mask(w, b'"');
        let mut m = eq_mask(w, b',');
        while m != 0 {
            let p = i + (m.trailing_zeros() / 8) as usize;
            if k == N - 1 {
                return None; // too many fields (every comma separates, quoted or not)
            }
            out[k] = &line[fs..p];
            k += 1;
            fs = p + 1;
            m &= m - 1;
        }
        i += 8;
    }
    while i < n {
        let b = line[i];
        if b == b',' {
            if k == N - 1 {
                return None;
            }
            out[k] = &line[fs..i];
            k += 1;
            fs = i + 1;
        } else if b == b'"' {
            quotes = 1;
        }
        i += 1;
    }
    if k != N - 1 {
        return None;
    }
    if quotes != 0 {
        return split::<N>(line);
    }
    out[N - 1] = &line[fs..];
    Some(out)
}

/// RW9 scanner: lines by memchr, fields by `split_swar`. Same results as `split` per line.
#[inline]
pub fn for_each_record_swar<'a, const N: usize>(chunk: &'a [u8], mut f: impl FnMut(Option<[&'a [u8]; N]>)) {
    let mut start = 0;
    for nl in memchr::memchr_iter(b'\n', chunk) {
        f(split_swar::<N>(&chunk[start..nl]));
        start = nl + 1;
    }
    if start < chunk.len() {
        f(split_swar::<N>(&chunk[start..]));
    }
}


// ====================================================================== RW10: two-stage index scanner

/// Profiling only: stage 1 of RW10 alone (masks + position list), returns positions found.
#[allow(dead_code)]
pub fn ix_stage1(chunk: &[u8]) -> usize {
    let len = chunk.len();
    let mut buf: Vec<u32> = vec![0u32; WIN + OV + 64 + 16];
    let (mut total, mut w) = (0usize, 0usize);
    while w < len {
        let wend = (w + WIN).min(len);
        let mut n = 0usize;
        let nblk = (wend - w).div_ceil(64);
        for b in 0..nblk {
            let p = w + 64 * b;
            let (a, c, q) = if p + 64 <= len {
                masks64(chunk[p..p + 64].try_into().unwrap())
            } else {
                let mut pad = [0u8; 64];
                pad[..len - p].copy_from_slice(&chunk[p..len]);
                masks64(&pad)
            };
            push_bits(&mut buf, &mut n, (64 * b) as u32, a | c | q);
        }
        total += n;
        w = wend;
    }
    total
}

/// Profiling only: masks alone.
#[allow(dead_code)]
pub fn masks_only(chunk: &[u8]) -> u64 {
    let mut acc = 0u64;
    let mut p = 0;
    while p + 64 <= chunk.len() {
        let (a, c, q) = masks64(chunk[p..p + 64].try_into().unwrap());
        acc = acc.wrapping_add(a ^ c ^ q);
        p += 64;
    }
    acc
}

/// Append the positions (base + bit index) of the set bits of `m` to `buf` at `*n`. The
/// first 8 are written unconditionally (branch-free; extra writes land past `*n` and are
/// overwritten later), so a block with up to 8 separators costs no data-dependent branch.
#[inline(always)]
fn push_bits(buf: &mut [u32], n: &mut usize, base: u32, mut m: u64) {
    let cnt = m.count_ones() as usize;
    let at = *n;
    let dst = &mut buf[at..at + 8];
    for d in dst.iter_mut() {
        *d = base + m.trailing_zeros();
        m &= m.wrapping_sub(1);
    }
    if cnt > 8 {
        for j in 8..cnt {
            buf[at + j] = base + m.trailing_zeros();
            m &= m.wrapping_sub(1);
        }
    }
    *n = at + cnt;
}

/// RW10 scanner: stage 1 lists every `,` `\n` `"` position of a window (SIMD masks,
/// branch-free bit extraction); stage 2 takes the next N positions per line and, when they
/// are N-1 commas then a newline, slices the fields directly. Any other pattern (quotes,
/// wrong field count, empty line) re-splits the line with the exact `split`.
#[inline]
pub fn for_each_record_ix<'a, const N: usize>(chunk: &'a [u8], mut f: impl FnMut(Option<[&'a [u8]; N]>)) {
    let len = chunk.len();
    let mut buf: Vec<u32> = vec![0u32; WIN + OV + 64 + 16];
    let byte = |p: usize| -> u8 { if p < len { chunk[p] } else { b'\n' } };
    let mut s = 0usize;
    while s < len {
        let w = s;
        let wend = (w + WIN + OV).min(len);
        // ---- stage 1: separator positions, relative to w
        let mut n = 0usize;
        let nblk = (wend - w).div_ceil(64);
        for b in 0..nblk {
            let p = w + 64 * b;
            let (a, c, q) = if p + 64 <= len {
                masks64(chunk[p..p + 64].try_into().unwrap())
            } else {
                let mut pad = [0u8; 64];
                pad[..len - p].copy_from_slice(&chunk[p..len]);
                masks64(&pad)
            };
            let valid = if p + 64 <= wend { !0u64 } else { (1u64 << (wend - p)) - 1 };
            push_bits(&mut buf, &mut n, (64 * b) as u32, (a | c | q) & valid);
        }
        if wend == len && chunk[len - 1] != b'\n' {
            buf[n] = (len - w) as u32; // virtual newline ending the final line
            n += 1;
        }
        // ---- stage 2: lines starting in [w, w + WIN). Each field is either unquoted (the
        // next listed position is its separator) or quoted: an opening quote at the field
        // start, the next listed position is the closing quote, and the position right after
        // it is the separator. Anything else is malformed, exactly as `split` decides.
        let limit = (w + WIN).min(len);
        let mut t = 0usize;
        enum V {
            Line(bool), // the line's newline was consumed; fields ok?
            Bad,        // malformed before its newline; positions from `u` on are unconsumed
            Out,        // ran out of listed positions (line crosses the window slack)
        }
        while s < limit {
            let mut out: [&'a [u8]; N] = [&[]; N];
            let (mut k, mut fs, mut u) = (0usize, s, t);
            let verdict = loop {
                if u >= n {
                    break V::Out;
                }
                let p = w + buf[u] as usize;
                let (field, sep, nu): (&'a [u8], usize, usize);
                if byte(p) == b'"' {
                    if p != fs {
                        break V::Bad; // a quote not at the start of the field
                    }
                    if u + 1 >= n {
                        break V::Out;
                    }
                    let p2 = w + buf[u + 1] as usize;
                    if byte(p2) != b'"' {
                        break V::Bad; // opened but not closed before a separator
                    }
                    if u + 2 >= n {
                        break V::Out;
                    }
                    let p3 = w + buf[u + 2] as usize;
                    let c3 = byte(p3);
                    if p3 != p2 + 1 || (c3 != b',' && c3 != b'\n') {
                        break V::Bad; // closing quote not at the end of the field
                    }
                    field = &chunk[p + 1..p2];
                    sep = p3;
                    nu = u + 3;
                } else {
                    field = &chunk[fs..p];
                    sep = p;
                    nu = u + 1;
                }
                if k == N {
                    break V::Bad; // too many fields (u still points at this field)
                }
                out[k] = field;
                k += 1;
                fs = sep + 1;
                u = nu;
                if byte(sep) == b'\n' {
                    break V::Line(k == N);
                }
            };
            match verdict {
                V::Line(ok) => {
                    f(if ok { Some(out) } else { None });
                    s = fs;
                    t = u;
                }
                V::Bad => {
                    let mut v = u;
                    while v < n && byte(w + buf[v] as usize) != b'\n' {
                        v += 1;
                    }
                    if v < n {
                        f(None);
                        s = w + buf[v] as usize + 1;
                        t = v + 1;
                    } else {
                        let e = memchr::memchr(b'\n', &chunk[s..]).map_or(len, |i| s + i);
                        f(if e == s { None } else { split::<N>(&chunk[s..e]) });
                        s = e + 1;
                        break;
                    }
                }
                V::Out => {
                    let e = memchr::memchr(b'\n', &chunk[s..]).map_or(len, |i| s + i);
                    f(if e == s { None } else { split::<N>(&chunk[s..e]) });
                    s = e + 1;
                    break;
                }
            }
        }
    }
}


// ====================================================================== RW16: branch-free quotes

/// RW16 scanner: stage 1 lists separator positions (`,` and `\n`) and keeps a quote bitmask
/// per 64-byte block; stage 2 takes the next N separators of a line and, when they are
/// N-1 commas then a newline, cuts the fields with a branch-free quote rule:
///   q = field starts with '"', qe = field ends with '"' (for a field of 2+ bytes);
///   every field needs q == qe (and 2+ bytes if quoted), and the line's quote count must
///   equal the sum of q + qe, so any other quote (inner, lone, misplaced) is caught.
/// Lines failing either check, and every irregular line, are re-split by the exact `split`.
#[inline]
pub fn for_each_record_bq<'a, const N: usize>(chunk: &'a [u8], mut f: impl FnMut(Option<[&'a [u8]; N]>)) {
    let len = chunk.len();
    let mut seps: Vec<u32> = vec![0u32; WIN + OV + 64 + 16];
    let mut qmm = [0u64; MB];
    let mut s = 0usize;
    // the final line of a chunk without '\n' is handled by the exact path below
    let body = if len > 0 && chunk[len - 1] != b'\n' { chunk[..len].iter().rposition(|&b| b == b'\n').map_or(0, |i| i + 1) } else { len };
    while s < body {
        let w = s;
        let wend = (w + WIN + OV).min(body);
        let mut n = 0usize;
        let nblk = (wend - w).div_ceil(64);
        for b in 0..nblk {
            let p = w + 64 * b;
            let (a, c, q) = if p + 64 <= len {
                masks64(chunk[p..p + 64].try_into().unwrap())
            } else {
                let mut pad = [0u8; 64];
                pad[..len - p].copy_from_slice(&chunk[p..len]);
                masks64(&pad)
            };
            let valid = if p + 64 <= wend { !0u64 } else { (1u64 << (wend - p)) - 1 };
            push_bits(&mut seps, &mut n, (64 * b) as u32, (a | c) & valid);
            qmm[b] = q & valid;
        }
        let limit = (w + WIN).min(body);
        let mut t = 0usize;
        while s < limit {
            if t + N <= n {
                // every listed position is < body <= len, so indexing is in bounds
                let e = w + seps[t + N - 1] as usize;
                let mut ok = chunk[e] == b'\n';
                for j in 0..N - 1 {
                    ok &= chunk[w + seps[t + j] as usize] == b',';
                }
                if ok {
                    let mut out: [&'a [u8]; N] = [&[]; N];
                    let mut fs = s;
                    let mut qsum = 0u32;
                    for j in 0..N {
                        let fe = w + seps[t + j] as usize;
                        let l = fe - fs;
                        let q = (l > 0 && chunk[fs] == b'"') as usize;
                        let qe = (l > 1 && chunk[fe - 1] == b'"') as usize;
                        ok &= q == qe;
                        qsum += (q + qe) as u32;
                        out[j] = &chunk[fs + q..fe - qe];
                        fs = fe + 1;
                    }
                    // quotes in [s, e) from the per-block masks
                    let (rs, re) = (s - w, e - w);
                    let mut qn = 0u32;
                    for b in rs / 64..=(re.max(rs + 1) - 1) / 64 {
                        let mut x = qmm[b];
                        if b == rs / 64 {
                            x &= !0u64 << (rs % 64);
                        }
                        let end = re - b * 64;
                        if end < 64 {
                            x &= (1u64 << end) - 1;
                        }
                        qn += x.count_ones();
                    }
                    if ok && qn == qsum {
                        f(Some(out));
                    } else {
                        f(split::<N>(&chunk[s..e]));
                    }
                    s = e + 1;
                    t += N;
                    continue;
                }
            }
            // irregular line: its newline among the listed separators, then exact split
            let mut u = t;
            while u < n && chunk[w + seps[u] as usize] != b'\n' {
                u += 1;
            }
            if u < n {
                let e = w + seps[u] as usize;
                f(if e == s { None } else { split::<N>(&chunk[s..e]) });
                s = e + 1;
                t = u + 1;
            } else {
                let e = memchr::memchr(b'\n', &chunk[s..]).map_or(len, |i| s + i);
                f(if e == s { None } else { split::<N>(&chunk[s..e]) });
                s = e + 1;
                break;
            }
        }
    }
    // final line without '\n'
    if s < len {
        f(split::<N>(&chunk[s..len]));
    }
}

#[cfg(test)]
mod fused_tests {
    use super::*;

    fn lcg(x: &mut u64) -> u64 {
        *x = x.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        *x >> 33
    }

    fn reference<const N: usize>(chunk: &[u8]) -> Vec<Option<Vec<Vec<u8>>>> {
        let mut v = Vec::new();
        for_each_line(chunk, |l| v.push(split::<N>(l).map(|a| a.iter().map(|x| x.to_vec()).collect())));
        v
    }
    fn fused<const N: usize>(chunk: &[u8]) -> Vec<Option<Vec<Vec<u8>>>> {
        let mut v = Vec::new();
        for_each_record::<N>(chunk, |r| v.push(r.map(|a| a.iter().map(|x| x.to_vec()).collect())));
        let mut w = Vec::new();
        for_each_record_idx::<N>(chunk, |r| w.push(r.map(|a| a.iter().map(|x| x.to_vec()).collect())));
        assert_eq!(v, w, "night-04 and RW4 scanners disagree");
        let mut z = Vec::new();
        for_each_record_swar::<N>(chunk, |r| z.push(r.map(|a| a.iter().map(|x| x.to_vec()).collect())));
        assert_eq!(v, z, "night-04 and RW9 SWAR scanners disagree");
        let mut y = Vec::new();
        for_each_record_ix::<N>(chunk, |r| y.push(r.map(|a| a.iter().map(|x| x.to_vec()).collect())));
        assert_eq!(v, y, "night-04 and RW10 index scanners disagree");
        let mut x = Vec::new();
        for_each_record_bq::<N>(chunk, |r| x.push(r.map(|a| a.iter().map(|x| x.to_vec()).collect())));
        assert_eq!(v, x, "night-04 and RW16 branch-free-quote scanners disagree");
        v
    }

    #[test]
    fn eq_mask_exact() {
        let mut x = 3u64;
        for _ in 0..200_000 {
            let w = lcg(&mut x) << 32 | lcg(&mut x);
            let w = w ^ (lcg(&mut x) << 40); // mix in high bytes
            for c in [b',', b'"', b'\n', 0u8, 0x80, 0xff] {
                let m = eq_mask(w, c);
                for i in 0..8 {
                    assert_eq!((m >> (8 * i + 7)) & 1 == 1, (w >> (8 * i)) as u8 == c);
                    assert_eq!((m >> (8 * i)) & 0x7f, 0);
                }
            }
        }
    }

    /// The fused scanner equals for_each_line + split on random dialect-noise chunks,
    /// including lines around the 16/128-byte block edges and chunks without a final '\n'.
    #[test]
    fn fused_matches_split() {
        let alpha: &[u8] = b"ab,,,\"\"\"\n\n\r 0z";
        let mut x = 7u64;
        for _ in 0..30_000 {
            let len = (lcg(&mut x) % 300) as usize;
            let mut c: Vec<u8> = Vec::with_capacity(len);
            while c.len() < len {
                match lcg(&mut x) % 6 {
                    0 => { // a near-valid line with long fields
                        let fl = (lcg(&mut x) % 40) as usize;
                        for i in 0..(1 + lcg(&mut x) % 8) {
                            if i > 0 { c.push(b','); }
                            let q = lcg(&mut x) % 3 == 0;
                            if q { c.push(b'"'); }
                            for _ in 0..fl { c.push(b'a' + (lcg(&mut x) % 26) as u8); }
                            if q { c.push(b'"'); }
                        }
                        c.push(b'\n');
                    }
                    _ => c.push(alpha[(lcg(&mut x) as usize) % alpha.len()]),
                }
            }
            assert_eq!(fused::<2>(&c), reference::<2>(&c), "{:?}", String::from_utf8_lossy(&c));
            assert_eq!(fused::<4>(&c), reference::<4>(&c), "{:?}", String::from_utf8_lossy(&c));
            assert_eq!(fused::<7>(&c), reference::<7>(&c), "{:?}", String::from_utf8_lossy(&c));
        }
        // RW4: window edges (64 KiB) and lines longer than the 4 KiB slack
        let mut big: Vec<u8> = Vec::new();
        let mut y = 99u64;
        while big.len() < 300_000 {
            match lcg(&mut y) % 50 {
                0 => { big.extend(std::iter::repeat_n(b'a', (lcg(&mut y) % 9000) as usize)); big.push(b','); big.push(b'b'); big.push(b'\n'); }
                1 => big.extend_from_slice(b"\"q\",x\n"),
                2 => big.push(b'\n'),
                _ => { for i in 0..(1 + lcg(&mut y) % 7) { if i > 0 { big.push(b','); } for _ in 0..(lcg(&mut y) % 12) { big.push(b'a' + (lcg(&mut y) % 26) as u8); } } big.push(b'\n'); }
            }
        }
        for cut in [big.len(), big.len() - 1, 65_535, 65_536, 65_537, 69_631, 69_632, 69_633, 131_072, 140_000] {
            let c = &big[..cut.min(big.len())];
            assert_eq!(fused::<2>(c), reference::<2>(c), "cut={cut}");
            assert_eq!(fused::<4>(c), reference::<4>(c), "cut={cut}");
            assert_eq!(fused::<7>(c), reference::<7>(c), "cut={cut}");
        }
        // exact block edges
        for n in [15usize, 16, 17, 31, 32, 33, 126, 127, 128, 129, 130, 200] {
            let mut line = vec![b'a'; n];
            line[n / 2] = b',';
            for tail in [true, false] {
                let mut c = line.clone();
                if tail { c.push(b'\n'); }
                assert_eq!(fused::<2>(&c), reference::<2>(&c), "n={n} tail={tail}");
            }
        }
    }
}
