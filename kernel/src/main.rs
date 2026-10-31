#![no_std]
#![no_main]

// `Box`/`Vec` live here. The kernel has no `std`; `alloc` is the half of the standard
// library that only needs a `#[global_allocator]`, which `KernelHeap` below provides.
extern crate alloc;

use bootloader_api::config::Mapping;
use bootloader_api::info::MemoryRegionKind;
use bootloader_api::{entry_point, BootInfo, BootloaderConfig};
use core::alloc::{GlobalAlloc, Layout};
use core::cell::UnsafeCell;
use core::panic::PanicInfo;
use core::sync::atomic::{AtomicBool, Ordering};
use grey_os::{
    arena, display, frame, framebuffer, gdt, halt_loop, heap, idt, memory, paging, pic, qemu,
    serial, timer,
};

/// Where the arena's virtual span starts. Above the bootloader's physical window, which ends at
/// `offset + 1 TiB`, and below the addresses the mapping probes use, so no address this boot
/// gives out is one something else already owns (ADR-031).
const ARENA_BASE: usize = 0x5000_0000_0000;

/// Bytes the span reserves: 64 MiB. Large enough for every allocation this boot makes with room
/// to spare, small enough that a runaway allocation runs out instead of eating the address space
/// a user-space layout will want (ADR-031).
const ARENA_BYTES: usize = 64 * 1024 * 1024;

/// The sizes the churn probe walks: every class the arena has, one request larger than a page, and
/// a repeat at the end. Sixteen or fewer, because `arena::churn` holds them on the stack.
const CHURN_SIZES: [usize; 8] = [16, 64, 256, 1024, 4096, 8192, 64, 512];

/// The rate the PIT is programmed at. 100 Hz is the usual compromise: fine enough for a
/// scheduler tick later, coarse enough that the handler's cost does not matter yet.
const TIMER_HZ: u32 = 100;

/// Ticks the boot log waits for. Ten at [`TIMER_HZ`] is a tenth of a second, which proves the
/// counter moves without making every boot feel slow under emulation.
const OBSERVED_TICKS: u64 = 10;

/// How many frames the boot probe takes at once: enough for a bitmap that hands the same frame
/// out twice to show it, small enough to stay out of the way.
const PROBE_FRAMES: usize = 8;

/// The address the mapping probe gives its frame: nothing maps it, and after the probe nothing
/// will again.
///
/// Canonical, 96 TiB above zero - well clear of the physical window's 2.5 TiB and of everything
/// the kernel lives in - and in a top-level slot (`index(0x6000_0000_0000, Pml4)` is 192) the
/// bootloader has no reason to have filled. The probe walks it before mapping it, so a machine
/// where that is wrong skips the probe instead of failing the boot.
const MAPPING_PROBE: u64 = 0x0000_6000_0000_0000;

/// An address nothing maps, for the `#PF` injection below.
///
/// Canonical (below the 47-bit sign extension, so the CPU raises `#PF` and not `#GP`), and in
/// a top-level table slot the bootloader has no reason to have filled: the kernel sits in the
/// low canonical range and the physical memory window is mapped high.
#[cfg(grey_os_fault_pf)]
const UNMAPPED_PROBE: usize = 0x0000_7000_0000_0000;

// Faults twice on purpose, for the `df` phase: it moves `RSP` to an address nothing maps and
// then pushes, so the `#PF` the push raises cannot push its own frame either and the CPU
// escalates to `#DF` - which the TSS routes onto its own stack.
//
// Defined as an assembly stub because Rust cannot hand the compiler a stack pointer it does not
// own, and only for the phase that uses it: an unused global symbol has no business in the
// image.
//
// A software `int 8` would reach the same vector but not the same frame: an `int n` pushes no
// error code even for a vector whose hardware fault does, so the handler installed for `#DF`
// would read the instruction pointer as its error code.
#[cfg(grey_os_fault_df)]
core::arch::global_asm!(
    ".pushsection .text,\"ax\",@progbits",
    ".global grey_os_double_fault_probe",
    ".type grey_os_double_fault_probe, @function",
    "grey_os_double_fault_probe:",
    // Canonical, and nothing maps it - the same shape as `UNMAPPED_PROBE`.
    "movabs rsp, 0x700000000000",
    "push rax",
    ".size grey_os_double_fault_probe, . - grey_os_double_fault_probe",
    ".popsection",
);

#[cfg(grey_os_fault_df)]
extern "C" {
    // Does not return, though nothing here says so: a diverging signature would make every
    // line after the call unreachable, and the compiler is right to warn about that.
    fn grey_os_double_fault_probe();
}

/// Prints one line to the serial console and, once `display::init` has run, to the panel.
///
/// Serial first: it is the channel that keeps working when the screen does not.
macro_rules! println {
    () => {{
        grey_os::serial::print_args(format_args!(""));
        grey_os::display::print_args(format_args!(""));
    }};
    ($($arg:tt)*) => {{
        grey_os::serial::print_args(format_args!($($arg)*));
        grey_os::display::print_args(format_args!($($arg)*));
    }};
}

/// The bootstrap allocator: the only memory the kernel has before the frame allocator - whose own
/// bitmap is an allocation - can hand out a frame to map. ADR-021's bump allocator, kept for that
/// one job (ADR-031).
static BOOTSTRAP: heap::BumpAllocator = heap::BumpAllocator::new();

/// The arena every allocation after the address-space switch comes from (ADR-031).
static HEAP: arena::Heap<KernelPages> = arena::Heap::new(KernelPages::new());

/// The kernel's `#[global_allocator]`: the bootstrap until the arena is ready, the arena after.
/// It is initialised in `kernel_main` once the memory map has been read, so an allocation before
/// that point returns null and fails loudly instead of writing somewhere unowned.
struct KernelHeap;

// SAFETY: `alloc` returns null or a block from whichever allocator is in charge, aligned and large
// enough for the layout; `dealloc` hands a block back only to the allocator that handed it out,
// and the bootstrap's blocks are never reclaimed, which is what a bump allocator does anyway
// (ADR-021, ADR-031).
unsafe impl GlobalAlloc for KernelHeap {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: the caller of `GlobalAlloc::alloc` makes the promise both allocators need.
        unsafe {
            if HEAP.ready() {
                HEAP.alloc(layout)
            } else {
                BOOTSTRAP.alloc(layout)
            }
        }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        if in_bootstrap(ptr) {
            // Boot objects live as long as the kernel does: the console, its back buffer, the
            // frame allocator's bitmap. A bump allocator could not take one back in any order
            // other than the reverse of allocation anyway.
            return;
        }
        // SAFETY: as `alloc`; a pointer that is not the arena's is counted, not followed.
        unsafe { HEAP.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        if in_bootstrap(ptr) {
            // A boot block that wants to grow gets an arena block and a copy; the old one stays
            // where it is, which is what the bootstrap does with everything.
            // SAFETY: the caller promises the old block is live for `layout.size()` bytes and
            // that `new_size` with this alignment is a valid layout.
            unsafe {
                let bigger = Layout::from_size_align_unchecked(new_size, layout.align());
                let fresh = KernelHeap.alloc(bigger);
                if !fresh.is_null() {
                    core::ptr::copy_nonoverlapping(ptr, fresh, layout.size());
                }
                fresh
            }
        } else {
            // SAFETY: as above, for the arena.
            unsafe { HEAP.realloc(ptr, layout, new_size) }
        }
    }
}

#[global_allocator]
static ALLOCATOR: KernelHeap = KernelHeap;

/// Whether `ptr` came from the bootstrap's reserve rather than the arena's.
fn in_bootstrap(ptr: *mut u8) -> bool {
    let stats = BOOTSTRAP.stats();
    let address = ptr as usize;
    stats.base != 0 && stats.base <= address && address < stats.end
}

/// The arena's pages as the kernel can give them out: a frame from the frame allocator, mapped at
/// the arena's address through the table the CPU is running on (ADR-030).
struct KernelPages {
    /// The tree mappings are written into, and the window every frame is reached through.
    /// `UnsafeCell` because `GlobalAlloc` hands the allocator `&self` while mapping changes what
    /// the addresses in these cells name.
    root: UnsafeCell<u64>,
    offset: UnsafeCell<u64>,
}

impl KernelPages {
    const fn new() -> Self {
        Self {
            root: UnsafeCell::new(0),
            offset: UnsafeCell::new(0),
        }
    }

    /// Points the page source at the live tree and the physical window. Called once, after the
    /// kernel has switched to its own tables.
    ///
    /// # Safety
    ///
    /// `root` must be the tree `CR3` names, `offset` the window that reaches all of physical
    /// memory, and both must stay valid for the rest of the boot.
    unsafe fn install(&self, root: u64, offset: u64) {
        // SAFETY: `self` is a valid reference; nothing else writes these cells, and `install`
        // runs before the arena is handed its first page.
        unsafe {
            *self.root.get() = root;
            *self.offset.get() = offset;
        }
    }
}

impl arena::Pages for KernelPages {
    fn map(&mut self, address: usize) -> Option<u64> {
        // SAFETY: `install` ran before the arena was handed any page, and nothing else writes
        // these cells.
        let (root, offset) = unsafe { (*self.root.get(), *self.offset.get()) };
        let frame = frame::FRAMES.alloc()?;
        // A page the kernel writes to and never executes from.
        let flags = paging::Entry::WRITABLE | paging::Entry::NO_EXECUTE;
        // SAFETY: `root` is the live tree, `offset` the window, `frame` came from the allocator,
        // and `address` is inside the arena's reserved span, which nothing else maps.
        match unsafe {
            paging::map_4k(root, offset, address as u64, frame, flags, || {
                frame::FRAMES.alloc()
            })
        } {
            Ok(()) => Some(frame),
            Err(_) => {
                // The frame never got an address, so it is free again.
                frame::FRAMES.free(frame);
                None
            }
        }
    }

    fn unmap(&mut self, address: usize) -> bool {
        // SAFETY: as `map`.
        let (root, offset) = unsafe { (*self.root.get(), *self.offset.get()) };
        // The frame is read before the entry goes: `unmap_4k` takes a mapping away without saying
        // what it was, which is why the frame is looked up here instead (ADR-031).
        // SAFETY: `root` is the live tree and `offset` the window.
        let Some(translation) = (unsafe { paging::walk(root, offset, address as u64) }) else {
            return false;
        };
        // SAFETY: as above; the address is a 4 KiB mapping the arena grew, and the closure hands
        // out only frames the bitmap says are free, should a table need splitting.
        if unsafe { paging::unmap_4k(root, offset, address as u64, || frame::FRAMES.alloc()) }
            .is_err()
        {
            return false;
        }
        frame::FRAMES.free(translation.address & !(memory::PAGE_BYTES - 1))
    }
}

/// Asks the bootloader for a window onto all of physical memory (ADR-022).
///
/// `memory_regions` is a list of *physical* ranges, so without this window the kernel
/// has no way to touch the memory it has been told it owns: the only addresses it can
/// dereference are the ones the bootloader chose for the kernel, the boot info and the
/// framebuffer. `Mapping::Dynamic` lets the bootloader pick an unused 2 MiB-aligned
/// address and publish it in `BootInfo::physical_memory_offset`, so no address is
/// hardcoded here.
pub static BOOTLOADER_CONFIG: BootloaderConfig = {
    let mut config = BootloaderConfig::new_default();
    // `new_default` deliberately maps no physical memory; the heap needs the opposite.
    config.mappings.physical_memory = Some(Mapping::Dynamic);
    config
};

entry_point!(kernel_main, config = &BOOTLOADER_CONFIG);

fn kernel_main(boot_info: &'static mut BootInfo) -> ! {
    serial::init();
    println!("grey-os: serial console online (COM1, 115200 8N1)");

    // The CPU tables come before anything that can fault on them. From here a bad pointer is
    // a report instead of a triple fault; until here the bootloader's tables still apply, so
    // the window where a fault is undiagnosable is as small as this boot can make it. Their
    // log lines are deferred to the reported section below, like every other discovery
    // (ADR-028).
    gdt::init();
    idt::init();

    // The window onto all of physical memory (ADR-022). Read here rather than where it is
    // first used, because two things need it before the heap exists: the page-table walk
    // below, and the conversion of the heap's physical span to a virtual one.
    let offset = match boot_info.physical_memory_offset.as_mut() {
        Some(offset) => *offset,
        None => {
            println!(
                "grey-os: FATAL: boot info carries no physical memory offset, so nothing outside the kernel is reachable"
            );
            qemu::exit_qemu(qemu::QemuExitCode::Failed);
        }
    };

    // Where the framebuffer physically lives. It is ordinary RAM that firmware happens to
    // display from, so the map below may well offer its pages as usable; a heap that took them
    // would paint over the screen at some later moment.
    //
    // `bootloader_api` 0.11 publishes only the virtual address the bootloader picked, never
    // the frames behind it, so the address has to be translated by walking the tables `CR3`
    // names - M3's first job for `paging`, and the one that needs no allocator (ADR-029).
    let Some(screen) = boot_info.framebuffer.as_mut() else {
        // Failing loudly beats booting into a black screen nobody can diagnose.
        println!("grey-os: FATAL: boot info carries no framebuffer; GOP is required");
        qemu::exit_qemu(qemu::QemuExitCode::Failed);
    };
    let screen_info = screen.info();
    let inherited_root = paging::root();
    let screen_start = screen.buffer().as_ptr() as u64;
    // SAFETY: `inherited_root` is the tree `CR3` names, and `offset` is the window the
    // bootloader mapped all of physical memory at, so every table in it is readable.
    let found = unsafe {
        paging::span(
            inherited_root,
            offset,
            screen_start,
            screen_info.byte_len as u64,
        )
    };
    let Some(framebuffer_span) = found else {
        println!(
            "grey-os: FATAL: {} bytes of framebuffer at {screen_start:#x} are not contiguous physical memory",
            screen_info.byte_len
        );
        qemu::exit_qemu(qemu::QemuExitCode::Failed);
    };

    // Discovery runs before reporting: the console lives on the heap and draws into the
    // framebuffer, so neither exists yet. Deferring the first log line about the map is
    // what puts the whole boot log on the panel instead of only its tail.
    let mut usable_bytes: u64 = 0;
    for region in boot_info.memory_regions.iter() {
        if region.kind == MemoryRegionKind::Usable {
            usable_bytes =
                usable_bytes.saturating_add(memory::Span::new(region.start, region.end).len());
        }
    }

    let heap_span = memory::pick_heap_region(
        boot_info
            .memory_regions
            .iter()
            .map(|region| memory::Region {
                start: region.start,
                end: region.end,
                usable: region.kind == MemoryRegionKind::Usable,
            }),
        Some(framebuffer_span),
    );
    let Some(span) = heap_span else {
        println!("grey-os: FATAL: no usable memory region to draw a heap from");
        qemu::exit_qemu(qemu::QemuExitCode::Failed);
    };
    // The region is the bootstrap's reserve and nothing more: ADR-021's bump allocator takes the
    // frame allocator's bitmap and the console from here, because neither can come from the arena
    // before a frame exists to map. What the bootstrap does not use goes back to the frame
    // allocator once the arena is up, which is what stops this region being the 420 MiB guess M1
    // made it (ADR-031).
    let boot = span;

    // The span picked above is physical memory that firmware happens to map somewhere of
    // its choosing. Convert it through `physical_memory_offset` before the allocator sees
    // it: handing `GlobalAlloc` a physical address writes to whatever is mapped at that
    // virtual address, which is silent corruption rather than a failed boot.
    let range = {
        let start = offset.checked_add(boot.start);
        let end = offset.checked_add(boot.end);
        start.zip(end).and_then(|(start, end)| {
            Some((usize::try_from(start).ok()?, usize::try_from(end).ok()?))
        })
    };
    let Some((start, end)) = range else {
        println!(
            "grey-os: FATAL: bootstrap region {:#x}..{:#x} does not fit past the {offset:#x} mapping",
            boot.start, boot.end
        );
        qemu::exit_qemu(qemu::QemuExitCode::Failed);
    };
    // SAFETY: `start..end` is a region the firmware reports as usable, nothing else writes
    // there, the bootloader maps all of physical memory writable at
    // `physical_memory_offset` (ADR-022), and this is the only call to `init`.
    if !unsafe { BOOTSTRAP.init(start, end) } {
        println!("grey-os: FATAL: bootstrap range {start:#x}..{end:#x} is not usable");
        qemu::exit_qemu(qemu::QemuExitCode::Failed);
    }

    // A heap is only worth anything if the memory behind it is really mapped at the
    // virtual address the allocator returns. Write a whole page through a `Box` and read
    // it back: a wrong `physical_memory_offset` faults here, at boot, instead of in a
    // driver later. Every later allocation - the console's grid, its back buffer, the frame
    // allocator's bitmap - rides on this one having worked.
    let mut page = alloc::boxed::Box::new([0u8; 4096]);
    for (index, byte) in page.iter_mut().enumerate() {
        *byte = index as u8;
    }
    if page
        .iter()
        .enumerate()
        .any(|(index, byte)| *byte != index as u8)
    {
        println!("grey-os: FATAL: heap memory did not read back what was written");
        qemu::exit_qemu(qemu::QemuExitCode::Failed);
    }
    let probed = BOOTSTRAP.stats();
    // UEFI has no VGA text mode, so we draw to the framebuffer instead. This is the only
    // display path: GOP from boot information, no GPU drivers. `screen` was taken above,
    // where its physical span had to be walked; the layout is what gets checked here.
    let info = screen_info;
    if !framebuffer::fill(screen, framebuffer::BACKGROUND) {
        // Say what the firmware offered: under BIOS a VBE mode can be 24-bit or palettised,
        // and "unsupported" alone leaves the next person guessing which.
        println!("grey-os: FATAL: framebuffer layout unsupported");
        println!(
            "grey-os: framebuffer {:?}, {} bytes/pixel, stride {}, {}x{}",
            info.pixel_format, info.bytes_per_pixel, info.stride, info.width, info.height
        );
        qemu::exit_qemu(qemu::QemuExitCode::Failed);
    }
    // The walk's claim, checked rather than believed: write one byte through the framebuffer's
    // virtual pointer, then read the same offset through the physical window at the frame the
    // walk named. Nothing else ties those two addresses to the same memory - one came from the
    // bootloader, the other from our arithmetic - so this is what makes the span a fact. The
    // byte sits in the middle of the surface, where a wrong frame is far more likely to differ
    // than to coincide; the console paints over it a few lines later.
    let probe_byte = info.byte_len / 2;
    let probe_value = 0x5a;
    screen.buffer_mut()[probe_byte] = probe_value;
    // SAFETY: `framebuffer_span` came from the walk, `offset` maps all of physical memory, and
    // `probe_byte` is inside the span's length by construction.
    let through_frame = unsafe {
        core::ptr::read_volatile((offset + framebuffer_span.start + probe_byte as u64) as *const u8)
    };
    if through_frame != probe_value {
        println!(
            "grey-os: FATAL: the framebuffer's virtual and physical views disagree at byte {probe_byte}"
        );
        qemu::exit_qemu(qemu::QemuExitCode::Failed);
    }

    framebuffer::install(screen);
    // The console needs the heap above for its grid and the framebuffer for its pixels.
    if !display::init() {
        println!("grey-os: FATAL: panel has no room for one character cell");
        qemu::exit_qemu(qemu::QemuExitCode::Failed);
    }

    // From here every line goes to serial and to the panel. Serial first in each macro
    // call, because it is the channel that survives a screen going dark later.
    for region in boot_info.memory_regions.iter() {
        let span = memory::Span::new(region.start, region.end);
        let kind = match region.kind {
            MemoryRegionKind::Usable => "usable",
            MemoryRegionKind::Bootloader => "bootloader",
            // `MemoryRegionKind` is `#[non_exhaustive]`: a kind a newer firmware reports is
            // not usable here, and "reserved" is the safe reading.
            _ => "reserved",
        };
        println!(
            "grey-os: memory {:#012x}-{:#012x} {:>7} KiB {}",
            span.start,
            span.end,
            span.len() / 1024,
            kind
        );
    }
    println!("grey-os: memory usable total {} KiB", usable_bytes / 1024);
    println!(
        "grey-os: heap bootstrap {:#012x}-{:#012x} ({} KiB region, {} KiB used)",
        boot.start,
        boot.end,
        boot.len() / 1024,
        (BOOTSTRAP
            .stats()
            .cursor
            .saturating_sub(BOOTSTRAP.stats().base))
            / 1024
    );
    println!(
        "grey-os: heap probe ok: 4 KiB read back, {} allocations, {} bytes in use",
        probed.allocations, probed.bytes_in_use
    );
    println!(
        "grey-os: framebuffer {}x{}, stride {}, {} bytes/pixel",
        info.width, info.height, info.stride, info.bytes_per_pixel
    );
    if let Some((columns, rows)) = display::grid() {
        println!(
            "grey-os: display adapter online, {columns}x{rows} cells at scale {}",
            display::SCALE
        );
        match display::back_buffer_bytes() {
            // The console draws into this and copies the rows that changed onto the panel,
            // which is what stops a line from being painted glyph by glyph on the panel.
            Some(bytes) => println!(
                "grey-os: console draws into a {} KiB back buffer",
                bytes / 1024
            ),
            None => println!("grey-os: console draws straight to the panel, no back buffer"),
        }
    }

    // The frame allocator (ADR-029). Its bitmap comes from the bootstrap, which is why this runs
    // after `BOOTSTRAP.init` and before the arena exists, and it is never freed: every later
    // allocation asks this bitmap
    // first. The index covers the physical address space up to the highest frame that could
    // be handed out or must be kept out of circulation - usable RAM, the framebuffer and the
    // kernel image - so a frame number is just an address divided by the page size, and the
    // reserved MMIO ranges above that are simply not described.
    let highest = boot_info
        .memory_regions
        .iter()
        .filter(|region| region.kind == MemoryRegionKind::Usable)
        .map(|region| region.end)
        .chain([
            framebuffer_span.end,
            boot_info.kernel_addr.saturating_add(boot_info.kernel_len),
        ])
        .max()
        .unwrap_or(0);
    let frames = highest.div_ceil(memory::PAGE_BYTES) as usize;
    let words = frames.div_ceil(usize::BITS as usize);
    // Leaked on purpose: the bitmap is the allocator's index and has to outlive every frame
    // it hands out, which is the rest of the kernel's life.
    let bitmap: &'static mut [u64] =
        alloc::boxed::Box::leak(alloc::vec![0u64; words].into_boxed_slice());
    // SAFETY: `bitmap` is a live slice on the heap that nothing else writes, `frames` fits in
    // it, and this is the only call to `init`.
    if !unsafe { frame::FRAMES.init(bitmap.as_mut_ptr(), bitmap.len(), frames) } {
        println!("grey-os: FATAL: the frame allocator refused its bitmap");
        qemu::exit_qemu(qemu::QemuExitCode::Failed);
    }

    // Only frames firmware calls usable may be handed out, and the kernel's own memory comes
    // back out of them first. The bootloader already marks what it allocated as not usable,
    // which is what keeps the boot info and the inherited tables out of reach; what it does
    // not promise is the kernel image, the heap and the framebuffer, so each is reserved by
    // span here.
    for region in boot_info.memory_regions.iter() {
        if region.kind == MemoryRegionKind::Usable {
            frame::FRAMES.mark_free(memory::Span::new(region.start, region.end));
        }
    }
    frame::FRAMES.mark_used(memory::Span::new(
        boot_info.kernel_addr,
        boot_info.kernel_addr.saturating_add(boot_info.kernel_len),
    ));
    // Frame zero is never handed out. The map may call the real-mode low memory usable - it is
    // history, not free space - and a frame at address 0 is also what a null pointer would
    // reach through the physical window.
    frame::FRAMES.mark_used(memory::Span::new(0, memory::PAGE_BYTES));
    // The bootstrap's used bytes, not its whole region: the rest of it is memory the frame
    // allocator may hand out from now on, which is the 400-and-something MiB M1 reserved for a
    // bump cursor that never got near it (ADR-031). The cursor is frozen from here: once the
    // arena is ready every allocation goes there, so nothing can take another byte of this span
    // after the line below reserves it.
    let bootstrap_used = (BOOTSTRAP
        .stats()
        .cursor
        .saturating_sub(BOOTSTRAP.stats().base)) as u64;
    frame::FRAMES.mark_used(memory::Span::new(
        boot.start,
        boot.start
            .saturating_add(bootstrap_used.div_ceil(memory::PAGE_BYTES) * memory::PAGE_BYTES),
    ));
    frame::FRAMES.mark_used(framebuffer_span);

    let frame_stats = frame::FRAMES
        .stats()
        .expect("the frame allocator was just initialised");
    println!(
        "grey-os: frames: {} total, {} free, {} KiB bitmap",
        frame_stats.frames,
        frame_stats.free,
        frame_stats.bitmap_bytes / 1024
    );

    // The frame allocator's half of the heap's 4 KiB probe: frames it hands out have to be
    // distinct, writable through the window, and read back what was written.
    let Some(probe) = probe_frames(offset) else {
        println!("grey-os: FATAL: the frame probe did not read back what it wrote");
        qemu::exit_qemu(qemu::QemuExitCode::Failed);
    };
    println!(
        "grey-os: frame probe ok: {PROBE_FRAMES} frames from {probe:#x} written and read back"
    );

    // The kernel's own tables: a deep copy of the tree `CR3` currently names, so every mapping
    // the bootloader made survives untouched - the kernel, its stack, the boot info, the
    // physical window, the framebuffer - while the frames behind it come from the allocator.
    // From the `mov cr3` on, the kernel owns the address space and can edit it (ADR-029).
    let mut table_frames = 0usize;
    // SAFETY: `inherited_root` is the live tree, and the closure hands out only frames the
    // bitmap says are free.
    let own_root = unsafe {
        paging::clone_root(inherited_root, offset, || {
            let frame = frame::FRAMES.alloc()?;
            table_frames += 1;
            Some(frame)
        })
    };
    let Some(own_root) = own_root else {
        println!("grey-os: FATAL: the page-table copy ran out of frames");
        qemu::exit_qemu(qemu::QemuExitCode::Failed);
    };
    if own_root == inherited_root {
        // A copy at the same address would mean the clone did nothing, and the kernel would be
        // editing tables it does not own while believing it does.
        println!("grey-os: FATAL: the page-table copy came back with the inherited root");
        qemu::exit_qemu(qemu::QemuExitCode::Failed);
    }
    println!(
        "grey-os: framebuffer physical {:#012x}-{:#012x} ({} KiB, resolved by page walk)",
        framebuffer_span.start,
        framebuffer_span.end,
        framebuffer_span.len() / 1024
    );
    println!("grey-os: framebuffer byte {probe_byte} reads the same through both mappings");
    println!(
        "grey-os: page tables: inherited root {inherited_root:#x}, own root {own_root:#x}, \
         {table_frames} frames"
    );
    // SAFETY: `own_root` is a complete copy of the tree the CPU is already using, so the code
    // making this call, its stack and everything reachable stays mapped.
    unsafe { paging::load_root(own_root) };

    // The arena (ADR-031): a fixed span, pages from the frame allocator, mapped through the tree
    // just loaded. From here every allocation is memory this kernel mapped itself, so growth
    // costs frames and a free at the top gives them back.
    //
    // SAFETY: `own_root` is the tree the CPU is switching to and `offset` is the window; the span
    // is the constant above, which nothing else in this boot maps.
    unsafe { HEAP.source().install(own_root, offset) };
    // SAFETY: the span is address space nothing else maps, and this is the only call.
    if !unsafe { HEAP.init_arena(ARENA_BASE, ARENA_BYTES) } {
        println!("grey-os: FATAL: the arena refused {ARENA_BASE:#x}..{ARENA_BYTES:#x}");
        qemu::exit_qemu(qemu::QemuExitCode::Failed);
    }

    // M4's growth proof: the frames a page of heap costs have to be frames the frame allocator
    // says are gone. A heap that mapped pages out of its own span without asking would show the
    // same growth and none of the cost, which is the bug this line exists to catch.
    let free_before = frame::FRAMES
        .stats()
        .expect("the frame allocator was initialised above")
        .free;
    // SAFETY: every layout in the list is valid, the chunk sizes fit the static buffer inside
    // `churn`, and each block is freed once with the layout it was allocated with.
    let churn = unsafe { arena::churn(&HEAP, &CHURN_SIZES) };
    let taken = free_before.saturating_sub(
        frame::FRAMES
            .stats()
            .expect("the frame allocator was initialised above")
            .free,
    );
    // The invariant: the frames the frame allocator lost are the pages the arena kept plus
    // whatever tables the span needed, and never fewer than the pages. An arena that mapped pages
    // out of its own span without asking would show growth and cost nothing, which is the bug
    // this is here to catch.
    let net = churn.grown.saturating_sub(churn.returned);
    let tables = taken.saturating_sub(net);
    if !churn.balanced() || taken < net {
        println!(
            "grey-os: FATAL: churn left {} bytes in use (started at {}), grew {} pages, returned {} and cost {taken} frames for {net}",
            churn.bytes_after, churn.bytes_before, churn.grown, churn.returned
        );
        qemu::exit_qemu(qemu::QemuExitCode::Failed);
    }
    println!(
        "grey-os: heap churn ok: {} allocations and {} frees over {} bytes, in use back to {}, {} pages grown and {} returned, {net} frames kept plus {tables} for the span's page tables",
        churn.allocations,
        churn.frees,
        churn.bytes_before,
        churn.bytes_after,
        churn.grown,
        churn.returned
    );

    // The three frees a checked allocator has to refuse, counted rather than followed (ADR-030).
    // SAFETY: `refusals` allocates the block it frees, and frees it once with its real size.
    let refused = unsafe { arena::refusals(&HEAP) };
    if refused != 3 {
        println!("grey-os: FATAL: only {refused} of the three bad frees were refused");
        qemu::exit_qemu(qemu::QemuExitCode::Failed);
    }
    println!(
        "grey-os: heap frees: a double free, a pointer from outside and a wrong size all refused and counted"
    );

    // The shrink half: a block of whole pages at the top of an empty arena goes back to the frame
    // allocator, which is the only caller `frame::FRAMES.free` had until now.
    let Ok(whole_pages) = Layout::from_size_align(arena::PAGE_BYTES, arena::PAGE_BYTES) else {
        println!("grey-os: FATAL: the trim probe's layout is not valid");
        qemu::exit_qemu(qemu::QemuExitCode::Failed);
    };
    // SAFETY: the layout is valid and the block is freed below, once.
    let trimmed = unsafe {
        let pointer = HEAP.alloc_layout(whole_pages);
        if pointer.is_null() {
            println!("grey-os: FATAL: the trim probe could not allocate a page");
            qemu::exit_qemu(qemu::QemuExitCode::Failed);
        }
        let mapped = HEAP.stats().pages;
        HEAP.dealloc_layout(pointer, whole_pages);
        mapped - HEAP.stats().pages
    };
    if trimmed == 0 {
        println!("grey-os: FATAL: a free at the top of the arena returned no pages");
        qemu::exit_qemu(qemu::QemuExitCode::Failed);
    }
    println!("grey-os: heap trim ok: {trimmed} pages went back to the frame allocator");

    // The containers the next milestones need, on the new allocator: a `Vec` that grows until it
    // reallocates several times, and a `String` built from parts. Both are leaked on purpose -
    // the claim is that they were built, and the heap reporting something in use afterwards is
    // what gives the guard line below a heap to guard.
    let mut numbers: alloc::vec::Vec<u64> = alloc::vec::Vec::new();
    for value in 0..512u64 {
        numbers.push(value * value);
    }
    let expected: u64 = (0..512u64).map(|value| value * value).sum();
    let mut text = alloc::string::String::new();
    for word in ["grey", "-", "os"] {
        text.push_str(word);
    }
    if numbers.iter().copied().sum::<u64>() != expected || text != "grey-os" {
        println!("grey-os: FATAL: a Vec or a String did not come back with what went into it");
        qemu::exit_qemu(qemu::QemuExitCode::Failed);
    }
    let container_bytes = HEAP.stats().bytes_in_use;
    println!(
        "grey-os: containers ok: a {}-element Vec and a {} byte String, {container_bytes} bytes in use on the arena",
        numbers.len(),
        text.len()
    );
    core::mem::forget(numbers);
    core::mem::forget(text);

    // The guard page, without arming it: the arena has mapped everything below `end` and nothing
    // at or above it, so the first byte past the heap faults because no entry describes it
    // (ADR-031). M3 had to take a mapping away to get this; a heap that grows a page at a time
    // never puts one there in the first place.
    let arena_stats = HEAP.stats();
    println!(
        "grey-os: heap guard armed at {:#x}: the first page above the heap's {} mapped pages, nothing mapped there",
        arena_stats.end, arena_stats.pages
    );

    // The mapper, both ways: a frame the allocator handed out, an address nothing mapped, and the
    // frame read back through it. M3 could only take a mapping away (ADR-029); this is the first
    // time the kernel gives one out, and the shape of the check is the framebuffer's - two views
    // of the same frame have to agree (ADR-030).
    match probe_mapping(own_root, offset) {
        MappingProbe::Ok(address) => println!(
            "grey-os: mapping probe ok: frame mapped at {address:#x}, written through the \
             mapping and read back by frame"
        ),
        MappingProbe::Skipped(reason) => {
            println!("grey-os: WARNING: mapping probe skipped: {reason}");
        }
        MappingProbe::Refused(error) => {
            println!("grey-os: FATAL: the mapper refused a free address: {error:?}");
            qemu::exit_qemu(qemu::QemuExitCode::Failed);
        }
        MappingProbe::Disagreed => {
            println!("grey-os: FATAL: the byte written through the new mapping did not arrive");
            qemu::exit_qemu(qemu::QemuExitCode::Failed);
        }
        MappingProbe::LeftMapped(error) => {
            println!("grey-os: FATAL: the mapping probe could not unmap its page: {error:?}");
            qemu::exit_qemu(qemu::QemuExitCode::Failed);
        }
    }

    // Interrupts last, and in this order: the PIC is remapped before any line is unmasked,
    // the timer claims IRQ0 and installs its handler, and only then is `IF` set. Enabling
    // earlier would leave a window where a tick arrives on a vector with no handler behind it
    // (ADR-028).
    pic::init();
    let Some(actual_hz) = timer::init(TIMER_HZ) else {
        println!(
            "grey-os: FATAL: {} Hz is outside what the pit can divide",
            TIMER_HZ
        );
        qemu::exit_qemu(qemu::QemuExitCode::Failed);
    };
    if !pic::is_unmasked(pic::TIMER) {
        println!(
            "grey-os: FATAL: irq{} is still masked after the pic setup",
            pic::TIMER
        );
        qemu::exit_qemu(qemu::QemuExitCode::Failed);
    }
    idt::enable();
    // Waiting is the test: a counter that does not move means the gate, the remap or the
    // masking is wrong, and the boot log would otherwise claim a timer it does not have.
    timer::wait_ticks(OBSERVED_TICKS);

    println!(
        "grey-os: gdt loaded: code {:#04x}, data {:#04x}, tss {:#04x}, {} KiB double-fault stack",
        gdt::KERNEL_CODE,
        gdt::KERNEL_DATA,
        gdt::TSS,
        gdt::STACK_BYTES / 1024
    );
    println!(
        "grey-os: idt loaded: {} gates of 256 ({} exceptions, 16 irqs)",
        idt::gates(),
        idt::EXCEPTIONS.len()
    );
    println!(
        "grey-os: pic remapped to {:#04x}..={:#04x}, only irq{} unmasked",
        pic::VECTOR_BASE,
        pic::VECTOR_BASE + 15,
        pic::TIMER
    );
    println!(
        "grey-os: timer {} Hz, {} ticks observed, interrupts enabled, {} unexpected irqs",
        actual_hz,
        timer::ticks(),
        idt::unexpected_irqs()
    );

    // M2's fault paths are tested the way M0's panic path is: a cfg compiled in from an
    // environment variable, one image per fault, and `cargo xtask test` booting each and
    // asserting the report (ADR-023, ADR-028). Each block is a real instruction the CPU
    // refuses, so the handler is reached the way it will be in the wild rather than by being
    // called from Rust, which would prove only that a function prints.
    #[cfg(grey_os_fault_gp)]
    {
        println!("grey-os: GREY_OS_EXCEPTION_TEST=gp, loading a selector that is not in the gdt");
        // SAFETY: deliberately loads selector 0x28, which is past the end of the five-entry
        // table loaded at boot. The #GP it raises is the point of the run: the handler
        // reports it and exits.
        unsafe {
            core::arch::asm!("mov ax, 0x28", "mov ds, ax", out("ax") _);
        }
    }

    #[cfg(grey_os_fault_pf)]
    {
        println!("grey-os: GREY_OS_EXCEPTION_TEST=pf, reading an address nothing maps");
        // SAFETY: reads an address no page table maps, which raises #PF - the point of the
        // run. `volatile` keeps the read from being optimised away.
        let _ = unsafe { core::ptr::read_volatile(UNMAPPED_PROBE as *const u8) };
    }

    #[cfg(grey_os_fault_ud)]
    {
        println!("grey-os: GREY_OS_EXCEPTION_TEST=ud, executing an undefined instruction");
        // SAFETY: `ud2` is defined by the architecture as raising #UD, which is the point.
        unsafe { core::arch::asm!("ud2") };
    }

    #[cfg(grey_os_fault_df)]
    {
        println!("grey-os: GREY_OS_EXCEPTION_TEST=df, faulting while a fault is delivered");
        // Interrupts off first: a timer tick landing after the stack pointer moves would fault
        // on the same bad stack and make the report's instruction pointer a coin toss.
        idt::disable();
        // SAFETY: `grey_os_double_fault_probe` moves `RSP` to an address nothing maps and then
        // pushes, so the `#PF` the push raises cannot push its own frame either, and the CPU
        // escalates to `#DF`. Reaching the handler is the test: with the TSS stack or its gate
        // wrong this triple-faults and QEMU exits without printing a line (ADR-028).
        //
        // `int 8` would reach the same vector but not the same frame: a software `int n` pushes
        // no error code, even for a vector whose hardware fault does, so the handler would read
        // the instruction pointer as its error code.
        unsafe { grey_os_double_fault_probe() };
    }

    // M3's fault path: a write one byte past the end of the heap has to reach the `#PF`
    // handler instead of landing in whatever follows. The guard is the first page past the arena's
    // growth frontier, which nothing ever mapped, and `HEAP.stats().end` is its first byte - after
    // the churn and the containers above, so it is the *grown* heap's end (ADR-031).
    #[cfg(grey_os_fault_guard)]
    {
        let end = HEAP.stats().end;
        println!(
            "grey-os: GREY_OS_EXCEPTION_TEST=guard, writing one byte past the heap at {end:#x}"
        );
        // SAFETY: `end` is the first page above the arena's mapped pages, which no table entry
        // describes, so this write raises #PF - the point of the run.
        unsafe { core::ptr::write_volatile(end as *mut u8, 0xa5) };
    }

    // The M0 acceptance criterion "a forced panic prints `[panic] <message>` and its
    // location" needs a kernel that can be told to fail. `kernel/build.rs` turns
    // `GREY_OS_PANIC_TEST` into the cfg below, and `cargo xtask test` boots a second
    // image with it set. Deliberately after `framebuffer::install`: the panel half of
    // the panic path is only worth testing once there is a panel to paint.
    //
    // The two exits are mutually exclusive cfgs rather than a panic block followed by
    // the success path: with the flag on, everything after a diverging block would be
    // unreachable code, and `unreachable_code` would then fire on a boot that is
    // working exactly as designed.
    #[cfg(grey_os_panic_test)]
    {
        println!("grey-os: GREY_OS_PANIC_TEST is set, panicking on purpose");
        panic!("injected panic for the panic-path test");
    }

    #[cfg(not(grey_os_panic_test))]
    {
        println!("grey-os: boot ok");
        qemu::exit_qemu(qemu::QemuExitCode::Success);
    }
}

/// Allocates [`PROBE_FRAMES`], writes a distinct value into each frame through the physical
/// window, reads them back and returns them.
///
/// This is the frame allocator's half of the heap's 4 KiB probe: a bitmap that hands out
/// frames over memory the kernel does not own, or hands the same frame out twice, fails here at
/// boot instead of in a driver later with a corrupted structure.
fn probe_frames(offset: u64) -> Option<u64> {
    let mut frames = [0u64; PROBE_FRAMES];
    for slot in frames.iter_mut() {
        *slot = frame::FRAMES.alloc()?;
    }
    // The allocator's whole job is to hand out each frame once.
    for (index, address) in frames.iter().enumerate() {
        if frames[..index].contains(address) {
            return None;
        }
    }
    let pattern = |index: usize| 0xfeed_0000_0000_0000 | index as u64;
    for (index, address) in frames.iter().enumerate() {
        // SAFETY: the frame came from the allocator, so nothing else uses it, and the
        // bootloader maps all of physical memory writable at `offset` (ADR-022).
        unsafe { core::ptr::write_volatile((offset + address) as *mut u64, pattern(index)) };
    }
    for (index, address) in frames.iter().enumerate() {
        // SAFETY: the write above put the value at this address, and the frame is still ours.
        let value = unsafe { core::ptr::read_volatile((offset + address) as *const u64) };
        if value != pattern(index) {
            return None;
        }
    }
    let first = frames[0];
    for address in frames {
        frame::FRAMES.free(address);
    }
    Some(first)
}

/// What the mapping round trip found.
///
/// The failures are separate because they mean different things: no free address or no frame left
/// means the probe did not run and the boot can carry on, while a mapper that refuses a free
/// address, writes to the wrong frame or leaves a page mapped is a bug worth stopping for.
enum MappingProbe {
    /// The address the probe mapped and then unmapped again.
    Ok(u64),
    /// The probe could not run, and this says why.
    Skipped(&'static str),
    /// `map_4k` refused an address and a frame it was told were free.
    Refused(paging::MapError),
    /// The write through the new mapping did not reach the frame behind it.
    Disagreed,
    /// `unmap_4k` failed, so the address is still mapped.
    LeftMapped(paging::UnmapError),
}

/// Maps a frame at [`MAPPING_PROBE`], writes through the new mapping, reads the frame back through
/// the window, then takes the mapping away and hands the frame back.
///
/// [`probe_frames`] proves the allocator over memory the bootloader mapped; this proves the
/// mapper, over a page that exists only because this call asked for it. A wrong leaf, a wrong
/// parent entry or a missing `invlpg` shows up as a fault or as a byte that never arrived.
///
/// The three tables the mapping creates are kept: a boot maps this address once, and taking tables
/// away needs its own bookkeeping, which is the next thing the heap will need anyway.
fn probe_mapping(root: u64, offset: u64) -> MappingProbe {
    // SAFETY: `root` is the tree the CPU is using and `offset` is the window it maps.
    if unsafe { paging::walk(root, offset, MAPPING_PROBE) }.is_some() {
        // Mapping over an address something else owns would not prove anything, and it would
        // overwrite it: the probe stands down rather than guessing an address of its own.
        return MappingProbe::Skipped("the address the probe wanted is already mapped");
    }
    let Some(frame) = frame::FRAMES.alloc() else {
        return MappingProbe::Skipped("no frame left to map");
    };
    let flags = paging::Entry::WRITABLE | paging::Entry::NO_EXECUTE;
    // SAFETY: `root` is the live tree, `offset` the window, `frame` came from the allocator,
    // `MAPPING_PROBE` was just walked and found unmapped, and the closure hands out only frames
    // the bitmap says are free.
    let mapped = unsafe {
        paging::map_4k(root, offset, MAPPING_PROBE, frame, flags, || {
            frame::FRAMES.alloc()
        })
    };
    if let Err(error) = mapped {
        // The frame never got an address, so it is free again.
        frame::FRAMES.free(frame);
        return MappingProbe::Refused(error);
    }
    // A byte nothing else in this boot writes, through the mapping and not the window: if the
    // leaf or any level above it is wrong, the CPU says so here.
    // SAFETY: the mapping just made is present, writable, 4 KiB and reaches this frame.
    unsafe { core::ptr::write_volatile(MAPPING_PROBE as *mut u8, 0xa5) };
    // SAFETY: the same frame, read the way [`probe_frames`] reads one.
    let arrived = unsafe { core::ptr::read_volatile((offset + frame) as *const u8) };
    // SAFETY: as above. The address is a 4 KiB entry, so this needs no split.
    let unmapped =
        unsafe { paging::unmap_4k(root, offset, MAPPING_PROBE, || frame::FRAMES.alloc()) };
    // SAFETY: `root` is the live tree.
    let gone = unsafe { paging::walk(root, offset, MAPPING_PROBE) }.is_none();
    // Free again either way: the mapping that might still point here belongs to a boot that stops.
    frame::FRAMES.free(frame);
    match (arrived, unmapped, gone) {
        (0xa5, Err(error), _) => MappingProbe::LeftMapped(error),
        (0xa5, Ok(()), true) => MappingProbe::Ok(MAPPING_PROBE),
        // A byte that did not arrive, or an entry that went empty while the walk still found it.
        _ => MappingProbe::Disagreed,
    }
}

/// Set once the panic path runs, so a panic inside it cannot recurse forever.
static PANICKING: AtomicBool = AtomicBool::new(false);
#[panic_handler]
fn panic(info: &PanicInfo<'_>) -> ! {
    if PANICKING.swap(true, Ordering::SeqCst) {
        halt_loop();
    }

    // Report message and location so a real-hardware failure is diagnosable.
    if !serial::try_print_args(format_args!("\n[panic] {info}")) {
        serial::print_args_unlocked(format_args!(
            "\n[panic] {info}\n[panic] (console was busy; lines may be interleaved)"
        ));
    }

    // Show the failure on the panel too: hardware rarely has a serial cable attached, and a
    // black screen is undiagnosable. The log so far stays above the report and both are
    // drawn on the panic colour, so the context that led here is still readable. The
    // outcome is reported on serial as well, because nothing outside the guest can see the
    // screen: that line is what lets `cargo xtask test` cover this half of the path.
    if display::paint_panic(info) {
        serial::print_args_unlocked(format_args!("[panic] panel painted red"));
    } else {
        serial::print_args_unlocked(format_args!("[panic] no framebuffer to paint"));
    }

    qemu::exit_qemu(qemu::QemuExitCode::Failed)
}
