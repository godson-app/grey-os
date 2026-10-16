//! Builds the kernel (via the artifact dependency) and wraps it in
//! bootable UEFI and BIOS disk images.

use anyhow::{Context, Result};
use bootloader::DiskImageBuilder;
use std::{env, path::PathBuf};

fn main() -> Result<()> {
    println!("cargo::rerun-if-changed=build.rs");

    let kernel_path = env::var("CARGO_BIN_FILE_KERNEL")
        .context("CARGO_BIN_FILE_KERNEL not set; is the artifact dependency configured?")?;
    let out_dir = PathBuf::from(env::var("OUT_DIR").context("OUT_DIR not set")?);

    let builder = DiskImageBuilder::new(PathBuf::from(kernel_path));
    let uefi_path = out_dir.join("grey-os-uefi.img");
    let bios_path = out_dir.join("grey-os-bios.img");

    builder
        .create_uefi_image(&uefi_path)
        .context("failed to create UEFI image")?;
    builder
        .create_bios_image(&bios_path)
        .context("failed to create BIOS image")?;

    println!("cargo::rustc-env=UEFI_IMAGE={}", uefi_path.display());
    println!("cargo::rustc-env=BIOS_IMAGE={}", bios_path.display());
    Ok(())
}
