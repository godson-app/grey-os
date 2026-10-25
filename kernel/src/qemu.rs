//! QEMU's `isa-debug-exit` device: how an automated run learns pass from fail.
//!
//! The device is added by our own run commands (`cargo xtask`). It is the only
//! way a `no_std` kernel can hand an exit status back to the test harness.

use core::arch::asm;

/// QEMU exits with `(value << 1) | 1` after a write here.
const PORT: u16 = 0xf4;

/// Values the test harness checks for; changing them means changing `xtask`.
#[derive(Clone, Copy)]
#[repr(u32)]
pub enum QemuExitCode {
    Success = 0x10,
    Failed = 0x11,
}

pub fn exit_qemu(code: QemuExitCode) -> ! {
    // SAFETY: port 0xf4 belongs to QEMU's `isa-debug-exit` device, which only
    // our own run commands attach. On real hardware the chipset discards the
    // write, so execution falls through into the halt below instead of exiting.
    unsafe {
        asm!(
            "out dx, eax",
            in("dx") PORT,
            in("eax") code as u32,
            options(nomem, nostack, preserves_flags)
        );
    }
    crate::halt_loop()
}
