//! Grey OS kernel library.
//!
//! Everything that can be tested without a machine lives here. The `[[bin]]`
//! target keeps only what has to be the entry point: `entry_point!`, the boot
//! sequence, the panic handler and the `println!` macro those use.
//!
//! `canvas`, `memory` and `heap` are free of port I/O and of `bootloader_api`, so
//! `cargo test --package kernel --lib` can run them on the host. The other modules
//! are `x86_64`-only and are compiled out there, because inline `asm!` and port
//! I/O do not exist on the machine running the tests.

#![cfg_attr(not(test), no_std)]

pub mod canvas;
pub mod heap;
pub mod memory;

#[cfg(target_arch = "x86_64")]
pub mod framebuffer;
#[cfg(target_arch = "x86_64")]
pub mod qemu;
#[cfg(target_arch = "x86_64")]
pub mod serial;

/// Parks the CPU until the next interrupt instead of burning 100% in a busy loop.
///
/// Lives in the library rather than the binary because `qemu::exit_qemu` falls
/// through into it, and that call now crosses a crate boundary.
#[cfg(target_arch = "x86_64")]
pub fn halt_loop() -> ! {
    loop {
        // SAFETY: `hlt` only pauses the CPU until the next interrupt. It touches
        // no memory and leaves registers and flags alone.
        unsafe {
            core::arch::asm!("hlt", options(nomem, nostack, preserves_flags));
        }
    }
}
