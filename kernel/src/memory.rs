//! Kernel memory: the map firmware handed over, and the allocator that will spend it.
//!
//! Deliberately free of firmware types - `bootloader_api` stays in the adapter
//! (`framebuffer`) and in `main` - so this module compiles and is tested on the host,
//! where there is no firmware to ask.
//!
//! The map is data from firmware, not a fact we chose. The framebuffer is the sharp
//! edge of that: it is ordinary RAM that firmware happens to be displaying from, so
//! its pages may be listed as usable. Handing those to an allocator paints over the
//! screen at a moment nobody can predict. So the audit names the framebuffer's
//! physical span, and `pick_heap_region` refuses anything that touches it.
//!
//! What is left of the chosen region is split once more: the last whole page becomes a guard,
//! which M3 leaves unmapped so that a write past the end of the heap faults instead of landing
//! in whatever follows.

/// Bytes in a page and in a frame: x86_64's smallest mapping granule, and the unit the
/// frame allocator and every page table below work in.
pub const PAGE_BYTES: u64 = 4096;

/// A half-open physical range: `start` is inside it, `end` is not.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Span {
    pub start: u64,
    pub end: u64,
}

impl Span {
    pub const fn new(start: u64, end: u64) -> Self {
        Self { start, end }
    }

    /// Bytes in the range, saturating at zero for an inverted range.
    pub const fn len(&self) -> u64 {
        self.end.saturating_sub(self.start)
    }

    pub const fn is_empty(&self) -> bool {
        self.end <= self.start
    }

    /// Whether the two ranges share at least one byte. An empty range on either side
    /// shares nothing, which is what lets a zero-length framebuffer be treated as
    /// "nothing to protect" rather than as the whole of memory.
    pub const fn overlaps(&self, other: &Self) -> bool {
        if self.is_empty() || other.is_empty() {
            return false;
        }
        self.start < other.end && other.start < self.end
    }
}

/// One map entry, as this module sees it: a range plus the one bit that decides
/// whether it may be written.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Region {
    pub start: u64,
    pub end: u64,
    pub usable: bool,
}

impl Region {
    pub const fn span(&self) -> Span {
        Span::new(self.start, self.end)
    }
}

/// The largest usable region that does not overlap `reserved`.
///
/// `reserved` carries memory that is live despite being usable in the map - the
/// framebuffer, on a machine where firmware places it in ordinary RAM. When the
/// caller cannot derive that span it passes `None`, and this returns the largest
/// region it can see: the caller is responsible for saying so, because using a heap
/// that overlaps a live buffer is silent corruption rather than a failed boot.
pub fn pick_heap_region(
    regions: impl Iterator<Item = Region>,
    reserved: Option<Span>,
) -> Option<Span> {
    let mut best: Option<Span> = None;
    for region in regions {
        if !region.usable {
            continue;
        }
        let span = region.span();
        if span.is_empty() {
            continue;
        }
        if reserved.is_some_and(|reserved| span.overlaps(&reserved)) {
            continue;
        }
        if best.is_none_or(|best| span.len() > best.len()) {
            best = Some(span);
        }
    }
    best
}

/// Splits a heap region into the part a heap may use and the one page at its end to leave
/// unmapped.
///
/// The guard is the last page *fully inside* the region, so writing one byte past the end of
/// the heap lands in it: M3's fault test writes exactly there and expects a `#PF`. Rounding
/// inwards matters for a region whose firmware-reported end is not page-aligned - a guard
/// there would take a page that belongs to whatever follows.
///
/// A region with less than two whole pages has none to spare, so it comes back whole and
/// unguarded: a heap too small for one object is worse than an unguarded one, and the caller
/// reports which it got.
pub fn split_guard_page(span: Span) -> (Span, Option<Span>) {
    if span.len() < 2 * PAGE_BYTES {
        return (span, None);
    }
    let aligned_end = span.end & !(PAGE_BYTES - 1);
    let guard_start = aligned_end - PAGE_BYTES;
    // One whole page has to be left for the heap itself.
    if guard_start < span.start + PAGE_BYTES {
        return (span, None);
    }
    (
        Span::new(span.start, guard_start),
        Some(Span::new(guard_start, aligned_end)),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const USABLE: Region = Region {
        start: 0,
        end: 0,
        usable: true,
    };

    fn usable(start: u64, end: u64) -> Region {
        Region {
            start,
            end,
            usable: true,
        }
    }

    fn reserved_region(start: u64, end: u64) -> Region {
        Region {
            start,
            end,
            usable: false,
        }
    }

    #[test]
    fn a_span_reports_its_length_or_zero_when_inverted() {
        assert_eq!(Span::new(0x1000, 0x2000).len(), 0x1000);
        assert_eq!(Span::new(0x2000, 0x1000).len(), 0);
        assert!(Span::new(0x1000, 0x1000).is_empty());
    }

    #[test]
    fn adjacent_spans_do_not_overlap() {
        // `end` is exclusive, so a region ending where the next begins shares nothing.
        assert!(!Span::new(0x0, 0x1000).overlaps(&Span::new(0x1000, 0x2000)));
        assert!(!Span::new(0x1000, 0x2000).overlaps(&Span::new(0x0, 0x1000)));
    }

    #[test]
    fn overlapping_spans_are_detected_from_either_side() {
        let outer = Span::new(0x0, 0x4000);
        let inner = Span::new(0x1000, 0x2000);
        assert!(outer.overlaps(&inner));
        assert!(inner.overlaps(&outer));
        // Touching by one byte still counts.
        assert!(Span::new(0x0, 0x1001).overlaps(&Span::new(0x1000, 0x2000)));
    }

    #[test]
    fn an_empty_span_never_overlaps_anything() {
        let empty = Span::new(0x1000, 0x1000);
        assert!(!empty.overlaps(&Span::new(0x0, 0x8000)));
        assert!(!Span::new(0x0, 0x8000).overlaps(&empty));
    }

    #[test]
    fn picks_the_largest_usable_region() {
        let regions = [
            usable(0x10_0000, 0x20_0000),   // 1 MiB
            usable(0x100_0000, 0x180_0000), // 8 MiB
            usable(0x200_0000, 0x240_0000), // 4 MiB
        ];
        let picked = pick_heap_region(regions.into_iter(), None);
        assert_eq!(picked, Some(Span::new(0x100_0000, 0x180_0000)));
    }

    #[test]
    fn ignores_regions_the_kernel_may_not_write() {
        // A big region that is not usable must lose to a small usable one.
        let regions = [
            reserved_region(0x0, 0x4000_0000),
            usable(0x10_0000, 0x11_0000),
        ];
        let picked = pick_heap_region(regions.into_iter(), None);
        assert_eq!(picked, Some(Span::new(0x10_0000, 0x11_0000)));
    }

    #[test]
    fn never_picks_a_region_that_touches_the_framebuffer() {
        // The framebuffer sits inside the largest region, which is exactly what
        // firmware tends to report: the heap has to skip it, not share it.
        let regions = [
            usable(0x0, 0x1_0000_0000),
            usable(0x2_0000_0000, 0x2_1000_0000),
        ];
        // The physical span of a 1280x800 buffer firmware placed at 0x8000_0000:
        // 1280 * 800 * 4 = 4096000 bytes, which ends well inside the large region.
        let framebuffer = Span::new(0x8000_0000, 0x803e_8000);
        let picked = pick_heap_region(regions.into_iter(), Some(framebuffer));
        assert_eq!(picked, Some(Span::new(0x2_0000_0000, 0x2_1000_0000)));

        // Without the reservation the same map picks the region the screen lives in,
        // which is why the caller must not pass `None` without saying so.
        let unprotected = pick_heap_region(regions.into_iter(), None);
        assert_eq!(unprotected, Some(Span::new(0x0, 0x1_0000_0000)));
    }

    #[test]
    fn a_region_reserved_down_to_its_last_byte_is_rejected() {
        let regions = [usable(0x0, 0x1000)];
        let reserved = Span::new(0xfff, 0x2000);
        assert_eq!(pick_heap_region(regions.into_iter(), Some(reserved)), None);
    }

    #[test]
    fn returns_none_when_there_is_nothing_to_use() {
        let regions = [reserved_region(0x0, 0x4000_0000), usable(0x1000, 0x1000)];
        assert_eq!(pick_heap_region(regions.into_iter(), None), None);
        assert_eq!(pick_heap_region(core::iter::empty(), None), None);
    }

    #[test]
    fn an_empty_usable_region_is_not_a_heap() {
        let regions = [USABLE];
        // `USABLE` is written as an empty range, so it must be rejected, not picked.
        assert_eq!(pick_heap_region(regions.into_iter(), None), None);
    }

    #[test]
    fn the_guard_is_the_last_whole_page_of_the_region() {
        let (heap, guard) = split_guard_page(Span::new(0x10_0000, 0x14_0000));
        assert_eq!(heap, Span::new(0x10_0000, 0x13_f000));
        assert_eq!(guard, Some(Span::new(0x13_f000, 0x14_0000)));
        // The two halves still cover exactly the region, with nothing shared.
        assert_eq!(heap.len() + guard.unwrap().len(), 0x4_0000);
        assert!(!heap.overlaps(&guard.unwrap()));
    }

    #[test]
    fn an_unaligned_end_gives_up_the_tail_rather_than_its_neighbour() {
        // Firmware-reported ends are not always page-aligned. The guard has to be the last
        // page that ends at or before the region's end, never a page that reaches past it.
        let (heap, guard) = split_guard_page(Span::new(0x10_0000, 0x14_0abc));
        assert_eq!(guard, Some(Span::new(0x13_f000, 0x14_0000)));
        assert_eq!(heap.end, 0x13_f000);
        assert!(guard.unwrap().end <= 0x14_0abc);
    }

    #[test]
    fn an_unaligned_start_keeps_the_first_bytes_for_the_heap() {
        let (heap, guard) = split_guard_page(Span::new(0x10_0800, 0x14_0000));
        assert_eq!(
            heap.start, 0x10_0800,
            "the heap still starts where the region does"
        );
        assert_eq!(guard.map(|guard| guard.start), Some(0x13_f000));
    }

    #[test]
    fn a_region_without_a_spare_page_is_not_guarded() {
        // One page: no room for a guard. Two pages: one page of heap plus one guard is the
        // smallest split this allows, and a one-page region cannot reach it.
        let small = Span::new(0x10_0000, 0x10_1000);
        assert_eq!(split_guard_page(small), (small, None));

        let two = Span::new(0x10_0000, 0x10_2000);
        let (heap, guard) = split_guard_page(two);
        assert_eq!(heap, Span::new(0x10_0000, 0x10_1000));
        assert_eq!(guard, Some(Span::new(0x10_1000, 0x10_2000)));
    }

    #[test]
    fn an_empty_region_has_nothing_to_guard() {
        let empty = Span::new(0x10_0000, 0x10_0000);
        assert_eq!(split_guard_page(empty), (empty, None));
    }
}
