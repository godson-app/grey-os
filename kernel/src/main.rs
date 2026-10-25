#![no_std]
#![no_main]

use bootloader_api::{entry_point, BootInfo};
use core::panic::PanicInfo;
use core::sync::atomic::{AtomicBool, Ordering};
use grey_os::{framebuffer, halt_loop, qemu, serial};

/// Prints one line to the serial console.
macro_rules! println {
    () => {
        grey_os::serial::print_args(format_args!(""))
    };
    ($($arg:tt)*) => {
        grey_os::serial::print_args(format_args!($($arg)*))
    };
}

entry_point!(kernel_main);

fn kernel_main(boot_info: &'static mut BootInfo) -> ! {
    serial::init();
    println!("grey-os: serial console online (COM1, 115200 8N1)");

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

    println!("grey-os: boot ok");
    qemu::exit_qemu(qemu::QemuExitCode::Success);
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
    // attached, and a black screen is undiagnosable.
    if !framebuffer::with_screen(|screen| framebuffer::fill(screen, framebuffer::PANIC)) {
        serial::print_args_unlocked(format_args!("[panic] no framebuffer to paint"));
    }

    qemu::exit_qemu(qemu::QemuExitCode::Failed)
}
