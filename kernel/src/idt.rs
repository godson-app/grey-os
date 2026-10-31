//! The interrupt descriptor table, and the handlers behind every exception vector.
//!
//! Owning the IDT is what turns a CPU fault from a triple fault into a report. Each handler
//! names its vector, the error code the CPU pushed and the instruction that faulted, prints
//! one line on serial and then panics, so the M0/M1 path paints the panel and QEMU exits with
//! a failure status (ADR-028).
//!
//! Handlers are written with `extern "x86-interrupt"` rather than hand-rolled stubs: the
//! compiler generates the prologue and epilogue, which is where the vectors that push an
//! error code, the `iretq` and register preservation are easy to get subtly wrong. The frame
//! type is [`InterruptFrame`], declared here rather than imported: `core` ships the ABI but not
//! the frame, and the fields this kernel reads are the architecture's fixed layout.
//!
//! The gate layout and the exception naming are plain data and are host-tested; everything
//! that installs a gate or runs one is `x86_64`-only.

/// The gate is present, so the CPU may deliver this vector.
const PRESENT: u8 = 0x80;

/// An interrupt gate: the CPU clears `IF` on entry, so a fault handler cannot itself be
/// interrupted by the timer.
const INTERRUPT_GATE: u8 = 0x0e;

/// One 16-byte gate descriptor (Intel SDM 3A, 6.14.1).
#[repr(C)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Entry {
    offset_low: u16,
    selector: u16,
    ist: u8,
    options: u8,
    offset_mid: u16,
    offset_high: u32,
    reserved: u32,
}

impl Entry {
    /// A gate the CPU will not deliver: present bit clear, everything else zero.
    pub const fn absent() -> Self {
        Self {
            offset_low: 0,
            selector: 0,
            ist: 0,
            options: 0,
            offset_mid: 0,
            offset_high: 0,
            reserved: 0,
        }
    }

    /// A gate for `handler`, run on `selector`, using `ist` (1..=7) or the current stack (0),
    /// and callable from ring `dpl`.
    pub const fn interrupt_gate(handler: u64, selector: u16, ist: u8, dpl: u8) -> Self {
        Self {
            offset_low: handler as u16,
            selector,
            ist: ist & 0x7,
            options: PRESENT | (dpl & 0x3) << 5 | INTERRUPT_GATE,
            offset_mid: (handler >> 16) as u16,
            offset_high: (handler >> 32) as u32,
            reserved: 0,
        }
    }

    /// The handler address, reassembled from the three fields it is split across.
    pub const fn handler(self) -> u64 {
        (self.offset_low as u64)
            | ((self.offset_mid as u64) << 16)
            | ((self.offset_high as u64) << 32)
    }

    /// Whether the CPU may deliver this vector.
    pub const fn is_present(self) -> bool {
        self.options & PRESENT != 0
    }

    /// The code selector the gate names, which has to exist in the GDT.
    pub const fn selector(self) -> u16 {
        self.selector
    }

    /// The stack index from the task state segment, or zero for "the current stack".
    pub const fn ist(self) -> u8 {
        self.ist & 0x7
    }

    /// The ring the vector may be invoked from.
    pub const fn dpl(self) -> u8 {
        (self.options >> 5) & 0x3
    }
}

/// The part of the stack frame the CPU pushes on entry that every vector shares.
///
/// `RSP` and `SS` follow these three, but only when the CPU switched stacks - a ring change or
/// an IST entry - so a handler that is always reached from ring 0 must not read them. The
/// kernel only reads the instruction pointer, which is always first.
///
/// The layout is the architecture's, not this kernel's. A handler takes the frame **by value**:
/// the ABI reads the arguments out of the interrupted stack, so the frame lands on `RSP + 8`
/// (the error code occupies `RSP + 0` on the vectors that push one) and taking it by reference
/// would make the handler dereference the saved `RIP` as if it were a pointer to the frame.
#[repr(C)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct InterruptFrame {
    /// Where execution was when the interrupt arrived.
    pub instruction_pointer: u64,
    /// The code segment. Anything but [`crate::gdt::KERNEL_CODE`] means the fault came from
    /// somewhere this kernel does not own.
    pub code_segment: u64,
    /// `RFLAGS` as it was before the CPU cleared `IF`.
    pub cpu_flags: u64,
}

/// One entry of the architectural exception table.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Exception {
    /// The vector number.
    pub vector: u8,
    /// The mnemonic the manuals use, so a report can be matched against them.
    pub mnemonic: &'static str,
    /// The name in words, for a human reading a serial log.
    pub name: &'static str,
    /// Whether the CPU pushes an error code before entering the handler.
    pub error_code: bool,
}

/// Vectors 0..=21, which is every exception this kernel handles. A vector outside the table
/// is left absent on purpose: the CPU turns an undeliverable vector into a `#GP`, and that
/// report says more than a handler that pretends to know what it caught.
pub const EXCEPTIONS: [Exception; 22] = [
    Exception {
        vector: 0,
        mnemonic: "#DE",
        name: "divide error",
        error_code: false,
    },
    Exception {
        vector: 1,
        mnemonic: "#DB",
        name: "debug",
        error_code: false,
    },
    Exception {
        vector: 2,
        mnemonic: "NMI",
        name: "non-maskable interrupt",
        error_code: false,
    },
    Exception {
        vector: 3,
        mnemonic: "#BP",
        name: "breakpoint",
        error_code: false,
    },
    Exception {
        vector: 4,
        mnemonic: "#OF",
        name: "overflow",
        error_code: false,
    },
    Exception {
        vector: 5,
        mnemonic: "#BR",
        name: "bound range exceeded",
        error_code: false,
    },
    Exception {
        vector: 6,
        mnemonic: "#UD",
        name: "invalid opcode",
        error_code: false,
    },
    Exception {
        vector: 7,
        mnemonic: "#NM",
        name: "device not available",
        error_code: false,
    },
    Exception {
        vector: 8,
        mnemonic: "#DF",
        name: "double fault",
        error_code: true,
    },
    Exception {
        vector: 9,
        mnemonic: "#CO",
        name: "coprocessor segment overrun",
        error_code: false,
    },
    Exception {
        vector: 10,
        mnemonic: "#TS",
        name: "invalid task state segment",
        error_code: true,
    },
    Exception {
        vector: 11,
        mnemonic: "#NP",
        name: "segment not present",
        error_code: true,
    },
    Exception {
        vector: 12,
        mnemonic: "#SS",
        name: "stack-segment fault",
        error_code: true,
    },
    Exception {
        vector: 13,
        mnemonic: "#GP",
        name: "general protection fault",
        error_code: true,
    },
    Exception {
        vector: 14,
        mnemonic: "#PF",
        name: "page fault",
        error_code: true,
    },
    Exception {
        vector: 15,
        mnemonic: "-",
        name: "reserved",
        error_code: false,
    },
    Exception {
        vector: 16,
        mnemonic: "#MF",
        name: "x87 floating-point exception",
        error_code: false,
    },
    Exception {
        vector: 17,
        mnemonic: "#AC",
        name: "alignment check",
        error_code: true,
    },
    Exception {
        vector: 18,
        mnemonic: "#MC",
        name: "machine check",
        error_code: false,
    },
    Exception {
        vector: 19,
        mnemonic: "#XM",
        name: "SIMD floating-point exception",
        error_code: false,
    },
    Exception {
        vector: 20,
        mnemonic: "#VE",
        name: "virtualisation exception",
        error_code: false,
    },
    Exception {
        vector: 21,
        mnemonic: "#CP",
        name: "control protection exception",
        error_code: true,
    },
];

/// The CPU's error code for `#PF`, bit by bit (Intel SDM 3A, 4.7).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct PageFaultError(u64);

impl PageFaultError {
    /// Wraps the raw error code.
    pub const fn new(bits: u64) -> Self {
        Self(bits)
    }

    /// Set when a present page refused the access, clear when there was no mapping at all.
    pub const fn protection(self) -> bool {
        self.0 & 0x1 != 0
    }

    /// Set for a write, clear for a read or an instruction fetch.
    pub const fn write(self) -> bool {
        self.0 & 0x2 != 0
    }

    /// Set when the access came from ring 3.
    pub const fn user(self) -> bool {
        self.0 & 0x4 != 0
    }

    /// Set when a reserved bit was set in a paging structure: a bug in the tables, not in
    /// the address that was touched.
    pub const fn reserved_bit(self) -> bool {
        self.0 & 0x8 != 0
    }

    /// Set when the access was an instruction fetch (requires the NX feature).
    pub const fn instruction_fetch(self) -> bool {
        self.0 & 0x10 != 0
    }

    /// The raw code, for a report that has to print it.
    pub const fn bits(self) -> u64 {
        self.0
    }
}

#[cfg(target_arch = "x86_64")]
mod arch {
    use super::{Entry, Exception, InterruptFrame, PageFaultError, EXCEPTIONS};
    use crate::gdt::KERNEL_CODE;
    use crate::serial;
    use core::arch::asm;
    use core::mem::size_of;
    use core::sync::atomic::{AtomicU64, Ordering};

    /// First vector the PIC's IRQs arrive on, once [`crate::pic::init`] has run.
    const IRQ_BASE: u8 = 0x20;

    /// The one vector a program may raise itself.
    ///
    /// `0x80` because it is the hole the PC's own software-interrupt custom left: the first 32
    /// vectors are the CPU's, `0x20..=0x2f` are the PIC's, and everything above is free - so
    /// the choice is about being recognisable in a log, not about avoiding a collision.
    const SYSCALL_VECTOR: usize = 0x80;

    /// The table itself. Built once during boot, then only read - by the CPU as well as by
    /// [`gates`].
    static mut TABLE: [Entry; 256] = [Entry::absent(); 256];

    /// What `lidt` is handed: a limit and a base, in that order, packed.
    #[repr(C, packed)]
    struct TablePointer {
        limit: u16,
        base: u64,
    }

    /// IRQs that arrived with no driver behind them. Counted rather than printed: a handler
    /// may not log, and may not allocate (ADR-028).
    static UNEXPECTED: AtomicU64 = AtomicU64::new(0);

    /// Reads `CR2`, which the CPU loads with the address that faulted in `#PF`.
    fn read_cr2() -> u64 {
        let value: u64;
        // SAFETY: `CR2` is a read-only register from this ring; reading it has no side
        // effects and cannot itself fault.
        unsafe {
            asm!(
                "mov {}, cr2",
                out(reg) value,
                options(nomem, nostack, preserves_flags)
            )
        }
        value
    }

    /// Reports a CPU exception on serial and hands over to the panic path.
    ///
    /// Serial first, and forced: the fault may have happened inside a console write, so a
    /// lock that is held is expected rather than exceptional (ADR-028). `panic!` then carries
    /// the same story to the panel and to QEMU's exit status.
    fn report(exception: Exception, error: Option<u64>, frame: &InterruptFrame) -> ! {
        let ip = frame.instruction_pointer;
        // A fault from ring 3 belongs to the program, not to the kernel: it ends the program
        // with a code that says which vector killed it, and the kernel carries on. The low two
        // bits of the saved `cs` are the privilege level the CPU was at - the only field of a
        // selector long mode still consults - so this is the check, not a guess from the
        // address.
        if frame.code_segment & 3 == 3 {
            // `CR2` holds a valid faulting address for `#PF` and nothing else.
            let address = (exception.vector == 14).then(read_cr2);
            crate::usermode::killed_by_fault(exception, error, ip, address);
        }
        match (exception.vector, error) {
            (14, Some(code)) => {
                let fault = PageFaultError::new(code);
                serial::print_args_forced(format_args!(
                    "[exception] #PF page fault: error {code:#x} on a {} to a {} page, \
                     rip {ip:#x}, cr2 {:#x}",
                    if fault.write() { "write" } else { "read" },
                    if fault.protection() {
                        "present"
                    } else {
                        "not-present"
                    },
                    read_cr2(),
                ));
            }
            (_, Some(code)) => serial::print_args_forced(format_args!(
                "[exception] {} {}: error {code:#x}, rip {ip:#x}",
                exception.mnemonic, exception.name
            )),
            (_, None) => serial::print_args_forced(format_args!(
                "[exception] {} {}: no error code, rip {ip:#x}",
                exception.mnemonic, exception.name
            )),
        }
        panic!("{} at {ip:#x}", exception.name);
    }

    /// Defines one handler per exception vector, plus the list [`init`] installs.
    ///
    /// `with` and `without` pick the ABI signature: the vectors that push an error code take
    /// it as a second argument, and getting that wrong is a handler reading a return address
    /// as an error code.
    macro_rules! exception_handlers {
        ($($vector:literal => $name:ident, $error:ident, $ist:literal;)*) => {
            $(
                exception_handlers!(@one $vector, $name, $error);
            )*

            /// Writes one gate per handler this macro defined.
            ///
            /// A function rather than a `const` table because a handler's address is a
            /// pointer, and a pointer cannot be cast to an integer during constant
            /// evaluation. Taking the address here is the same relocation either way.
            ///
            /// # Safety
            ///
            /// `table` must point at the kernel's own table, and interrupts must still be
            /// disabled while it is being written.
            unsafe fn install_exception_gates(table: *mut [Entry; 256]) {
                $(
                    // SAFETY: the caller promises `table` is a live, writable table of 256
                    // gates; `$vector` is in 0..=21.
                    unsafe {
                        (*table)[$vector] = Entry::interrupt_gate(
                            $name as *const () as u64,
                            KERNEL_CODE,
                            $ist,
                            0,
                        );
                    }
                )*
            }
        };
        (@one $vector:literal, $name:ident, without) => {
            extern "x86-interrupt" fn $name(frame: InterruptFrame) {
                report(EXCEPTIONS[$vector], None, &frame);
            }
        };
        (@one $vector:literal, $name:ident, with) => {
            extern "x86-interrupt" fn $name(frame: InterruptFrame, error: u64) {
                report(EXCEPTIONS[$vector], Some(error), &frame);
            }
        };
    }

    exception_handlers! {
        0 => divide_error, without, 0;
        1 => debug, without, 0;
        2 => non_maskable_interrupt, without, 0;
        3 => breakpoint, without, 0;
        4 => overflow, without, 0;
        5 => bound_range, without, 0;
        6 => invalid_opcode, without, 0;
        7 => device_not_available, without, 0;
        8 => double_fault, with, 1;
        9 => coprocessor_overrun, without, 0;
        10 => invalid_tss, with, 0;
        11 => segment_not_present, with, 0;
        12 => stack_segment_fault, with, 0;
        13 => general_protection, with, 0;
        14 => page_fault, with, 0;
        15 => reserved, without, 0;
        16 => x87_floating_point, without, 0;
        17 => alignment_check, with, 0;
        18 => machine_check, without, 0;
        19 => simd_floating_point, without, 0;
        20 => virtualisation, without, 0;
        21 => control_protection, with, 0;
    }

    /// Defines a stub per IRQ line, plus the list [`init`] installs.
    ///
    /// Until a driver claims a line, the stub counts it and acknowledges it. Counting is the
    /// whole handler: an IRQ nobody expected must not print, because the console it would
    /// print to is the thing the interrupted code may be halfway through using (ADR-028).
    macro_rules! irq_stubs {
        ($($irq:literal => $name:ident),* $(,)?) => {
            $(
                extern "x86-interrupt" fn $name(_frame: InterruptFrame) {
                    UNEXPECTED.fetch_add(1, Ordering::Relaxed);
                    crate::pic::eoi($irq);
                }
            )*

            /// Writes the counter stub for every IRQ line this macro defined.
            ///
            /// # Safety
            ///
            /// `table` must point at the kernel's own table, and interrupts must still be
            /// disabled while it is being written.
            unsafe fn install_irq_stubs(table: *mut [Entry; 256]) {
                $(
                    // SAFETY: the caller promises `table` is a live, writable table of 256
                    // gates; `IRQ_BASE + $irq` is inside 0x20..=0x2f.
                    unsafe {
                        (*table)[usize::from(IRQ_BASE + $irq)] = Entry::interrupt_gate(
                            $name as *const () as u64,
                            KERNEL_CODE,
                            0,
                            0,
                        );
                    }
                )*
            }
        };
    }

    irq_stubs! {
        0 => irq0, 1 => irq1, 2 => irq2, 3 => irq3,
        4 => irq4, 5 => irq5, 6 => irq6, 7 => irq7,
        8 => irq8, 9 => irq9, 10 => irq10, 11 => irq11,
        12 => irq12, 13 => irq13, 14 => irq14, 15 => irq15,
    }

    /// Builds the table, installs every gate and points the CPU at it.
    ///
    /// Must run once, after [`crate::gdt::init`] - the gates name a code selector that only
    /// exists once the GDT is loaded - and before interrupts are enabled.
    pub fn init() {
        // SAFETY: `TABLE` is written here, once, during boot with interrupts still disabled,
        // and everything afterwards only reads it. The writes go through a raw pointer, so no
        // `&mut` to a `static mut` is formed - and that same pointer is what both installers
        // take, along with the disabled interrupts they require.
        unsafe {
            let table = &raw mut TABLE;
            install_exception_gates(table);
            install_irq_stubs(table);
            // The gate back: the only entry in this table with `DPL 3`, which is what makes it
            // the only one a ring 3 program can raise. `ist 0` sends the transition through
            // `TSS.RSP0`, so the stub starts on the kernel's own stack rather than on the
            // program's.
            (*table)[SYSCALL_VECTOR] =
                Entry::interrupt_gate(crate::usermode::syscall_gate(), KERNEL_CODE, 0, 3);
            let pointer = TablePointer {
                limit: (size_of::<[Entry; 256]>() - 1) as u16,
                base: table as u64,
            };
            // SAFETY: `pointer` describes `TABLE`, which outlives the run, and its `limit` is
            // the table's size in bytes minus one, which is what `lidt` reads.
            asm!(
                "lidt [{}]",
                in(reg) &pointer,
                options(readonly, nostack, preserves_flags)
            );
        }
    }

    /// Points IRQ `irq` at `handler`, replacing the counter stub [`init`] installed.
    ///
    /// The handler takes the frame by value, like every other handler in this module: the ABI
    /// reads it out of the interrupted stack.
    pub fn set_irq(irq: u8, handler: extern "x86-interrupt" fn(InterruptFrame)) {
        // SAFETY: called during boot, before interrupts are enabled; the table is otherwise
        // only ever read.
        unsafe {
            let table = &raw mut TABLE;
            (*table)[usize::from(IRQ_BASE + irq)] =
                Entry::interrupt_gate(handler as usize as u64, KERNEL_CODE, 0, 0);
        }
    }

    /// How many gates are present: what the boot log reports as the table's coverage.
    pub fn gates() -> usize {
        // SAFETY: reads a table that `init` finished writing before interrupts were enabled,
        // and which nothing writes afterwards. A shared reference is all this needs.
        let table = unsafe { &*core::ptr::addr_of!(TABLE) };
        table.iter().filter(|entry| entry.is_present()).count()
    }

    /// IRQs that arrived with no driver behind them since boot: a stray line, or a masking
    /// bug in the PIC setup.
    pub fn unexpected_irqs() -> u64 {
        UNEXPECTED.load(Ordering::Relaxed)
    }

    /// Lets maskable interrupts through.
    ///
    /// Only after every handler is installed and any state a handler may observe is
    /// consistent: the timer can arrive one instruction later (ADR-028).
    pub fn enable() {
        // SAFETY: `sti` sets the interrupt flag, which is the point of calling it. It has no
        // memory operands and does not touch the stack.
        unsafe { asm!("sti", options(nomem, nostack)) }
    }

    /// Blocks maskable interrupts. `#NMI` and faults still arrive.
    pub fn disable() {
        // SAFETY: `cli` clears the interrupt flag. Nothing is delayed by it except interrupt
        // delivery, which is exactly what the caller is asking for.
        unsafe { asm!("cli", options(nomem, nostack)) }
    }

    /// Whether maskable interrupts are currently allowed through.
    ///
    /// A driver that has to touch state an interrupt handler also touches masks interrupts
    /// for the length of that touch, and needs to know whether to put the flag back. This
    /// reads `RFLAGS` rather than remembering a count, because the boot set the flag at one
    /// point and there is nothing to count.
    pub fn enabled() -> bool {
        let flags: u64;
        // SAFETY: `pushfq` puts the flags on the stack and `pop` takes them into a
        // register, which the compiler has reserved: the two instructions leave the stack
        // exactly as they found it and touch no memory the compiler knows about.
        unsafe {
            asm!("pushfq", "pop {}", out(reg) flags, options(preserves_flags));
        }
        flags & (1 << 9) != 0
    }
}

#[cfg(target_arch = "x86_64")]
pub use arch::{disable, enable, enabled, gates, init, set_irq, unexpected_irqs};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_gate_is_sixteen_bytes_and_present() {
        assert_eq!(core::mem::size_of::<Entry>(), 16);
        let gate = Entry::interrupt_gate(0x1234, 0x08, 0, 0);
        assert!(gate.is_present());
        assert_eq!(gate.selector(), 0x08);
        assert_eq!(gate.ist(), 0);
        assert_eq!(gate.dpl(), 0);
    }

    #[test]
    fn the_interrupt_frame_starts_with_the_instruction_pointer() {
        // The CPU pushes RIP first, then CS and RFLAGS. The kernel reads RIP and nothing
        // beyond it, because RSP and SS only follow on a stack switch.
        assert_eq!(core::mem::size_of::<InterruptFrame>(), 24);
        assert_eq!(
            core::mem::offset_of!(InterruptFrame, instruction_pointer),
            0
        );
        assert_eq!(core::mem::offset_of!(InterruptFrame, code_segment), 8);
        assert_eq!(core::mem::offset_of!(InterruptFrame, cpu_flags), 16);
    }

    #[test]
    fn the_handler_address_survives_being_split_into_three_fields() {
        // A 64-bit address is cut into 16, 16 and 32 bits at offsets 0, 6 and 8.
        let handler = 0x0000_ffff_8000_1234_u64;
        let gate = Entry::interrupt_gate(handler, 0x08, 0, 0);
        assert_eq!(gate.handler(), handler);
        assert_eq!(gate.offset_low, 0x1234);
        assert_eq!(gate.offset_mid, 0x8000);
        assert_eq!(gate.offset_high, 0x0000_ffff);
    }

    #[test]
    fn the_stack_index_and_ring_are_carried_in_their_own_fields() {
        let gate = Entry::interrupt_gate(0x1000, 0x08, 1, 3);
        assert_eq!(gate.ist(), 1);
        assert_eq!(gate.dpl(), 3);
        // Interrupt gate, not trap gate: `IF` is cleared on entry.
        assert_eq!(gate.options & 0x0f, INTERRUPT_GATE);
        assert_eq!(gate.reserved, 0);
    }

    #[test]
    fn an_absent_gate_has_nothing_set() {
        let gate = Entry::absent();
        assert!(!gate.is_present());
        assert_eq!(gate.handler(), 0);
        assert_eq!(gate.selector(), 0);
    }

    #[test]
    fn every_vector_up_to_21_is_handled_once_and_in_order() {
        assert_eq!(EXCEPTIONS.len(), 22);
        for (index, exception) in EXCEPTIONS.iter().enumerate() {
            assert_eq!(usize::from(exception.vector), index);
            assert!(!exception.name.is_empty());
            assert!(!exception.mnemonic.is_empty());
        }
    }

    #[test]
    fn exactly_the_documented_vectors_carry_an_error_code() {
        // Intel SDM 3A, 6.3.1: these are the vectors the CPU pushes an error code for, and a
        // handler that expects one where there is none reads the return address as a code.
        let with_code: Vec<u8> = EXCEPTIONS
            .iter()
            .filter(|exception| exception.error_code)
            .map(|exception| exception.vector)
            .collect();
        assert_eq!(with_code, [8, 10, 11, 12, 13, 14, 17, 21]);
    }

    #[test]
    fn the_page_fault_error_code_decodes_bit_by_bit() {
        let missing_read = PageFaultError::new(0x0);
        assert!(!missing_read.protection());
        assert!(!missing_read.write());
        assert!(!missing_read.user());
        assert!(!missing_read.instruction_fetch());

        let write_to_present_user_page = PageFaultError::new(0x7);
        assert!(write_to_present_user_page.protection());
        assert!(write_to_present_user_page.write());
        assert!(write_to_present_user_page.user());
        assert!(!write_to_present_user_page.reserved_bit());

        let broken_table = PageFaultError::new(0x1b);
        assert!(broken_table.reserved_bit());
        assert!(broken_table.instruction_fetch());
        assert_eq!(broken_table.bits(), 0x1b);
    }
}
