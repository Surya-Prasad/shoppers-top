//! Select event winners in their scan buffers. Only the index and loser bits
//! are written during deduplication.

use std::cell::RefCell;

use crate::{EvRec, keys::same_id, packed::fmix};

const LOCATION_BITS: u32 = 24;
const LOCATION_MASK: u32 = (1 << LOCATION_BITS) - 1;

struct Scratch {
    slots: Vec<u32>,
    losers: Vec<u64>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selects_exact_winners_across_slices() {
        let distinct_pointer = || {
            let id = Box::leak(b"opaque-event-id".to_vec().into_boxed_slice());
            crate::keys::local_key(id)
        };
        let a = distinct_pointer();
        let b = distinct_pointer();
        let row = |eid, rank, person, pc| EvRec { eid, rank, person, pc, hk: 123, ts: 0 };
        // All rows deliberately share a hash. The earliest long-ID copy is
        // invalid, and equal ranks for the other ID retain their first copy.
        let first = [row(a, 9, 1, 40), row(1, 3, 2, 50)];
        let second = [row(b, 2, 0, 1), row(1, 3, 3, 80), row(2, 6, 4, 90)];
        let mut winners = Vec::new();
        let kept = select(&[&first, &[], &second], |r| winners.push((r.person, r.pc)));
        assert_eq!(kept, Some(3));
        assert_eq!(winners, [(2, 50), (0, 1), (4, 90)]);
    }
}

thread_local! {
    static SCRATCH: RefCell<Scratch> = const {
        RefCell::new(Scratch { slots: Vec::new(), losers: Vec::new() })
    };
}

/// Return `None` before emitting anything if the partition exceeds the compact
/// index's limits. Each slot stores an eight-bit tag and a nonzero row locator.
/// The tag only filters probes. Equality of event IDs is always checked exactly.
pub(super) fn select(parts: &[&[EvRec]], mut emit: impl FnMut(&EvRec)) -> Option<usize> {
    let count: usize = parts.iter().map(|part| part.len()).sum();
    let max_len = parts.iter().map(|part| part.len()).max().unwrap_or(0);
    if count == 0 {
        return Some(0);
    }
    let stride = max_len.checked_next_power_of_two()?;
    let locations = parts.len().checked_mul(stride)?;
    if locations > LOCATION_MASK as usize {
        return None;
    }
    let shift = stride.trailing_zeros();
    let cap = (count + count.div_ceil(3)).next_power_of_two().max(16);
    let mask = cap - 1;

    SCRATCH.with(|scratch| {
        let Scratch { slots, losers } = &mut *scratch.borrow_mut();
        slots.clear();
        slots.resize(cap, 0);
        losers.clear();
        losers.resize(locations.div_ceil(64), 0);
        let mut duplicates = 0;

        for (pi, part) in parts.iter().enumerate() {
            let base = pi * stride;
            for (ri, row) in part.iter().enumerate() {
                let location = base + ri;
                let mixed = fmix(row.hk);
                let tag = ((mixed >> 32) as u32 & 0xff) << LOCATION_BITS;
                let entry = tag | (location as u32 + 1);
                let mut i = mixed as usize & mask;
                loop {
                    let previous = slots[i];
                    if previous == 0 {
                        slots[i] = entry;
                        break;
                    }
                    if previous & !LOCATION_MASK == tag {
                        let old = ((previous & LOCATION_MASK) - 1) as usize;
                        let winner = &parts[old >> shift][old & (stride - 1)];
                        if row.hk == winner.hk && same_id(row.eid, winner.eid) {
                            let loser = if row.rank < winner.rank {
                                slots[i] = entry;
                                old
                            } else {
                                location
                            };
                            losers[loser >> 6] |= 1 << (loser & 63);
                            duplicates += 1;
                            break;
                        }
                    }
                    i = (i + 1) & mask;
                }
            }
        }

        for (pi, part) in parts.iter().enumerate() {
            let base = pi * stride;
            for (ri, row) in part.iter().enumerate() {
                let location = base + ri;
                if losers[location >> 6] & (1 << (location & 63)) == 0 {
                    emit(row);
                }
            }
        }
        Some(count - duplicates)
    })
}
