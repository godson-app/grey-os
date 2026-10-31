//! The keyboard: a PS/2 decoder that can be tested, and the 8042 that feeds it.
//!
//! # Two halves, split where the tests are
//!
//! [`Decoder`] turns bytes into [`Event`]s. It is the part where the interesting mistakes
//! live - a modifier that latches on its release, caps lock that behaves like shift, the
//! `$` that sits where the `4` is, the arrow keys that arrive behind a prefix byte - so it
//! is plain Rust with no port and no interrupt, and the host tests drive it byte by byte.
//!
//! The device half is the 8042 controller: enable the first port, leave translation on so
//! the set the decoder understands is the set the controller hands over, claim IRQ1, and
//! push what arrives into a fixed queue. Nothing in that half allocates, because it runs
//! in an interrupt handler.
//!
//! # The queue
//!
//! One producer (the IRQ handler) and one consumer (whatever reads a key next), over a
//! fixed array. The producer never blocks: a full queue drops the newest event and counts
//! it, because a keystroke that arrives while the reader is busy is already late. The
//! consumer masks interrupts while it moves the read index, which is what makes the queue
//! safe without a lock: with `IF` clear the handler cannot run, and the handler is the only
//! other writer.

/// What a key press means once the scancode has been decoded.
///
/// Named keys and characters are one enum because a shell does not care where a key sits
/// on the board: it wants `Enter` or `'a'`. Modifiers are not keys here - they are state
/// on the event, because `shift-a` and `A` are the same keystroke to a caller.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Key {
    /// A printable character, already shifted and caps-adjusted.
    Char(char),
    /// Return.
    Enter,
    /// Backspace.
    Backspace,
    /// Tab.
    Tab,
    /// Escape.
    Escape,
    /// The arrow keys.
    Up,
    Down,
    Left,
    Right,
    /// Home, end, and the page keys.
    Home,
    End,
    PageUp,
    PageDown,
    /// Delete.
    Delete,
}

/// The modifiers held when an event was produced.
#[derive(Clone, Copy, PartialEq, Eq, Default, Debug)]
pub struct Modifiers {
    /// Shift, either side.
    pub shift: bool,
    /// Control, either side.
    pub control: bool,
    /// Alt, either side.
    pub alt: bool,
    /// Caps lock, which is a latch rather than a hold.
    pub caps: bool,
}

/// One decoded key press.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Event {
    /// What was pressed.
    pub key: Key,
    /// What was held at the time.
    pub modifiers: Modifiers,
}

/// Events the queue holds before it starts dropping. Sixty-four keystrokes is more than a
/// person can produce while a line of text is being drawn.
pub const QUEUE_CAPACITY: usize = 64;

/// The byte that prefixes a two-byte scancode: the arrows, home, end, delete, the keypad's
/// divide and enter, and the right-hand control and alt.
const EXTENDED: u8 = 0xe0;

/// The scancode set 1 table, unshifted and shifted, for codes 0x00..=0x7f.
///
/// `'\0'` marks a code that produces no character: modifiers, function keys, the keypad with
/// num lock off, and the codes nothing is wired to. Set 1 is what a controller with
/// translation enabled hands over, which is the state `init` asks for.
const SET_1: [(char, char); 128] = {
    let mut table = [('\0', '\0'); 128];
    // An array literal would be a page of `(char, char)`; the loop is the same at compile
    // time and shows the shape of each row.
    let rows: [(usize, &str, &str); 6] = [
        (0x02, "1234567890-=", "!@#$%^&*()_+"),
        (0x10, "qwertyuiop[]", "QWERTYUIOP{}"),
        (0x1e, "asdfghjkl;'", "ASDFGHJKL:\""),
        (0x2b, "\\zxcvbnm,./", "|ZXCVBNM<>?"),
        (0x29, "`", "~"),
        (0x39, " ", " "),
    ];
    let mut row = 0;
    while row < rows.len() {
        let (start, plain, shifted) = rows[row];
        let plain = plain.as_bytes();
        let shifted = shifted.as_bytes();
        let mut index = 0;
        while index < plain.len() {
            table[start + index] = (plain[index] as char, shifted[index] as char);
            index += 1;
        }
        row += 1;
    }
    table
};

/// The state one scancode at a time is decoded against.
///
/// A value rather than a static, so a test can drive one without touching the machine's
/// keyboard, and so the interrupt handler's state is the same type.
#[derive(Clone, Copy, Default)]
pub struct Decoder {
    shift: bool,
    control: bool,
    alt: bool,
    caps: bool,
    extended: bool,
}

impl Decoder {
    /// A decoder with nothing held and nothing latched.
    pub const fn new() -> Self {
        Self {
            shift: false,
            control: false,
            alt: false,
            caps: false,
            extended: false,
        }
    }

    /// What is held right now, for a report or for the next event.
    pub fn modifiers(&self) -> Modifiers {
        Modifiers {
            shift: self.shift,
            control: self.control,
            alt: self.alt,
            caps: self.caps,
        }
    }

    /// Feeds one byte and returns an event if that byte completed one.
    ///
    /// The three cases that are not a character: a modifier changes state and produces
    /// nothing, a release (`0x80` set) is swallowed, and the extended prefix arms the next
    /// byte. A key this decoder has no name for is dropped rather than invented.
    pub fn feed(&mut self, byte: u8) -> Option<Event> {
        if byte == EXTENDED {
            self.extended = true;
            return None;
        }
        if byte == PAUSE {
            // The pause key sends five more bytes; clearing the prefix leaves them to be
            // swallowed as unknown codes, which is the right outcome.
            self.extended = false;
            return None;
        }
        let extended = core::mem::take(&mut self.extended);
        let released = byte & 0x80 != 0;
        let code = byte & 0x7f;
        match code {
            // Shift, either side. Held, not latched.
            0x2a | 0x36 => {
                self.shift = !released;
                None
            }
            // Control and alt.
            0x1d => {
                self.control = !released;
                None
            }
            0x38 => {
                self.alt = !released;
                None
            }
            // Caps lock: latched on the press, and the release does nothing - otherwise the
            // release would toggle it straight back.
            0x3a => {
                if !released {
                    self.caps = !self.caps;
                }
                None
            }
            _ if released => None,
            _ if extended => named(code).map(|key| self.event(key)),
            _ => plain_named(code)
                .or_else(|| char_for(code, self.shift, self.caps))
                .map(|key| self.event(key)),
        }
    }

    /// Wraps a key with the modifiers held now.
    fn event(&self, key: Key) -> Event {
        Event {
            key,
            modifiers: self.modifiers(),
        }
    }
}

/// The keys that have a scancode of their own rather than a character.
///
/// Set 1 puts return, escape, backspace and tab in the same low range as the letters, which is
/// why the character table has gaps at 0x01, 0x0e, 0x0f and 0x1c. Leaving them as gaps is what
/// made the shell receive nine characters and no return at all: the letters of three commands
/// arrived and none of them was ever submitted.
const fn plain_named(code: u8) -> Option<Key> {
    match code {
        0x01 => Some(Key::Escape),
        0x0e => Some(Key::Backspace),
        0x0f => Some(Key::Tab),
        0x1c => Some(Key::Enter),
        _ => None,
    }
}

/// The named keys that arrive behind the extended prefix.
const fn named(code: u8) -> Option<Key> {
    match code {
        0x48 => Some(Key::Up),
        0x50 => Some(Key::Down),
        0x4b => Some(Key::Left),
        0x4d => Some(Key::Right),
        0x47 => Some(Key::Home),
        0x4f => Some(Key::End),
        0x49 => Some(Key::PageUp),
        0x51 => Some(Key::PageDown),
        0x53 => Some(Key::Delete),
        _ => None,
    }
}

/// The character a set 1 code produces, with shift and caps lock applied.
///
/// Caps lock inverts shift for letters only. That is what the lamp on the keyboard means:
/// with caps on, `a` is `A` and shift-`a` is `a`, while shift-`4` is still `$`.
fn char_for(code: u8, shift: bool, caps: bool) -> Option<Key> {
    if code >= 0x80 {
        return None;
    }
    let (plain, shifted) = SET_1[code as usize];
    if plain == '\0' {
        return None;
    }
    if plain.is_ascii_alphabetic() {
        let upper = shift ^ caps;
        return Some(if upper {
            Key::Char(plain.to_ascii_uppercase())
        } else {
            Key::Char(plain)
        });
    }
    Some(Key::Char(if shift { shifted } else { plain }))
}

/// The byte that prefixes the pause key's long sequence, which this decoder ignores.
const PAUSE: u8 = 0xe1;

/// The ring the interrupt handler fills and the reader drains.
///
/// One producer and one consumer, over a fixed array: the slot before `head` is never
/// written, which is how a full ring and an empty one are told apart, and a full ring
/// refuses the new event rather than overwriting the oldest - the oldest is the one a
/// reader is about to take.
///
/// It lives outside the device module because it is an ordinary data structure: the host
/// tests fill it, wrap it and empty it with no controller in sight.
pub struct Queue {
    events: [Option<Event>; QUEUE_CAPACITY],
    head: usize,
    tail: usize,
}

impl Queue {
    /// An empty ring.
    pub const fn new() -> Self {
        Self {
            events: [None; QUEUE_CAPACITY],
            head: 0,
            tail: 0,
        }
    }

    /// Adds one event, or reports that the ring is full.
    pub fn push(&mut self, event: Event) -> bool {
        let next = (self.tail + 1) % QUEUE_CAPACITY;
        if next == self.head {
            return false;
        }
        self.events[self.tail] = Some(event);
        self.tail = next;
        true
    }

    /// Takes the oldest event, or `None` if there is none.
    pub fn pop(&mut self) -> Option<Event> {
        if self.head == self.tail {
            return None;
        }
        let event = self.events[self.head].take();
        self.head = (self.head + 1) % QUEUE_CAPACITY;
        event
    }
}

impl Default for Queue {
    fn default() -> Self {
        Self::new()
    }
}

/// The 8042 controller, the queue it feeds and the IRQ that drains it.
///
/// `x86_64`-only: everything here is a port or an interrupt.
#[cfg(target_arch = "x86_64")]
mod device {
    use core::cell::UnsafeCell;
    use core::sync::atomic::{AtomicU64, Ordering};

    use super::{Decoder, Event, Queue};
    use crate::idt::{self, InterruptFrame};
    use crate::pic;
    use crate::port::{inb, outb};

    /// The controller's data port and its status/command port.
    const DATA: u16 = 0x60;
    const STATUS_COMMAND: u16 = 0x64;

    /// Status register bits.
    const OUTPUT_FULL: u8 = 1 << 0;
    const INPUT_FULL: u8 = 1 << 1;
    /// Set on a status byte that describes data from the mouse's port rather than the
    /// keyboard's. The handler checks it so a movement byte is never decoded as a key.
    const AUX_DATA: u8 = 1 << 5;

    /// Commands the controller understands.
    const READ_CONFIG: u8 = 0x20;
    const WRITE_CONFIG: u8 = 0x60;
    const DISABLE_FIRST: u8 = 0xad;
    const DISABLE_SECOND: u8 = 0xa7;
    const ENABLE_FIRST: u8 = 0xae;

    /// Configuration byte bits.
    const CONFIG_FIRST_IRQ: u8 = 1 << 0;
    /// Set by the firmware: scancode set 2 from the keyboard is translated to set 1 for the
    /// CPU, which is the set [`Decoder`] speaks. Turning it off would mean decoding set 2.
    const CONFIG_TRANSLATE: u8 = 1 << 6;
    /// Set means the first port's clock is disabled, which also stops its interrupts.
    const CONFIG_FIRST_CLOCK_OFF: u8 = 1 << 4;

    /// The line the keyboard is on.
    const IRQ: u8 = 1;

    /// How many status polls before a byte is declared lost. The controller answers in
    /// microseconds; a hundred thousand polls is already far past "something is wrong".
    const POLLS: u32 = 100_000;

    /// A cell the kernel's single-threaded boot may hand out mutably, because every borrow
    /// of it is bounded by either an interrupt mask or the fact that only the handler runs
    /// while the reader is masked out.
    struct Cell<T>(UnsafeCell<T>);

    // SAFETY: `Queue` and `Decoder` are written by the IRQ handler and by `pop`, and `pop`
    // masks interrupts across its access, so no two borrows of the same cell overlap.
    unsafe impl<T: Send> Sync for Cell<T> {}

    static QUEUE: Cell<Queue> = Cell(UnsafeCell::new(Queue::new()));
    static DECODER: Cell<Decoder> = Cell(UnsafeCell::new(Decoder::new()));
    static EVENTS: AtomicU64 = AtomicU64::new(0);
    static DROPPED: AtomicU64 = AtomicU64::new(0);

    /// Reads one byte from the controller, waiting for it to have one.
    ///
    /// # Safety
    ///
    /// The caller must own the 8042's ports, which `init` arranges and nothing undoes.
    unsafe fn read_data() -> Option<u8> {
        for _ in 0..POLLS {
            // SAFETY: the status port is read-only.
            if unsafe { inb(STATUS_COMMAND) } & OUTPUT_FULL != 0 {
                // SAFETY: the output buffer is full, so this reads a byte rather than
                // waiting or returning stale data.
                return Some(unsafe { inb(DATA) });
            }
        }
        None
    }

    /// Sends a command byte, waiting for the controller's input buffer to empty.
    ///
    /// # Safety
    ///
    /// As [`read_data`].
    unsafe fn write_command(value: u8) -> bool {
        for _ in 0..POLLS {
            // SAFETY: the status port is read-only.
            if unsafe { inb(STATUS_COMMAND) } & INPUT_FULL == 0 {
                // SAFETY: the controller is ready for a command byte.
                unsafe { outb(STATUS_COMMAND, value) };
                return true;
            }
        }
        false
    }

    /// Sends a data byte, the second half of the two-byte commands.
    ///
    /// # Safety
    ///
    /// As [`read_data`].
    unsafe fn write_data(value: u8) -> bool {
        for _ in 0..POLLS {
            // SAFETY: the status port is read-only.
            if unsafe { inb(STATUS_COMMAND) } & INPUT_FULL == 0 {
                // SAFETY: the controller is ready for a data byte.
                unsafe { outb(DATA, value) };
                return true;
            }
        }
        false
    }

    /// Drains whatever the firmware left in the output buffer.
    ///
    /// # Safety
    ///
    /// As [`read_data`].
    unsafe fn flush() {
        for _ in 0..POLLS {
            // SAFETY: the status port is read-only.
            if unsafe { inb(STATUS_COMMAND) } & OUTPUT_FULL == 0 {
                return;
            }
            // SAFETY: a byte is waiting and reading it is how the buffer empties.
            unsafe {
                inb(DATA);
            }
        }
    }

    /// Configures the controller and starts the keyboard's interrupt.
    ///
    /// Returns the configuration byte it wrote, so the boot log can name what it asked
    /// for, or `None` if the controller never answered.
    ///
    /// # Safety
    ///
    /// Must run once, with interrupts disabled, before anything else claims port 0x60.
    pub unsafe fn init() -> Option<u8> {
        let wanted;
        // SAFETY: the caller promised this runs first and alone.
        unsafe {
            // Both ports go quiet while their configuration is rewritten: a keystroke
            // arriving between the command and its data byte would be read as configuration.
            if !write_command(DISABLE_FIRST) || !write_command(DISABLE_SECOND) {
                return None;
            }
            flush();
            if !write_command(READ_CONFIG) {
                return None;
            }
            let config = read_data()?;
            // Translation on (so the decoder's set is the one the controller sends), the
            // keyboard's interrupt on, and its clock on. Everything else as found.
            wanted = (config | CONFIG_FIRST_IRQ | CONFIG_TRANSLATE) & !CONFIG_FIRST_CLOCK_OFF;
            if !write_command(WRITE_CONFIG) || !write_data(wanted) {
                return None;
            }
            if !write_command(ENABLE_FIRST) {
                return None;
            }
            idt::set_irq(IRQ, interrupt);
        }
        // Unmasked only after the gate is in place: an interrupt with the counter stub
        // behind it would be counted as unexpected rather than as a keystroke.
        pic::unmask(IRQ);
        Some(wanted)
    }

    /// The IRQ1 handler: one byte in, at most one event out, and an acknowledgement.
    ///
    /// The byte is read whatever it is. The controller will not raise another interrupt
    /// until its output buffer is empty, so a decoder that rejected the byte without
    /// reading it would wedge the keyboard.
    extern "x86-interrupt" fn interrupt(_frame: InterruptFrame) {
        // The status is read first: the two ports share a data register, and a byte that
        // belongs to the mouse has to be left for the mouse's handler. Reading it here
        // would turn a movement byte into a keystroke.
        // SAFETY: the status port is read-only.
        let status = unsafe { inb(STATUS_COMMAND) };
        if status & OUTPUT_FULL != 0 && status & AUX_DATA == 0 {
            // SAFETY: the output buffer is full, or this interrupt would not have fired.
            let byte = unsafe { inb(DATA) };
            // SAFETY: the handler is the only writer of the decoder while the reader is
            // either masked or the one that masked it, and this borrow ends with the block.
            let decoder = unsafe { &mut *DECODER.0.get() };
            if let Some(event) = decoder.feed(byte) {
                // SAFETY: as above, for the queue; `push` is the producer's half of the ring.
                let queued = unsafe { (&mut *QUEUE.0.get()).push(event) };
                if queued {
                    EVENTS.fetch_add(1, Ordering::Relaxed);
                } else {
                    DROPPED.fetch_add(1, Ordering::Relaxed);
                }
            }
        }
        pic::eoi(IRQ);
    }

    /// Takes the oldest event, or `None` if none arrived.
    ///
    /// Interrupts are masked across the read: with `IF` clear the handler cannot run, and
    /// it is the only other writer, so the borrow that follows is exclusive.
    pub fn pop() -> Option<Event> {
        let enabled = idt::enabled();
        if enabled {
            idt::disable();
        }
        // SAFETY: the handler cannot run while interrupts are off, so no other borrow of
        // the queue exists for the length of this statement.
        let event = unsafe { (&mut *QUEUE.0.get()).pop() };
        if enabled {
            idt::enable();
        }
        event
    }

    /// How many events the handler produced.
    pub fn events() -> u64 {
        EVENTS.load(Ordering::Relaxed)
    }

    /// How many of those the queue had no room for.
    pub fn dropped() -> u64 {
        DROPPED.load(Ordering::Relaxed)
    }

    /// The modifiers held right now, whether or not an event was produced.
    pub fn modifiers() -> super::Modifiers {
        // SAFETY: reading a `Copy` value that the handler may also be writing; a torn read
        // would be one modifier byte, and the handler's next event carries the truth.
        unsafe { (*DECODER.0.get()).modifiers() }
    }
}

#[cfg(target_arch = "x86_64")]
pub use device::{dropped, events, init, modifiers, pop};

#[cfg(test)]
mod tests {
    use super::*;

    /// Feeds a slice of scancodes and returns every event they produced, which is how a
    /// test says "these bytes are this keystroke".
    fn decode(bytes: &[u8]) -> Vec<Event> {
        let mut decoder = Decoder::new();
        bytes
            .iter()
            .filter_map(|byte| decoder.feed(*byte))
            .collect()
    }

    #[test]
    fn a_press_produces_its_character_and_a_release_produces_nothing() {
        assert_eq!(decode(&[0x1e]).len(), 1);
        assert_eq!(decode(&[0x1e])[0].key, Key::Char('a'));
        assert!(decode(&[0x9e]).is_empty());
    }

    #[test]
    fn shift_is_held_and_released_rather_than_latched() {
        // Shift down, `a`, shift up, `a` again.
        let events = decode(&[0x2a, 0x1e, 0xaa, 0x1e]);
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].key, Key::Char('A'));
        assert!(events[0].modifiers.shift);
        assert_eq!(events[1].key, Key::Char('a'));
        assert!(!events[1].modifiers.shift);
        // Either shift works.
        assert_eq!(decode(&[0x36, 0x1e])[0].key, Key::Char('A'));
    }

    #[test]
    fn caps_lock_latches_on_the_press_and_shift_inverts_it() {
        // Caps on, `a` is `A`.
        let events = decode(&[0x3a, 0x1e]);
        assert_eq!(events[0].key, Key::Char('A'));
        assert!(events[0].modifiers.caps);
        // With caps still on, shift-`a` is `a`: the two invert each other.
        assert_eq!(decode(&[0x3a, 0x2a, 0x1e])[0].key, Key::Char('a'));
        // Caps off again: the release of the caps key itself must not toggle it back.
        let mut decoder = Decoder::new();
        assert!(decoder.feed(0x3a).is_none());
        assert!(decoder.modifiers().caps);
        assert!(decoder.feed(0xba).is_none());
        assert!(
            decoder.modifiers().caps,
            "the release toggled caps back off"
        );
        assert!(decoder.feed(0x3a).is_none());
        assert!(!decoder.modifiers().caps);
        assert_eq!(
            decoder.feed(0x1e).map(|event| event.key),
            Some(Key::Char('a'))
        );
    }

    #[test]
    fn the_number_row_comes_from_the_table_not_from_arithmetic() {
        // `4` and `$` are the classic pair: they are not adjacent in ASCII.
        assert_eq!(decode(&[0x05])[0].key, Key::Char('4'));
        assert_eq!(decode(&[0x2a, 0x05])[0].key, Key::Char('$'));
        // And the keys where the table's rows meet.
        assert_eq!(decode(&[0x02])[0].key, Key::Char('1'));
        assert_eq!(decode(&[0x0d])[0].key, Key::Char('='));
        assert_eq!(decode(&[0x2a, 0x0d])[0].key, Key::Char('+'));
        assert_eq!(decode(&[0x39])[0].key, Key::Char(' '));
    }

    #[test]
    fn the_arrow_keys_arrive_behind_their_prefix() {
        assert_eq!(decode(&[0xe0, 0x48])[0].key, Key::Up);
        assert_eq!(decode(&[0xe0, 0x50])[0].key, Key::Down);
        assert_eq!(decode(&[0xe0, 0x4b])[0].key, Key::Left);
        assert_eq!(decode(&[0xe0, 0x4d])[0].key, Key::Right);
        assert_eq!(decode(&[0xe0, 0x47])[0].key, Key::Home);
        assert_eq!(decode(&[0xe0, 0x4f])[0].key, Key::End);
        // A release of an extended key is still a release.
        assert!(decode(&[0xe0, 0xc8]).is_empty());
        // The prefix must not leak into the next key.
        let events = decode(&[0xe0, 0x48, 0x1e]);
        assert_eq!(events.len(), 2);
        assert_eq!(events[1].key, Key::Char('a'));
    }

    #[test]
    fn control_and_alt_are_state_on_the_event() {
        let events = decode(&[0x1d, 0x1e]);
        assert_eq!(events[0].key, Key::Char('a'));
        assert!(events[0].modifiers.control);
        assert!(!events[0].modifiers.alt);
        let events = decode(&[0x38, 0x1e]);
        assert!(events[0].modifiers.alt);
        // Both released again.
        let events = decode(&[0x1d, 0x9d, 0x1e]);
        assert!(!events[0].modifiers.control);
    }

    #[test]
    fn codes_with_no_character_are_dropped_rather_than_invented() {
        // 0x7f is not a key on this keyboard; 0x3b is F1.
        assert!(decode(&[0x7f]).is_empty());
        assert!(decode(&[0x3b]).is_empty());
        // An extended code the table does not name.
        assert!(decode(&[0xe0, 0x5b]).is_empty());
        // The pause prefix, which this decoder deliberately ignores.
        assert!(decode(&[0xe1]).is_empty());
    }

    #[test]
    fn letters_carry_both_cases_and_shift_carries_the_symbols() {
        for (code, plain, shifted) in [
            (0x10u8, 'q', 'Q'),
            (0x1e, 'a', 'A'),
            (0x32, 'm', 'M'),
            (0x03, '2', '@'),
            (0x0b, '0', ')'),
        ] {
            assert_eq!(
                decode(&[code]).first().map(|event| event.key),
                Some(Key::Char(plain)),
                "code {code:#04x}"
            );
            assert_eq!(
                decode(&[0x2a, code]).first().map(|event| event.key),
                Some(Key::Char(shifted)),
                "code {code:#04x} with shift"
            );
        }
    }

    #[test]
    fn return_escape_backspace_and_tab_have_names_of_their_own() {
        // The four codes the character table leaves as gaps. A shell cannot be driven without
        // return, which is what the missing mapping cost: nine letters arrived and no
        // command was ever submitted.
        assert_eq!(
            decode(&[0x1c]).first().map(|event| event.key),
            Some(Key::Enter)
        );
        assert_eq!(
            decode(&[0x01]).first().map(|event| event.key),
            Some(Key::Escape)
        );
        assert_eq!(
            decode(&[0x0e]).first().map(|event| event.key),
            Some(Key::Backspace)
        );
        assert_eq!(
            decode(&[0x0f]).first().map(|event| event.key),
            Some(Key::Tab)
        );
        // And their releases produce nothing, like every other key's.
        for code in [0x9c, 0x81, 0x8e, 0x8f] {
            assert!(decode(&[code]).is_empty(), "code {code:#04x}");
        }
    }

    #[test]
    fn the_table_is_its_own_reference_for_every_printable_code() {
        // A code with a character must decode to that character with no modifiers; a code
        // without one must produce nothing, unless it is one of the four named keys that share
        // the low range. This is the table checking itself.
        for code in 0u8..0x80 {
            let (plain, _) = SET_1[code as usize];
            let events = decode(&[code]);
            match plain {
                '\0' => {
                    let named = plain_named(code).is_some();
                    assert_eq!(
                        events.is_empty(),
                        !named,
                        "code {code:#04x} produced {:?}",
                        events
                    );
                }
                _ => {
                    assert_eq!(events.len(), 1, "code {code:#04x} produced no event");
                    assert_eq!(events[0].key, Key::Char(plain), "code {code:#04x}");
                }
            }
        }
    }

    #[test]
    fn the_queue_holds_a_burst_and_drops_the_rest() {
        // The ring is the one piece of this module that is not a pure function, so it is
        // tested as one: a stack value, a capacity of events, and a full ring that says so
        // instead of overwriting what is still unread.
        let mut queue = Queue::new();
        for _ in 0..QUEUE_CAPACITY - 1 {
            assert!(queue.push(event('x')));
        }
        assert!(!queue.push(event('y')), "the ring accepted too much");
        for _ in 0..QUEUE_CAPACITY - 1 {
            assert_eq!(queue.pop(), Some(event('x')));
        }
        assert_eq!(queue.pop(), None);
        // And it is usable again after wrapping.
        assert!(queue.push(event('z')));
        assert_eq!(queue.pop(), Some(event('z')));
    }

    fn event(character: char) -> Event {
        Event {
            key: Key::Char(character),
            modifiers: Modifiers::default(),
        }
    }
}
