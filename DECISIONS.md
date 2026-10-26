# Decisions

Architecture decisions for Grey OS, in the order they were made. Each records the
context, the choice, and what it costs. A decision is only revisited when its
consequences actually bite - the point of writing them down is to stop re-arguing
them every milestone.

## ADR-001: Build the image with artifact dependencies (bindeps)

**Status:** accepted, M0.

**Context.** The kernel must be built for `x86_64-unknown-none` while the image
builder runs on the host. Before artifact dependencies this meant a shell script or
an xtask that shelled out to `cargo` twice with different `--target`s.

**Decision.** Declare the kernel as a build-dependency with `artifact = "bin"` and
`target = "x86_64-unknown-none"`, and read it from `CARGO_BIN_FILE_KERNEL` in
`build.rs`. `cargo build` is then the whole build.

**Consequences.** Requires a nightly toolchain and the unstable `bindeps` feature.
One command builds everything, and the toolchain is pinned in `rust-toolchain.toml`
so the feature cannot silently disappear. `cargo build --workspace` is no longer a
meaningful command - see ADR-006.

## ADR-002: Hand-rolled COM1 driver instead of `uart_16550` + `spin`

**Status:** accepted, M0. Revisit at M2 if interrupts touch it.

**Context.** M0 needed a serial console. The obvious route is the `uart_16550` crate
with a `spin::Mutex`. Both would have been added without being able to read their
documentation in this environment, and the diagnostic channel is the one component
that must work before anything else can be debugged.

**Decision.** Write the 16550 in roughly 60 lines of `asm!` port I/O plus an
`AtomicBool` lock, in `kernel/src/main.rs`.

**Consequences.** No dependency risk on the most critical path, and the whole driver
is auditable in one screen. The cost is that output is polling-based (ADR-004), that
the driver has no input direction yet, and that the `mod` lives in `main.rs` rather
than its own file until files can be created.

## ADR-003: Publish the framebuffer as an address, not a guarded reference

**Status:** accepted, M0.

**Context.** The panic handler needs to paint the screen. A panic can happen while
any lock is held, so taking a mutex there risks deadlocking the only failure path.

**Decision.** `framebuffer::install` stores a `*mut FrameBuffer` in an `AtomicPtr`
with `Release`; `with_screen` loads it with `Acquire` and hands out a `&mut` for the
duration of a closure.

**Consequences.** The panic path never blocks. Safety rests on the invariant that the
boot GOP allocation outlives the kernel and that `with_screen` is only reachable from
boot and from the panic path, never concurrently. When double buffering lands in M1
this becomes a real ownership problem and this ADR is the one to reopen.

## ADR-004: Bounded polling, never an unbounded wait, in the UART

**Status:** accepted, M0.

**Context.** `while (inb(LSR) & 0x20) == 0 {}` is the textbook transmit routine. On
hardware where the UART is absent or its clock is wrong, it is also an infinite loop
during boot - the exact failure that makes a hobby OS undebuggable.

**Decision.** Poll at most `TX_SPINS` times, then drop the byte. The console is a
diagnostic convenience, not a protocol.

**Consequences.** Boot can never be lost to a missing UART. Truncated output on real
hardware is possible and acceptable; the alternative loses the whole log.

## ADR-005: QEMU exit codes through `isa-debug-exit`

**Status:** accepted, M0.

**Context.** A kernel that ends in `hlt` can only be stopped by a human or a timeout,
which makes automated verification awkward and slow.

**Decision.** The kernel writes 0x10 or 0x11 to port 0xf4; QEMU translates that to
`(value << 1) | 1`, so 33 means a clean boot and 35 a kernel-reported failure. The
runner and `xtask test` both check for it, and check the serial marker as well - a
clean status with an empty log is a failure, not a pass.

**Consequences.** Verification is fast and unambiguous. Port 0xf4 is meaningless on
real hardware, where the write is discarded and the halt loop runs as intended.

## ADR-006: The kernel is linted for its own target, from one workspace

**Status:** accepted, M0. This one cost real time and is worth remembering.

**Context.** `cargo clippy --workspace --all-targets` reported eight errors: invalid
`asm!` registers `dx`/`eax`/`al`, `entry_point` missing from `bootloader_api`, and
`unwinding panics are not supported without std`. None were kernel bugs - clippy was
compiling the kernel for the host (`aarch64-apple-darwin`), where `bootloader_api`'s
`entry_point!` is configured out and x86 register names do not exist.

**Decision.** Keep the kernel in `workspace.members` so it can be linted explicitly,
set `default-members = ["."]` so bare commands mean the host tools, and use:

```bash
cargo clippy --workspace --exclude kernel                   # host tools and xtask
cargo clippy --package kernel --target x86_64-unknown-none  # the kernel
```

**Consequences.** Any CI that runs `--workspace` will fail for the wrong reason.
Those two lines are the contract, and ROADMAP.md repeats them.

## ADR-007: OVMF vars drive is copied per run

**Status:** accepted, M0.

**Context.** QEMU writes UEFI boot variables back into the vars drive. Sharing one
file makes each run depend on the previous ones: a stale boot entry, a changed boot
order, or a full NVRAM changes the next run's behaviour.

**Decision.** Reuse the read-only code drive, copy the vars drive to
`target/grey-os/ovmf-vars.fd` before every boot.

**Consequences.** Runs are reproducible and `xtask test` cannot be poisoned by a
previous manual session. Costs one 500 KiB copy per boot.

## ADR-008: `-nic none` and no `-machine q35`

**Status:** accepted, M0.

**Context.** A default NIC adds a PCI option ROM, changes firmware boot order, and
costs a DHCP wait before boot. `-machine q35` would modernise the chipset but changes
the PCI layout, and the current disk is attached as an IDE drive on an i440fx layout.

**Decision.** Ship neither. Keep the machine default and attach no network device.

**Consequences.** Faster, quieter, deterministic runs. The machine will need to move
to q35 eventually - real hardware is UEFI with PCIe - and that is a deliberate change
for a later milestone, not a default to drift into.

## ADR-009: BIOS image is built but not verified

**Status:** accepted, M0.

**Context.** `DiskImageBuilder` produces a BIOS image for free, and legacy boot is the
cheapest fallback for an old machine.

**Decision.** Build it, stage it alongside the UEFI image, wire it up as
`cargo xtask run --bios`, and state plainly that it has never been executed.

**Consequences.** No false confidence. It is one command away from being tested, and
`xtask test` does not touch it.

## ADR-010: The kernel logs the geometry firmware gives it

**Status:** accepted, M0.

**Context.** The brief specifies 1280x720. GOP mode selection is the firmware's to
make: OVMF gives this machine 1280x800, stride 1280, Bgr, 4 bytes/pixel.

**Decision.** Never assume a resolution. Log `width`, `height`, `stride` and
`bytes_per_pixel` at boot and drive every draw from those values. All coordinate
maths is checked arithmetic, because those numbers come from firmware and a wrong
stride writes past the buffer.

**Consequences.** Rendering is correct on any mode firmware picks. If a fixed
resolution is ever genuinely required, it is a `bootloader` configuration entry, not
a kernel assumption.

## ADR-011: One QEMU command line, eventually

**Status:** open, M0.

**Context.** The argument list now exists twice: `src/bin/qemu-uefi.rs` for a human,
`xtask` for the test run. Two copies drift.

**Decision deferred.** `xtask` is meant to replace the runner outright, and the runner
deleted. It is kept for now because it works and because deleting the only working
path in the same milestone that introduces its replacement is how a project loses a
day.

## ADR-012: Automated panic test is deferred, deliberately

**Status:** open, M1. Approved to land in M1 - see ADR-019.

**Context.** `cargo xtask test --panic` was planned for M0. Forcing a panic requires
rebuilding the kernel with a different flag, and artifact-dependency caching will not
reliably notice an environment change inside the kernel's own build, so the test would
quietly re-run the good image and report green on a path it never exercised.

**Decision.** Do not ship it in M0. Land it in M1 on a kernel build script with
`cargo::rerun-if-env-changed=GREY_OS_PANIC_TEST`, which makes the rebuild real.

**Consequences.** M0's panic path is verified by hand and recorded as such. A test
that cannot fail is worse than no test.

## ADR-013: Fonts and licence

**Status:** open, M1.

**Context.** M1 renders text, which means shipping font data. The project licence is
still unchosen (README says so), and font licences must be compatible with it.

**Decision deferred.** Only open-licence fonts - Selawik (SIL OFL), Open Sans
(Apache-2.0), Noto (SIL OFL) - with the licence text vendored next to the font files.
Project licence proposal: dual MIT / Apache-2.0, matching the Rust ecosystem.

## ADR-014: `xtask` resolves paths from `CARGO_MANIFEST_DIR`, not the cwd

**Status:** accepted, M0.

**Context.** The first `cargo xtask test` built the images, then reported
`target/grey-os/grey-os-uefi.img is missing after a successful build`. Both halves
were true: `build.rs` staged the images under the workspace target directory, while
`xtask` checked a relative path resolved against its own working directory, and
`cargo run` makes no promise about what that directory is.

**Decision.** Derive the workspace root at compile time from
`env!("CARGO_MANIFEST_DIR")` - the parent of `xtask/` - and make every path in the
tool absolute from there. The nested `cargo build` is also given
`.current_dir(workspace_root())`, so the two sides cannot disagree. A missing-image
failure now prints the absolute path plus a listing of the directory, so the next
mismatch names itself instead of costing a debugging session. Absolutising the
paths was necessary but not sufficient; the other half of the fault is ADR-015.

## ADR-015: `build.rs` finds the target directory by name, not by ancestor count

**Status:** accepted, M0.

**Context.** With ADR-014 in place, `cargo xtask test` still reported the image
missing - now with the correct absolute path and a listing of the directory
showing only `ovmf-vars.fd`. `build.rs` located its target directory with
`out_dir.ancestors().nth(4)`, which hard-codes the layout
`<target>/<profile>/build/<package>-<hash>/out`. Cargo also uses
`<target>/<profile>/build/<package>/<hash>/out`, one level deeper, so `nth(4)`
resolved to the _profile_ directory and both images were staged in
`target/debug/grey-os/` instead of `target/grey-os/`.

Nothing failed, which is why it survived: the wrongly resolved directory
existed, so `.context()` never fired; and `cargo run` reads `UEFI_IMAGE`
straight out of `OUT_DIR`, so every boot up to that point came from the
canonical copy and never touched the staged ones. The staging code was correct
looking and completely unexercised. `touch build.rs` (defeating
`rerun-if-changed` and so ruling out a cached build-script replay), then
`ls target/grey-os` and `find . -name 'grey-os-*.img'`, placed the images in a
single command.

**Decision.** Walk `OUT_DIR`'s ancestors to the nearest component named `build`
and step up two levels from there, rather than indexing ancestors by depth.
Stage into `<target>/grey-os/`.

**Consequences.** The staging path survives either nesting depth and a
relocated `CARGO_TARGET_DIR`, and the guesswork is gone rather than corrected
by one. The staging directory is not profile-scoped, so a release build would
overwrite the debug images; acceptable while only one profile is in use, and
the thing to revisit if a release profile is ever shipped.

**Verification.** `touch build.rs && cargo build --package grey-os`, then
confirm both images exist in `target/grey-os/`; `cargo run --package xtask --
test` must print `pass:`.

## ADR-016: the OVMF cache path is absolute as well

**Status:** accepted, M0.

**Context.** ADR-014 absolutised the image and vars paths but left
`Prebuilt::fetch(Source::LATEST, "target/ovmf")` relative, which ADR-014 called
out as a live trap. The first fully green `xtask test` transcript showed it
plainly, in the one place anyone would look:

```text
"-drive" "format=raw,if=pflash,readonly=on,file=target/ovmf/x64/code.fd"
"-drive" "format=raw,if=pflash,file=/Users/.../target/grey-os/ovmf-vars.fd"
"-drive" "format=raw,file=/Users/.../target/grey-os/grey-os-uefi.img"
```

One relative path among two absolute ones. It works only because `cargo run`
happens to set the workspace root as the working directory; from any
subdirectory the firmware code drive would not resolve, and the failure would be
QEMU complaining about a missing file rather than anything naming `xtask`.

**Decision.** Resolve the fetch directory through `workspace_root()`.

**Consequences.** Every path in the command line is now absolute, so the tool
behaves identically from any cwd and ADR-014's claim holds for the whole command
line rather than the parts that happened to be fixed first. The separate
`src/bin/qemu-uefi.rs` runner still resolves its OVMF cache relatively; it is
superseded by `xtask` (ADR-011) and is a candidate for removal rather than
repair.

## ADR-017: The heap arrives in M1, and M4 is re-scoped

**Status:** accepted, M1.

**Context.** M1 needs a back buffer at whatever resolution firmware picks (ADR-010
forbids assuming 1280x720), which is 4096000 bytes on the `1280x800` mode OVMF
currently gives us, plus a glyph cache that grows as text renders. Neither is
expressible without allocation. ROADMAP.md placed the heap at M4, so following the
roadmap literally would leave M1 with an M1-sized problem and no M1-sized solution.

**Decision.** Pull `bootloader_api`'s `allocator` feature and `alloc` into M1,
backed by a `GlobalAlloc` that counts allocations and bytes. M4 keeps the parts
that are genuinely memory management: the physical frame allocator, paging
control, the kernel/user split, and taking over mapping from the bootloader if the
need arises.

**Consequences.** M1 can render at the size it is given instead of at a size baked
into the binary. A ~4 MiB allocation on a machine whose page tables we do not yet
manage is a real risk, mitigated by allocating once at boot and by the counters
making a leak visible rather than inferred. M4 shrinks to mechanisms that are
still missing; it does not disappear, and this ADR is the one to reopen if
allocating before M4 turns out to hide a paging bug.

## ADR-018: Selawik first, with Open Sans as the fallback

**Status:** accepted, M1.

**Context.** The brief asks for a Metro-inspired language and open licences only.
Metro's type is Segoe UI, which cannot be shipped. Selawik is Microsoft's
OFL-licensed metric-compatible substitute for Segoe UI, so it gets the design
language without the licensing problem. Open Sans covers the cases where Selawik's
coverage runs out.

**Decision.** Selawik is the primary face, Open Sans the fallback, and Noto the
escape hatch if coverage ever has to beat fidelity. Both licences are vendored next
to the font files.

**Consequences.** M1's text step begins by vendoring Selawik and its OFL text, not
by writing a rasteriser against a font we cannot ship. If either face changes terms
this ADR reopens rather than being worked around.

## ADR-019: The automated panic test lands in M1

**Status:** accepted, M1. Supersedes the "deferred" framing of ADR-012.

**Context.** ADR-012 deferred this for a specific reason: an `xtask test --panic`
would assert against a kernel artifact that artifact-dependency caching would not
necessarily rebuild, so it could report green having never executed the panic path.
That reasoning still holds - which is why the test is only worth adding alongside
the wiring that makes it real.

**Decision.** Land it in M1. The kernel gets its own `build.rs` emitting
`cargo::rerun-if-env-changed=GREY_OS_PANIC_TEST`, so flipping the flag genuinely
rebuilds the image and a green result means the panic path ran. `xtask` grows the
matching mode.

**Consequences.** The kernel goes from having no build script to having one whose
only job is to make a test honest. Until this lands the panic path stays _manually_
verified (message, source location, red screen, exit 35) and is not described
anywhere as tested.

## ADR-020: The framebuffer adapter validates the layout once

**Status:** accepted, M1.

**Context.** M0 drew with a `fill` that re-checked firmware numbers per pixel
(`checked_mul` on stride, pixel size and offset, then `get_mut`), carried a `Color`
type of its own, and switched on `PixelFormat` for every pixel it wrote. All of that
was needed only because nothing had yet proved the layout was sane. With `Canvas` in
place, the checks belong where they run once rather than once per pixel.

**Decision.** `framebuffer::describe` maps the firmware's `PixelFormat` to a
`ChannelOrder` and its numbers to a `Geometry`; `Canvas::new` then rejects the geometry
unless it fits the buffer, and every draw after that is in bounds by construction. The
palette format `U8`, and any format this renderer does not recognise, is refused rather
than guessed at: `fill` returns `false` and the kernel logs a FATAL and exits, because
painting noise over the panel is worse than refusing to boot. `install` now takes
`&'static mut FrameBuffer`, so the pointer the panic handler reads is justified by a
type instead of by a comment.

**Consequences.** One `Color`, one `fill`, and one place that decides byte order.
`write_pixel` and its per-pixel `match` are gone, along with the U8 luminance path. The
cost is that a palette framebuffer cannot boot at all; no GOP implementation in
practice reports one, and the failure is loud and specific rather than a corrupted
image. The channel order itself is verified once by eye (`cargo xtask run` must be teal,
not orange-brown) and afterwards by the unit test that encodes one colour both ways,
since `xtask test` runs with `-display none` and cannot see the screen.

## ADR-021: The kernel writes its own bump allocator

**Status:** accepted, M1. Qualifies ADR-017.

**Context.** ADR-017 assumed the bootloader would supply the heap. Checked against the
vendored source, `bootloader_api` 0.11.17 contains no allocator code and does not
mention one: the heap is ours to build from `boot_info.memory_regions`. M1's allocation
pattern is append-only - one back buffer at whatever resolution firmware picked, then a
glyph cache that only grows - which is the one pattern a bump allocator serves exactly.

**Decision.** A `GlobalAlloc` handing out memory from a single region chosen by
`memory::pick_heap_region`, with `dealloc` as a no-op and counters (allocations, bytes
in use, high-water mark) logged at boot. Roughly thirty auditable lines rather than a
dependency, following ADR-002's precedent: an allocator bug is silent memory
corruption, the worst kind of risk to take on something unread. The region search takes
the framebuffer's physical span and refuses any overlap, because firmware may report
that RAM as usable: it is ordinary RAM that happens to be on screen.

**Amended, M1.** That guard is inert on this bootloader. `bootloader_api` 0.11 publishes
the framebuffer only as a virtual address the bootloader chose, never the physical frames
behind it, so the boot sequence passes `None` and the log says so out loud. Subtracting
an unrelated offset to invent a span would be worse than having none: a made-up range
reserves the wrong pages and looks like protection. The guard stays because it is tested
and because the real span arrives with a page-table walk (M3). ADR-022 records the
mapping that makes the heap reachable at all.

**Consequences.** Frees are not honoured, so any code that allocates and drops in a
loop leaks until the region is exhausted. This ADR reopens at the first `dealloc` that
matters, or the first per-frame allocation, whichever comes first; the fallback is
`linked_list_allocator` or a size-class allocator, and the counters are what will say
which is needed. Until then nothing may allocate in a render loop, which M1 does not.
Because a heap that is only *logged* proves nothing, the boot sequence allocates one
4 KiB `Box`, fills it with a position-dependent pattern and reads it back; a wrong
offset faults there instead of inside a driver later.

## ADR-022: The kernel asks the bootloader for a window onto physical memory

**Status:** accepted, M1. Amends ADR-021.

**Context.** `BootInfo::memory_regions` reports _physical_ ranges, and the heap has to be
handed addresses it can dereference. `Mappings::new_default` leaves
`physical_memory` as `None` - its own doc comment says "Defaults to `None`, i.e. no
mapping of the physical memory" - and the bare `entry_point!(kernel_main)` in use was
therefore equivalent to that default. The first boot with the allocator wired up ended
at `FATAL: boot info carries no physical memory offset, so the heap cannot be reached`.
Before this, the only addresses the kernel could touch were the three the bootloader had
chosen for it: the kernel image, the boot info struct and the framebuffer.

**Decision.** `kernel/src/main.rs` declares its own `BOOTLOADER_CONFIG` and passes it to
`entry_point!(kernel_main, config = &BOOTLOADER_CONFIG)`, setting
`config.mappings.physical_memory = Some(Mapping::Dynamic)`. `Dynamic` rather than a
`FixedAddress`: the bootloader already tracks used level-4 entries, so it picks a free
2 MiB-aligned window, marks it used, and publishes the address in
`BootInfo::physical_memory_offset`. No address is hardcoded, and a collision with the
kernel's own mapping is impossible by construction. The offset is read in exactly one
place - the heap setup - and each physical span is converted through it there, so the
allocator never sees a physical address (ADR-021).

**Consequences.** The kernel can now touch any physical frame. That is what a frame
allocator and a page-table walk will need, and it also means a wrong pointer no longer
reliably faults: `unsafe` around memory became load-bearing rather than ornamental. The
mapping covers all reported memory or 4 GiB, whichever is larger, in 2 MiB pages, so the
bootloader builds a few thousand page-table entries before the kernel starts. Under QEMU
it picks 0x280_0000_0000 (2.5 TiB), which the boot log shows as
`heap ready 0x28001780000..0x2801bb6c000`. A mapping that merely appears in `BootInfo`
proves nothing, so the boot sequence writes a 4 KiB page through the allocator and reads
it back before drawing anything.

**Not covered.** The framebuffer keeps its own virtual address, so its physical frames
are still unknown and ADR-021's overlap guard still has no span to enforce. Closing that
needs the page-table walk, which is M3's job, not M1's.

