//! The programmable interval timer, and the tick counter everything else reads.
//!
//! A tick is the only clock this kernel has until M3 proves the TSC and M10 gives the APIC a
//! time source that does not run at 1.19 MHz. Channel 0 in rate-generator mode is the
//! smallest thing that makes time pass: it counts down, fires IRQ0, and reloads.
//!
//! The handler runs with interrupts disabled, on the stack of whatever it interrupted, and
//! does exactly two things - bump a counter and acknowledge the controller. It never prints
//! and never allocates: the console and the heap are single-threaded by assumption, and a
//! handler that entered them would break that assumption for the code it interrupted
//! (ADR-028).

use core::sync::atomic::{AtomicU64, Ordering};

/// The PIT's input clock, in Hz. Fixed by the original PC and by nothing else.
pub const INPUT_HZ: u32 = 1_193_182;

/// Channel 0's data port.
#[cfg(target_arch = "x86_64")]
const CHANNEL_0: u16 = 0x40;

/// Channel 0: lobyte/hibyte access, mode 2 (rate generator), binary counting.
#[cfg(target_arch = "x86_64")]
const RATE_GENERATOR: u8 = 0x34;

/// Ticks since the timer was started.
static TICKS: AtomicU64 = AtomicU64::new(0);

/// The divisor that makes the PIT tick at `hz`, or `None` if a 16-bit divisor cannot reach it.
///
/// A divisor of 65536 is encoded as zero, which is why the `u16` is a real answer and not a
/// truncation: 18 Hz is the slowest rate this chip has, and 1.19 MHz the fastest.
pub const fn divisor(hz: u32) -> Option<u16> {
    if hz == 0 || hz > INPUT_HZ {
        return None;
    }
    let divisor = (INPUT_HZ + hz / 2) / hz;
    Some(if divisor > 0xffff { 0 } else { divisor as u16 })
}

/// The frequency a divisor actually produces, rounded to the nearest hertz.
pub const fn frequency(divisor: u16) -> u32 {
    let counts = if divisor == 0 {
        0x1_0000
    } else {
        divisor as u32
    };
    (INPUT_HZ + counts / 2) / counts
}

/// How many ticks have elapsed since [`init`].
pub fn ticks() -> u64 {
    TICKS.load(Ordering::Relaxed)
}

#[cfg(target_arch = "x86_64")]
mod arch {
    use super::{divisor, frequency, Ordering, CHANNEL_0, RATE_GENERATOR, TICKS};
    use crate::idt::InterruptFrame;
    use crate::pic;
    use core::arch::asm;

    /// The mode register, which is shared by all three channels.
    const COMMAND: u16 = 0x43;

    /// Writes one byte to an I/O port.
    fn outb(port: u16, value: u8) {
        // SAFETY: callers pass the PIT's two ports; a wrong byte changes a rate, it does not
        // corrupt memory.
        unsafe {
            asm!(
                "out dx, al",
                in("dx") port,
                in("al") value,
                options(nomem, nostack, preserves_flags)
            )
        }
    }

    /// Programs channel 0 at `hz`, claims IRQ0 and starts counting.
    ///
    /// Returns the frequency the divisor actually produces, or `None` if `hz` is outside
    /// 18..=1_193_182 Hz. Interrupts still have to be enabled afterwards
    /// ([`crate::idt::enable`]) before a tick arrives.
    pub fn init(hz: u32) -> Option<u32> {
        let divisor = divisor(hz)?;
        outb(COMMAND, RATE_GENERATOR);
        // The low byte first: the channel latches it and waits for the high byte.
        outb(CHANNEL_0, divisor as u8);
        outb(CHANNEL_0, (divisor >> 8) as u8);
        pic::unmask(pic::TIMER);
        crate::idt::set_irq(pic::TIMER, tick);
        Some(frequency(divisor))
    }

    /// The IRQ0 handler: one counter, one acknowledgement, and nothing else.
    extern "x86-interrupt" fn tick(_frame: InterruptFrame) {
        TICKS.fetch_add(1, Ordering::Relaxed);
        // Sleeping tasks are woken here, in the handler, and nowhere else: a task that wants
        // the CPU again says when, and the timer is the only thing that knows the time.
        crate::scheduler::tick();
        pic::eoi(pic::TIMER);
    }

    /// Blocks until `count` more ticks have arrived.
    ///
    /// Sleeps in `hlt` between them, so the CPU idles instead of spinning. Requires
    /// interrupts to be enabled: with `IF` clear this never returns, which is the honest
    /// behaviour rather than a spin that looks like progress.
    pub fn wait_ticks(count: u64) {
        let target = TICKS.load(Ordering::Relaxed) + count;
        while TICKS.load(Ordering::Relaxed) < target {
            // SAFETY: `hlt` waits for the next interrupt. One is due: the timer is armed and
            // enabled, and the race with a tick that already happened only costs a wait.
            unsafe { asm!("hlt", options(nomem, nostack)) }
        }
    }
}

#[cfg(target_arch = "x86_64")]
pub use arch::{init, wait_ticks};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_hundred_hertz_is_a_divisor_the_chip_can_hold() {
        let divisor = divisor(100).expect("100 Hz is well inside the range");
        assert_eq!(divisor, 11_932);
        assert_eq!(frequency(divisor), 100);
    }

    #[test]
    fn the_slowest_and_fastest_rates_round_trip() {
        // 65536 is encoded as zero; the caller must not read that as "stop".
        assert_eq!(divisor(18), Some(0));
        assert_eq!(frequency(0), 18);
        assert_eq!(divisor(INPUT_HZ), Some(1));
        assert_eq!(frequency(1), INPUT_HZ);
    }

    #[test]
    fn a_rate_the_divisor_cannot_reach_is_refused() {
        assert_eq!(divisor(0), None);
        assert_eq!(divisor(INPUT_HZ + 1), None);
        assert_eq!(divisor(2_000_000), None);
    }

    #[test]
    fn every_rate_it_accepts_is_within_a_hertz_of_the_request() {
        // Up to 1 kHz the divisor is large enough that rounding the divisor costs less than a
        // hertz. Above that the step between divisors is wider than the request, which is why
        // the boot log reports the frequency that was programmed and not the one asked for.
        for hz in [18, 50, 60, 100, 250, 1000] {
            let divisor = divisor(hz).expect("inside the range");
            let actual = frequency(divisor);
            assert!(
                actual.abs_diff(hz) <= 1,
                "{hz} Hz became {actual} Hz with divisor {divisor}"
            );
        }
    }

    #[test]
    fn a_high_rate_is_a_quarter_of_a_percent_faithful() {
        let divisor = divisor(10_000).expect("inside the range");
        let actual = frequency(divisor);
        assert!(
            actual >= 9_900 && actual <= 10_100,
            "10 kHz became {actual} Hz"
        );
    }
}
