//! Build, run and test automation for Grey OS.
//!
//! Driven through the `xtask` alias in `.cargo/config.toml`:
//!
//! ```text
//! cargo xtask build   # stage both images in target/grey-os/
//! cargo xtask run     # build, then boot the UEFI image in QEMU
//! cargo xtask test    # build, then boot headless and assert a clean exit
//! ```
//!
//! The QEMU command line lives here as well as in `src/bin/qemu-uefi.rs`: the
//! runner is for a human watching a window, this is what the test run uses, and
//! merging them is tracked in `DECISIONS.md`.

use anyhow::{anyhow, bail, Context, Result};
use ovmf_prebuilt::{Arch, FileType, Prebuilt, Source};
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, ExitStatus, Stdio};
use std::time::{Duration, Instant};

/// Where `build.rs` stages the images, relative to the workspace root.
const IMAGE_DIR: &str = "target/grey-os";
const UEFI_IMAGE: &str = "grey-os-uefi.img";
const BIOS_IMAGE: &str = "grey-os-bios.img";

const USAGE: &str = "usage: cargo xtask <build|run|test> [--no-build] [--bios]";

/// QEMU's `isa-debug-exit` reports `(value << 1) | 1`; the kernel sends 0x10.
const EXIT_CLEAN: i32 = (0x10 << 1) | 1;
/// Printed by the kernel once the M0 boot sequence has finished.
const BOOT_OK: &str = "grey-os: boot ok";
/// Upper bound on a `test` run, so a hung boot fails instead of hanging CI.
const TEST_TIMEOUT: Duration = Duration::from_secs(120);

struct Options {
    /// Run `cargo build` first; `--no-build` skips it.
    build: bool,
    /// Boot the UEFI image (default); `--bios` picks the BIOS one.
    uefi: bool,
}

fn main() -> Result<ExitCode> {
    let mut arguments = std::env::args().skip(1);
    let command = arguments.next().unwrap_or_else(|| String::from("run"));

    let mut options = Options {
        build: true,
        uefi: true,
    };
    for argument in arguments {
        match argument.as_str() {
            "--no-build" => options.build = false,
            "--bios" => options.uefi = false,
            other => bail!("unknown option `{other}`\n{USAGE}"),
        }
    }
    if !matches!(command.as_str(), "build" | "run" | "test") {
        bail!("unknown command `{command}`\n{USAGE}");
    }

    if options.build {
        build()?;
    }

    match command.as_str() {
        "build" => {
            println!("images staged in {IMAGE_DIR}/");
            Ok(ExitCode::SUCCESS)
        }
        "run" => run(&options),
        _ => test(&options),
    }
}

/// Runs the image builder, whose `build.rs` stages both images into `IMAGE_DIR`.
///
/// Spawning `cargo` from an xtask is safe because cargo releases its build lock
/// before executing the binary. `--no-build` is the escape hatch in case that
/// ever stops being true.
fn build() -> Result<()> {
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| OsString::from("cargo"));
    let status = Command::new(cargo)
        .args(["build", "--package", "grey-os"])
        .current_dir(workspace_root())
        .status()
        .context("failed to run cargo build")?;
    if !status.success() {
        bail!("cargo build failed; the images were not staged");
    }
    for name in [UEFI_IMAGE, BIOS_IMAGE] {
        let path = image_path(name);
        if !path.exists() {
            bail!(
                "{} is missing after a successful build; {} contains [{}]",
                path.display(),
                image_dir().display(),
                list_dir(&image_dir())
            );
        }
    }
    Ok(())
}

/// Boots QEMU with a window and inherited stdio, then maps its exit status.
fn run(options: &Options) -> Result<ExitCode> {
    let status = boot_command(options, false)?
        .status()
        .context("failed to run qemu-system-x86_64")?;
    interpret(status)
}

/// Boots headless and asserts both the serial marker and the exit status.
///
/// Both are checked on purpose: QEMU exiting cleanly having printed nothing, and
/// a kernel that prints `boot ok` and then dies, are both failures.
fn test(options: &Options) -> Result<ExitCode> {
    let mut command = boot_command(options, true)?;
    println!("boot command: {command:?}");
    let mut child = command
        .spawn()
        .context("failed to start qemu-system-x86_64")?;

    let started = Instant::now();
    loop {
        if child.try_wait().context("failed to poll qemu")?.is_some() {
            break;
        }
        if started.elapsed() > TEST_TIMEOUT {
            let _ = child.kill();
            let _ = child.wait();
            bail!(
                "boot did not finish within {}s; the kernel is hung",
                TEST_TIMEOUT.as_secs()
            );
        }
        std::thread::sleep(Duration::from_millis(50));
    }

    let output = child
        .wait_with_output()
        .context("failed to collect qemu output")?;
    let mut log = String::from_utf8_lossy(&output.stdout).into_owned();
    log.push_str(&String::from_utf8_lossy(&output.stderr));

    if !log.contains(BOOT_OK) {
        bail!("serial log never reported `{BOOT_OK}`\n--- serial ---\n{log}");
    }
    if output.status.code() != Some(EXIT_CLEAN) {
        bail!(
            "qemu exited with {:?}, expected {EXIT_CLEAN} for a clean boot\n--- serial ---\n{log}",
            output.status.code()
        );
    }
    println!("pass: `{BOOT_OK}` seen on serial and qemu exited {EXIT_CLEAN}");
    Ok(ExitCode::SUCCESS)
}

/// Maps QEMU's exit status onto ours.
fn interpret(status: ExitStatus) -> Result<ExitCode> {
    match status.code() {
        Some(code) if code == EXIT_CLEAN => Ok(ExitCode::SUCCESS),
        Some(code) => {
            bail!("qemu exited {code} (33 means a clean boot, 35 a kernel-reported failure)")
        }
        None => bail!("qemu was killed by a signal"),
    }
}

/// Builds the QEMU command line shared by `run` and `test`.
fn boot_command(options: &Options, capture: bool) -> Result<Command> {
    let mut command = Command::new("qemu-system-x86_64");
    command.args(["-m", "512M"]);
    // Stop on a triple fault instead of reboot-looping forever.
    command.arg("-no-reboot");
    // Lets the kernel report a pass/fail status back to this process.
    command.args(["-device", "isa-debug-exit,iobase=0xf4,iosize=0x04"]);
    // No network: a NIC changes the firmware's boot order and costs a DHCP wait.
    command.args(["-nic", "none"]);
    command.args(["-serial", "stdio"]);

    if capture {
        command.args(["-display", "none"]);
        command.stdout(Stdio::piped());
        command.stderr(Stdio::piped());
    }

    if options.uefi {
        let (code, vars) = firmware()?;
        command.args([
            "-drive",
            &format!("format=raw,if=pflash,readonly=on,file={}", code.display()),
            "-drive",
            &format!("format=raw,if=pflash,file={}", vars.display()),
            "-drive",
            &format!("format=raw,file={}", image_path(UEFI_IMAGE).display()),
        ]);
    } else {
        // BIOS: the firmware boots the raw disk directly, so no pflash drives.
        // Built but not covered by `test`; see ROADMAP.md.
        command.args([
            "-drive",
            &format!("format=raw,file={}", image_path(BIOS_IMAGE).display()),
        ]);
    }

    Ok(command)
}

/// Resolves the OVMF firmware for a UEFI boot.
///
/// The code drive is read-only and shared, but the vars drive is copied per run:
/// QEMU writes boot variables back into it, and a shared copy makes every run
/// depend on the ones before it.
fn firmware() -> Result<(PathBuf, PathBuf)> {
    let prebuilt = Prebuilt::fetch(Source::LATEST, workspace_root().join("target/ovmf"))
        .map_err(|error| anyhow!("failed to fetch OVMF firmware: {error:?}"))?;
    // `to_path_buf` keeps this working whether the API returns `Path` or `PathBuf`.
    let code = prebuilt.get_file(Arch::X64, FileType::Code).to_path_buf();
    let vars_source = prebuilt.get_file(Arch::X64, FileType::Vars).to_path_buf();

    fs::create_dir_all(image_dir())
        .with_context(|| format!("failed to create {}", image_dir().display()))?;
    let vars = image_path("ovmf-vars.fd");
    fs::copy(&vars_source, &vars).with_context(|| {
        format!(
            "failed to copy {} to {}",
            vars_source.display(),
            vars.display()
        )
    })?;
    Ok((code, vars))
}

/// Absolute path to the workspace root, baked in at compile time.
///
/// `cargo run` does not promise the working directory of the process it starts,
/// so a relative `target/grey-os` resolves to different directories depending on
/// who launched the tool. That is what made the first `cargo xtask test` report
/// the images missing immediately after building them.
fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .map_or_else(|| PathBuf::from("."), Path::to_path_buf)
}

fn image_dir() -> PathBuf {
    workspace_root().join(IMAGE_DIR)
}

fn image_path(name: &str) -> PathBuf {
    image_dir().join(name)
}

/// Comma-separated directory listing, so a failure can name what it did find.
fn list_dir(directory: &Path) -> String {
    match fs::read_dir(directory) {
        Ok(entries) => {
            let mut names: Vec<String> = entries
                .filter_map(|entry| entry.ok())
                .map(|entry| entry.file_name().to_string_lossy().into_owned())
                .collect();
            names.sort();
            names.join(", ")
        }
        Err(error) => format!("unreadable: {error}"),
    }
}
