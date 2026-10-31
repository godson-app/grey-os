//! The kernel's allocator: an arena of pages the frame allocator backs on demand.
//!
//! ADR-021's bump allocator is kept in [`crate::heap`] for exactly one job, the bootstrap: the
//! frame allocator's bitmap is an allocation, and the frame allocator is what this arena asks
//! for pages, so something has to hand out memory before a page can be mapped. Everything after
//! that bootstrap allocates from here (ADR-031).
//!
//! The arena is a fixed virtual span, [`Heap::init_arena`]'s `base..base+capacity`, and only the
//! pages it has grown into are mapped. That is what makes the guard page M3 armed by hand
//! (`unmap_4k` on the last page of a region) an accident of construction here: the first page
//! past [`Stats::end`] was never mapped, so a write to it faults with `cr2` at the heap's end and
//! nothing has to be taken away to make that true.
//!
//! Blocks carry a 16-byte header - size, the size of the block before it, a magic and free bit,
//! the size the caller asked for - and free blocks are threaded onto one list per size class.
//! Allocation splits the block it finds; freeing merges with both neighbours and, when the freed
//! block reaches the top of the arena, gives whole pages back to the frame allocator. A free that
//! does not match its block - a double free, a pointer that never came from here, a size the
//! block was not allocated with - is counted and ignored instead of followed, because following
//! it corrupts the free lists silently (ADR-030).
//!
//! The pages come from a [`Pages`] implementation: the kernel maps them with `arch::map_4k` and
//! the frame allocator, the host tests point them at an array. Everything between - splitting,
//! coalescing, class bookkeeping - is plain arithmetic over the arena, so the host tests exercise
//! the allocator the kernel runs.

use core::alloc::{GlobalAlloc, Layout};

use crate::sync::InterruptMutex;

/// Bytes in a page: the unit the arena grows in and the frame allocator hands out.
pub const PAGE_BYTES: usize = 4096;

/// Bytes of header in front of every block's payload.
///
/// Sixteen rather than eight so a payload starts at the arena's own alignment, which keeps a
/// sixteen-byte type - `u128`, and every `Layout` Rust derives for a struct containing one - from
/// needing a split just for its alignment.
const HEADER_BYTES: usize = 16;

/// The smallest block worth splitting off: a header and one free-list link's worth of payload.
const MIN_BLOCK: usize = 32;

/// Size classes: 64 bytes and below, then a class per power of two up to 4096, then "larger".
const CLASSES: usize = 8;

/// The class for blocks too large for a power of two of a page or less.
const LARGE: usize = CLASSES - 1;

/// Header words: total size, size of the block before it, magic and free bit, requested payload.
const WORD_SIZE: usize = 0;
const WORD_PREV: usize = 1;
const WORD_FLAGS: usize = 2;
const WORD_REQUESTED: usize = 3;

/// The high half of the flags word. A header without it is not one of ours, which is how a
/// pointer that never came from this allocator is refused rather than followed.
const MAGIC: u32 = 0xace1_0000;
const MAGIC_MASK: u32 = 0xffff_0000;

/// The low bit of the flags word: the block is on a free list.
const FREE: u32 = 1;

/// What the boot log reports, and what a leak hunt reads.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Stats {
    /// First byte of the arena's virtual span, mapped or not.
    pub base: usize,
    /// One past the last mapped byte: the guard page starts here.
    pub end: usize,
    /// Bytes the span reserves. `base + capacity` is one past its last byte.
    pub capacity: usize,
    /// Pages mapped now.
    pub pages: usize,
    /// Pages grown: every frame the arena has ever taken from the page source.
    pub grown: usize,
    /// Pages handed back by a free at the top of the arena.
    pub returned: usize,
    /// Successful allocations. A zero-sized request counts as one and consumes nothing.
    pub allocations: usize,
    /// Successful frees.
    pub frees: usize,
    /// Frees refused because they did not match the block they named.
    pub bad_frees: usize,
    /// Bytes handed out and not yet returned.
    pub bytes_in_use: usize,
    /// The largest `bytes_in_use` has ever been.
    pub high_water: usize,
}

/// What the arena needs from whatever backs its pages.
///
/// One page per call rather than a range: the arena grows a page at a time, and a page the source
/// cannot map is the answer "out of memory" rather than a half-grown range to unwind.
pub trait Pages {
    /// Backs `address` with a fresh frame, returning that frame, or `None` if it could not.
    fn map(&mut self, address: usize) -> Option<u64>;

    /// Takes the mapping at `address` away, returning whether it did.
    fn unmap(&mut self, address: usize) -> bool;
}

/// A block, read out of its header.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Block {
    /// Address of the header.
    addr: usize,
    /// Header and payload together.
    size: usize,
    /// Size of the block before this one, zero for the first block in the arena.
    prev: usize,
    /// Whether the block is on a free list.
    free: bool,
    /// The payload size the caller asked for, zero for a free block.
    requested: usize,
}

#[derive(Clone, Copy)]
struct State {
    base: usize,
    capacity: usize,
    end: usize,
    /// One head per size class: the address of the first free block, zero for empty.
    heads: [usize; CLASSES],
    /// The block that ends at `end`, so growth can extend it instead of walking the arena.
    tail: usize,
    stats: Stats,
}

impl State {
    const fn empty() -> Self {
        Self {
            base: 0,
            capacity: 0,
            end: 0,
            heads: [0; CLASSES],
            tail: 0,
            stats: Stats {
                base: 0,
                end: 0,
                capacity: 0,
                pages: 0,
                grown: 0,
                returned: 0,
                allocations: 0,
                frees: 0,
                bad_frees: 0,
                bytes_in_use: 0,
                high_water: 0,
            },
        }
    }
}

/// The arena allocator. One instance lives in the binary as `GlobalAlloc`; the host tests build
/// one per test over an array.
pub struct Heap<A>
where
    A: Pages,
{
    /// The page source. `GlobalAlloc` takes `&self` and mapping mutates the source, so it needs
    /// the same interior mutability the state does.
    pages: InterruptMutex<A>,
    state: InterruptMutex<State>,
}

/// Reads word `word` of the header at `addr`.
///
/// # Safety
///
/// `addr` must be the start of a block header inside a live arena: sixteen writable bytes,
/// aligned to four.
unsafe fn header_word(addr: usize, word: usize) -> u32 {
    // SAFETY: the caller promises a live, four-aligned header at `addr`, and `word` is below
    // four for every call in this module.
    unsafe { (addr as *const u32).add(word).read() }
}

/// Writes word `word` of the header at `addr`.
///
/// # Safety
///
/// As [`header_word`]: `addr` must be the start of a live header.
unsafe fn set_header_word(addr: usize, word: usize, value: u32) {
    // SAFETY: as `header_word`.
    unsafe { (addr as *mut u32).add(word).write(value) }
}

/// Reads the header at `addr`. A header whose magic is gone is read as an empty block, which
/// every caller treats as "not a block of mine".
///
/// # Safety
///
/// `addr` must be the start of a live header inside this arena.
unsafe fn read_block(addr: usize) -> Block {
    // SAFETY: the caller promises a live header.
    let flags = unsafe { header_word(addr, WORD_FLAGS) };
    if flags & MAGIC_MASK != MAGIC {
        return Block {
            addr,
            size: 0,
            prev: 0,
            free: true,
            requested: 0,
        };
    }
    Block {
        addr,
        // SAFETY: as above.
        size: unsafe { header_word(addr, WORD_SIZE) } as usize,
        // SAFETY: as above.
        prev: unsafe { header_word(addr, WORD_PREV) } as usize,
        free: flags & FREE != 0,
        // SAFETY: as above.
        requested: unsafe { header_word(addr, WORD_REQUESTED) } as usize,
    }
}

/// Writes `block`'s four words at `block.addr`.
///
/// # Safety
///
/// `block.addr` must be the start of a live header inside this arena.
unsafe fn write_block(block: Block) {
    let flags = MAGIC | if block.free { FREE } else { 0 };
    // SAFETY: the caller promises a live header at `block.addr`; each call writes one of its
    // four words.
    unsafe {
        set_header_word(block.addr, WORD_SIZE, block.size as u32);
        set_header_word(block.addr, WORD_PREV, block.prev as u32);
        set_header_word(block.addr, WORD_FLAGS, flags);
        set_header_word(block.addr, WORD_REQUESTED, block.requested as u32);
    }
}

/// The free list of a block of `size` bytes.
const fn class_of(size: usize) -> usize {
    // 64 bytes and below share the first class; above that, one class per power of two. The
    // shift is the exponent of the next power of two, which is what makes a block's class a
    // lower bound on its size and never an upper one.
    let exponent = (size.next_power_of_two().trailing_zeros() as usize).saturating_sub(6);
    if exponent >= LARGE {
        LARGE
    } else {
        exponent
    }
}

/// `value` rounded up to a multiple of `align`, saturating instead of wrapping.
const fn round_up(value: usize, align: usize) -> usize {
    let mask = align - 1;
    match value.checked_add(mask) {
        Some(sum) => sum & !mask,
        None => usize::MAX & !mask,
    }
}

impl<A> Heap<A>
where
    A: Pages,
{
    /// An arena with `pages` as the thing that backs it, and no span yet.
    pub const fn new(pages: A) -> Self {
        Self {
            pages: InterruptMutex::new(pages),
            state: InterruptMutex::new(State::empty()),
        }
    }

    /// Whether [`Heap::init_arena`] has run.
    pub fn ready(&self) -> bool {
        // SAFETY: a single read of a cell nothing else touches; `State` is `Copy`, so no
        // reference escapes.
        unsafe { (*self.state.as_ptr()).base != 0 }
    }

    /// A snapshot of the counters, valid before `init_arena` as well as after it.
    pub fn stats(&self) -> Stats {
        // SAFETY: as `ready`.
        unsafe { (*self.state.as_ptr()).stats }
    }

    /// Points the arena at `base..base + capacity` without mapping anything.
    ///
    /// Returns `false` for a span that is unaligned, empty, larger than the address space left
    /// above `base`, or a second call: an arena that moved after the first allocation would
    /// leave live objects pointing into memory it believes is free.
    ///
    /// # Safety
    ///
    /// `base..base + capacity` must be virtual address space nothing else maps or will map, and
    /// the [`Pages`] implementation must map exactly the page it is given. Getting that wrong is
    /// silent corruption rather than a failed boot (ADR-031).
    pub unsafe fn init_arena(&self, base: usize, capacity: usize) -> bool {
        if base == 0
            || capacity == 0
            || !base.is_multiple_of(PAGE_BYTES)
            || !capacity.is_multiple_of(PAGE_BYTES)
        {
            return false;
        }
        if base.checked_add(capacity).is_none() {
            return false;
        }
        // SAFETY: `self` is a valid reference and the reference formed here does not outlive
        // this function, so the cell is never aliased.
        let state = unsafe { &mut *self.state.as_ptr() };
        if state.base != 0 {
            return false;
        }
        state.base = base;
        state.capacity = capacity;
        state.end = base;
        state.stats.base = base;
        state.stats.end = base;
        state.stats.capacity = capacity;
        true
    }

    /// The mutable state, for the length of one call. Nothing else runs at the same time, which
    /// is the argument [`InterruptMutex`] carries (ADR-038).
    // The reference handed back points into an `UnsafeCell`, which is what makes taking `&self`
    // sound: `mut_from_ref` cannot see through the cell, so it is told to stand down here.
    #[allow(clippy::mut_from_ref)]
    fn state(&self) -> &mut State {
        // SAFETY: `self` outlives the borrow, and the cell guarantees exclusive access by
        // masking interrupt handlers out for the length of it.
        unsafe { &mut *self.state.as_ptr() }
    }

    /// The page source, for the length of one call. This is how the kernel points it at its own
    /// tables once they exist: the arena cannot be given a span before something can map pages
    /// for it.
    ///
    /// # Safety
    ///
    /// Whatever the caller does with it must respect what [`Pages`] promises: each address handed
    /// to `map` lies inside the arena's span and is mapped exactly once.
    // As `state`: the reference points into an `UnsafeCell`.
    #[allow(clippy::mut_from_ref)]
    pub unsafe fn source(&self) -> &mut A {
        // SAFETY: as `state` - a single-threaded kernel, and the cell is the only way in.
        unsafe { &mut *self.pages.as_ptr() }
    }

    /// The page source, for the length of one call, on the same argument as [`Heap::state`].
    // As `state`: the reference points into an `UnsafeCell`.
    #[allow(clippy::mut_from_ref)]
    fn pages(&self) -> &mut A {
        // SAFETY: as `state`.
        unsafe { &mut *self.pages.as_ptr() }
    }
}

/// Reads the free-list link stored in a free block's payload.
///
/// # Safety
///
/// `addr` must be the header of a block inside this arena that is on a free list, so its payload
/// holds two `usize` links.
unsafe fn link(addr: usize, second: bool) -> usize {
    let offset = HEADER_BYTES
        + if second {
            core::mem::size_of::<usize>()
        } else {
            0
        };
    // SAFETY: the caller promises a free block, whose payload is at least two links wide
    // (`MIN_BLOCK` is a header plus that much), and the arena is usize-aligned.
    unsafe { ((addr + offset) as *const usize).read() }
}

/// Writes the free-list link stored in a free block's payload.
///
/// # Safety
///
/// As [`link`].
unsafe fn set_link(addr: usize, second: bool, value: usize) {
    let offset = HEADER_BYTES
        + if second {
            core::mem::size_of::<usize>()
        } else {
            0
        };
    // SAFETY: as `link`.
    unsafe { ((addr + offset) as *mut usize).write(value) }
}

impl State {
    /// Puts `addr` at the head of its class's list.
    ///
    /// # Safety
    ///
    /// `addr` must be the header of a free block inside this arena that is not already linked.
    unsafe fn insert_free(&mut self, addr: usize) {
        // SAFETY: the caller promises a free block, so this reads a live header.
        let class = class_of(unsafe { read_block(addr) }.size);
        let head = self.heads[class];
        // SAFETY: as above; a free block's payload holds the two links. The backward link is
        // cleared because this block becomes the head, and whatever its payload held when it was
        // allocated is still there.
        unsafe {
            set_link(addr, false, head);
            set_link(addr, true, 0);
            if head != 0 {
                set_link(head, true, addr);
            }
        }
        self.heads[class] = addr;
    }

    /// Takes `addr` off its class's list.
    ///
    /// # Safety
    ///
    /// `addr` must be the header of a free block inside this arena that is on a list.
    unsafe fn unlink_free(&mut self, addr: usize) {
        // SAFETY: the caller promises a linked free block.
        let block = unsafe { read_block(addr) };
        let class = class_of(block.size);
        // SAFETY: as above.
        let next = unsafe { link(addr, false) };
        // SAFETY: as above.
        let prev = unsafe { link(addr, true) };
        if prev == 0 {
            self.heads[class] = next;
        } else {
            // SAFETY: `prev` came out of this block's own backward link, so it is a free block
            // whose forward link points back here.
            unsafe { set_link(prev, false, next) };
        }
        if next != 0 {
            // SAFETY: `next` came out of this block's forward link, so it is free and its
            // backward link points back here.
            unsafe { set_link(next, true, prev) };
        }
        // SAFETY: as above; zeroing the links makes a stale second unlink visible as zeroes
        // rather than as whatever the list holds by then.
        unsafe {
            set_link(addr, false, 0);
            set_link(addr, true, 0);
        }
    }

    /// Marks the header at `addr` free, merges it with whichever neighbours are free, and puts the
    /// result on its class's list. Returns the merged block, which sits at a lower address than
    /// `addr` when the block before it was free.
    ///
    /// # Safety
    ///
    /// `addr` must be the header of a block inside this arena that is not on any list, and its
    /// header must already say free.
    unsafe fn merge(&mut self, addr: usize) -> Block {
        // SAFETY: the caller promises a live header, and its neighbours are either inside the
        // arena or past `end`.
        let block = unsafe { read_block(addr) };
        let mut merged = block;
        let previous = if block.prev != 0 {
            // SAFETY: a non-zero `prev` is the size of a block that ends at `addr`.
            let previous = unsafe { read_block(block.addr - block.prev) };
            previous.free.then_some(previous)
        } else {
            None
        };
        let following = if block.addr + block.size < self.end {
            // SAFETY: a block that starts inside the arena has a live header.
            let following = unsafe { read_block(block.addr + block.size) };
            following.free.then_some(following)
        } else {
            None
        };
        if let Some(previous) = previous {
            // SAFETY: both blocks are free and on a list, which is what merging requires.
            unsafe { self.unlink_free(previous.addr) };
            merged = Block {
                addr: previous.addr,
                size: previous.size + merged.size,
                prev: previous.prev,
                free: true,
                requested: 0,
            };
        }
        if let Some(following) = following {
            // SAFETY: as above.
            unsafe { self.unlink_free(following.addr) };
            merged.size += following.size;
        }
        // SAFETY: `merged.addr` is a live header inside the arena.
        unsafe { write_block(merged) };
        if merged.addr + merged.size < self.end {
            // SAFETY: the block after a merged one has a live header, and its `prev` is now the
            // merged size.
            let mut next = unsafe { read_block(merged.addr + merged.size) };
            next.prev = merged.size;
            // SAFETY: `merged.addr + merged.size` is that block's live header.
            unsafe { write_block(next) };
        }
        // SAFETY: `merged` is free, inside the arena, and on no list.
        unsafe { self.insert_free(merged.addr) };
        if merged.addr + merged.size == self.end {
            self.tail = merged.addr;
        }
        merged
    }
}

impl<A> Heap<A>
where
    A: Pages,
{
    /// Gives whole pages back to the page source when the block at the top of the arena is the
    /// one being freed.
    ///
    /// Only a block that starts on a page boundary is trimmed: below that, part of a page is in
    /// use by the block before it and a page cannot be half unmapped. The pages stay mapped until
    /// that block is freed too, which is waste bounded by one page, not a leak that grows.
    ///
    /// # Safety
    ///
    /// `block` must be free, on a list, and inside this arena.
    unsafe fn trim(&self, block: Block) {
        if block.addr + block.size != self.state().end
            || !block.addr.is_multiple_of(PAGE_BYTES)
            || !block.size.is_multiple_of(PAGE_BYTES)
        {
            return;
        }
        let pages = self.pages();
        let state = self.state();
        // SAFETY: `block` is free and on a list.
        unsafe { state.unlink_free(block.addr) };
        let mut end = state.end;
        while end > block.addr {
            end -= PAGE_BYTES;
            // A refused unmap stops the trim rather than leaving `end` claiming a page that is
            // still mapped, which would hand the same memory out twice.
            if !pages.unmap(end) {
                end += PAGE_BYTES;
                break;
            }
            state.stats.pages -= 1;
            state.stats.returned += 1;
        }
        state.end = end;
        state.stats.end = end;
        state.tail = if block.prev != 0 {
            block.addr - block.prev
        } else {
            0
        };
    }

    /// Grows the arena by enough pages to hold `bytes`, mapping each one, and returns whether it
    /// could. A page the source refuses unwinds the pages this call mapped, so a failed growth
    /// leaves the arena exactly as it was.
    ///
    /// # Safety
    ///
    /// Every page in `end..base + capacity` must be address space nothing else maps or will map.
    unsafe fn grow(&self, bytes: usize) -> bool {
        let wanted = bytes.max(1).div_ceil(PAGE_BYTES);
        let pages_source = self.pages();
        let state = self.state();
        if state.stats.pages + wanted > state.capacity / PAGE_BYTES {
            return false;
        }
        let old_end = state.end;
        let mut mapped = 0;
        while mapped < wanted {
            if pages_source.map(old_end + mapped * PAGE_BYTES).is_some() {
                state.stats.pages += 1;
                state.stats.grown += 1;
                mapped += 1;
            } else {
                for undo in 0..mapped {
                    pages_source.unmap(old_end + undo * PAGE_BYTES);
                }
                state.stats.pages -= mapped;
                state.stats.grown -= mapped;
                return false;
            }
        }
        state.end = old_end + wanted * PAGE_BYTES;
        state.stats.end = state.end;
        // The new pages are one free block, extended into the tail when the tail was free.
        // An arena with no blocks yet has no tail to read: zero is the "nothing before this".
        let tail = if state.tail != 0 {
            // SAFETY: `tail` is the header of a block inside the arena.
            unsafe { read_block(state.tail) }
        } else {
            Block {
                addr: 0,
                size: 0,
                prev: 0,
                free: false,
                requested: 0,
            }
        };
        let growable = state.tail != 0 && tail.free && tail.addr + tail.size == old_end;
        let grown = if growable {
            // SAFETY: the tail is free and on a list.
            unsafe { state.unlink_free(tail.addr) };
            Block {
                size: tail.size + wanted * PAGE_BYTES,
                prev: tail.prev,
                free: true,
                requested: 0,
                ..tail
            }
        } else {
            Block {
                addr: old_end,
                size: wanted * PAGE_BYTES,
                prev: tail.size,
                free: true,
                requested: 0,
            }
        };
        // SAFETY: the grown block is inside the arena, free, and on no list; the header is
        // written before merging, because merging reads it.
        unsafe { write_block(grown) };
        // SAFETY: as above.
        let merged = unsafe { state.merge(grown.addr) };
        state.tail = merged.addr;
        true
    }
}

/// Bytes a request needs from a block: the header, the payload rounded to the arena's
/// granularity, two alignment steps, and enough left over to split a block worth splitting.
///
/// The two steps are what [`Heap::carve`] may spend moving the payload onto its alignment - once
/// to align it, once more when the gap the first step left is smaller than [`MIN_BLOCK`].
const fn need_for(size: usize, align: usize) -> usize {
    HEADER_BYTES
        .saturating_add(round_up(size, 16))
        .saturating_add(align.saturating_mul(2))
        .saturating_add(MIN_BLOCK)
}

impl<A> Heap<A>
where
    A: Pages,
{
    /// Takes a block of at least `want` bytes off the first class list that has one, splits it to
    /// fit a payload of `requested` bytes aligned to `align`, and returns that payload's address.
    ///
    /// # Safety
    ///
    /// The free lists must be consistent with the arena's headers, which holds as long as every
    /// change to them goes through this module's `merge`, `insert_free` and `unlink_free`.
    unsafe fn take(&self, want: usize, align: usize, requested: usize) -> Option<usize> {
        let state = self.state();
        for class in class_of(want)..CLASSES {
            let mut candidate = state.heads[class];
            while candidate != 0 {
                // SAFETY: a class head and every link reachable from it name a free block inside
                // the arena, so its header is live.
                let block = unsafe { read_block(candidate) };
                // SAFETY: as above.
                let next = unsafe { link(candidate, false) };
                if block.size >= want {
                    // SAFETY: the candidate is free and on this class's list.
                    unsafe { state.unlink_free(candidate) };
                    // SAFETY: the candidate is off every list and large enough for the request.
                    return Some(unsafe { self.carve(block, align, requested) });
                }
                candidate = next;
            }
        }
        None
    }

    /// Cuts a free block that is on no list into the allocation and, where there is room, a
    /// prefix and a suffix that go back on the free lists. Returns the payload's address.
    ///
    /// # Safety
    ///
    /// `block` must be a free block inside this arena, on no list, with `block.size` at least
    /// [`need_for`]`(requested, align)` bytes.
    unsafe fn carve(&self, block: Block, align: usize, requested: usize) -> usize {
        let original_end = block.addr + block.size;
        let mut payload = round_up(block.addr + HEADER_BYTES, align);
        let mut prefix_bytes = payload - HEADER_BYTES - block.addr;
        if prefix_bytes != 0 && prefix_bytes < MIN_BLOCK {
            // A prefix too small to be a block of its own would leave the payload misaligned.
            // `need_for` bought the room for a second alignment step, so take it.
            payload += align;
            prefix_bytes += align;
        }
        let addr = payload - HEADER_BYTES;
        let total = original_end - addr;
        let needed = HEADER_BYTES + round_up(requested, 16);
        let used = if total >= needed + MIN_BLOCK {
            needed
        } else {
            total
        };
        // The allocation's own header goes down before anything is merged: `merge` reads the block
        // after the one it is given, and with a prefix that block is this allocation. Its `prev`
        // is corrected below, once merging may have joined the prefix with the block before it.
        let allocation = Block {
            addr,
            size: used,
            prev: block.prev,
            free: false,
            requested,
        };
        // SAFETY: `addr` is inside the block being carved, and `used` bytes from it are too.
        unsafe { write_block(allocation) };
        if prefix_bytes != 0 {
            let prefix = Block {
                addr: block.addr,
                size: prefix_bytes,
                prev: block.prev,
                free: true,
                requested: 0,
            };
            // SAFETY: the prefix is the head of the block being carved, so its header is live
            // memory inside the arena.
            unsafe { write_block(prefix) };
            // SAFETY: the prefix is free and on no list; merging may join it with the block
            // before it, which is why the allocation's `prev` is read back from the result.
            let merged = unsafe { self.state().merge(prefix.addr) };
            let fixed = Block {
                prev: merged.size,
                ..allocation
            };
            // SAFETY: `addr` is the allocation's header, written above and not on any list.
            unsafe { write_block(fixed) };
        }
        let state = self.state();
        if used < total {
            let suffix = Block {
                addr: addr + used,
                size: total - used,
                prev: used,
                free: true,
                requested: 0,
            };
            // SAFETY: the suffix is inside the block being carved.
            unsafe { write_block(suffix) };
            // SAFETY: the suffix is free and on no list; merging may join it with the block
            // after it.
            unsafe { state.merge(suffix.addr) };
        } else {
            if addr + used < state.end {
                // SAFETY: the block after this one is inside the arena.
                let mut following = unsafe { read_block(addr + used) };
                following.prev = used;
                // SAFETY: `addr + used` is that block's live header, inside the arena.
                unsafe { write_block(following) };
            }
            if addr + used == state.end {
                state.tail = addr;
            }
        }
        payload
    }
}

impl<A> Heap<A>
where
    A: Pages,
{
    /// Hands out a block for `layout`, growing the arena when no free block fits.
    ///
    /// # Safety
    ///
    /// The same promise `GlobalAlloc::alloc` takes from its caller.
    pub unsafe fn alloc_layout(&self, layout: Layout) -> *mut u8 {
        if layout.size() == 0 {
            // An aligned dangling pointer is a valid empty allocation, and this keeps a
            // zero-sized request from consuming a block it would never be able to give back.
            return layout.align() as *mut u8;
        }
        if !self.ready() {
            return core::ptr::null_mut();
        }
        let want = need_for(layout.size(), layout.align());
        // SAFETY: the free lists are this module's, and the caller promises not to hold a
        // reference the arena would invalidate.
        let payload = match unsafe { self.take(want, layout.align(), layout.size()) } {
            Some(payload) => payload,
            None => {
                // SAFETY: every page of the span is address space nothing else maps, and the
                // span itself was promised to `init_arena`.
                if !unsafe { self.grow(want) } {
                    // Out of arena: null is the only honest answer.
                    return core::ptr::null_mut();
                }
                // SAFETY: as above; the arena now holds a free block, unless the block that was
                // grown is smaller than the request.
                match unsafe { self.take(want, layout.align(), layout.size()) } {
                    Some(payload) => payload,
                    None => return core::ptr::null_mut(),
                }
            }
        };
        let state = self.state();
        state.stats.allocations += 1;
        state.stats.bytes_in_use += layout.size();
        state.stats.high_water = state.stats.high_water.max(state.stats.bytes_in_use);
        payload as *mut u8
    }

    /// Takes back a block [`Heap::alloc_layout`] handed out, and refuses anything that does not
    /// match it.
    ///
    /// # Safety
    ///
    /// The same promise `GlobalAlloc::dealloc` takes: `ptr` was returned by this allocator with
    /// this `layout`, or is null.
    pub unsafe fn dealloc_layout(&self, ptr: *mut u8, layout: Layout) {
        if ptr.is_null() {
            return;
        }
        let state = self.state();
        // SAFETY: `state` is this allocator's, and `ptr` may be anything the caller passed.
        let Some(block) = (unsafe { live_block(state, ptr as usize, layout.size()) }) else {
            state.stats.bad_frees += 1;
            return;
        };
        let header = block.addr;
        let freed = Block {
            free: true,
            requested: 0,
            ..block
        };
        // SAFETY: `header` is a live header of an allocated block.
        unsafe { write_block(freed) };
        // SAFETY: the block is free and on no list, so merging is free to join it with its
        // neighbours.
        let merged = unsafe { state.merge(header) };
        state.stats.frees += 1;
        state.stats.bytes_in_use -= block.requested;
        // SAFETY: `merged` is free and on a list, which is what trimming requires.
        unsafe { self.trim(merged) };
    }
}

/// The block behind a payload, if `ptr` names one of this allocator's blocks of `size` bytes.
///
/// A pointer outside the arena, one that is not where a block's header would be, one whose header
/// lost its magic (never a block of ours), one already free (a double free) and one allocated with
/// a different size all answer `None`. That is the check ADR-030 asks for: a free the allocator
/// cannot account for is counted, not followed.
///
/// # Safety
///
/// `state` must be this allocator's state; `ptr` may be anything.
unsafe fn live_block(state: &State, ptr: usize, size: usize) -> Option<Block> {
    if state.base == 0
        || ptr < state.base + HEADER_BYTES
        || ptr >= state.end
        || !ptr.is_multiple_of(HEADER_BYTES)
    {
        return None;
    }
    // SAFETY: `ptr - HEADER_BYTES` is inside the arena, so four readable words exist there; the
    // magic check inside `read_block` decides whether they are a header.
    let block = unsafe { read_block(ptr - HEADER_BYTES) };
    if block.size == 0 || block.free || block.requested != size {
        return None;
    }
    Some(block)
}

impl<A> Heap<A>
where
    A: Pages,
{
    /// Resizes a block, keeping the bytes that still fit.
    ///
    /// # Safety
    ///
    /// The same promise `GlobalAlloc::realloc` takes: `ptr` was returned by this allocator with
    /// `layout`, and `new_size` still leaves a valid layout.
    pub unsafe fn realloc_layout(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        if ptr.is_null() {
            // SAFETY: the caller promises `new_size` and `layout`'s alignment are a valid layout.
            let fresh = unsafe { Layout::from_size_align_unchecked(new_size, layout.align()) };
            // SAFETY: `fresh` is that valid layout, so the allocation path's promise holds.
            return unsafe { self.alloc_layout(fresh) };
        }
        if new_size == 0 {
            // SAFETY: the caller promises `ptr` came from this allocator with `layout`.
            unsafe { self.dealloc_layout(ptr, layout) };
            return core::ptr::null_mut();
        }
        let state = self.state();
        // SAFETY: `state` is this allocator's and `ptr` may be anything.
        let live = unsafe { live_block(state, ptr as usize, layout.size()) };
        if new_size <= layout.size() {
            // Shrinking in place: the block already covers it, and recording the smaller size is
            // what keeps a later free from being refused as a mismatch.
            let Some(block) = live else {
                return core::ptr::null_mut();
            };
            let shrunk = Block {
                requested: new_size,
                ..block
            };
            // SAFETY: `block.addr` is that block's live header.
            unsafe { write_block(shrunk) };
            state.stats.bytes_in_use -= layout.size() - new_size;
            return ptr;
        }
        // Growing: a fresh block, the bytes that still fit, then the old one back. Moving is the
        // simple answer and the one ADR-030 chose; reallocating in place is an optimisation for a
        // heap that turns out to grow the same block often.
        // SAFETY: `new_size` is above a valid size with the same alignment, so this is valid.
        let bigger = unsafe { Layout::from_size_align_unchecked(new_size, layout.align()) };
        // SAFETY: the caller promises `ptr` and `layout`; the allocation path takes the rest.
        let fresh = unsafe { self.alloc_layout(bigger) };
        if fresh.is_null() {
            return fresh;
        }
        // SAFETY: both blocks hold at least `layout.size()` bytes, they are distinct because the
        // new one was just cut from free space, and the copy is followed by the old block going
        // back on the lists.
        unsafe {
            core::ptr::copy_nonoverlapping(ptr, fresh, layout.size());
            self.dealloc_layout(ptr, layout);
        }
        fresh
    }
}

// SAFETY: `alloc` returns null or a pointer into the arena that nothing else has been given,
// aligned as asked and large enough for the layout; `dealloc` refuses anything that does not
// match a live block instead of corrupting the lists; `realloc` keeps the bytes that still fit
// (ADR-030, ADR-031).
unsafe impl<A> GlobalAlloc for Heap<A>
where
    A: Pages,
{
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: the caller of `GlobalAlloc::alloc` makes the promise `alloc_layout` needs.
        unsafe { self.alloc_layout(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: as above, for `dealloc`.
        unsafe { self.dealloc_layout(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        // SAFETY: as above, for `realloc`.
        unsafe { self.realloc_layout(ptr, layout, new_size) }
    }
}

/// Blocks a churn run can hold at once. Sixteen mixes every size class and still fits on the
/// stack, which is what lets the boot path use this.
pub const CHURN_SLOTS: usize = 16;

/// What a churn run saw.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Churn {
    /// Blocks allocated and filled.
    pub allocations: usize,
    /// Blocks freed again.
    pub frees: usize,
    /// Pages the arena grew during the run.
    pub grown: usize,
    /// Pages it handed back during the run.
    pub returned: usize,
    /// `bytes_in_use` before the run.
    pub bytes_before: usize,
    /// `bytes_in_use` after it.
    pub bytes_after: usize,
    /// Whether every block still held the byte written into it.
    pub intact: bool,
}

impl Churn {
    /// Whether the run gave everything back: nothing lost, nothing corrupted.
    pub fn balanced(&self) -> bool {
        self.intact && self.bytes_after == self.bytes_before
    }
}

/// Allocates one block per entry of `sizes`, writes each one through, reads it back, frees them
/// all, and reports what the counters did. At most [`CHURN_SLOTS`] sizes are used.
///
/// This is M4's churn probe and a host test in one function: the kernel calls it on the boot path
/// and prints the result, the tests call it and assert [`Churn::balanced`].
///
/// # Safety
///
/// The caller must hold the same promise [`Heap::alloc_layout`] takes, and must not hold a
/// reference into the arena while this runs.
pub unsafe fn churn<A>(heap: &Heap<A>, sizes: &[usize]) -> Churn
where
    A: Pages,
{
    let before = heap.stats();
    let mut held = [(core::ptr::null_mut::<u8>(), 0usize); CHURN_SLOTS];
    let mut intact = true;
    let mut count = 0;
    for (index, size) in sizes.iter().take(CHURN_SLOTS).enumerate() {
        let Ok(request) = Layout::from_size_align(*size, 8) else {
            intact = false;
            break;
        };
        // SAFETY: `request` is valid and the caller holds the allocation promise.
        let pointer = unsafe { heap.alloc_layout(request) };
        if pointer.is_null() {
            intact = false;
            break;
        }
        // SAFETY: the block is ours for `size` bytes and nothing else writes it.
        unsafe { core::ptr::write_bytes(pointer, index as u8 + 1, *size) };
        held[index] = (pointer, *size);
        count += 1;
    }
    for (index, (pointer, size)) in held.iter().enumerate().take(count) {
        for offset in 0..*size {
            // SAFETY: `offset` is inside the block this run allocated.
            if unsafe { pointer.add(offset).read() } != index as u8 + 1 {
                intact = false;
            }
        }
    }
    let mut frees = 0;
    for (pointer, size) in held.iter().take(count) {
        let Ok(request) = Layout::from_size_align(*size, 8) else {
            continue;
        };
        // SAFETY: each block came from this allocator with this layout, once.
        unsafe { heap.dealloc_layout(*pointer, request) };
        frees += 1;
    }
    let after = heap.stats();
    Churn {
        allocations: count,
        frees,
        grown: after.grown - before.grown,
        returned: after.returned - before.returned,
        bytes_before: before.bytes_in_use,
        bytes_after: after.bytes_in_use,
        intact,
    }
}

/// Runs the three frees that must be refused - a double free, a pointer this arena never handed
/// out, and a size the block was not allocated with - and returns how many were counted.
///
/// The last thing it does is free the block with the size it really had, because a refusal that
/// damaged the block would otherwise be indistinguishable from one that did not.
///
/// # Safety
///
/// As [`churn`].
pub unsafe fn refusals<A>(heap: &Heap<A>) -> usize
where
    A: Pages,
{
    let Ok(block) = Layout::from_size_align(64, 8) else {
        return 0;
    };
    let Ok(wrong) = Layout::from_size_align(32, 8) else {
        return 0;
    };
    let before = heap.stats().bad_frees;
    // SAFETY: `block` is valid and the caller holds the allocation promise.
    let pointer = unsafe { heap.alloc_layout(block) };
    if pointer.is_null() {
        return 0;
    }
    let outside = 0u64;
    // SAFETY: the block is ours, and each call below is a caller's promise for a free.
    unsafe {
        heap.dealloc_layout(pointer, block);
        // Second time: the header says free, so this has to be refused.
        heap.dealloc_layout(pointer, block);
        // A stack address: never came from this arena, so this has to be refused.
        heap.dealloc_layout(core::ptr::from_ref(&outside).cast_mut().cast::<u8>(), block);
    }
    // SAFETY: as above.
    let second = unsafe { heap.alloc_layout(block) };
    if second.is_null() {
        return heap.stats().bad_frees - before;
    }
    // SAFETY: as above; the size differs from the one the block was allocated with.
    unsafe { heap.dealloc_layout(second, wrong) };
    let counted = heap.stats().bad_frees - before;
    // SAFETY: the block is still allocated, and this is the free with its real size.
    unsafe { heap.dealloc_layout(second, block) };
    counted
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Pages the fake source can map. The store below is the arena's memory, so a bug that walks
    /// off the end writes into the array's neighbours and is caught by the assertions rather than
    /// becoming corruption somewhere else.
    const PAGES: usize = 64;

    #[repr(align(4096))]
    struct Store([u8; PAGE_BYTES * PAGES]);

    /// A page source over an array: every page is mapped before it is used, and unmapping is
    /// visible in the statistics the way a frame going back to the allocator is.
    struct Fake<'a> {
        store: &'a mut Store,
        mapped: [bool; PAGES],
    }

    impl Pages for Fake<'_> {
        fn map(&mut self, address: usize) -> Option<u64> {
            let base = self.store.0.as_ptr() as usize;
            if address < base || address % PAGE_BYTES != 0 {
                return None;
            }
            let index = (address - base) / PAGE_BYTES;
            match self.mapped.get(index).copied() {
                Some(false) => {
                    self.mapped[index] = true;
                    Some((index * PAGE_BYTES) as u64)
                }
                // Already mapped, or past the store: both are "cannot map".
                _ => None,
            }
        }

        fn unmap(&mut self, address: usize) -> bool {
            let base = self.store.0.as_ptr() as usize;
            if address < base || address % PAGE_BYTES != 0 {
                return false;
            }
            let index = (address - base) / PAGE_BYTES;
            match self.mapped.get(index).copied() {
                Some(true) => {
                    self.mapped[index] = false;
                    true
                }
                _ => false,
            }
        }
    }

    /// An arena over the calling test's own array, ready to allocate.
    fn arena(store: &mut Store) -> Heap<Fake<'_>> {
        let base = store.0.as_ptr() as usize;
        let arena = Heap::new(Fake {
            store,
            mapped: [false; PAGES],
        });
        // SAFETY: the span is exactly the array the fake backs, and nothing else in the test maps
        // it.
        assert!(unsafe { arena.init_arena(base, PAGE_BYTES * PAGES) });
        arena
    }

    /// A layout with an alignment that does not exceed its size, which is what `alloc` documents.
    fn layout(size: usize) -> Layout {
        Layout::from_size_align(size, 8).expect("valid layout")
    }

    #[test]
    fn an_arena_that_was_never_pointed_at_hands_out_nothing() {
        let mut store = Store([0; PAGE_BYTES * PAGES]);
        let arena = Heap::new(Fake {
            store: &mut store,
            mapped: [false; PAGES],
        });
        assert!(!arena.ready());
        // SAFETY: nothing is mapped, and an arena with no span returns null before touching
        // memory, so no address it could return is ever written.
        assert!(unsafe { arena.alloc_layout(layout(8)) }.is_null());
        assert_eq!(arena.stats().base, 0);
        assert_eq!(arena.stats().pages, 0);
    }

    #[test]
    fn a_span_off_a_page_or_run_twice_is_refused() {
        let mut store = Store([0; PAGE_BYTES * PAGES]);
        let base = store.0.as_ptr() as usize;
        let arena = Heap::new(Fake {
            store: &mut store,
            mapped: [false; PAGES],
        });
        // SAFETY: every call is refused before anything is mapped or written.
        unsafe {
            assert!(!arena.init_arena(base + 1, PAGE_BYTES));
            assert!(!arena.init_arena(base, PAGE_BYTES + 1));
            assert!(!arena.init_arena(base, 0));
            assert!(!arena.init_arena(0, PAGE_BYTES));
            assert!(arena.init_arena(base, PAGE_BYTES));
            assert!(
                !arena.init_arena(base, PAGE_BYTES * 2),
                "an arena must not move after it is in use"
            );
        }
        assert_eq!(arena.stats().base, base);
    }

    #[test]
    fn the_first_allocation_grows_exactly_one_page() {
        let mut store = Store([0; PAGE_BYTES * PAGES]);
        let arena = arena(&mut store);
        assert_eq!(arena.stats().pages, 0, "an arena starts unmapped");
        // SAFETY: `layout(64)` is valid and 64 bytes fit in the page this grows.
        let pointer = unsafe { arena.alloc_layout(layout(64)) };
        assert!(!pointer.is_null(), "{:?}", arena.stats());
        let stats = arena.stats();
        assert_eq!(stats.pages, 1);
        assert_eq!(stats.grown, 1);
        assert_eq!(stats.end, stats.base + PAGE_BYTES);
        assert_eq!(pointer as usize / PAGE_BYTES, stats.base / PAGE_BYTES);
        // SAFETY: the block is ours for 64 bytes.
        unsafe { arena.dealloc_layout(pointer, layout(64)) };
    }

    #[test]
    fn every_class_gets_the_alignment_it_asked_for() {
        let mut store = Store([0; PAGE_BYTES * PAGES]);
        let arena = arena(&mut store);
        let mut seen = [0usize; 9];
        for (count, align) in [16, 32, 64, 128, 256, 512, 1024, 2048, 4096]
            .into_iter()
            .enumerate()
        {
            let request = Layout::from_size_align(100, align).expect("valid layout");
            // SAFETY: the layout is valid.
            let pointer = unsafe { arena.alloc_layout(request) };
            assert!(!pointer.is_null(), "alignment {align}");
            assert_eq!(pointer as usize % align, 0, "alignment {align}");
            seen[count] = pointer as usize;
        }
        for (index, address) in seen.iter().enumerate() {
            for other in seen.iter().skip(index + 1) {
                assert_ne!(address, other, "two allocations share an address");
            }
        }
    }

    #[test]
    fn a_second_allocation_splits_the_first_page_rather_than_growing() {
        let mut store = Store([0; PAGE_BYTES * PAGES]);
        let arena = arena(&mut store);
        // SAFETY: both layouts are valid and the arena grows as it needs to.
        let first = unsafe { arena.alloc_layout(layout(64)) };
        let pages_after_first = arena.stats().pages;
        // SAFETY: as above.
        let second = unsafe { arena.alloc_layout(layout(64)) };
        assert!(!first.is_null() && !second.is_null());
        let stats = arena.stats();
        assert_eq!(
            stats.pages, pages_after_first,
            "a free block was there to split"
        );
        assert_eq!(stats.grown, 1);
        assert_ne!(first, second);
        assert_eq!(stats.allocations, 2);
        assert_eq!(stats.bytes_in_use, 128);
    }

    #[test]
    fn a_free_merges_with_the_block_before_it() {
        let mut store = Store([0; PAGE_BYTES * PAGES]);
        let arena = arena(&mut store);
        // SAFETY: the layouts are valid and each block is ours to free once.
        unsafe {
            let first = arena.alloc_layout(layout(100));
            let second = arena.alloc_layout(layout(100));
            let third = arena.alloc_layout(layout(100));
            assert!(!first.is_null() && !second.is_null() && !third.is_null());
            arena.dealloc_layout(second, layout(100));
            arena.dealloc_layout(first, layout(100));
            // Both neighbours are free again, so a request that fits in them has to land in the
            // merged block rather than growing the arena: that is coalescing doing its job.
            assert_eq!(
                arena.alloc_layout(layout(150)),
                first,
                "the two 100-byte blocks did not merge"
            );
            arena.dealloc_layout(third, layout(100));
            arena.dealloc_layout(first, layout(150));
        }
        assert_eq!(arena.stats().frees, 4);
    }

    #[test]
    fn freeing_the_top_of_the_arena_gives_pages_back() {
        let mut store = Store([0; PAGE_BYTES * PAGES]);
        let arena = arena(&mut store);
        // SAFETY: the layout is valid, and the arena grows by the two pages this needs.
        let pointer = unsafe { arena.alloc_layout(layout(PAGE_BYTES)) };
        assert!(!pointer.is_null());
        let grown = arena.stats();
        assert!(grown.pages >= 2, "a page of payload needs a header too");
        // SAFETY: the block is ours, once.
        unsafe { arena.dealloc_layout(pointer, layout(PAGE_BYTES)) };
        let after = arena.stats();
        assert_eq!(after.pages, 0, "every page went back");
        assert_eq!(after.returned, grown.grown);
        assert_eq!(after.end, after.base);
        assert_eq!(after.bytes_in_use, 0);
    }

    #[test]
    fn a_double_free_is_refused_and_counted() {
        let mut store = Store([0; PAGE_BYTES * PAGES]);
        let arena = arena(&mut store);
        // SAFETY: the layout is valid; the second free is the one that must be refused, and the
        // point is that refusing it is not an early-return that damages the block.
        unsafe {
            let pointer = arena.alloc_layout(layout(64));
            assert!(!pointer.is_null());
            arena.dealloc_layout(pointer, layout(64));
            arena.dealloc_layout(pointer, layout(64));
        }
        let stats = arena.stats();
        assert_eq!(stats.frees, 1);
        assert_eq!(stats.bad_frees, 1);
        assert_eq!(stats.bytes_in_use, 0, "the second free changed nothing");
    }

    #[test]
    fn a_pointer_that_never_came_from_the_arena_is_refused() {
        let mut store = Store([0; PAGE_BYTES * PAGES]);
        let arena = arena(&mut store);
        let outside = 0u64;
        // SAFETY: `outside` is a stack address, so the free has to be refused before anything is
        // read or written through it.
        unsafe {
            arena.dealloc_layout(
                core::ptr::from_ref(&outside).cast_mut().cast::<u8>(),
                layout(8),
            );
        }
        assert_eq!(arena.stats().bad_frees, 1);
        assert_eq!(arena.stats().frees, 0);
    }

    #[test]
    fn a_size_that_does_not_match_its_block_is_refused() {
        let mut store = Store([0; PAGE_BYTES * PAGES]);
        let arena = arena(&mut store);
        // SAFETY: the block is allocated with 64 bytes; the 32-byte free must be refused, and the
        // 64-byte one that follows must still work.
        unsafe {
            let pointer = arena.alloc_layout(layout(64));
            assert!(!pointer.is_null());
            arena.dealloc_layout(pointer, layout(32));
            assert_eq!(arena.stats().bad_frees, 1);
            arena.dealloc_layout(pointer, layout(64));
        }
        let stats = arena.stats();
        assert_eq!(stats.frees, 1);
        assert_eq!(stats.bytes_in_use, 0);
    }

    #[test]
    fn realloc_keeps_the_bytes_that_still_fit() {
        let mut store = Store([0; PAGE_BYTES * PAGES]);
        let arena = arena(&mut store);
        // SAFETY: the block is ours, the copy inside `realloc` reads only the old size, and the
        // reads after it stay inside the new size.
        unsafe {
            let pointer = arena.alloc_layout(layout(40));
            assert!(!pointer.is_null());
            for offset in 0..40 {
                pointer.add(offset).write(offset as u8);
            }
            let bigger = arena.realloc_layout(pointer, layout(40), 400);
            assert!(!bigger.is_null());
            for offset in 0..40 {
                assert_eq!(bigger.add(offset).read(), offset as u8, "byte {offset}");
            }
            assert_eq!(arena.stats().bytes_in_use, 400);
            arena.dealloc_layout(bigger, layout(400));
        }
        assert_eq!(arena.stats().bytes_in_use, 0);
    }

    #[test]
    fn realloc_shrinks_in_place_and_the_smaller_size_still_frees() {
        let mut store = Store([0; PAGE_BYTES * PAGES]);
        let arena = arena(&mut store);
        // SAFETY: the block is ours; the second free uses the size `realloc` recorded.
        unsafe {
            let pointer = arena.alloc_layout(layout(400));
            assert!(!pointer.is_null());
            let smaller = arena.realloc_layout(pointer, layout(400), 40);
            assert_eq!(smaller, pointer, "shrinking does not have to move");
            assert_eq!(arena.stats().bytes_in_use, 40);
            arena.dealloc_layout(smaller, layout(40));
        }
        assert_eq!(arena.stats().bytes_in_use, 0);
        assert_eq!(arena.stats().bad_frees, 0);
    }

    #[test]
    fn a_request_larger_than_the_arena_is_refused() {
        let mut store = Store([0; PAGE_BYTES * PAGES]);
        let arena = arena(&mut store);
        // SAFETY: the request is valid, and the arena refuses it before mapping anything past its
        // span.
        let pointer = unsafe { arena.alloc_layout(layout(PAGE_BYTES * PAGES)) };
        assert!(pointer.is_null());
        assert_eq!(arena.stats().pages, 0, "a refused growth maps nothing");
    }

    #[test]
    fn churn_returns_the_counters_to_where_they_started() {
        let mut store = Store([0; PAGE_BYTES * PAGES]);
        let arena = arena(&mut store);
        // SAFETY: every layout in the list is valid, and the run frees what it allocates.
        let report = unsafe { churn(&arena, &[16, 32, 64, 100, 500, 2000, 4096, 64]) };
        assert!(report.intact, "a block lost what was written into it");
        assert_eq!(report.allocations, 8);
        assert_eq!(report.frees, 8);
        assert_eq!(report.bytes_after, report.bytes_before);
        assert!(report.balanced());
        assert!(report.grown > 0, "the run had to grow the arena");
        assert_eq!(
            arena.stats().high_water,
            16 + 32 + 64 + 100 + 500 + 2000 + 4096 + 64
        );
    }

    #[test]
    fn the_three_frees_that_must_be_refused_are_counted() {
        let mut store = Store([0; PAGE_BYTES * PAGES]);
        let arena = arena(&mut store);
        // SAFETY: `refusals` allocates the blocks it frees and frees each one once with the size
        // it was allocated with.
        let refused = unsafe { refusals(&arena) };
        assert_eq!(refused, 3, "double free, bogus pointer and size mismatch");
        assert_eq!(arena.stats().bad_frees, 3);
        assert_eq!(
            arena.stats().bytes_in_use,
            0,
            "the block still freed cleanly"
        );
    }
}
