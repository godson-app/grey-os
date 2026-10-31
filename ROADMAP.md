# Roadmap

Grey OS is built one milestone at a time. A milestone is only marked done when its
acceptance criteria are demonstrated by output, not by code existing: the proof is
a `xtask test` run or a QEMU session whose serial log is pasted into the commit
message. No milestone is started before the previous one is accepted.

Scope for M0, M1, M2, M3, M4 and M5 is agreed. M6 onward is stated as themes in dependency order:
the authoritative wording comes from the engineering brief and is transcribed here milestone by
milestone, so that this file never claims more than was agreed.

## Status

| Milestone | Theme                                        | Status                                                     |
| --------- | -------------------------------------------- | ---------------------------------------------------------- |
| M0        | Boot, serial diagnostics, diagnosable panics | **Done** - criteria automated; BIOS image boots (ADR-009)  |
| M1        | Display primitives, text, on-screen boot log | **Done** - criteria met; TrueType moved to M7 (ADR-027)    |
| M2        | Interrupts, exceptions, timers               | **Done** - criteria met; PIC stays until an APIC (ADR-028) |
| M3        | Physical and virtual memory management       | **Done** - criteria met; guard page armed (ADR-029)        |
| M4        | Heap allocation and kernel data structures   | **Done** - criteria 1-5 met; stretch tree open (ADR-031)   |
| M5        | Storage: disk driver and filesystem          | **Done** - criteria 1-6 met; no free list yet (ADR-032)     |
| M6        | Input: keyboard and pointer                  | Not started                                                |
| M7        | Compositor, windows, theme                   | Not started                                                |
| M8        | Shell and process model                      | Not started                                                |
| M9        | Applications                                 | Not started                                                |
| M10       | SMP, ports, polish                           | Not started                                                |

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

- The framebuffer's physical span, so ADR-021's overlap guard stops being inert: M3 resolved it
  with a page-table walk, and the boot log now says the guard is armed (ADR-029).
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

**Carried forward.** M2 is accepted with the three items above open; none of them blocks M3,
which needs page tables rather than a PIC. Two notes on what landed rather than on what is
missing: the `#DF` stack is 16 KiB, enough to print a report and no more, and the PIC's mask is
fixed at `irq0`, so the rest of `0x20..=0x2f` is claimed but silent until a driver unmasks a
line.

## M3 - Physical and virtual memory management

**Goal.** Own the address space. Before M3 the kernel borrowed the bootloader's page tables, so it
could not map a page it wanted, unmap one it should not have had, or say where a buffer it was
handed physically lived - which is why ADR-021's heap guard was inert and why a `#PF` named an
address the kernel could do nothing about.

**Agreed order** (ADR-029): the page-table types and a walker, which can read the bootloader's
tables without allocating anything; then a frame allocator that reserves what the kernel is using,
the framebuffer included; then the kernel's own tables loaded into `CR3`; then the heap guard armed
from the framebuffer's span; then a guard page the `#PF` handler can be shown to catch. That is
also the order it landed in.

**Landed.**

- [x] `paging.rs`: PML4/PDPT/PD/PT entries with the flags this kernel uses (present, writable,
      no-execute), the index arithmetic, and a walker that translates a virtual address by reading
      the tables - the piece that turns the framebuffer's virtual pointer into a frame range, and
      the first thing M3 could prove, since it needs no allocator.
- [x] `frame.rs`: a 4 KiB frame allocator over `BootInfo::memory_regions`, reserving the kernel
      image, frame zero, the heap and, once the walk has resolved it, the framebuffer. The boot log
      reports total and free frames, and a probe allocates eight frames and gives them back.
- [x] The kernel's own tables, built through `physical_memory_offset` (ADR-022) and loaded with
      `mov cr3`, keeping the kernel mapping, the physical window and the framebuffer mapped. The
      log names the inherited root and the new one.
- [x] `main.rs`'s `framebuffer_span` comes from a table walk, so `pick_heap_region` enforces
      something and the `WARNING: framebuffer physical base unknown, heap guard unarmed` line is
      gone. The walk is cross-checked byte for byte through both mappings.
- [x] A guard page at the end of the heap, and `GREY_OS_EXCEPTION_TEST=guard` as a fifth `xtask`
      phase: write one byte past the guard and assert that the `#PF` report names a write to a
      not-present page.

**Open.**

- The heap stays a bump allocator: replacing it is M4's work, not this milestone (ADR-017 moved
  the heap itself into M1).
- Nothing frees memory yet, so the allocator's free path is exercised only by its own probe.
- The tables are a faithful copy of the bootloader's, including its 1 TiB 2 MiB-page window, which
  is about 1044 frames of tables for one boot. A tree that maps only what the kernel uses is the
  follow-up (ADR-029).
- Kernel text is still writable and its data still executable: `bootloader_api` 0.11 publishes the
  kernel image as one range rather than per-segment bounds, so W^X for the kernel's own pages waits
  for a bootloader that reports more. The heap and the framebuffer are already no-execute.
- User-mode mappings, copy-on-write, swapping and a page cache are out of scope.

**Acceptance criteria.**

1. The boot log reports the frame allocator's totals derived from the memory map, and a probe that
   allocated a run of frames and gave them back.
2. The kernel runs on tables it built itself: the log names the new `CR3` root, it differs from the
   inherited one, and the boot still reaches `grey-os: boot ok` with the console, the back buffer
   and the heap working.
3. The framebuffer's physical span is resolved by the walk, and the walk is checked rather than
   believed: the same byte read through the framebuffer's virtual pointer and through
   `physical_memory_offset` at the resolved frame agree, the span is printed, and the
   "unarmed" warning is gone from the log.
4. A write past the heap's guard page reaches the `#PF` handler, is reported as a write to a
   not-present page with a `cr2` inside the guard, and QEMU exits 35 - asserted by
   `cargo xtask test` next to the M2 phases.

**Status against them.** All four are met, in QEMU, on the boots `cargo xtask test` performs. The
clean boot now prints, after the heap lines:

```text
grey-os: frames: 525288 total, 20547 free, 64 KiB bitmap
grey-os: frame probe ok: 8 frames from 0x1000 written and read back
grey-os: framebuffer physical 0x0080000000-0x00803e8000 (4000 KiB, resolved by page walk)
grey-os: framebuffer byte 2048000 reads the same through both mappings
grey-os: page tables: inherited root 0x101000, own root 0x1000, 1044 frames
grey-os: heap guard armed at 0x2801bb6b000 (physical 0x1bb6b000), one page unmapped
```

Criterion 1 is the first two lines. Criterion 3 is the framebuffer pair: 0x8000_0000 is where the
bootloader said the panel is, and the byte at offset 2048000 read back through the window at
`0x28000000000 + 0x8000_0000 + 2048000` is the same byte, which is what ties the bootloader's
pointer to our arithmetic rather than leaving it a plausible number. Criterion 2 is the fifth line:
the copy costs 1044 frames because the bootloader maps a 1 TiB window with 2 MiB pages, and the
kernel refuses to continue if the new root came back equal to the inherited one. Criterion 4 is the
fifth fault phase `xtask` boots:

```text
grey-os: GREY_OS_EXCEPTION_TEST=guard, writing one byte past the heap at 0x2801bb6b000
[exception] #PF page fault: error 0x2 on a write to a not-present page, rip 0x100000110d6, cr2 0x2801bb6b000
```

Error 0x2 is a write to a page with no mapping, and `cr2` is the first byte past the heap: the guard
page is the last whole page of the heap's region, the tables leave it absent, and the write is the
allocator's own size arithmetic being checked by hardware instead of by itself. Making that page
absent needed one thing the copied tables could not express: a single 4 KiB hole inside a 2 MiB page
is a split, so `unmap_4k` allocates one frame, writes a table of 4 KiB entries covering the same
2 MiB, and repoints the parent entry. The pages on either side of the guard kept their frames and
their flags, which the console and the heap still working are the evidence for.

**Carried forward.** M3 is accepted with the five items above open. Two of them shape M4: the bump
heap cannot give a frame back to anything, and the free path in `frame.rs` has only ever been run by
its own probe. The table copy is where the milestone's cost is visible - 4 MiB of frames to own an
address space whose interesting part is a few megabytes - and the fix is a tree built from scratch,
not a smaller bitmap. Tests: 119 on the host, covering the entry layout, the index arithmetic, the
walk (2 MiB and 1 GiB leaves included), the bitmap, and the heap/guard split; everything that
executes an instruction is left to the QEMU boots.

## M4 - Heap allocation and kernel data structures

**Goal.** Make memory reusable. M1's heap is a bump allocator: it never gives a byte back, so
everything the kernel has ever asked for stays asked for, and M3's frame free path has only ever
run inside its own probe. M4 replaces the allocator, grows the heap from frames instead of
guessing one region at boot, and puts the containers the next milestones need - `Vec`, `String`,
an intrusive list - on top of it.

**Agreed order** (ADR-030): `paging::map_4k`, because growth means giving a frame an address and M3
could only take a mapping away; then a heap that grows in pages, with the guard page moving above
the grown heap; then a free-list allocator per size class, which is the first real caller of
`frame::Allocator::free`; then `Vec`, `String` and a list on top of `GlobalAlloc`; then, if it still
matters by then, the tree built from scratch that ADR-029 leaves open.

**Landed.**

- [x] `paging::map_4k`: 4 KiB mappings built level by level, with the arithmetic in `paging::map`
      over a `Tables` trait so the host tests drive it with tables in an array. A fresh tree costs
      three tables, a neighbour in the same 2 MiB page costs none, and a duplicate address, a
      misaligned address or frame, a huge page and a dry allocator are each refused with the reason
      named; the levels it creates keep their flags out of the leaf's way, so a leaf decides its own
      writability and executability. Proved at boot: one frame mapped at `0x6000_0000_0000` - an
      address the walk says nothing maps - a byte written through the mapping, read back through the
      window at the frame, the mapping taken away and the frame returned to the bitmap.

- [x] A heap in an arena of its own (ADR-031): 64 MiB of reserved virtual span at
      `0x5000_0000_0000`, pages taken from the frame allocator as they are needed and mapped
      `WRITABLE | NO_EXECUTE` through the kernel's own tables, and pages given back when a free leaves
      whole ones free at the top. The region `pick_heap_region` picks is now only the bootstrap - the
      frame allocator's bitmap and the console, which exist before a frame can be mapped - and the
      4000 KiB it used is reserved instead of the 420000 KiB region it was handed.
- [x] A free-list allocator per size class: 16-byte boundary-tagged headers (size, previous size,
      magic and free bit, requested size), classes for 64 bytes and below, one per power of two up to
      4096, then larger; first fit, split on allocation, merged with both neighbours on free, and
      trimmed by whole pages when the block at the top of the arena comes back. `realloc` keeps the
      bytes that still fit, shrinks in place, and moves when it must.
- [x] Checked frees (ADR-030's fourth choice): a double free, a pointer from outside the arena and a
      size the block was not allocated with are each refused and counted, and the boot log reports
      that all three were. Nothing is followed: a header whose magic is gone answers "not a block".
- [x] `frame::Allocator::free` has callers that mean it: every page the arena grows costs a frame,
      every page it trims gives one back, and each unmap reads the frame out of the tables before
      taking the entry away, because `unmap_4k` still does not return it.
- [x] The guard page, without arming it: the first page past the arena's growth frontier was never
      mapped, so a write to it faults with `cr2` at the heap's end. `GREY_OS_EXCEPTION_TEST=guard`
      writes at `0x500000004000` and the handler reports `cr2 0x500000004000`.
- [x] `Vec` and `String` on the allocator: a 512-element `Vec<u64>` grown through several
      reallocations and a `String` built from parts, both read back and reported.

**Open.** Nothing that gates the milestone; what is left is either stretch or M5's:

- The intrusive list is not built. `Vec` and `String` are enough for the boot log and the console,
  and a list needs a consumer to be worth writing; the first milestone with something to thread
  (M6's input queue, M7's display list) gets it.
- The tables a growth creates are never freed, and `unmap_4k` still hands back no frame: the arena's
  page source walks the address first to learn the frame, which is a workaround at the caller, not a
  fix in the mapper.
- A free only trims when the block is page-aligned at the top, so a fragmented arena can hold pages
  it no longer needs. Bounded by one page per block below a free tail, not a leak that grows.
- The arena's span is a constant. A user-space layout is where that becomes a decision rather than a
  placeholder.
- The copy of the bootloader's tree still costs 1044 frames (M3's debt 3). A tree built from scratch
  is criterion 6, the stretch item, and it is not attempted: it needs a table free path of its own -
  walking up from a leaf to release the tables a removal made empty.

**Acceptance criteria.**

1. The boot log reports the heap's growth in pages and the frames it took to grow, and those
   frames are the ones `frame::FRAMES.stats()` says are no longer free.
2. A churn probe allocates and frees a mixed run of sizes, returns `bytes_in_use` to the value it
   started at, and prints how many allocations, frees and frames it took to do it.
3. A double free and a free of an address that was never allocated are refused and counted, and the
   log reports the count: the free list stays intact rather than being quietly corrupted.
4. The guard page still catches a write past the heap after the heap has grown:
   `GREY_OS_EXCEPTION_TEST=guard` stays a phase `cargo xtask test` asserts, and its `cr2` is the
   grown heap's end rather than the end of the region the boot picked.
5. Host tests cover the class bookkeeping, block splitting and coalescing, alignment for every
   class, a `realloc` that keeps the bytes, and the refusal path for a size that does not match its
   block.
6. Stretch, not gating: the page tables the boot owns fall from 1044 frames to double digits.

**Status against them.** 1-5 met, 6 not attempted. What the clean boot prints, after M3's lines:

```text
grey-os: heap bootstrap 0x0001780000-0x001bb6c000 (430000 KiB region, 4007 KiB used)
grey-os: heap probe ok: 4 KiB read back, 1 allocations, 4096 bytes in use
grey-os: frames: 525288 total, 126988 free, 64 KiB bitmap
grey-os: heap churn ok: 8 allocations and 8 frees over 0 bytes, in use back to 0, 6 pages grown and 6 returned, 0 frames kept plus 3 for the span's page tables
grey-os: heap frees: a double free, a pointer from outside and a wrong size all refused and counted
grey-os: heap trim ok: 4 pages went back to the frame allocator
grey-os: containers ok: a 512-element Vec and a 7 byte String, 4104 bytes in use on the arena
grey-os: heap guard armed at 0x500000004000: the first page above the heap's 4 mapped pages, nothing mapped there
grey-os: mapping probe ok: frame mapped at 0x600000000000, written through the mapping and read back by frame
grey-os: boot ok
```

and the guard phase of `cargo xtask test`:

```text
grey-os: GREY_OS_EXCEPTION_TEST=guard, writing one byte past the heap at 0x500000004000
[exception] #PF page fault: error 0x2 on a write to a not-present page, rip 0x100000150f5, cr2 0x500000004000
```

Criterion 1 is the numbers in the churn line: six pages grown cost six frames plus the three the
span's tables took, and the frames the frame allocator reports are exactly those nine. Criterion 2 is
that line's `in use back to 0`. Criterion 4 is the `cr2` above, which is the address the guard line
named. Criterion 5 is 15 host tests in `arena`: class bookkeeping through the free lists, splitting,
coalescing with the block before, alignment for every class from 16 to 4096, a trim that gives pages
back, the three refusals, `realloc` growing and shrinking, and the churn probe itself. Tests: 136 on
the host. The write through a real arena page and the `invlpg` after it are left to the QEMU boot, as
`mov cr3` was in M3.

## M5 - Storage: disk driver and filesystem

**Goal.** Make something outlive the boot. Everything the kernel has written so far - the console's
grid, the arena's pages - is gone when the machine stops, and there is no way to hand a later
milestone anything bigger than what fits in the image. M5 adds the first block device and the first
filesystem: a driver that can fail a transfer and say why, and a format whose writes survive a power
cut badly rather than silently.

**Agreed shape** (ADR-032): parallel ATA in programmed-I/O mode on the primary channel, polled, with
no interrupt and no DMA; `greyfs` over it, a flat format with a sealed superblock, a positional
directory of 64 slots and one contiguous extent per file. The kernel formats a blank disk itself; the
harness only creates the file.

**Landed.**

- [x] `disk.rs`: the ATA driver. Eight host tests for the parts that are arithmetic - the drive/head
      byte that carries bits 24-27 of the block address, the capability word, the size in words 60
      and 61, the byte-swapped model string, the 256-encoded-as-zero count register, extent rounding,
      and a transfer-bounds check that agrees with the host tests. The port half is `x86_64`-only and
      bounds every wait: 2,000,000 polls then `Timeout`, and `ERR`/`DF` become `DeviceFault` instead
      of a wait for data that is not coming.
- [x] `fs.rs`: `greyfs`, all of it host-testable behind the `Block` trait. Superblock with an FNV-1a
      seal over its own fields; a mount that refuses a bad magic, a version or sector size it does
      not read, a broken seal, a `next_free` outside the device, an extent inside the metadata, an
      extent past `next_free`, a length the format cannot represent, or two entries with one name;
      and `open`, which formats *only* a sector of zeros.
- [x] Writes ordered data, then directory, then superblock, so the two ways a crash can leave the
      disk are a lost write and a mount that refuses it - never a file pointing at live sectors.
- [x] `port.rs`: `inb`, `outb`, `inw`, `outw`, `insw`, `outsw` and `io_wait`, each one `unsafe`
      block with its promise written down, so the drivers after this one do not grow their own.
- [x] `xtask` creates the data image under `target/` (4 MiB of zeros) and attaches it as the primary
      master; the boot image moves to the secondary channel's master so the two do not collide.
- [x] Boot proof, in this order: the drive identified by model and size, the format or the mount with
      its generation, the counter written and read back and compared, and the count itself.
- [x] `cargo xtask test` wipes the disk, asserts the clean boot's format, then boots the same disk a
      second time and asserts a mount of generation 1, a counter that reads `boot 2`, and the
      *absence* of the format line - a kernel that re-formatted would pass every other assertion.

**Open.** No free list, so replacing a file leaks its extent and removing one leaks it entirely. No
directories, permissions, timestamps or file growth in place. No write-cache flush: the driver waits
for busy to drop, which is not `FLUSH CACHE`. The ATA slave and the secondary channel are not probed.
The directory is fixed at 64 slots. `pic`, `serial` and `timer` still spell their own `outb`.

**Acceptance criteria.**

1. The boot log names the drive the driver found, with the model string and the sector count the
   identify command reported.
2. A blank disk is formatted and the log says so, including the number of directory slots.
3. A file written through the filesystem reads back byte for byte, through the disk, and the log
   reports the extent it landed in.
4. A second boot of the same disk mounts the filesystem instead of formatting it, reports the
   generation the first boot left, and reads back a counter the first boot wrote.
5. Host tests cover the format's round trip, every refusal a mount can make, the name alphabet, a
   full disk, a full directory, removal, replacement, and the seal's coverage.
6. `cargo xtask test` asserts 1-4 on serial and stays green through the panic and fault phases.

**Status against them.** All six met. What the clean boot prints, after M4's lines:

```text
grey-os: ata: primary master "QEMU HARDDISK", 8192 sectors of 512 bytes
grey-os: greyfs: no filesystem, formatted 8192 sectors with 64 directory slots
grey-os: greyfs: wrote /boot-count (15 bytes at sector 5), read back and compared
grey-os: storage ok: boot 1 of this image, 1 file(s), 6 of 8192 sectors in use
```

and the second boot, which is the persistence phase of `cargo xtask test`:

```text
grey-os: ata: primary master "QEMU HARDDISK", 8192 sectors of 512 bytes
grey-os: greyfs: mounted generation 1, 1 file(s), 6 of 8192 sectors in use
grey-os: greyfs: wrote /boot-count (15 bytes at sector 6), read back and compared
grey-os: storage ok: boot 2 of this image, 1 file(s), 7 of 8192 sectors in use
```

Criterion 2 is the second line: 64 slots is `fs::MAX_ENTRIES`, and the format wrote all four
directory sectors. Criterion 3 is the third line's `read back and compared`, which is a `memcmp` of
what `Filesystem::read` returned against the bytes that went in. Criterion 4 is the second block: the
mount's generation is one because the first boot wrote generation zero, the counter read `boot 1` and
is now `boot 2`, and the format line is absent - asserted, not merely not looked for. Criterion 5 is
26 host tests, 8 in `disk` and 18 in `fs`. Tests: 162 on the host.

## M6 onward - provisional themes

The ordering above is dependency-driven and each theme is a milestone of its own.
Detailed scope, acceptance criteria and verification are added when the milestone
is approved, per the brief's one-at-a-time rule.

Two items already have a home when their milestone opens: TrueType rasterisation, with the face
question it drags along, belongs to M7 (ADR-027); and the framebuffer's physical span, which would
make ADR-021's heap guard bite, is now part of M3 above.

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

`cargo xtask test` builds once per failure path on purpose: the clean boot, then one image each
for `GREY_OS_PANIC_TEST` and for `GREY_OS_EXCEPTION_TEST=gp|pf|ud|df`, and finally one more with
neither variable set, so `target/` is left holding the image every other command boots.
