//! Tasks: a cooperative round-robin scheduler with real stacks.
//!
//! # Cooperative, and honest about it
//!
//! The timer counts ticks and wakes tasks whose sleep has expired; it does not switch. A
//! switch happens when a task calls [`yield_now`] or [`sleep_ticks`], and that is the whole
//! scheduling policy: a task that never yields never loses the CPU. Preemption needs a
//! context save on an interrupt stack frame per task, which is a different design - and one
//! that belongs with per-task address spaces rather than before them.
//!
//! # What a switch saves
//!
//! The callee-saved registers and the stack pointer: `rbx`, `rbp`, `r12`-`r15` and `rsp`.
//! Everything else is already on the task's stack, because `switch` is an ordinary function
//! call. A new task's stack is prepared with a return address that lands in
//! [`task_entry`], so switching to it for the first time "returns" into the task.
//!
//! # Why the policy is a separate, testable function
//!
//! Round robin has edge cases that are easy to get wrong and impossible to see in a boot log:
//! what happens when the current task is the only one ready, or when every task has finished,
//! or when the one sleeping is the one whose turn it is. Those live in [`next_ready`], which
//! takes a slice of states and returns an index, and the host tests walk every case.

/// The most tasks this kernel will run, including the boot task. Eight is more than the boot
/// needs and small enough that the task array is a fixed part of the scheduler rather than an
/// allocation the scheduler has to make before it can schedule.
pub const MAX_TASKS: usize = 8;

/// The stack each task gets, in bytes. Large enough for a task that formats a line and calls
/// into the compositor, small enough that eight of them cost 256 KiB rather than a megabyte.
pub const STACK_BYTES: usize = 32 * 1024;

/// What a task is doing.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum State {
    /// Waiting for its turn.
    Ready,
    /// The task holding the CPU.
    Running,
    /// Not to be run before this tick.
    Sleeping(u64),
    /// Returned from its entry point. It will not run again.
    Finished,
}

/// One task's scheduling state, as the policy sees it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Slot {
    /// What the task is doing.
    pub state: State,
}

/// The next slot to run after `current`, or `None` if nothing can run.
///
/// Round robin, starting at the slot after `current` and wrapping, so a task that yields
/// cannot keep the CPU by being first in the array. A sleeping task whose tick has not come is
/// passed over; a finished one never runs again. `ticks` is passed in rather than read so the
/// policy is a pure function of its inputs and the tests can walk time forward.
pub fn next_ready(slots: &[Slot], current: Option<usize>, ticks: u64) -> Option<usize> {
    if slots.is_empty() {
        return None;
    }
    let start = current.map_or(0, |index| index + 1);
    // The running task is the fallback rather than a candidate: returning it first would stop
    // the rotation dead, because a yield to yourself is not a switch. It is returned only when
    // nothing else can run, and `yield_now` then treats "next is current" as "keep going".
    let mut running = None;
    for offset in 0..slots.len() {
        let index = (start + offset) % slots.len();
        match slots[index].state {
            State::Ready => return Some(index),
            State::Running => running = Some(index),
            State::Sleeping(until) if ticks >= until => return Some(index),
            State::Sleeping(_) | State::Finished => {}
        }
    }
    running
}

/// Whether a sleeping task's wake tick has arrived.
pub fn is_awake(state: State, ticks: u64) -> bool {
    match state {
        State::Sleeping(until) => ticks >= until,
        _ => false,
    }
}

/// The state a task is left in when it gives up the CPU.
///
/// Only a task that was *running* becomes ready: one that has already said it is sleeping or
/// finished keeps saying so. Getting this wrong is what makes a finished task come back - the
/// first version of `yield_now` set every yielding task to `Ready`, so `finish` marked a task
/// done and the next switch undid it, and three tasks that had each finished their work ran
/// for six hundred ticks without ever finishing.
pub fn after_yield(state: State) -> State {
    match state {
        State::Running => State::Ready,
        other => other,
    }
}

/// The saved half of a task: what `switch` writes before it leaves and reads when it returns.
///
/// `#[repr(C)]` because the assembly addresses these fields by offset. The order here is the
/// order the assembly uses, and the comment beside each offset in `switch` names it.
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct Context {
    /// The task's stack pointer.
    pub rsp: u64,
    /// Callee-saved registers, in the order the procedure call convention lists them.
    pub rbx: u64,
    pub rbp: u64,
    pub r12: u64,
    pub r13: u64,
    pub r14: u64,
    pub r15: u64,
}

/// One entry point of a task.
pub type Entry = fn();

/// What the boot log and the shell report.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Stats {
    /// Tasks that exist, including the boot task.
    pub tasks: usize,
    /// Tasks that have finished.
    pub finished: usize,
    /// Context switches, including the ones back to the boot task.
    pub switches: u64,
    /// The slot that is running now.
    pub current: usize,
}

/// The machine half: stacks, the switch, and the scheduler's own state.
#[cfg(target_arch = "x86_64")]
mod arch {
    use alloc::alloc::{alloc, Layout};
    use core::cell::UnsafeCell;

    use super::{
        after_yield, is_awake, next_ready, Context, Entry, Slot, State, Stats, MAX_TASKS,
        STACK_BYTES,
    };
    use crate::idt;

    /// One task, with everything the scheduler needs to run it and to report it.
    struct Task {
        name: &'static str,
        /// The size of the stack the task was given; zero for the boot task, whose stack is
        /// the one the firmware handed the kernel. The allocation itself is deliberately
        /// leaked: a task's stack outlives every borrow the kernel has of it.
        stack_len: usize,
        ctx: Context,
        state: State,
        runs: u64,
    }

    struct Scheduler {
        tasks: [Option<Task>; MAX_TASKS],
        current: usize,
        switches: u64,
        started: bool,
    }

    impl Scheduler {
        const fn new() -> Self {
            Self {
                tasks: [const { None }; MAX_TASKS],
                current: 0,
                switches: 0,
                started: false,
            }
        }
    }

    /// The scheduler's state. One writer at a time: every function below masks interrupts for
    /// its critical section, and the timer's `tick` runs only with them unmasked.
    struct Cell<T>(UnsafeCell<T>);

    // SAFETY: every access is masked, and the only other writer is the timer's handler, which
    // cannot run while they are masked. `Scheduler` holds a raw pointer to each task's stack,
    // which is why it is not `Send`; the kernel is single-core and the pointer never leaves it.
    unsafe impl<T> Sync for Cell<T> {}

    static SCHEDULER: Cell<Scheduler> = Cell(UnsafeCell::new(Scheduler::new()));

    // The switch itself. Written as assembly because "save the callee-saved registers and the
    // stack pointer, load the other task's, return" has no Rust spelling: the compiler decides
    // which registers to save around a call, and this function has to decide.
    //
    // `ret` is what makes it work: the return address on the new stack is where the new task
    // resumes - the instruction after its own call the last time it was switched away from, or
    // `task_entry` for a task that has never run.
    core::arch::global_asm!(
        ".pushsection .text,\"ax\",@progbits",
        ".global grey_os_switch",
        ".type grey_os_switch, @function",
        "grey_os_switch:",
        "    mov [rdi + 0], rsp",
        "    mov [rdi + 8], rbx",
        "    mov [rdi + 16], rbp",
        "    mov [rdi + 24], r12",
        "    mov [rdi + 32], r13",
        "    mov [rdi + 40], r14",
        "    mov [rdi + 48], r15",
        "    mov rsp, [rsi + 0]",
        "    mov rbx, [rsi + 8]",
        "    mov rbp, [rsi + 16]",
        "    mov r12, [rsi + 24]",
        "    mov r13, [rsi + 32]",
        "    mov r14, [rsi + 40]",
        "    mov r15, [rsi + 48]",
        "    ret",
        ".size grey_os_switch, . - grey_os_switch",
        ".popsection",
    );

    extern "C" {
        /// Saves this task's context into `from` and resumes `to`.
        fn grey_os_switch(from: *mut Context, to: *const Context);
    }

    /// Sets up the scheduler with the boot task in slot zero.
    ///
    /// # Safety
    ///
    /// Must be called once, from the boot task, before any task is spawned.
    pub unsafe fn init() {
        // SAFETY: the caller promised this is the only call, and no task exists yet.
        unsafe {
            let scheduler = &mut *SCHEDULER.0.get();
            scheduler.tasks[0] = Some(Task {
                name: "boot",
                stack_len: 0,
                ctx: Context::default(),
                state: State::Running,
                runs: 1,
            });
            scheduler.current = 0;
            scheduler.started = true;
        }
    }

    /// Prepares a stack so that switching to it lands in [`task_entry`], with the entry point
    /// in `r12`.
    ///
    /// The entry travels in a callee-saved register rather than in a global, because a global
    /// would be overwritten by the next `spawn` before the first task ever ran - which is
    /// exactly what happened the first time: all three tasks started at the last one's entry
    /// point, and the log showed the same task three times.
    fn prepare(stack: *mut u8, len: usize, entry: Entry) -> Context {
        // Sixteen-byte alignment, then room for the return address `ret` pops.
        let top = (stack as usize + len) & !15;
        let pointer = top - 8;
        // SAFETY: `pointer` is inside the allocation, and this is written once, before any
        // task can see the stack.
        unsafe { (pointer as *mut u64).write(task_entry as *const () as u64) };
        Context {
            rsp: pointer as u64,
            r12: entry as usize as u64,
            ..Context::default()
        }
    }

    /// The first code a new task runs.
    ///
    /// Interrupts are enabled here because the switch that got here ran with them masked, and
    /// a task that began life with the timer stopped would never see a tick.
    extern "C" fn task_entry() -> ! {
        idt::enable();
        let start: usize;
        // SAFETY: `prepare` put the task's entry point in `r12`, and `switch` restored it from
        // the context before returning here.
        unsafe { core::arch::asm!("mov {}, r12", out(reg) start) };
        // SAFETY: the value came from `entry as usize` in `prepare`, for this task only.
        let entry: Entry = unsafe { core::mem::transmute(start) };
        entry();
        finish();
    }

    /// Creates a task, ready to run at the next switch.
    ///
    /// Returns its slot, or `None` if the table is full or the stack could not be allocated.
    pub fn spawn(name: &'static str, entry: Entry) -> Option<usize> {
        let enabled = idt::enabled();
        if enabled {
            idt::disable();
        }
        let slot = {
            let layout = Layout::from_size_align(STACK_BYTES, 16).ok();
            // SAFETY: interrupts are masked just above, so nothing else can be reading or
            // writing the table, and this borrow ends with the block.
            let scheduler = unsafe { &mut *SCHEDULER.0.get() };
            let free = scheduler
                .tasks
                .iter()
                .enumerate()
                .skip(1)
                .find(|(_, task)| task.is_none())
                .map(|(index, _)| index);
            match (layout, free) {
                (Some(layout), Some(index)) => {
                    // SAFETY: the layout is valid. A task's stack outlives every borrow the
                    // kernel has of it, and freeing it would need the task to still be inside
                    // a function on it, so it is leaked into the task instead.
                    let stack = unsafe { alloc(layout) };
                    if stack.is_null() {
                        None
                    } else {
                        scheduler.tasks[index] = Some(Task {
                            name,
                            stack_len: STACK_BYTES,
                            ctx: prepare(stack, STACK_BYTES, entry),
                            state: State::Ready,
                            runs: 0,
                        });
                        Some(index)
                    }
                }
                _ => None,
            }
        };
        if enabled {
            idt::enable();
        }
        slot
    }

    /// The states as the policy sees them.
    fn slots(scheduler: &Scheduler) -> [Slot; MAX_TASKS] {
        let mut states = [Slot {
            state: State::Finished,
        }; MAX_TASKS];
        for (index, task) in scheduler.tasks.iter().enumerate() {
            if let Some(task) = task {
                states[index] = Slot { state: task.state };
            }
        }
        states
    }

    /// Gives up the CPU if another task can run, and returns when this one is resumed.
    ///
    /// The borrow of the scheduler ends before the switch: the task that runs next borrows it
    /// too, and two live references into the same state would be exactly the aliasing the mask
    /// exists to prevent.
    pub fn yield_now() {
        let enabled = idt::enabled();
        if enabled {
            idt::disable();
        }
        // Computed in a scope so no reference into the scheduler outlives it.
        let pointers = {
            // SAFETY: interrupts are masked just above, and this borrow ends with the block -
            // before the switch, which is the point.
            let scheduler = unsafe { &mut *SCHEDULER.0.get() };
            let current = scheduler.current;
            let states = slots(scheduler);
            match next_ready(&states, Some(current), crate::timer::ticks()) {
                Some(next) if next != current => {
                    scheduler.switches += 1;
                    scheduler.tasks[current]
                        .as_mut()
                        .expect("the running task exists")
                        .state = after_yield(
                        scheduler.tasks[current]
                            .as_ref()
                            .expect("the running task exists")
                            .state,
                    );
                    let task = scheduler.tasks[next]
                        .as_mut()
                        .expect("the next task exists");
                    task.state = State::Running;
                    task.runs += 1;
                    scheduler.current = next;
                    // Two separate statements: one `&mut` into the table at a time, because
                    // both pointers point into the same array.
                    let from = {
                        let task = scheduler.tasks[current]
                            .as_mut()
                            .expect("the running task exists");
                        &mut task.ctx as *mut Context
                    };
                    let to = {
                        let task = scheduler.tasks[next]
                            .as_ref()
                            .expect("the next task exists");
                        &task.ctx as *const Context
                    };
                    Some((from, to))
                }
                _ => None,
            }
        };
        if let Some((from, to)) = pointers {
            // SAFETY: both point into the scheduler's task table, which nothing else writes
            // while interrupts are masked, and each names a different task.
            unsafe { grey_os_switch(from, to) };
        }
        if enabled {
            idt::enable();
        }
    }

    /// Stops the current task until `ticks` ticks have passed.
    pub fn sleep_ticks(ticks: u64) {
        let until = crate::timer::ticks().saturating_add(ticks);
        {
            let enabled = idt::enabled();
            if enabled {
                idt::disable();
            }
            // SAFETY: masked, so the timer's handler cannot see a half-written state.
            let scheduler = unsafe { &mut *SCHEDULER.0.get() };
            let current = scheduler.current;
            if let Some(task) = scheduler.tasks[current].as_mut() {
                task.state = State::Sleeping(until);
            }
            if enabled {
                idt::enable();
            }
        }
        // Yield until the timer has woken this task, which `tick` does by putting it back in
        // the ready state. `yield_now` with nothing else ready returns immediately, so this
        // waits in `hlt` rather than burning the CPU.
        while crate::timer::ticks() < until {
            yield_now();
            crate::halt_once();
        }
    }

    /// Marks the current task finished and never returns to it.
    pub fn finish() -> ! {
        {
            let enabled = idt::enabled();
            if enabled {
                idt::disable();
            }
            // SAFETY: masked, as above.
            let scheduler = unsafe { &mut *SCHEDULER.0.get() };
            let current = scheduler.current;
            if let Some(task) = scheduler.tasks[current].as_mut() {
                task.state = State::Finished;
            }
            if enabled {
                idt::enable();
            }
        }
        loop {
            yield_now();
            crate::halt_once();
        }
    }

    /// Wakes sleeping tasks whose tick has arrived. Runs in the timer's handler.
    pub fn tick() {
        let ticks = crate::timer::ticks();
        // SAFETY: this runs in the timer's interrupt handler, which cannot be interrupted by
        // another timer tick, and every other accessor masks interrupts.
        let scheduler = unsafe { &mut *SCHEDULER.0.get() };
        for task in scheduler.tasks.iter_mut().flatten() {
            if is_awake(task.state, ticks) {
                task.state = State::Ready;
            }
        }
    }

    /// What the boot log and the shell report.
    pub fn stats() -> Stats {
        let enabled = idt::enabled();
        if enabled {
            idt::disable();
        }
        // SAFETY: masked, as above.
        let scheduler = unsafe { &*SCHEDULER.0.get() };
        let stats = Stats {
            tasks: scheduler.tasks.iter().flatten().count(),
            finished: scheduler
                .tasks
                .iter()
                .flatten()
                .filter(|task| task.state == State::Finished)
                .count(),
            switches: scheduler.switches,
            current: scheduler.current,
        };
        if enabled {
            idt::enable();
        }
        stats
    }

    /// The name of the task in `slot`, or `None` if the slot is empty.
    pub fn task_name(slot: usize) -> Option<&'static str> {
        let enabled = idt::enabled();
        if enabled {
            idt::disable();
        }
        // SAFETY: masked, as above; only the name is read.
        let scheduler = unsafe { &*SCHEDULER.0.get() };
        let name = scheduler
            .tasks
            .get(slot)
            .and_then(|task| task.as_ref())
            .map(|task| task.name);
        if enabled {
            idt::enable();
        }
        name
    }

    /// The running task's name.
    pub fn current_name() -> &'static str {
        let enabled = idt::enabled();
        if enabled {
            idt::disable();
        }
        // SAFETY: masked, as above; only the name is read.
        let scheduler = unsafe { &*SCHEDULER.0.get() };
        let name = scheduler
            .tasks
            .get(scheduler.current)
            .and_then(|task| task.as_ref())
            .map_or("none", |task| task.name);
        if enabled {
            idt::enable();
        }
        name
    }

    /// How many times the task in `slot` has been switched to.
    pub fn runs(slot: usize) -> u64 {
        let enabled = idt::enabled();
        if enabled {
            idt::disable();
        }
        // SAFETY: masked, as above.
        let scheduler = unsafe { &*SCHEDULER.0.get() };
        let runs = scheduler
            .tasks
            .get(slot)
            .and_then(|task| task.as_ref())
            .map_or(0, |task| task.runs);
        if enabled {
            idt::enable();
        }
        runs
    }

    /// The stack a task was given, for a report that says what a task costs.
    pub fn stack_bytes(slot: usize) -> usize {
        let enabled = idt::enabled();
        if enabled {
            idt::disable();
        }
        // SAFETY: masked, as above.
        let scheduler = unsafe { &*SCHEDULER.0.get() };
        let bytes = scheduler
            .tasks
            .get(slot)
            .and_then(|task| task.as_ref())
            .map_or(0, |task| task.stack_len);
        if enabled {
            idt::enable();
        }
        bytes
    }
}

#[cfg(target_arch = "x86_64")]
pub use arch::{
    current_name, finish, init, runs, sleep_ticks, spawn, stack_bytes, stats, task_name, tick,
    yield_now,
};

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds a table of slots from a list of states.
    fn slots(states: &[State]) -> alloc::vec::Vec<Slot> {
        states.iter().map(|state| Slot { state: *state }).collect()
    }

    #[test]
    fn the_next_task_is_the_one_after_the_current_and_then_wraps() {
        let table = slots(&[State::Running, State::Ready, State::Ready]);
        // From slot 0, the next ready one is 1.
        assert_eq!(next_ready(&table, Some(0), 0), Some(1));
        // From slot 1, it is 2.
        assert_eq!(next_ready(&table, Some(1), 0), Some(2));
        // From the last, it wraps - and skips over the running task at 0, because a yield to
        // yourself is not a switch and would starve the others.
        assert_eq!(next_ready(&table, Some(2), 0), Some(1));
        // With nothing running, the search starts at the front.
        assert_eq!(next_ready(&table, None, 0), Some(1));
    }

    #[test]
    fn the_running_task_keeps_the_cpu_when_nothing_else_can_run() {
        // Every other task is asleep or done: the answer is "you", which `yield_now` turns
        // into "do not switch" rather than into a switch to the same task.
        let table = slots(&[State::Running, State::Finished, State::Sleeping(500)]);
        assert_eq!(next_ready(&table, Some(0), 10), Some(0));
        // With the same table but the boot task not marked running, nothing can run at all.
        let table = slots(&[State::Finished, State::Finished, State::Sleeping(500)]);
        assert_eq!(next_ready(&table, Some(0), 10), None);
    }

    #[test]
    fn a_sleeping_task_is_passed_over_until_its_tick() {
        let table = slots(&[State::Running, State::Sleeping(50), State::Ready]);
        assert_eq!(next_ready(&table, Some(0), 10), Some(2));
        assert_eq!(next_ready(&table, Some(0), 49), Some(2));
        // At its tick it is a candidate again, and it is the first one after the current.
        assert_eq!(next_ready(&table, Some(0), 50), Some(1));
    }

    #[test]
    fn a_finished_task_never_runs_again() {
        let table = slots(&[State::Running, State::Finished, State::Finished]);
        assert_eq!(next_ready(&table, Some(0), 0), Some(0));
        let table = slots(&[State::Finished, State::Finished]);
        assert_eq!(next_ready(&table, Some(0), 0), None);
        assert_eq!(next_ready(&[], None, 0), None);
    }

    #[test]
    fn a_task_that_yields_is_ready_but_one_that_slept_or_finished_is_not() {
        // The bug this function exists to prevent: every yielding task used to become ready,
        // which turned a finished task back into a runnable one every time it yielded.
        assert_eq!(after_yield(State::Running), State::Ready);
        assert_eq!(after_yield(State::Sleeping(10)), State::Sleeping(10));
        assert_eq!(after_yield(State::Finished), State::Finished);
        // Ready stays ready: a task that yields before it is switched to is still runnable.
        assert_eq!(after_yield(State::Ready), State::Ready);
    }

    #[test]
    fn a_sleep_wakes_on_its_tick_and_not_before() {
        assert!(!is_awake(State::Sleeping(100), 99));
        assert!(is_awake(State::Sleeping(100), 100));
        assert!(is_awake(State::Sleeping(100), 101));
        // Tasks in any other state are not woken by the timer, whatever the time is.
        assert!(!is_awake(State::Ready, 1000));
        assert!(!is_awake(State::Running, 1000));
        assert!(!is_awake(State::Finished, 1000));
    }
}
