//! Build, run and test automation for Grey OS.
//!
//! Driven through the `xtask` alias in `.cargo/config.toml`:
//!
//! ```text
//! cargo xtask build   # stage both images in target/grey-os/
//! cargo xtask run     # build, then boot the UEFI image in QEMU
//! cargo xtask run --interactive   # the same boot, but the shell stays up (ADR-041)
//! cargo xtask test    # build, boot clean, then boot a kernel per injected failure
//! cargo xtask ci      # every gate in one command: fmt, lints, host tests, boot tests
//! ```
//!
//! The QEMU command line lives here as well as in `src/bin/qemu-uefi.rs`: the
//! runner is for a human watching a window, this is what the test run uses, and
//! merging them is tracked in `DECISIONS.md`.

use anyhow::{anyhow, bail, Context, Result};
use ovmf_prebuilt::{Arch, FileType, Prebuilt, Source};
use std::ffi::OsString;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, ExitStatus, Stdio};
use std::sync::mpsc;
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

/// Variable that makes `kernel/build.rs` compile the listening input run in (M6).
const INPUT_TEST_ENV: &str = "GREY_OS_INPUT_TEST";
/// Variable that makes `kernel/build.rs` compile the preview in (ADR-041): the shell, with no
/// tick deadline. Only `run --interactive` sets it, because a boot test would wait for a
/// session that nobody is there to end.
const INTERACTIVE_ENV: &str = "GREY_OS_INTERACTIVE";
/// Printed once the input test is listening. The harness types only after it sees this:
/// keystrokes sent before the 8042 is configured are dropped by the controller.
const INPUT_LISTENING: &str = "grey-os: input test: listening";
/// Printed once the input run has stopped listening.
const INPUT_OK: &str = "grey-os: input ok:";
/// The keys the harness types, in order. Each one has to appear in a `key: 'x'` line, so
/// the word is the assertion.
const INPUT_KEYS: [&str; 4] = ["g", "r", "e", "y"];
/// The commands the harness types into the shell once it says it is listening (M8). `exit`
/// ends the session, which is what lets the phase finish rather than wait for its timeout.
/// `cpus` and the two `run`s are M10: the first asks the kernel what the firmware said, the
/// second runs a program off the disk, and the third asks for one that is not there, which has
/// to be a message rather than a fault.
const SHELL_COMMANDS: [&str; 6] = ["ls", "mem", "cpus", "run fib", "run nope", "exit"];
/// Printed by the shell before it starts reading keys.
const SHELL_LISTENING: &str = "grey-os: shell: listening";
/// Printed when the shell's `exit` command ended the session.
const SHELL_EXIT: &str = "grey-os: shell: exit";
/// Printed by the task demonstration (M8).
const SCHED_OK: &str = "grey-os: sched ok:";
/// Printed by the application demonstration (M9): the programs installed on the disk, and what
/// they printed when a task ran them.
const APPS_INSTALLED: &str = "grey-os: apps: 3 program(s) written this boot";
const APPS_OK: &str = "grey-os: apps ok:";
const APP_HELLO: &str = "grey-os: app: hello from apps/hello.gvm";
const APP_COUNT: &str = "grey-os: app: 1 2 3 4 5";
const APP_FIB: &str = "grey-os: app: 0 1 1 2 3 5 8 13 21 34";
const APP_HALTED: &str = "halted after";
/// Printed by the firmware report (M10): the RSDP, what the MADT says, and how many CPUs the
/// kernel actually started.
const ACPI_RSDP: &str = "grey-os: acpi: rsdp at";
const ACPI_MADT: &str = "grey-os: acpi: madt: 4 cpu(s), 4 enabled, ioapic at";
const SMP_STARTED: &str = "grey-os: smp: 1 of 4 cpu(s) started";
/// Printed by a normal boot once the keyboard has been configured and its handler installed
/// (M6). The mouse's line follows it, but a machine without a pointer is allowed.
const INPUT_READY: &str = "grey-os: input: keyboard on irq1";

/// The theme's desktop colour, which the screenshot check looks for in the corners.
const DESKTOP_COLOUR: [u8; 3] = [0x1f, 0x6f, 0x8b];
/// A window's body colour, from the same theme. A preview's screenshot is judged on it: the
/// boot log's background is [`DESKTOP_COLOUR`], so a log line painted after the desktop was
/// composited replaces window pixels with the desktop's colour and the body stops matching.
const WINDOW_BODY_COLOUR: [u8; 3] = [0x1a, 0x20, 0x26];
/// The pointer's fill, which has to appear in a frame that drew one.
const POINTER_COLOUR: [u8; 3] = [0xf5, 0xf8, 0xfa];
/// Printed once the desktop has been composited. The harness takes its screenshot after it
/// sees this line, which is the only moment the panel holds the finished desktop.
const COMPOSITOR_OK: &str = "grey-os: compositor ok:";
/// Printed by a normal boot once the theme and the three windows are up (M7).
const THEME_READY: &str = "grey-os: theme";
/// Printed by a normal boot with the window list's shape on it (M7).
const DESKTOP_READY: &str = "grey-os: desktop:";

const USAGE: &str = "usage: cargo xtask <build|run|test|ci> [--no-build] [--bios] [--interactive]";

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
        announcement: "GREY_OS_EXCEPTION_TEST=gp, loading a selector the gdt cannot reach",
        report: "[exception] #GP general protection fault: error 0xfff8",
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
/// Printed by a ring 3 program through the write syscall (P2). The prefix is the kernel's, so a
/// program whose bytes never reached the log cannot make this appear.
const USER_HELLO: &str = "grey-os: user> hello from ring 3";
/// A line from the counting program: three of them, with a sleep between each.
const USER_TICK: &str = "grey-os: user> tick from ring 3";
/// Printed once a program has returned through `iretq`. The exit code, syscall count and byte
/// count are all in the line, and the byte count is the sum of the messages the two programs
/// wrote - so it cannot be right unless every byte crossed the gate.
const USER_HELLO_OK: &str = "grey-os: user ok: hello exited 0, 2 syscalls, 18 bytes";
const USER_COUNT_OK: &str = "grey-os: user ok: count exited 3, 7 syscalls, 51 bytes";
/// The report a ring 3 fault has to produce: a page fault (error 0x6 is user + write +
/// present) on the kernel's own text, which the program's page table maps supervisor-only.
const USER_FAULT: &str = "grey-os: user: #PF page fault at rip";
const USER_FAULT_DETAIL: &str = "error 0x6, address 0x100000000, killed";
/// `128 + 14`, the shell's convention for a fault: the kernel carried the program's death as an
/// exit code rather than panicking.
const USER_FAULT_EXIT: &str = "grey-os: user ok: fault exited 142";

/// Upper bound on a `test` run, so a hung boot fails instead of hanging CI.
const TEST_TIMEOUT: Duration = Duration::from_secs(120);

struct Options {
    /// Run `cargo build` first; `--no-build` skips it.
    build: bool,
    /// Boot the UEFI image (default); `--bios` picks the BIOS one.
    uefi: bool,
    /// Build the preview kernel - the shell with no deadline - and keep the window up
    /// (ADR-041). `run` only: no other command has anybody to type.
    interactive: bool,
}

fn main() -> Result<ExitCode> {
    let mut arguments = std::env::args().skip(1);
    let command = arguments.next().unwrap_or_else(|| String::from("run"));

    let mut options = Options {
        build: true,
        uefi: true,
        interactive: false,
    };
    for argument in arguments {
        match argument.as_str() {
            "--no-build" => options.build = false,
            "--bios" => options.uefi = false,
            "--interactive" => options.interactive = true,
            other => bail!("unknown option `{other}`\n{USAGE}"),
        }
    }
    if !matches!(command.as_str(), "build" | "run" | "test" | "ci") {
        bail!("unknown command `{command}`\n{USAGE}");
    }
    // The preview is a kernel as much as the failure runs are: the flag decides what is
    // compiled, so agreeing to boot a previously built image would boot whatever was there.
    if options.interactive && !options.build {
        bail!(
            "`--interactive` has to build: it compiles the preview in with {INTERACTIVE_ENV}=1, \
             and `--no-build` would boot the image left by the last build\n{USAGE}"
        );
    }
    if options.interactive && command != "run" {
        bail!("`--interactive` is for `run` only: nothing else has a keyboard\n{USAGE}");
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

    if command == "ci" {
        // `ci` owns its build order: the lints and host tests come before the first boot, and
        // the release build comes last. A plain `--bios` run or a skipped build would make it
        // check something other than what CI checks, so both are refused rather than ignored.
        if !options.build || !options.uefi {
            bail!("`cargo xtask ci` takes no options\n{USAGE}");
        }
        return ci();
    }

    if options.build {
        // The preview is the one build that asks for `INTERACTIVE_ENV`; every other build,
        // `test`'s phases included, gets a kernel that ends on its own.
        let environment: &[(&str, &str)] = if options.interactive {
            &[(INTERACTIVE_ENV, "1")]
        } else {
            &[]
        };
        build(environment)?;
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
    // Every build starts from a normal kernel, whatever the caller's shell has exported: an
    // inherited `GREY_OS_INTERACTIVE` would otherwise turn the boot tests into sessions that
    // never exit. A caller that wants the preview says so below, and the last `env` wins.
    command.env(INTERACTIVE_ENV, "0");
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

/// Runs every gate the repository claims, in the order that fails fastest.
///
/// The cheap checks come first on purpose: a formatting slip should cost seconds, not one QEMU
/// boot per phase. The boot tests are the same [`test`] that `cargo xtask test` runs - calling
/// it directly rather than spawning a second xtask keeps one process in charge of the exit
/// status, and the image build is the same `cargo build` either way.
fn ci() -> Result<ExitCode> {
    cargo_step("formatting", &["fmt", "--all", "--", "--check"])?;
    cargo_step(
        "host lints",
        &[
            "clippy",
            "--workspace",
            "--exclude",
            "kernel",
            "--all-targets",
            "--",
            "-D",
            "warnings",
        ],
    )?;
    cargo_step(
        "kernel lints",
        &[
            "clippy",
            "--package",
            "kernel",
            "--target",
            "x86_64-unknown-none",
            "--",
            "-D",
            "warnings",
        ],
    )?;
    cargo_step("host tests", &["test", "--package", "kernel", "--lib"])?;

    println!("grey-os: ci: boot tests");
    let options = Options {
        build: true,
        uefi: true,
        interactive: false,
    };
    let _ = test(&options)?;

    cargo_step(
        "release build",
        &["build", "--release", "--package", "grey-os"],
    )?;

    println!("grey-os: ci ok: every gate passed");
    Ok(ExitCode::SUCCESS)
}

/// Runs one `cargo` command from the workspace root, failing the run when it fails.
///
/// `CARGO` is preferred over the literal name so the step runs the same toolchain that started
/// this xtask; `cargo` is the fallback for a binary run outside cargo.
fn cargo_step(name: &str, arguments: &[&str]) -> Result<()> {
    println!("grey-os: ci: {name}");
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| OsString::from("cargo"));
    let status = Command::new(cargo)
        .args(arguments)
        .current_dir(workspace_root())
        .status()
        .with_context(|| format!("failed to run cargo {}", arguments.join(" ")))?;
    if !status.success() {
        bail!("cargo {} failed", arguments.join(" "));
    }
    Ok(())
}

/// Boots QEMU with a window and inherited stdio, then maps its exit status.
fn run(options: &Options) -> Result<ExitCode> {
    let status = boot_command(options, false)?
        .status()
        .context("failed to run qemu-system-x86_64")?;
    if options.interactive {
        return preview_exit(status);
    }
    interpret(status)
}

/// Maps a preview's exit status, where two different endings are both correct.
///
/// `exit` in the shell is a finished boot and reports 33 like any other. Closing the window is
/// the other way a session ends, and QEMU calls that 0 - a failure to the boot tests, whose
/// whole point is that a kernel ends by itself, but the intended end of a session somebody
/// walked away from.
fn preview_exit(status: ExitStatus) -> Result<ExitCode> {
    match status.code() {
        Some(0) => Ok(ExitCode::SUCCESS),
        _ => interpret(status),
    }
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
            APPS_INSTALLED,
            &format!("{STORAGE_OK} 1"),
            ACPI_RSDP,
            ACPI_MADT,
            SMP_STARTED,
            INPUT_READY,
            "grey-os: input ready:",
            THEME_READY,
            DESKTOP_READY,
            COMPOSITOR_OK,
            SCHED_OK,
            APPS_OK,
            APP_HELLO,
            APP_COUNT,
            APP_FIB,
            APP_HALTED,
            USER_HELLO,
            USER_TICK,
            USER_HELLO_OK,
            USER_COUNT_OK,
            USER_FAULT,
            USER_FAULT_DETAIL,
            USER_FAULT_EXIT,
        ],
        EXIT_CLEAN,
    )?;
    println!(
        "pass: `{BOOT_OK}` and the memory bring-up lines seen on serial, qemu exited {EXIT_CLEAN}"
    );
    println!("pass: `{ATA_DISK}` driven, `{GREYFS_FORMATTED}` on a blank disk and the counter written back");
    println!(
        "pass: `{APPS_INSTALLED}`, then `{APP_COUNT}` and `{APP_FIB}` printed by the interpreter, \
         each program's `{APP_HALTED}` line and `{APPS_OK}` seen"
    );
    println!(
        "pass: `{ACPI_RSDP}`, `{ACPI_MADT}` and `{SMP_STARTED}` seen on serial, so the cpu count came out of the madt"
    );
    println!(
        "pass: `{USER_HELLO}` and `{USER_TICK}` written from ring 3, `{USER_COUNT_OK}` after seven \
         syscalls, and `{USER_FAULT_DETAIL}` for the program that wrote the kernel's text"
    );

    storage_persistence(options)?;

    input_path(options)?;

    preview_path(options)?;

    let phases = panic_path(options).and_then(|()| exception_paths(options));
    // Restore the image the other commands boot whether the phases passed or not:
    // leaving `target/` holding a kernel that faults on every boot would make the next
    // `cargo xtask run` baffling to debug.
    let restored = build(&[]);
    phases?;
    restored?;
    Ok(ExitCode::SUCCESS)
}

/// Boots a kernel that listens for the keyboard, types into it through QEMU's monitor, and
/// asserts what the kernel decoded.
///
/// This is the one phase that needs to talk to a running guest: a kernel cannot press its
/// own keyboard, and accepting the decoder's host tests as proof of the interrupt path
/// would be accepting that a function works because it was called. QEMU's QMP socket is the
/// injection point, and the serial output is watched line by line so the keys are typed
/// only after the kernel says it is listening.
fn input_path(options: &Options) -> Result<()> {
    println!("--- input path: rebuilding the kernel with {INPUT_TEST_ENV}=1");
    build(&[(INPUT_TEST_ENV, "1")])?;

    let socket = image_path("qmp.sock");
    let _ = fs::remove_file(&socket);
    let mut command = boot_command(options, true)?;
    command.args([
        "-qmp",
        &format!("unix:{},server=on,wait=off", socket.display()),
    ]);
    println!("boot command: {command:?}");
    let mut child = command
        .spawn()
        .context("failed to start qemu-system-x86_64")?;
    let stdout = child.stdout.take().context("qemu's stdout was not piped")?;

    // The serial log has to be watched while the guest runs, not collected afterwards: the
    // whole phase depends on knowing when the kernel started listening.
    let (sender, receiver) = mpsc::channel::<String>();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            let Ok(line) = line else { break };
            if sender.send(line).is_err() {
                break;
            }
        }
    });

    let started = Instant::now();
    let mut log = String::new();
    let mut typed = false;
    let mut captured = false;
    let mut typed_shell = false;
    loop {
        match receiver.recv_timeout(Duration::from_millis(100)) {
            Ok(line) => {
                log.push_str(&line);
                log.push('\n');
                if !typed && line.contains(INPUT_LISTENING) {
                    println!(
                        "typing `{}` into the machine's keyboard through qmp",
                        INPUT_KEYS.concat()
                    );
                    inject(&socket)?;
                    typed = true;
                }
                // The desktop exists only while the kernel is alive to draw it, so the
                // screenshot is taken the moment the compositor says it is finished.
                if !captured && line.contains(COMPOSITOR_OK) {
                    let path = image_path("desktop.ppm");
                    screenshot(&socket, &path)?;
                    check_screenshot(&path)?;
                    println!("pass: `{}` and the panel holds a desktop", path.display());
                    captured = true;
                }
                // The shell is listening: type the commands the phase asserts on.
                if !typed_shell && line.contains(SHELL_LISTENING) {
                    println!(
                        "typing `{}` into the shell through qmp",
                        SHELL_COMMANDS.join(", ")
                    );
                    type_commands(&socket)?;
                    typed_shell = true;
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if started.elapsed() > TEST_TIMEOUT {
                    let _ = child.kill();
                    let _ = child.wait();
                    bail!(
                        "the input run did not finish within {}s; the kernel is hung\
                         \n--- serial ---\n{log}",
                        TEST_TIMEOUT.as_secs()
                    );
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    let status = child.wait().context("failed to collect qemu output")?;
    if !typed {
        bail!("the kernel never reported `{INPUT_LISTENING}`\n--- serial ---\n{log}");
    }
    expect(
        &log,
        status.code(),
        &[
            BOOT_OK,
            INPUT_LISTENING,
            &format!("{INPUT_OK} {} keys", INPUT_KEYS.len()),
        ],
        EXIT_CLEAN,
    )?;
    // Each key separately: the count alone would pass if the decoder reported one key four
    // times, which is exactly the failure a stuck scan code produces.
    for key in INPUT_KEYS {
        let needle = format!("grey-os: key: '{key}'");
        if !log.contains(&needle) {
            bail!("the keyboard never reported `{needle}`\n--- serial ---\n{log}");
        }
    }
    println!(
        "pass: `{INPUT_OK} {} keys` with each of `{}` decoded, qemu exited {EXIT_CLEAN}",
        INPUT_KEYS.len(),
        INPUT_KEYS.concat()
    );
    // The pointer is required too: the same console that carries the keystrokes carries the
    // movement, and a three-byte packet is only a packet if the device produced one.
    if !log.contains("grey-os: mouse:") {
        bail!("the pointer never reported a packet\n--- serial ---\n{log}");
    }
    println!("pass: a pointer packet reached the guest and was decoded");
    // The shell: what was typed has to come back out, and `exit` has to end the session.
    if !typed_shell {
        bail!("the kernel never reported `{SHELL_LISTENING}`\n--- serial ---\n{log}");
    }
    for needle in [
        SHELL_EXIT,
        "grey-os: shell: > ls",
        "grey-os: shell: boot-count",
        "grey-os: shell: > mem",
        "grey-os: shell: heap:",
        // M10: the shell asks the firmware report what the machine is, and runs a program the
        // boot's storage bring-up wrote to the disk.
        "grey-os: shell: > cpus",
        "grey-os: shell: 4 cpu(s) in the firmware's tables, 4 enabled, 1 started",
        "grey-os: shell: > run fib",
        "grey-os: shell: 0 1 1 2 3 5 8 13 21 34",
        "grey-os: shell: apps/fib.gvm: halted after",
        "grey-os: shell: > run nope",
        "grey-os: shell: ? apps/nope.gvm: NotFound",
    ] {
        if !log.contains(needle) {
            bail!("the shell never reported `{needle}`\n--- serial ---\n{log}");
        }
    }
    println!(
        "pass: `ls`, `mem`, `cpus`, `run fib`, `run nope` and `exit` typed into the shell, the answers and the program's output on serial, `{SHELL_EXIT}` seen"
    );
    Ok(())
}

/// Boots the preview - the shell with no deadline - photographs it, and ends it with `exit`.
///
/// Two claims the other phases cannot make. The first is the panel: a preview holds the desktop
/// up for as long as somebody is looking at it, so nothing may paint over it after the first
/// frame - and in the first preview something did, the boot log, which turned the desktop into
/// a terminal of log lines with fragments of windows in it. A headless boot cannot see that,
/// and the input path never does: its panel is photographed after the boot has repainted what
/// the log had covered. So this phase screenshots the preview while the shell is listening and
/// measures it, which is the only moment the panel is up and nobody has repainted it.
///
/// The second claim is the ending. A preview's loop has no deadline, so the session lasts until
/// `exit` is typed; that it then reports a finished boot and qemu's clean status is what keeps
/// one exit-status contract for both kinds of run.
fn preview_path(options: &Options) -> Result<()> {
    println!("--- preview: rebuilding the kernel with {INTERACTIVE_ENV}=1");
    build(&[(INTERACTIVE_ENV, "1")])?;

    let socket = image_path("qmp.sock");
    let _ = fs::remove_file(&socket);
    let mut command = boot_command(options, true)?;
    command.args([
        "-qmp",
        &format!("unix:{},server=on,wait=off", socket.display()),
    ]);
    println!("boot command: {command:?}");
    let mut child = command
        .spawn()
        .context("failed to start qemu-system-x86_64")?;
    let stdout = child.stdout.take().context("qemu's stdout was not piped")?;
    // The serial log has to be watched while the guest runs: the panel is photographed on a
    // line, not after the guest exits - in a preview it never exits on its own.
    let (sender, receiver) = mpsc::channel::<String>();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            let Ok(line) = line else { break };
            if sender.send(line).is_err() {
                break;
            }
        }
    });

    let started = Instant::now();
    let mut log = String::new();
    let mut photographed = false;
    loop {
        match receiver.recv_timeout(Duration::from_millis(100)) {
            Ok(line) => {
                log.push_str(&line);
                log.push('\n');
                if !photographed && line.contains(SHELL_LISTENING) {
                    // Nothing is painted between this line and the photograph: the kernel
                    // prints it before it waits for a key, and the loop after it repaints
                    // only what a keystroke or a click damages.
                    let path = image_path("preview.ppm");
                    screenshot(&socket, &path)?;
                    check_preview_screenshot(&path)?;
                    println!("typing `exit` into the preview through qmp");
                    let (mut stream, mut reader) = monitor(&socket)?;
                    type_line(&mut stream, &mut reader, "exit")?;
                    photographed = true;
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if started.elapsed() > TEST_TIMEOUT {
                    let _ = child.kill();
                    let _ = child.wait();
                    bail!(
                        "the preview did not finish within {}s; the kernel is hung\
                         \n--- serial ---\n{log}",
                        TEST_TIMEOUT.as_secs()
                    );
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    let status = child.wait().context("failed to collect qemu output")?;
    if !photographed {
        bail!("the kernel never reported `{SHELL_LISTENING}`\n--- serial ---\n{log}");
    }
    expect(
        &log,
        status.code(),
        &[
            SHELL_LISTENING,
            "grey-os: shell: > exit",
            SHELL_EXIT,
            BOOT_OK,
        ],
        EXIT_CLEAN,
    )?;
    println!(
        "pass: `{SHELL_LISTENING}` with the desktop still on the panel, `exit` typed into it, \
         `{SHELL_EXIT}` and `{BOOT_OK}`, qemu exited {EXIT_CLEAN}"
    );
    Ok(())
}

/// Connects to QEMU's monitor, enables it, and types the keys.
///
/// QMP speaks one JSON object per line, and every command is answered with either a
/// `return` or an `error` object. Reading each answer is not decoration: QEMU processes
/// commands in order, and a command whose reply is not consumed leaves the next reader off
/// by one.
fn inject(socket: &Path) -> Result<()> {
    let (mut stream, mut reader) = monitor(socket)?;
    for key in INPUT_KEYS {
        // QMP has no `sendkey`: it lives in the human monitor, which QMP can relay. The
        // relay is still a keystroke through the machine's keyboard controller - the path a
        // person's key takes - not a call into the kernel.
        qmp(
            &mut stream,
            &mut reader,
            &format!(
                r#"{{"execute":"human-monitor-command","arguments":{{"command-line":"sendkey {key}"}}}}"#
            ),
        )?;
        // One keystroke per call, spaced out: the controller's buffer holds a couple of
        // bytes, and the kernel's queue is drained between them.
        std::thread::sleep(Duration::from_millis(80));
    }
    // A relative movement, which is what a real mouse sends: the axes are relative to where
    // the pointer already is, so the guest reports the same numbers back.
    qmp(
        &mut stream,
        &mut reader,
        r#"{"execute":"input-send-event","arguments":{"events":[{"type":"rel","data":{"axis":"x","value":25}},{"type":"rel","data":{"axis":"y","value":-10}}]}}"#,
    )?;
    Ok(())
}

/// Connects to QEMU's monitor and enables it, returning the connection and its reader.
///
/// QMP speaks one JSON object per line, and every command is answered with either a `return`
/// or an `error` object. Reading each answer is not decoration: QEMU processes commands in
/// order, and a command whose reply is not consumed leaves the next reader off by one.
fn monitor(socket: &Path) -> Result<(UnixStream, BufReader<UnixStream>)> {
    let mut connected = None;
    // The socket file appears as the guest starts and can exist a moment before it accepts a
    // connection, so the connect is retried rather than trusted to be immediate.
    for _ in 0..50 {
        match UnixStream::connect(socket) {
            Ok(stream) => {
                connected = Some(stream);
                break;
            }
            Err(_) => std::thread::sleep(Duration::from_millis(100)),
        }
    }
    let mut stream =
        connected.with_context(|| format!("failed to connect to {}", socket.display()))?;
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;
    let mut reader = BufReader::new(stream.try_clone().context("failed to clone the socket")?);
    let mut greeting = String::new();
    reader
        .read_line(&mut greeting)
        .context("failed to read qemu's qmp greeting")?;
    if !greeting.contains("QMP") {
        bail!("unexpected greeting from qemu's monitor: {greeting}");
    }
    qmp(
        &mut stream,
        &mut reader,
        r#"{"execute":"qmp_capabilities"}"#,
    )?;
    Ok((stream, reader))
}

/// Types each shell command, terminated by Enter.
///
/// One connection for all of them: QEMU's monitor is happy to have several, but a keystroke
/// per round trip keeps the ordering obvious.
fn type_commands(socket: &Path) -> Result<()> {
    let (mut stream, mut reader) = monitor(socket)?;
    for command in SHELL_COMMANDS {
        type_line(&mut stream, &mut reader, command)?;
        // Long enough for the command to run and its answer to be drawn.
        std::thread::sleep(Duration::from_millis(250));
    }
    Ok(())
}

/// Types one line into the guest and presses Enter, on an open monitor connection.
///
/// One keystroke per round trip, spaced out: the controller's buffer holds a couple of bytes
/// and the kernel's queue is drained between them.
fn type_line(
    stream: &mut UnixStream,
    reader: &mut BufReader<UnixStream>,
    line: &str,
) -> Result<()> {
    for key in line.chars() {
        qmp(stream, reader, &sendkey(&key_name(key)))?;
        std::thread::sleep(Duration::from_millis(60));
    }
    qmp(stream, reader, &sendkey("ret"))
}

/// QEMU's name for a character the monitor does not accept as itself.
///
/// `sendkey a` types an `a`; `sendkey ` types nothing, so the three characters this shell's
/// commands need and the keyboard does not have a literal name for are spelled out.
fn key_name(character: char) -> String {
    let name = match character {
        ' ' => "spc",
        '/' => "slash",
        '.' => "dot",
        '-' => "minus",
        other => return other.to_string(),
    };
    String::from(name)
}

/// The QMP command that sends one key through the machine's keyboard.
fn sendkey(name: &str) -> String {
    // QMP has no `sendkey`: it lives in the human monitor, which QMP can relay. The relay is
    // still a keystroke through the keyboard controller - the path a person's key takes - not
    // a call into the kernel.
    format!(
        r#"{{"execute":"human-monitor-command","arguments":{{"command-line":"sendkey {name}"}}}}"#
    )
}

fn screenshot(socket: &Path, path: &Path) -> Result<()> {
    let _ = fs::remove_file(path);
    let (mut stream, mut reader) = monitor(socket)?;
    qmp(
        &mut stream,
        &mut reader,
        &format!(
            r#"{{"execute":"screendump","arguments":{{"filename":"{}"}}}}"#,
            path.display()
        ),
    )?;
    if !path.exists() {
        bail!(
            "qemu reported a screenshot but {} is not there",
            path.display()
        );
    }
    Ok(())
}

/// The dimensions and raw RGB of a raw PPM, which is the format a QEMU screendump writes.
///
/// P6, then width, height, the maximum value, then raw RGB. QEMU writes exactly that, so the
/// tokens are read rather than a general PPM parser written.
fn read_panel(path: &Path) -> Result<(usize, usize, Vec<u8>)> {
    let bytes = fs::read(path).with_context(|| format!("failed to read {}", path.display()))?;
    let mut tokens = Vec::new();
    let mut cursor = 0;
    while tokens.len() < 4 {
        while cursor < bytes.len() && bytes[cursor].is_ascii_whitespace() {
            cursor += 1;
        }
        let start = cursor;
        while cursor < bytes.len() && !bytes[cursor].is_ascii_whitespace() {
            cursor += 1;
        }
        if start == cursor {
            bail!("{} ends inside its header", path.display());
        }
        tokens.push(String::from_utf8_lossy(&bytes[start..cursor]).into_owned());
    }
    // One whitespace byte separates the header from the pixels.
    cursor += 1;
    if tokens[0] != "P6" {
        bail!("{} is not a raw PPM: {}", path.display(), tokens[0]);
    }
    let width: usize = tokens[1].parse().context("bad PPM width")?;
    let height: usize = tokens[2].parse().context("bad PPM height")?;
    if tokens[3] != "255" {
        bail!(
            "{} is not 8 bits per channel: {}",
            path.display(),
            tokens[3]
        );
    }
    let pixels = bytes[cursor..].to_vec();
    if pixels.len() < width * height * 3 {
        bail!(
            "{} holds {} bytes of pixels, not {} for {width}x{height}",
            path.display(),
            pixels.len(),
            width * height * 3
        );
    }
    Ok((width, height, pixels))
}

/// Checks that the screenshot is a desktop rather than a blank panel.
///
/// Three claims, each of which a compositor that drew nothing or drew it wrong would fail:
/// the corners are the theme's desktop colour, the frame holds several distinct colours (so
/// windows were painted on it), and the pointer's fill appears somewhere. The file is kept
/// under `target/` so a person can look at what the test measured.
fn check_screenshot(path: &Path) -> Result<()> {
    let (width, height, pixels) = read_panel(path)?;
    let pixel = |x: usize, y: usize| -> [u8; 3] {
        let index = (y * width + x) * 3;
        [pixels[index], pixels[index + 1], pixels[index + 2]]
    };

    for (x, y, corner) in [
        (0, 0, "top left"),
        (width - 1, 0, "top right"),
        (0, height - 1, "bottom left"),
    ] {
        let colour = pixel(x, y);
        if colour != DESKTOP_COLOUR {
            bail!(
                "the {corner} corner of the panel is {colour:?}, not the desktop's {DESKTOP_COLOUR:?}"
            );
        }
    }

    // A desktop has at least the background, a window's body, a title bar and its edge, and
    // the pointer is a handful of pixels. The whole frame is scanned rather than sampled: the
    // pointer is small enough that every-seventh-pixel sampling misses it, and a check that
    // can miss what it looks for is worse than no check.
    let mut distinct = std::collections::HashSet::new();
    let mut pointer_pixels = 0usize;
    for y in 0..height {
        for x in 0..width {
            let colour = pixel(x, y);
            if colour == POINTER_COLOUR {
                pointer_pixels += 1;
            }
            distinct.insert(colour);
        }
    }
    if distinct.len() < 4 {
        bail!(
            "the panel holds {} distinct colours, so nothing was composited onto it",
            distinct.len()
        );
    }
    if pointer_pixels == 0 {
        bail!("the pointer is not on the panel: no pixel is {POINTER_COLOUR:?}");
    }
    println!(
        "pass: {width}x{height} screenshot, {} distinct colours, {pointer_pixels} pointer pixels, saved to {}",
        distinct.len(),
        path.display()
    );
    Ok(())
}

/// Checks that the panel under a running shell is still the desktop.
///
/// The claim is one band of the shell window's body: `kernel/src/main.rs` puts that window at
/// 80,380 and gives it 720x380, so the rows just under its title bar - clear of the prompt
/// above and of the window overlapping its bottom left below - belong to it and nothing else.
/// The boot log's background is the desktop's colour, so a line of the log painted after the
/// desktop composites replaces window pixels with teal and text: a band that is mostly window
/// colour is a band the log did not paint over. Sampled across the band rather than at one
/// pixel, because the text of a window that moved underneath would not shift it much.
fn check_preview_screenshot(path: &Path) -> Result<()> {
    /// The band: x from 300 to 790, y from 440 to 468, on the 1280x800 panel OVMF picks.
    const BAND: (std::ops::Range<usize>, std::ops::Range<usize>) = (300..790, 440..468);
    let (width, height, pixels) = read_panel(path)?;
    if width < BAND.0.end || height < BAND.1.end {
        bail!(
            "{} is {width}x{height}, smaller than the panel this check was written for",
            path.display()
        );
    }
    let mut body = 0usize;
    let mut total = 0usize;
    for y in BAND.1 {
        for x in BAND.0.clone() {
            let index = (y * width + x) * 3;
            let colour = [pixels[index], pixels[index + 1], pixels[index + 2]];
            total += 1;
            if colour == WINDOW_BODY_COLOUR {
                body += 1;
            }
        }
    }
    // Nine in ten: the band is empty body, so anything above a tenth of it being some other
    // colour is something drawn on top of the desktop.
    if body * 10 < total * 9 {
        bail!(
            "{}: {body} of {total} pixels in the shell window's body are {WINDOW_BODY_COLOUR:?}, \
             so something painted over the desktop after it was composited",
            path.display()
        );
    }
    println!(
        "pass: {body} of {total} pixels of the shell window's body still hold the window colour, saved to {}",
        path.display()
    );
    Ok(())
}

fn qmp(stream: &mut UnixStream, reader: &mut BufReader<UnixStream>, command: &str) -> Result<()> {
    stream
        .write_all(command.as_bytes())
        .and_then(|()| stream.write_all(b"\n"))
        .and_then(|()| stream.flush())
        .context("failed to send a qmp command")?;
    let mut reply = String::new();
    reader
        .read_line(&mut reply)
        .context("failed to read a qmp reply")?;
    if !reply.contains(r#""return""#) {
        bail!("qemu rejected `{command}`: {reply}");
    }
    Ok(())
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
    // Four CPUs, of which the kernel starts exactly one (ADR-037). The number is not decoration:
    // the boot log's ACPI line has to say four, and a kernel that could not read the MADT would
    // say one - which is what makes the assertion evidence rather than a constant.
    command.args(["-smp", "4"]);
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
        // `-serial stdio` makes QEMU read the terminal as well as write it. With no terminal -
        // a CI runner, a background job, a process whose stdin is a pipe that never closes -
        // that read is a `SIGTTIN` that stops QEMU and hangs the harness until its timeout.
        // The guest never reads serial input here; the input phases drive the guest through
        // the monitor and QMP instead.
        command.stdin(Stdio::null());
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
