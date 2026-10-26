//! Build, run and test automation for Grey OS.
//!
//! Driven through the `xtask` alias in `.cargo/config.toml`:
//!
//! ```text
//! cargo xtask build   # stage both images in target/grey-os/
//! cargo xtask run     # build, then boot the UEFI image in QEMU
//! cargo xtask test    # build, boot clean, then boot a kernel built to panic
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
/// The same encoding for `QemuExitCode::Failed` (0x11), which the panic path sends.
const EXIT_FAILURE: i32 = (0x11 << 1) | 1;
/// Printed by the kernel once the M0 boot sequence has finished.
const BOOT_OK: &str = "grey-os: boot ok";
/// Variable that makes `kernel/build.rs` compile the injected panic in (ADR-023).
const PANIC_TEST_ENV: &str = "GREY_OS_PANIC_TEST";
/// Printed by the panic handler ahead of the message and its location.
const PANIC_MARKER: &str = "[panic]";
/// The file the report has to name. Line numbers move whenever the file is edited, so
/// only the path is asserted: the criterion is that a location is reported at all.
const PANIC_LOCATION: &str = "panicked at kernel/src/main.rs";
/// The message the injected panic carries; must match the one in `kernel/src/main.rs`.
const PANIC_MESSAGE: &str = "injected panic for the panic-path test";
/// Proof that the panic path reached the framebuffer. Nothing outside the guest can
/// see the screen, so the kernel reports the paint on serial and this asserts it.
const PANIC_PAINTED: &str = "[panic] panel painted red";
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
    // `test` flips `PANIC_TEST_ENV` and therefore has to build twice more; agreeing to
    // `--no-build` and then building anyway would be a lie, and skipping the panic
    // phase would quietly weaken the test.
    if command == "test" && !options.build {
        bail!(
            "`cargo xtask test` has to build: it boots the image twice, the second time \
             with {PANIC_TEST_ENV} set\n{USAGE}"
        );
    }

    if options.build {
        build(&[])?;
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
/// `environment` is handed to cargo and reaches `kernel/build.rs`, which reads
/// `GREY_OS_PANIC_TEST`. That script's `rerun-if-env-changed` is what makes a flipped
/// flag produce a new kernel instead of reusing the cached one - cargo cannot see the
/// variable on its own, and a stale artifact here would make the panic test pass
/// against a kernel that never panics.
///
/// Spawning `cargo` from an xtask is safe because cargo releases its build lock
/// before executing the binary. `--no-build` is the escape hatch in case that
/// ever stops being true.
fn build(environment: &[(&str, &str)]) -> Result<()> {
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| OsString::from("cargo"));
    let mut command = Command::new(cargo);
    command
        .args(["build", "--package", "grey-os"])
        .current_dir(workspace_root());
    for (key, value) in environment {
        command.env(key, value);
    }
    let status = command.status().context("failed to run cargo build")?;
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

/// Boots headless twice and asserts both halves of the M0 acceptance criteria.
///
/// The first boot is the normal image: it must print `grey-os: boot ok` and exit 33.
/// The second is a kernel rebuilt with [`PANIC_TEST_ENV`] set: it must report
/// `[panic]`, its location, its message and the painted panel, then exit 35. Asserting
/// both is the point - a flag that does not take effect would pass a test that only
/// checked the clean boot, and a kernel that always panics would fail the first phase.
fn test(options: &Options) -> Result<ExitCode> {
    let (log, code) = boot_headless(options)?;
    expect(&log, code, &[BOOT_OK], EXIT_CLEAN)?;
    println!("pass: `{BOOT_OK}` seen on serial and qemu exited {EXIT_CLEAN}");

    let panic_phase = panic_path(options);
    // Restore the image the other commands boot whether the phase passed or not:
    // leaving `target/` holding a kernel that panics on every boot would make the next
    // `cargo xtask run` baffling to debug.
    let restored = build(&[]);
    panic_phase?;
    restored?;
    Ok(ExitCode::SUCCESS)
}

/// Builds a kernel whose only job is to panic, boots it, and asserts the report.
fn panic_path(options: &Options) -> Result<()> {
    println!("--- panic path: rebuilding the kernel with {PANIC_TEST_ENV}=1");
    build(&[(PANIC_TEST_ENV, "1")])?;
    let (log, code) = boot_headless(options)?;
    expect(
        &log,
        code,
        &[PANIC_MARKER, PANIC_LOCATION, PANIC_MESSAGE, PANIC_PAINTED],
        EXIT_FAILURE,
    )?;
    // A cached artifact is the one way this phase can pass without testing anything,
    // so say which failure it is rather than leaving a bare mismatch.
    if log.contains(BOOT_OK) {
        bail!(
            "the kernel reported `{BOOT_OK}` as well, so the injected panic did not \
             take effect; is the kernel artifact stale?\n--- serial ---\n{log}"
        );
    }
    println!(
        "pass: `{PANIC_MARKER}` with its location, the message and the painted panel \
         seen on serial, qemu exited {EXIT_FAILURE}"
    );
    Ok(())
}

/// Boots headless, returning the combined serial log and QEMU's exit code.
///
/// The timeout is not decoration: a kernel that hangs has to fail the test instead of
/// hanging whatever runs it, and a QEMU that never exits is exactly what a broken exit
/// path looks like from here.
fn boot_headless(options: &Options) -> Result<(String, Option<i32>)> {
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
    Ok((log, output.status.code()))
}

/// Asserts that every entry in `needles` appears in the serial log and that QEMU
/// exited with `expected`.
///
/// Both are checked on purpose: QEMU exiting cleanly having printed nothing, and a
/// kernel that prints its marker and then dies, are both failures.
fn expect(log: &str, code: Option<i32>, needles: &[&str], expected: i32) -> Result<()> {
    for needle in needles {
        if !log.contains(needle) {
            bail!("serial log never reported `{needle}`\n--- serial ---\n{log}");
        }
    }
    if code != Some(expected) {
        bail!("qemu exited {code:?}, expected {expected}\n--- serial ---\n{log}");
    }
    Ok(())
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
