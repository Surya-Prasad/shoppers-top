//! RW8: global allocator that backs large allocations with 2 MiB transparent huge pages.
//!
//! The pipeline allocates gigabytes of fresh record vectors; with 4 KiB pages every first
//! touch is a page fault (~600k faults per 2.4 GB). On Linux, allocations of at least
//! `BIG` bytes are `mmap`ed directly, advised `MADV_HUGEPAGE`, and grown in place or moved
//! by `mremap` (no copy). Everything else goes to the system allocator. Other operating
//! systems use the system allocator unchanged.
//!
//! The large/small decision depends only on the size in the `Layout`, which `dealloc` and
//! `realloc` receive unchanged, so every block is freed by the allocator that made it.

use std::alloc::{GlobalAlloc, Layout, System};

pub struct HugeAlloc;

#[cfg(target_os = "linux")]
const BIG: usize = 4 << 20;
#[cfg(target_os = "linux")]
const HUGE: usize = 2 << 20;

#[cfg(target_os = "linux")]
#[inline]
fn is_big(layout: &Layout) -> bool {
    layout.size() >= BIG && layout.align() <= 4096
}

#[cfg(target_os = "linux")]
#[inline]
fn mapped_len(size: usize) -> usize {
    size.div_ceil(HUGE) * HUGE
}

unsafe impl GlobalAlloc for HugeAlloc {
    #[inline]
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        #[cfg(target_os = "linux")]
        if is_big(&layout) {
            unsafe {
                let len = mapped_len(layout.size());
                let p = libc::mmap(
                    std::ptr::null_mut(),
                    len,
                    libc::PROT_READ | libc::PROT_WRITE,
                    libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                    -1,
                    0,
                );
                if p == libc::MAP_FAILED {
                    return std::ptr::null_mut();
                }
                libc::madvise(p, len, libc::MADV_HUGEPAGE);
                return p as *mut u8;
            }
        }
        unsafe { System.alloc(layout) }
    }

    #[inline]
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        #[cfg(target_os = "linux")]
        if is_big(&layout) {
            // fresh anonymous mappings are already zero
            return unsafe { self.alloc(layout) };
        }
        unsafe { System.alloc_zeroed(layout) }
    }

    #[inline]
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        #[cfg(target_os = "linux")]
        if is_big(&layout) {
            unsafe {
                libc::munmap(ptr as *mut libc::c_void, mapped_len(layout.size()));
            }
            return;
        }
        unsafe { System.dealloc(ptr, layout) }
    }

    #[inline]
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        #[cfg(target_os = "linux")]
        {
            let new_layout = unsafe { Layout::from_size_align_unchecked(new_size, layout.align()) };
            match (is_big(&layout), is_big(&new_layout)) {
                (true, true) => unsafe {
                    let (old_len, new_len) = (mapped_len(layout.size()), mapped_len(new_size));
                    if old_len == new_len {
                        return ptr;
                    }
                    let p = libc::mremap(ptr as *mut libc::c_void, old_len, new_len, libc::MREMAP_MAYMOVE);
                    if p == libc::MAP_FAILED {
                        return std::ptr::null_mut();
                    }
                    if new_len > old_len {
                        libc::madvise(p, new_len, libc::MADV_HUGEPAGE);
                    }
                    return p as *mut u8;
                },
                (false, false) => return unsafe { System.realloc(ptr, layout, new_size) },
                _ => unsafe {
                    // crossing the threshold: move between allocators
                    let q = self.alloc(new_layout);
                    if !q.is_null() {
                        std::ptr::copy_nonoverlapping(ptr, q, layout.size().min(new_size));
                        self.dealloc(ptr, layout);
                    }
                    return q;
                },
            }
        }
        #[cfg(not(target_os = "linux"))]
        unsafe {
            System.realloc(ptr, layout, new_size)
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn big_and_small_vectors_roundtrip() {
        // exercises alloc, growth across the threshold, mremap growth, shrink and free
        let mut v: Vec<u64> = Vec::new();
        for i in 0..3_000_000u64 {
            v.push(i);
        }
        assert!(v.iter().enumerate().all(|(i, &x)| x == i as u64));
        v.truncate(10);
        v.shrink_to_fit();
        assert_eq!(v, (0..10).collect::<Vec<_>>());
        let z = vec![0u8; 9 << 20];
        assert!(z.iter().all(|&b| b == 0));
        let mut w = vec![1u32; 2 << 20];
        w.resize(5 << 20, 7);
        assert_eq!(w[(2 << 20) - 1], 1);
        assert_eq!(w[(5 << 20) - 1], 7);
    }
}
/// v2: an anonymous, 2 MiB-aligned mapping of at least `size` bytes advised for transparent
/// huge pages (`MADV_HUGEPAGE`), zero-filled by the kernel on first touch; returns
/// (pointer, mapped length) or None. Off Linux: a zeroed system allocation.
pub fn map_huge(size: usize) -> Option<(*mut u8, usize)> {
    #[cfg(target_os = "linux")]
    unsafe {
        let len = size.max(1).div_ceil(HUGE) * HUGE;
        let raw = libc::mmap(std::ptr::null_mut(), len + HUGE, libc::PROT_READ | libc::PROT_WRITE, libc::MAP_PRIVATE | libc::MAP_ANONYMOUS, -1, 0);
        if raw == libc::MAP_FAILED {
            return None;
        }
        let a = raw as usize;
        let p = (a + HUGE - 1) & !(HUGE - 1);
        if p > a {
            libc::munmap(raw, p - a);
        }
        let tail = a + len + HUGE - (p + len);
        if tail > 0 {
            libc::munmap((p + len) as *mut libc::c_void, tail);
        }
        libc::madvise(p as *mut libc::c_void, len, libc::MADV_HUGEPAGE);
        Some((p as *mut u8, len))
    }
    #[cfg(not(target_os = "linux"))]
    unsafe {
        let len = size.max(1).div_ceil(4096) * 4096;
        let p = System.alloc_zeroed(Layout::from_size_align(len, 4096).ok()?);
        (!p.is_null()).then_some((p, len))
    }
}

/// Release a `map_huge` block.
pub fn unmap_huge(p: *mut u8, len: usize) {
    #[cfg(target_os = "linux")]
    unsafe {
        libc::munmap(p as *mut libc::c_void, len);
    }
    #[cfg(not(target_os = "linux"))]
    unsafe {
        System.dealloc(p, Layout::from_size_align_unchecked(len, 4096));
    }
}
