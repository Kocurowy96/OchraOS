// SPDX-License-Identifier: GPL-2.0-only

//! Kernel tasks and the context switch: the mechanism of scheduling. The policy (which task
//! runs next) lives in the safe kernel crate.
//!
//! A task is a function running on its own stack. To switch from task P to task N, the CPU
//! state P needs to continue later is saved on P's stack, P's stack pointer is stored in P's
//! [`Task`], and the CPU continues on N's stack, restoring what N saved there. Only the
//! "callee-saved" registers (rbx, rbp, r12–r15) need saving by hand: [`switch_to`] is an
//! ordinary function call for the compiler, which already keeps every other register it
//! cares about safe across calls.
//!
//! When the timer interrupt preempts a task, the interrupt entry path has already saved all
//! registers on the task's stack; switching inside the handler just leaves them there until
//! the task is picked again and returns from the interrupt.
//!
//! Every task stack is a [`KernelStack`] with a guard page below it.
//!
//! With several CPUs, a task put back into the run queue can be picked by another CPU while
//! the old one is still saving it. So every task has an `on_cpu` flag (as in Linux): it is set
//! by the CPU that switches to the task, and cleared by [`switch`] only once the task's state
//! is saved and the CPU has left its stack. A CPU waits for the flag to clear before it runs
//! the task.

use alloc::sync::Arc;
use core::arch::naked_asm;
use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use crate::arch::interrupts;
use crate::cpu_local;
use crate::mm::paging::KernelStack;
use crate::sync::IrqMutex;

/// Stack size of a task: 32 KiB (debug builds use a lot of stack for formatting).
const STACK_PAGES: u64 = 8;

/// A kernel task. Shared as `Arc<Task>` between the scheduler's queues and the CPU running it.
pub struct Task {
    id: u64,
    name: &'static str,
    /// Saved stack pointer while the task is not running.
    saved_rsp: UnsafeCell<u64>,
    /// A CPU runs this task, or is still saving it (see the module documentation).
    on_cpu: AtomicBool,
    /// Owned so it lives as long as the task.
    _stack: KernelStack,
}

// SAFETY: `saved_rsp` is only accessed by `switch_to` and `start`, with interrupts off: written
// by `switch` on the CPU leaving the task, and read by a CPU that has claimed the task through
// `on_cpu`, which happens only after the writer cleared it (release/acquire). So at most one
// CPU touches it at a time. Everything else in `Task` is atomic or immutable.
unsafe impl Sync for Task {}
// SAFETY: as above; nothing in a task is tied to the CPU that created it.
unsafe impl Send for Task {}

impl Task {
    pub fn id(&self) -> u64 {
        self.id
    }

    pub fn name(&self) -> &'static str {
        self.name
    }
}

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

/// Creates a task that will run `entry` (with interrupts on) the first time it is switched
/// to. Tasks do not end yet, hence `-> !`.
pub fn spawn(name: &'static str, entry: fn() -> !) -> Arc<Task> {
    let stack = KernelStack::new(STACK_PAGES);
    // The first switch to the task "returns" into `task_trampoline`, popping six zeroed
    // callee-saved registers first; r12 carries `entry`. Seven slots in all, placed so that
    // the stack is 16-byte aligned after the `ret`, as the ABI wants at a function start.
    let rsp = stack.top() - 72;
    let initial: [u64; 7] = [
        0,                                   // r15
        0,                                   // r14
        0,                                   // r13
        entry as usize as u64,               // r12
        0,                                   // rbx
        0,                                   // rbp
        task_trampoline as *const () as u64, // return address
    ];
    // SAFETY: `rsp..rsp + 56` lies inside the freshly mapped, writable stack (72 bytes below
    // its top), which nothing else uses yet.
    unsafe { core::ptr::copy_nonoverlapping(initial.as_ptr(), rsp as *mut u64, initial.len()) };

    Arc::new(Task {
        id: NEXT_ID.fetch_add(1, Ordering::Relaxed),
        name,
        saved_rsp: UnsafeCell::new(rsp),
        on_cpu: AtomicBool::new(false),
        _stack: stack,
    })
}

/// The running task, or `None` before [`start`].
pub fn current() -> Option<Arc<Task>> {
    let were_enabled = interrupts::disable();
    let task = current_slot().lock().clone();
    if were_enabled {
        interrupts::enable();
    }
    task
}

/// Leaves this CPU's boot stack for good and runs `first`. Call once on every CPU.
pub fn start(first: Arc<Task>) -> ! {
    interrupts::disable();
    claim(&first);
    // SAFETY: `first` is claimed by this CPU, so its saved stack pointer is stable.
    let next_rsp = unsafe { *first.saved_rsp.get() };
    *current_slot().lock() = Some(first);
    // The boot stack's context and flag are throw-away variables: nothing ever switches back.
    let mut boot_rsp = 0;
    let boot_on_cpu = AtomicBool::new(true);
    // SAFETY: `next_rsp` belongs to the claimed `first`; the boot stack is abandoned, so what
    // `switch` saves there is never used (it stays mapped, so the writes are harmless).
    unsafe { switch(&mut boot_rsp, next_rsp, boot_on_cpu.as_ptr()) };
    unreachable!("switched back to the boot stack");
}

/// Stops the current task and continues `next`. Returns when the current task is switched
/// back to. Must not be called while holding a lock that `next` might need.
pub fn switch_to(next: Arc<Task>) {
    let were_enabled = interrupts::disable();
    let previous = current_slot()
        .lock()
        .replace(next.clone())
        .expect("task::start has run");
    if Arc::ptr_eq(&previous, &next) {
        if were_enabled {
            interrupts::enable();
        }
        return;
    }
    let previous_rsp = previous.saved_rsp.get();
    claim(&next);
    // SAFETY: `next` is claimed by this CPU, so its saved stack pointer is stable.
    let next_rsp = unsafe { *next.saved_rsp.get() };
    // The per-CPU current task keeps `next` alive; `previous` stays alive in this stack frame while
    // suspended, and the scheduler's queue holds it too.
    drop(next);
    // SAFETY: `previous_rsp` and the `on_cpu` flag point into the current task's `Task`,
    // alive (see above); `next_rsp` is the stack pointer `next` saved when it was switched
    // away from (or the one `spawn` prepared), and `next` is claimed by this CPU. Interrupts
    // are off, so nothing runs in between.
    unsafe { switch(previous_rsp, next_rsp, previous.on_cpu.as_ptr()) };
    drop(previous);
    if were_enabled {
        interrupts::enable();
    }
}

/// Marks `task` as running on this CPU, first waiting until the CPU that ran it before has
/// finished saving it. The wait is short: that CPU is inside `switch_to` with interrupts off.
fn claim(task: &Task) {
    while task
        .on_cpu
        .compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed)
        .is_err()
    {
        core::hint::spin_loop();
    }
}

/// This CPU's "current task" slot. Interrupts must be off (see `cpu_local`).
fn current_slot() -> &'static IrqMutex<Option<Arc<Task>>> {
    &cpu_local::this()
        .expect("per-CPU data is set up before tasks run")
        .current_task
}

/// Saves the callee-saved registers and the stack pointer into `*previous_rsp`, then loads
/// `next_rsp`, clears `*previous_on_cpu` (the previous task is now fully saved and its stack
/// no longer in use) and restores the registers that were saved on the new stack.
///
/// # Safety
///
/// `previous_rsp` and `previous_on_cpu` must be valid for a write; `next_rsp` must be a stack
/// pointer saved by this function or prepared by [`spawn`], and nothing else may be using
/// that stack.
#[unsafe(naked)]
unsafe extern "C" fn switch(previous_rsp: *mut u64, next_rsp: u64, previous_on_cpu: *mut bool) {
    naked_asm!(
        "push rbp",
        "push rbx",
        "push r12",
        "push r13",
        "push r14",
        "push r15",
        "mov [rdi], rsp",
        "mov rsp, rsi",
        // A plain store is a release on x86-64: the saved stack pointer above becomes visible
        // to other CPUs before the cleared flag.
        "mov byte ptr [rdx], 0",
        "pop r15",
        "pop r14",
        "pop r13",
        "pop r12",
        "pop rbx",
        "pop rbp",
        "ret",
    )
}

/// Where a new task starts: passes the entry function (in r12, see `spawn`) to `task_start`.
#[unsafe(naked)]
extern "C" fn task_trampoline() -> ! {
    naked_asm!("mov rdi, r12", "call {start}", "ud2", start = sym task_start)
}

/// `entry` is the task's `fn() -> !` as an address: a Rust `fn` type cannot cross the
/// `extern "C"` boundary of the trampoline directly.
extern "C" fn task_start(entry: usize) -> ! {
    // SAFETY: `spawn` put a `fn() -> !` into r12, and the trampoline passes it on unchanged;
    // a `fn` pointer round-trips through `usize`.
    let entry = unsafe { core::mem::transmute::<usize, fn() -> !>(entry) };
    // The task was switched to with interrupts off (from `start`, `switch_to`, or a timer
    // interrupt handler); a new task begins with them on.
    interrupts::enable();
    entry()
}
