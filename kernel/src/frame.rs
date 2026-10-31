//! The frame allocator: one bit per 4 KiB frame, set when the frame is in use.
//!
//! ADR-029 picks a bitmap over a free list for M3 because the question this has to answer
//! most often is "is this frame free?", and that is one bit read rather than a walk down a
//! list. The whole structure is a single allocation whose size follows from the memory map,
//! so there is nothing to grow later either.
//!
//! The bitmap covers the physical address space, holes included, and starts with every frame
//! marked used. A frame becomes allocatable only when the boot sequence marks its span free,
//! which means a range it forgets is wasted rather than handed to a second owner. Getting that
//! the wrong way round is silent corruption of whatever the first owner was doing.
//!
//! Everything here is plain array arithmetic, so the host tests exercise it directly; the
//! `static` below is the kernel's single instance.

use crate::memory::{Span, PAGE_BYTES};
use crate::sync::InterruptMutex;

/// Frames one bitmap word covers.
const FRAMES_PER_WORD: usize = u64::BITS as usize;

/// What the boot log reports about the allocator.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Stats {
    /// Frames the bitmap describes: the physical address space, holes included.
    pub frames: usize,
    /// Frames still free.
    pub free: usize,
    /// Bytes the bitmap itself takes.
    pub bitmap_bytes: usize,
}

/// A bitmap over physical frames: bit set means the frame is in use.
pub struct Bitmap<'a> {
    words: &'a mut [u64],
    frames: usize,
}

impl<'a> Bitmap<'a> {
    /// A view of `words` that carries whatever marks they already hold.
    ///
    /// Covers `frames` frames, or as many as the words can hold, whichever is fewer: a count
    /// larger than the storage would index past the end of the bitmap.
    pub fn view(words: &'a mut [u64], frames: usize) -> Self {
        Self {
            frames: frames.min(words.len() * FRAMES_PER_WORD),
            words,
        }
    }

    /// A view with every frame marked used.
    ///
    /// Pessimistic on purpose: the boot sequence hands ranges over one at a time, and a frame
    /// nobody handed over must not be allocatable. See the module docs.
    pub fn all_used(words: &'a mut [u64], frames: usize) -> Self {
        let bitmap = Self::view(words, frames);
        bitmap.words.fill(u64::MAX);
        bitmap
    }

    /// Frames this bitmap describes.
    pub const fn frames(&self) -> usize {
        self.frames
    }

    /// Frames still free.
    pub fn free_frames(&self) -> usize {
        // Counted a word at a time: this runs at boot on a bitmap that spans gigabytes of
        // address space, and a bit-at-a-time loop over a million frames is measurable under
        // emulation. Bits past the last frame are masked out rather than trusted.
        let mut free = 0;
        for (word, bits) in self.words.iter().enumerate() {
            let start = word * FRAMES_PER_WORD;
            if start >= self.frames {
                break;
            }
            let valid = (self.frames - start).min(FRAMES_PER_WORD);
            let mask = if valid == FRAMES_PER_WORD {
                u64::MAX
            } else {
                (1u64 << valid) - 1
            };
            free += (!*bits & mask).count_ones() as usize;
        }
        free
    }

    fn is_used(&self, frame: usize) -> bool {
        self.words[frame / FRAMES_PER_WORD] & (1 << (frame % FRAMES_PER_WORD)) != 0
    }

    fn set(&mut self, frame: usize, used: bool) {
        let word = frame / FRAMES_PER_WORD;
        let bit = 1 << (frame % FRAMES_PER_WORD);
        if used {
            self.words[word] |= bit;
        } else {
            self.words[word] &= !bit;
        }
    }
}

impl Bitmap<'_> {
    /// Marks every frame that lies entirely inside `span` free, and returns how many.
    ///
    /// Rounding inwards: a frame the span only touches keeps its mark, because handing out a
    /// frame that shares bytes with something reserved is exactly the corruption this
    /// allocator exists to prevent. A span past the end of the bitmap is clamped, not trusted.
    pub fn mark_free(&mut self, span: Span) -> usize {
        let first = span.start.div_ceil(PAGE_BYTES);
        let end = (span.end / PAGE_BYTES).min(self.frames as u64);
        let mut freed = 0;
        for frame in first..end {
            let frame = frame as usize;
            if self.is_used(frame) {
                self.set(frame, false);
                freed += 1;
            }
        }
        freed
    }

    /// Marks every frame `span` touches used, and returns how many were free before.
    ///
    /// Rounding outwards, the mirror of [`Bitmap::mark_free`]: a reservation takes every frame
    /// it shares a byte with.
    pub fn mark_used(&mut self, span: Span) -> usize {
        let first = span.start / PAGE_BYTES;
        let end = span.end.div_ceil(PAGE_BYTES).min(self.frames as u64);
        let mut taken = 0;
        for frame in first..end {
            let frame = frame as usize;
            if !self.is_used(frame) {
                self.set(frame, true);
                taken += 1;
            }
        }
        taken
    }

    /// The lowest free frame, marked used. `None` when every frame is spoken for.
    ///
    /// The scan restarts at frame zero every call. There is no cursor to advance because the
    /// kernel allocates frames in bursts at boot: when that stops being true, this is the
    /// first thing to change.
    pub fn alloc(&mut self) -> Option<u64> {
        for (word, bits) in self.words.iter_mut().enumerate() {
            if *bits == u64::MAX {
                continue;
            }
            let bit = (!*bits).trailing_zeros() as usize;
            let frame = word * FRAMES_PER_WORD + bit;
            if frame >= self.frames {
                return None;
            }
            *bits |= 1 << bit;
            return Some(frame as u64 * PAGE_BYTES);
        }
        None
    }

    /// Marks the frame at `address` free, and returns whether it was in use.
    ///
    /// An address that is not a frame start frees nothing: a caller passing the middle of a
    /// frame has a bug, and clearing the bit would hand the whole frame out again while
    /// something is still using the other half. A frame that was never allocated looks
    /// exactly like one that is still in use, so "only free what you were given" is a rule
    /// this type cannot enforce.
    pub fn free(&mut self, address: u64) -> bool {
        if !address.is_multiple_of(PAGE_BYTES) {
            return false;
        }
        let frame = (address / PAGE_BYTES) as usize;
        if frame >= self.frames || !self.is_used(frame) {
            return false;
        }
        self.set(frame, false);
        true
    }
}

#[derive(Clone, Copy)]
struct State {
    words: *mut u64,
    len: usize,
    frames: usize,
}

/// The kernel's frame allocator, initialised once during boot.
pub struct Allocator {
    // `alloc` takes `&self` so the allocator can live in a `static`: the same bargain as
    // `heap::BumpAllocator` (ADR-021). The cell is the kernel's one shared-mutable type, whose
    // safety argument lives with it in `sync` (ADR-038).
    state: InterruptMutex<State>,
}

impl Allocator {
    /// An allocator that owns nothing, until [`Allocator::init`].
    pub const fn new() -> Self {
        Self {
            state: InterruptMutex::new(State {
                words: core::ptr::null_mut(),
                len: 0,
                frames: 0,
            }),
        }
    }

    /// Points the allocator at `len` bitmap words covering `frames` frames.
    ///
    /// Every frame starts marked used, so the caller has to hand over the spans firmware calls
    /// usable before anything can be allocated. Returns `false` for a second call, an empty
    /// bitmap or no frames at all: an allocator that quietly moved would hand out frames that
    /// are still in use.
    ///
    /// # Safety
    ///
    /// `words` must point at `len` writable words that outlive the kernel and that nothing
    /// else writes, and `frames` must not exceed `len * 64`.
    pub unsafe fn init(&self, words: *mut u64, len: usize, frames: usize) -> bool {
        if words.is_null() || len == 0 || frames == 0 {
            return false;
        }
        // SAFETY: `self` is the only reference to the cell, and it does not escape this call.
        let state = unsafe { &mut *self.state.as_ptr() };
        if !state.words.is_null() {
            return false;
        }
        *state = State { words, len, frames };
        // SAFETY: the caller promises `words` is `len` writable words that outlive the kernel.
        let bitmap = unsafe { core::slice::from_raw_parts_mut(words, len) };
        Bitmap::all_used(bitmap, frames);
        true
    }

    /// Runs `f` with the bitmap, or returns `None` before [`Allocator::init`].
    fn with<R>(&self, f: impl FnOnce(&mut Bitmap<'_>) -> R) -> Option<R> {
        // SAFETY: `words` and `len` are written once by `init` and never changed afterwards,
        // and the slice formed here does not outlive the call.
        let state = unsafe { &*self.state.as_ptr() };
        if state.words.is_null() {
            return None;
        }
        // SAFETY: as above; `init` checked that `frames` fits in `len` words.
        let words = unsafe { core::slice::from_raw_parts_mut(state.words, state.len) };
        Some(f(&mut Bitmap::view(words, state.frames)))
    }

    /// Marks every frame inside `span` free.
    pub fn mark_free(&self, span: Span) -> usize {
        self.with(|bitmap| bitmap.mark_free(span)).unwrap_or(0)
    }

    /// Marks every frame `span` touches used.
    pub fn mark_used(&self, span: Span) -> usize {
        self.with(|bitmap| bitmap.mark_used(span)).unwrap_or(0)
    }

    /// A free frame, marked used. `None` when [`Allocator::init`] has not run, or when every
    /// frame is spoken for.
    pub fn alloc(&self) -> Option<u64> {
        self.with(|bitmap| bitmap.alloc()).flatten()
    }

    /// Gives the frame at `address` back, and returns whether it was in use.
    pub fn free(&self, address: u64) -> bool {
        self.with(|bitmap| bitmap.free(address)).unwrap_or(false)
    }

    /// What the boot log reports, or `None` before [`Allocator::init`].
    pub fn stats(&self) -> Option<Stats> {
        // SAFETY: as `with`; only `init` writes these fields, and it has already run when the
        // pointer is non-null.
        let state = unsafe { &*self.state.as_ptr() };
        if state.words.is_null() {
            return None;
        }
        let len = state.len;
        self.with(|bitmap| Stats {
            frames: bitmap.frames(),
            free: bitmap.free_frames(),
            bitmap_bytes: len * core::mem::size_of::<u64>(),
        })
    }
}

/// The kernel's frames. Named `FRAMES` rather than `ALLOCATOR` because `frame::FRAMES` reads
/// as what it holds.
pub static FRAMES: Allocator = Allocator::new();

impl Default for Allocator {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Frames in the two-word bitmaps below. Two words, so the second-word path is walked.
    const TOTAL: usize = 128;

    fn bitmap(words: &mut [u64; 2]) -> Bitmap<'_> {
        Bitmap::all_used(words, TOTAL)
    }

    #[test]
    fn a_fresh_bitmap_hands_out_nothing() {
        let mut words = [0u64; 2];
        let mut bitmap = bitmap(&mut words);
        assert_eq!(bitmap.frames(), TOTAL);
        assert_eq!(bitmap.free_frames(), 0);
        assert_eq!(bitmap.alloc(), None);
    }

    #[test]
    fn marking_a_span_free_hands_out_its_frames_in_order() {
        let mut words = [0u64; 2];
        let mut bitmap = bitmap(&mut words);
        let freed = bitmap.mark_free(Span::new(0x4000, 0x8000));
        assert_eq!(freed, 4);
        assert_eq!(bitmap.free_frames(), 4);
        let handed_out: Vec<u64> = (0..4).filter_map(|_| bitmap.alloc()).collect();
        assert_eq!(handed_out, vec![0x4000, 0x5000, 0x6000, 0x7000]);
        assert_eq!(bitmap.alloc(), None);
    }

    #[test]
    fn a_span_that_does_not_fill_a_frame_frees_nothing() {
        let mut words = [0u64; 2];
        let mut bitmap = bitmap(&mut words);
        // Starts and ends inside frames 3 and 4, so neither is wholly described.
        assert_eq!(bitmap.mark_free(Span::new(0x3010, 0x4ff0)), 0);
        // Covering both frames exactly does free both, and nothing else.
        assert_eq!(bitmap.mark_free(Span::new(0x3000, 0x5000)), 2);
        assert_eq!(bitmap.free_frames(), 2);
    }

    #[test]
    fn a_reservation_takes_every_frame_it_touches() {
        let mut words = [0u64; 2];
        let mut bitmap = bitmap(&mut words);
        bitmap.mark_free(Span::new(0x3000, 0x6000));
        assert_eq!(bitmap.free_frames(), 3);
        // One byte into frame 4, ending one byte into frame 5: both frames go, because a
        // reservation rounds outwards. Frame 3 is untouched.
        assert_eq!(bitmap.mark_used(Span::new(0x4001, 0x5001)), 2);
        assert_eq!(bitmap.free_frames(), 1);
        assert_eq!(bitmap.alloc(), Some(0x3000));
    }

    #[test]
    fn a_frame_comes_back_exactly_once() {
        let mut words = [0u64; 2];
        let mut bitmap = bitmap(&mut words);
        bitmap.mark_free(Span::new(0x9000, 0xa000));
        assert_eq!(bitmap.alloc(), Some(0x9000));
        assert!(bitmap.free(0x9000));
        assert!(!bitmap.free(0x9000), "a frame cannot be freed twice");
        assert_eq!(bitmap.free_frames(), 1);
        // The middle of a frame is not a frame, so it frees nothing.
        assert!(!bitmap.free(0x9080));
        // A frame still marked used comes free whether or not `alloc` handed it out: the
        // bitmap cannot tell those apart, so keeping that rule is the caller's job.
        assert!(bitmap.free(0x1000));
        assert_eq!(bitmap.free_frames(), 2);
    }

    #[test]
    fn a_span_past_the_end_is_clamped_rather_than_trusted() {
        let mut words = [0u64; 2];
        let mut bitmap = bitmap(&mut words);
        // A firmware region that runs past the last frame the bitmap describes must not index
        // past the words it has.
        bitmap.mark_free(Span::new(0x0, 0x1000_0000));
        assert_eq!(bitmap.free_frames(), TOTAL);
        assert_eq!(bitmap.mark_used(Span::new(0x0, 0xffff_ffff)), TOTAL);
        assert_eq!(bitmap.free_frames(), 0);
    }

    #[test]
    fn a_bitmap_covers_only_what_its_words_hold() {
        let mut words = [0u64; 2];
        // Asking for more frames than the storage can describe is clamped, not trusted.
        assert_eq!(Bitmap::all_used(&mut words, 1000).frames(), TOTAL);
    }

    #[test]
    fn frames_past_the_last_one_are_never_handed_out_or_counted() {
        let mut words = [0u64; 2];
        // 100 frames described, 28 spare bits in the words. They are free bits, and they must
        // not count as free frames: no address built from them is one this bitmap describes.
        let mut bitmap = Bitmap::view(&mut words, 100);
        assert_eq!(bitmap.free_frames(), 100);
        let mut last = None;
        while let Some(frame) = bitmap.alloc() {
            last = Some(frame);
        }
        assert_eq!(last, Some(99 * PAGE_BYTES));
        assert_eq!(bitmap.free_frames(), 0);
    }

    #[test]
    fn the_allocator_refuses_to_run_before_it_is_pointed_at_memory() {
        let allocator = Allocator::new();
        assert_eq!(allocator.alloc(), None);
        assert_eq!(allocator.stats(), None);
        assert_eq!(allocator.mark_free(Span::new(0, 0x1000)), 0);
        assert!(!allocator.free(0));
    }

    #[test]
    fn the_allocator_takes_its_bitmap_once_and_then_works() {
        let mut words = [0u64; 2];
        let allocator = Allocator::new();
        // SAFETY: `words` is a live array of two words, and 128 frames fit in them.
        assert!(unsafe { allocator.init(words.as_mut_ptr(), words.len(), TOTAL) });
        // SAFETY: as above; the second call must be refused before it writes anything.
        assert!(!unsafe { allocator.init(words.as_mut_ptr(), words.len(), TOTAL) });
        assert_eq!(
            allocator.stats(),
            Some(Stats {
                frames: TOTAL,
                free: 0,
                bitmap_bytes: 16,
            })
        );
        // Nothing is allocatable until a span is handed over.
        assert_eq!(allocator.mark_free(Span::new(0x2000, 0x3000)), 1);
        assert_eq!(allocator.alloc(), Some(0x2000));
        assert_eq!(allocator.stats().map(|stats| stats.free), Some(0));
        assert!(allocator.free(0x2000));
        assert_eq!(allocator.stats().map(|stats| stats.free), Some(1));
    }

    #[test]
    fn an_empty_bitmap_is_not_an_allocator() {
        let allocator = Allocator::new();
        let mut words = [0u64; 1];
        // SAFETY: every argument is either null or a live array; each call is refused before
        // anything is read or written.
        unsafe {
            assert!(!allocator.init(core::ptr::null_mut(), 0, 0));
            assert!(!allocator.init(core::ptr::null_mut(), 1, 1));
            assert!(!allocator.init(words.as_mut_ptr(), 0, 0));
            assert!(!allocator.init(words.as_mut_ptr(), 1, 0));
        }
        assert_eq!(allocator.stats(), None);
    }
}
