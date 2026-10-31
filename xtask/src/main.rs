//! Build, run and test automation for Grey OS.
//!
//! Driven through the `xtask` alias in `.cargo/config.toml`:
//!
//! ```text
//! cargo xtask build   # stage both images in target/grey-os/
//! cargo xtask run     # build, then boot the UEFI image in QEMU
//! cargo xtask test    # build, boot clean, then boot a kernel per injected failure
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

/// The data disk `greyfs` lives on (M5). Created empty and never anywhere but under
/// `target/`: the kernel formats it on first boot, which is what makes that boot's log
/// line evidence that formatting works.
const DISK_IMAGE: &str = "grey-os-disk.img";
/// Its size: 4 MiB, 8192 sectors. Small enough that every transfer a boot makes is quick,
/// large enough for the directory, the boot counter and, from M9, a few programs.
const DISK_BYTES: u64 = 4 * 1024 * 1024;
/// Printed once the ATA driver has identified the primary master.
const ATA_DISK: &str = "grey-os: ata: primary master";
/// Printed when the kernel found nothing on the disk and formatted it.
const GREYFS_FORMATTED: &str = "grey-os: greyfs: no filesystem, formatted";
/// Printed when the kernel mounted a filesystem that was already there. The generation
/// number in the line is what proves an earlier boot wrote it.
const GREYFS_MOUNTED: &str = "grey-os: greyfs: mounted generation";
/// Printed after the filesystem's write was read back and compared.
const GREYFS_WRITE: &str = "grey-os: greyfs: wrote /boot-count";
/// The end of the storage bring-up. The number after it is the boot count the disk held,
/// which is the whole persistence claim: `boot 1` means the counter was written, `boot 2`
/// means a second boot read it back from the platter.
const STORAGE_OK: &str = "grey-os: storage ok: boot";

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
/// Variable that picks one injected CPU fault for `kernel/build.rs` to compile in (ADR-028).
const EXCEPTION_TEST_ENV: &str = "GREY_OS_EXCEPTION_TEST";
/// Printed once the frame allocator has counted the frames the memory map offers (M3).
const FRAME_TOTALS: &str = "grey-os: frames:";
/// Printed once the kernel has switched to tables it built itself, naming both roots (M3).
/// The kernel refuses to continue if the two are equal, so naming them is the whole claim.
const PAGE_TABLES: &str = "grey-os: page tables: inherited root";
/// Printed once the page walk has resolved the framebuffer's frames (M3).
const FRAMEBUFFER_PHYSICAL: &str = "grey-os: framebuffer physical";
/// Printed once the heap names the first page above its growth frontier (M3, then M4: the guard is
/// the first page the arena never mapped rather than one taken away, ADR-031).
const HEAP_GUARD: &str = "grey-os: heap guard armed";
/// Printed once the arena has grown, then handed pages back, with the frame allocator's count
/// having to agree with both (M4).
const HEAP_CHURN: &str = "grey-os: heap churn ok";
/// Printed once a double free, a pointer from outside the arena and a wrong size have all been
/// refused and counted (M4).
const HEAP_FREES: &str = "grey-os: heap frees:";
/// Printed once a free at the top of the arena gave whole pages back to the frame allocator (M4).
const HEAP_TRIM: &str = "grey-os: heap trim ok";
/// Printed once a `Vec` and a `String` have been built on the arena and read back (M4).
const CONTAINERS: &str = "grey-os: containers ok";
/// Printed once M4's mapper has given a frame an address nothing mapped, written through that
/// address and read the frame back through the window, then taken the mapping away again (M4).
const MAPPING_PROBE: &str = "grey-os: mapping probe ok";
/// One injected fault: what the variable is set to, the line the kernel prints before the
/// faulting instruction, and the report the handler has to produce.
struct Fault {
    /// `gp`, `pf`, `ud`, `df` or `guard` - the values `kernel/build.rs` accepts.
    value: &'static str,
    /// Printed by `kernel/src/main.rs` ahead of the fault. Asserted because it is the only
    /// proof the flag reached the kernel rather than a stale image being booted.
    announcement: &'static str,
    /// The handler's report, up to the point where the values differ from run to run.
    report: &'static str,
}
/// Every fault `test` injects, one image each. The report text is matched, not the faulting
/// address: where the instruction lands depends on the build, and the criterion is that the
/// handler names the vector, its error code and where execution was.
const FAULTS: [Fault; 5] = [
    Fault {
        value: "gp",
        announcement: "GREY_OS_EXCEPTION_TEST=gp, loading a selector that is not in the gdt",
        report: "[exception] #GP general protection fault: error 0x28",
    },
    Fault {
        value: "pf",
        announcement: "GREY_OS_EXCEPTION_TEST=pf, reading an address nothing maps",
        report: "[exception] #PF page fault: error 0x0 on a read to a not-present page",
    },
    Fault {
        value: "ud",
        announcement: "GREY_OS_EXCEPTION_TEST=ud, executing an undefined instruction",
        report: "[exception] #UD invalid opcode: no error code",
    },
    Fault {
        value: "df",
        announcement: "GREY_OS_EXCEPTION_TEST=df, faulting while a fault is delivered",
        report: "[exception] #DF double fault: error 0x0",
    },
    Fault {
        value: "guard",
        announcement: "GREY_OS_EXCEPTION_TEST=guard, writing one byte past the heap",
        // Error 0x2: a write (bit 1) to a page with no mapping (bit 0 clear). The arena's guard is
        // the first page above its growth frontier, which nothing ever mapped (ADR-031).
        report: "[exception] #PF page fault: error 0x2 on a write to a not-present page",
    },
];
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
    // `test` flips `PANIC_TEST_ENV` and `EXCEPTION_TEST_ENV` and therefore has to rebuild for
    // every phase; agreeing to `--no-build` and then building anyway would be a lie, and
    // skipping a phase would quietly weaken the test.
    if command == "test" && !options.build {
        bail!(
            "`cargo xtask test` has to build: it boots the image once per failure, rebuilding \
             with {PANIC_TEST_ENV} and then {EXCEPTION_TEST_ENV} set\n{USAGE}"
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

/// Boots headless once per M0/M2/M3 failure path and asserts every one of them.
///
/// The first boot is the normal image: it must print `grey-os: boot ok` and exit 33. That run
/// also carries the memory claims - frame totals, the framebuffer's resolved span, the kernel's
/// own page-table root, the armed heap guard, the arena's churn, refusals and trim, the containers
/// and the mapping round trip - because they are all lines a successful boot prints and a broken
/// one does not. Then one image per injected failure - a panic, and each fault in [`FAULTS`] - each
/// of which must report itself and exit 35. Asserting both halves is the point: a flag that does
/// not take effect would pass a test that only checked the clean boot, and a kernel that always
/// panics would fail the first phase.
fn test(options: &Options) -> Result<ExitCode> {
    // A deterministic disk: the clean boot has to format, and the persistence phase that
    // follows has to find what it wrote. Leaving an old counter behind would make the
    // first `boot N` line depend on how many times `test` had run before.
    wipe_disk()?;
    let (log, code) = boot_headless(options)?;
    expect(
        &log,
        code,
        &[
            BOOT_OK,
            FRAME_TOTALS,
            PAGE_TABLES,
            FRAMEBUFFER_PHYSICAL,
            HEAP_GUARD,
            HEAP_CHURN,
            HEAP_FREES,
            HEAP_TRIM,
            CONTAINERS,
            MAPPING_PROBE,
            ATA_DISK,
            GREYFS_FORMATTED,
            GREYFS_WRITE,
            &format!("{STORAGE_OK} 1"),
        ],
        EXIT_CLEAN,
    )?;
    println!(
        "pass: `{BOOT_OK}` and the memory bring-up lines seen on serial, qemu exited {EXIT_CLEAN}"
    );
    println!("pass: `{ATA_DISK}` driven, `{GREYFS_FORMATTED}` on a blank disk and the counter written back");

    storage_persistence(options)?;

    let phases = panic_path(options).and_then(|()| exception_paths(options));
    // Restore the image the other commands boot whether the phases passed or not:
    // leaving `target/` holding a kernel that faults on every boot would make the next
    // `cargo xtask run` baffling to debug.
    let restored = build(&[]);
    phases?;
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

/// Boots the same disk a second time and asserts the filesystem came back.
///
/// This is the half of M5 that a single boot cannot show: written sectors are only a
/// filesystem if the next mount reads them, and a kernel that silently re-formatted would
/// pass every other assertion here. So the formatted line must be *absent*, the mount
/// must name the generation the first boot left, and the counter must have moved on.
fn storage_persistence(options: &Options) -> Result<()> {
    println!("--- storage persistence: booting the disk the clean boot wrote");
    let (log, code) = boot_headless(options)?;
    expect(
        &log,
        code,
        &[
            BOOT_OK,
            ATA_DISK,
            GREYFS_MOUNTED,
            GREYFS_WRITE,
            &format!("{STORAGE_OK} 2"),
            &format!("{GREYFS_MOUNTED} 1"),
        ],
        EXIT_CLEAN,
    )?;
    if log.contains(GREYFS_FORMATTED) {
        bail!(
            "the second boot formatted the disk again instead of mounting it, so nothing \
             was persisted\n--- serial ---\n{log}"
        );
    }
    println!(
        "pass: `{GREYFS_MOUNTED} 1`, the counter read back as `boot 2` and no format on the \
         second boot"
    );
    Ok(())
}

///
/// Each iteration rebuilds the kernel with [`EXCEPTION_TEST_ENV`] set to a different value, so
/// what is booted is a kernel whose only job is that fault. The announcement line is asserted
/// alongside the report: it is what rules out a cached image from the previous phase passing
/// this one.
fn exception_paths(options: &Options) -> Result<()> {
    for fault in FAULTS {
        println!(
            "--- exception path: rebuilding the kernel with {EXCEPTION_TEST_ENV}={}",
            fault.value
        );
        build(&[(EXCEPTION_TEST_ENV, fault.value)])?;
        let (log, code) = boot_headless(options)?;
        expect(
            &log,
            code,
            &[fault.announcement, fault.report, PANIC_PAINTED],
            EXIT_FAILURE,
        )?;
        println!(
            "pass: `{}` seen on serial, qemu exited {EXIT_FAILURE}",
            fault.report
        );
    }
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
            // The boot image goes on the secondary channel's master (index 2), leaving
            // the primary master free for the data disk below. OVMF enumerates every
            // block device and finds the ESP wherever it is.
            "-drive",
            &format!(
                "format=raw,if=ide,index=2,media=disk,file={}",
                image_path(UEFI_IMAGE).display()
            ),
        ]);
    } else {
        // BIOS: the firmware boots the raw disk directly, so no pflash drives.
        // Built but not covered by `test`; see ROADMAP.md.
        command.args([
            "-drive",
            &format!(
                "format=raw,if=ide,index=2,media=disk,file={}",
                image_path(BIOS_IMAGE).display()
            ),
        ]);
    }

    // The data disk (M5): the primary master of the machine's first IDE channel, which is
    // where `disk::Pio::probe` looks. `index=0` is the master, and `media=disk` stops QEMU
    // from guessing a CD-ROM from the image's size.
    command.args([
        "-drive",
        &format!(
            "format=raw,if=ide,index=0,media=disk,file={}",
            ensure_disk()?.display()
        ),
    ]);

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

/// The data disk's path, creating it if it is not there.
///
/// A missing disk would make the kernel print "no disk" and carry on, which is a state the
/// boot tests would then have to treat as a failure of the driver rather than of the
/// harness. Creating it here keeps the two apart.
fn ensure_disk() -> Result<PathBuf> {
    fs::create_dir_all(image_dir())
        .with_context(|| format!("failed to create {}", image_dir().display()))?;
    let path = image_path(DISK_IMAGE);
    if !path.exists() {
        let file = fs::File::create(&path)
            .with_context(|| format!("failed to create {}", path.display()))?;
        file.set_len(DISK_BYTES)
            .with_context(|| format!("failed to size {}", path.display()))?;
    }
    Ok(path)
}

/// Throws the data disk away and creates a blank one.
///
/// Only ever the file under `target/`: the one rule this repository makes about disks is
/// that nothing outside that directory is written.
fn wipe_disk() -> Result<PathBuf> {
    let path = image_path(DISK_IMAGE);
    match fs::remove_file(&path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(error).with_context(|| format!("failed to remove {}", path.display()))
        }
    }
    ensure_disk()
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
