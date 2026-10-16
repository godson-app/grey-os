#![no_std]
#![no_main]

mod framebuffer;

use bootloader_api::{entry_point, BootInfo};
use core::arch::asm;
use core::panic::PanicInfo;
use framebuffer::Color;

entry_point!(kernel_main);

fn kernel_main(boot_info: &'static mut BootInfo) -> ! {
    // UEFI has no VGA text mode, so we draw to the framebuffer instead.
    if let Some(fb) = boot_info.framebuffer.as_mut() {
        framebuffer::fill(fb, Color::BACKGROUND);
    }
    halt_loop()
}

/// Parks the CPU until the next interrupt instead of burning 100% in a busy loop.
fn halt_loop() -> ! {
    loop {
        // SAFETY: `hlt` only pauses the CPU until the next interrupt; it touches no memory.
        unsafe { asm!("hlt", options(nomem, nostack, preserves_flags)) };
    }
}

#[panic_handler]
fn panic(_info: &PanicInfo<'_>) -> ! {
    halt_loop()
}
