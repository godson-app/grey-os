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
}
