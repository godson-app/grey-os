# Roadmap

Grey OS is built one milestone at a time. A milestone is only marked done when its
acceptance criteria are demonstrated by output, not by code existing: the proof is
a `xtask test` run or a QEMU session whose serial log is pasted into the commit
message. No milestone is started before the previous one is accepted.

Scope for M0 and M1 is agreed. M2 onward is stated as themes in dependency order:
the authoritative wording comes from the engineering brief and is transcribed here
milestone by milestone, so that this file never claims more than was agreed.

## Status

| Milestone | Theme                                        | Status                                                              |
| --------- | -------------------------------------------- | ------------------------------------------------------------------- |
| M0        | Boot, serial diagnostics, diagnosable panics | **Done** - criteria automated; BIOS image boots (ADR-009)            |
| M1        | Display primitives, text, on-screen boot log | **Done** - criteria met; TrueType moved to M7 (ADR-027)               |
| M2        | Interrupts, exceptions, timers               | **Criteria met** - awaiting acceptance (ADR-028)                     |
| M3        | Physical and virtual memory management       | Not started                                                         |
| M4        | Heap allocation and kernel data structures   | Not started (the heap itself moved into M1, see ADR-017)            |
| M5        | Storage: disk driver and filesystem          | Not started                                                           |
| M6        | Input: keyboard and pointer                  | Not started                                                           |
| M7        | Compositor, windows, theme                   | Not started                                                           |
| M8        | Shell and process model                      | Not started                                                           |
| M9        | Applications                                 | Not started                                                           |
| M10       | SMP, ports, polish                           | Not started                                                           |

## M0 - Boot, serial diagnostics, diagnosable panics

**Goal.** A kernel that boots under UEFI, can tell a human what it is doing, and
fails in a way that is visible on a machine with no serial cable attached.

**Done.**

- [x] UEFI boot through the `bootloader` 0.11 artifact-dependency build.
- [x] Both images (`grey-os-uefi.img`, `grey-os-bios.img`) staged in `target/grey-os/`.
- [x] GOP framebuffer located and the real geometry logged, never assumed.
- [x] COM1 serial console, 115200 8N1, hand-rolled on raw port I/O.
- [x] Panic handler reporting message and location to serial and painting the screen red.
- [x] QEMU exit codes through `isa-debug-exit`: 33 clean, 35 reported failure.
- [x] `cargo xtask build|run|test`, with `test` asserting the serial marker _and_ exit 33.
- [x] Automated panic-path test: `GREY_OS_PANIC_TEST` becomes `cfg(grey_os_panic_test)`
      in `kernel/build.rs`, and `cargo xtask test` rebuilds, boots and asserts
      `[panic]`, the message, the location and the painted panel, then exit 35
      (ADR-023).
- [x] BIOS image executed: SeaBIOS boots it, the bootloader hands over to the kernel, and
      the kernel refuses the 24-bit VBE surface it is given (`Bgr, 3 bytes/pixel,
      1280x720`) with a FATAL line that names the layout (ADR-009). Booting is proven;
      rendering under BIOS is not, and is not M1's target.

**Open.** Nothing for M0.

**Acceptance criteria.**

1. `cargo xtask test` exits 0 having seen `grey-os: boot ok` and QEMU status 33.
2. A forced panic prints `[panic] <message>` and its location, paints the screen
   0xb01b2f, and produces QEMU status 35. Automated by `cargo xtask test`; the paint is
   asserted through the kernel's own `[panic] panel painted red` line, because nothing
   outside the guest can read the screen.
3. A missing framebuffer produces `FATAL: boot info carries no framebuffer` rather
   than a silent hang or a black screen.

## M1 - Display primitives and text

**Goal.** Replace the serial-only boot log with something on screen.

**Landed so far.**

- [x] `canvas.rs`: validated `Geometry`, one `Color`, `fill`, `blit`, alpha blending, and
      12 host tests for the pure maths.
- [x] `framebuffer.rs` adapter: the layout is validated once, palette/unknown formats are
      refused (ADR-020), and the kernel owns the surface behind `install`/`with_screen`, so
      the panic handler can paint a screen it was never handed.
- [x] `memory.rs`: picks the heap region out of the firmware's memory map - largest usable
      region that is not the framebuffer - with 10 host tests.
- [x] `heap.rs`: bump allocator behind `GlobalAlloc`, counters in the boot log, 8 host tests
      (ADR-021).
- [x] The bootloader maps physical memory at a dynamic offset, and the boot sequence
      converts the region through it before handing it to the allocator (ADR-022).
- [x] A 4 KiB write/read-back through the allocator at boot, so the heap is proved mapped
      rather than assumed.
- [x] Kernel split into `lib.rs` + a thin `bin`, so the pure code is host-tested by
      `cargo test -p kernel --lib`.
- [x] `kernel/build.rs`, turning `GREY_OS_PANIC_TEST` into `cfg(grey_os_panic_test)`
      with `cargo::rerun-if-env-changed`, which is what closes M0's panic-path item
      (ADR-023).
- [x] `font.rs`: public-domain 8x8 bitmaps for printable ASCII, provenance and upstream
      checksum in the header (ADR-024).
- [x] `text.rs`: glyph drawing with integer scaling, clipping delegated to `fill_rect`, and
      16 host tests covering bit order, scaling, clipping, pen advance and metrics.
- [x] `console.rs`: a character grid with a cursor, wrapping, `\t`/`\r` handling, scrolling
      and dirty-row tracking, host-tested against real glyph output.
- [x] `display.rs`: the console instance on the heap, `println!` writing to serial and the
      panel, one glyph per canvas call, a scroll that copies pixels instead of redrawing
      them, and a panic screen that keeps the log above the report (ADR-025).
- [x] The M0 boot log on screen, 78x48 cells at scale 2 on the 1280x800 surface OVMF picks.
- [x] Back buffer (ADR-026): `display.rs` allocates a 4,000 KiB surface with the panel's own
      geometry and channel order, the console draws into it, and `Canvas::blit` copies the
      rows that changed onto the panel - one band copy per line instead of per-pixel writes
      into panel memory, and the whole grid after a scroll because every row moved. A failed
      allocation falls back to the ADR-025 path and the boot log says which one is live.

**Carried forward.** M1 is accepted with two items open; neither blocks the criteria below, and
each names where it gets fixed. TrueType rasterisation and the face it needs left M1 for M7
(ADR-027), so the text stack stays on ADR-024's public-domain bitmaps at integer scale.

- The framebuffer's physical span, so ADR-021's overlap guard stops being inert. Needs a
  page-table walk, which is M3's machinery; today the boot log says the guard is unarmed.
- Rendering under BIOS/VBE: the legacy image boots but is handed a 24-bit surface this
  renderer refuses (ADR-009). Either a 24-bit path in `canvas.rs` or a 32-bit VBE mode; not
  needed while UEFI is the target.

**Acceptance criteria.** The boot log is legible at the firmware-selected
resolution; a panic paints the panel red with the message rendered in text; screen
output is presentable without dropping frames on a 1280x800 surface.

Status against them: the log is legible - every line of the boot log was read back off a
QEMU screenshot with the font table, at 78x48 cells on 1280x800, and the earliest regions
scroll off because the map is longer than the screen. A panic paints the whole panel
0xb01b2f (a screendump check found the grid area was still teal until ADR-026 fixed it),
keeps the log above the report and renders both in text. The third criterion is met by the
back buffer: a line is complete in RAM before any of its pixels reach the panel, so a
repaint is presented in one band copy rather than watched glyph by glyph. Measured emulated
boot: 3.49 s, of which about 2 s is firmware - unchanged from before the buffer, because the
time is in QEMU's firmware and in the ~110 memory-map lines, not in the presenting.

## M2 - Interrupts, exceptions and timers

**Goal.** Stop borrowing the bootloader's CPU tables. A CPU fault has to name itself and reach
the panic path instead of triple-faulting the machine, and time has to pass so that a scheduler
and a timeout have something to hang off.

**Agreed order** (ADR-028): GDT with a TSS and a `#DF` stack, then the IDT with a handler per
exception vector, then the 8259 PIC remapped off the exception range, then the PIT.

**Landed so far.**

- [x] `gdt.rs`: 64-bit code and data descriptors, a TSS carrying an IST stack for `#DF`, loaded
      with `lgdt`/`ltr` and a reload of every segment register.
- [x] `idt.rs`: 256 entries, a gate per exception vector, error-code vectors handled as such,
      and a report line naming the vector, its error code and the faulting instruction.
- [x] `pic.rs`: both 8259s remapped to 0x20..=0x2F and masked except the vectors in use.
- [x] `timer.rs`: PIT channel 0 in rate-generator mode, a tick counter the boot log samples.
- [x] Injected `#GP`/`#PF`/`#UD`/`#DF` as test phases in `cargo xtask test`, each asserted
      through the serial report and QEMU's exit status.

**Open.**

- Printing from an interrupt handler: not allowed while the console and the heap assume a
  single thread (ADR-028). A handler that wants to log needs a lock or a queue first.
- `#MC`, `#XM` and the rest of the rare vectors are reported like any other fault, but nothing
  recovers from them and nothing pretends to.
- The 8259 is a stopgap: an APIC replaces it when SMP or per-CPU interrupts arrive (M10).

**Acceptance criteria.**

1. A deliberate `#GP`, `#PF` and `#UD` each reach the panic path naming the vector, the error
   code where the vector has one, and the faulting instruction, and QEMU exits 35.
2. A `#DF` runs on the TSS stack and is reported rather than triple-faulting.
3. The boot log shows the image's own GDT, an IDT with every exception vector filled, the PIC
   remapped, and a timer counter that advances while the kernel waits for it.

**Status against them.** All three are met, in QEMU, on the boots `cargo xtask test` performs. The
boot log reports `gdt loaded: code 0x08, data 0x10, tss 0x18, 16 KiB double-fault stack`,
`idt loaded: 38 gates of 256 (22 exceptions, 16 irqs)`, `pic remapped to 0x20..=0x2f, only irq0
unmasked` and `timer 100 Hz, 13 ticks observed, interrupts enabled, 0 unexpected irqs`. The tick
count is read only after the boot path has waited for ten of them, so a counter that did not move
would fail at the wait rather than print a number that looked like progress.

The fault phases are four further images, each built with `GREY_OS_EXCEPTION_TEST` set to one of
`gp`, `pf`, `ud` or `df`, each booted and each asserted on its report and on QEMU's exit 35:

```text
[exception] #GP general protection fault: error 0x28, rip 0x1000000aad8
[exception] #PF page fault: error 0x0 on a read to a not-present page, rip 0x1000000ac51, cr2 0x700000000000
[exception] #UD invalid opcode: no error code, rip 0x1000000aac4
[exception] #DF double fault: error 0x0, rip 0x1000000845e
```

Each phase faults with a real instruction the CPU refuses - a segment load of a selector past the
end of the image's own GDT, a read of an unmapped address, `ud2`, and a push onto an unmapped
stack - rather than by calling a handler from Rust, which would prove only that a function prints.
The `#DF` line is criterion 2: it exists because the TSS routed the fault onto its own stack. The
injected stub moves `RSP` to an address nothing maps and then pushes, so the `#PF` the push raises
cannot push a frame either and the CPU escalates; the `rip` reported is the stub's `push`, and the
error code is 0.

Two details the phases caught, both of which a plausible-looking version had wrong. A handler has
to take `InterruptFrame` **by value**: the ABI reads its arguments out of the interrupted stack, so
the frame sits at `RSP + 8` and a `&InterruptFrame` parameter makes the handler dereference the
saved `RIP` as if it were a pointer to the frame. And `int 8` cannot stand in for a `#DF`: a
software interrupt pushes no error code even on a vector whose hardware fault does, so the handler
would read a return address as its error code. Hence the assembly stub.

**Carried forward.** The `#DF` stack is 16 KiB, which is enough to print a report and no more; a
handler that ever needed to do work on it would want a larger one. The PIC's mask is fixed at
`irq0`, so the vector range above it is claimed but silent until a driver unmasks one.

## M3 onward - provisional themes

The ordering above is dependency-driven and each theme is a milestone of its own.
Detailed scope, acceptance criteria and verification are added when the milestone
is approved, per the brief's one-at-a-time rule.

Two items already have a home when their milestone opens: TrueType rasterisation, with the face
question it drags along, belongs to M7 (ADR-027); and the framebuffer's physical span, which
would make ADR-021's heap guard bite, belongs to M3's page-table work.

## Hardware compatibility

| Target                   | State                                                             |
| ------------------------ | ----------------------------------------------------------------- |
| QEMU UEFI (OVMF), x86_64 | Primary target. Boots, logs, exits cleanly.                       |
| QEMU UEFI, 1280x800      | What OVMF actually selects; the kernel logs whatever it is given. |
| QEMU BIOS (SeaBIOS)      | Boots to the kernel, which refuses VBE's 24-bit surface (ADR-009). |
| Real x86_64 hardware     | **Not ready.** The UEFI image boots from the root of a FAT32      |

stick with secure boot disabled, but with no filesystem or storage driver there is
nothing to persist and no installer. Deferred until M5. |

A dual-boot machine's existing bootloader and partitions must survive every step:
no tool in this repository writes to a disk other than an image file under `target/`.

## How verification works

```bash
cargo fmt --all                                              # formatting
cargo test --package kernel --lib                            # host tests for the pure code
cargo clippy --workspace --exclude kernel                     # host tools and xtask
cargo clippy --package kernel --target x86_64-unknown-none    # the kernel itself
cargo xtask test                                             # clean boot, then one per injected failure
```

The kernel lint line is not optional and not interchangeable with the host one: the
kernel is `x86_64`-only `no_std` code, so linting it for the host architecture fails on
inline-asm register names and on `bootloader_api::entry_point!`. See `DECISIONS.md`.
The host tests use `--lib` for the same reason: the library also compiles for the host,
the binary does not, so a bare `cargo test --package kernel` fails.

`cargo xtask test` builds three times on purpose: once for the clean boot, once with
`GREY_OS_PANIC_TEST` set so the panic path runs, and once more without it, so `target/`
is left holding the image every other command boots.
