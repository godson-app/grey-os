# Grey OS

A hobby operating system written in Rust for x86_64, booting via UEFI.

> **Status: early development.** The kernel boots in QEMU, paints the screen, owns its page tables and has working heap and frame allocators. Nothing else works yet. Do not run this on hardware you care about.

## Goals

- Learn and build a real OS kernel in Rust, with `unsafe` kept small and documented.
- Boot on real UEFI hardware, not just emulators.
- Grow toward userspace programs, a filesystem and a simple desktop.
- Coexist with Windows in a dual-boot setup (Windows stays for games that need its anti-cheat).

## Current state

- [x] Boots in QEMU through UEFI firmware
- [x] Framebuffer fill (pixel-format aware)
- [x] Serial logging on COM1 and a panic handler that reports to serial and screen
- [x] Heap allocation: a bump allocator over a region picked from the memory map, proved by a boot-time write/read-back (ADR-021, ADR-022)
- [x] Glyph rendering: a public-domain 8x8 bitmap font and an integer-scaled text renderer, host-tested (ADR-024)
- [x] On-screen text console: the boot log goes to serial and to the panel, scrolling included (ADR-025)
- [x] Back buffer and `Canvas::blit`: the console draws into a heap surface and the panel receives only the rows that changed (ADR-026)
- [x] Interrupts and exception handling: GDT/TSS, IDT, PIC, PIT, injected faults (ADR-028)
- [x] Memory management: page-table walker, frame allocator, own `CR3`, heap guard page (ADR-029)
- [ ] A heap that gives memory back: the mapper landed, the allocator is next (ADR-030)
- [ ] Scheduler and kernel threads
- [ ] Userspace, system calls and ELF loading
- [ ] Storage and filesystem
- [ ] Input (USB), graphics, networking

## Requirements

- macOS or Linux development machine
- [rustup](https://rustup.rs/): the pinned nightly toolchain installs automatically from `rust-toolchain.toml`
- [QEMU](https://www.qemu.org/) (`qemu-system-x86_64`), e.g. `brew install qemu`
- Internet access on first run (downloads the OVMF UEFI firmware)

## Build and run

```bash
cargo run
```

This builds the kernel, wraps it in a bootable UEFI disk image with the `bootloader` crate, and launches QEMU. On success you will see the bootloader log briefly, then the screen is painted with the boot colour and the kernel exits QEMU with a success status. The serial log lands in this terminal.

The first build is slow because it compiles the bootloader and fetches firmware.

A healthy boot prints approximately:

```text
grey-os: serial console online (COM1, 115200 8N1)
grey-os: memory 0x0000000000-0x0000087000     540 KiB usable
grey-os: memory 0x0000087000-0x0000088000       4 KiB usable
...                                    # one line per firmware region, elided here
grey-os: memory usable total 512200 KiB
grey-os: heap candidate 0x0001780000-0x001bb6b000 (429996 KiB, guard page at 0x1bb6b000)
grey-os: heap ready 0x28001780000..0x2801bb6b000, 429996 KiB reserved
grey-os: heap probe ok: 4 KiB read back, 1 allocations, 4096 bytes in use
grey-os: framebuffer 1280x800, stride 1280, 4 bytes/pixel
grey-os: display adapter online, 78x48 cells at scale 2
grey-os: console draws into a 4000 KiB back buffer
grey-os: frames: 525288 total, 20541 free, 64 KiB bitmap
grey-os: frame probe ok: 8 frames from 0x1000 written and read back
grey-os: framebuffer physical 0x0080000000-0x00803e8000 (4000 KiB, resolved by page walk)
grey-os: framebuffer byte 2048000 reads the same through both mappings
grey-os: page tables: inherited root 0x101000, own root 0x1000, 1044 frames
grey-os: heap guard armed at 0x2801bb6b000 (physical 0x1bb6b000), one page unmapped
grey-os: mapping probe ok: frame mapped at 0x600000000000, written through the mapping and read back by frame
grey-os: gdt loaded: code 0x08, data 0x10, tss 0x18, 16 KiB double-fault stack
grey-os: idt loaded: 38 gates of 256 (22 exceptions, 16 irqs)
grey-os: pic remapped to 0x20..=0x2f, only irq0 unmasked
grey-os: timer 100 Hz, 13 ticks observed, interrupts enabled, 0 unexpected irqs
grey-os: boot ok
```

The geometry comes from the firmware; the kernel reports what GOP actually gave it
rather than assuming a resolution. The heap is the largest usable region in that map that
does not overlap the framebuffer, converted from its physical address through the offset
the bootloader picked for the physical-memory window (`0x28001780000` here, 2.5 TiB above
zero). Its last page is deliberately left unmapped as a guard, so a write past the end of
the heap faults instead of landing in whatever follows.

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

The same lines are drawn on the panel: the 8x8 font at scale 2, 78 columns by 48 rows on
the 1280x800 surface, 16 pixels in from each edge. The firmware's memory map is longer
than 48 lines, so the earliest regions scroll off before boot finishes - the panel shows
the tail of the log and the serial port keeps all of it.

Because the kernel exits through QEMU's `isa-debug-exit` device, the QEMU window closes
as soon as boot finishes and `cargo run` exits with status 0. A panic prints its message
and location to the terminal and exits with status 1, after painting the panel 0xb01b2f,
keeping the log so far on it and rendering the report underneath in text.

## `xtask`

The same boot, automated:

```bash
cargo xtask build   # stage both images in target/grey-os/
cargo xtask run     # build, then boot the UEFI image with a window
cargo xtask test    # build, boot clean, then boot one kernel per injected failure
```

`test` checks the serial markers _and_ the exit status, and fails on a hang rather
than waiting forever. It boots once per failure path: first the normal image, which must print
`grey-os: boot ok` and exit 33 - and, before that, the memory bring-up lines: the frame
totals, the framebuffer's resolved physical span, the kernel's own page-table root and the
armed heap guard. Then a kernel rebuilt with `GREY_OS_PANIC_TEST=1`, which
must print `[panic]`, its message, its location and `[panic] panel painted red`, and
exit 35. The last line is how the painted screen is verified from outside a guest that
cannot be looked into. Five more images follow, each rebuilt with
`GREY_OS_EXCEPTION_TEST=gp`, `pf`, `ud`, `df` or `guard`: each has to print its own `[exception]`
report - the vector, its error code where the vector has one, and the faulting
instruction - and exit 35. `guard` writes one byte past the heap, so the report is a write to
a page that is not present and `cr2` is the heap's end. `--no-build` skips the nested
`cargo build` for `build` and `run`; `test` refuses it, since the flags have to be compiled
in. `--bios` boots the BIOS image instead (built, never verified - see `ROADMAP.md`).

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
    ├── build.rs          # `GREY_OS_PANIC_TEST` -> `cfg(grey_os_panic_test)` (ADR-023)
    └── src/
        ├── main.rs        # entry point, boot sequence, panic handler
        ├── lib.rs         # module list: what the host tests can reach
        ├── canvas.rs      # geometry, one Colour, fill/blend/scroll/blit
        ├── font.rs        # public-domain 8x8 bitmaps, printable ASCII (ADR-024)
        ├── text.rs        # glyph drawing at an integer scale, metrics
        ├── console.rs     # character grid: cursor, wrapping, scrolling (ADR-025)
        ├── display.rs     # console instance, back buffer, panel repaint, panic screen
        ├── framebuffer.rs # boot-info framebuffer -> Canvas adapter
        ├── memory.rs      # memory map, heap region selection, guard-page split
        ├── heap.rs        # bump allocator behind `GlobalAlloc`
        ├── frame.rs       # 4 KiB frame allocator over a bitmap (ADR-029)
        ├── paging.rs      # page-table entries, the walk, the kernel's own CR3 (ADR-029)
        ├── qemu.rs        # exit codes through `isa-debug-exit`
        └── serial.rs      # COM1 console
```

## Toolchain notes

- Uses **nightly Rust**, pinned to `nightly-2026-10-14`. The build relies on the unstable `bindeps` (artifact dependency) Cargo feature, so stable Rust cannot build it.
- If a future nightly breaks the build, keep the pin and update it deliberately.
- The kernel targets the built-in `x86_64-unknown-none` target.

## Running on real hardware

Not yet tested. The build also produces a UEFI disk image that can in principle be written to a USB drive. Before trying it:

- Back up your data and boot from USB or a spare drive, never your main system disk.
- Secure Boot must be disabled to boot an unsigned image, then re-enabled for Windows games that require it.

## Learning resources

- [Writing an OS in Rust](https://os.phil-opp.com/) (the third edition is still in progress)
- [OSDev Wiki](https://wiki.osdev.org/)
- [rust-osdev](https://github.com/rust-osdev) crates: `bootloader`, `x86_64`, `uart_16550`, `acpi`

## License

Not yet chosen.
