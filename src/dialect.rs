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
