//! Ring 3: a program the kernel does not trust, and the one gate back.
//!
//! # What this is for
//!
//! Every task so far ran in ring 0, which means every task could turn off interrupts, write
//! `CR3` and call `hlt`. Nothing in the scheduler ever needed to stop that, because every task
//! was code this repository wrote. A user program is the first thing the kernel runs that it
//! did not write, so the isolation has to come from the CPU rather than from good manners
//! (ADR-040). That costs three pieces, and this module is all three:
//!
//! - **A ring.** `iretq` with the user selectors from [`crate::gdt`] drops the CPU to ring 3.
//!   Segment limits are inert in long mode, so this is a change of privilege and nothing else -
//!   the flat user descriptors exist only because a ring change has to name segments.
//! - **An address space.** A program gets a top-level entry the kernel's own map does not use
//!   ([`paging::share_root`] copies the kernel's entries so the switch is invisible from ring
//!   0, and the program's pages fill a free slot). Everything it can reach is one of those
//!   pages; everything else is mapped present-and-supervisor, which the CPU refuses at ring 3.
//! - **A gate.** `int 0x80` is the only vector callable from ring 3 - its gate is the only one
//!   with `DPL 3` - and it lands on the stub below, which reads the number and two arguments
//!   out of registers and returns through the same path.
//!
//! # What this deliberately is not
//!
//! Not a scheduler for user tasks: [`run`] is synchronous, called from a kernel task, and
//! returns when the program exits or faults. Not preemption: the timer still cannot take the
//! CPU away from ring 3, because a switch out of a user context would need the scheduler to
//! save and restore one through `iretq`, which is the next milestone's problem. Not a
//! filesystem: a program is bytes in memory, and the syscalls are write, exit and sleep. Each
//! of those is what a smoke test needs to prove the ring change happened and can be undone;
//! anything more would be a design decision made before the mechanism works.
//!
//! # The shape of the file
//!
//! The layout of a program's address space, the syscall numbers and the dispatch table are
//! plain data and plain functions, tested on the host. The `x86_64` half is the part that
//! needs a CPU: the stacks, the `iretq`, the page-directory switch. Keeping them apart is what
//! makes "the ABI refuses a bad pointer" a unit test rather than a QEMU phase.

use crate::memory::PAGE_BYTES;
use crate::paging;
use crate::vm::Host;

/// One page, as the layout arithmetic below wants it.
const PAGE: u64 = PAGE_BYTES;

/// Pages of code a program gets. One, because a program here is a few instructions and a
/// string: a loader that can describe an arbitrary image is a different milestone.
pub const CODE_PAGES: u64 = 1;

/// Pages of stack a program gets. Three is enough for the `int 0x80` frames plus anything a
/// test program pushes, and small enough that overflowing it is a page fault rather than a
/// silent walk into the next region.
pub const STACK_PAGES: u64 = 3;

/// The most bytes one [`SYS_WRITE`] may hand the kernel. A cap rather than the whole stack,
/// because the kernel copies the text onto its own stack before it looks at it, and a syscall
/// that can ask for an unbounded copy is a syscall that can exhaust the kernel's stack.
pub const MAX_WRITE: u64 = 4096;

/// The first top-level slot a program's pages may use.
///
/// Slot 0 holds the bootloader's low mappings and slot 2 holds the kernel at 1 GiB, so the
/// search starts above them - but it still *checks* each slot rather than assuming the ones
/// below are taken ([`pick_slot`] over [`paging::occupied`]), because where the bootloader put
/// things is a fact about the machine, not a constant this file gets to declare.
pub const FIRST_SLOT: usize = 3;

/// Where a program's pages sit inside its top-level slot.
///
/// ```text
/// slot base + 0x0000  guard, never mapped: a wild pointer below the code faults
/// slot base + 0x1000  code, one page, readable and executable, not writable
/// slot base + 0x2000  stack, three pages, readable and writable, not executable
/// slot base + 0x5000  one past the end: where the program starts with `rsp`
/// ```
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Layout {
    /// Which top-level entry the program's pages live in.
    slot: usize,
}

impl Layout {
    /// The layout of a program whose pages occupy top-level slot `slot`.
    pub const fn new(slot: usize) -> Self {
        Self { slot }
    }

    /// The address the slot begins at: its index, shifted up to the bits PML4 reads.
    const fn base(&self) -> u64 {
        (self.slot as u64) << 39
    }

    /// Where the code page starts, and therefore where execution starts.
    pub const fn code(&self) -> u64 {
        self.base() + PAGE
    }

    /// Where the stack pages start.
    pub const fn stack(&self) -> u64 {
        self.base() + 2 * PAGE
    }

    /// One past the last stack page, which is what `iretq` puts in `rsp`: a stack that starts
    /// at its top grows down into pages that are mapped.
    pub const fn stack_top(&self) -> u64 {
        self.stack() + STACK_PAGES * PAGE
    }

    /// Whether `address .. address + length` lies inside the memory a program may hand to a
    /// syscall.
    ///
    /// The whole image: the code page and the stack pages, which are adjacent by construction,
    /// and nothing else. The guard page below is excluded, and so is every address outside the
    /// slot - which matters because the kernel copies from a user pointer *in ring 0*, where
    /// the `U/S` bit no longer stops a read. The kernel's own text is mapped in this address
    /// space, so a write syscall pointed at it would otherwise copy kernel memory into the log.
    pub const fn contains(&self, address: u64, length: u64) -> bool {
        let Some(end) = address.checked_add(length) else {
            return false;
        };
        address >= self.code() && end <= self.stack_top()
    }
}

/// The first free slot at or after `from`.
///
/// Split out of [`run`] so the choice is a host test: the real call site has a live page
/// table and an address that only QEMU can check, while this has a slice of `bool`s.
pub fn pick_slot(occupied: &[bool], from: usize) -> Option<usize> {
    occupied
        .iter()
        .enumerate()
        .skip(from)
        .find(|(_, used)| !**used)
        .map(|(slot, _)| slot)
}

/// Writes text to the kernel's output, with the address in `a0` and the length in `a1`.
pub const SYS_WRITE: u64 = 1;
/// Ends the program, with its exit code in `a0`.
pub const SYS_EXIT: u64 = 2;
/// Gives up the CPU for `a0` ticks. The kernel does the sleeping, so a program cannot spin the
/// core it was given.
pub const SYS_SLEEP: u64 = 3;

/// No such syscall.
pub const ENOSYS: u64 = 38;
/// The pointer or length a syscall was given is not one the program owns.
pub const EFAULT: u64 = 14;
/// The argument is well formed but not acceptable: text that is not UTF-8, or a length over
/// [`MAX_WRITE`].
pub const EINVAL: u64 = 22;

/// What the kernel hands back in `rax`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Reply {
    /// A value the program may use: bytes written, or zero for a call that returns nothing.
    Value(i64),
    /// A failure, as one of the `E*` numbers above. Negative in `rax`, the way a Unix system
    /// call reports it, so a program can tell "wrote nothing" from "refused".
    Error(u64),
    /// The program asked to end. The stub stops returning and leaves ring 3 instead.
    Exit(i64),
}

/// The program's memory, as the kernel sees it.
///
/// A trait rather than a pointer because the dispatch table has to be testable: a host test
/// implements this over a local array and gets to check that an out-of-range pointer is
/// refused without a page fault to prove it.
pub trait Memory {
    /// Copies exactly `into.len()` bytes from `address`.
    ///
    /// `None` when any byte of the range is not the program's to read. A partial copy is not
    /// reported as one: a syscall reads all of what it was asked for or fails, because half a
    /// write is a program's bug the kernel should not paper over.
    fn read(&self, address: u64, into: &mut [u8]) -> Option<()>;
}

/// Handles one syscall, with no CPU involved.
///
/// `nr` and the two arguments are what the stub took out of `rax`, `rdi` and `rsi`; the reply
/// is what it puts back in `rax`. Everything a handler needs from the machine - output and
/// sleeping - arrives through `host`, which is the same seam [`crate::vm`] uses, so the
/// handlers a real boot runs and the handlers the host tests run are one implementation.
pub fn dispatch<M, H>(nr: u64, a0: u64, a1: u64, layout: &Layout, memory: &M, host: &mut H) -> Reply
where
    M: Memory,
    H: Host,
{
    match nr {
        SYS_WRITE => {
            if a1 > MAX_WRITE {
                return Reply::Error(EINVAL);
            }
            if !layout.contains(a0, a1) {
                return Reply::Error(EFAULT);
            }
            let length = a1 as usize;
            let mut buffer = [0u8; MAX_WRITE as usize];
            if memory.read(a0, &mut buffer[..length]).is_none() {
                return Reply::Error(EFAULT);
            }
            let Ok(text) = core::str::from_utf8(&buffer[..length]) else {
                return Reply::Error(EINVAL);
            };
            host.write(text);
            Reply::Value(a1 as i64)
        }
        SYS_EXIT => Reply::Exit(a0 as i64),
        SYS_SLEEP => {
            // The argument is clamped rather than refused: a program asking for an hour of
            // sleep wants to yield, and a tick count no `u16` can hold is not a reason to
            // return an error to a program that has no way to use the distinction.
            let ticks = u16::try_from(a0).unwrap_or(u16::MAX);
            host.sleep(ticks);
            Reply::Value(0)
        }
        _ => Reply::Error(ENOSYS),
    }
}

/// How a program ended, as the kernel reports it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Report {
    /// The program's exit code, or `128 + vector` when a fault killed it - the shell's
    /// convention, so a reader of the log can tell an exit from a kill by the number alone.
    pub exit: i64,
    /// How many times the program entered the kernel.
    pub syscalls: u64,
    /// How many bytes it asked the kernel to write.
    pub bytes: u64,
    /// Ticks that passed while it ran, which is the proof the timer kept interrupting a ring 3
    /// context and not only a ring 0 one.
    pub ticks: u64,
}

/// Why a program could not be run.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Error {
    /// The image is empty or larger than [`CODE_PAGES`].
    TooLarge,
    /// Every top-level slot from [`FIRST_SLOT`] up is in use.
    NoSlot,
    /// A frame was needed and the allocator had none.
    OutOfFrames,
    /// The page table refused a mapping.
    Map(paging::MapError),
    /// Another program is already in ring 3. There is one kernel stack for the gate and one
    /// set of statics behind it, so nesting is refused rather than half-supported.
    Busy,
}

#[cfg(target_arch = "x86_64")]
mod arch {
    use super::{pick_slot, Error, Layout, Report, CODE_PAGES, FIRST_SLOT, PAGE, STACK_PAGES};
    use crate::paging::Entry;
    use crate::sync::InterruptMutex;
    use core::sync::atomic::{AtomicBool, Ordering};
    /// Bytes of kernel stack the gate runs on.
    ///
    /// The CPU loads `TSS.RSP0` on every transition into ring 0, so this is where the five
    /// words `int 0x80` pushes land and where everything the handler calls runs. Sixteen KiB
    /// is four pages: enough for a syscall that sleeps through a timer interrupt and then
    /// copies a [`super::MAX_WRITE`] buffer, with room left for the console.
    pub const KERNEL_STACK_BYTES: usize = 16 * 1024;

    /// That stack. `align(16)` because `RSP0` has to be 16-byte aligned, and `repr(C)` so the
    /// array starts where the alignment says it does.
    #[repr(C, align(16))]
    struct KernelStack([u8; KERNEL_STACK_BYTES]);

    /// The gate's stack, one per machine. A single stack is enough because only one program
    /// runs at a time ([`super::Error::Busy`]); a second would be a scheduler for user tasks,
    /// which this milestone is not.
    static mut KERNEL_STACK: KernelStack = KernelStack([0; KERNEL_STACK_BYTES]);

    /// Where `leave_user` returns to: the stack pointer `enter_user` saved, which is a frame
    /// in the kernel function that called [`run`].
    static SAVED: InterruptMutex<u64> = InterruptMutex::new(0);

    /// The layout of the program that is running, or is about to: the syscall handlers need
    /// it to decide what the program owns.
    static LAYOUT: InterruptMutex<Layout> = InterruptMutex::new(Layout::new(0));

    /// What a program's output is handed to: a function that writes bytes somewhere.
    type Sink = fn(&[u8]);

    /// Where a program's output goes. A function pointer rather than a sink built here, so a
    /// headless boot can write to serial alone and a boot with a panel can write to both.
    static EMIT: InterruptMutex<Option<Sink>> = InterruptMutex::new(None);

    /// What the report needs and the program cannot be trusted to say.
    #[derive(Clone, Copy, Default)]
    struct Counters {
        /// Syscalls entered.
        syscalls: u64,
        /// Bytes written.
        bytes: u64,
    }

    /// The counters, updated by every entry and read once when the program ends.
    static COUNTERS: InterruptMutex<Counters> = InterruptMutex::new(Counters {
        syscalls: 0,
        bytes: 0,
    });

    /// Whether a program is in ring 3. Set for the whole call to [`run`], so nesting is
    /// refused rather than corrupting the one stack the gate has.
    static RUNNING: AtomicBool = AtomicBool::new(false);

    /// The physical window, which [`run`] needs to write a program's image into its frames.
    /// Published by [`init`] because the bootloader picked it, not this module.
    static WINDOW: InterruptMutex<u64> = InterruptMutex::new(0);

    /// Points the TSS at the gate's stack and remembers the physical window.
    ///
    /// Must run after [`crate::gdt::init`], which is what loads the task register: writing
    /// `RSP0` into a descriptor the CPU has not been told about would be writing to a table
    /// nothing reads.
    pub fn init(window: u64) {
        *WINDOW.lock() = window;
        let start = &raw mut KERNEL_STACK as *mut u8;
        // SAFETY: `KERNEL_STACK` is this module's static, so its bytes outlive every program,
        // and one past its end is a valid (if not dereferenceable) address for the register.
        let top = unsafe { start.add(KERNEL_STACK_BYTES) } as u64;
        // SAFETY: `gdt::init` has run, so the TSS descriptor is in the loaded table; `top` is
        // the end of a 16-byte-aligned static, which is what `RSP0` requires.
        unsafe { crate::gdt::set_privilege_stack(top) };
    }

    /// The address of the `int 0x80` stub, for the gate `idt` installs with `DPL 3`.
    ///
    /// A function rather than a constant for the reason `idt`'s own installer gives: an
    /// address is a relocation, and taking it here is the same relocation wherever it is
    /// taken.
    pub fn syscall_gate() -> u64 {
        syscall_stub as *const () as u64
    }

    extern "C" {
        /// The stub `global_asm` below defines. Never called from Rust: the CPU reaches it
        /// through the gate.
        #[link_name = "grey_os_syscall"]
        fn syscall_stub();

        /// The ring 3 entry and exit halves, likewise.
        #[link_name = "grey_os_enter_user"]
        fn enter_user(saved: *mut u64, entry: u64, stack: u64) -> i64;
        #[link_name = "grey_os_leave_user"]
        fn leave_user(saved: *const u64, code: u64) -> !;
    }

    // The assembly. Written as `global_asm!` rather than as naked functions for the reason the
    // scheduler's switch is: the selectors are immediates inside the instructions, so they have
    // to be literals in the string, and the `const` assertion below is what keeps the literals
    // and [`crate::gdt`] from drifting apart.
    core::arch::global_asm!(
        ".pushsection .text,\"ax\",@progbits",
        // grey_os_enter_user(saved: *mut u64, entry: u64, stack: u64) -> i64
        ".global grey_os_enter_user",
        ".type grey_os_enter_user, @function",
        "grey_os_enter_user:",
        // Callee-saved registers, because the program is allowed to clobber them and the
        // kernel frame this call was made from still needs them when it resumes.
        "    push rbx",
        "    push rbp",
        "    push r12",
        "    push r13",
        "    push r14",
        "    push r15",
        // Where `grey_os_leave_user` finds the stack to return to. Saved after the pushes, so
        // its pops land back on this function's return address.
        "    mov [rdi], rsp",
        // The data segments, which `iretq` does not restore: ring 3 running with `ds` naming a
        // ring 0 selector would fault on its first string instruction rather than on the
        // instruction that deserved it.
        "    mov ax, 0x33",
        "    mov ds, ax",
        "    mov es, ax",
        // The frame `iretq` pops, in the order it reads it: `ss`, `rsp`, `rflags`, `cs`,
        // `rip`. The `3` in the low two bits of `cs` is what makes this a return to ring 3,
        // and `IF` in `rflags` is what keeps the program interruptible - without it the timer
        // would stop the moment the program started.
        "    push 0x33",
        "    push rdx",
        "    push 0x202",
        "    push 0x2b",
        "    push rsi",
        "    iretq",
        ".size grey_os_enter_user, . - grey_os_enter_user",
        // grey_os_leave_user(saved: *const u64, code: u64) -> !
        ".global grey_os_leave_user",
        ".type grey_os_leave_user, @function",
        "grey_os_leave_user:",
        "    mov rsp, [rdi]",
        "    pop r15",
        "    pop r14",
        "    pop r13",
        "    pop r12",
        "    pop rbp",
        "    pop rbx",
        // The data segments again: `enter_user` left them naming the user's data descriptor,
        // and the kernel should not keep running with a ring 3 selector in `ds` - legal in
        // long mode because the base is ignored, and a lie about what the code below is doing.
        "    mov ax, 0x10",
        "    mov ds, ax",
        "    mov es, ax",
        // The kernel frame expects the answer in `rax`, and expects interrupts back on: the
        // gate ran with them off, and nothing below this point would turn them on again.
        "    mov rax, rsi",
        "    sti",
        "    ret",
        ".size grey_os_leave_user, . - grey_os_leave_user",
        // grey_os_syscall: the one vector a program can reach.
        ".global grey_os_syscall",
        ".type grey_os_syscall, @function",
        "grey_os_syscall:",
        // The CPU has switched to `TSS.RSP0` already and pushed `ss`, `rsp`, `rflags`, `cs`
        // and `rip`, which leaves `rsp` at 8 modulo 16. The nine pushes below take it to 0,
        // which is where `call` needs it.
        "    push rcx",
        "    push rdx",
        "    push rsi",
        "    push rdi",
        "    push r8",
        "    push r9",
        "    push r10",
        "    push r11",
        "    push rax",
        // The C ABI the handler is written with: number, then the two arguments. Pushing the
        // registers did not change them, so `rsi` still holds `a1` and `rdi` still holds `a0`.
        "    mov rdx, rsi",
        "    mov rsi, rdi",
        "    mov rdi, rax",
        "    call grey_os_syscall_rust",
        // The program's `rax` is replaced rather than restored: that is where the answer goes.
        "    add rsp, 8",
        "    pop r11",
        "    pop r10",
        "    pop r9",
        "    pop r8",
        "    pop rdi",
        "    pop rsi",
        "    pop rdx",
        "    pop rcx",
        // Back to ring 3, popping exactly the five words the CPU pushed. A program that asked
        // to exit never reaches this: its handler left this stack for the kernel's own frame.
        "    iretq",
        ".size grey_os_syscall, . - grey_os_syscall",
        ".popsection",
    );

    /// The assembly above names the two user selectors as literals. This is what keeps them
    /// honest: if `gdt` ever renumbers a slot, the build fails here rather than at the first
    /// `iretq`.
    const _: () = assert!(crate::gdt::USER_CODE == 0x2b && crate::gdt::USER_DATA == 0x33);

    /// Fills a frame with zeros through the physical window.
    ///
    /// # Safety
    ///
    /// `frame` must be a frame nothing else uses - the allocator's guarantee - and `window`
    /// must be the physical window.
    unsafe fn zero(window: u64, frame: u64) {
        // SAFETY: the caller promises the frame is unused, so nothing else reads or writes it
        // while this runs, and a frame is one aligned page of memory the window maps.
        unsafe { core::ptr::write_bytes((window + frame) as *mut u8, 0, PAGE as usize) };
    }

    /// One program's address space: a root of its own, plus every frame allocated behind it.
    struct Space {
        /// The value `CR3` is given while the program runs.
        root: u64,
        /// Everything to hand back, the root included. The kernel's own tables are not here:
        /// the root points at them rather than owning them, which is the trade
        /// [`crate::paging::share_root`] makes.
        frames: alloc::vec::Vec<u64>,
    }

    impl Space {
        /// Builds the space for `program`, mapped at `layout`.
        fn build(layout: &Layout, program: &[u8], window: u64) -> Result<Self, Error> {
            let kernel = crate::paging::root();
            let root = crate::frame::FRAMES.alloc().ok_or(Error::OutOfFrames)?;
            let mut space = Self {
                root,
                frames: alloc::vec![root],
            };
            // SAFETY: `root` is a frame the allocator just handed out, which nothing else maps
            // or reads; `kernel` is the tree `CR3` names, so it is live; and `window` is the
            // window that tree maps the kernel's frames through.
            unsafe { crate::paging::share_root(root, kernel, window) };
            match space.fill(layout, program, window) {
                Ok(()) => Ok(space),
                Err(error) => {
                    // A half-built space is worse than none: every frame goes back before the
                    // caller is told no, so a refused program costs nothing permanent.
                    space.release();
                    Err(error)
                }
            }
        }

        /// Maps the code and stack pages, then copies the image into the code pages.
        fn fill(&mut self, layout: &Layout, program: &[u8], window: u64) -> Result<(), Error> {
            let mut code = alloc::vec::Vec::new();
            for page in 0..CODE_PAGES {
                // Readable and executable, not writable. The image is copied through the
                // physical window, so nothing needs the mapping to be writable for the copy to
                // work - and a program that cannot rewrite its own code is one less thing a
                // syscall has to trust.
                let address = layout.code() + page * PAGE;
                code.push(self.map(address, Entry::USER, window)?);
            }
            for page in 0..STACK_PAGES {
                let address = layout.stack() + page * PAGE;
                let flags = Entry::USER | Entry::WRITABLE | Entry::NO_EXECUTE;
                self.map(address, flags, window)?;
            }
            for (chunk, frame) in program.chunks(PAGE as usize).zip(code) {
                // SAFETY: `frame` is one of this program's code pages, which nothing else maps
                // and no CPU is executing yet, and `chunk` is at most one page long.
                unsafe {
                    core::ptr::copy_nonoverlapping(
                        chunk.as_ptr(),
                        (window + frame) as *mut u8,
                        chunk.len(),
                    );
                }
            }
            Ok(())
        }

        /// Gives one page of the program an address, and returns the frame behind it.
        fn map(&mut self, address: u64, flags: u64, window: u64) -> Result<u64, Error> {
            let frame = crate::frame::FRAMES.alloc().ok_or(Error::OutOfFrames)?;
            self.frames.push(frame);
            // Zeroed before it is mapped: a program must not read the previous tenant of a
            // page it was handed, and on a machine that has just booted the previous tenant is
            // the firmware.
            // SAFETY: the frame came from the allocator, so nothing else uses it, and `window`
            // maps it.
            unsafe { zero(window, frame) };
            let root = self.root;
            let frames = &mut self.frames;
            // SAFETY: `root` is this space's own table and only this call builds it, `frame` is
            // unused, and the closure hands the walk a frame for each table it creates -
            // recorded first, so `release` gives it back.
            unsafe {
                crate::paging::map_4k(root, window, address, frame, flags, move || {
                    let table = crate::frame::FRAMES.alloc()?;
                    frames.push(table);
                    Some(table)
                })
            }
            .map_err(Error::Map)?;
            Ok(frame)
        }

        /// Hands every frame back, the root included.
        fn release(self) {
            for frame in self.frames {
                // The answer is ignored: every frame here came from this module, so the bitmap
                // will say each one was in use.
                crate::frame::FRAMES.free(frame);
            }
        }
    }

    /// Runs `program` in ring 3 until it exits or faults, and reports what it did.
    ///
    /// Synchronous, and the only entry point: the caller is a kernel task, and the call
    /// returns to it once the program is gone. The module docs say what that does not cover.
    pub fn run(program: &[u8], emit: fn(&[u8])) -> Result<Report, Error> {
        if program.is_empty() || program.len() > (CODE_PAGES * PAGE) as usize {
            return Err(Error::TooLarge);
        }
        if RUNNING.swap(true, Ordering::SeqCst) {
            return Err(Error::Busy);
        }
        let outcome = attempt(program, emit);
        RUNNING.store(false, Ordering::SeqCst);
        outcome
    }

    /// The part of [`run`] that can fail, split out so a failure still clears the flag.
    fn attempt(program: &[u8], emit: fn(&[u8])) -> Result<Report, Error> {
        let window = *WINDOW.lock();
        let kernel = crate::paging::root();
        // SAFETY: `kernel` is the tree `CR3` names and `window` is the window that tree maps
        // the kernel's frames through, which is what `occupied` reads.
        let occupied = unsafe { crate::paging::occupied(kernel, window) };
        let slot = pick_slot(&occupied, FIRST_SLOT).ok_or(Error::NoSlot)?;
        let layout = Layout::new(slot);
        let space = Space::build(&layout, program, window)?;
        *LAYOUT.lock() = layout;
        *EMIT.lock() = Some(emit);
        *COUNTERS.lock() = Counters::default();
        let started = crate::timer::ticks();
        // SAFETY: `space.root` maps every slot the kernel's root maps - `share_root` copied
        // them - so the kernel's own code, stack and statics stay reachable across this switch,
        // which is what makes the next instruction safe to execute.
        unsafe { crate::paging::load_root(space.root) };
        // SAFETY: `SAVED` is this module's cell, the layout is mapped with the user bit on
        // every level by `Space::fill`, and `RUNNING` says no other program is in ring 3.
        let exit = unsafe { enter_user(SAVED.as_ptr(), layout.code(), layout.stack_top()) };
        // SAFETY: switching back to the tree the kernel started in is the reverse of the call
        // above, and that tree never stopped being alive.
        unsafe { crate::paging::load_root(kernel) };
        space.release();
        *EMIT.lock() = None;
        let counters = *COUNTERS.lock();
        Ok(Report {
            exit,
            syscalls: counters.syscalls,
            bytes: counters.bytes,
            ticks: crate::timer::ticks().saturating_sub(started),
        })
    }

    /// The program's memory, as ring 0 reaches it.
    ///
    /// Reading a user address here is a plain copy, not a walk: while a program runs, `CR3`
    /// names *its* tree, so its pages are mapped in the address space this code is executing
    /// in. `dispatch` has already checked the range against the layout - that check is what
    /// makes the pointer safe - and this type is the only implementation that trusts it.
    struct UserMemory;

    impl super::Memory for UserMemory {
        fn read(&self, address: u64, into: &mut [u8]) -> Option<()> {
            // SAFETY: the caller is `dispatch`, which refuses any range `Layout::contains`
            // does not cover, so this reads inside the program's own stack pages; those are
            // mapped read-write for as long as the program is in ring 3, and `into` is a
            // distinct buffer on the kernel's stack.
            unsafe {
                core::ptr::copy_nonoverlapping(address as *const u8, into.as_mut_ptr(), into.len());
            }
            Some(())
        }
    }

    /// The [`Host`](crate::vm::Host) a syscall runs against: the boot's output function, and
    /// the scheduler's sleep.
    struct Emitter;

    impl crate::vm::Host for Emitter {
        fn write(&mut self, text: &str) {
            let emit = *EMIT.lock();
            if let Some(emit) = emit {
                emit(text.as_bytes());
            }
            COUNTERS.lock().bytes += text.len() as u64;
        }

        fn sleep(&mut self, ticks: u16) {
            // The gate ran with interrupts off, and `sleep_ticks` waits for the timer: with
            // `IF` clear that wait is a `hlt` nothing can wake. So they go back on for the
            // sleep and off again after it - the rest of the handler still runs with them
            // masked, and the stub's `iretq` restores the program's own flags on the way out.
            crate::idt::enable();
            crate::scheduler::sleep_ticks(u64::from(ticks));
            crate::idt::disable();
        }
    }

    /// What the stub calls. The system call ABI is the C one here and only here: the stub
    /// arranged `rdi`, `rsi`, `rdx` before the call, and takes the answer out of `rax`.
    ///
    /// `no_mangle` because the assembly names the symbol.
    #[no_mangle]
    extern "C" fn grey_os_syscall_rust(nr: u64, a0: u64, a1: u64) -> u64 {
        let layout = *LAYOUT.lock();
        COUNTERS.lock().syscalls += 1;
        let reply = super::dispatch(nr, a0, a1, &layout, &UserMemory, &mut Emitter);
        match reply {
            super::Reply::Value(value) => value as u64,
            // Negative in `rax`, which is how a program reads "the kernel refused": the same
            // convention a Unix system call uses, so a program's error check is one sign test.
            super::Reply::Error(error) => error.wrapping_neg(),
            super::Reply::Exit(code) => leave(code as u64),
        }
    }

    /// Returns to the kernel side of [`run`], with `code` as its answer.
    ///
    /// The other half of [`enter_user`]: it puts the stack pointer back where `enter_user`
    /// saved it, restores the callee-saved registers the program was allowed to clobber, and
    /// returns from the call that started all this.
    fn leave(code: u64) -> ! {
        let saved = *SAVED.lock();
        // SAFETY: `enter_user` stored a live stack pointer in `SAVED` and has not returned, so
        // the frame it names is the one `run` is still sitting in; `saved` itself lives on the
        // gate's stack, which `leave_user` reads before it switches away from it.
        unsafe { leave_user(&saved, code) }
    }

    /// Ends a program that faulted in ring 3.
    ///
    /// Called from [`crate::idt`] for a fault whose code segment was the user one. It is not a
    /// panic: the program is what was wrong, and a kernel that panics because a program was
    /// wrong has handed every program a way to stop the machine. The exit code is
    /// `128 + vector`, the shell's convention for "killed by a signal", so a caller can tell
    /// the difference between a program that chose its exit code and one that never got to.
    pub fn killed_by_fault(
        exception: crate::idt::Exception,
        error: Option<u64>,
        ip: u64,
        fault_address: Option<u64>,
    ) -> ! {
        match (error, fault_address) {
            (Some(code), Some(address)) => crate::serial::print_args_forced(format_args!(
                "grey-os: user: {} {} at rip {ip:#x}: error {code:#x}, address {address:#x}, \
                 killed",
                exception.mnemonic, exception.name
            )),
            (Some(code), None) => crate::serial::print_args_forced(format_args!(
                "grey-os: user: {} {} at rip {ip:#x}: error {code:#x}, killed",
                exception.mnemonic, exception.name
            )),
            (None, _) => crate::serial::print_args_forced(format_args!(
                "grey-os: user: {} {} at rip {ip:#x}: killed",
                exception.mnemonic, exception.name
            )),
        }
        leave(128 + u64::from(exception.vector))
    }

    // The programs the boot runs. Three, because each proves something the others cannot: that
    // a program runs and returns, that its registers survive the gate, and that a fault in
    // ring 3 kills the program rather than the kernel. They are assembly rather than compiled
    // code because there is no user-space toolchain in this repository yet - a `cargo` for
    // ring 3 is a later milestone, and this is the smoke test that says the ground is ready
    // for it.
    core::arch::global_asm!(
        ".pushsection .rodata,\"a\",@progbits",
        // The hello program: write once, exit zero.
        ".global grey_os_user_hello_start",
        ".global grey_os_user_hello_end",
        "grey_os_user_hello_start:",
        "    lea rdi, [rip + grey_os_user_hello_text]",
        "    lea rax, [rip + grey_os_user_hello_text_end]",
        "    sub rax, rdi",
        "    mov esi, eax",
        "    mov eax, 1",
        "    int 0x80",
        // `ud2` after the exit: if the kernel ever returned from an exit syscall, that is a
        // kernel bug and it should fault here rather than run on into whatever follows.
        "    mov eax, 2",
        "    xor edi, edi",
        "    int 0x80",
        "    ud2",
        "grey_os_user_hello_text:",
        "    .ascii \"hello from ring 3\\n\"",
        "grey_os_user_hello_text_end:",
        ".global grey_os_user_hello_end",
        "grey_os_user_hello_end:",
        // The counting program: three writes with a sleep between each, and a loop counter in
        // `r13` that has to survive all six entries into the kernel. If the gate clobbered a
        // callee-saved register the loop would either never end or end early, and the exit code
        // is the count so the log says which.
        ".global grey_os_user_count_start",
        ".global grey_os_user_count_end",
        "grey_os_user_count_start:",
        "    xor r12d, r12d",
        "    mov r13d, 3",
        "grey_os_user_count_loop:",
        "    lea rdi, [rip + grey_os_user_count_text]",
        "    lea rax, [rip + grey_os_user_count_text_end]",
        "    sub rax, rdi",
        "    mov esi, eax",
        "    mov eax, 1",
        "    int 0x80",
        "    inc r12d",
        "    mov eax, 3",
        "    mov edi, 5",
        "    int 0x80",
        "    dec r13d",
        "    jnz grey_os_user_count_loop",
        "    mov eax, 2",
        "    mov edi, r12d",
        "    int 0x80",
        "    ud2",
        "grey_os_user_count_text:",
        "    .ascii \"tick from ring 3\\n\"",
        "grey_os_user_count_text_end:",
        ".global grey_os_user_count_end",
        "grey_os_user_count_end:",
        // The faulting program: writes to the kernel's own text, which the program's page
        // table maps as present and supervisor-only. The CPU refuses on the `U/S` bit rather
        // than on a missing mapping, which is exactly the isolation this milestone claims -
        // and the report that follows is the proof the kernel survived it.
        ".global grey_os_user_fault_start",
        ".global grey_os_user_fault_end",
        "grey_os_user_fault_start:",
        "    mov rax, 0x100000000",
        "    mov qword ptr [rax], 0",
        "    ud2",
        ".global grey_os_user_fault_end",
        "grey_os_user_fault_end:",
        ".popsection",
    );

    extern "C" {
        /// The three blobs' ends, so [`blob`] can measure them.
        static grey_os_user_hello_start: u8;
        static grey_os_user_hello_end: u8;
        static grey_os_user_count_start: u8;
        static grey_os_user_count_end: u8;
        static grey_os_user_fault_start: u8;
        static grey_os_user_fault_end: u8;
    }

    /// The bytes between two symbols of the assembly above.
    ///
    /// # Safety
    ///
    /// `start` and `end` must bracket one blob, with `start` at or below `end`.
    unsafe fn blob(start: &'static u8, end: &'static u8) -> &'static [u8] {
        let length = (end as *const u8 as usize) - (start as *const u8 as usize);
        // SAFETY: the caller promises the two symbols bracket a blob of initialised bytes in
        // this binary's read-only data, which nothing writes and which lives as long as the
        // kernel does.
        unsafe { core::slice::from_raw_parts(start, length) }
    }

    /// The program that writes one line and exits zero.
    pub fn hello_program() -> &'static [u8] {
        // SAFETY: the two symbols bracket the hello blob, in that order.
        unsafe { blob(&grey_os_user_hello_start, &grey_os_user_hello_end) }
    }

    /// The program that writes three lines, sleeping between them, and exits with the number of
    /// writes it made.
    pub fn count_program() -> &'static [u8] {
        // SAFETY: as above, for the counting blob.
        unsafe { blob(&grey_os_user_count_start, &grey_os_user_count_end) }
    }

    /// The program that writes to the kernel's text and is killed for it.
    pub fn fault_program() -> &'static [u8] {
        // SAFETY: as above, for the faulting blob.
        unsafe { blob(&grey_os_user_fault_start, &grey_os_user_fault_end) }
    }
}

#[cfg(target_arch = "x86_64")]
pub use arch::{
    count_program, fault_program, hello_program, init, killed_by_fault, run, syscall_gate,
};

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::string::String;

    /// The slot every test program uses. Not [`FIRST_SLOT`] itself, because a test that only
    /// ever saw slot 3 would not notice a `base` that dropped the shift.
    const SLOT: usize = 7;

    /// The memory a test program pretends to have: the bytes below, mapped at the start of its
    /// stack, and nothing else.
    struct Space {
        bytes: [u8; 64],
    }

    impl Space {
        fn new(text: &str) -> Self {
            let mut bytes = [0u8; 64];
            bytes[..text.len()].copy_from_slice(text.as_bytes());
            Self { bytes }
        }
    }

    impl Memory for Space {
        fn read(&self, address: u64, into: &mut [u8]) -> Option<()> {
            let layout = Layout::new(SLOT);
            if !layout.contains(address, into.len() as u64) {
                return None;
            }
            let offset = usize::try_from(address - layout.stack()).ok()?;
            into.copy_from_slice(self.bytes.get(offset..offset + into.len())?);
            Some(())
        }
    }

    /// A host that records what it was told to write and how long it was told to sleep.
    #[derive(Default)]
    struct Recorder {
        output: String,
        slept: alloc::vec::Vec<u16>,
    }

    impl Host for Recorder {
        fn write(&mut self, text: &str) {
            self.output.push_str(text);
        }

        fn sleep(&mut self, ticks: u16) {
            self.slept.push(ticks);
        }
    }

    #[test]
    fn the_layout_puts_a_guard_below_the_code_and_the_stack_on_top() {
        let layout = Layout::new(SLOT);
        assert_eq!(layout.base(), 7 << 39);
        assert_eq!(layout.code(), (7 << 39) + 0x1000);
        assert_eq!(layout.stack(), (7 << 39) + 0x2000);
        assert_eq!(layout.stack_top(), (7 << 39) + 0x5000);
        // The guard page below the code is the one address nothing maps, so it is the one a
        // syscall must refuse even though everything above it belongs to the program.
        assert!(!layout.contains(7 << 39, 16));
        assert!(!layout.contains((7 << 39) + 0xfff, 16));
        // The image - the code page first, then the stack - is entirely the program's, and one
        // byte past its end is not.
        assert!(layout.contains(layout.code(), PAGE));
        assert!(layout.contains(layout.stack(), STACK_PAGES * PAGE));
        assert!(!layout.contains(layout.stack_top(), 1));
        assert!(!layout.contains(layout.stack_top() - 1, 2));
    }

    #[test]
    fn a_length_that_would_wrap_is_not_in_the_image() {
        let layout = Layout::new(SLOT);
        // `address + length` overflows, so there is no window it could be inside - and a
        // `contains` that wrapped would answer `true` for a range starting in the stack.
        assert!(!layout.contains(layout.stack(), u64::MAX));
    }

    #[test]
    fn the_first_free_slot_is_the_one_used() {
        let mut occupied = [false; 8];
        occupied[3] = true;
        occupied[4] = true;
        assert_eq!(pick_slot(&occupied, 3), Some(5));
        // A slot below `from` that is free is not a candidate: the search starts where the
        // caller says the program's side of the map does.
        occupied[0] = false;
        assert_eq!(pick_slot(&occupied, 4), Some(5));
        // Nothing free is a refusal, not a panic and not slot 0.
        occupied.fill(true);
        assert_eq!(pick_slot(&occupied, 0), None);
        // An empty table is no evidence of a free slot either.
        assert_eq!(pick_slot(&[], 0), None);
    }

    #[test]
    fn a_write_reaches_the_host_and_reports_its_length() {
        let layout = Layout::new(SLOT);
        let memory = Space::new("hello from ring 3");
        let mut host = Recorder::default();
        let reply = dispatch(SYS_WRITE, layout.stack(), 17, &layout, &memory, &mut host);
        assert_eq!(reply, Reply::Value(17));
        assert_eq!(host.output, "hello from ring 3");
    }

    #[test]
    fn a_write_outside_the_image_is_refused_without_touching_the_host() {
        let layout = Layout::new(SLOT);
        let memory = Space::new("hello");
        let mut host = Recorder::default();
        // The kernel's own text is mapped in every program's address space, so a syscall that
        // read any mapped address would happily copy it into the log. It must not.
        let kernel_text = 0x1_0000_0000;
        assert_eq!(
            dispatch(SYS_WRITE, kernel_text, 5, &layout, &memory, &mut host),
            Reply::Error(EFAULT)
        );
        // One byte past the top of the stack is the same refusal, so the window has an end.
        assert_eq!(
            dispatch(
                SYS_WRITE,
                layout.stack_top() - 1,
                2,
                &layout,
                &memory,
                &mut host
            ),
            Reply::Error(EFAULT)
        );
        // A length past the cap is refused before the copy, which is the point of the cap.
        assert_eq!(
            dispatch(
                SYS_WRITE,
                layout.stack(),
                MAX_WRITE + 1,
                &layout,
                &memory,
                &mut host
            ),
            Reply::Error(EINVAL)
        );
        assert!(host.output.is_empty());
    }

    #[test]
    fn text_that_is_not_utf8_is_refused() {
        let layout = Layout::new(SLOT);
        let mut memory = Space::new("");
        memory.bytes[0] = 0xff;
        memory.bytes[1] = 0xfe;
        let mut host = Recorder::default();
        assert_eq!(
            dispatch(SYS_WRITE, layout.stack(), 2, &layout, &memory, &mut host),
            Reply::Error(EINVAL)
        );
        assert!(host.output.is_empty());
    }

    #[test]
    fn exit_carries_the_code_and_sleep_is_clamped() {
        let layout = Layout::new(SLOT);
        let memory = Space::new("");
        let mut host = Recorder::default();
        assert_eq!(
            dispatch(SYS_EXIT, 7, 0, &layout, &memory, &mut host),
            Reply::Exit(7)
        );
        // A negative exit code is the program's business; the kernel only carries it.
        assert_eq!(
            dispatch(SYS_EXIT, u64::MAX, 0, &layout, &memory, &mut host),
            Reply::Exit(-1)
        );
        assert_eq!(
            dispatch(SYS_SLEEP, 5, 0, &layout, &memory, &mut host),
            Reply::Value(0)
        );
        // One tick more than a `u16` holds becomes the longest sleep a tick count can name,
        // rather than wrapping to a short one.
        assert_eq!(
            dispatch(SYS_SLEEP, 0x1_0000, 0, &layout, &memory, &mut host),
            Reply::Value(0)
        );
        assert_eq!(host.slept, [5, u16::MAX]);
    }

    #[test]
    fn a_number_nothing_implements_is_refused() {
        let layout = Layout::new(SLOT);
        let memory = Space::new("");
        let mut host = Recorder::default();
        for nr in [0, 4, 60, u64::MAX] {
            assert_eq!(
                dispatch(nr, 0, 0, &layout, &memory, &mut host),
                Reply::Error(ENOSYS)
            );
        }
        assert!(host.output.is_empty() && host.slept.is_empty());
    }
}
