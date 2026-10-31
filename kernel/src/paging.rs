//! Page tables: the entry layout, the index arithmetic, and the walk that translates a
//! virtual address.
//!
//! This is M3's first piece (ADR-029) because it needs no allocator: a walk reads whatever
//! tables `CR3` points at, which is how the framebuffer's virtual pointer becomes the physical
//! span `memory::pick_heap_region` has to be given. The entry layout and the walk are plain
//! arithmetic over 512-entry arrays, so the host tests drive them with a table built by hand;
//! the half that touches `CR3`, the physical window or `invlpg` is `x86_64`-only.
//!
//! A 4-level table splits a virtual address into four 9-bit indices and a 12-bit offset. A
//! leaf can sit at the PD level (2 MiB) or the PDPT level (1 GiB) as well as at the PT level
//! (4 KiB); the bootloader maps the physical window with 2 MiB pages, so the walk has to
//! understand them or it would report the window as a tree of tables that is not there.

use crate::memory::PAGE_BYTES;

/// Entries in one table: a 4 KiB frame of 8-byte entries.
pub const ENTRIES: usize = 512;

/// One 8-byte page-table entry (Intel SDM 3A, 4.5).
///
/// Wrapped rather than used as a bare `u64` so the bit positions live in one place. A caller
/// that needs the raw value has [`Entry::bits`], and one that has a raw value from a table has
/// [`Entry::from_bits`].
#[repr(transparent)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Entry(u64);

impl Entry {
    /// The CPU may use this entry. Without it the rest of the bits mean nothing.
    pub const PRESENT: u64 = 1 << 0;
    /// Writes are allowed through this mapping.
    pub const WRITABLE: u64 = 1 << 1;
    /// Ring 3 may use this mapping. Nothing here sets it yet; it is named so that a walk can
    /// report what the bootloader left behind.
    pub const USER: u64 = 1 << 2;
    /// At the PD and PDPT levels: this entry maps a page itself instead of pointing at the
    /// next table down. At the PT level the same bit is `PAT` and maps nothing.
    pub const HUGE: u64 = 1 << 7;
    /// Instruction fetches through this mapping fault. Needs `EFER.NXE`, which the
    /// bootloader sets.
    pub const NO_EXECUTE: u64 = 1 << 63;

    /// Bits 12..=51: the physical address of the next table, or of the page a leaf maps.
    /// The limit at bit 52 is the physical address width a 4-level table can hold.
    pub const ADDRESS_MASK: u64 = 0x000f_ffff_ffff_f000;

    /// Flags worth carrying from a huge entry into the 4 KiB entries it is split into.
    /// `ACCESSED` and `DIRTY` belong to the CPU, and `HUGE` must not survive the split.
    pub const COPY_FLAGS: u64 = Self::PRESENT | Self::WRITABLE | Self::USER | Self::NO_EXECUTE;

    /// An entry the CPU treats as absent.
    pub const fn empty() -> Self {
        Self(0)
    }

    /// An entry with the raw bits read out of a table.
    pub const fn from_bits(bits: u64) -> Self {
        Self(bits)
    }

    /// The raw bits, as they go back into a table.
    pub const fn bits(self) -> u64 {
        self.0
    }

    /// Whether the CPU may use this entry.
    pub const fn is_present(self) -> bool {
        self.0 & Self::PRESENT != 0
    }

    /// Whether a write may pass through this mapping.
    pub const fn is_writable(self) -> bool {
        self.0 & Self::WRITABLE != 0
    }

    /// Whether this entry maps a page itself rather than pointing at a lower table.
    pub const fn is_huge(self) -> bool {
        self.0 & Self::HUGE != 0
    }

    /// Whether instructions may be fetched through this mapping.
    pub const fn is_executable(self) -> bool {
        self.0 & Self::NO_EXECUTE == 0
    }

    /// The physical address in this entry, with the flag bits cleared. For a huge entry this
    /// is the base of the 2 MiB or 1 GiB page, because those low bits are reserved zero.
    pub const fn address(self) -> u64 {
        self.0 & Self::ADDRESS_MASK
    }

    /// An entry pointing at the table or page at `address`, carrying `flags`.
    ///
    /// Any address bits that collide with flag bits are dropped, so a caller cannot
    /// accidentally claim an unaligned table and a `PRESENT` bit at once.
    pub const fn pointing_at(address: u64, flags: u64) -> Self {
        Self((address & Self::ADDRESS_MASK) | (flags & !Self::ADDRESS_MASK))
    }
}

/// The four levels of a 4-level table, root first.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Level {
    /// Level 4: the table `CR3` names.
    Pml4,
    /// Level 3.
    Pdpt,
    /// Level 2.
    Pd,
    /// Level 1: the leaf level, always 4 KiB.
    Pt,
}

impl Level {
    /// The order a walk visits them in.
    pub const ALL: [Level; 4] = [Level::Pml4, Level::Pdpt, Level::Pd, Level::Pt];

    /// How far to shift an address to bring this level's 9-bit index down to bits 0..=8.
    const fn shift(self) -> u32 {
        match self {
            Level::Pml4 => 39,
            Level::Pdpt => 30,
            Level::Pd => 21,
            Level::Pt => 12,
        }
    }

    /// Whether an entry at this level may map a page instead of pointing at a table. The PML4
    /// and PT levels cannot: their bit 7 is `PAT` or reserved.
    pub const fn maps_huge_pages(self) -> bool {
        matches!(self, Level::Pdpt | Level::Pd)
    }

    /// Bytes one such huge entry covers. Only meaningful when [`Level::maps_huge_pages`].
    pub const fn huge_bytes(self) -> u64 {
        match self {
            Level::Pdpt => 1 << 30,
            Level::Pd => 1 << 21,
            // No huge form: the walk never asks, and 4 KiB is the least wrong answer.
            Level::Pml4 | Level::Pt => PAGE_BYTES,
        }
    }

    /// The level below this one. The leaf returns itself, because there is nothing below it
    /// and a copy of a leaf's entries has nowhere to descend to.
    pub const fn child(self) -> Level {
        match self {
            Level::Pml4 => Level::Pdpt,
            Level::Pdpt => Level::Pd,
            Level::Pd | Level::Pt => Level::Pt,
        }
    }
}

/// Which of the 512 entries at `level` covers `address`.
pub const fn index(address: u64, level: Level) -> usize {
    ((address >> level.shift()) & (ENTRIES as u64 - 1)) as usize
}

/// What a walk found: the physical address, and how large the mapping it landed in is.
///
/// `size` is 4 KiB, 2 MiB or 1 GiB, and says how much of the virtual address was carried over
/// from the leaf entry rather than followed into another table.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Translation {
    /// The physical address the virtual one maps to.
    pub address: u64,
    /// The size of the mapping, in bytes.
    pub size: u64,
}

/// Translates `address` by walking the table rooted at `root`.
///
/// `read` returns entry `index` of the 4 KiB table at physical address `frame`. Passing it in
/// is what lets the host tests walk a table in an array while the kernel reads the same
/// entries through the physical window: the arithmetic in between is identical, and it is the
/// part that is easy to get wrong.
///
/// Returns `None` as soon as a level is not present, which is the answer for an address
/// nothing maps. A huge entry stops the walk early, because everything below it is implied by
/// the entry rather than stored.
pub fn translate<F>(root: u64, address: u64, mut read: F) -> Option<Translation>
where
    F: FnMut(u64, usize) -> Entry,
{
    let mut table = root;
    for level in Level::ALL {
        let entry = read(table, index(address, level));
        if !entry.is_present() {
            return None;
        }
        if level == Level::Pt {
            return Some(Translation {
                address: entry.address() | (address & (PAGE_BYTES - 1)),
                size: PAGE_BYTES,
            });
        }
        if level.maps_huge_pages() && entry.is_huge() {
            let size = level.huge_bytes();
            return Some(Translation {
                address: entry.address() | (address & (size - 1)),
                size,
            });
        }
        table = entry.address();
    }
    // The loop always returns from the leaf level. `None` is the safe answer if `Level::ALL`
    // is ever edited to skip a level, rather than a translation built from a partial walk.
    None
}

/// What a mapping needs from the tables, so the arithmetic can run without touching memory.
///
/// [`translate`] takes one closure because it does one thing. A mapping reads entries, writes
/// them, zeroes a table it just took and asks the allocator for the next one, so it takes a
/// trait: the host tests implement it over an array of tables and the kernel implements it over
/// the physical window, which is the same trade the walk already makes.
pub trait Tables {
    /// Entry `index` of the 4 KiB table at physical `frame`.
    fn read(&self, frame: u64, index: usize) -> Entry;

    /// Replaces entry `index` of the table at `frame`.
    fn write(&mut self, frame: u64, index: usize, entry: Entry);

    /// Fills the frame at `frame` with zeroes, for a table about to be filled in.
    fn zero(&mut self, frame: u64);

    /// A frame nothing else uses, or `None` when the allocator has none left.
    fn alloc(&mut self) -> Option<u64>;
}

/// Why [`map`] could not put a frame at an address.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MapError {
    /// Something already reaches the address. A huge page counts: the address is mapped either
    /// way, and taking a 2 MiB page apart to reach one 4 KiB entry inside it belongs to the
    /// unmapping path, not to a mapping.
    AlreadyMapped,
    /// The address or the frame is not 4 KiB aligned, so the low bits of the entry would carry
    /// part of an address into a field that is not an address.
    Misaligned,
    /// A level had to be created and the allocator had no frame left. Tables created before
    /// the failure are kept rather than zeroed back out of the tree: they are empty and
    /// unreachable, and the caller is by definition a caller that has no frames to spare.
    NoFrame,
}

/// Maps `frame` at `address` in the tree rooted at `root`, creating the levels it needs.
///
/// The name is the bare verb for the same reason [`translate`] is: this half asks a [`Tables`]
/// implementation instead of reading memory, and `arch::map_4k` is the one that writes through
/// the physical window. A call site can tell the two apart by name alone.
///
/// `flags` are [`Entry`] bits for the leaf - [`Entry::WRITABLE`], [`Entry::NO_EXECUTE`],
/// [`Entry::USER`] - and `PRESENT` is added here, because an entry the CPU treats as absent is
/// not a mapping. The levels created on the way down carry `PRESENT | WRITABLE` and nothing
/// else, which is deliberate twice over: a write needs the `WRITABLE` bit at every level, so
/// leaving it off a parent would make a writable leaf read-only; and `NO_EXECUTE` on a parent
/// applies to everything below it, so putting it there would silently deny a leaf that asked
/// to be executable.
pub fn map<T>(
    tables: &mut T,
    root: u64,
    address: u64,
    frame: u64,
    flags: u64,
) -> Result<(), MapError>
where
    T: Tables,
{
    if !address.is_multiple_of(PAGE_BYTES) || !frame.is_multiple_of(PAGE_BYTES) {
        return Err(MapError::Misaligned);
    }
    let mut table = root;
    for level in [Level::Pml4, Level::Pdpt, Level::Pd] {
        let slot = index(address, level);
        let entry = tables.read(table, slot);
        if !entry.is_present() {
            let fresh = tables.alloc().ok_or(MapError::NoFrame)?;
            tables.zero(fresh);
            tables.write(
                table,
                slot,
                Entry::pointing_at(fresh, Entry::PRESENT | Entry::WRITABLE),
            );
            table = fresh;
            continue;
        }
        if level.maps_huge_pages() && entry.is_huge() {
            return Err(MapError::AlreadyMapped);
        }
        table = entry.address();
    }
    let leaf = index(address, Level::Pt);
    if tables.read(table, leaf).is_present() {
        return Err(MapError::AlreadyMapped);
    }
    tables.write(
        table,
        leaf,
        Entry::pointing_at(frame, flags | Entry::PRESENT),
    );
    Ok(())
}

#[cfg(target_arch = "x86_64")]
mod arch {
    use super::{index, Entry, Level, MapError, Translation, ENTRIES};
    use crate::memory::{Span, PAGE_BYTES};
    use core::arch::asm;

    /// Why [`unmap_4k`] could not leave a hole at an address.
    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    pub enum UnmapError {
        /// Nothing maps the address, so there is no mapping to take away.
        NotMapped,
        /// A 1 GiB entry covers it. Splitting one is more than a guard page needs, and a
        /// limit this code states is better than a limit it hides.
        OneGibPage,
        /// The split needed a table frame and the allocator had none left.
        NoFrame,
    }

    /// The root of the tables the CPU is using: `CR3` with its flag bits removed.
    pub fn root() -> u64 {
        let value: u64;
        // SAFETY: `CR3` is readable from ring 0, and reading it has no side effects.
        unsafe {
            asm!(
                "mov {}, cr3",
                out(reg) value,
                options(nomem, nostack, preserves_flags)
            );
        }
        value & Entry::ADDRESS_MASK
    }

    /// Makes `root` the tables the CPU uses from here on.
    ///
    /// Writing `CR3` also discards the TLB, which is why it is the honest way to switch
    /// address spaces - and why [`unmap_4k`] uses it after a split.
    ///
    /// # Safety
    ///
    /// `root` must name a complete 4-level table in which the code making this call, its
    /// stack, and everything the caller will touch next are mapped. Loading a root that
    /// misses any of those faults on the very next instruction.
    pub unsafe fn load_root(root: u64) {
        // SAFETY: the caller promises `root` is a live table that maps the running code.
        unsafe {
            asm!("mov cr3, {}", in(reg) root, options(nostack, preserves_flags));
        }
    }

    /// Reads entry `index` of the table at physical `frame`, through the window `offset`.
    ///
    /// Volatile because the CPU writes `ACCESSED` and `DIRTY` into these entries: a cached
    /// read would be a value the CPU has since changed.
    ///
    /// # Safety
    ///
    /// `offset` must be the window the bootloader mapped all physical memory at (ADR-022),
    /// `frame` must be the physical address of a table mapped there, and `index` must be one
    /// of its 512 entries.
    unsafe fn read_entry(offset: u64, frame: u64, index: usize) -> Entry {
        let pointer = offset.wrapping_add(frame).wrapping_add(index as u64 * 8) as *const Entry;
        // SAFETY: the caller promises the address is a live table entry.
        unsafe { core::ptr::read_volatile(pointer) }
    }

    /// Writes entry `index` of the table at physical `frame`.
    ///
    /// # Safety
    ///
    /// As [`read_entry`], and the table must be one this kernel owns: overwriting a table the
    /// firmware still believes in is how a machine stops booting at the next power cycle.
    unsafe fn write_entry(offset: u64, frame: u64, index: usize, entry: Entry) {
        let pointer = offset.wrapping_add(frame).wrapping_add(index as u64 * 8) as *mut Entry;
        // SAFETY: as `read_entry`.
        unsafe { core::ptr::write_volatile(pointer, entry) };
    }

    /// Fills the frame at `frame` with zeroes, through the window at `offset`.
    ///
    /// # Safety
    ///
    /// `frame` must be a frame nothing else uses, and mapped at `offset`.
    unsafe fn zero_frame(offset: u64, frame: u64) {
        let pointer = offset.wrapping_add(frame) as *mut u8;
        // SAFETY: `PAGE_BYTES` bytes start at the frame, which the caller owns.
        unsafe { core::ptr::write_bytes(pointer, 0, PAGE_BYTES as usize) };
    }

    /// Translates `address` through the tables `root` names.
    ///
    /// The name differs from the arithmetic in [`super::translate`] so that the two cannot be
    /// confused at a call site: this one reads memory through the physical window, and the
    /// other one asks a closure.
    ///
    /// # Safety
    ///
    /// `root` must name a live 4-level table and `offset` must be the physical window.
    pub unsafe fn walk(root: u64, offset: u64, address: u64) -> Option<Translation> {
        super::translate(root, address, |frame, entry| {
            // SAFETY: `frame` came from an entry of a table `root` names, so it is a table
            // too; the caller promises both are mapped.
            unsafe { read_entry(offset, frame, entry) }
        })
    }

    /// The physical bytes behind `len` bytes of virtual memory starting at `address`.
    ///
    /// `None` when any of the range is unmapped, or when the frames behind it are not
    /// contiguous. The second half matters as much as the first: a range that straddles a
    /// hole would otherwise come back as a span that claims memory it does not own, and
    /// `memory::pick_heap_region` acts on that claim.
    ///
    /// # Safety
    ///
    /// As [`walk`].
    pub unsafe fn span(root: u64, offset: u64, address: u64, len: u64) -> Option<Span> {
        let last = address.checked_add(len.checked_sub(1)?)?;
        // SAFETY: the caller promises the tables are live and the window maps them.
        let start = unsafe { walk(root, offset, address) }?;
        // SAFETY: as above.
        let end = unsafe { walk(root, offset, last) }?;
        // Every byte in between has to land where linear arithmetic says it does.
        if end.address.checked_sub(start.address)? != last - address {
            return None;
        }
        Some(Span::new(start.address, end.address + 1))
    }

    /// Copies the table at `frame`, and every table below it, into frames from `alloc`.
    ///
    /// Returns the copy's own frame, or `None` if the allocator ran out. Frames taken before
    /// a failure are not handed back: the boot is about to stop, and a half-built tree is not
    /// a state worth handing on.
    ///
    /// A huge entry is copied as it stands - everything below it is implied by the entry
    /// itself, so there is no table to walk, and preserving the huge page keeps the copy the
    /// same shape as the original.
    ///
    /// # Safety
    ///
    /// As [`walk`], and `frame` must be the root of the tree being copied.
    unsafe fn copy_table<A>(frame: u64, offset: u64, level: Level, alloc: &mut A) -> Option<u64>
    where
        A: FnMut() -> Option<u64>,
    {
        let copy = alloc()?;
        // SAFETY: `copy` is unused until this returns, and `offset` maps it.
        unsafe { zero_frame(offset, copy) };
        for entry in 0..ENTRIES {
            // SAFETY: `frame` is a table of this level and `entry` is one of its indices.
            let source = unsafe { read_entry(offset, frame, entry) };
            if !source.is_present() {
                continue;
            }
            let copied = if level == Level::Pt || (level.maps_huge_pages() && source.is_huge()) {
                source
            } else {
                // SAFETY: the entry is present and not huge, so it names a table one level
                // down, which is part of the same tree.
                let below = unsafe { copy_table(source.address(), offset, level.child(), alloc) }?;
                Entry::pointing_at(below, source.bits() & !Entry::ADDRESS_MASK)
            };
            // SAFETY: `copy` is the frame allocated above, and `entry` is in range.
            unsafe { write_entry(offset, copy, entry, copied) };
        }
        Some(copy)
    }

    /// Builds an independent copy of the tree rooted at `root`.
    ///
    /// The copy maps exactly what the original mapped, including the kernel, its stack, the
    /// boot info, the physical window and the framebuffer, so the switch is invisible except
    /// for the ownership. That is the point: the kernel can then edit entries without
    /// changing tables the bootloader still believes it owns (ADR-029).
    ///
    /// # Safety
    ///
    /// As [`walk`], and `alloc` must hand out frames nothing else uses.
    pub unsafe fn clone_root<A>(root: u64, offset: u64, mut alloc: A) -> Option<u64>
    where
        A: FnMut() -> Option<u64>,
    {
        // SAFETY: the caller promises `root` is a live tree and `alloc` unused frames.
        unsafe { copy_table(root, offset, Level::Pml4, &mut alloc) }
    }

    /// Takes away the 4 KiB mapping at `address`, splitting a 2 MiB page if one covers it.
    ///
    /// The physical window is mapped with 2 MiB pages, so a single 4 KiB hole inside it is not
    /// something the tables can express until the page covering it is replaced by a table of
    /// 4 KiB entries. That split is the whole reason this function exists: it is how the heap
    /// guard page becomes unmapped while its neighbours stay exactly as they were.
    ///
    /// # Safety
    ///
    /// As [`walk`], and `address` must be mapped by `root`.
    pub unsafe fn unmap_4k<A>(
        root: u64,
        offset: u64,
        address: u64,
        mut alloc: A,
    ) -> Result<(), UnmapError>
    where
        A: FnMut() -> Option<u64>,
    {
        // SAFETY: every read and write below goes through a frame this walk itself took out
        // of a live table, so each one is a table mapped at `offset`.
        unsafe {
            let pml4 = read_entry(offset, root, index(address, Level::Pml4));
            if !pml4.is_present() {
                return Err(UnmapError::NotMapped);
            }
            let pdpt = read_entry(offset, pml4.address(), index(address, Level::Pdpt));
            if !pdpt.is_present() {
                return Err(UnmapError::NotMapped);
            }
            if pdpt.is_huge() {
                return Err(UnmapError::OneGibPage);
            }
            let pd_frame = pdpt.address();
            let pd_entry = index(address, Level::Pd);
            let mut pd = read_entry(offset, pd_frame, pd_entry);
            if !pd.is_present() {
                return Err(UnmapError::NotMapped);
            }
            if pd.is_huge() {
                // One table describing the same 2 MiB with 4 KiB entries, and a PD entry
                // pointing at it. Nothing moves physically: only the granularity changes.
                let base = pd.address();
                let flags = pd.bits() & Entry::COPY_FLAGS;
                let table = alloc().ok_or(UnmapError::NoFrame)?;
                zero_frame(offset, table);
                for entry in 0..ENTRIES {
                    let frame = base + entry as u64 * PAGE_BYTES;
                    write_entry(offset, table, entry, Entry::pointing_at(frame, flags));
                }
                write_entry(offset, pd_frame, pd_entry, Entry::pointing_at(table, flags));
                // The 2 MiB translation is cached and the CPU does not know the tables below
                // it changed, so the TLB has to go before the new entries are read.
                load_root(root);
                pd = Entry::pointing_at(table, flags);
            }
            let pt_frame = pd.address();
            let leaf = index(address, Level::Pt);
            if !read_entry(offset, pt_frame, leaf).is_present() {
                return Err(UnmapError::NotMapped);
            }
            write_entry(offset, pt_frame, leaf, Entry::empty());
            // One page changed, so one translation has to go; `invlpg` is the narrow
            // instruction for that (Intel SDM 3A, 4.10.4).
            asm!("invlpg [{}]", in(reg) address, options(nostack, preserves_flags));
        }
        Ok(())
    }

    /// The tables the CPU uses, reached through the physical window at `offset`.
    ///
    /// [`super::Tables`] over memory: every method is one of the `unsafe` helpers above, and
    /// the promises those make are what [`map_4k`] extracts from its caller. Nothing outside
    /// this module builds one, so a `Window` cannot outlive those promises.
    struct Window<'a, A> {
        offset: u64,
        alloc: &'a mut A,
    }

    impl<A> super::Tables for Window<'_, A>
    where
        A: FnMut() -> Option<u64>,
    {
        fn read(&self, frame: u64, index: usize) -> Entry {
            // SAFETY: `map_4k` promises a live tree behind the window, and every `frame` it
            // passes came out of a present entry of that tree.
            unsafe { read_entry(self.offset, frame, index) }
        }

        fn write(&mut self, frame: u64, index: usize, entry: Entry) {
            // SAFETY: as `read`.
            unsafe { write_entry(self.offset, frame, index, entry) }
        }

        fn zero(&mut self, frame: u64) {
            // SAFETY: `frame` came from the allocator, which hands out each frame once.
            unsafe { zero_frame(self.offset, frame) }
        }

        fn alloc(&mut self) -> Option<u64> {
            (self.alloc)()
        }
    }

    /// Gives `frame` the virtual address `address`, creating the levels above it.
    ///
    /// The mirror of [`unmap_4k`], and the half of this module M3 did not need: taking a mapping
    /// away was enough to arm a guard page (ADR-029), while everything past that - a heap that
    /// grows, a driver's buffer, a user page - has to be given an address first (ADR-030).
    ///
    /// # Safety
    ///
    /// `root` must name a live 4-level table, `offset` must be the physical window (ADR-022),
    /// `frame` must be a frame nothing else uses, and `alloc` must hand out frames nothing else
    /// uses. `address` must not be the address of anything this call is running on: a leaf
    /// written over the code or the stack takes effect at the next translation, which is the
    /// instruction after this one.
    pub unsafe fn map_4k<A>(
        root: u64,
        offset: u64,
        address: u64,
        frame: u64,
        flags: u64,
        mut alloc: A,
    ) -> Result<(), MapError>
    where
        A: FnMut() -> Option<u64>,
    {
        let mut window = Window {
            offset,
            alloc: &mut alloc,
        };
        // SAFETY: the caller promises the tree, the window, the frame and the allocator.
        let outcome = super::map(&mut window, root, address, frame, flags);
        if outcome.is_ok() {
            // A translation cached before the entry was written would outlive the entry that
            // now describes it - the address may have been mapped earlier in this boot - so it
            // goes before anything reads through the new mapping.
            // SAFETY: `invlpg` touches the TLB and nothing else, and cannot fault.
            unsafe {
                asm!("invlpg [{}]", in(reg) address, options(nostack, preserves_flags));
            }
        }
        outcome
    }
}

#[cfg(target_arch = "x86_64")]
pub use arch::{clone_root, load_root, map_4k, root, span, unmap_4k, walk, UnmapError};

#[cfg(test)]
mod tests {
    use super::*;

    /// The virtual address every hand-built table below maps. Only the middle of the
    /// address matters to the arithmetic, so it is deliberately unremarkable.
    const VIRTUAL: u64 = 0x0000_4000_1234_5678;

    /// The pretend physical address of each table, so `read` can find it.

    const L4: u64 = 0x1000;
    const L3: u64 = 0x2000;
    const L2: u64 = 0x3000;
    const L1: u64 = 0x4000;

    /// Four tables in memory, addressed the way the walk addresses real ones: by the
    /// physical address in the parent entry. Nothing is ever dereferenced, so a table is
    /// just an array with a number attached.
    struct Fake {
        pml4: [Entry; ENTRIES],
        pdpt: [Entry; ENTRIES],
        pd: [Entry; ENTRIES],
        pt: [Entry; ENTRIES],
    }

    impl Fake {
        /// Four tables with every entry absent.
        fn empty() -> Self {
            Self {
                pml4: [Entry::empty(); ENTRIES],
                pdpt: [Entry::empty(); ENTRIES],
                pd: [Entry::empty(); ENTRIES],
                pt: [Entry::empty(); ENTRIES],
            }
        }

        /// A complete 4 KiB mapping of [`VIRTUAL`] to `frame`.
        fn page(frame: u64) -> Self {
            let mut fake = Self::empty();
            fake.table_mut(Level::Pml4)[index(VIRTUAL, Level::Pml4)] =
                Entry::pointing_at(L3, Entry::PRESENT);
            fake.table_mut(Level::Pdpt)[index(VIRTUAL, Level::Pdpt)] =
                Entry::pointing_at(L2, Entry::PRESENT);
            fake.table_mut(Level::Pd)[index(VIRTUAL, Level::Pd)] =
                Entry::pointing_at(L1, Entry::PRESENT);
            fake.table_mut(Level::Pt)[index(VIRTUAL, Level::Pt)] =
                Entry::pointing_at(frame, Entry::PRESENT | Entry::WRITABLE);
            fake
        }

        /// The table at `level`, for a test that wants to replace one entry.
        fn table_mut(&mut self, level: Level) -> &mut [Entry; ENTRIES] {
            match level {
                Level::Pml4 => &mut self.pml4,
                Level::Pdpt => &mut self.pdpt,
                Level::Pd => &mut self.pd,
                Level::Pt => &mut self.pt,
            }
        }

        /// Reads entry `index` of the table at `frame`, which is what a walk through the
        /// physical window does with real tables.
        fn read(&self, frame: u64, index: usize) -> Entry {
            match frame {
                L4 => self.pml4[index],
                L3 => self.pdpt[index],
                L2 => self.pd[index],
                L1 => self.pt[index],
                _ => Entry::empty(),
            }
        }

        /// Walks [`VIRTUAL`] through this table.
        fn translate(&self) -> Option<Translation> {
            translate(L4, VIRTUAL, |frame, index| self.read(frame, index))
        }
    }

    #[test]
    fn an_index_is_the_matching_run_of_nine_bits() {
        // The canonical example from the manuals: PML4 0, PDPT 0, PD 0, PT 1 for 0x1000.
        assert_eq!(index(0x0, Level::Pml4), 0);
        assert_eq!(index(0x1000, Level::Pt), 1);
        assert_eq!(index(0x200000, Level::Pd), 1);
        assert_eq!(index(0x4000_0000, Level::Pdpt), 1);
        assert_eq!(index(0x80_0000_0000, Level::Pml4), 1);
    }

    #[test]
    fn a_high_address_splits_the_way_the_manuals_show() {
        // 0x0000_7fff_ffff_f000 is the last 4 KiB page of the lower canonical half.
        let top = 0x0000_7fff_ffff_f000;
        assert_eq!(index(top, Level::Pml4), 255);
        assert_eq!(index(top, Level::Pdpt), 511);
        assert_eq!(index(top, Level::Pd), 511);
        assert_eq!(index(top, Level::Pt), 511);
        // An index never leaves 0..512, whatever the address: the bits above the level's
        // nine are dropped, not shifted in.
        for address in [0u64, 1, 0xdead_beef_0000, u64::MAX] {
            for level in Level::ALL {
                assert!(index(address, level) < ENTRIES);
            }
        }
    }

    #[test]
    fn flags_and_addresses_do_not_bleed_into_each_other() {
        let flags = Entry::PRESENT | Entry::WRITABLE | Entry::NO_EXECUTE;
        let entry = Entry::pointing_at(L1 | 0xfff, flags);
        assert_eq!(
            entry.address(),
            L1,
            "the flag bits are not part of the address"
        );
        assert!(entry.is_present() && entry.is_writable() && !entry.is_executable());
        assert_eq!(entry.bits() & !Entry::ADDRESS_MASK, flags);
        assert_eq!(Entry::from_bits(entry.bits()), entry);
    }

    #[test]
    fn an_empty_entry_is_absent_and_maps_nothing() {
        let entry = Entry::empty();
        assert!(!entry.is_present());
        assert_eq!(entry.address(), 0);
        assert_eq!(entry.bits(), 0);
        assert_eq!(translate(0, VIRTUAL, |_, _| Entry::empty()), None);
    }

    #[test]
    fn a_walk_reports_the_leaf_frame_and_the_offset_inside_it() {
        let fake = Fake::page(0x8000_0000);
        let found = fake.translate().expect("the address is mapped");
        assert_eq!(found.size, PAGE_BYTES);
        assert_eq!(found.address, 0x8000_0000 + (VIRTUAL & 0xfff));

        // A different address in the same page lands on the same frame plus its offset, and
        // one page on lands in the next entry, which is empty.
        let same_page = translate(L4, VIRTUAL + 0x10, |frame, index| fake.read(frame, index))
            .expect("still mapped");
        assert_eq!(same_page.address, found.address + 0x10);
        assert_eq!(
            translate(L4, VIRTUAL + PAGE_BYTES, |frame, index| fake
                .read(frame, index)),
            None
        );
    }

    #[test]
    fn a_missing_entry_at_any_level_is_no_mapping() {
        let mut fake = Fake::page(0x8000_0000);
        assert!(fake.translate().is_some());
        for level in Level::ALL {
            let entry = index(VIRTUAL, level);
            let saved = core::mem::replace(&mut fake.table_mut(level)[entry], Entry::empty());
            assert_eq!(fake.translate(), None, "{level:?} should stop the walk");
            fake.table_mut(level)[entry] = saved;
        }
    }

    #[test]
    fn a_huge_entry_stops_the_walk_and_keeps_the_offset() {
        // A 2 MiB page at the PD level, which is how the bootloader maps the physical
        // window. The PT entry is cleared: a walk that descended would find nothing.
        let mut fake = Fake::page(0x8000_0000);
        fake.table_mut(Level::Pt).fill(Entry::empty());
        fake.table_mut(Level::Pd)[index(VIRTUAL, Level::Pd)] = Entry::pointing_at(
            0x4000_0000,
            Entry::PRESENT | Entry::WRITABLE | Entry::HUGE | Entry::NO_EXECUTE,
        );
        let found = fake.translate().expect("the huge page is mapped");
        assert_eq!(found.size, 1 << 21);
        assert_eq!(found.address, 0x4000_0000 + (VIRTUAL & 0x1f_ffff));

        // The last byte of the 2 MiB page is still inside it, not one page along.
        let last_byte = (VIRTUAL & !0x1f_ffff) | 0x1f_ffff;
        let last = translate(L4, last_byte, |frame, index| fake.read(frame, index))
            .expect("the end of the huge page is mapped");
        assert_eq!(last.address, 0x401f_ffff);
        assert_eq!(last.size, 1 << 21);
    }

    #[test]
    fn a_one_gibibyte_page_is_translated_too() {
        let mut fake = Fake::page(0x8000_0000);
        fake.table_mut(Level::Pdpt)[index(VIRTUAL, Level::Pdpt)] = Entry::pointing_at(
            0x1_0000_0000,
            Entry::PRESENT | Entry::WRITABLE | Entry::HUGE,
        );
        let found = fake.translate().expect("the 1 GiB page is mapped");
        assert_eq!(found.size, 1 << 30);
        assert_eq!(found.address, 0x1_0000_0000 + (VIRTUAL & 0x3fff_ffff));
    }

    /// Tables in a fixed array of frames, for the tests that map into a tree with no levels
    /// below the root.
    ///
    /// [`Fake`] cannot serve here: a mapping *creates* levels, so the double has to hand out
    /// frames and keep what is written into them. `N` is how many frames the pool holds, which
    /// is how a test runs one dry.
    struct Pool<const N: usize> {
        frames: [[Entry; ENTRIES]; N],
        next: usize,
    }

    impl<const N: usize> Pool<N> {
        /// The pretend physical address of the pool's first frame, which is the root.
        const BASE: u64 = 0x10_0000;

        /// An empty root in frame zero. Frame zero is out of the allocator for the same reason
        /// the real tree's root is: the tree has to exist before anything is handed out.
        fn new() -> Self {
            Self {
                frames: [[Entry::empty(); ENTRIES]; N],
                next: 1,
            }
        }

        /// The address of the pool's `index`-th frame.
        fn frame(index: usize) -> u64 {
            Self::BASE + index as u64 * PAGE_BYTES
        }

        /// How many frames the pool has handed out, the root included.
        fn used(&self) -> usize {
            self.next
        }

        /// Which array `frame` names, if it is one of the pool's.
        fn slot(&self, frame: u64) -> Option<usize> {
            let index = frame.checked_sub(Self::BASE)? / PAGE_BYTES;
            (index < N as u64).then_some(index as usize)
        }
    }

    impl<const N: usize> Tables for Pool<N> {
        fn read(&self, frame: u64, index: usize) -> Entry {
            self.slot(frame)
                .map_or_else(Entry::empty, |slot| self.frames[slot][index])
        }

        fn write(&mut self, frame: u64, index: usize, entry: Entry) {
            if let Some(slot) = self.slot(frame) {
                self.frames[slot][index] = entry;
            }
        }

        fn zero(&mut self, frame: u64) {
            if let Some(slot) = self.slot(frame) {
                self.frames[slot] = [Entry::empty(); ENTRIES];
            }
        }

        fn alloc(&mut self) -> Option<u64> {
            if self.next == N {
                return None;
            }
            let frame = Self::frame(self.next);
            self.next += 1;
            Some(frame)
        }
    }

    /// A fresh virtual address for the mapping tests: nothing else in this file maps it, and
    /// every one of its four indices is non-zero, so a mapping that mixed two levels up shows.
    const FRESH: u64 = 0x0000_6000_0000_0000;

    /// The frame the mapping tests hand out.
    const FRAME: u64 = 0x9000;

    #[test]
    fn a_mapping_from_an_empty_root_creates_the_levels_it_needs() {
        let mut pool = Pool::<8>::new();
        let root = Pool::<8>::frame(0);
        let mapped = map(
            &mut pool,
            root,
            FRESH,
            FRAME,
            Entry::WRITABLE | Entry::NO_EXECUTE,
        );
        assert_eq!(mapped, Ok(()));
        // The root, plus one table each for the three levels the address needed.
        assert_eq!(pool.used(), 4);
        let found = translate(root, FRESH, |frame, index| pool.read(frame, index))
            .expect("the address is mapped now");
        assert_eq!(found.address, FRAME);
        assert_eq!(found.size, PAGE_BYTES);
        // The leaf carries what the caller asked for and nothing else.
        let leaf = pool.read(Pool::<8>::frame(3), index(FRESH, Level::Pt));
        assert!(leaf.is_present() && leaf.is_writable() && !leaf.is_executable());
        assert_eq!(leaf.address(), FRAME);
    }

    #[test]
    fn a_second_address_in_the_same_two_mib_page_reuses_the_levels_above_it() {
        let mut pool = Pool::<8>::new();
        let root = Pool::<8>::frame(0);
        map(&mut pool, root, FRESH, FRAME, Entry::WRITABLE).expect("the first page");
        assert_eq!(pool.used(), 4);
        // The neighbour is a different PT entry inside the same PT, so it costs no frame.
        let next = map(
            &mut pool,
            root,
            FRESH + PAGE_BYTES,
            FRAME + PAGE_BYTES,
            Entry::WRITABLE,
        );
        assert_eq!(next, Ok(()));
        assert_eq!(pool.used(), 4);
        // One 2 MiB page along is a different PD entry, so it needs a table of its own.
        let two_mib = map(
            &mut pool,
            root,
            FRESH + (1 << 21),
            FRAME + PAGE_BYTES * 2,
            Entry::WRITABLE,
        );
        assert_eq!(two_mib, Ok(()));
        assert_eq!(pool.used(), 5);
    }

    #[test]
    fn a_second_mapping_at_the_same_address_is_refused() {
        let mut pool = Pool::<8>::new();
        let root = Pool::<8>::frame(0);
        map(&mut pool, root, FRESH, FRAME, Entry::WRITABLE).expect("the first mapping");
        let used = pool.used();
        let second = map(&mut pool, root, FRESH, FRAME + PAGE_BYTES, Entry::WRITABLE);
        assert_eq!(second, Err(MapError::AlreadyMapped));
        assert_eq!(pool.used(), used, "a refused mapping allocates nothing");
        let found = translate(root, FRESH, |frame, index| pool.read(frame, index))
            .expect("the first mapping is still there");
        assert_eq!(found.address, FRAME);
    }

    #[test]
    fn a_misaligned_address_or_frame_is_refused() {
        let mut pool = Pool::<8>::new();
        let root = Pool::<8>::frame(0);
        let address = map(&mut pool, root, FRESH + 0x800, FRAME, Entry::WRITABLE);
        assert_eq!(address, Err(MapError::Misaligned));
        let frame = map(&mut pool, root, FRESH, FRAME + 0x1ff, Entry::WRITABLE);
        assert_eq!(frame, Err(MapError::Misaligned));
        assert_eq!(pool.used(), 1, "nothing was created for either attempt");
    }

    #[test]
    fn a_huge_page_counts_as_already_mapped() {
        for level in [Level::Pdpt, Level::Pd] {
            let mut pool = Pool::<8>::new();
            let root = Pool::<8>::frame(0);
            // The levels above the huge entry have to exist before one can sit in them, which
            // is the same walk a mapping does, stopping one level short.
            let mut table = root;
            for above in Level::ALL {
                if above == level {
                    break;
                }
                let next = pool.alloc().expect("a frame for the level above");
                let entry = Entry::pointing_at(next, Entry::PRESENT | Entry::WRITABLE);
                pool.write(table, index(FRESH, above), entry);
                table = next;
            }
            let flags = Entry::PRESENT | Entry::WRITABLE | Entry::HUGE;
            pool.write(
                table,
                index(FRESH, level),
                Entry::pointing_at(0x4000_0000, flags),
            );
            let used = pool.used();
            let mapped = map(&mut pool, root, FRESH, FRAME, Entry::WRITABLE);
            assert_eq!(mapped, Err(MapError::AlreadyMapped), "{level:?}");
            assert_eq!(pool.used(), used, "{level:?} needs no table of its own");
        }
    }

    #[test]
    fn a_mapping_that_runs_out_of_frames_leaves_no_leaf_behind() {
        // Two frames: the root and one table, where the address needs three.
        let mut pool = Pool::<2>::new();
        let root = Pool::<2>::frame(0);
        let mapped = map(&mut pool, root, FRESH, FRAME, Entry::WRITABLE);
        assert_eq!(mapped, Err(MapError::NoFrame));
        // The one table it did create stays behind, empty, and the address is still unmapped.
        assert_eq!(pool.used(), 2);
        let found = translate(root, FRESH, |frame, index| pool.read(frame, index));
        assert_eq!(found, None);
    }

    #[test]
    fn the_levels_a_mapping_creates_leave_the_flags_to_the_leaf() {
        let mut pool = Pool::<8>::new();
        let root = Pool::<8>::frame(0);
        map(
            &mut pool,
            root,
            FRESH,
            FRAME,
            Entry::WRITABLE | Entry::NO_EXECUTE,
        )
        .expect("the mapping");
        // The three entries on the way down, in the order the walk asked for them. Each is
        // writable, because a write needs `WRITABLE` at every level, and executable, because
        // `NO_EXECUTE` here would deny the leaf a permission it never gave away.
        let chain = [
            (root, Level::Pml4),
            (Pool::<8>::frame(1), Level::Pdpt),
            (Pool::<8>::frame(2), Level::Pd),
        ];
        for (table, level) in chain {
            let entry = pool.read(table, index(FRESH, level));
            assert!(entry.is_present(), "{level:?}");
            assert!(entry.is_writable(), "{level:?}");
            assert!(entry.is_executable(), "{level:?}");
            assert!(!entry.is_huge(), "{level:?}");
        }
    }
}
