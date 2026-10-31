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

**Outcome (M1).** Executed at last, and the honest result is that it boots but does not
render. SeaBIOS loads the image, the bootloader maps the kernel and jumps to it, and the
kernel reaches its framebuffer check with COM1 up - then refuses the surface VBE offers:
`Bgr, 3 bytes/pixel, 1280x720`, a 24-bit mode, where the renderer only draws 32-bit pixels
(`canvas::BYTES_PER_PIXEL`). So the image is executed and the failure is the kernel's own
stated limitation rather than an untested path. Teaching `Canvas` 24-bit surfaces, or asking
VBE for a 32-bit mode, is work for a milestone that cares about BIOS; the UEFI path is what
M1 is verified on.

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


## ADR-023: The panic path is tested through a build-script cfg

**Status:** accepted, M0 (landed during M1).

**Context.** M0's second acceptance criterion - a forced panic prints `[panic] <message>`
and its location, paints the screen, and reports status 35 - was verified by hand, which
means it was verified once. Automating it needs a kernel that can be told to fail, and a
`no_std` kernel has no argv, no environment and no test harness: whether it panics has to
be decided when the image is built, not when it boots.

A cargo feature (`panic-test`) is the idiomatic Rust answer and cargo fingerprints it
properly. It loses here for two reasons. The kernel is reached only as an artifact
dependency (ADR-001), so turning a feature on from `xtask` means either editing the root
manifest for the run or passing `--features` to the whole build - machinery to reach a
dependency's feature. And features are additive: `--all-features`, a plausible thing for a
CI config to do, would then produce a kernel that panics on every boot.

**Decision.** `kernel/build.rs` maps a non-empty, non-`0` `GREY_OS_PANIC_TEST` to
`cargo::rustc-cfg=grey_os_panic_test`, declares the cfg with `rustc-check-cfg`, and
re-runs when the variable changes. `kernel/src/main.rs` gates one injected `panic!` on
that cfg, placed after `framebuffer::install` so the panel half of the path runs, and
gates the success exit on the other side of the cfg so neither build contains unreachable
code. `cargo xtask test` then builds with the flag, boots, and asserts `[panic]`, the
location, the message, `[panic] panel painted red` and status 35 - and rebuilds without
the flag so `target/` keeps the image every other command boots.

**Consequences.** The panic path is covered by the command this project treats as truth,
and the red panel is verified from outside the guest through the kernel's own report line,
which is the only way: nothing here can read the framebuffer. The builds in `cargo xtask test`
cost a couple of seconds each and `--no-build` is refused for `test`,
because the flag has to be compiled in. The injected message is duplicated as a string in
`xtask`, so editing it in `main.rs` fails the test loudly instead of passing silently. The
kernel is also no longer a package without a build script: `rerun-if-env-changed` and
`rustc-check-cfg` have to stay correct, or the flag silently does nothing and the test
passes against a stale artifact - which is why `test` also fails when a panicking build
still reports `boot ok`.

## ADR-024: Public-domain 8x8 bitmaps for the first console font

**Status:** accepted, M1.

**Context.** Text on a panel needs glyphs, and there were three ways to get them. A font
crate would add a dependency whose licence and API have to be audited for the most basic
thing the kernel does, and the diagnostic path has to work before anything that could help
debug a dependency. A TrueType stack is a milestone item of its own: it needs an
allocator-backed glyph cache, `cmap`/`glyf`/`loca` parsing and a licence decision for a
real typeface. Neither is needed to put a boot log on the screen.

**Decision.** Embed the printable ASCII subset of `font8x8_basic.h` - Daniel Hepper's
public-domain 8x8 bitmaps, themselves derived from Marcel Sondaar's public-domain IBM VGA
fonts - as a `static` table in `kernel/src/font.rs`, with provenance, fetch date and the
upstream sha256 in the header, and draw it at an integer scale (`text.rs`). The scale is
data rather than a constant: 2 gives a 16x16 cell, which is readable on a 1280x800 panel,
and 1 or 3 need no new code.

An Apache-2.0 8x16 face (the Verilog-encoded IBM VGA font) was considered for the better
proportions and rejected: shipping it means shipping a licence notice beside a data blob
in a repository that carries no licence text at all, for a gain of eight rows of pixels
that the scale factor covers adequately.

**Consequences.** Public domain means no licence file, no attribution obligation and no
audit to repeat when the table is regenerated; the source is credited anyway. The cost is
that 8x8 at 2x is chunkier than the 8x16 face DOS actually used, and that only ASCII is
covered - anything else draws a visible box rather than a silent gap, which is honest and
greppable. Swapping in a 8x16 table or a TTF rasteriser later touches `font.rs` and
nothing else, because layout code only knows the cell size. Bit order is the source's
(bit 0 leftmost) rather than a convention chosen here, so the table stays diffable against
upstream; `F`'s first row is the test that pins it down, since mirrored `A` or `O` look
perfectly plausible.


## ADR-025: The boot log goes to a heap-allocated cell grid, painted by dirty rows

**Status:** accepted, M1.

**Context.** Printing to a panel needs a text model. Immediate mode - draw each line at a
cursor and never look back - cannot scroll, because the pixels above are the only copy of
the earlier lines. A ring of formatted strings costs a format per line and cannot render
twice with a different background. A grid of characters costs one byte per cell and makes
wrapping, `\r`, tabs and scrolling arithmetic instead of pixel work, which also means they
can be tested on the host.

Two smaller questions came with it. Where the grid lives: it cannot be a local in
`kernel_main`, because the panic handler has no caller to borrow it from. And how a
repaint stays cheap: the firmware's memory map is about 110 lines on a 512 MiB guest, far
more than the 48 rows a 1280x800 panel holds at scale 2, so most lines arrive after the
screen is full and every one of them scrolls.

**Decision.** `console.rs` is a pure grid - `Vec<u8>`, cursor, wrap, scroll, dirty-row
tracking - allocated once at boot from the bump allocator and covered by host tests.
`display.rs` (x86_64) owns the single instance behind an `AtomicPtr`, exactly like the
framebuffer, so `println!` and the panic handler reach the same log without passing
references. Logging writes the text, appends the newline and repaints only the rows marked
dirty; a scroll copies the pixels up with `Canvas::scroll_up` (a `copy_within` per row) and
repaints the two rows that moved. A panic fills the panel 0xb01b2f, appends the report to
the grid and repaints everything with red as the background, so the log stays visible above
the message. The boot sequence also had to be reordered: discovery first, logging after
`display::init`, so the panel receives the whole log rather than its tail.

**Consequences.** Writing a character never allocates, which is what makes the console
usable from the panic handler after something has gone wrong. Wrapping, tabs and scrolling
are unit-tested rather than eyeballed, and the boot log was read back off a QEMU screenshot
by matching cells against the font table - the check that the panel really says what the
serial port says. Costs, all measured: a full repaint per line took 12.7 s of emulated boot
before dirty rows and the pixel-copy scroll, 3.5 s after, of which about 2 s is firmware.
The remaining per-glyph cost is why a back buffer and a proper `blit` are still on M1's
list. The first two log lines (serial online, and any failure before the heap exists) are
serial-only by nature, and the grid is sized at boot, so a mode change would need a new
console.

## ADR-026: The console draws into a back buffer and the panel receives the rows that changed

**Status:** accepted, M1.

**Context.** ADR-025 put the log on the panel but painted it glyph by glyph directly into the
firmware's framebuffer. Three things came with that. A line was visible while it was being
drawn, so a screenshot - or a human - could catch half a glyph. Every pixel a glyph wrote was
a separate write into panel memory. And the last M1 criterion, "presentable without dropping
frames", cannot be met while drawing and presenting are the same operation, because there is
nothing to present *from*.

**Decision.** `display.rs` allocates one full-screen surface from the bump heap at boot -
4,000 KiB on the 1280x800 panel OVMF picks - with the panel's own `Geometry` and
`ChannelOrder`, and the console draws into it. `Canvas::blit` then copies onto the panel only
the area that changed: the dirty band for an appended line, the whole grid after a scroll,
because a scroll moved every row of it. `blit` is a `copy_from_slice` per row, so the panel is
written in bands instead of per pixel, and it refuses two surfaces whose channel order or
pixel size differ - a byte copy would put red where blue belongs, and copying nothing is the
only honest answer. `Console::mark_painted` lets a full `render` leave the grid current, so
the line after a repaint costs one row rather than a screen. A failed allocation, or a boot
before the heap exists, falls back to the ADR-025 path of drawing straight onto the panel, and
the boot log names the one in use.

**Consequences.** A line now reaches the panel as one band copy after it is complete in RAM,
which is what makes a repaint presentable rather than watchable, and the cost of a line no
longer grows with the number of pixels its glyphs touch in panel memory. Measured: emulated
boot 3.49 s against 3.5 s before (about 2 s of which is firmware), 4 MiB of a 430 MiB heap,
one extra `memcpy` of roughly 1 MB per scroll. The buffer doubles the memory a screen needs,
so a much larger panel would want a bounded one - a band buffer rather than a full surface -
which is a change to the same three functions when it is needed. The panic path had to be told
the new background: verifying this with a screendump showed the grid area still on the boot
teal, because `paint` had always used `BACKGROUND` and only the margins were red - so
ADR-025's "paints the screen 0xb01b2f" was true only around the text, and now it is true of the
whole panel.

## ADR-027: M1 closes without TrueType, and the item moves to M7

**Status:** accepted, M1 scope.

**Context.** ROADMAP.md listed TrueType rasterisation under M1, next to the bitmap font, and
M1's three acceptance criteria were then met without it: the log is legible at the resolution
firmware picks, a panic paints the panel and renders the report in text, and a line is
presented as one band copy. What TrueType actually costs is four pieces that only pay off
together - an sfnt parser, quadratic/cubic flattening, a scanline fill, and a glyph cache -
and the cache is the problem: ADR-021's bump allocator never reclaims, so a cache of rasterised
glyphs grows for the whole run and its size is set by the text a program happens to draw. It
also reopens ADR-018's face question, because a scalable face has to be vendored with its
licence text before it can be parsed, tested or shipped. Deferring an item because a milestone
can be accepted without it is a scope decision, not a silent drop, so it is recorded here
rather than left in a list.

**Decision.** M1 is accepted as it stands, on the 8x8 public-domain bitmap font of ADR-024 at
integer scale. TrueType moves out of M1 and into M7 (compositor, windows, theme), which is the
first milestone whose acceptance needs type at more than one size. ADR-018's choice of Selawik
as the primary face stands and is the starting point there, unchanged.

**Consequences.** M1 is done and M2 may start; the console keeps a fixed 16x16 cell on
1280x800 and cannot render non-ASCII or anything the bitmap table lacks. A UI milestone cannot
substitute integer scaling for it: 2x of an 8x8 is the whole fidelity there is, and 3x wraps
the memory map. The item reopens early if a milestone before M7 needs text the bitmap font
cannot express - the answer then is a second font table, not half a rasteriser.

## ADR-028: M2 starts with the CPU's own structures, and handlers touch no state

**Status:** accepted, M2. Restates the invariant ADR-021 relied on.

**Context.** Until now the kernel ran on the bootloader's GDT and IDT. Any CPU exception
therefore went to the bootloader's idea of a handler - in practice a triple fault and a reboot
loop, or a stop that says nothing - so a `#PF` in a driver would have been undebuggable, and
the M0 panic path could only report panics the kernel raised itself. Two questions decide the
design. First, how much state a handler has to save: the exception handlers here diverge into
the panic path, but a timer handler has to return to whatever it interrupted. Second, and more
sharply, reentrancy: `console.rs`, `display.rs`, `framebuffer.rs` and `heap.rs` all hand out
`&mut` on the argument that the kernel is single-threaded and nothing re-enters - an interrupt
handler breaks that argument the moment it prints or allocates, and `heap.rs`'s `unsafe impl
Sync` says so in as many words.

**Decision.** M2 runs in dependency order: GDT with a TSS and an IST stack for `#DF`, then the
IDT with handlers for every exception vector, then the 8259 PIC remapped to 0x20..=0x2F, then
the PIT. Handlers are written with `extern "x86-interrupt"` and the `abi_x86_interrupt` feature
rather than hand-rolled naked stubs: the compiler generates the stub, which is where the
error-code-pushing vectors, `iretq` and register preservation are easy to get subtly wrong,
and the toolchain is pinned (ADR-001) so the feature cannot vanish unnoticed. That feature gate
is the one place M2 takes an unstable dependency, and it buys more safety than it costs.
Exceptions report one line on serial - vector name, error code, faulting instruction - using
`try_print_args` with an unlocked fallback, then panic into the M0/M1 path so the report also
reaches the panel. Interrupt handlers do exactly two things: bump an `AtomicU64` and send EOI.

**Consequences.** `heap.rs` keeps its `unsafe impl Sync` only because handlers do not allocate;
that is now an invariant with a name, and the first handler that wants to allocate has to bring
a lock or a queue with it. Nothing prints from an interrupt handler, so the timer is proved by
the boot path sampling the counter after waiting for it, never by a heartbeat written from the
handler. An exception raised *inside* a console write still leaves the grid torn - the report is
already on serial by then, and the panel repaint is best-effort - which is why the console is
repaired by repainting rather than trusted. `#DF` gets its own 16 KiB stack through the TSS, so
a fault inside a fault handler reports instead of triple-faulting. The 8259 stays until an
APIC is needed, and everything it learns about masking is relearned there.

The ABI has two traps worth naming, both of which the fault tests found. A handler takes its
frame **by value**: `extern "x86-interrupt"` reads its arguments off the interrupted stack, so
the frame lands at `RSP + 8` and a `&InterruptFrame` parameter makes the handler dereference the
saved `RIP` as if it were a pointer to the frame. And `int n` pushes no error code even for a
vector whose hardware fault does, so `#DF` cannot be reached with `int 8`: the handler would read
a return address as its error code. That fault is injected by a real double fault instead - an
assembly stub that moves `RSP` to an address nothing maps and pushes - which also makes the
phase a test of the TSS stack rather than of a software interrupt.

## ADR-029: A bitmap for frames, the bootloader's layout, and a guard page that needs a split

**Status:** accepted, M3. Records the four choices M3's scope left open.

**Context.** M3 owns the address space, and four questions decide how. First, how a free frame
is found: a bitmap, a free list, or a stack of free frames. Second, whether the kernel keeps the
address space the bootloader built or moves itself to a higher half in the same milestone. Third,
how much of the address space the tables describe and in what granule - the bootloader maps the
whole of physical memory, and it does so with 2 MiB pages. Fourth, what policy decides which
pages may execute, given that the framebuffer is ordinary RAM the kernel writes to and the heap
is ordinary RAM the kernel allocates from.

**Decision.** A bitmap, one bit per 4 KiB frame, set when the frame is in use. It covers the
physical address space up to the highest frame that could be handed out or must be kept out of
circulation - usable RAM, the framebuffer, the kernel image - so a frame number is its address
divided by 4096 and `alloc` returns addresses rather than indices. It starts with every frame
marked used, and a frame becomes allocatable only when the boot sequence marks its span free:
the allocator is pessimistic by construction, so a range nobody remembered to hand over is
wasted rather than shared. The bitmap lives on the bump heap and is leaked, because the heap is
already an allocator that works and the bitmap is one long-lived allocation. The cost of that
choice is ordering: the frame allocator cannot run before `HEAP.init`, because nothing can hold
its bitmap before then, and the bitmap occupies heap memory that will never be handed back.

A free list was rejected for two reasons. Its header would have to live inside the frame it
describes, which means writing to memory the kernel has not yet decided it owns, and a corrupted
header is a wild pointer that only shows up at the next allocation. And the question this M3
actually asks most often - "is this frame free?" - is one bit read in a bitmap and a walk down a
list otherwise.

The kernel keeps the bootloader's address layout for M3. It deep-copies the whole tree through
`physical_memory_offset` into frames from the new allocator and loads the copy with `mov cr3`, so
every mapping the bootloader made - kernel, stack, boot info, physical window, framebuffer -
survives unchanged and only the ownership moves. A higher half is deferred: moving the kernel
would also change where the heap, the stack and every pointer the boot info hands over live, and
that is a milestone of its own rather than a step inside this one.

The consequence is worth stating with numbers, because it is the copy's real price: the
bootloader maps a 1 TiB window onto physical memory with 2 MiB pages, which is 1029 page
directories, so owning the tree costs about 1044 frames - roughly 4 MiB of tables. That is
faithful rather than necessary, and a tree built from scratch that maps only what the kernel uses
is the follow-up. Frames behind the inherited tables are not reclaimed either: the memory map
calls them bootloader memory, so the bitmap never offers them, and nothing here pretends to know
their extent.

The walker understands 1 GiB and 2 MiB leaves as well as 4 KiB ones, because the window it has to
translate is made of them, and the copy preserves them rather than expanding them - expanding 1
TiB into 4 KiB entries would cost a quarter of a million tables. The heap's guard page then needs
the opposite: a single 4 KiB hole inside a 2 MiB page, which the tables cannot express until that
page is replaced by a table of 4 KiB entries. `unmap_4k` does exactly that with one allocated
frame, and the split is the reason the guard page exists at all rather than being blocked on the
bootloader's choice of granule.

W^X is inherited rather than decided in M3. The bootloader marks the physical window no-execute,
so the heap and the framebuffer image are already non-executable in the copied tree, and the
kernel text keeps the flags the ELF asked for. Making the kernel's own text read-only and its data
no-execute is blocked on `bootloader_api` 0.11: it publishes the kernel image as one
`kernel_addr`/`kernel_len` range, not per-segment bounds, so the tables cannot name the text
range. Carried forward.

**Consequences.** The boot log reports the frame totals from the memory map, the probe that
allocated eight frames and gave them back, the inherited and the new `CR3` root, the framebuffer's
resolved physical span and the armed guard page; `cargo xtask test` asserts all of them on the
clean boot and adds `GREY_OS_EXCEPTION_TEST=guard` as a fifth fault phase, whose only job is to
write one byte past the heap. That write now faults with error 0x2 - a write to a page that is not
present - and `cr2` equal to the heap's end, which is the first check this kernel has ever had
that its allocator arithmetic is right rather than merely self-consistent. Frame zero is never
handed out either: firmware calls the real-mode low memory usable, and a frame at address 0 is
also what a null pointer reaches through the window.

The walk is checked, not believed. The framebuffer's pointer comes from the bootloader and its
frame from our arithmetic, and nothing else ties them together, so the boot writes a byte through
the virtual pointer and reads it back through the window at the resolved frame; the log line that
follows says it matched. The rest of `paging` is as unsafe as it reads - every entry is a raw
pointer through the window - which is why the host tests cover the entry layout, the index
arithmetic and the walk with a table built by hand, and only the instructions (`mov cr3`,
`invlpg`) are left to QEMU.

What M3 does not reach: nothing frees a frame in anger, so the free path is exercised only by the
probe, and the heap is still the bump allocator it was, so a frame given back cannot be reused by
anything above it. Both belong to M4, where a real allocator gives `dealloc` something to mean.

## ADR-030: A mapper first, a heap that grows in pages, and free lists per size class

**Status:** accepted, M4. Records the four choices M4's scope leaves open.

**Context.** M3 ends with a bump allocator over one fixed region and a frame allocator whose free
path nothing but its own probe has ever run. M4 has to make memory reusable, and four questions
decide how. First, what M4 needs from `paging` that `unmap_4k` did not provide. Second, where the
heap's memory comes from: the one region `pick_heap_region` chose, or frames as they are needed.
Third, how a freed block is found again: free lists per size class, a buddy system, or slab caches.
Fourth, what a bug in the allocator's caller looks like - a double free, a free of an address that
was never allocated, an out-of-memory - because a kernel that trusts its callers never finds out.

**Decision.** The mapper comes first. M4 adds `paging::map_4k` before it touches the heap, because
growth means giving a frame an address and nothing could do that: M3's only table edit took a
mapping away. It is written as `paging::map`, which asks a `Tables` implementation instead of
reading memory, with `arch::map_4k` implementing that trait over the physical window - the same
split as `translate` and `walk`, and for the same reason: creating three levels on the way down to
a leaf is exactly the arithmetic worth testing on the host, where a table is an array. The levels it
creates carry `PRESENT | WRITABLE` and nothing else: a write needs the writable bit at every level,
so a parent without it would silently make a writable leaf read-only, and `NO_EXECUTE` on a parent
applies to everything below it, so putting it there would deny execution to a leaf that asked for
it. A huge page counts as already mapped rather than being split: splitting is `unmap_4k`'s job,
and a mapping that quietly replaced a 2 MiB page would be a surprising amount of work behind a
three-word call.

The heap grows in pages taken from the frame allocator, rather than being one larger region chosen
at boot. The first pages are still the region `pick_heap_region` picked, so nothing about the boot
sequence changes; when the free lists cannot satisfy an allocation, the heap takes a frame, maps it
at the page after its current top, and adds it to the class bookkeeping. A fixed-size heap was
rejected because it turns out-of-memory into a budget someone has to guess in advance, and the guess
would be made by whoever wrote the boot sequence rather than by the workload. The guard page moves
up with the heap: after growth the page above the new top is unmapped, so a write past the end of
the heap still faults, and `GREY_OS_EXCEPTION_TEST=guard` still has something to hit.

Free lists per size class, with the free list threaded through the free blocks themselves. Classes
are powers of two from 16 bytes to 4 KiB, so a block's class is the size of its page's slot rather
than something stored beside it; a request that no free list can satisfy splits a larger block, and
a `dealloc` returns the block to its class. This is the smallest mechanism that makes `dealloc`
mean something, and it is bounded: the search is over a fixed set of classes, and the split is at
most one halving per class. Buddy allocation was rejected as the wrong granularity - a buddy system
here would be a page allocator, and `frame.rs` already is one - and slab caches are the right answer
when one type dominates allocation, which is a measurement this milestone cannot make yet. A page's
class is recorded outside the page, in a descriptor the heap owns, so a block's size is known from
its address and a mismatched free is a bug the allocator can name.

Free is checked, not trusted. A double free, a free of an address that was never handed out, and a
free with a size that does not match the block's class are all refused, counted, and reported in
the boot log rather than corrupting the free list quietly; out-of-memory returns null to
`GlobalAlloc` instead of spinning or panicking, and the boot's probes treat a null as a failure of
the milestone rather than of the machine. The counters exist because the alternative - trusting the
caller - is what makes allocator bugs invisible until a driver corrupts memory in a way nobody can
attribute.

**Consequences.** `paging` gains its first mapping path and `frame::Allocator::free` gains its first
real caller, so the two functions M3 could only demonstrate with a probe are now load-bearing. The
boot log reports the heap's growth in pages, the churn probe's allocations and frees, and the frames
returned to the frame allocator, and `cargo xtask test` asserts them next to M3's lines. The tables
a growth creates are not freed when the heap shrinks, and `unmap_4k` still does not hand back the
frame it unmaps: a mapping taken out of the bootloader's window has no owner to hand it to, and
frame ownership - who may free the frame behind a mapping - is a question the allocator has to
answer first. The tree built from scratch that ADR-029 leaves open stays a stretch item at the end
of M4, after the allocator closes the loop, because it needs table bookkeeping of its own: walking
up from a leaf to free the tables that a removal made empty.

## ADR-031: The heap moves into an arena, and its bookkeeping moves into the blocks

**Status:** accepted, M4. Amends ADR-030 in three places, each learned by building it.

**Context.** ADR-030 said the heap would keep growing inside the region `pick_heap_region` chose,
take its size classes from "a descriptor the heap owns per page", and arm the guard by moving an
unmapped page above the new top. Building that ran into the bootloader's window: every address in
that region is already mapped, in 2 MiB pages, by `Mapping::Dynamic` (ADR-022). A page there cannot
be "taken from the frame allocator" - it already has a mapping and a frame behind it - so growth in
place would either remap a frame over live memory or claim to grow while consuming nothing, which is
exactly the lie the milestone's first criterion is written to catch. And a page descriptor per page
rules out the ordinary case: two small blocks freed next to each other must become one block even
when a page boundary runs between them, so the block has to describe itself wherever it sits.

**Decision.** The heap gets a span of its own: `ARENA_BASE = 0x5000_0000_0000`, `ARENA_BYTES` 64
MiB, reserved as virtual address space and backed one 4 KiB page at a time from `frame::FRAMES`,
mapped `WRITABLE | NO_EXECUTE` into the tree the CPU is running on. Nothing maps that span, so
`arch::map_4k` never has to split a huge page for it and never meets a duplicate entry.

That leaves the ordering problem the arena creates: the frame allocator's bitmap is itself an
allocation, and the arena cannot map its first page without a frame. So ADR-021's bump allocator
stays, for exactly one job - the bootstrap: the bitmap, the console's grid and its 4 MiB back buffer,
all of which exist before the arena does. Its region is frozen the moment the arena is ready: the
`#[global_allocator]` routes to the bootstrap until then and to the arena afterwards, the bootstrap's
blocks are never freed (a bump allocator could not take one back in any order but the reverse), and
only the bytes it actually used are reserved in the frame allocator. The 420000 KiB region M1
reserved for a cursor that used 4000 KiB of it goes back into circulation, which is the first thing
M4's "stop guessing a region" was worth.

Bookkeeping lives in the blocks: a 16-byte header carrying the block's size, the size of the block
before it, a magic word with the free bit, and the payload size the caller asked for. The previous
size is a boundary tag, so a free finds both neighbours in constant time, and the requested size is
what makes a size-mismatched free detectable at all - the block's own size is rounded and normally
larger, so comparing it to the `Layout` would refuse honest frees. Free blocks are threaded onto one
list per size class through their payload, forward and backward, because coalescing has to unlink a
neighbour out of the middle of a list; the class is derived from the block's size rather than stored,
which is why the size has to be in the header. The links sit in memory the previous tenant wrote
arbitrary bytes into, so `insert_free` writes both of them: the first version wrote only the forward
link, and the host tests caught the backward link still holding a freed block's payload.

Free is checked against the header, not against the caller's word: not in the span, not at a header
boundary, magic gone, already free, or a requested size that differs are five ways to be refused, and
each is counted in `bad_frees` and reported at boot. `realloc` shrinks in place by recording the
smaller size and grows by allocating, copying and freeing.

The guard page stops needing to be armed. M3 unmapped one page at the end of the region so that a
write past the heap would fault; an arena that maps only what it has grown into has an unmapped page
above its frontier by construction, and it moves with every growth and every trim. `cr2` on the
resulting `#PF` is therefore the grown heap's end, which is criterion 4 with no `unmap_4k` in the
path. The frame behind a page that is trimmed is looked up by walking the address before the entry
is taken away, because `unmap_4k` still does not say what it unmapped; that is a workaround at the
caller and stays on the open list.

**Consequences.** `heap.rs` keeps ADR-021's bump allocator as the bootstrap and `arena.rs` is the
allocator proper; `main.rs` owns the `GlobalAlloc` that routes between them, the `KernelPages` that
maps and unmaps through `paging`, and the boot sequence's new order: bootstrap, frame allocator,
table switch, arena. `pick_heap_region` still picks the bootstrap's region and `split_guard_page` is
gone with the region guard. The tables a growth creates are never freed and a trim only happens at a
page-aligned top, both recorded as open. The arena's address is a constant, not a layout: M5 or the
first user-space mapping is where that stops being a placeholder.


## ADR-034: A theme is data, a window is a rect and some lines, and damage is a list

**Status:** accepted, M7.

**Context.** The panel has shown one thing since M1: a console that paints its own grid. The
milestone is "compositor, windows, theme", which is three questions that do not have to be answered
in the same place: what things look like, what is on the screen, and what has to be repainted.

**Decision.** Each gets its own module.

`theme::Theme` is a struct of colours and measurements, and everything the compositor draws asks the
theme for it. There are no colours in the compositor and no drawing calls in the theme, which is why
the theme can be checked without a screen - the tests assert that body text is at least a hundred
levels of brightness away from a window's body, that the title bar has room for a line of text, and
that focus is *two* cues (a different title bar and a different edge) rather than one.

`window::Windows` is the list, and the list's order *is* the stacking order: last is front, last is
focused. Raising moves a window to the end, so "what is on top here" is a walk from the back of the
list and there is no z number to keep in step with anything. A window is a title, a rect and lines of
text - enough for the log, the system panel and the shell that comes next, and small enough that
"draw a window" stays one function.

Damage is a list of rects rather than a dirty flag. [`Windows`] pushes the rect every change affected,
[`window::merge`] unions overlapping rects and drops ones another covers or the panel cannot show, and
the compositor repaints exactly those rectangles: background, then every window that reaches into
them, then the pointer. The clipping is load-bearing rather than an optimisation - a compositor that
painted outside the damage would have to paint everything to be correct.

**What building it taught.** Three things, all found by tests rather than by looking at the screen:

1. A full frame has to *clear* the damage list. `draw_all` painted everything and left the pending
   rects in place, so the next `draw_damage` repainted what was already correct - caught by a test
   that snapshots the pixel buffer and expects an empty frame to touch nothing.
2. Focus changes damage two windows, not one. `raise` damaged the raised window and set the focus,
   which left the previously focused window's title bar painted as if it were still in front. The
   test raises the back window and then reads the front one's title bar; it failed until `refocus`
   damaged the window that lost focus too.
3. The first version of the `draw_damage` test moved a window 76 pixels and expected two rects. They
   overlapped, so the merge produced one - which is the merge working, and the test being wrong. It
   now moves far enough to produce two disjoint rects, and the boot's own demonstration reports both
   numbers (`4 damage rects merged into 2`) so neither is invisible.

**Proof, and why it is a picture.** Windows are the first thing this kernel has produced that cannot
be checked by reading a log line: "the compositor ran" is not "the panel shows a desktop". So the
input phase - which is already talking to QEMU's monitor - asks for a `screendump` the moment the
compositor says it is finished, and checks the PPM it gets back: the corners are the theme's desktop
colour, the frame holds at least four distinct colours, and the pointer's fill is somewhere on it.
The file is kept at `target/grey-os/desktop.ppm` so a person can look at what the test measured. The
kernel lingers fifty ticks after the desktop is up, because a kernel that exited first would leave
QEMU nothing to dump.

**Consequences.** `display` gains a quiet flag: printing after the desktop exists still goes to serial
and still goes into the console's grid - which is what `paint_panic` shows - but no longer paints the
panel. `Compositor::paint` repaints every window that intersects a damage rect rather than only the
visible parts of them, so a full-screen damage repaints every window; at three windows that is
cheaper than an occlusion pass, and the pass is the thing to add when it stops being.

**Open.** Nothing in a window can be typed into or clicked yet: the pointer is drawn, not wired, and
that is M8's. TrueType is still not here (ADR-027 put it in this milestone and it stays open: the
bitmap font is scaled, not replaced). No drag or resize, no scrollbars, no window borders that can be
grabbed, no per-window clipping beyond text, and no double buffering for the parts of a window that a
damage rect only partly covers.


## ADR-033: The 8042 carries both inputs, and it answers its own commands

**Status:** accepted, M6.

**Context.** The milestone is "input: keyboard and pointer", and the machine has one input
controller: the 8042, with a keyboard on its first port and a mouse on its second. Both report by
interrupt (IRQ1 and IRQ12, the latter through the slave's cascade). USB would be a different
milestone and a host-controller stack.

**Decision.** Both devices are driven through the 8042, and both decoders live in the library as
plain data so the host tests can drive them byte by byte. The keyboard's decoder is a table over
scancode set 1 - the set a controller with translation enabled hands over - with the modifier state
on the event rather than as events of its own, caps lock inverting shift for letters only, releases
swallowed, and the arrow keys resolved behind the `0xe0` prefix. The mouse's parser assembles three
bytes, uses bit 3 of the first as its only synchronisation, and treats overflow as a dropped packet
rather than half a movement.

Events go into a fixed 64-entry ring: one producer (the handler), one consumer (whatever reads a
key), no allocation and no lock. The consumer masks interrupts across the read, which is what makes
the borrow exclusive - `idt::enabled()` was added so it puts the flag back only if it was on. A full
ring drops the newest event and counts it: a keystroke that arrives while the reader is busy is
already late.

**What building it taught.** Three things, each of which the milestone's proof caught and no amount
of reading would have:

1. The controller answers its own commands - `read config`, the mouse's `ack` - in the *same* output
   buffer a keystroke arrives in. With IRQ1 live, the keyboard's handler read the mouse's
   configuration byte and fed `0x47` to the scancode decoder, so `mouse::init` saw silence and
   reported `Config`. The fix is to mask IRQ1 for the length of the mouse's handshake (`pic::mask` is
   new) and, in both handlers, to check the status byte's aux bit before reading: a byte that belongs
   to the other port is left for the other handler.
2. QMP has no `sendkey`; the command lives in the human monitor, so the harness relays it through
   `human-monitor-command`. `input-send-event` *is* a QMP command, and a relative movement reaches
   the PS/2 mouse through the same console the keystrokes use.
3. The acceptance loop was written as `while keys < 4 || packets == 0`, and printed the pointer line
   in its body. A packet that arrived during the last wait ended the loop before the body ran again,
   so the run said "4 keys and 1 pointer packets" with no pointer line to assert. The loop now checks
   after its body, not in its header.

**Why a machine drives it.** A kernel cannot press its own keyboard, and a host test of the decoder
proves only that the decoder works. So `GREY_OS_INPUT_TEST` compiles in a listening run: it prints
`input test: listening`, waits up to ten seconds for four keys and one pointer packet, and echoes
each one. `cargo xtask test` watches the serial output for the marker, then types through QEMU's
monitor - the same path a person's key takes - and asserts each character separately, because a
count alone would pass if one stuck scancode were decoded four times.

**Consequences.** `keyboard.rs` and `mouse.rs` each split at the same seam as `paging`: the
arithmetic and the table are host-tested (nine keyboard tests, seven mouse tests), and the port half
is `x86_64`-only and exercised by the boot. `pic::mask` and `idt::enabled` join the API. The input
phase costs a rebuild and about two seconds per `cargo xtask test`, which is the price of proving an
interrupt path with a machine rather than with a mock.

**Open.** No key repeat (the 8042 does not repeat; the kernel does not synthesise it), no keymap
beyond US set 1, no scroll wheel or the fourth and fifth buttons (IntelliMouse mode is not enabled),
no hot-plug after boot, and no USB. The mouse is clamped to the panel's pixels but nothing draws a
pointer yet: that is M7's.


## ADR-032: An ATA drive polled through its ports, and a filesystem the kernel formats

**Status:** accepted, M5.

**Context.** M5's theme is "storage: disk driver and filesystem", and the two halves constrain each
other. A filesystem needs a device that can fail a transfer and report why; a driver is only worth
having if something reads sectors it wrote. The machine has no PCI bus walk, no interrupts claimed
beyond the timer, and no allocator when the boot starts, so the driver has to be one that can run on
port I/O alone.

**Decision.** The driver is the parallel-ATA interface in programmed-I/O mode, on the primary channel,
polling its status register. It needs four registers to address a sector, one to hand the data over,
and no descriptor rings, no DMA programming, no PCI enumeration and no interrupt handler - which is
exactly the smallest thing that is still a disk. It claims the primary master and nothing else: the
slave and the secondary channel are the same code with different constants and no test image that
would exercise them, so probing them would add untested paths for the sake of a line in the log.

The placement in QEMU is part of the decision. The boot image used to be QEMU's first IDE drive; it
moves to the secondary channel's master (`if=ide,index=2`) so that the *data* disk can be the primary
master (`index=0`), which is the drive the driver claims. OVMF enumerates block devices and finds the
ESP wherever it is, so the boot does not care.

`greyfs` is the filesystem, and its layout is deliberately one sector longer than the smallest thing
that could work:

| sector | contents |
| ------ | -------- |
| 0 | superblock: magic, version, sector size, device size, `next_free`, mount generation, FNV-1a seal |
| 1-4 | the directory: 64 slots of 32 bytes, name (22), kind, start sector, length |
| 5.. | file data, one contiguous extent per file, allocated at `next_free` |

Three things about that are choices rather than accidents. The seal exists because a magic number
alone cannot tell a filesystem from a sector that happens to begin with it, and the seal is what makes
"blank disk" and "damaged filesystem" different answers - [`Filesystem::open`] formats only a sector
of zeros, and refuses everything else. The directory is positional: slot *n* on disk is slot *n* in
memory, so a removal is one zeroed slot instead of a rewrite, and a duplicate name is detectable at
mount rather than at lookup. And the allocator is a single bump cursor, which is why replacing a file
leaks its old extent and removing one leaks it entirely: without a free list, the honest statement
about those sectors is that nothing points at them any more.

A write is ordered data, then directory, then superblock. A crash in the middle leaves either a lost
write (the directory has not caught up) or a mount that refuses the disk (an extent the superblock
says is unallocated); the third order would leave a file pointing at sectors another file has since
claimed, which is the only one of the three that loses data silently.

**Where the format lives.** In the kernel, not in the harness. `xtask` creates the data image as a
file of zeros and the kernel formats it on first boot, which means one implementation of the layout
instead of two that have to be kept in step - the same argument that keeps the framebuffer's geometry
out of `xtask`. The cost is that the first boot of a fresh image is the only one that exercises
`format`, so `cargo xtask test` wipes the image before its clean boot and then boots it a second time
to assert the *other* path: a mount that finds the filesystem, names its generation, and reads back a
counter the first boot wrote. The counter (`/boot-count`) is the persistence proof, and the mount's
`generation` field is the same claim from the superblock's side.

**Consequences.** `disk.rs` holds the pure half - the drive/head byte, the capability word, the
28-bit size, the big-endian model string, the extent rounding and the transfer bounds - under eight
host tests, and the port half under `cfg(target_arch = "x86_64")` where it can only be exercised in
QEMU; `fs.rs` holds all of `greyfs` behind the `Block` trait, under eighteen host tests that mount an
array. `port.rs` is the kernel's first shared port-I/O module: `pic`, `serial` and `timer` keep their
own `outb` for now, because changing three working drivers is not what M5 is for, and folding them in
is a polish item. `main.rs` gains `storage()`, which probes, mounts or formats, writes the counter,
reads it back and compares - and returns `None` with a reported line when no drive answers, because a
machine without a disk still has to boot.

**Open.** No free list, so extents leak on replace and remove. No directories, no permissions, no
timestamps, no `fsck` beyond what a mount validates, and no write-cache flush: the driver waits for
the controller to drop busy after the last word, which is not the same as `FLUSH CACHE`. The slave
drive and the secondary channel are not probed. The directory is fixed at 64 slots.

