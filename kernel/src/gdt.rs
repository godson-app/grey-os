//! The image's own global descriptor table, and the task state segment behind it.
//!
//! Until M2 the kernel ran on the bootloader's tables. Owning them is a prerequisite for
//! owning the IDT: a gate names a code selector, so an IDT that points at a descriptor the
//! bootloader did not keep is a fault inside a fault. The TSS exists for one reason here - it
//! carries the stack a double fault runs on, so `#DF` reports instead of triple-faulting
//! (ADR-028).
//!
//! The descriptor and TSS layouts are plain data and are tested on the host; the `lgdt`/`ltr`
//! half is `x86_64`-only, because inline assembly does not exist on the machine that runs the
//! tests.

use core::mem::size_of;

/// Selector of the 64-bit kernel code descriptor (table index 1).
pub const KERNEL_CODE: u16 = 1 << 3;

/// Selector of the kernel data descriptor (table index 2).
pub const KERNEL_DATA: u16 = 2 << 3;

/// Selector of the task state segment (table index 3; its descriptor is two slots long).
pub const TSS: u16 = 3 << 3;

/// Bytes of stack reserved for the double-fault handler.
pub const STACK_BYTES: usize = 16 * 1024;

/// Present, ring 0, executable and readable.
///
/// The four encoding bytes below are read by the arch half and asserted by the host tests;
/// a host build that is not running tests uses neither, so they are compiled out there.
#[cfg(any(target_arch = "x86_64", test))]
const CODE_ACCESS: u8 = 0x9a;

/// Present, ring 0, readable and writable data.
#[cfg(any(target_arch = "x86_64", test))]
const DATA_ACCESS: u8 = 0x92;

/// Present, ring 0, available 64-bit TSS.
const TSS_ACCESS: u8 = 0x89;

/// 4 KiB granularity, long mode: the bits a 64-bit code segment needs.
#[cfg(any(target_arch = "x86_64", test))]
const CODE_FLAGS: u8 = 0xa;

/// 4 KiB granularity, 32-bit default operand size, for the data segment.
#[cfg(any(target_arch = "x86_64", test))]
const DATA_FLAGS: u8 = 0xc;

/// One 8-byte descriptor, already packed the way the CPU reads it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Descriptor(u64);

impl Descriptor {
    /// The mandatory first entry: loading a null selector has to fault rather than work.
    pub const fn null() -> Self {
        Self(0)
    }

    /// Packs a base, a limit, an access byte and a flags nibble into the layout the CPU
    /// reads: limit 0..16, base 0..16, base 16..24, access, flags and limit 16..20, base
    /// 24..32 (Intel SDM 3A, 3.4.5).
    pub const fn new(base: u32, limit: u32, access: u8, flags: u8) -> Self {
        let limit_low = (limit & 0xffff) as u64;
        let base_low = (base & 0xffff) as u64;
        let base_mid = ((base >> 16) & 0xff) as u64;
        let limit_high = ((limit >> 16) & 0x0f) as u64;
        let base_high = ((base >> 24) & 0xff) as u64;
        Self(
            limit_low
                | base_low << 16
                | base_mid << 32
                | (access as u64) << 40
                | (limit_high | (flags as u64) << 4) << 48
                | base_high << 56,
        )
    }

    /// The descriptor as the CPU sees it.
    pub const fn raw(self) -> u64 {
        self.0
    }
}

/// The 64-bit task state segment (Intel SDM 3A, 7.2.3), all 104 bytes of it.
///
/// `packed` is not decoration: the documented layout puts 64-bit stack pointers at offset 4,
/// so the natural alignment `repr(C)` would insert is exactly what makes the structure the
/// wrong size. It is only ever written through raw pointers with unaligned stores.
#[repr(C, packed)]
pub struct TaskStateSegment {
    reserved_0: u32,
    /// `RSP0..RSP2`, the stacks the CPU loads on entry to ring 0..2. Unused until M8: the
    /// kernel never runs ring 3 code yet.
    privilege_stack_table: [u64; 3],
    reserved_1: u64,
    /// `IST1..IST7`, stacks for handlers that must not run on a stack that may be broken.
    interrupt_stack_table: [u64; 7],
    reserved_2: u64,
    reserved_3: u16,
    /// Offset of the I/O permission bitmap, or the segment limit for "no bitmap at all".
    iomap_base: u16,
}

impl TaskStateSegment {
    /// Every stack unset. `iomap_base` past the limit denies ring 3 every port, which is the
    /// only answer available before there is ring 3.
    pub const fn new() -> Self {
        Self {
            reserved_0: 0,
            privilege_stack_table: [0; 3],
            reserved_1: 0,
            interrupt_stack_table: [0; 7],
            reserved_2: 0,
            reserved_3: 0,
            iomap_base: size_of::<Self>() as u16,
        }
    }
}

impl Default for TaskStateSegment {
    fn default() -> Self {
        Self::new()
    }
}

/// The 16 bytes a 64-bit TSS descriptor occupies: an ordinary descriptor whose base holds the
/// low 32 bits, followed by the high 32 bits of the base.
///
/// The base has to be published in full because the TSS lives wherever the bootloader put the
/// kernel, which is not necessarily below 4 GiB (ADR-022).
pub const fn tss_descriptor(base: u64, limit: u32) -> (u64, u64) {
    (
        Descriptor::new(base as u32, limit, TSS_ACCESS, 0).raw(),
        base >> 32,
    )
}

/// The upper limit of a TSS descriptor: the size of the segment in bytes, minus one.
pub const fn tss_limit() -> u32 {
    size_of::<TaskStateSegment>() as u32 - 1
}

#[cfg(target_arch = "x86_64")]
mod arch {
    use super::{
        tss_descriptor, tss_limit, Descriptor, TaskStateSegment, KERNEL_CODE, KERNEL_DATA,
        STACK_BYTES, TSS,
    };
    use core::arch::asm;
    use core::mem::size_of;
    use core::ptr::addr_of_mut;

    /// Stack the `#DF` handler runs on, so a fault inside a fault handler has somewhere to go.
    #[repr(C, align(16))]
    struct Stack([u8; STACK_BYTES]);

    static mut DOUBLE_FAULT_STACK: Stack = Stack([0; STACK_BYTES]);

    /// Null, code, data, and the two slots the TSS descriptor needs.
    static mut TABLE: [u64; 5] = [0; 5];

    static mut TSS_STATE: TaskStateSegment = TaskStateSegment::new();

    /// What `lgdt` is handed: a limit and a base, in that order, packed.
    #[repr(C, packed)]
    struct TablePointer {
        limit: u16,
        base: u64,
    }

    /// Loads the table this module builds and points the CPU at it.
    ///
    /// Must run once, before [`crate::idt::init`]: every gate in the IDT names
    /// [`KERNEL_CODE`], which does not exist until this does.
    pub fn init() {
        // SAFETY: the three statics are written once during boot, before interrupts are
        // enabled, and nothing else in the kernel refers to them. Writing through raw
        // pointers avoids forming references to `static mut`.
        unsafe {
            let table = &raw mut TABLE;
            (*table)[0] = Descriptor::null().raw();
            (*table)[1] = Descriptor::new(0, 0xfffff, super::CODE_ACCESS, super::CODE_FLAGS).raw();
            (*table)[2] = Descriptor::new(0, 0xfffff, super::DATA_ACCESS, super::DATA_FLAGS).raw();

            // The TSS has to be complete before `ltr` publishes it: an IST entry that is
            // still zero would send a double fault to address zero instead of to a stack.
            let tss = &raw mut TSS_STATE;
            let stack_top = (&raw mut DOUBLE_FAULT_STACK as *mut u8).add(size_of::<Stack>()) as u64;
            addr_of_mut!((*tss).interrupt_stack_table[0]).write_unaligned(stack_top);

            let (low, high) = tss_descriptor(tss as u64, tss_limit());
            (*table)[3] = low;
            (*table)[4] = high;

            let pointer = TablePointer {
                limit: (size_of::<[u64; 5]>() - 1) as u16,
                base: table as u64,
            };
            // SAFETY: `pointer` describes `TABLE`, which stays at that address for the rest
            // of the run, and `limit` is its size in bytes minus one, as `lgdt` expects.
            asm!(
                "lgdt [{}]",
                in(reg) &pointer,
                options(readonly, nostack, preserves_flags)
            );

            // Reloading the segment registers is part of loading a table: each one still
            // caches the descriptor it was loaded with, and that descriptor belongs to the
            // old table. `CS` cannot be written directly, so the far return below does it.
            // SAFETY: pushes the new code selector and the address of the label after the
            // `push`es, then returns through both pairs, which is the documented way to
            // reload `CS` on x86_64.
            asm!(
                "push {code}",
                "lea {tmp}, [rip + 2f]",
                "push {tmp}",
                "retfq",
                "2:",
                code = in(reg) u64::from(KERNEL_CODE),
                tmp = lateout(reg) _,
            );
            // SAFETY: `DS`..`GS` take the data selector built above. None of them has to be
            // a far pointer in 64-bit mode, so a plain move reloads the cached descriptor.
            asm!(
                "mov ds, {0:x}",
                "mov es, {0:x}",
                "mov ss, {0:x}",
                "mov fs, {0:x}",
                "mov gs, {0:x}",
                in(reg) u64::from(KERNEL_DATA),
                options(nostack, preserves_flags),
            );
            // SAFETY: the TSS descriptor is in the table that is already loaded, so the CPU
            // can read it; `ltr` also marks the segment busy, which is why it runs once.
            asm!(
                "ltr {0:x}",
                in(reg) u64::from(TSS),
                options(nostack, preserves_flags)
            );
        }
    }
}

#[cfg(target_arch = "x86_64")]
pub use arch::init;

#[cfg(test)]
mod tests {
    use super::*;
    use core::mem::offset_of;

    #[test]
    fn the_null_descriptor_is_all_zeroes() {
        assert_eq!(Descriptor::null().raw(), 0);
    }

    #[test]
    fn the_code_and_data_descriptors_match_their_documented_encoding() {
        // `0x00af9a000000ffff`: 4 KiB-granular limit, present, ring 0, code, long mode.
        let code = Descriptor::new(0, 0xfffff, CODE_ACCESS, CODE_FLAGS);
        assert_eq!(code.raw(), 0x00af_9a00_0000_ffff);
        let data = Descriptor::new(0, 0xfffff, DATA_ACCESS, DATA_FLAGS);
        assert_eq!(data.raw(), 0x00cf_9200_0000_ffff);
    }

    #[test]
    fn base_and_limit_are_split_across_the_descriptor() {
        let base = 0x1234_5678;
        let limit = 0xabcdef;
        let raw = Descriptor::new(base, limit, CODE_ACCESS, CODE_FLAGS).raw();
        assert_eq!(raw & 0xffff, u64::from(limit as u16));
        assert_eq!((raw >> 16) & 0xffff, u64::from(base as u16));
        assert_eq!((raw >> 32) & 0xff, u64::from((base >> 16) as u8));
        assert_eq!((raw >> 48) & 0xf, u64::from((limit >> 16) as u8 & 0xf));
        assert_eq!((raw >> 56) & 0xff, u64::from((base >> 24) as u8));
    }

    #[test]
    fn the_tss_descriptor_publishes_a_base_above_four_gib() {
        // The kernel is linked where the bootloader puts it, and that address has been above
        // 4 GiB in practice; the upper word is what stops the CPU reading a truncated base.
        let base = 0x0000_0280_0178_0000_u64;
        let (low, high) = tss_descriptor(base, tss_limit());
        assert_eq!(high, 0x280);
        assert_eq!((low >> 16) & 0xffff, 0);
        assert_eq!((low >> 32) & 0xff, 0x78);
        assert_eq!((low >> 56) & 0xff, 0x01);
        assert_eq!((low >> 40) & 0xff, u64::from(TSS_ACCESS));
    }

    #[test]
    fn the_tss_is_the_size_and_shape_the_cpu_expects() {
        assert_eq!(size_of::<TaskStateSegment>(), 104);
        assert_eq!(tss_limit(), 103);
        // The stack tables sit where the SDM puts them; `packed` is what keeps them there.
        assert_eq!(offset_of!(TaskStateSegment, privilege_stack_table), 4);
        assert_eq!(offset_of!(TaskStateSegment, interrupt_stack_table), 36);
        assert_eq!(offset_of!(TaskStateSegment, iomap_base), 102);
    }

    #[test]
    fn an_unused_tss_denies_every_io_port() {
        let empty = TaskStateSegment::new();
        // Copied out of the packed field rather than referenced, so no unaligned borrow.
        let iomap = empty.iomap_base;
        assert_eq!(usize::from(iomap), size_of::<TaskStateSegment>());
    }
}
