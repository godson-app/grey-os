//! The two 8259 programmable interrupt controllers.
//!
//! They are programmed for one reason: on reset they deliver IRQ0..15 on vectors 0x08..=0x0F,
//! which is inside the exception range, so a timer tick would arrive looking like a double
//! fault. Remapping to 0x20..=0x2F puts them where an operating system is expected to put
//! them, above every exception this kernel defines (ADR-028).
//!
//! Until a driver asks for its line, every line is masked: an interrupt with no handler is
//! not a thing this kernel can do anything useful with yet.

/// Vector IRQ0 arrives on. The slave's IRQs start eight vectors higher.
pub const VECTOR_BASE: u8 = 0x20;

/// IRQ line the slave is cascaded through on the master.
pub const CASCADE: u8 = 2;

/// The PIT's line, which [`crate::timer`] claims.
pub const TIMER: u8 = 0;

/// Master command port, and its interrupt mask register.
#[cfg(target_arch = "x86_64")]
const MASTER: u16 = 0x20;
const MASTER_DATA: u16 = 0x21;

/// Slave command port, and its interrupt mask register.
#[cfg(target_arch = "x86_64")]
const SLAVE: u16 = 0xa0;
const SLAVE_DATA: u16 = 0xa1;

/// The port a dummy write goes to when a moment has to pass.
#[cfg(target_arch = "x86_64")]
const IO_WAIT: u16 = 0x80;

/// ICW1: begin initialisation, and ICW4 follows.
#[cfg(target_arch = "x86_64")]
const ICW1_INIT: u8 = 0x11;

/// ICW4: 8086/88 mode, normal end of interrupt.
#[cfg(target_arch = "x86_64")]
const ICW4_8086: u8 = 0x01;

/// End of interrupt, written to a command port.
#[cfg(target_arch = "x86_64")]
const END_OF_INTERRUPT: u8 = 0x20;

/// The IDT vector an IRQ line arrives on.
pub const fn vector(irq: u8) -> u8 {
    VECTOR_BASE + irq
}

/// The mask register and bit that control `irq`.
pub const fn line(irq: u8) -> (u16, u8) {
    if irq < 8 {
        (MASTER_DATA, irq)
    } else {
        (SLAVE_DATA, irq - 8)
    }
}

#[cfg(target_arch = "x86_64")]
mod arch {
    use super::{
        line, CASCADE, END_OF_INTERRUPT, ICW1_INIT, ICW4_8086, IO_WAIT, MASTER, MASTER_DATA, SLAVE,
        SLAVE_DATA, VECTOR_BASE,
    };
    use core::arch::asm;

    /// Writes one byte to an I/O port.
    fn outb(port: u16, value: u8) {
        // SAFETY: every caller passes a port in the 8259's block or the POST port 0x80, all
        // of which are I/O ports this kernel may write. Writing the wrong byte to the mask
        // register masks the wrong line, which is a bug but not an unsafe one.
        unsafe {
            asm!(
                "out dx, al",
                in("dx") port,
                in("al") value,
                options(nomem, nostack, preserves_flags)
            )
        }
    }

    /// Reads one byte from an I/O port.
    fn inb(port: u16) -> u8 {
        let value: u8;
        // SAFETY: as `outb`: the callers only read the two mask registers.
        unsafe {
            asm!(
                "in al, dx",
                in("dx") port,
                out("al") value,
                options(nomem, nostack, preserves_flags)
            )
        }
        value
    }

    /// Gives the older 8259s time to notice a command. A write to the POST port has the same
    /// latency as the `jmp $+2` the manuals use, and needs no label.
    fn io_wait() {
        outb(IO_WAIT, 0);
    }

    /// Remaps both controllers and masks every line.
    ///
    /// Must run before [`crate::idt::enable`], and after [`crate::idt::init`] has installed a
    /// stub on every IRQ vector.
    pub fn init() {
        outb(MASTER, ICW1_INIT);
        io_wait();
        outb(SLAVE, ICW1_INIT);
        io_wait();
        // ICW2: the vector each controller's first line arrives on.
        outb(MASTER_DATA, VECTOR_BASE);
        io_wait();
        outb(SLAVE_DATA, VECTOR_BASE + 8);
        io_wait();
        // ICW3: the slave is cascaded through `CASCADE` on the master, and is line `CASCADE`
        // as far as the slave itself is concerned.
        outb(MASTER_DATA, 1 << CASCADE);
        io_wait();
        outb(SLAVE_DATA, CASCADE);
        io_wait();
        // ICW4: 8086 mode.
        outb(MASTER_DATA, ICW4_8086);
        io_wait();
        outb(SLAVE_DATA, ICW4_8086);
        io_wait();
        // Mask everything. A line is unmasked by the driver that claims it.
        outb(MASTER_DATA, 0xff);
        outb(SLAVE_DATA, 0xff);
    }

    /// Lets IRQ `irq` reach the CPU.
    pub fn unmask(irq: u8) {
        let (port, bit) = line(irq);
        outb(port, inb(port) & !(1 << bit));
    }

    /// Stops IRQ `irq` reaching the CPU.
    ///
    /// Needed for more than tidiness: the 8042 puts the answers to its own commands in the
    /// same output buffer a keystroke arrives in, so a driver sending it commands has to
    /// stop the keyboard's handler from consuming them as scancodes.
    pub fn mask(irq: u8) {
        let (port, bit) = line(irq);
        outb(port, inb(port) | (1 << bit));
    }

    /// Acknowledges `irq`, so the controller will deliver the next one.
    ///
    /// A slave line has to be acknowledged twice: once in the slave, then in the master,
    /// because the cascade line on the master counts as in service.
    pub fn eoi(irq: u8) {
        if irq >= 8 {
            outb(SLAVE, END_OF_INTERRUPT);
        }
        outb(MASTER, END_OF_INTERRUPT);
    }

    /// Whether `irq` is currently allowed through, for the boot log.
    pub fn is_unmasked(irq: u8) -> bool {
        let (port, bit) = line(irq);
        inb(port) & (1 << bit) == 0
    }
}

#[cfg(target_arch = "x86_64")]
pub use arch::{eoi, init, is_unmasked, mask, unmask};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn irqs_land_above_every_exception_vector() {
        // The whole point of the remap: on reset the 8259s deliver 0x08..=0x0f, which would
        // arrive as a double fault and a spare vector. Nothing in 0x20..=0x2f may collide
        // with an exception this kernel handles.
        let last_exception = crate::idt::EXCEPTIONS
            .last()
            .expect("the exception table is not empty")
            .vector;
        for irq in 0..16 {
            assert!(
                vector(irq) > last_exception,
                "irq {irq} arrives on {:#04x}, inside the exception range",
                vector(irq)
            );
        }
        assert_eq!(vector(0), 0x20);
        assert_eq!(vector(15), 0x2f);
    }

    #[test]
    fn the_two_controllers_split_the_sixteen_lines() {
        assert_eq!(line(0), (0x21, 0));
        assert_eq!(line(7), (0x21, 7));
        assert_eq!(line(8), (0xa1, 0));
        assert_eq!(line(15), (0xa1, 7));
    }
}
