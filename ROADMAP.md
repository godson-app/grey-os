# Roadmap

Grey OS is built one milestone at a time. A milestone is only marked done when its
acceptance criteria are demonstrated by output, not by code existing: the proof is
a `xtask test` run or a QEMU session whose serial log is pasted into the commit
message. No milestone is started before the previous one is accepted.

Scope for M0 to M10 is agreed, and all ten themes have landed; what remains is the `Open` list
each milestone's ADR records. What comes after M10 is stated below as themes in dependency order:
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
| M6        | Input: keyboard and pointer                  | **Done** - criteria 1-5 met; no repeat or keymaps (ADR-033) |
| M7        | Compositor, windows, theme                   | **Done** - criteria 1-5 met; TrueType still open (ADR-034) |
| M8        | Shell and process model                      | **Done** - criteria 1-5 met; cooperative (ADR-035)          |
| M9        | Applications                                 | **Done** - criteria 1-5 met; bytecode (ADR-036)            |
| M10       | SMP, ports, polish                           | **Done** - criteria 1-5 met; APs not started (ADR-037)     |
| M11       | Hardening before the next primitive          | **Done** - one lock type, one CI command (ADR-038)         |
| M12       | User mode: ring 3, a syscall gate            | **Done** - 3 programs run, 1 killed by the CPU (ADR-040)   |

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
`idt loaded: 39 gates of 256 (22 exceptions, 16 irqs, 1 syscall)`, `pic remapped to 0x20..=0x2f, only irq0
unmasked` and `timer 100 Hz, 13 ticks observed, interrupts enabled, 0 unexpected irqs`. The tick
count is read only after the boot path has waited for ten of them, so a counter that did not move
would fail at the wait rather than print a number that looked like progress.

The fault phases are four further images, each built with `GREY_OS_EXCEPTION_TEST` set to one of
`gp`, `pf`, `ud` or `df`, each booted and each asserted on its report and on QEMU's exit 35:

```text
[exception] #GP general protection fault: error 0xfff8, rip 0x1000000aad8
[exception] #PF page fault: error 0x0 on a read to a not-present page, rip 0x1000000ac51, cr2 0x700000000000
[exception] #UD invalid opcode: no error code, rip 0x1000000aac4
[exception] #DF double fault: error 0x0, rip 0x1000000845e
```

Each phase faults with a real instruction the CPU refuses - a segment load of a selector past the
end of the image's own GDT (index 8191, `0xfff8`, because an index just past the table stops being
past it the moment the table grows: M12's user descriptors took it from five entries to seven, and
the selector M2 used, `0x28`, became the user code descriptor - a readable code segment the CPU
happily loads into `ds` from ring 0, so the phase quietly stopped faulting), a read of an unmapped
address, `ud2`, and a push onto an unmapped stack - rather than by calling a handler from Rust, which
would prove only that a function prints.
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

## M6 - Input: keyboard and pointer

**Goal.** Let the machine be told something. Every milestone so far has run a boot sequence chosen
at compile time: the log is written, not driven. M6 adds the two devices a person actually uses - a
keyboard and a pointer - as interrupt-driven inputs whose events outlive the handler that produced
them, and proves them with keystrokes that arrive through the machine's own controller rather than
through a function call.

**Agreed shape** (ADR-033): the 8042 carries both ports; a table-driven scancode set 1 decoder and a
three-byte mouse packet parser live in the library where the host tests can drive them; events go
into a fixed 64-entry ring read under a mask; the acceptance run is a cfg-gated kernel that listens
for four keys and one packet and is typed into through QEMU's monitor.

**Landed.**

- [x] `keyboard.rs`: the decoder as a value - translation-on set 1, both shifts, control, alt, caps
      lock that latches on the press and does not unlatch on the release, caps inverting shift for
      letters, the number row from the table rather than from arithmetic, the arrow keys behind
      `0xe0`, and unknown codes dropped rather than invented. Nine host tests, including one that
      walks every code in the table and checks it against itself.
- [x] The 8042 setup: both ports disabled while the configuration byte is rewritten, translation
      forced on, IRQ1 enabled, the keyboard's clock on, the handler installed before the line is
      unmasked.
- [x] `mouse.rs`: the three-byte parser (bit 3 as the only synchronisation, sign extension as a
      nine-bit two's complement, overflow dropping the whole packet) and the position (y inverted
      once, clamped to the panel rather than wrapped). Seven host tests.
- [x] The second port: enabled, its clock and IRQ12 turned on in the same configuration byte, the
      mouse told to use defaults and start reporting with its `0xfa` acknowledgements checked, and
      the cascade line unmasked along with IRQ12 - a driver that forgot the cascade would configure a
      mouse that never moves and every register would look right.
- [x] The queue: 64 events, one producer in the IRQ, one consumer that masks interrupts across its
      read, and a count of what a full ring dropped.
- [x] The input phase of `cargo xtask test`: a kernel built with `GREY_OS_INPUT_TEST=1` says when it
      is listening, the harness types `grey` and moves the pointer through QMP, and the phase
      asserts each key separately plus the pointer packet. QMP has no `sendkey`, so the keystroke is
      relayed through the human monitor; the movement is a real QMP `input-send-event`.

**Open.** No key repeat (the controller does not repeat and the kernel does not synthesise it), no
keymap beyond US set 1, no scroll wheel or the extra buttons, no hot-plug, no USB. Nothing draws the
pointer yet: that arrives with M7's windows.

**Acceptance criteria.**

1. A normal boot reports the keyboard configured on IRQ1, the mouse configured on IRQ12 through the
   cascade, and how many events each has produced.
2. A byte the mouse's port produced is never decoded as a keystroke, and a command answer from the
   controller is never decoded as one either.
3. Host tests cover the decoder for every code in the table, the modifier states, caps lock, the
   extended prefix, the packet parser's resynchronisation and sign handling, and the pointer's
   clamping.
4. Four keystrokes typed into the machine's own keyboard controller are decoded and reported, each
   one named - not merely counted.
5. A pointer movement injected through the same console reaches the PS/2 port, is parsed, and moves
   the pointer by the amounts sent.

**Status against them.** All five met. A normal boot prints, after the timer line:

```text
grey-os: input: keyboard on irq1, 8042 config 0x67, translation on so set 1 arrives
grey-os: input: mouse on irq12 through the cascade, 3-byte packets, pointer at 640,400
grey-os: input ready: 0 keys decoded, 0 dropped, 0 pointer packets
```

and the input phase of `cargo xtask test`:

```text
grey-os: input test: listening for keys and pointer
grey-os: key: 'g' shift=false ctrl=false alt=false caps=false
grey-os: key: 'r' shift=false ctrl=false alt=false caps=false
grey-os: key: 'e' shift=false ctrl=false alt=false caps=false
grey-os: key: 'y' shift=false ctrl=false alt=false caps=false
grey-os: mouse: dx=+25 dy=+10 buttons=0x00 at 665,390
grey-os: input ok: 4 keys and 1 pointer packets in 423 ticks, 0 dropped
```

Criterion 1 is the first three lines, including the configuration byte the controller accepted.
Criterion 2 is the `0x67` in that line being readable at all: the fix for the mouse's `Config`
failure was masking IRQ1 while the controller is talked to, and the status byte's aux bit is checked
before either handler reads a data byte. Criterion 3 is 16 host tests, nine in `keyboard` and seven
in `mouse`; 178 on the host in total. Criterion 4 is the four `key:` lines: each character is
asserted by name, so a stuck scancode cannot pass as four presses. Criterion 5 is the `mouse:` line's
`dx=+25 dy=+10`, which is the movement sent - `y` positive in the log because QMP's `-10` is up and
the packet's `y` grows upwards while the screen's grows down.

## M7 - Compositor, windows, theme

**Goal.** Give the panel something that is not a log. Everything so far has been one console
painting its own grid; M7 turns the screen into a desktop of windows, with a look that is a value
rather than a set of constants inside a drawing function, and repaints that are proportional to what
changed rather than to the size of the screen.

**Agreed shape** (ADR-034): a `Theme` of colours and measurements, a `Windows` list whose order *is*
the stacking order, and damage as a list of rects that are merged before they are painted.

**Landed.**

- [x] `theme.rs`: eleven colours and six measurements, with tests that the text is readable on every
      background it is drawn on, that focus is two cues rather than one, that the chrome leaves room
      for the text it exists for, and that a window smaller than its chrome reports no cells instead
      of underflowing.
- [x] `window.rs`: create (cascaded and clamped to the panel), raise, click, hit test from the front,
      move with clamping, close with focus falling back, focus, push-line, and the damage list.
- [x] `merge`: unions overlapping rects, drops ones another covers, clips to the panel and drops what
      is not visible; tested for overlap, edge-touching, containment, partly-off-screen, empty and a
      chain of three.
- [x] `compositor.rs`: desktop, windows back to front (edge, title bar, title, body, body text) and
      the pointer, all clipped to a damage rect when there is one, with text truncated at the clip
      and at the window's own edge. Eight host tests read pixels back from a surface in memory.
- [x] The panel handover: `display::set_quiet` stops the console painting after the desktop exists,
      while the log keeps going to serial and into the console's grid, which is what the panic path
      still shows.
- [x] A screenshot proof: the input phase asks QEMU for a `screendump` the moment the compositor says
      it is finished, and checks the PPM - desktop colour in the corners, at least four distinct
      colours, the pointer's fill present - keeping the file at `target/grey-os/desktop.ppm`.

**Open.** Nothing in a window can be typed into or clicked: the pointer is drawn, not wired (M8).
TrueType is still not here, so ADR-027's face question stays open with the bitmap font scaled rather
than replaced. No drag, resize, scrollbars or per-window clipping beyond text. Every window that
intersects a damage rect is repainted in full rather than only its visible part.

**Acceptance criteria.**

1. A normal boot reports the theme's measurements, three windows with a panel size, and how many of
   them overlap.
2. The compositor's first frame paints every window, and a frame after a raise and a move paints
   only the merged damage - with both numbers on the line.
3. Host tests cover the theme's readability and measurements, the window list's ordering, hit
   testing, focus, clamping, closing, the damage merge, and pixels: title bars, bodies, overlap,
   damage that leaves another window alone, and the pointer.
4. The panel, once the desktop exists, is a desktop: checked from outside the guest, from a
   screenshot, not from what the kernel says about itself.
5. The boot log keeps working after the handover, and the panic path still paints its own screen.

**Status against them.** All five met. The clean boot prints, after the input lines:

```text
grey-os: theme "grey": title 30px, border 2px, padding 8px, text scale 2
grey-os: desktop: 3 windows on 1280x800, 2 overlapping pair(s), focused #1, 3 drawn in the first frame
grey-os: compositor ok: 4 damage rects merged into 2, pointer drawn at 640,400, 2 frame(s)
```

Criterion 2 is `4 damage rects merged into 2`: the raise damaged the window it raised and the one
that lost focus, the move damaged the old and the new rect, and the merge left two rectangles because
those two groups do not overlap. Criterion 4 is the harness's line, which is the screenshot talking:

```text
pass: 1280x800 screenshot, 10 distinct colours, 77 pointer pixels, saved to target/grey-os/desktop.ppm
```

Criterion 3 is 23 host tests, eight in `compositor`, nine in `window` and six in `theme`; 201 on the
host in total. Criterion 5 is every later line of the boot log still arriving on serial with the
desktop on the panel, and the panic phase still painting its red screen.

## M8 - Shell and process model

**Goal.** More than one thing at a time, and a way to ask the machine to do something. Everything so
far has been one boot sequence on one stack; M8 adds tasks with their own stacks and a switch between
them, and a shell whose lines are typed into a window and whose commands reach the filesystem, the
allocator and the window list.

**Agreed shape** (ADR-035): cooperative round robin over eight fixed slots, each task with a 32 KiB
stack from the arena and a saved context of the callee-saved registers; a parser and a line editor
that live in the library and are host-tested; the executor in `main.rs`, which is the only part that
needs a machine.

**Landed.**

- [x] `scheduler.rs`: `next_ready` as a pure function over a slice of states (round robin from the
      slot after the current one, sleeping tasks passed over until their tick, the running task kept
      as the fallback so a single runnable task does not thrash), `after_yield` as the state
      transition, and five host tests that walk the awkward cases.
- [x] The switch: `grey_os_switch`, 22 instructions that save and restore `rsp`, `rbx`, `rbp` and
      `r12`-`r15`, and return into whatever the new stack's return address says. A new task's stack
      is prepared with `task_entry` as that address and its own entry point in `r12`.
- [x] `spawn`, `yield_now`, `sleep_ticks`, `finish`, `tick` and the report (`stats`, `task_name`,
      `runs`, `stack_bytes`), with interrupts masked across every table access and the timer as the
      only writer that runs unmasked.
- [x] Three demonstration tasks - a counter that yields five times, a sleeper woken by the timer, and
      a worker that yields three times - each writing a line into the tasks window and to serial
      while the boot task is the one that redraws.
- [x] `shell.rs`: a parser for fourteen commands with aliases and arguments, and an editor with a
      cursor, a backspace that respects character boundaries, and history; 19 host tests.
- [x] The shell window: a prompt line that is always last, output pushed above it, `clear`, `raise`,
      `close`, `windows`, `tasks`, `mem`, `ls`, `cat`, `write`, `rm`, `echo`, `help` and `exit`, all
      against the real filesystem, allocator and window list.
- [x] Clicks: the pointer's first job, raising whatever is under it and reporting the id on serial.

**Open.** Cooperative only - a task that never yields never gives up the CPU - with no priorities, no
per-task address spaces, no user mode and no way to kill a task. A finished task's stack is never
freed. The shell has no quoting, redirection, pipes or completion, and its history is in memory. The
shell only exists in the acceptance build: a normal boot runs the tasks and exits.

**Acceptance criteria.**

1. The boot log reports how many tasks exist, how many finished, how many switches happened and how
   many lines the tasks wrote - and every task's line appears, in an order only a scheduler could
   produce.
2. A task that sleeps is woken by the timer rather than by another task's yield, and the log shows
   the tick it woke on.
3. Commands typed into the machine reach the shell: `ls` lists the boot counter, `mem` reports the
   heap and the frame allocator, and `exit` ends the session, each with its answer on serial.
4. The document a command reads is the one the boot wrote: `ls` names `/boot-count`, the file the
   storage bring-up wrote before the desktop existed.
5. Host tests cover the scheduling policy, the parser and the editor, including the state transition
   that lets a finished task come back.

**Status against them.** All five met. The clean boot prints, after the theme's line:

```text
grey-os: task: counter: step 1
grey-os: task: sleeper: asleep for 40 ticks
grey-os: task: worker: pass 1
grey-os: task: counter: step 2
grey-os: task: worker: pass 2
grey-os: task: counter: step 3
grey-os: task: worker: pass 3
grey-os: task: counter: step 4
grey-os: task: counter: step 5
grey-os: task: sleeper: woke at tick 1075
grey-os: sched ok: 4 tasks (3 finished), 19 switches, 10 lines written by tasks, 32 KiB per stack
```

and the shell phase of `cargo xtask test`, which types into it:

```text
grey-os: shell: > ls
grey-os: shell: boot-count 15 bytes at sector 10
grey-os: shell: 1 file(s), 11 of 8192 sectors in use
grey-os: shell: > mem
grey-os: shell: heap: 108282 bytes in use, 31 pages mapped
grey-os: shell: frames: 525288 total, 125696 free, 64 KiB bitmap
grey-os: shell: > exit
grey-os: shell: exit
```

Criterion 1 is the interleaving: the counter's steps are separated by the worker's and the sleeper's
lines, which is what a switch looks like from outside. Criterion 2 is `woke at tick 1075` - the
sleeper asked for forty ticks and got them, and only the timer can have woken it. Criterion 4 is
`boot-count 15 bytes at sector 10`: that file was written by the storage bring-up long before the
shell existed, and the shell read it through the filesystem rather than from a copy. Criterion 5 is 24
host tests, five in `scheduler` and nineteen in `shell`; 223 on the host in total.

## M9 - Applications

**Goal.** A program that is a file rather than a function. Everything so far has been linked into one
image; M9 adds a program format, a machine that runs it, a way to get one onto the disk, and a task
that loads one from the filesystem and puts its output in a window.

**Agreed shape** (ADR-036): bytecode, not native code. A `GVM1` file is a header, code, and a pool of
NUL-terminated strings, for a stack machine with 32 cells, 16 locals and 22 opcodes; the parser
validates every jump, pool offset and local index before the first instruction runs, and the
interpreter's only failures are underflow, overflow, division by zero and a step budget. Output and
sleeping go through a `Host` trait, so a host test can record what a program printed and the kernel
can put it in a window.

**Landed.**

- [x] `vm.rs`: the format, `Program::parse` with a refusal for every way a file can be wrong
      (`BadMagic`, `BadVersion`, `Truncated`, `TooLarge`, `BadOpcode`, `BadJump`, `BadPool`,
      `BadLocal`), and the `Machine` with `run(budget) -> Outcome::Halted { steps }`, whose only
      failures are `StackUnderflow`, `StackOverflow`, `DivideByZero` and `StepLimit`.
- [x] The 22 opcodes: arithmetic and comparison, `dup`/`drop`/`swap`, absolute jumps, `load`/`store`,
      `puts`/`print`/`print_char`, `sleep` and `halt`.
- [x] A `Builder` assembler, so the kernel's applications are readable code rather than hex:
      `jump_ahead`/`patch` for the forward jump an assembler cannot resolve in one pass.
- [x] `apps.rs`: three programs - `apps/hello.gvm`, `apps/count.gvm`, `apps/fib.gvm` - written to
      `greyfs` as `Kind::Program` files on the boot that formats the disk, and left alone when the
      bytes already match.
- [x] One task per application: each reads its file, parses it, interprets it with a 100 000 step
      budget, and pushes its output into a new `apps` window and to serial.
- [x] `STORAGE`, the filesystem published through an `AtomicPtr` the way the desktop is, because a
      task has its own stack; the boot now waits for six tasks, not three.
- [x] 18 host tests: the parser's refusals, arithmetic and comparison, a loop over a local, the stack
      limits, the budget, the assembler round trip, and the three programs' exact output.
- [x] `cargo xtask test` asserts the install line, the three programs' output lines, the halt lines and
      the summary on the clean boot.

**Open.** No ELF, no user mode and no per-program address space: isolation is "cannot reach anything
but its own stack and locals", not "cannot reach the kernel". No function calls, arrays, heap strings,
arguments, file access or drawing. Programs cannot start other programs, and the shell has no command
to run one - the boot starts them. The step budget is a constant rather than a time slice.

**Acceptance criteria.**

1. Programs are files: the kernel writes them to the filesystem, the boot log reports how many bytes
   and how many slots, and a later boot finds them already there rather than rewriting them.
2. A program is loaded from the disk and run by a task: the log shows the load, the parse and the halt
   step count for each, and a task that is still running interleaves with it.
3. Output is the program's, not the kernel's: `fib` prints `0 1 1 2 3 5 8 13 21 34` and `count` prints
   `1 2 3 4 5`, both produced by instructions the interpreter executed.
4. A malformed file cannot fault the machine: each refusal has a host test, and the interpreter is only
   reachable with a validated program.
5. A program that misbehaves ends, and the window says why: division by zero is an error and an
   infinite loop is `StepLimit`, neither of which hangs the boot.

**Status against them.** All five met. The clean boot prints, after the task lines:

```text
grey-os: apps: 3 program(s) written this boot, 205 bytes of bytecode on disk, 4 slot(s) used
...
grey-os: app: hello from apps/hello.gvm
grey-os: app: 1 2 3 4 5
grey-os: app: apps/count.gvm: halted after 67 steps, 38 bytes of code, 2 of strings
grey-os: app: 0 1 1 2 3 5 8 13 21 34
grey-os: app: apps/fib.gvm: halted after 193 steps, 70 bytes of code, 2 of strings
grey-os: app: goodbye from 20 ticks later
grey-os: app: apps/hello.gvm: halted after 4 steps, 10 bytes of code, 56 of strings
grey-os: apps ok: 3 of 3 programs ran from /apps, 7 lines of output, 264 instructions
```

Criterion 1 is `205 bytes ... 4 slot(s) used` on the formatting boot and `0 program(s) written` on
every boot after it. Criterion 2 is the halt lines: each names the file, the program's byte count and
the instructions it took, and each was read back through `greyfs` rather than from a copy in the
image. Criterion 3 is the two series, which only the interpreter can produce - the numbers are
computed by `Add` and `Mod` on a stack, not stored. Criterion 4 is the eight parser refusals, a test
each, and an interpreter that cannot be reached without one of them having passed. Criterion 5 is the
budget in `run` and the divide-by-zero check, both host-tested.

The interleaving is worth reading in full, because it is the milestone: `hello` prints, sleeps twenty
ticks, and its `goodbye` arrives after the counter's five steps and the worker's three passes, while
`count` and `fib` finish in between.

## M10 - SMP, ports, polish

**Goal.** The last theme in the brief's list, and the one that needs the most honesty: "SMP" on this
kernel is a set of subsystems that do not exist, and the useful thing a single-CPU cooperative
kernel can do about it is find out what the machine actually is and say so, instead of assuming one
CPU and a pair of 8259s.

**Agreed shape** (ADR-037): read the firmware's description of the machine - the RSDP, the root
table and the MADT - and report the processors, the IOAPIC and the interrupt source overrides; start
no application processor and keep the PIC and the PIT; add the two shell commands the report needs
(`cpus`, and `run` for the programs M9 put on the disk); and bring the README and the harness up to
what the kernel has been since M8.

**Landed.**

- [x] `acpi.rs`: `Phys` as the one read path, the RSDP from the bootloader's UEFI hint or the ACPI
      1.0 search (extended BIOS data area, then the BIOS area), both root table formats, and the
      MADT's processor, x2APIC, IOAPIC and source-override entries, with a checksum on every table.
- [x] 13 host tests: four processors with one disabled, a 1.0 `RSDT`, the EBDA path, no RSDP, a bad
      RSDP checksum, a root that is not one, a table that is not a MADT, a MADT with a bad checksum,
      an entry with a zero length, an x2APIC entry, and both halves of the hint rule.
- [x] The boot report: the RSDP and how it was found, the CPU count and how many the firmware
      enabled, the IOAPIC's address, the override count, and `1 of N cpu(s) started`.
- [x] `cpus` in the shell: the same facts, asked for from inside the running system.
- [x] `run <name>` in the shell: `run fib` is `apps/fib.gvm`, interpreted with the M9 machine, its
      output in the shell's window and on serial, and a missing file as a message.
- [x] The harness boots QEMU with `-smp 4`, asserts the three firmware lines on the clean boot, and
      types `cpus`, `run fib` and `run nope` into the shell along with the M8 commands.
- [x] `sendkey`'s three missing names (`spc`, `slash`, `dot`) and `SHELL_TICKS` doubled, because the
      session now types twice as much.
- [x] A README that describes this kernel rather than M4's.

**Open.** No application processor is started: no trampoline, no per-CPU state, no spinlocks, no
APIC or IOAPIC programming, and no preemption, so two CPUs would run one cooperative thread each
rather than share the work. The PIC and the PIT remain the interrupt controller and the timer, and
the MADT's five source overrides on QEMU are counted and not obeyed. ACPI is read-only: no FADT, no
shutdown, no reboot. No port to a second architecture.

**Acceptance criteria.**

1. The boot log reports what the firmware says: the RSDP's address and how it was found, how many
   CPUs the MADT describes and how many of them the firmware enabled, the IOAPIC's address, and the
   source-override count - and the numbers follow the machine, not a constant.
2. A machine whose tables cannot be read says so and boots anyway, with one CPU and the PIC, rather
   than failing or pretending.
3. The same facts are available from inside the running system: `cpus`, typed into the shell by the
   harness, prints what the boot printed.
4. A program can be started from the shell, its output reaches the shell's window and serial, and a
   program that is not there is a message rather than a fault.
5. Host tests cover the reading: checksums, both root table formats, the legacy search, the
   bootloader's hint, and a MADT whose entries are malformed.

**Status against them.** All five met. The clean boot prints:

```text
grey-os: acpi: rsdp at 0x1f77e014 (from the bootloader's table), revision 2, 5 table(s) named
grey-os: acpi: madt: 4 cpu(s), 4 enabled, ioapic at 0xfec00000, 5 source override(s)
grey-os: smp: 1 of 4 cpu(s) started, on the bootstrap processor; the others wait for a trampoline, an ioapic and per-cpu state (ADR-037)
```

and the shell phase, typing into the running machine:

```text
grey-os: shell: > cpus
grey-os: shell: 4 cpu(s) in the firmware's tables, 4 enabled, 1 started
grey-os: shell: acpi revision 2, 5 table(s), ioapic at 0xfec00000, 5 source override(s)
grey-os: shell: > run fib
grey-os: shell: 0 1 1 2 3 5 8 13 21 34
grey-os: shell: apps/fib.gvm: halted after 193 steps
grey-os: shell: > run nope
grey-os: shell: ? apps/nope.gvm: NotFound
grey-os: shell: exit
```

Criterion 1 is `4 cpu(s), 4 enabled` on a QEMU given four CPUs and `1 cpu(s)` on one given a single
CPU, from the same image: the number comes out of the MADT. Criterion 2 is the `Err` path, which
prints `no usable tables (...)`, assumes one CPU and keeps booting. Criterion 3 is the shell's
`cpus`, which reads what the boot stored rather than re-reading the tables. Criterion 4 is `run
fib`: the series that `apps/fib.gvm` computes, in the shell's window and on serial, with the halt
line after it - and `run nope` saying `NotFound` instead of faulting. Criterion 5 is 13 host tests
over the reader; 254 on the host in total.

## M11 - Hardening before the next primitive

The review that preceded user mode found three things that would have made the next milestone unsafe
to build on: seven copies of the same `unsafe impl Sync`, no single command that ran the checks, and
a headless harness that could hang on an inherited stdin. M11 closes those. It adds no OS primitive,
which is the point - the primitives come next, and they should arrive on a green tree.

**Delivered.**

1. `kernel/src/sync.rs`: `InterruptMutex<T>` and `IrqGuard`, the kernel's one
   interior-mutability type. `lock()` returns a guard that masks maskable interrupts and restores
   the previous flag when it drops, so nesting is safe; `as_ptr()` is the documented escape hatch
   the allocators need, because `GlobalAlloc` hands them `&self`. The seven sites -
   `heap::BumpAllocator`, `frame::Allocator`, `arena::Heap`, `scheduler`, `keyboard`, `mouse`, and
   the desktop pointer in `main.rs` - now use it, and `grep -rn "unsafe impl.*Sync" kernel/src`
   finds exactly one, in `sync.rs`.
2. `cargo xtask ci`: formatting, clippy for the host tools, clippy for the kernel's real target with
   `-D warnings`, the host tests, the whole `xtask test` boot run, and a release build - one
   command, failing at the first gate.
3. `.github/workflows/ci.yml`: that command on every push and pull request, with the pinned
   toolchain, QEMU, and a `target/` cache.
4. `[profile.release]` gains `panic = "abort"` and `overflow-checks = true`.
5. `LICENSE` (MIT) and the workspace's `license` field. `README.md`'s storage claim now matches
   `fs.rs`, which formats only an all-zero sector 0 and refuses anything else.
6. `boot_headless` sets `Stdio::null()` on QEMU's stdin. `-serial stdio` made QEMU read the
   terminal as well as write it, and that read is a `SIGTTIN` - not an error - when the harness has
   no terminal, which is exactly the CI case.

**Acceptance.** `cargo xtask ci` is green from a clean `target/`: 260 host tests, six of them new
for `sync` (masking, nesting, the `static`, the exclusive borrow), and the thirteen boot phases
M0-M10 already asserted, unchanged. `git status` is clean afterwards, because every phase restores
the image it rebuilt.

**Not done, and deliberately.** `InterruptMutex` is not a spinlock: it is still single-core and
still depends on no interrupt handler taking a lock the interrupted code holds (ADR-028). A guard
is also not safe to hold across a yield - the scheduler is cooperative, so that rule is a comment
rather than a check. User mode, preemption and durable writes are the next three themes; the
milestone that changes the scheduler or the page tables needs this one green first.

## M12 - User mode

Every task until now ran in ring 0, which is a privilege level the kernel gave itself by default.
M12 is the first milestone where the CPU, not the code, decides what a program may do: a program
runs at ring 3, in a page table of its own, and can reach the kernel only through one gate.

**Delivered.**

1. `gdt`: `USER_CODE` (0x2b, index 5) and `USER_DATA` (0x33, index 6), DPL 3, installed with the
   rest of the table; `set_privilege_stack` publishes `TSS.RSP0`, which is the stack the CPU loads
   when `int 0x80` arrives from ring 3.
2. `paging`: `map_4k` now sets `USER` on the intermediate levels as well, because the CPU checks the
   bit at every level and refuses the leaf otherwise. `share_root` builds a new top-level table that
   points at the kernel's lower tables, so a program's address space shows the kernel's mappings
   without copying a frame, and `occupied` says which top-level slot the bootloader left free.
3. `usermode`: an address space per program (one guard page, one read-only code page, three stack
   pages), the `int 0x80` gate with `DPL 3`, the entry and exit halves in assembly, and three
   syscalls - write, exit, sleep. The portable half (layout, ABI, dispatch) is tested on the host;
   the half that needs a CPU is the kernel's only `x86_64`-only code in the module.
4. `idt`: the syscall gate is the one entry with `DPL 3`; a fault whose saved `cs` names ring 3 ends
   the program with `128 + vector` instead of panicking the kernel.

**Acceptance.** `cargo xtask test` boots three programs from `kernel/src/usermode.rs`: `hello`
prints one line and exits 0 after two syscalls; `count` prints three lines across six more entries,
sleeping between them, and exits with the number of writes it made - which is only right if the
callee-saved registers survive the gate; `fault` writes the kernel's own text and is killed with
`#PF` error `0x6` (user, write, present) at exit code 142, and the boot carries on to `boot ok`.
The tick counts in the reports are how the run shows the timer kept interrupting a ring 3 context.

**Not done, and deliberately.** One program at a time, entered synchronously from a kernel task:
there is no preemption of ring 3 and no user context in the scheduler, so the timer ticks but cannot
switch away from a running program. There is no user-space toolchain - the three programs are
assembly blobs in the kernel's read-only data - no `fork`, no file system in the syscall ABI, and two
arguments per call. ADR-040 records each of those as the next milestone's work rather than a gap.

## After M10 - what is not done

The brief's list ends at M10, so this section is no longer a transcription of agreed scope: it is the
`Open` list of the ten ADRs, gathered in dependency order. Nothing here is agreed and nothing here
is claimed.

The blockers are named rather than ranked. User mode and system calls were the first, and M12 has them
in the smallest form that runs: the ring, the address space and the one gate. What is still missing
there, and what everything after this assumes, is preemption (a full interrupt frame per task, then
per-CPU state) - a program that can be switched away from rather than only entered - and a loader
that reads an image the kernel did not write. Then an APIC and IOAPIC driver, which makes the PIC and
its five ignored source overrides a thing of the past; then a real TCP/IP stack or a USB stack, both
of which need DMA and a device model this kernel does not have. Storage is one extent per file with
no free-space reuse, and fixing that is a smaller job than any of the above.

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
