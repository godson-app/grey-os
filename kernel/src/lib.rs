//! Grey OS kernel library.
//!
//! Everything that can be tested without a machine lives here. The `[[bin]]`
//! target keeps only what has to be the entry point: `entry_point!`, the boot
//! sequence, the panic handler and the `println!` macro those use.
//!
//! `canvas`, `font`, `text`, `memory`, `frame`, `paging`, `heap` and `arena` are free of port I/O
//! and of `bootloader_api`, so `cargo test --package kernel --lib` can run them on the host. The
//! other modules are `x86_64`-only and are compiled out there, because inline `asm!` and
//! port I/O do not exist on the machine running the tests.
//!
//! `gdt`, `idt`, `pic` and `timer` are written the same way: the data structures and the
//! arithmetic are plain Rust and are tested on the host, while the half that executes
//! instructions gets the same `cfg` treatment as the rest of the machine-specific code.

#![cfg_attr(not(test), no_std)]
// The `extern "x86-interrupt"` calling convention, which the IDT's handlers are written
// with. Gated by target so a host build on another architecture can still be tested.
#![cfg_attr(target_arch = "x86_64", feature(abi_x86_interrupt))]

// The kernel has no `std`, and the host tests get `alloc` through it, so declaring the
// crate here lets the console's `Vec` and `String` be written once for both builds.
extern crate alloc;

pub mod arena;
pub mod canvas;
pub mod console;
pub mod disk;
pub mod font;
pub mod frame;
pub mod fs;
pub mod gdt;
pub mod heap;
pub mod idt;
pub mod keyboard;
pub mod memory;
pub mod mouse;
pub mod paging;
pub mod pic;
pub mod text;
pub mod timer;

#[cfg(target_arch = "x86_64")]
pub mod display;
#[cfg(target_arch = "x86_64")]
pub mod framebuffer;
#[cfg(target_arch = "x86_64")]
pub mod port;
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
