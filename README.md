# Grey OS

A hobby operating system written in Rust for x86_64, booting via UEFI.

> **Status: early development.** The kernel boots in QEMU and paints the screen. Nothing else works yet. Do not run this on hardware you care about.

## Goals

- Learn and build a real OS kernel in Rust, with `unsafe` kept small and documented.
- Boot on real UEFI hardware, not just emulators.
- Grow toward userspace programs, a filesystem and a simple desktop.
- Coexist with Windows in a dual-boot setup (Windows stays for games that need its anti-cheat).

## Current state

- [x] Boots in QEMU through UEFI firmware
- [x] Framebuffer fill (pixel-format aware)
- [ ] Serial logging and a printing panic handler
- [ ] Framebuffer text console and `println!`
- [ ] Interrupts and exception handling
- [ ] Memory management (frame allocator, paging, heap)
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

This builds the kernel, wraps it in a bootable UEFI disk image with the `bootloader` crate, and launches QEMU. On success you will see the bootloader log briefly, then a blue-teal screen drawn by the kernel.

The first build is slow because it compiles the bootloader and fetches firmware.

## Project layout

```
grey-os/
├── Cargo.toml            # workspace root + image builder package
├── build.rs              # builds the kernel and creates UEFI/BIOS disk images
├── rust-toolchain.toml   # pinned nightly toolchain
├── .cargo/config.toml    # enables the unstable artifact-dependency feature
├── src/bin/qemu-uefi.rs  # launches the UEFI image in QEMU
└── kernel/               # the OS kernel (no_std)
    ├── Cargo.toml
    └── src/
        ├── main.rs       # entry point, halt loop, panic handler
        └── framebuffer.rs
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
