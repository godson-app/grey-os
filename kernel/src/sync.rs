//! The one place the kernel says "this cell is safe to share", written as a type.
//!
//! # Why this exists
//!
//! Seven modules used to carry their own `struct Cell<T>(UnsafeCell<T>)` and their own
//! `unsafe impl Sync`. Each one was correct for the same reason - the kernel is single-core,
//! and every borrow is either masked by `cli` or made by the one task that is running - and
//! each one wrote that reason down separately. Seven copies of an argument is seven chances to
//! copy it wrongly, and a comment cannot be checked by the compiler.
//!
//! [`InterruptMutex`] is that argument once, in one type, with one `unsafe impl Sync`. Its
//! safe path, [`InterruptMutex::lock`], masks maskable interrupts for as long as the borrow
//! lives and restores the previous flag when it drops - the nesting is handled, so a lock
//! taken inside a lock does not re-enable interrupts on the way out (ADR-038).
//!
//! # The two ways in
//!
//! - [`lock`](InterruptMutex::lock) is the default and needs no `unsafe`: it returns a guard
//!   that dereferences to `&mut T`, and the mask lives exactly as long as the guard does.
//! - [`as_ptr`](InterruptMutex::as_ptr) is the escape hatch for the allocators, whose
//!   `GlobalAlloc` methods take `&self` and need a pointer rather than a scoped borrow. A raw
//!   pointer is safe to form; dereferencing it is `unsafe`, so the caller is where the
//!   "interrupts are masked / nothing else runs" argument has to be written.
//!
//! # A guard does not survive a switch
//!
//! `lock` keeps interrupt handlers out, not other tasks. The scheduler is cooperative - it
//! switches only when a task asks it to - so the rule is the one that model already has: never
//! hold a [`Locked`] across a call that can switch. `main`'s `with_desktop` reads the pointer out
//! of its guard and drops the guard before drawing, which is the shape to copy.
//!
//! # What this does not claim
//!
//! This is not a spinlock. It is safe because there is one core and because no interrupt
//! handler allocates or takes a lock the interrupted code may hold (ADR-028). ADR-037 names
//! exactly this gap when it says SMP needs "a spinlock on every table that is currently
//! protected only by masking interrupts". When that milestone lands, `InterruptMutex` grows
//! the atomic test-and-set and becomes the spinlock; the call sites should not have to move.

use core::cell::UnsafeCell;
use core::marker::PhantomData;
use core::ops::{Deref, DerefMut};

/// Whether maskable interrupts are currently enabled, and how to change that.
///
/// The kernel reads the real flag through `idt`; a host build has no CPU to ask, so the
/// tests get a flag they can watch the guard flip. Both have the same contract: `mask` and
/// `unmask` are not counted, so callers must only undo a mask they applied.
#[cfg(target_arch = "x86_64")]
mod interrupts {
    /// `RFLAGS.IF`, as `idt` reads it.
    pub fn enabled() -> bool {
        crate::idt::enabled()
    }

    /// `cli`.
    pub fn mask() {
        crate::idt::disable();
    }

    /// `sti`.
    pub fn unmask() {
        crate::idt::enable();
    }
}

#[cfg(all(not(target_arch = "x86_64"), test))]
mod interrupts {
    use std::cell::Cell;

    std::thread_local! {
        /// The flag a real CPU would keep in `RFLAGS`. Thread-local because the host test
        /// harness runs tests in parallel, and a process-wide flag would let one test's mask
        /// be observed by another.
        static ENABLED: Cell<bool> = const { Cell::new(true) };
    }

    pub fn enabled() -> bool {
        ENABLED.with(Cell::get)
    }

    pub fn mask() {
        ENABLED.with(|flag| flag.set(false));
    }

    pub fn unmask() {
        ENABLED.with(|flag| flag.set(true));
    }
}

#[cfg(all(not(target_arch = "x86_64"), not(test)))]
mod interrupts {
    // A host build that is not the test harness has no interrupts to mask. Nothing reaches
    // this path today - the kernel's host build is `cargo test --lib` - but the module has
    // to exist so `sync` compiles anywhere the pure modules do.
    pub fn enabled() -> bool {
        true
    }

    pub fn mask() {}

    pub fn unmask() {}
}

/// Maskable interrupts are blocked for as long as this value lives.
///
/// Dropping it puts the flag back exactly as [`IrqGuard::enter`] found it. A guard that did
/// not have to mask - because interrupts were already off - does not unmask on the way out,
/// which is what makes nested critical sections safe.
#[must_use = "the mask ends as soon as the guard is dropped"]
pub struct IrqGuard {
    was_enabled: bool,
}

impl IrqGuard {
    /// Blocks maskable interrupts and remembers whether that was a change.
    pub fn enter() -> Self {
        let was_enabled = interrupts::enabled();
        if was_enabled {
            interrupts::mask();
        }
        Self { was_enabled }
    }

    /// Whether this guard is the one that turned interrupts off.
    ///
    /// The scheduler reads it to decide whether it still owns the mask after a switch: the
    /// guard that masked must be the one that unmasks.
    pub fn was_enabled(&self) -> bool {
        self.was_enabled
    }
}

impl Drop for IrqGuard {
    fn drop(&mut self) {
        if self.was_enabled {
            interrupts::unmask();
        }
    }
}

/// A value the kernel shares between the boot task and its interrupt handlers.
///
/// See the module documentation for the argument behind the single `unsafe impl Sync` below.
pub struct InterruptMutex<T: ?Sized> {
    value: UnsafeCell<T>,
}

// SAFETY: the kernel runs on one core, and every borrow of `value` is exclusive by one of two
// rules. `lock` masks maskable interrupts before handing out the guard, so a handler cannot
// run while it is alive; `as_ptr` is only dereferenced by code that has already masked, or by
// the boot before any handler is installed. Interrupt handlers here never allocate and never
// take one of these locks, which is the second half of the argument (ADR-028, ADR-038). A
// second core, or a handler that takes a lock, breaks it - and that is the milestone ADR-037
// describes, where this impl becomes a spinlock.
unsafe impl<T: ?Sized> Sync for InterruptMutex<T> {}

impl<T> InterruptMutex<T> {
    /// A cell holding `value`, usable in a `static`.
    pub const fn new(value: T) -> Self {
        Self {
            value: UnsafeCell::new(value),
        }
    }
}

impl<T: ?Sized> InterruptMutex<T> {
    /// Borrows the value with interrupts masked until the guard drops.
    ///
    /// This is the path new code should use: no `unsafe`, and the mask cannot be forgotten
    /// because it is a lifetime rather than a call to remember.
    pub fn lock(&self) -> Locked<'_, T> {
        let irq = IrqGuard::enter();
        Locked {
            value: self.value.get(),
            _irq: irq,
            _life: PhantomData,
        }
    }

    /// The address of the value, for the `&self` callers that need a pointer.
    ///
    /// Forming the pointer is safe; dereferencing it is not, and the caller has to say why it
    /// is the only one touching the cell. Prefer [`InterruptMutex::lock`].
    pub fn as_ptr(&self) -> *mut T {
        self.value.get()
    }
}

/// An exclusive borrow of an [`InterruptMutex`], with interrupts masked underneath it.
pub struct Locked<'a, T: ?Sized> {
    value: *mut T,
    /// Dropped after `value` is no longer used, which is why it is a field rather than a
    /// drop guard the borrow could outlive.
    _irq: IrqGuard,
    /// Ties the pointer to the cell it came from, so the borrow cannot escape `lock`.
    _life: PhantomData<&'a mut T>,
}

impl<T: ?Sized> Deref for Locked<'_, T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        // SAFETY: `value` came from a live cell, and the guard in this struct keeps interrupts
        // masked - so this is the only reference to the value - for as long as `self` lives.
        unsafe { &*self.value }
    }
}

impl<T: ?Sized> DerefMut for Locked<'_, T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        // SAFETY: as `deref`; taking `&mut self` also proves no other borrow of this guard
        // exists.
        unsafe { &mut *self.value }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_guard_masks_and_restores() {
        assert!(interrupts::enabled());
        {
            let _guard = IrqGuard::enter();
            assert!(!interrupts::enabled());
        }
        assert!(interrupts::enabled());
    }

    #[test]
    fn nested_guards_restore_in_order() {
        let outer = IrqGuard::enter();
        let inner = IrqGuard::enter();
        // The inner guard found interrupts already off, so it must not claim the mask.
        assert!(!inner.was_enabled());
        drop(inner);
        assert!(!interrupts::enabled());
        drop(outer);
        assert!(interrupts::enabled());
    }

    #[test]
    fn the_outermost_guard_is_the_one_that_masks() {
        let outer = IrqGuard::enter();
        assert!(outer.was_enabled());
        let inner = IrqGuard::enter();
        drop(inner);
        assert!(!interrupts::enabled());
        drop(outer);
        assert!(interrupts::enabled());
    }

    #[test]
    fn a_lock_hands_out_exclusive_access() {
        let cell = InterruptMutex::new(41u32);
        {
            let mut value = cell.lock();
            *value += 1;
            assert!(!interrupts::enabled());
        }
        assert!(interrupts::enabled());
        assert_eq!(*cell.lock(), 42);
    }

    #[test]
    fn a_static_cell_can_be_locked() {
        static COUNT: InterruptMutex<u64> = InterruptMutex::new(0);

        for _ in 0..3 {
            *COUNT.lock() += 1;
        }
        assert_eq!(*COUNT.lock(), 3);
    }

    #[test]
    fn as_ptr_names_the_same_value() {
        let cell = InterruptMutex::new(7u8);
        // SAFETY: nothing else touches `cell` in this test, and the dereference happens before
        // the reference would have a chance to overlap anything.
        let value = unsafe { *cell.as_ptr() };
        assert_eq!(value, 7);
    }
}
