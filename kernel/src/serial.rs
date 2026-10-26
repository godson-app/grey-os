//! Minimal COM1 (0x3F8) serial console.
//!
//! Hand-rolled rather than pulled from a crate: the diagnostic path must not
//! depend on code whose version and API we have not verified, and it has to work
//! before anything else in the kernel exists. It is also the only channel that
//! survives a framebuffer failure on real hardware.

use core::arch::asm;
use core::fmt::{self, Write as _};
use core::sync::atomic::{AtomicBool, Ordering};

/// Base port of the first 16550-compatible UART.
const COM1: u16 = 0x3f8;

/// Bounded wait for the transmitter, so a missing UART cannot hang the kernel.
const TX_SPINS: u32 = 100_000;

fn outb(port: u16, value: u8) {
    // SAFETY: `out` writes one byte to an I/O port. Every caller passes a
    // constant in 0x3F8..=0x3FD, the legacy COM1 register block, which nothing
    // else in this kernel claims, so no unrelated device state is touched.
    unsafe {
        asm!(
            "out dx, al",
            in("dx") port,
            in("al") value,
            options(nomem, nostack, preserves_flags)
        );
    }
}

fn inb(port: u16) -> u8 {
    let value: u8;
    // SAFETY: `in` reads one byte from the same legacy COM1 register block;
    // reading the line-status register has no side effects.
    unsafe {
        asm!(
            "in al, dx",
            out("al") value,
            in("dx") port,
            options(nomem, nostack, preserves_flags)
        );
    }
    value
}

/// Programs COM1 for 115200 baud, 8 data bits, no parity, one stop bit.
pub fn init() {
    outb(COM1 + 1, 0x00); // no UART interrupts: polling keeps this path simple
    outb(COM1 + 3, 0x80); // enable the divisor latch
    outb(COM1, 0x01); // divisor low byte 0b01 -> 115200 baud
    outb(COM1 + 1, 0x00);
    outb(COM1 + 3, 0x03); // 8 data bits, no parity, one stop bit
    outb(COM1 + 2, 0xc7); // enable and clear both FIFOs
    outb(COM1 + 4, 0x03); // assert DTR and RTS
}

fn write_byte_raw(byte: u8) {
    for _ in 0..TX_SPINS {
        if inb(COM1 + 5) & 0x20 != 0 {
            outb(COM1, byte);
            return;
        }
    }
    // The transmitter never became ready: drop the byte rather than hang.
}

fn write_byte(byte: u8) {
    // A bare LF moves down a line without returning the carriage.
    if byte == b'\n' {
        write_byte_raw(b'\r');
    }
    write_byte_raw(byte);
}

static LOCKED: AtomicBool = AtomicBool::new(false);

/// Keeps concurrent writers, including the panic path, from interleaving.
struct Guard {
    owns_lock: bool,
}

impl Drop for Guard {
    fn drop(&mut self) {
        if self.owns_lock {
            LOCKED.store(false, Ordering::Release);
        }
    }
}

impl fmt::Write for Guard {
    fn write_str(&mut self, text: &str) -> fmt::Result {
        for byte in text.bytes() {
            write_byte(byte);
        }
        Ok(())
    }
}

fn acquire() -> Guard {
    while LOCKED
        .compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed)
        .is_err()
    {
        core::hint::spin_loop();
    }
    Guard { owns_lock: true }
}

/// Takes the lock only if it is free, without waiting.
fn try_acquire() -> Option<Guard> {
    LOCKED
        .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
        .is_ok()
        .then_some(Guard { owns_lock: true })
}

fn write_line(mut guard: Guard, args: fmt::Arguments<'_>) {
    let _ = guard.write_fmt(args);
    let _ = guard.write_fmt(format_args!("\n"));
}

/// Prints one line, spinning until the console is free.
pub fn print_args(args: fmt::Arguments<'_>) {
    write_line(acquire(), args);
}

/// Prints one line, or returns `false` if another writer holds the console.
pub fn try_print_args(args: fmt::Arguments<'_>) -> bool {
    let Some(guard) = try_acquire() else {
        return false;
    };
    write_line(guard, args);
    true
}

/// Prints one line, taking the lock if it is free and writing anyway if it is not.
///
/// For the reporting paths - a panic, a CPU exception - where the alternative to an
/// interleaved line is a missing one. A writer that already holds the console is exactly the
/// situation the exception path cannot rule out: the fault may have happened inside
/// `print_args`, and waiting for that lock would hang the only channel that can say so.
pub fn print_args_forced(args: fmt::Arguments<'_>) {
    let guard = try_acquire().unwrap_or(Guard { owns_lock: false });
    write_line(guard, args);
}

/// Prints one line without taking the lock.
///
/// Only the panic path uses this, where the panicking context may already
/// hold the console and waiting would deadlock the only diagnostic channel.
/// Lines may interleave; a truncated report beats no report.
pub fn print_args_unlocked(args: fmt::Arguments<'_>) {
    write_line(Guard { owns_lock: false }, args);
}
