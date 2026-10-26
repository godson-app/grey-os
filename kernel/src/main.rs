#![no_std]
#![no_main]

// `Box`/`Vec` live here. The kernel has no `std`; `alloc` is the half of the standard
// library that only needs a `#[global_allocator]`, which `HEAP` below provides.
extern crate alloc;

use bootloader_api::config::Mapping;
use bootloader_api::info::MemoryRegionKind;
use bootloader_api::{entry_point, BootInfo, BootloaderConfig};
use core::panic::PanicInfo;
use core::sync::atomic::{AtomicBool, Ordering};
use grey_os::{framebuffer, halt_loop, heap, memory, qemu, serial};

/// Prints one line to the serial console.
macro_rules! println {
    () => {
        grey_os::serial::print_args(format_args!(""))
    };
    ($($arg:tt)*) => {
        grey_os::serial::print_args(format_args!($($arg)*))
    };
}

/// The kernel's only allocator: a bump allocator over the region the boot sequence
/// picks out of the memory map (ADR-021). It is initialised in `kernel_main` once the
/// map has been read, so an allocation before that point returns null and fails
/// loudly instead of writing somewhere unowned.
#[global_allocator]
static HEAP: heap::BumpAllocator = heap::BumpAllocator::new();

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

    // Where the framebuffer physically lives. It is ordinary RAM that firmware
    // happens to display from, so the map below may well offer its pages as usable;
    // a heap that took them would paint over the screen at some later moment.
    //
    // `bootloader_api` 0.11 publishes the framebuffer only as a *virtual* address the
    // bootloader picked; it never says which physical frames back it. Subtracting
    // `physical_memory_offset` from that pointer would therefore produce a number that
    // is not an address at all, which is worse than no number. Until the kernel can
    // walk its own page tables (ROADMAP M3), the guard in `pick_heap_region` has no
    // span to enforce, and this says so rather than reserving a made-up range.
    let framebuffer_span: Option<memory::Span> = None;
    if framebuffer_span.is_none() {
        println!(
            "grey-os: WARNING: framebuffer physical base unpublished, so the heap is not kept off it"
        );
    }

    // The memory map is firmware data rather than a fact we chose, so log it before
    // anything spends it. This is the last checkpoint before the heap exists.
    let mut usable_bytes: u64 = 0;
    for region in boot_info.memory_regions.iter() {
        let span = memory::Span::new(region.start, region.end);
        let kind = match region.kind {
            MemoryRegionKind::Usable => "usable",
            MemoryRegionKind::Bootloader => "bootloader",
            // `MemoryRegionKind` is `#[non_exhaustive]`: a kind a newer firmware
            // reports is not usable here, and "reserved" is the safe reading.
            _ => "reserved",
        };
        if region.kind == MemoryRegionKind::Usable {
            usable_bytes = usable_bytes.saturating_add(span.len());
        }
        println!(
            "grey-os: memory {:#012x}-{:#012x} {:>7} KiB {}",
            span.start,
            span.end,
            span.len() / 1024,
            kind
        );
    }
    println!("grey-os: memory usable total {} KiB", usable_bytes / 1024);

    let heap_span = memory::pick_heap_region(
        boot_info
            .memory_regions
            .iter()
            .map(|region| memory::Region {
                start: region.start,
                end: region.end,
                usable: region.kind == MemoryRegionKind::Usable,
            }),
        framebuffer_span,
    );
    match heap_span {
        Some(span) => println!(
            "grey-os: heap candidate {:#012x}-{:#012x} ({} KiB)",
            span.start,
            span.end,
            span.len() / 1024
        ),
        None => {
            println!("grey-os: FATAL: no usable memory region to draw a heap from");
            qemu::exit_qemu(qemu::QemuExitCode::Failed);
        }
    }

    // The span picked above is physical memory that firmware happens to map somewhere
    // of its choosing. Convert it through `physical_memory_offset` before the
    // allocator sees it: handing `GlobalAlloc` a physical address writes to whatever
    // is mapped at that virtual address, which is silent corruption rather than a
    // failed boot.
    if let Some(span) = heap_span {
        // The window enabled by `BOOTLOADER_CONFIG` above. Read here rather than at the
        // top of the function: this is the only place that needs it.
        let offset = boot_info
            .physical_memory_offset
            .as_mut()
            .map(|offset| *offset);
        let range = offset.and_then(|offset| {
            let start = offset.checked_add(span.start)?;
            let end = offset.checked_add(span.end)?;
            Some((usize::try_from(start).ok()?, usize::try_from(end).ok()?))
        });
        let Some((start, end)) = range else {
            // Two ways to get here, and they need different fixes, so say which.
            match offset {
                Some(offset) => println!(
                    "grey-os: FATAL: heap {:#x}..{:#x} does not fit past the {offset:#x} mapping",
                    span.start, span.end
                ),
                None => println!(
                    "grey-os: FATAL: boot info carries no physical memory offset, so the heap cannot be reached"
                ),
            }
            qemu::exit_qemu(qemu::QemuExitCode::Failed);
        };
        // SAFETY: `start..end` is a region the firmware reports as usable, nothing else
        // writes there, the bootloader maps all of physical memory writable at
        // `physical_memory_offset` (ADR-022), and this is the only call to `init`.
        if !unsafe { HEAP.init(start, end) } {
            println!("grey-os: FATAL: heap range {start:#x}..{end:#x} is not usable");
            qemu::exit_qemu(qemu::QemuExitCode::Failed);
        }
        let stats = HEAP.stats();
        println!(
            "grey-os: heap ready {:#x}..{:#x}, {} KiB reserved",
            stats.base,
            stats.end,
            stats.end.saturating_sub(stats.base) / 1024
        );

        // A heap is only worth anything if the memory behind it is really mapped at the
        // virtual address the allocator returns. Write a whole page through a `Box` and
        // read it back: a wrong `physical_memory_offset` faults here, at boot, instead of
        // in a driver later. This is the kernel's only allocation until a driver needs
        // one, and it is why the log line below is a claim and not a hope.
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
        let stats = HEAP.stats();
        println!(
            "grey-os: heap probe: 4 KiB written and read back, {} allocations, {} bytes in use",
            stats.allocations, stats.bytes_in_use
        );
    }
    // UEFI has no VGA text mode, so we draw to the framebuffer instead. This is
    // the only display path: GOP from boot information, no GPU drivers.
    if let Some(screen) = boot_info.framebuffer.as_mut() {
        let info = screen.info();
        println!(
            "grey-os: framebuffer {}x{}, stride {}, {} bytes/pixel",
            info.width, info.height, info.stride, info.bytes_per_pixel
        );

        if !framebuffer::fill(screen, framebuffer::BACKGROUND) {
            println!("grey-os: FATAL: framebuffer layout unsupported (need 4-byte RGB or BGR)");
            qemu::exit_qemu(qemu::QemuExitCode::Failed);
        }
        framebuffer::install(screen);

        println!("grey-os: display adapter online (direct-to-framebuffer, no back buffer yet)");
    } else {
        // Failing loudly beats booting into a black screen nobody can diagnose.
        println!("grey-os: FATAL: boot info carries no framebuffer; GOP is required");
        qemu::exit_qemu(qemu::QemuExitCode::Failed);
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

    // Show the failure on the panel too: hardware rarely has a serial cable
    // attached, and a black screen is undiagnosable. The outcome is reported on
    // serial as well, because nothing outside the guest can see the screen: that
    // line is what lets `cargo xtask test` cover this half of the path at all.
    if framebuffer::with_screen(|screen| framebuffer::fill(screen, framebuffer::PANIC)) {
        serial::print_args_unlocked(format_args!("[panic] panel painted red"));
    } else {
        serial::print_args_unlocked(format_args!("[panic] no framebuffer to paint"));
    }

    qemu::exit_qemu(qemu::QemuExitCode::Failed)
}
