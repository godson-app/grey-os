//! Launches the UEFI disk image in QEMU.
//!
//! This is what plain `cargo run` runs, and it boots the same machine `cargo xtask run`
//! does: four CPUs, no NIC, the boot image on the secondary IDE channel and the 4 MiB
//! `greyfs` data disk on the primary master. The argument list exists twice (ADR-011), and
//! a runner that lags behind the kernel's expectations is not a black screen but a FATAL:
//! with no data disk, the boot image itself becomes the primary master, and `greyfs`
//! refuses the first sector of the ESP.
//!
//! `GREY_OS_INTERACTIVE=1 cargo run` boots the preview (ADR-041): the same machine, with the
//! shell compiled in and no deadline, so the window stays up until `exit` - or until it is
//! closed, which QEMU reports as 0 and this runner passes on as success.

use anyhow::{anyhow, Context, Result};
use ovmf_prebuilt::{Arch, FileType, Prebuilt, Source};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

/// Where `build.rs` stages the images, relative to the workspace root.
const IMAGE_DIR: &str = "target/grey-os";
/// The data disk `greyfs` lives on (M5), created empty on the first run and formatted by
/// the kernel. Only ever a file under `target/`: nothing in this repository writes to a
/// real device.
const DISK_IMAGE: &str = "grey-os-disk.img";
/// Its size: 4 MiB, 8192 sectors - the same disk `xtask` hands the kernel, small enough
/// that every transfer a boot makes is quick and large enough for the directory, the boot
/// counter and the three programs.
const DISK_BYTES: u64 = 4 * 1024 * 1024;

fn main() -> Result<ExitCode> {
    let image_dir = workspace_root().join(IMAGE_DIR);
    fs::create_dir_all(&image_dir)
        .with_context(|| format!("failed to create {}", image_dir.display()))?;

    // The firmware is fetched once into `target/ovmf`. The code drive is read-only and
    // shared, but the vars drive is copied per run: QEMU writes boot variables back into
    // it, and a shared copy makes every run depend on the ones before it (ADR-009).
    let firmware = Prebuilt::fetch(Source::LATEST, workspace_root().join("target/ovmf"))
        .map_err(|e| anyhow!("failed to fetch OVMF firmware: {e:?}"))?;
    let code = firmware.get_file(Arch::X64, FileType::Code).to_path_buf();
    let vars_source = firmware.get_file(Arch::X64, FileType::Vars).to_path_buf();
    let vars = image_dir.join("ovmf-vars.fd");
    fs::copy(&vars_source, &vars).with_context(|| {
        format!(
            "failed to copy {} to {}",
            vars_source.display(),
            vars.display()
        )
    })?;

    let disk = ensure_disk(&image_dir)?;

    let status = Command::new("qemu-system-x86_64")
        .args(["-m", "512M"])
        // Four CPUs, of which the kernel starts exactly one (ADR-037). The number is not
        // decoration: the boot log's ACPI line has to say four, and a machine booted with
        // fewer says fewer, which is what makes that line evidence rather than a constant.
        .args(["-smp", "4"])
        .args(["-serial", "stdio"]) // the kernel's serial log appears in this terminal
        .arg("-no-reboot") // stop on a triple fault instead of reboot-looping
        // No NIC: a card changes the firmware's boot order and costs a DHCP wait.
        .args(["-nic", "none"])
        // Lets the kernel hand a pass/fail status back to this process.
        .args(["-device", "isa-debug-exit,iobase=0xf4,iosize=0x04"])
        .args([
            "-drive",
            &format!("format=raw,if=pflash,readonly=on,file={}", code.display()),
            "-drive",
            &format!("format=raw,if=pflash,file={}", vars.display()),
            // The boot image goes on the secondary channel's master (index 2), leaving the
            // primary master free for the data disk below. OVMF enumerates every block
            // device and finds the ESP wherever it is.
            "-drive",
            &format!(
                "format=raw,if=ide,index=2,media=disk,file={}",
                env!("UEFI_IMAGE")
            ),
            // The data disk is the primary master, which is where `disk::Pio::probe` looks.
            // `index=0` is the master and `media=disk` stops QEMU from guessing a CD-ROM
            // from the image's size.
            "-drive",
            &format!(
                "format=raw,if=ide,index=0,media=disk,file={}",
                disk.display()
            ),
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

/// The data disk's path, created empty if it is not there.
///
/// Without this the machine boots with no disk - a supported state and not the one the
/// README describes - because the kernel formats whatever primary master it is given
/// rather than providing one.
fn ensure_disk(image_dir: &Path) -> Result<PathBuf> {
    let path = image_dir.join(DISK_IMAGE);
    if !path.exists() {
        let file = fs::File::create(&path)
            .with_context(|| format!("failed to create {}", path.display()))?;
        file.set_len(DISK_BYTES)
            .with_context(|| format!("failed to size {}", path.display()))?;
    }
    Ok(path)
}

/// Absolute path to the workspace root, baked in at compile time.
///
/// `cargo run` does not promise the working directory of the process it starts, so a
/// relative `target/` resolves differently depending on who launched the tool (ADR-016).
/// This package is the workspace root, so its manifest directory is one.
fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}
