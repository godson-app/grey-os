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
        .args(["-serial", "stdio"]) // kernel logs will appear in this terminal later
        .arg("-no-reboot") // stop on a triple fault instead of reboot-looping
        .args([
            "-drive",
            &format!("format=raw,if=pflash,readonly=on,file={}", code.display()),
            "-drive",
            &format!("format=raw,if=pflash,file={}", vars.display()),
            "-drive",
            &format!("format=raw,file={}", env!("UEFI_IMAGE")),
        ])
        .status()?;

    Ok(ExitCode::from(
        status
            .code()
            .and_then(|c| u8::try_from(c).ok())
            .unwrap_or(1),
    ))
}
