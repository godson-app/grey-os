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
use grey_os::{display, framebuffer, gdt, halt_loop, heap, idt, memory, pic, qemu, serial, timer};

/// The rate the PIT is programmed at. 100 Hz is the usual compromise: fine enough for a
/// scheduler tick later, coarse enough that the handler's cost does not matter yet.
const TIMER_HZ: u32 = 100;

/// Ticks the boot log waits for. Ten at [`TIMER_HZ`] is a tenth of a second, which proves the
/// counter moves without making every boot feel slow under emulation.
const OBSERVED_TICKS: u64 = 10;

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

    // The CPU tables come before anything that can fault on them. From here a bad pointer is
    // a report instead of a triple fault; until here the bootloader's tables still apply, so
    // the window where a fault is undiagnosable is as small as this boot can make it. Their
    // log lines are deferred to the reported section below, like every other discovery
    // (ADR-028).
    gdt::init();
    idt::init();

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
        framebuffer_span,
    );
    let Some(span) = heap_span else {
        println!("grey-os: FATAL: no usable memory region to draw a heap from");
        qemu::exit_qemu(qemu::QemuExitCode::Failed);
    };

    // The span picked above is physical memory that firmware happens to map somewhere of
    // its choosing. Convert it through `physical_memory_offset` before the allocator sees
    // it: handing `GlobalAlloc` a physical address writes to whatever is mapped at that
    // virtual address, which is silent corruption rather than a failed boot.
    //
    // The window enabled by `BOOTLOADER_CONFIG` above. Read here rather than at the top of
    // the function: this is the only place that needs it.
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
    // SAFETY: `start..end` is a region the firmware reports as usable, nothing else writes
    // there, the bootloader maps all of physical memory writable at
    // `physical_memory_offset` (ADR-022), and this is the only call to `init`.
    if !unsafe { HEAP.init(start, end) } {
        println!("grey-os: FATAL: heap range {start:#x}..{end:#x} is not usable");
        qemu::exit_qemu(qemu::QemuExitCode::Failed);
    }

    // A heap is only worth anything if the memory behind it is really mapped at the
    // virtual address the allocator returns. Write a whole page through a `Box` and read
    // it back: a wrong `physical_memory_offset` faults here, at boot, instead of in a
    // driver later. This is the kernel's only allocation until a driver needs one, and it
    // is why the log line below is a claim and not a hope.
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
    let probed = HEAP.stats();
    // UEFI has no VGA text mode, so we draw to the framebuffer instead. This is the only
    // display path: GOP from boot information, no GPU drivers.
    let Some(screen) = boot_info.framebuffer.as_mut() else {
        // Failing loudly beats booting into a black screen nobody can diagnose.
        println!("grey-os: FATAL: boot info carries no framebuffer; GOP is required");
        qemu::exit_qemu(qemu::QemuExitCode::Failed);
    };
    let info = screen.info();
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
    framebuffer::install(screen);
    // The console needs the heap above for its grid and the framebuffer for its pixels.
    if !display::init() {
        println!("grey-os: FATAL: panel has no room for one character cell");
        qemu::exit_qemu(qemu::QemuExitCode::Failed);
    }

    // From here every line goes to serial and to the panel. Serial first in each macro
    // call, because it is the channel that survives a screen going dark later.
    if framebuffer_span.is_none() {
        println!("grey-os: WARNING: framebuffer physical base unknown, heap guard unarmed");
    }
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
        "grey-os: heap candidate {:#012x}-{:#012x} ({} KiB)",
        span.start,
        span.end,
        span.len() / 1024
    );
    let stats = HEAP.stats();
    println!(
        "grey-os: heap ready {:#x}..{:#x}, {} KiB reserved",
        stats.base,
        stats.end,
        stats.end.saturating_sub(stats.base) / 1024
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
