//! The kernel's allocator.
//!
//! ADR-021 chose a bump allocator over a dependency: an allocator bug is silent
//! memory corruption, and thirty lines can be read. `alloc` moves a cursor forward,
//! `dealloc` does nothing, and the counters say how much of the reserve was ever
//! touched - which is what makes a leak visible before anything can be reclaimed.
//!
//! The allocator never sees a physical address. The region arrives as a physical
//! span from `memory::pick_heap_region`, and the boot sequence converts it through
//! `BootInfo::physical_memory_offset` before calling [`BumpAllocator::init`].

use core::alloc::{GlobalAlloc, Layout};
use core::cell::UnsafeCell;
use core::ptr;

/// What the boot log reports, and what a later leak hunt will read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Stats {
    /// The first byte the allocator may hand out.
    pub base: usize,
    /// One past the last byte of the reserve. `base == end` means uninitialised.
    pub end: usize,
    /// `alloc` calls that consumed reserve bytes; zero-sized calls do not count.
    pub allocations: usize,
    /// Bytes handed out and never returned, because nothing is ever returned yet.
    pub bytes_in_use: usize,
    /// The largest `bytes_in_use` has ever been: the real peak demand.
    pub high_water: usize,
}

#[derive(Clone, Copy)]
struct State {
    cursor: usize,
    stats: Stats,
}

/// A bump allocator over one contiguous region.
pub struct BumpAllocator {
    // `GlobalAlloc` takes `&self`, so the cursor needs interior mutability for the
    // allocator to stay in a `static`.
    state: UnsafeCell<State>,
}

// SAFETY: the kernel is single-threaded and no interrupt handler allocates, so this
// cell is never entered twice at once. From M2 on the second half of that is a rule with a
// reason: exception and timer handlers may not allocate or print, because either would enter
// this cell (or the console) while the interrupted code still holds it (ADR-028). The
// argument breaks the moment a handler does, or a second thread exists.
unsafe impl Sync for BumpAllocator {}

impl BumpAllocator {
    /// A heap holding nothing, ready for [`BumpAllocator::init`].
    pub const fn new() -> Self {
        Self {
            state: UnsafeCell::new(State {
                cursor: 0,
                stats: Stats {
                    base: 0,
                    end: 0,
                    allocations: 0,
                    bytes_in_use: 0,
                    high_water: 0,
                },
            }),
        }
    }

    /// Points the allocator at `start..end`.
    ///
    /// Returns `false` for an empty range, and for a second call: a heap that
    /// silently moved after the first allocation would leave live objects pointing
    /// into memory the allocator believes is free.
    ///
    /// # Safety
    ///
    /// `start..end` must be mapped, writable, and used by nothing else. The
    /// allocator cannot check that, and getting it wrong is silent corruption rather
    /// than a failed boot (ADR-021).
    pub unsafe fn init(&self, start: usize, end: usize) -> bool {
        if start >= end {
            return false;
        }
        // SAFETY: `self` is a valid reference, and the reference formed here does
        // not outlive this function, so the cell is never aliased.
        let state = unsafe { &mut *self.state.get() };
        if state.stats.end != 0 {
            return false;
        }
        state.cursor = start;
        state.stats.base = start;
        state.stats.end = end;
        true
    }

    /// A snapshot of the counters, valid before `init` as well as after it.
    pub fn stats(&self) -> Stats {
        // SAFETY: as in `init` - a single read of a cell nothing else is touching,
        // and `State` is `Copy`, so no reference to the cell escapes.
        unsafe { *self.state.get() }.stats
    }
}

impl Default for BumpAllocator {
    fn default() -> Self {
        Self::new()
    }
}

/// Rounds `value` up to the next multiple of `align`, which must be a power of two.
/// `None` on overflow, so a length read from somewhere untrusted cannot wrap the
/// cursor backwards.
fn align_up(value: usize, align: usize) -> Option<usize> {
    let mask = align.checked_sub(1)?;
    Some(value.checked_add(mask)? & !mask)
}

// SAFETY: `alloc` returns either null or a pointer into the range the caller declared
// mapped, writable and otherwise unused, aligned as asked and never handed out twice;
// `dealloc` touches nothing at all. That is the whole of `GlobalAlloc`'s contract
// (ADR-021).
unsafe impl GlobalAlloc for BumpAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if layout.size() == 0 {
            // Nothing comes out of the reserve; an aligned dangling pointer is a
            // valid empty allocation, and this keeps `size == 0` from consuming a
            // byte it would never be able to give back.
            return layout.align() as *mut u8;
        }
        // SAFETY: as in `init`. Nothing else runs concurrently, per the `Sync` impl.
        let state = unsafe { &mut *self.state.get() };
        let Some(cursor) = align_up(state.cursor, layout.align()) else {
            return ptr::null_mut();
        };
        let Some(next) = cursor.checked_add(layout.size()) else {
            return ptr::null_mut();
        };
        if next > state.stats.end {
            // Out of reserve. Null is the only honest answer until there is a way to
            // ask for more virtual memory.
            return ptr::null_mut();
        }
        state.cursor = next;
        state.stats.allocations += 1;
        state.stats.bytes_in_use += layout.size();
        state.stats.high_water = state.stats.high_water.max(state.stats.bytes_in_use);
        cursor as *mut u8
    }

    unsafe fn dealloc(&self, _ptr: *mut u8, _layout: Layout) {
        // A bump allocator cannot reclaim one allocation without reclaiming every
        // allocation made after it. ADR-021: revisit at the first `dealloc` that
        // matters, not in anticipation.
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Points `heap` at a stack buffer owned by the calling test, so the suite stays
    /// parallel-safe without a shared `static mut`.
    fn point_at(heap: &BumpAllocator, store: &mut [u8]) -> bool {
        let start = store.as_mut_ptr() as usize;
        // SAFETY: `store` outlives every allocation made from `heap` in the calling
        // test, and the test never touches it through a reference afterwards.
        unsafe { heap.init(start, start + store.len()) }
    }

    #[test]
    fn an_uninitialised_heap_hands_out_nothing() {
        let heap = BumpAllocator::new();
        let layout = Layout::from_size_align(8, 8).expect("valid layout");
        // SAFETY: a heap that was never pointed at anything has no range to write
        // to, so this call must return null without touching memory at all.
        assert!(unsafe { heap.alloc(layout) }.is_null());
        assert_eq!(heap.stats().end, 0);
        assert_eq!(heap.stats().allocations, 0);
    }

    #[test]
    fn a_zero_sized_allocation_does_not_consume_the_reserve() {
        let mut store = [0u8; 16];
        let heap = BumpAllocator::new();
        assert!(point_at(&heap, &mut store));
        let layout = Layout::from_size_align(0, 4).expect("valid layout");
        // SAFETY: initialised over `store`, which is still alive.
        let pointer = unsafe { heap.alloc(layout) };
        assert_eq!(pointer as usize % 4, 0);
        assert_eq!(heap.stats().allocations, 0);
        assert_eq!(heap.stats().bytes_in_use, 0);
    }

    #[test]
    fn hands_out_aligned_pointers() {
        let mut store = [0u8; 256];
        let heap = BumpAllocator::new();
        assert!(point_at(&heap, &mut store));
        for align in [1, 2, 4, 8, 16, 32, 64] {
            let layout = Layout::from_size_align(3, align).expect("valid layout");
            // SAFETY: initialised over `store`, which is still alive, and 256 bytes is
            // enough for seven three-byte allocations however they are padded.
            let pointer = unsafe { heap.alloc(layout) };
            assert!(!pointer.is_null(), "alignment {align}");
            assert_eq!(pointer as usize % align, 0, "alignment {align}");
        }
    }

    #[test]
    fn refuses_once_the_reserve_is_exhausted() {
        let mut store = [0u8; 64];
        let heap = BumpAllocator::new();
        assert!(point_at(&heap, &mut store));
        let layout = Layout::from_size_align(40, 8).expect("valid layout");
        // SAFETY: initialised over `store`, which is still alive.
        assert!(!unsafe { heap.alloc(layout) }.is_null());
        // 40 is already 8-aligned, so a second one would end at 80, past 64.
        // SAFETY: as above; the allocator must refuse rather than run past `end`.
        assert!(unsafe { heap.alloc(layout) }.is_null());
        assert_eq!(heap.stats().allocations, 1);
    }

    #[test]
    fn no_allocation_leaves_the_reserve() {
        let mut store = [0u8; 100];
        let heap = BumpAllocator::new();
        assert!(point_at(&heap, &mut store));
        let start = store.as_ptr() as usize;
        let end = start + store.len();
        let layout = Layout::from_size_align(7, 1).expect("valid layout");
        let mut handed_out = 0;
        loop {
            // SAFETY: initialised over `store`, which is still alive; the loop stops
            // as soon as the allocator reports it is full.
            let pointer = unsafe { heap.alloc(layout) } as usize;
            if pointer == 0 {
                break;
            }
            assert!(
                pointer >= start && pointer + layout.size() <= end,
                "{pointer:#x} is outside {start:#x}..{end:#x}"
            );
            handed_out += 1;
            assert!(
                handed_out <= store.len(),
                "the allocator never reported being full"
            );
        }
        assert_eq!(handed_out, store.len() / layout.size());
    }

    #[test]
    fn counters_track_use_and_the_high_water_mark() {
        let mut store = [0u8; 128];
        let heap = BumpAllocator::new();
        assert!(point_at(&heap, &mut store));
        let small = Layout::from_size_align(10, 1).expect("valid layout");
        let large = Layout::from_size_align(30, 1).expect("valid layout");
        // SAFETY: initialised over `store`, which is still alive.
        let first = unsafe { heap.alloc(small) };
        // SAFETY: as above.
        let second = unsafe { heap.alloc(large) };
        assert!(!first.is_null() && !second.is_null());
        assert_eq!(heap.stats().allocations, 2);
        assert_eq!(heap.stats().bytes_in_use, 40);
        assert_eq!(heap.stats().high_water, 40);
        // SAFETY: `first` came from this allocator with `small`, the allocator ignores
        // the call, and nothing reads `first` afterwards.
        unsafe { heap.dealloc(first, small) };
        assert_eq!(heap.stats().bytes_in_use, 40, "bumping reclaims nothing");
        assert_eq!(heap.stats().high_water, 40);
    }

    #[test]
    fn initialising_twice_is_refused() {
        let mut store = [0u8; 16];
        let heap = BumpAllocator::new();
        assert!(point_at(&heap, &mut store));
        assert!(
            !point_at(&heap, &mut store),
            "a heap must not move after it is in use"
        );
        assert_eq!(heap.stats().base, store.as_ptr() as usize);
    }

    #[test]
    fn an_empty_range_is_not_a_heap() {
        let heap = BumpAllocator::new();
        // SAFETY: an empty range means the allocator hands out nothing, so no address
        // it could return is ever dereferenced.
        assert!(!unsafe { heap.init(0x1000, 0x1000) });
        assert_eq!(heap.stats().end, 0);
    }
}
