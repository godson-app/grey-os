//! A stack machine, so an application can be a file rather than a function.
//!
//! # Why a bytecode
//!
//! "Applications" needs a program the kernel did not compile in: something in the filesystem,
//! loaded and run. Native code would need an ELF loader, per-program address spaces and a jump
//! into ring 3 - a milestone of its own. A small stack machine gets the interesting part - a
//! program is a file, the kernel parses it, a task runs it, its output arrives in a window -
//! and keeps the parser and the interpreter in a module the host tests can drive without a
//! disk or a screen.
//!
//! # The format
//!
//! ```text
//! "GVM1"  version:u8  code_len:u16  pool_len:u16   code[code_len]  pool[pool_len]
//! ```
//!
//! Little-endian, and validated when it is parsed rather than when it runs: every jump target
//! has to be inside the code, every pool offset in `Puts` inside the pool, and a truncated file
//! is refused before a single instruction executes. An interpreter that discovers its program
//! is malformed mid-run is a crash waiting for a bad file.
//!
//! # The machine
//!
//! A value stack of 32 cells, sixteen locals, and a step budget so a program that loops forever
//! ends as `StepLimit` instead of hanging the kernel. Output and sleeping go through [`Host`],
//! which is what lets the tests record what a program printed without a screen.

use alloc::string::String;
use alloc::vec::Vec;

/// The four bytes every program starts with.
pub const MAGIC: [u8; 4] = *b"GVM1";

/// The format this kernel runs. A file with another version is refused rather than guessed at.
pub const VERSION: u8 = 1;

/// The header: magic, version, two lengths.
pub const HEADER: usize = 9;

/// The largest value stack a program gets.
pub const STACK_CELLS: usize = 32;

/// The number of locals a program can use.
pub const LOCALS: usize = 16;

/// The largest program this kernel will run, in bytes.
pub const MAX_PROGRAM_BYTES: usize = 4096;

/// The opcodes.
///
/// Constants rather than an enum so the encoding is visible where the interpreter reads it,
/// and so an assembler can emit one byte at a time.
pub mod op {
    /// Ends the program.
    pub const HALT: u8 = 0x00;
    /// Pushes the next four bytes as a signed value.
    pub const PUSH: u8 = 0x01;
    /// Arithmetic: two values in, one out.
    pub const ADD: u8 = 0x02;
    pub const SUB: u8 = 0x03;
    pub const MUL: u8 = 0x04;
    pub const DIV: u8 = 0x05;
    pub const MOD: u8 = 0x06;
    /// Stack shuffling.
    pub const DUP: u8 = 0x07;
    pub const DROP: u8 = 0x08;
    pub const SWAP: u8 = 0x09;
    /// Comparisons, pushing 1 or 0.
    pub const EQ: u8 = 0x0a;
    pub const LT: u8 = 0x0b;
    pub const GT: u8 = 0x0c;
    /// Control flow. The target is an absolute code offset in the next two bytes.
    pub const JUMP: u8 = 0x0d;
    /// Pops and jumps when the value is zero.
    pub const JUMP_IF_ZERO: u8 = 0x0e;
    /// Pops and jumps when the value is not zero.
    pub const JUMP_IF_NOT_ZERO: u8 = 0x0f;
    /// Locals: one byte of index each.
    pub const LOAD: u8 = 0x10;
    pub const STORE: u8 = 0x11;
    /// Output. `PUTS` takes a two-byte offset into the pool.
    pub const PUTS: u8 = 0x12;
    /// Pops a value and prints it as a decimal number.
    pub const PRINT: u8 = 0x13;
    /// Pops a value and prints it as a character.
    pub const PRINT_CHAR: u8 = 0x14;
    /// Sleeps for the two-byte tick count, which is where a program lets other tasks run.
    pub const SLEEP: u8 = 0x15;
}

/// Everything that can be wrong with a program, and everything a program can do wrong.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Error {
    /// The four magic bytes are not there.
    BadMagic,
    /// A version this kernel does not run.
    BadVersion(u8),
    /// The header promises more bytes than the file has.
    Truncated,
    /// A file larger than a program has any business being.
    TooLarge(usize),
    /// An opcode this machine does not have.
    BadOpcode(u8),
    /// A jump target outside the code.
    BadJump(u16),
    /// A `Puts` offset outside the pool.
    BadPool(u16),
    /// A local index past [`LOCALS`].
    BadLocal(u8),
    /// The value stack ran out.
    StackUnderflow,
    /// The value stack overflowed.
    StackOverflow,
    /// Division or remainder by zero.
    DivideByZero,
    /// The program was still running when its budget ran out.
    StepLimit,
}

/// A validated program: code and pool, with every offset already checked.
pub struct Program {
    code: Vec<u8>,
    pool: Vec<u8>,
}

impl Program {
    /// Parses and validates `bytes`.
    ///
    /// Validation walks the code once with the same decoder the interpreter uses, which is what
    /// makes "this program cannot fault the machine" a property of the parse rather than a
    /// hope: after this returns `Ok`, every jump lands on an instruction and every pool
    /// reference is inside the pool.
    pub fn parse(bytes: &[u8]) -> Result<Self, Error> {
        if bytes.len() < HEADER {
            return Err(Error::Truncated);
        }
        if bytes.len() > MAX_PROGRAM_BYTES {
            return Err(Error::TooLarge(bytes.len()));
        }
        if bytes[0..4] != MAGIC {
            return Err(Error::BadMagic);
        }
        if bytes[4] != VERSION {
            return Err(Error::BadVersion(bytes[4]));
        }
        let code_len = u16::from_le_bytes([bytes[5], bytes[6]]) as usize;
        let pool_len = u16::from_le_bytes([bytes[7], bytes[8]]) as usize;
        let end = HEADER + code_len + pool_len;
        if bytes.len() < end {
            return Err(Error::Truncated);
        }
        let program = Program {
            code: bytes[HEADER..HEADER + code_len].to_vec(),
            pool: bytes[HEADER + code_len..end].to_vec(),
        };
        program.validate()?;
        Ok(program)
    }

    /// How many bytes of code the program has.
    pub fn code_len(&self) -> usize {
        self.code.len()
    }

    /// How many bytes of strings the program has.
    pub fn pool_len(&self) -> usize {
        self.pool.len()
    }

    /// Walks the code and checks every operand that refers to somewhere else.
    fn validate(&self) -> Result<(), Error> {
        let mut pc = 0;
        while pc < self.code.len() {
            let opcode = self.code[pc];
            pc += 1;
            match opcode {
                op::PUSH => {
                    if pc + 4 > self.code.len() {
                        return Err(Error::Truncated);
                    }
                    pc += 4;
                }
                op::JUMP | op::JUMP_IF_ZERO | op::JUMP_IF_NOT_ZERO => {
                    let target = self.word(pc)?;
                    if target as usize >= self.code.len() {
                        return Err(Error::BadJump(target));
                    }
                    pc += 2;
                }
                op::LOAD | op::STORE => {
                    let index = *self.code.get(pc).ok_or(Error::Truncated)?;
                    if index as usize >= LOCALS {
                        return Err(Error::BadLocal(index));
                    }
                    pc += 1;
                }
                op::PUTS => {
                    let offset = self.word(pc)?;
                    if offset as usize >= self.pool.len() {
                        return Err(Error::BadPool(offset));
                    }
                    pc += 2;
                }
                op::SLEEP => {
                    let _ = self.word(pc)?;
                    pc += 2;
                }
                op::HALT
                | op::ADD
                | op::SUB
                | op::MUL
                | op::DIV
                | op::MOD
                | op::DUP
                | op::DROP
                | op::SWAP
                | op::EQ
                | op::LT
                | op::GT
                | op::PRINT
                | op::PRINT_CHAR => {}
                other => return Err(Error::BadOpcode(other)),
            }
        }
        Ok(())
    }

    /// Two bytes at `at` as a little-endian word, or `Truncated`.
    fn word(&self, at: usize) -> Result<u16, Error> {
        match (self.code.get(at), self.code.get(at + 1)) {
            (Some(low), Some(high)) => Ok(u16::from_le_bytes([*low, *high])),
            _ => Err(Error::Truncated),
        }
    }
}

/// What a program is allowed to do to the world.
///
/// Two calls, because those are the two things a program can do that the machine cannot do for
/// itself: print, and wait. Everything else is arithmetic on its own stack.
pub trait Host {
    /// Writes text somewhere: a window, a recorder, a serial line.
    fn write(&mut self, text: &str);

    /// Gives up the CPU for `ticks`. A host with no scheduler behind it does nothing.
    fn sleep(&mut self, ticks: u16);
}

/// How a program ended.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Outcome {
    /// `Halt`, or the program counter ran off the end of the code.
    Halted {
        /// How many instructions ran.
        steps: u32,
    },
}

/// The interpreter.
pub struct Machine {
    stack: [i32; STACK_CELLS],
    top: usize,
    locals: [i32; LOCALS],
    pc: usize,
}

impl Machine {
    /// A machine with an empty stack and no locals set.
    pub fn new() -> Self {
        Self {
            stack: [0; STACK_CELLS],
            top: 0,
            locals: [0; LOCALS],
            pc: 0,
        }
    }

    /// How many values are on the stack. For the tests and the report.
    pub fn depth(&self) -> usize {
        self.top
    }

    /// Runs `program` until it halts, misbehaves or spends `budget` instructions.
    pub fn run(
        &mut self,
        program: &Program,
        host: &mut impl Host,
        budget: u32,
    ) -> Result<Outcome, Error> {
        let mut steps = 0u32;
        loop {
            if steps >= budget {
                return Err(Error::StepLimit);
            }
            steps += 1;
            // Running off the end is a halt: a program that forgot its `Halt` should stop, not
            // read whatever follows the code.
            if self.pc >= program.code.len() {
                return Ok(Outcome::Halted { steps });
            }
            let opcode = program.code[self.pc];
            self.pc += 1;
            match opcode {
                op::HALT => return Ok(Outcome::Halted { steps }),
                op::PUSH => {
                    let value = self.word32(program)?;
                    self.push(value)?;
                }
                op::ADD => self.binary(i32::wrapping_add)?,
                op::SUB => self.binary(i32::wrapping_sub)?,
                op::MUL => self.binary(i32::wrapping_mul)?,
                op::DIV => self.checked(i32::checked_div)?,
                op::MOD => self.checked(i32::checked_rem)?,
                op::DUP => {
                    let value = self.pop()?;
                    self.push(value)?;
                    self.push(value)?;
                }
                op::DROP => {
                    self.pop()?;
                }
                op::SWAP => {
                    let a = self.pop()?;
                    let b = self.pop()?;
                    self.push(a)?;
                    self.push(b)?;
                }
                op::EQ => self.compare(|a, b| a == b)?,
                op::LT => self.compare(|a, b| a < b)?,
                op::GT => self.compare(|a, b| a > b)?,
                op::JUMP => self.pc = self.word(program)? as usize,
                op::JUMP_IF_ZERO => {
                    let target = self.word(program)? as usize;
                    if self.pop()? == 0 {
                        self.pc = target;
                    }
                }
                op::JUMP_IF_NOT_ZERO => {
                    let target = self.word(program)? as usize;
                    if self.pop()? != 0 {
                        self.pc = target;
                    }
                }
                op::LOAD => {
                    let index = self.byte(program)? as usize;
                    let value = self.locals[index];
                    self.push(value)?;
                }
                op::STORE => {
                    let index = self.byte(program)? as usize;
                    let value = self.pop()?;
                    self.locals[index] = value;
                }
                op::PUTS => {
                    let offset = self.word(program)? as usize;
                    let text = pool_string(program, offset)?;
                    host.write(text);
                }
                op::PRINT => {
                    let value = self.pop()?;
                    host.write(&decimal(value));
                }
                op::PRINT_CHAR => {
                    // A control character would be invisible on a screen and a value that is
                    // not a character at all would be a wild `char`, so both print as `?`.
                    let text = match u32::try_from(self.pop()?).ok().and_then(char::from_u32) {
                        Some(character) if !character.is_control() => {
                            let mut text = String::new();
                            text.push(character);
                            text
                        }
                        _ => String::from("?"),
                    };
                    host.write(&text);
                }
                op::SLEEP => {
                    let ticks = self.word(program)?;
                    host.sleep(ticks);
                }
                other => return Err(Error::BadOpcode(other)),
            }
        }
    }
}

impl Default for Machine {
    fn default() -> Self {
        Self::new()
    }
}

impl Machine {
    /// Reads a four-byte value behind the opcode.
    fn word32(&mut self, program: &Program) -> Result<i32, Error> {
        let end = self.pc + 4;
        let bytes = program.code.get(self.pc..end).ok_or(Error::Truncated)?;
        let value = i32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
        self.pc = end;
        Ok(value)
    }

    /// Reads a two-byte word behind the opcode.
    fn word(&mut self, program: &Program) -> Result<u16, Error> {
        let low = *program.code.get(self.pc).ok_or(Error::Truncated)?;
        let high = *program.code.get(self.pc + 1).ok_or(Error::Truncated)?;
        self.pc += 2;
        Ok(u16::from_le_bytes([low, high]))
    }

    /// Reads one byte behind the opcode.
    fn byte(&mut self, program: &Program) -> Result<u8, Error> {
        let byte = *program.code.get(self.pc).ok_or(Error::Truncated)?;
        self.pc += 1;
        Ok(byte)
    }

    fn push(&mut self, value: i32) -> Result<(), Error> {
        if self.top >= STACK_CELLS {
            return Err(Error::StackOverflow);
        }
        self.stack[self.top] = value;
        self.top += 1;
        Ok(())
    }

    fn pop(&mut self) -> Result<i32, Error> {
        if self.top == 0 {
            return Err(Error::StackUnderflow);
        }
        self.top -= 1;
        Ok(self.stack[self.top])
    }

    /// Two operands, one result.
    fn binary(&mut self, operation: fn(i32, i32) -> i32) -> Result<(), Error> {
        let b = self.pop()?;
        let a = self.pop()?;
        self.push(operation(a, b))
    }

    /// Two operands through a checked operation, so division by zero is an error rather than a
    /// fault.
    fn checked(&mut self, operation: fn(i32, i32) -> Option<i32>) -> Result<(), Error> {
        let b = self.pop()?;
        let a = self.pop()?;
        self.push(operation(a, b).ok_or(Error::DivideByZero)?)
    }

    /// Two operands, compared into 1 or 0.
    fn compare(&mut self, operation: fn(i32, i32) -> bool) -> Result<(), Error> {
        let b = self.pop()?;
        let a = self.pop()?;
        self.push(i32::from(operation(a, b)))
    }
}

/// The string at `offset` in the pool, up to a NUL or the end of the pool.
fn pool_string(program: &Program, offset: usize) -> Result<&str, Error> {
    let tail = program
        .pool
        .get(offset..)
        .ok_or(Error::BadPool(offset as u16))?;
    let end = tail
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(tail.len());
    core::str::from_utf8(&tail[..end]).map_err(|_| Error::BadPool(offset as u16))
}

/// A signed decimal number, without `format!` so this module needs no allocator machinery
/// beyond `String`.
fn decimal(value: i32) -> String {
    let mut text = String::new();
    if value < 0 {
        text.push('-');
    }
    // `unsigned_abs` handles `i32::MIN`, which `-value` does not.
    let mut digits = value.unsigned_abs();
    let mut buffer = [0u8; 10];
    let mut index = buffer.len();
    loop {
        index -= 1;
        buffer[index] = b'0' + (digits % 10) as u8;
        digits /= 10;
        if digits == 0 {
            break;
        }
    }
    for byte in &buffer[index..] {
        text.push(char::from(*byte));
    }
    text
}

/// An assembler, so the kernel's own applications are a few readable lines rather than an
/// array of hex.
///
/// Jumps take absolute code offsets, which is why [`Builder::here`] exists: a loop is
/// `let start = builder.here(); ... ; builder.jump(start);`.
#[derive(Default)]
pub struct Builder {
    code: Vec<u8>,
    pool: Vec<u8>,
}

impl Builder {
    /// An empty program.
    pub fn new() -> Self {
        Self::default()
    }

    /// The offset the next instruction will have.
    pub fn here(&self) -> u16 {
        self.code.len() as u16
    }

    /// Emits a bare opcode.
    pub fn op(&mut self, opcode: u8) -> &mut Self {
        self.code.push(opcode);
        self
    }

    /// Emits `Push` with a four-byte value.
    pub fn push(&mut self, value: i32) -> &mut Self {
        self.code.push(op::PUSH);
        self.code.extend_from_slice(&value.to_le_bytes());
        self
    }

    /// Adds `text` to the pool and emits the `Puts` that prints it. The NUL is the
    /// terminator, so a program does not need to know its own string lengths.
    pub fn puts(&mut self, text: &str) -> &mut Self {
        let offset = self.pool.len() as u16;
        self.pool.extend_from_slice(text.as_bytes());
        self.pool.push(0);
        self.code.push(op::PUTS);
        self.code.extend_from_slice(&offset.to_le_bytes());
        self
    }

    /// Emits `Sleep`.
    pub fn sleep(&mut self, ticks: u16) -> &mut Self {
        self.code.push(op::SLEEP);
        self.code.extend_from_slice(&ticks.to_le_bytes());
        self
    }

    /// Emits `Load` for a local.
    pub fn load(&mut self, local: u8) -> &mut Self {
        self.code.push(op::LOAD);
        self.code.push(local);
        self
    }

    /// Emits `Store` into a local.
    pub fn store(&mut self, local: u8) -> &mut Self {
        self.code.push(op::STORE);
        self.code.push(local);
        self
    }

    /// Emits an unconditional jump to `target`.
    pub fn jump(&mut self, target: u16) -> &mut Self {
        self.jump_with(op::JUMP, target)
    }

    /// Emits a jump that pops and takes `target` when the value was zero.
    pub fn jump_if_zero(&mut self, target: u16) -> &mut Self {
        self.jump_with(op::JUMP_IF_ZERO, target)
    }

    /// Emits a jump that pops and takes `target` when the value was not zero.
    pub fn jump_if_not_zero(&mut self, target: u16) -> &mut Self {
        self.jump_with(op::JUMP_IF_NOT_ZERO, target)
    }

    fn jump_with(&mut self, opcode: u8, target: u16) -> &mut Self {
        self.code.push(opcode);
        self.code.extend_from_slice(&target.to_le_bytes());
        self
    }

    /// Emits a jump whose target is ahead, and returns the offset [`Builder::patch`] takes.
    ///
    /// A forward jump is the one thing an assembler cannot resolve in one pass, so it is the
    /// one thing that needs a name.
    pub fn jump_ahead(&mut self, opcode: u8) -> u16 {
        let at = self.here();
        self.jump_with(opcode, 0);
        at
    }

    /// Points the jump emitted at `at` at `target`.
    pub fn patch(&mut self, at: u16, target: u16) -> &mut Self {
        let operand = at as usize + 1;
        self.code[operand..operand + 2].copy_from_slice(&target.to_le_bytes());
        self
    }

    /// The program as bytes, ready to be written to a file.
    pub fn finish(&self) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(HEADER + self.code.len() + self.pool.len());
        bytes.extend_from_slice(&MAGIC);
        bytes.push(VERSION);
        bytes.extend_from_slice(&(self.code.len() as u16).to_le_bytes());
        bytes.extend_from_slice(&(self.pool.len() as u16).to_le_bytes());
        bytes.extend_from_slice(&self.code);
        bytes.extend_from_slice(&self.pool);
        bytes
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A host that keeps what the program printed and how it slept, so a test can assert on a
    /// program's output without a window.
    #[derive(Default)]
    struct Recorder {
        output: String,
        sleeps: Vec<u16>,
    }

    impl Host for Recorder {
        fn write(&mut self, text: &str) {
            self.output.push_str(text);
        }

        fn sleep(&mut self, ticks: u16) {
            self.sleeps.push(ticks);
        }
    }

    /// Runs `bytes` and returns what it printed.
    fn run(bytes: &[u8]) -> Result<(String, u32), Error> {
        let program = Program::parse(bytes)?;
        let mut host = Recorder::default();
        let mut machine = Machine::new();
        match machine.run(&program, &mut host, 10_000)? {
            Outcome::Halted { steps } => Ok((host.output, steps)),
        }
    }

    #[test]
    fn a_program_needs_its_magic_and_its_version() {
        let mut bytes = Builder::new().op(op::HALT).finish();
        assert_eq!(Program::parse(&bytes).map(|_| ()), Ok(()));

        bytes[0] = b'X';
        assert_eq!(Program::parse(&bytes).err(), Some(Error::BadMagic));

        let mut bytes = Builder::new().op(op::HALT).finish();
        bytes[4] = 99;
        assert_eq!(Program::parse(&bytes).err(), Some(Error::BadVersion(99)));
    }

    #[test]
    fn a_file_that_is_cut_short_is_refused_before_it_runs() {
        let full = Builder::new().puts("hello").op(op::HALT).finish();
        // Cut into the header, into the code, and into the pool: all three are `Truncated`,
        // because the header's promise about lengths is checked against the file.
        assert_eq!(Program::parse(&full[..4]).err(), Some(Error::Truncated));
        assert_eq!(
            Program::parse(&full[..HEADER + 1]).err(),
            Some(Error::Truncated)
        );
        assert_eq!(
            Program::parse(&full[..full.len() - 1]).err(),
            Some(Error::Truncated)
        );
        assert!(Program::parse(&[]).is_err());
    }

    #[test]
    fn a_program_bigger_than_the_limit_is_refused() {
        let bytes = alloc::vec![0u8; MAX_PROGRAM_BYTES + 1];
        assert_eq!(
            Program::parse(&bytes).err(),
            Some(Error::TooLarge(MAX_PROGRAM_BYTES + 1))
        );
    }

    #[test]
    fn a_bad_opcode_bad_jump_and_bad_pool_offset_are_caught_at_parse_time() {
        // 0xff is not an instruction.
        let mut bytes = Builder::new().op(0xff).finish();
        assert_eq!(Program::parse(&bytes).err(), Some(Error::BadOpcode(0xff)));

        // A jump past the end of the code.
        bytes = Builder::new().jump(99).finish();
        assert_eq!(Program::parse(&bytes).err(), Some(Error::BadJump(99)));

        // A `Puts` into a pool that is empty.
        bytes = Builder::new().puts("x").finish();
        bytes[7] = 0;
        bytes[8] = 0;
        assert_eq!(Program::parse(&bytes).err(), Some(Error::BadPool(0)));

        // A local past the last one.
        bytes = Builder::new().load(LOCALS as u8).finish();
        assert_eq!(
            Program::parse(&bytes).err(),
            Some(Error::BadLocal(LOCALS as u8))
        );
    }

    #[test]
    fn arithmetic_reaches_the_host_as_decimal() {
        let mut builder = Builder::new();
        builder.push(2).push(3).op(op::ADD).op(op::PRINT);
        builder.puts(" ");
        builder.push(9).push(4).op(op::SUB).op(op::PRINT);
        builder.puts(" ");
        builder.push(6).push(7).op(op::MUL).op(op::PRINT);
        builder.puts(" ");
        builder.push(9).push(2).op(op::DIV).op(op::PRINT);
        builder.puts(" ");
        builder.push(9).push(2).op(op::MOD).op(op::PRINT);
        builder.op(op::HALT);
        let (output, _) = run(&builder.finish()).expect("the program runs");
        assert_eq!(output, "5 5 42 4 1");
    }

    #[test]
    fn division_by_zero_is_an_error_and_not_a_fault() {
        let mut builder = Builder::new();
        builder.push(1).push(0).op(op::DIV).op(op::HALT);
        assert_eq!(run(&builder.finish()).err(), Some(Error::DivideByZero));

        // The remainder is checked the same way, because `x % 0` is a fault too.
        let mut builder = Builder::new();
        builder.push(1).push(0).op(op::MOD).op(op::HALT);
        assert_eq!(run(&builder.finish()).err(), Some(Error::DivideByZero));
    }

    #[test]
    fn the_stack_reports_underflow_and_overflow() {
        let mut builder = Builder::new();
        builder.op(op::DROP).op(op::HALT);
        assert_eq!(run(&builder.finish()).err(), Some(Error::StackUnderflow));

        let mut builder = Builder::new();
        for _ in 0..=STACK_CELLS {
            builder.push(1);
        }
        builder.op(op::HALT);
        assert_eq!(run(&builder.finish()).err(), Some(Error::StackOverflow));
    }

    #[test]
    fn a_loop_with_a_local_counts_to_five() {
        let mut builder = Builder::new();
        builder.push(0).store(0);
        let start = builder.here();
        builder.load(0).push(5).op(op::LT);
        let forward = builder.jump_ahead(op::JUMP_IF_ZERO);
        builder.load(0).op(op::PRINT);
        builder.puts(" ");
        builder.load(0).push(1).op(op::ADD).store(0);
        builder.jump(start);
        let end = builder.here();
        builder.op(op::HALT);
        builder.patch(forward, end);
        let (output, _) = run(&builder.finish()).expect("the program runs");
        assert_eq!(output, "0 1 2 3 4 ");
    }

    #[test]
    fn a_program_that_never_stops_runs_out_of_budget() {
        let mut builder = Builder::new();
        builder.jump(0);
        let program = Program::parse(&builder.finish()).expect("a jump to itself parses");
        let mut host = Recorder::default();
        let mut machine = Machine::new();
        assert_eq!(
            machine.run(&program, &mut host, 50).err(),
            Some(Error::StepLimit)
        );
    }

    #[test]
    fn a_program_that_falls_off_the_end_halts() {
        let mut builder = Builder::new();
        builder.push(7).op(op::PRINT);
        let (output, steps) = run(&builder.finish()).expect("the program runs");
        assert_eq!(output, "7");
        // Two instructions and the step that finds the code has ended, which is a halt rather
        // than a read past the end.
        assert_eq!(steps, 3);
    }

    #[test]
    fn strings_stop_at_their_nul_and_sleeps_reach_the_host() {
        let mut builder = Builder::new();
        builder.puts("two words");
        builder.sleep(25);
        builder.op(op::HALT);
        let program = Program::parse(&builder.finish()).expect("the program parses");
        // The pool holds the NUL: that is what makes the string end where it is meant to.
        assert_eq!(program.pool_len(), "two words".len() + 1);
        let mut host = Recorder::default();
        let mut machine = Machine::new();
        machine
            .run(&program, &mut host, 100)
            .expect("the program runs");
        assert_eq!(host.output, "two words");
        assert_eq!(host.sleeps, alloc::vec![25]);
    }

    #[test]
    fn characters_come_out_as_characters() {
        let mut builder = Builder::new();
        builder.push('g' as i32).op(op::PRINT_CHAR);
        builder.push(-1).op(op::PRINT_CHAR);
        builder.op(op::HALT);
        let (output, _) = run(&builder.finish()).expect("the program runs");
        // A value that is not a character is refused rather than turned into a wild `char`.
        assert_eq!(output, "g?");
    }

    #[test]
    fn the_assembler_writes_the_format_the_parser_reads() {
        let mut builder = Builder::new();
        builder.puts("hi").op(op::HALT);
        let bytes = builder.finish();
        assert_eq!(&bytes[0..4], &MAGIC);
        assert_eq!(bytes[4], VERSION);
        let program = Program::parse(&bytes).expect("what the assembler writes, the parser reads");
        // Two instructions: the `Puts` and the `Halt`.
        assert_eq!(program.code_len(), 4);
        assert_eq!(program.pool_len(), 3);
    }

    #[test]
    fn decimal_handles_negatives_and_the_smallest_int() {
        assert_eq!(decimal(0), "0");
        assert_eq!(decimal(7), "7");
        assert_eq!(decimal(-7), "-7");
        assert_eq!(decimal(1000), "1000");
        assert_eq!(decimal(i32::MIN), "-2147483648");
        assert_eq!(decimal(i32::MAX), "2147483647");
    }
}
