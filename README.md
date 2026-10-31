# Grey OS

A hobby operating system written in Rust for x86_64, booting via UEFI.

> **Status: early development, but no longer a boot demo.** The kernel boots under UEFI, owns its
> page tables, heap and frame allocator, drives an ATA disk with its own filesystem, decodes the
> keyboard and the pointer, composites a small desktop of overlapping windows, runs cooperative
> tasks, has a shell, and loads and runs programs from the disk. It also runs a program at ring 3,
> in a page table of its own, reaching the kernel only through `int 0x80`. There is no preemption,
> no ELF loading and no networking. Do not run this on hardware you care about.

## Goals

- Learn and build a real OS kernel in Rust, with `unsafe` kept small and documented.
- Boot on real UEFI hardware, not just emulators.
- Grow toward userspace programs, a filesystem and a simple desktop.
- Coexist with Windows in a dual-boot setup (Windows stays for games that need its anti-cheat).

## Current state

Twelve milestones, each with its own record in `DECISIONS.md` and its acceptance run in `ROADMAP.md`.

- [x] **M0** Boot, serial diagnostics, diagnosable panics: UEFI through the `bootloader` crate, COM1 at 115200 8N1, a panic handler that reports to serial and paints the panel (ADR-023)
- [x] **M1** Display, text and an on-screen boot log: the 8x8 font at an integer scale, a character grid, and a back buffer the panel only receives repaints of (ADR-024, ADR-025, ADR-026)
- [x] **M2** Interrupts, exceptions and the timer: GDT/TSS, IDT, the 8259s remapped, the PIT at 100 Hz, and five injected faults with their own reports (ADR-028)
- [x] **M3** Physical and virtual memory: a page-table walker, a frame allocator over a bitmap, the kernel's own `CR3`, and a heap guard that is never armed because nothing maps past the arena (ADR-029, ADR-030)
- [x] **M4** Heap allocation: an arena of frames, free lists per size class, checked frees, `Vec`/`String` on top (ADR-031)
- [x] **M5** Storage: an ATA drive polled through its ports, and `greyfs` - a format with a sealed superblock, a directory and one extent per file (ADR-032)
- [x] **M6** Input: the 8042 carrying both the keyboard and the pointer, named scancodes, three-byte PS/2 packets (ADR-033)
- [x] **M7** Compositor, windows and theme: a theme made of numbers, windows in list order, damage as a list of rects, a pointer drawn last (ADR-034)
- [x] **M8** Shell and process model: cooperative round-robin over eight slots with real 32 KiB stacks, a parser and a line editor, and a shell over the real filesystem (ADR-035)
- [x] **M9** Applications: `GVM1` bytecode for a stack machine, three programs written to the disk at first format, and one task per program (ADR-036)
- [x] **M10** What the machine is: ACPI read for the CPU count, the IOAPIC and the interrupt overrides, a `cpus` command, `run` for programs, and a README that matches (ADR-037)
- [x] **M11** Hardening before the next primitive: one `InterruptMutex` in place of seven ad-hoc `unsafe impl Sync`s, a `cargo xtask ci` that runs every gate, a release profile that aborts instead of unwinding, and a headless harness that no longer hangs on an inherited stdin (ADR-038)
- [x] **M12** User mode: ring 3 through the user selectors and `TSS.RSP0`, a page table per program shared at the top level, `int 0x80` as the one DPL 3 gate with write/exit/sleep, and a fault in a program that kills the program rather than the kernel (ADR-040)

Not there yet, in the order they matter: preemption of ring 3, ELF loading, a user-space toolchain,
application processors, an APIC/IOAPIC driver, networking.

## Requirements

- macOS or Linux development machine
- [rustup](https://rustup.rs/): the pinned nightly toolchain installs automatically from `rust-toolchain.toml`
- [QEMU](https://www.qemu.org/) (`qemu-system-x86_64`), e.g. `brew install qemu`
- Internet access on first run (downloads the OVMF UEFI firmware)

## Build and run

```bash
cargo run
```

This builds the kernel, wraps it in a bootable UEFI disk image with the `bootloader` crate, and launches QEMU with a window. The bootloader's log appears briefly on serial, then the kernel takes the panel over: the log scrolls, a desktop of six windows appears, three task lines and three programs run, and the kernel exits QEMU with a success status. The serial log lands in this terminal. The pointer, the keyboard and the shell need a person or the harness; a plain `cargo run` boots, runs the demonstration and exits.

The first build is slow because it compiles the bootloader and fetches firmware.

A healthy boot prints approximately (the memory map is elided):

```text
grey-os: serial console online (COM1, 115200 8N1)
grey-os: memory 0x0000000000-0x0000087000     540 KiB usable
...                                    # one line per firmware region, elided here
grey-os: memory usable total 511100 KiB
grey-os: heap bootstrap 0x0001780000-0x001bb3c000 (429808 KiB region, 4007 KiB used)
grey-os: heap probe ok: 4 KiB read back, 1 allocations, 4096 bytes in use
grey-os: framebuffer 1280x800, stride 1280, 4 bytes/pixel
grey-os: display adapter online, 78x48 cells at scale 2
grey-os: console draws into a 4000 KiB back buffer
grey-os: frames: 525288 total, 126756 free, 64 KiB bitmap
grey-os: frame probe ok: 8 frames from 0x1000 written and read back
grey-os: framebuffer physical 0x0080000000-0x00803e8000 (4000 KiB, resolved by page walk)
grey-os: framebuffer byte 2048000 reads the same through both mappings
grey-os: page tables: inherited root 0x101000, own root 0x1000, 1044 frames
grey-os: heap churn ok: 8 allocations and 8 frees over 0 bytes, in use back to 0, 6 pages grown and 6 returned, 0 frames kept plus 3 for the span's page tables
grey-os: heap frees: a double free, a pointer from outside and a wrong size all refused and counted
grey-os: heap trim ok: 4 pages went back to the frame allocator
grey-os: containers ok: a 512-element Vec and a 7 byte String, 4104 bytes in use on the arena
grey-os: heap guard armed at 0x500000004000: the first page above the heap's 4 mapped pages, nothing mapped there
grey-os: mapping probe ok: frame mapped at 0x600000000000, written through the mapping and read back by frame
grey-os: acpi: rsdp at 0x1f77e014 (from the bootloader's table), revision 2, 5 table(s) named
grey-os: acpi: madt: 4 cpu(s), 4 enabled, ioapic at 0xfec00000, 5 source override(s)
grey-os: smp: 1 of 4 cpu(s) started, on the bootstrap processor; the others wait for a trampoline, an ioapic and per-cpu state (ADR-037)
grey-os: ata: primary master "QEMU HARDDISK", 8192 sectors of 512 bytes
grey-os: greyfs: no filesystem, formatted 8192 sectors with 64 directory slots
grey-os: greyfs: wrote /boot-count (16 bytes at sector 16), read back and compared
grey-os: apps: 3 program(s) written this boot, 205 bytes of bytecode on disk, 4 slot(s) used
grey-os: storage ok: boot 1 of this image, 4 file(s), 21 of 8192 sectors in use
grey-os: gdt loaded: code 0x08, data 0x10, tss 0x18, 16 KiB double-fault stack
grey-os: idt loaded: 39 gates of 256 (22 exceptions, 16 irqs, 1 syscall)
grey-os: pic remapped to 0x20..=0x2f, only irq0 unmasked
grey-os: timer 100 Hz, 13 ticks observed, interrupts enabled, 0 unexpected irqs
grey-os: input: keyboard on irq1, 8042 config 0x67, translation on so set 1 arrives
grey-os: input: mouse on irq12 through the cascade, 3-byte packets, pointer at 640,400
grey-os: input ready: 0 keys decoded, 0 dropped, 0 pointer packets
grey-os: theme "grey": title 30px, border 2px, padding 8px, text scale 2
grey-os: task: counter: step 1
grey-os: task: sleeper: asleep for 40 ticks
grey-os: task: worker: pass 1
grey-os: app: hello from apps/hello.gvm
grey-os: app: 1 2 3 4 5
grey-os: app: apps/count.gvm: halted after 67 steps, 38 bytes of code, 2 of strings
grey-os: app: 0 1 1 2 3 5 8 13 21 34
grey-os: app: apps/fib.gvm: halted after 193 steps, 70 bytes of code, 2 of strings
grey-os: task: counter: step 2
grey-os: task: worker: pass 2
grey-os: task: counter: step 3
grey-os: task: worker: pass 3
grey-os: task: counter: step 4
grey-os: task: counter: step 5
grey-os: app: goodbye from 20 ticks later
grey-os: app: apps/hello.gvm: halted after 4 steps, 10 bytes of code, 56 of strings
grey-os: task: sleeper: woke at tick 71
grey-os: sched ok: 7 tasks (6 finished), 24 switches, 10 lines written by tasks, 32 KiB per stack
grey-os: apps ok: 3 of 3 programs ran from /apps, 7 lines of output, 264 instructions
grey-os: user> hello from ring 3
grey-os: user ok: hello exited 0, 2 syscalls, 18 bytes, 1 ticks in ring 3
grey-os: user> tick from ring 3
grey-os: user> tick from ring 3
grey-os: user> tick from ring 3
grey-os: user ok: count exited 3, 7 syscalls, 51 bytes, 15 ticks in ring 3
grey-os: user: #PF page fault at rip 0x4000000100a: error 0x6, address 0x100000000, killed
grey-os: user ok: fault exited 142, 0 syscalls, 0 bytes, 0 ticks in ring 3
grey-os: desktop: 6 windows on 1280x800, 7 overlapping pair(s), focused #6, 3 drawn in the first frame
grey-os: compositor ok: 4 damage rects merged into 2, pointer drawn at 640,400, 10 frame(s)
grey-os: boot ok
```

The geometry comes from the firmware; the kernel reports what GOP actually gave it
rather than assuming a resolution. The heap is an arena of its own: 64 MiB of reserved
virtual span at `0x5000_0000_0000`, backed one 4 KiB page at a time out of the frame
allocator and mapped `WRITABLE | NO_EXECUTE` through the kernel's own tables. The region
the memory map offers is only the bootstrap - the frame allocator's bitmap and the
console, which exist before a frame can be mapped - and only the 4007 KiB it used is
reserved, so the rest of the 430000 KiB region goes back into circulation. A write past
the arena's growth frontier faults because nothing ever mapped that page: no guard has to
be armed, and `cr2` names the grown heap's end.

`bootloader_api` 0.11 publishes the framebuffer only as a virtual address, so the kernel
walks the page tables the bootloader left in `CR3` to find the frames behind it, then reads
one byte back through the physical window to prove the two views agree. The 1044 frames of
page tables are the price of copying the bootloader's tree, which includes a 1 TiB physical
window mapped with 2 MiB pages; the kernel then loads that copy into `CR3` and owns the
address space. See ADR-029.

Owning the address space means editing it, so M4 starts with the other direction: `map_4k`
gives a frame an address, creating the tables it needs. The boot proves it the same way it
proves the walk - a frame from the allocator mapped at `0x6000_0000_0000`, a byte written
through the new mapping and read back through the window at the frame, then the mapping taken
away and the frame handed back. See ADR-030.

The heap on top of it reuses memory: blocks carry a 16-byte header, free blocks sit on one
list per size class, an allocation splits the block it finds, a free merges with both
neighbours and gives whole pages back when it frees the top of the arena. The churn probe
allocates eight sizes from 16 bytes to 8 KiB, writes a pattern through every block, reads it
back, frees the lot and reports that `bytes_in_use` returned to zero - with the frames it cost
having to match what the frame allocator says it no longer has. Three bad frees - a double
free, a pointer from outside the arena, a wrong size - are refused and counted rather than
followed, which is what keeps a caller's bug from becoming a corrupted free list. A `Vec` and
a `String` are built on the allocator and read back before the guard line. See ADR-031.

The panel shows the log while the boot runs: the 8x8 font at scale 2, 78 columns by 48 rows on the
1280x800 surface. From M7 it stops being a console: the boot's last act is to create six windows on
a desktop, repaint the whole panel once - the frame the harness photographs - and then let the
desktop own the screen while the log continues on serial.

M5 puts a disk behind the ports: an ATA drive driven by polling its status register, and `greyfs`
over it, with a superblock sealed by an FNV-1a hash, a directory of 64 entries and file data above
it. The boot writes `/boot-count` and reads it straight back, so the next boot's `boot 2` is
evidence about the disk rather than about this boot. A file is one extent: replacing or removing one
leaks its sectors until a free-space pass exists, which is written down rather than hidden. See
ADR-032.

M6 makes the inputs work. The 8042 carries both devices - the keyboard on IRQ1 with translation on
so set 1 scancodes arrive, the pointer on IRQ12 through the cascade - and the kernel keeps a queue of
named keys (`Enter`, `Escape`, `Backspace`, `Tab` and the ASCII ones) and a position for the pointer.
A kernel cannot press its own keyboard, so the acceptance run types through QEMU's monitor and
asserts each decoded key on serial. See ADR-033.

M7 is the compositor: a theme made of numbers, windows kept in list order with a focused one on top,
and damage as a list of rectangles, so a repaint touches what changed. The pointer is drawn last,
every frame, because a window painted over it used to erase it. See ADR-034.

M8 adds real tasks and a shell. Eight slots, each task with a 32 KiB stack from the arena and a
saved context of the callee-saved registers; the switch is 22 instructions and ends in `ret`, which
resumes whichever task's stack was loaded. Switching is cooperative - a task yields or sleeps - and
the timer only wakes sleepers, which is enough to run three demonstrations that take turns in a way
only a scheduler could produce. The shell parses sixteen commands and edits a line in the library
(host-tested), and executes them against the real filesystem, allocator and window list. See ADR-035.

M9 makes an application a file. `GVM1` is a header, code and a pool of strings for a stack machine
with 32 cells, 16 locals and 22 opcodes; `Program::parse` validates every jump, pool offset and local
index before the first instruction runs, so a bad file is rejected rather than interpreted, and the
interpreter's only failures are underflow, overflow, division by zero and a step budget. The kernel
assembles three programs (`hello`, `count`, `fib`), writes them to the disk at first format, and runs
one task per program - their output in the `apps` window and on serial. See ADR-036.

M10 asks the firmware what the machine is: the RSDP - from the bootloader's UEFI table, or by the
ACPI 1.0 search when there is no hint - the root table and the MADT, which says four CPUs and an
IOAPIC on the QEMU machine this is tested on. Nothing starts those CPUs: the line says `1 of 4 cpu(s)
started` because that is the truth, and ADR-037 lists what starting them would need. The same facts
are a `cpus` command, and `run fib` loads `apps/fib.gvm` from the disk and interprets it in the
shell's window.

Because the kernel exits through QEMU's `isa-debug-exit` device, the QEMU window closes as soon as
boot finishes and `cargo run` exits with status 0. A panic prints its message and location to the
terminal and exits with status 1, after painting the panel 0xb01b2f, keeping the log so far on it
and rendering the report underneath in text.

## `xtask`

The same boot, automated:

```bash
cargo xtask build   # stage both images in target/grey-os/
cargo xtask run     # build, then boot the UEFI image with a window
cargo xtask test    # build, boot clean, then boot one kernel per injected failure
cargo xtask ci      # fmt, both clippy halves, host tests, the boot tests, release build
```

`test` checks the serial markers _and_ the exit status, and fails on a hang rather
than waiting forever. It runs thirteen phases over one deterministic disk:

1. **The clean boot**, which must print `grey-os: boot ok` and exit 33 - and, before that, the
   bring-up lines: the frame totals, the framebuffer's resolved physical span, the kernel's own
   page-table root, the heap's churn, refusals and trim, the containers, the guard the arena never
   armed, the ACPI report, the disk formatted and the counter written back, the three programs
   installed, the three task lines and the three programs' output, and the desktop composited.
2. **Persistence**: a second boot of the same disk, which must mount rather than format, and read
   back the counter as `boot 2`.
3. **The input path**, which rebuilds the kernel with `GREY_OS_INPUT_TEST=1` and drives a running
   guest through QEMU's monitor and QMP socket: four keys typed and each asserted on serial, a
   relative pointer movement decoded, a screenshot taken and checked for the desktop's colour and
   the pointer's pixels, and five commands typed into the shell - `ls`, `mem`, `cpus`, `run fib`,
   `run nope` - with each answer and `exit` asserted.
4. **The panic path**, a kernel rebuilt with `GREY_OS_PANIC_TEST=1`, which must print `[panic]`, its
   message, its location and `[panic] panel painted red`, and exit 35. The last line is how the
   painted screen is verified from outside a guest that cannot be looked into.
5. **Five fault paths**, each rebuilt with `GREY_OS_EXCEPTION_TEST=gp`, `pf`, `ud`, `df` or `guard`:
   each has to print its own `[exception]` report - the vector, its error code where the vector has
   one, and the faulting instruction - and exit 35. `guard` writes one byte past the heap, so the
   report is a write to a page that is not present and `cr2` is the heap's end.

The images are then rebuilt without the flags, so a phase that failed leaves nothing behind that
would make the next `cargo xtask run` baffling. `--no-build` skips the nested `cargo build` for
`build` and `run`; `test` refuses it, since the flags have to be compiled in. `--bios` boots the BIOS
image instead (built, never verified - see `ROADMAP.md`).

To watch the panic path yourself, including the red panel:

```bash
GREY_OS_PANIC_TEST=1 cargo xtask run
```

For a CPU fault instead, pick one of `gp`, `pf`, `ud`, `df` or `guard`:

```bash
GREY_OS_EXCEPTION_TEST=pf cargo xtask run
```

Either way the image then stays in that state, because the flag is compiled in: the next plain
`cargo xtask build` (or `cargo xtask test`, which restores it on the way out) undoes it.

These commands need the alias in `.cargo/config.toml`:

```toml
[alias]
xtask = "run --package xtask --"
```

Without it, `cargo run --package xtask -- test` does the same thing.

`cargo xtask ci` is the whole set in one command, in the order that fails fastest: `cargo fmt
--check`, clippy for the host tools and for the kernel's real target with `-D warnings`, the host
tests, `cargo xtask test`, and a release build. It is what `.github/workflows/ci.yml` runs, and it is
the only command a change has to pass.

Lint the two halves separately: the kernel is `x86_64`-only `no_std` code and does
not compile for the host architecture (ADR-006 in `DECISIONS.md`).

```bash
cargo test --package kernel --lib                           # host tests for the pure code
cargo clippy --workspace --exclude kernel                   # host tools + xtask
cargo clippy --package kernel --target x86_64-unknown-none   # the kernel
```

`--lib` matters: the library compiles for the host, the binary does not.

Milestone status and acceptance criteria are in `ROADMAP.md`; the reasoning behind
the build, the drivers and the QEMU setup is in `DECISIONS.md`.

Both disk images are also copied to `target/grey-os/` (`grey-os-uefi.img` and
`grey-os-bios.img`) so tooling does not have to hunt through hashed build-script
output directories. Nothing in this repository ever writes to a real disk.

## Project layout

```
grey-os/
├── Cargo.toml            # workspace root + image builder package
├── build.rs              # builds the kernel and creates UEFI/BIOS disk images
├── rust-toolchain.toml   # pinned nightly toolchain
├── .cargo/config.toml    # artifact-dependency feature and the `xtask` alias
├── src/bin/qemu-uefi.rs  # launches the UEFI image in QEMU
├── xtask/                # build/run/test automation (`cargo xtask ...`)
└── kernel/               # the OS kernel (no_std)
    ├── Cargo.toml
    ├── build.rs          # `GREY_OS_*` environment flags -> `cfg(...)` (ADR-023)
    └── src/
        ├── main.rs        # entry point, boot sequence, tasks, shell executor, panic handler
        ├── lib.rs         # module list: what the host tests can reach
        ├── canvas.rs      # geometry, one Colour, fill/blend/scroll/blit
        ├── font.rs        # public-domain 8x8 bitmaps, printable ASCII (ADR-024)
        ├── text.rs        # glyph drawing at an integer scale, metrics
        ├── console.rs     # character grid: cursor, wrapping, scrolling (ADR-025)
        ├── display.rs     # console instance, back buffer, panel repaint, panic screen
        ├── framebuffer.rs # boot-info framebuffer -> Canvas adapter
        ├── theme.rs       # the theme as numbers: colours, padding, title height (ADR-034)
        ├── window.rs      # one window: a rect, a title and lines
        ├── compositor.rs  # the desktop: window list, focus, damage rects, pointer
        ├── memory.rs      # memory map, the bootstrap region's selection
        ├── sync.rs        # the one interior-mutability cell and its interrupt guard (ADR-038)
        ├── heap.rs        # bump allocator: the bootstrap only (ADR-021, ADR-031)
        ├── arena.rs       # the heap: pages from frames, free lists per class (ADR-031)
        ├── frame.rs       # 4 KiB frame allocator over a bitmap (ADR-029)
        ├── paging.rs      # page-table entries, the walk, the kernel's own CR3 (ADR-029)
        ├── gdt.rs         # GDT, TSS and the double-fault stack (ADR-028)
        ├── idt.rs         # IDT, exception reports, the injected faults (ADR-028)
        ├── pic.rs         # the 8259 pair, remapped off the exception vectors (ADR-028)
        ├── timer.rs       # the PIT at 100 Hz and its tick counter (ADR-028)
        ├── port.rs        # `in`/`out`, documented once instead of once per driver
        ├── disk.rs        # the ATA drive in PIO mode, sectors and `Block` (ADR-032)
        ├── fs.rs          # `greyfs`: superblock, directory, extents, the seal (ADR-032)
        ├── keyboard.rs    # the 8042's keyboard: set 1 scancodes to named keys (ADR-033)
        ├── mouse.rs       # the 8042's pointer: three-byte packets (ADR-033)
        ├── scheduler.rs   # eight slots, 32 KiB stacks, the cooperative switch (ADR-035)
        ├── shell.rs       # the command parser and the line editor (ADR-035)
        ├── vm.rs          # `GVM1`: the program format and the stack machine (ADR-036)
        ├── apps.rs        # the three programs the kernel writes to the disk (ADR-036)
        ├── acpi.rs        # RSDP, root table and MADT: what the machine is (ADR-037)
        ├── serial.rs      # COM1 console
        └── qemu.rs        # exit codes through `isa-debug-exit`
```

## Toolchain notes

- Uses **nightly Rust**, pinned to `nightly-2026-10-14`. The build relies on the unstable `bindeps` (artifact dependency) Cargo feature, so stable Rust cannot build it.
- If a future nightly breaks the build, keep the pin and update it deliberately.
- The kernel targets the built-in `x86_64-unknown-none` target.

## Running on real hardware

Not yet tested. The build also produces a UEFI disk image that can in principle be written to a USB
drive. Before trying it:

- Back up your data and boot from USB or a spare drive, never your main system disk.
- Secure Boot must be disabled to boot an unsigned image, then re-enabled for Windows games that require it.
- The kernel looks for an ATA disk on the primary channel and formats it **only when sector 0 is
  entirely zero** - a blank disk. Anything else is treated as somebody's data: a `greyfs` superblock
  is mounted, and any other content is refused with an error rather than overwritten. Nothing in this
  repository writes to a disk other than an image file under `target/`, and that is the only reason a
  stray `cargo run` cannot damage a machine.
- A real machine's ACPI tables may name more CPUs than the one this kernel uses; the boot log says
  how many it found, and ADR-037 says why the rest stay asleep.

## Learning resources

- [Writing an OS in Rust](https://os.phil-opp.com/) (the third edition is still in progress)
- [OSDev Wiki](https://wiki.osdev.org/)
- [rust-osdev](https://github.com/rust-osdev) crates: `bootloader`, `x86_64`, `uart_16550`, `acpi`

## License

MIT, copyright the Grey OS contributors. See [`LICENSE`](LICENSE).
