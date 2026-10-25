# Roadmap

Grey OS is built one milestone at a time. A milestone is only marked done when its
acceptance criteria are demonstrated by output, not by code existing: the proof is
a `xtask test` run or a QEMU session whose serial log is pasted into the commit
message. No milestone is started before the previous one is accepted.

Scope for M0 and M1 is agreed. M2 onward is stated as themes in dependency order:
the authoritative wording comes from the engineering brief and is transcribed here
milestone by milestone, so that this file never claims more than was agreed.

## Status

| Milestone | Theme                                        | Status                                                                |
| --------- | -------------------------------------------- | --------------------------------------------------------------------- |
| M0        | Boot, serial diagnostics, diagnosable panics | **In progress - code complete, awaiting the panic-path gap to close** |
| M1        | Display primitives, text, on-screen boot log | Not started                                                           |
| M2        | Interrupts, exceptions, timers               | Not started                                                           |
| M3        | Physical and virtual memory management       | Not started                                                           |
| M4        | Heap allocation and kernel data structures   | Not started                                                           |
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

**Open.**

- [ ] Automated panic-path test. Blocked on giving the kernel a way to be told to
      fail: the plan is a `GREY_OS_PANIC_TEST` build-script flag with
      `cargo::rerun-if-env-changed`, which needs the kernel to own a build script.
      Natural to land in M1. Until then the panic path is verified by hand.
- [ ] BIOS image boots. Built and wired to `cargo xtask run --bios` but never run.

**Acceptance criteria.**

1. `cargo xtask test` exits 0 having seen `grey-os: boot ok` and QEMU status 33.
2. A forced panic prints `[panic] <message>` and its location, paints the screen
   0xb01b2f, and produces QEMU status 35. Verified manually until the item above
   closes.
3. A missing framebuffer produces `FATAL: boot info carries no framebuffer` rather
   than a silent hang or a black screen.

## M1 - Display primitives and text

**Goal.** Replace the serial-only boot log with something on screen.

- GOP framebuffer wrapper owned by the kernel, not the raw boot info struct.
- Double buffering: draw off-screen, present once per frame.
- Rect fill, blit, and alpha blending, all `no_std` and allocation-free where practical.
- TrueType rasterisation in `no_std` with a glyph cache.
- Open-licence fonts only - Selawik, Open Sans, Noto. Licence audit recorded in `DECISIONS.md`.
- The M0 boot log rendered on screen through the new text path.
- Kernel build script, closing the `GREY_OS_PANIC_TEST` gap above.
- Host-side unit tests for blending and glyph-metric maths (kernel split into
  `lib.rs` + thin `bin`), so pure functions are tested without booting.

**Acceptance criteria.** The boot log is legible at the firmware-selected
resolution; a panic paints the panel red with the message rendered in text; screen
output is presentable without dropping frames on a 1280x800 surface.

## M2 onward - provisional themes

The ordering above is dependency-driven and each theme is a milestone of its own.
Detailed scope, acceptance criteria and verification are added when the milestone
is approved, per the brief's one-at-a-time rule.

## Hardware compatibility

| Target                   | State                                                             |
| ------------------------ | ----------------------------------------------------------------- |
| QEMU UEFI (OVMF), x86_64 | Primary target. Boots, logs, exits cleanly.                       |
| QEMU UEFI, 1280x800      | What OVMF actually selects; the kernel logs whatever it is given. |
| QEMU BIOS (SeaBIOS)      | Image built, invocation wired, never executed.                    |
| Real x86_64 hardware     | **Not ready.** The UEFI image boots from the root of a FAT32      |

stick with secure boot disabled, but with no filesystem or storage driver there is
nothing to persist and no installer. Deferred until M5. |

A dual-boot machine's existing bootloader and partitions must survive every step:
no tool in this repository writes to a disk other than an image file under `target/`.

## How verification works

```bash
cargo fmt --all                                              # formatting
cargo clippy --workspace --exclude kernel                     # host tools and xtask
cargo clippy --package kernel --target x86_64-unknown-none    # the kernel itself
cargo xtask test                                             # boots headless and asserts
```

The third line is not optional and not interchangeable with the second: the kernel
is `x86_64`-only `no_std` code, so linting it for the host architecture fails on
inline-asm register names and on `bootloader_api::entry_point!`. See `DECISIONS.md`.
