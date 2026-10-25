//! Launches the UEFI disk image in QEMU.

use anyhow::{anyhow, Result};
use ovmf_prebuilt::{Arch, FileType, Prebuilt, Source};
use std::process::{Command, ExitCode};

fn main() -> Result<ExitCode> {
    let firmware = Prebuilt::fetch(Source::LATEST, "target/ovmf")
        .map_err(|e| anyhow!("failed to fetch OVMF firmware: {e:?}"))?;
    let code = firmware.get_file(Arch::X64, FileType::Code);
    let vars = firmware.get_file(Arch::X64, FileType::Vars);

    let status = Command::new("qemu-system-x86_64")
        .args(["-m", "512M"])
        .args(["-serial", "stdio"]) // the kernel's serial log appears in this terminal
        .arg("-no-reboot") // stop on a triple fault instead of reboot-looping
        // Lets the kernel hand a pass/fail status back to this process.
        .args(["-device", "isa-debug-exit,iobase=0xf4,iosize=0x04"])
        .args([
            "-drive",
            &format!("format=raw,if=pflash,readonly=on,file={}", code.display()),
            "-drive",
            &format!("format=raw,if=pflash,file={}", vars.display()),
            "-drive",
            &format!("format=raw,file={}", env!("UEFI_IMAGE")),
        ])
        .status()?;

    // The kernel exits through QEMU's `isa-debug-exit`, which reports
    // `(code << 1) | 1`: 33 means it booted and finished cleanly, 35 means it
    // reported a failure. Any other status is QEMU itself complaining.
    match status.code() {
        Some(33) => Ok(ExitCode::SUCCESS),
        Some(35) => Ok(ExitCode::FAILURE),
        Some(other) => Ok(ExitCode::from(u8::try_from(other).unwrap_or(1))),
        None => Ok(ExitCode::FAILURE),
    }
}
