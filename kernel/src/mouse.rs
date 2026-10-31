//! The pointer: a three-byte packet parser, and the 8042's second port.
//!
//! A PS/2 mouse reports in packets of three bytes: a flag byte with the button bits, the
//! sign bits and an overflow bit per axis, then the two movements as magnitudes. Nothing
//! marks a packet boundary, so a reader that starts mid-packet sees whatever the previous
//! packet left. Bit 3 of the first byte is always set, which is the only synchronisation
//! the protocol offers - so a first byte without it is dropped, and that is the one rule
//! [`Parser`] exists for.
//!
//! [`Mouse`] is the part with a screen in it: y grows upwards in the packet and downwards
//! on the panel, and the position is clamped to the panel rather than left to wrap. Both
//! are plain arithmetic, so both are tested on the host.

/// One decoded movement.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Packet {
    /// Horizontal movement in mouse units, positive to the right.
    pub dx: i32,
    /// Vertical movement in mouse units, positive upwards.
    pub dy: i32,
    /// The low three bits: left, right, middle.
    pub buttons: u8,
}

/// The three-byte assembler.
#[derive(Clone, Copy, Default)]
pub struct Parser {
    buffer: [u8; 3],
    filled: usize,
}

impl Parser {
    /// A parser that has not seen byte one of a packet.
    pub const fn new() -> Self {
        Self {
            buffer: [0; 3],
            filled: 0,
        }
    }

    /// Feeds one byte and returns a packet if that byte completed one.
    ///
    /// A dropped packet - overflow, or a first byte with the wrong bit - is dropped whole:
    /// half a movement is worse than none, because the position would then be wrong
    /// without anything saying so.
    pub fn feed(&mut self, byte: u8) -> Option<Packet> {
        if self.filled == 0 {
            // Bit 3 is always set on the first byte of a packet. Anything else means the
            // reader and the device are out of step, which is what a lost byte leaves.
            if byte & 0x08 == 0 {
                return None;
            }
        }
        self.buffer[self.filled] = byte;
        self.filled += 1;
        if self.filled < 3 {
            return None;
        }
        self.filled = 0;
        let [flags, x, y] = self.buffer;
        // Bits 6 and 7 are overflow on x and y: the movement did not fit in nine bits.
        if flags & 0xc0 != 0 {
            return None;
        }
        Some(Packet {
            dx: sign_extend(x, flags & 0x10 != 0),
            dy: sign_extend(y, flags & 0x20 != 0),
            buttons: flags & 0x07,
        })
    }
}

/// A magnitude plus the packet's sign bit, as a signed movement.
fn sign_extend(byte: u8, negative: bool) -> i32 {
    if negative {
        byte as i32 - 256
    } else {
        byte as i32
    }
}

/// The pointer's position on the panel.
#[derive(Clone, Copy)]
pub struct Mouse {
    x: i32,
    y: i32,
    buttons: u8,
}

impl Mouse {
    /// A pointer at `(x, y)`, which is where the boot puts it: the middle of the screen.
    pub const fn new(x: i32, y: i32) -> Self {
        Self { x, y, buttons: 0 }
    }

    /// Applies one packet, clamped to a panel of `width` by `height`, and returns the new
    /// position.
    ///
    /// The y flip is here rather than in the caller because it is a property of the
    /// protocol, not of the screen: every PS/2 mouse reports up as positive. Clamping is
    /// here for the same reason - a pointer off the edge of the panel is a pointer nobody
    /// can see, and a signed wrap would put it on the opposite edge.
    pub fn apply(&mut self, packet: Packet, width: u32, height: u32) -> (i32, i32) {
        let max_x = width.saturating_sub(1) as i32;
        let max_y = height.saturating_sub(1) as i32;
        self.x = (self.x + packet.dx).clamp(0, max_x);
        self.y = (self.y - packet.dy).clamp(0, max_y);
        self.buttons = packet.buttons;
        (self.x, self.y)
    }

    /// Where the pointer is.
    pub const fn position(&self) -> (i32, i32) {
        (self.x, self.y)
    }

    /// Which buttons were held on the last packet: bit 0 left, bit 1 right, bit 2 middle.
    pub const fn buttons(&self) -> u8 {
        self.buttons
    }
}

/// Why [`init`] could not bring the pointer up.
///
/// Named steps rather than one `false`, because "the second port refused" and "the mouse
/// never acknowledged" are different machines: the first is a firmware setting, the second
/// is an absent device or a clock that stayed off.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MouseError {
    /// The controller never accepted the command that enables the second port.
    Port,
    /// The controller never handed back its configuration byte.
    Config,
    /// The controller never accepted the rewritten configuration.
    WriteConfig,
    /// The command byte never reached the mouse.
    Command,
    /// The mouse never answered; carries what the port did say, or `0x100` for silence.
    Ack(u32),
}

/// The 8042's second port, its interrupt, and the position it moves.
#[cfg(target_arch = "x86_64")]
mod device {
    use core::sync::atomic::{AtomicI32, AtomicU32, AtomicU64, Ordering};

    use super::{Mouse, Parser};
    use crate::idt::{self, InterruptFrame};
    use crate::pic;
    use crate::port::{inb, outb};
    use crate::sync::InterruptMutex;

    /// The same two ports the keyboard uses: one controller, two ports.
    const DATA: u16 = 0x60;
    const STATUS_COMMAND: u16 = 0x64;

    const OUTPUT_FULL: u8 = 1 << 0;
    const INPUT_FULL: u8 = 1 << 1;
    /// Set on a status byte that describes data from the second port.
    const AUX_DATA: u8 = 1 << 5;

    const READ_CONFIG: u8 = 0x20;
    const WRITE_CONFIG: u8 = 0x60;
    const ENABLE_SECOND: u8 = 0xa8;
    /// "The next data byte is for the second port", which is how a mouse command is sent.
    const WRITE_SECOND: u8 = 0xd4;
    const SET_DEFAULTS: u8 = 0xf6;
    const ENABLE_REPORTING: u8 = 0xf4;
    const ACK: u8 = 0xfa;

    const CONFIG_SECOND_IRQ: u8 = 1 << 1;
    const CONFIG_SECOND_CLOCK_OFF: u8 = 1 << 5;

    /// The line the mouse is on, and the cascade the slave's lines travel through.
    const IRQ: u8 = 12;
    const CASCADE: u8 = pic::CASCADE;
    /// The keyboard's line, masked while the controller is being talked to.
    const KEYBOARD_IRQ: u8 = 1;

    const POLLS: u32 = 100_000;

    /// The value [`last_ack`] reports when nothing came back at all.
    const NO_ACK: u32 = 0x100;

    /// `PARSER` and `MOUSE` are written by the IRQ handler and read through the getters
    /// below, which each read one `Copy` value; the handler is the only writer (ADR-038).
    static PARSER: InterruptMutex<Parser> = InterruptMutex::new(Parser::new());
    static MOUSE: InterruptMutex<Mouse> = InterruptMutex::new(Mouse::new(0, 0));
    static WIDTH: AtomicU32 = AtomicU32::new(0);
    static HEIGHT: AtomicU32 = AtomicU32::new(0);
    static PACKETS: AtomicU64 = AtomicU64::new(0);
    static LAST_DX: AtomicI32 = AtomicI32::new(0);
    static LAST_DY: AtomicI32 = AtomicI32::new(0);
    /// What the second port answered the last command: the byte, or [`NO_ACK`].
    static LAST_ACK: AtomicU32 = AtomicU32::new(NO_ACK);

    /// Reads one byte from the controller.
    ///
    /// # Safety
    ///
    /// The caller must own the 8042's ports.
    unsafe fn read_data() -> Option<u8> {
        for _ in 0..POLLS {
            // SAFETY: the status port is read-only.
            if unsafe { inb(STATUS_COMMAND) } & OUTPUT_FULL != 0 {
                // SAFETY: the output buffer is full.
                return Some(unsafe { inb(DATA) });
            }
        }
        None
    }

    /// Sends a command byte.
    ///
    /// # Safety
    ///
    /// As [`read_data`].
    unsafe fn write_command(value: u8) -> bool {
        for _ in 0..POLLS {
            // SAFETY: the status port is read-only.
            if unsafe { inb(STATUS_COMMAND) } & INPUT_FULL == 0 {
                // SAFETY: the controller's input buffer is empty.
                unsafe { outb(STATUS_COMMAND, value) };
                return true;
            }
        }
        false
    }

    /// Sends a data byte.
    ///
    /// # Safety
    ///
    /// As [`read_data`].
    unsafe fn write_data(value: u8) -> bool {
        for _ in 0..POLLS {
            // SAFETY: the status port is read-only.
            if unsafe { inb(STATUS_COMMAND) } & INPUT_FULL == 0 {
                // SAFETY: the controller's input buffer is empty.
                unsafe { outb(DATA, value) };
                return true;
            }
        }
        false
    }

    /// Sends one command to the mouse and waits for its acknowledgement.
    ///
    /// # Safety
    ///
    /// As [`read_data`], and the second port must have been enabled.
    unsafe fn send_to_mouse(command: u8) -> bool {
        // SAFETY: the caller owns the controller.
        unsafe {
            if !write_command(WRITE_SECOND) || !write_data(command) {
                return false;
            }
            // The mouse answers `0xFA`. Anything else in the buffer is a movement packet
            // that arrived while the command was in flight; it is read, kept for the
            // report, and the wait continues.
            for _ in 0..POLLS {
                match read_data() {
                    Some(ACK) => {
                        LAST_ACK.store(u32::from(ACK), Ordering::Relaxed);
                        return true;
                    }
                    Some(other) => LAST_ACK.store(u32::from(other), Ordering::Relaxed),
                    None => {
                        LAST_ACK.store(NO_ACK, Ordering::Relaxed);
                        return false;
                    }
                }
            }
            LAST_ACK.store(NO_ACK, Ordering::Relaxed);
            false
        }
    }

    /// Enables the second port, sets the mouse reporting, and starts IRQ12.
    ///
    /// `width` and `height` are the panel's size, which is what the pointer is clamped to.
    /// Returns whether the mouse answered; a machine with no pointer is a machine the
    /// keyboard still works on.
    ///
    /// # Safety
    ///
    /// Must run after [`crate::keyboard::init`], which owns the configuration byte this
    /// function reads and rewrites, and with interrupts disabled.
    pub unsafe fn init(width: u32, height: u32) -> Result<(), super::MouseError> {
        WIDTH.store(width, Ordering::Relaxed);
        HEIGHT.store(height, Ordering::Relaxed);
        // Start in the middle: a pointer at the origin sits under the boot log's first line.
        // SAFETY: nothing has read the mouse yet, and `apply` is the only writer after this.
        unsafe {
            *MOUSE.as_ptr() = Mouse::new((width / 2) as i32, (height / 2) as i32);
        }
        // SAFETY: the caller promised this runs after the keyboard's own setup.
        unsafe {
            // The controller answers its own commands in the same output buffer a keystroke
            // arrives in, and the keyboard's handler would read those answers as scancodes.
            // Masking IRQ1 for the length of this conversation is what keeps the two apart;
            // unmasking it again afterwards is what resumes typing.
            pic::mask(KEYBOARD_IRQ);
            let result = handshake();
            pic::unmask(KEYBOARD_IRQ);
            result?;
            idt::set_irq(IRQ, interrupt);
        }
        // The slave's line only reaches the CPU through the cascade on the master, so both
        // have to be unmasked. A driver that forgot the cascade would configure a mouse
        // that never moves, and every register it could check would look correct.
        pic::unmask(CASCADE);
        pic::unmask(IRQ);
        Ok(())
    }

    /// Enables the second port and tells the mouse to start reporting.
    ///
    /// # Safety
    ///
    /// As [`init`], and IRQ1 must be masked: the bytes this reads are the controller's
    /// answers, not scancodes.
    unsafe fn handshake() -> Result<(), super::MouseError> {
        use super::MouseError;
        // SAFETY: the caller owns the controller and has masked the keyboard's line.
        unsafe {
            if !write_command(ENABLE_SECOND) {
                return Err(MouseError::Port);
            }
            if !write_command(READ_CONFIG) {
                return Err(MouseError::Config);
            }
            let Some(config) = read_data() else {
                return Err(MouseError::Config);
            };
            // The second port's interrupt on and its clock on; the keyboard's bits as the
            // keyboard's `init` left them.
            let wanted = (config | CONFIG_SECOND_IRQ) & !CONFIG_SECOND_CLOCK_OFF;
            if !write_command(WRITE_CONFIG) || !write_data(wanted) {
                return Err(MouseError::WriteConfig);
            }
            if !send_to_mouse(SET_DEFAULTS) {
                let said = LAST_ACK.load(Ordering::Relaxed);
                return Err(if said == NO_ACK {
                    MouseError::Command
                } else {
                    MouseError::Ack(said)
                });
            }
            if !send_to_mouse(ENABLE_REPORTING) {
                let said = LAST_ACK.load(Ordering::Relaxed);
                return Err(if said == NO_ACK {
                    MouseError::Command
                } else {
                    MouseError::Ack(said)
                });
            }
        }
        Ok(())
    }

    /// The IRQ12 handler: one byte in, and a packet every third byte.
    ///
    /// The status byte is checked before the data byte is read: the two ports share the
    /// data register, and a byte that belongs to the keyboard has to be left for the
    /// keyboard's handler.
    extern "x86-interrupt" fn interrupt(_frame: InterruptFrame) {
        // SAFETY: the status port is read-only.
        let status = unsafe { inb(STATUS_COMMAND) };
        if status & OUTPUT_FULL != 0 && status & AUX_DATA != 0 {
            // SAFETY: the buffer holds a byte from the second port.
            let byte = unsafe { inb(DATA) };
            // SAFETY: the handler is the only writer of the parser, and this borrow ends
            // with the block.
            let packet = unsafe { (*PARSER.as_ptr()).feed(byte) };
            if let Some(packet) = packet {
                let width = WIDTH.load(Ordering::Relaxed);
                let height = HEIGHT.load(Ordering::Relaxed);
                // SAFETY: as above, for the position.
                unsafe { (*MOUSE.as_ptr()).apply(packet, width, height) };
                LAST_DX.store(packet.dx, Ordering::Relaxed);
                LAST_DY.store(packet.dy, Ordering::Relaxed);
                PACKETS.fetch_add(1, Ordering::Relaxed);
            }
        }
        pic::eoi(IRQ);
    }

    /// Where the pointer is now.
    pub fn position() -> (i32, i32) {
        // SAFETY: reading one `Copy` value the handler may also be writing; a torn read is
        // one coordinate, and the next packet corrects it.
        unsafe { (*MOUSE.as_ptr()).position() }
    }

    /// Which buttons the last packet reported.
    pub fn buttons() -> u8 {
        // SAFETY: as `position`.
        unsafe { (*MOUSE.as_ptr()).buttons() }
    }

    /// How many packets have been decoded.
    pub fn packets() -> u64 {
        PACKETS.load(Ordering::Relaxed)
    }

    /// The movement of the most recent packet.
    pub fn last_movement() -> (i32, i32) {
        (
            LAST_DX.load(Ordering::Relaxed),
            LAST_DY.load(Ordering::Relaxed),
        )
    }

    /// What the second port last answered: the byte, or `0x100` for silence.
    ///
    /// Only a diagnosis. A machine with no mouse is a machine the keyboard still works on,
    /// but the boot log has to distinguish "the port answered `0xfe`" - a command it did
    /// not accept - from "nothing is attached".
    pub fn last_ack() -> u32 {
        LAST_ACK.load(Ordering::Relaxed)
    }
}

#[cfg(target_arch = "x86_64")]
pub use device::{buttons, init, last_ack, last_movement, packets, position};

#[cfg(test)]
mod tests {
    use super::*;

    /// Feeds three bytes to a fresh parser.
    fn packet(flags: u8, x: u8, y: u8) -> Option<Packet> {
        let mut parser = Parser::new();
        assert_eq!(parser.feed(flags), None);
        assert_eq!(parser.feed(x), None);
        parser.feed(y)
    }

    #[test]
    fn three_bytes_are_one_packet_and_the_flag_byte_carries_the_signs() {
        assert_eq!(
            packet(0x08, 0x05, 0x03),
            Some(Packet {
                dx: 5,
                dy: 3,
                buttons: 0
            })
        );
        // X negative: the byte is the movement's nine-bit two's complement, so `0xff`
        // with the sign bit set is one pixel left, not 255.
        assert_eq!(
            packet(0x18, 0xff, 0x02).map(|packet| (packet.dx, packet.dy)),
            Some((-1, 2))
        );
        // Y negative.
        assert_eq!(
            packet(0x28, 0x02, 0xff).map(|packet| (packet.dx, packet.dy)),
            Some((2, -1))
        );
        // Both negative.
        assert_eq!(
            packet(0x38, 0xff, 0xfe).map(|packet| (packet.dx, packet.dy)),
            Some((-1, -2))
        );
    }

    #[test]
    fn a_first_byte_without_bit_three_is_dropped_until_one_arrives() {
        let mut parser = Parser::new();
        // Out of step: data bytes where a flag byte should be.
        assert_eq!(parser.feed(0x01), None);
        assert_eq!(parser.feed(0x02), None);
        // A real flag byte, then the rest of that packet.
        assert_eq!(parser.feed(0x08), None);
        assert_eq!(parser.feed(0x03), None);
        assert_eq!(parser.feed(0x04).map(|packet| packet.dx), Some(3));
    }

    #[test]
    fn an_overflow_bit_drops_the_whole_packet() {
        let mut parser = Parser::new();
        assert_eq!(parser.feed(0x48), None);
        assert_eq!(parser.feed(0x7f), None);
        assert_eq!(parser.feed(0x7f), None);
        // The parser is back at a packet boundary, so the next three bytes are a packet.
        assert_eq!(parser.feed(0x08), None);
        assert_eq!(parser.feed(0x00), None);
        assert_eq!(parser.feed(0x00).map(|packet| packet.dx), Some(0));
    }

    #[test]
    fn the_button_bits_are_the_low_three_of_the_flag_byte() {
        assert_eq!(
            packet(0x0f, 0x00, 0x00).map(|packet| packet.buttons),
            Some(7)
        );
        assert_eq!(
            packet(0x08, 0x00, 0x00).map(|packet| packet.buttons),
            Some(0)
        );
    }

    #[test]
    fn the_pointer_moves_and_the_y_axis_is_inverted() {
        let mut mouse = Mouse::new(100, 100);
        let move_by = |dx, dy, buttons| Packet { dx, dy, buttons };
        // Right and up: x grows, screen y shrinks.
        assert_eq!(mouse.apply(move_by(10, 5, 0), 800, 600), (110, 95));
        // Left and down.
        assert_eq!(mouse.apply(move_by(-20, -5, 1), 800, 600), (90, 100));
        assert_eq!(mouse.buttons(), 1);
        assert_eq!(mouse.position(), (90, 100));
    }

    #[test]
    fn the_pointer_stops_at_the_edges_rather_than_wrapping() {
        let mut mouse = Mouse::new(10, 10);
        let move_by = |dx, dy| Packet { dx, dy, buttons: 0 };
        // Far past the left edge: clamped to zero, not wrapped to the right.
        assert_eq!(mouse.apply(move_by(-1000, 0), 800, 600), (0, 10));
        // Far past the right and top edges.
        assert_eq!(mouse.apply(move_by(5000, 5000), 800, 600), (799, 0));
        // Far past the bottom.
        assert_eq!(mouse.apply(move_by(0, -5000), 800, 600), (799, 599));
        // A panel with no pixels at all does not produce a negative clamp bound.
        assert_eq!(mouse.apply(move_by(-10, 10), 0, 0), (0, 0));
    }
}
