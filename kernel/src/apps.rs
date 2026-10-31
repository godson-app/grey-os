//! The applications the kernel puts on a fresh disk.
//!
//! Three programs, assembled by [`Builder`] rather than compiled in, written to `greyfs` as
//! [`Kind::Program`] files at first format and read back and run as tasks. The point is the
//! path: bytes in a filesystem, a parser, an interpreter, a window. If these three files run,
//! the whole path works.
//!
//! [`Kind::Program`]: crate::fs::Kind::Program

use alloc::vec::Vec;

use crate::vm::{op, Builder};

/// Prints two lines and sleeps in between, so its sleeps are visible in the tick count.
pub const HELLO: &str = "apps/hello.gvm";
/// Counts from one to a limit, exercising a loop, a local and a comparison.
pub const COUNT: &str = "apps/count.gvm";
/// Prints the first ten Fibonacci numbers, exercising two locals and a swap through them.
pub const FIB: &str = "apps/fib.gvm";

/// The names of every application, in the order [`all`] returns them.
pub const NAMES: [&str; 3] = [HELLO, COUNT, FIB];

/// Every application, ready to be written to a freshly formatted disk.
pub fn all() -> [(&'static str, Vec<u8>); 3] {
    [(HELLO, hello()), (COUNT, count(5)), (FIB, fib(10))]
}

/// `hello from apps/hello.gvm`, a sleep, then `goodbye`.
pub fn hello() -> Vec<u8> {
    let mut builder = Builder::new();
    builder.puts("hello from apps/hello.gvm\n");
    // The sleep is what makes this a task rather than a print: the scheduler runs the others
    // while this program is waiting.
    builder.sleep(20);
    builder.puts("goodbye from 20 ticks later\n");
    builder.op(op::HALT);
    builder.finish()
}

/// Counts from one to `limit` on one line.
pub fn count(limit: i32) -> Vec<u8> {
    let mut builder = Builder::new();
    builder.push(1).store(0);
    let start = builder.here();
    builder.load(0).push(limit).op(op::GT);
    let forward = builder.jump_ahead(op::JUMP_IF_NOT_ZERO);
    builder.load(0).op(op::PRINT);
    builder.puts(" ");
    builder.load(0).push(1).op(op::ADD).store(0);
    builder.jump(start);
    let end = builder.here();
    builder.op(op::HALT);
    builder.patch(forward, end);
    builder.finish()
}

/// Prints the first `count` Fibonacci numbers on one line.
///
/// Locals: 0 is the limit, 1 the previous value, 2 the current one, 3 the index.
pub fn fib(count: i32) -> Vec<u8> {
    let mut builder = Builder::new();
    builder.push(count).store(0);
    builder.push(0).store(1);
    builder.push(1).store(2);
    builder.push(0).store(3);
    let start = builder.here();
    builder.load(3).push(count).op(op::LT);
    let forward = builder.jump_ahead(op::JUMP_IF_ZERO);
    builder.load(1).op(op::PRINT);
    builder.puts(" ");
    // The next pair is (b, a + b), which the two stores below write in that order: the sum is
    // computed while both values are still in the locals, then `a` takes `b` and `b` takes the
    // sum.
    builder.load(1).load(2).op(op::ADD);
    builder.load(2).store(1);
    builder.store(2);
    builder.load(3).push(1).op(op::ADD).store(3);
    builder.jump(start);
    let end = builder.here();
    builder.op(op::HALT);
    builder.patch(forward, end);
    builder.finish()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vm::{Host, Machine, Outcome, Program};

    /// Collects a program's output so a test can read it without a window.
    #[derive(Default)]
    struct Recorder {
        output: alloc::string::String,
    }

    impl Host for Recorder {
        fn write(&mut self, text: &str) {
            self.output.push_str(text);
        }

        fn sleep(&mut self, _ticks: u16) {}
    }

    /// Runs a program the way the kernel does: parse the bytes, run them, return the output.
    fn run(bytes: &[u8]) -> (alloc::string::String, u32) {
        let program = Program::parse(bytes).expect("an application the kernel writes must parse");
        let mut host = Recorder::default();
        let mut machine = Machine::new();
        match machine
            .run(&program, &mut host, 10_000)
            .expect("it must run")
        {
            Outcome::Halted { steps } => (host.output, steps),
        }
    }

    #[test]
    fn every_application_parses_and_runs() {
        // The names and the programs are in the same order, which is what the disk writer relies
        // on.
        let programs = all();
        for (index, (name, bytes)) in programs.iter().enumerate() {
            assert_eq!(*name, NAMES[index]);
            let program = Program::parse(bytes).expect("a written application must parse");
            // Well under the size the disk and the parser are willing to take.
            assert!(bytes.len() < crate::vm::MAX_PROGRAM_BYTES);
            assert!(program.code_len() > 0);
        }
    }

    #[test]
    fn hello_prints_two_lines_with_a_sleep_between_them() {
        let (output, _) = run(&hello());
        assert_eq!(
            output,
            "hello from apps/hello.gvm\ngoodbye from 20 ticks later\n"
        );
    }

    #[test]
    fn count_counts_to_its_limit() {
        let (output, _) = run(&count(5));
        assert_eq!(output, "1 2 3 4 5 ");
        // A limit below the start prints nothing rather than looping.
        let (output, _) = run(&count(0));
        assert_eq!(output, "");
    }

    #[test]
    fn fib_prints_the_series_the_slow_way_is_not_allowed_to_disagree_with() {
        let (output, steps) = run(&fib(10));
        assert_eq!(output, "0 1 1 2 3 5 8 13 21 34 ");
        // Ten numbers, each a handful of instructions: a budget of ten thousand is generous, and
        // the count is asserted loosely so this test does not break when the assembler changes.
        assert!(steps < 500, "fib(10) took {steps} steps");
    }
}
