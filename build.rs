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

    // Publish the images at a stable path as well: a runner cannot know the
    // hashed build-script output directory, and the images must stay inside the
    // project-local target directory (they are never written to a real device).
    let target_dir = out_dir
        .ancestors()
        .skip_while(|path| !path.ends_with("build"))
        .nth(2) // up two from `build` = target dir
        .context("OUT_DIR is not <target>/<profile>/build/<package>/out")?
        .to_path_buf();
    let image_dir = target_dir.join("grey-os");
    std::fs::create_dir_all(&image_dir).context("failed to create the image directory")?;
    for (source, name) in [
        (&uefi_path, "grey-os-uefi.img"),
        (&bios_path, "grey-os-bios.img"),
    ] {
        std::fs::copy(source, image_dir.join(name)).with_context(|| {
            format!(
                "failed to copy {} into {}",
                source.display(),
                image_dir.display()
            )
        })?;
    }
    println!("cargo::rustc-env=GREY_OS_IMAGE_DIR={}", image_dir.display());
    println!("cargo::rustc-env=UEFI_IMAGE={}", uefi_path.display());
    println!("cargo::rustc-env=BIOS_IMAGE={}", bios_path.display());
    Ok(())
}
