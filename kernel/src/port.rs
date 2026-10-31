//! Port I/O, in one place.
//!
//! `pic`, `serial` and `timer` each grew their own `outb` because each was the first
//! thing that needed one. Every driver after them needs the same two instructions and a
//! way to widen them to words, so this module is where that lives from M5 on: one
//! `unsafe` block per instruction, documented once, instead of one per driver.
//!
//! The whole module is `x86_64`-only. The port address space is part of the platform,
//! not of any driver, which is why it is not called `disk::port` or `keyboard::port`.

/// Reads one byte from `port`.
///
/// # Safety
///
/// The caller must know what is at `port`. Reading a port nothing answers on a real
/// machine returns `0xff` and can upset the device's state machine; on QEMU it is
/// usually harmless, which is exactly why the promise is on the caller.
#[cfg(target_arch = "x86_64")]
pub unsafe fn inb(port: u16) -> u8 {
    let value: u8;
    // SAFETY: `in` reads the port the caller vouched for and has no memory operands.
    unsafe {
        core::arch::asm!(
            "in al, dx",
            in("dx") port,
            out("al") value,
            options(nomem, nostack, preserves_flags)
        );
    }
    value
}

/// Writes one byte to `port`.
///
/// # Safety
///
/// As [`inb`]: a write to a port that means something else is a device command the
/// caller did not intend to send.
#[cfg(target_arch = "x86_64")]
pub unsafe fn outb(port: u16, value: u8) {
    // SAFETY: `out` writes the port the caller vouched for and has no memory operands.
    unsafe {
        core::arch::asm!(
            "out dx, al",
            in("dx") port,
            in("al") value,
            options(nomem, nostack, preserves_flags)
        );
    }
}

/// Reads one 16-bit word from `port`.
///
/// # Safety
///
/// As [`inb`].
#[cfg(target_arch = "x86_64")]
pub unsafe fn inw(port: u16) -> u16 {
    let value: u16;
    // SAFETY: `in` reads the port the caller vouched for and has no memory operands.
    unsafe {
        core::arch::asm!(
            "in ax, dx",
            in("dx") port,
            out("ax") value,
            options(nomem, nostack, preserves_flags)
        );
    }
    value
}

/// Writes one 16-bit word to `port`.
///
/// # Safety
///
/// As [`outb`].
#[cfg(target_arch = "x86_64")]
pub unsafe fn outw(port: u16, value: u16) {
    // SAFETY: `out` writes the port the caller vouched for and has no memory operands.
    unsafe {
        core::arch::asm!(
            "out dx, ax",
            in("dx") port,
            in("ax") value,
            options(nomem, nostack, preserves_flags)
        );
    }
}

/// Reads `buffer.len() / 2` words from `port` into `buffer`.
///
/// The ATA controller hands a sector over through a single port, one word at a time, and
/// `rep insw` is what turns that into one instruction instead of 256 round trips through
/// the assembler block. Odd lengths are refused rather than half-read: a caller that
/// wants bytes has to say so.
///
/// # Safety
///
/// As [`inb`], and `buffer` must be at least `buffer.len()` bytes of writable memory.
#[cfg(target_arch = "x86_64")]
pub unsafe fn insw(port: u16, buffer: &mut [u8]) {
    debug_assert!(buffer.len().is_multiple_of(2), "insw needs whole words");
    let words = buffer.len() / 2;
    // SAFETY: `rep insw` writes `words` 2-byte words to `rdi`, which is `buffer`, and the
    // assertion above says that many words fit. It reads port `dx` the caller vouched for.
    unsafe {
        core::arch::asm!(
            "rep insw",
            in("dx") port,
            inout("rcx") words => _,
            in("rdi") buffer.as_mut_ptr(),
            options(nostack, preserves_flags)
        );
    }
}

/// Writes `source` to `port`, one word at a time.
///
/// # Safety
///
/// As [`outb`], and `source` must be at least `source.len()` bytes of readable memory.
#[cfg(target_arch = "x86_64")]
pub unsafe fn outsw(port: u16, source: &[u8]) {
    debug_assert!(source.len().is_multiple_of(2), "outsw needs whole words");
    let words = source.len() / 2;
    // SAFETY: `rep outsw` reads `words` 2-byte words from `rsi`, which is `source`, and the
    // assertion above says it holds that many. It writes port `dx` the caller vouched for.
    unsafe {
        core::arch::asm!(
            "rep outsw",
            in("dx") port,
            inout("rcx") words => _,
            in("rsi") source.as_ptr(),
            options(nostack, preserves_flags)
        );
    }
}

/// The read of the alternate status register some controllers need between commands.
///
/// A port read takes about a microsecond on real hardware and nothing at all on an
/// emulator, which is what the ATA specification asks for before the next command is
/// written. Reading a status register has no side effects on the primary channel's
/// alternate port, which is why it is the usual way to do it.
///
/// # Safety
///
/// As [`inb`], for the alternate status port of a real controller.
#[cfg(target_arch = "x86_64")]
pub unsafe fn io_wait(control: u16) {
    // SAFETY: the alternate status register is read-only, so this has no side effects
    // beyond the delay it exists for.
    unsafe {
        let _ = inb(control);
    }
}
