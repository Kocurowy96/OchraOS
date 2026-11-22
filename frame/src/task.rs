// SPDX-License-Identifier: GPL-2.0-only

//! Kernel tasks and the context switch: the mechanism of scheduling. The policy (which task
//! runs next, when a task blocks) lives in the safe kernel crate.
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
//! The x87/SSE registers (floating point and vectors) are not callee-saved: user programs
//! use them freely, and the switch can happen in the middle of their use (a timer tick). So
//! every switch also saves them with `fxsave` into the task's [`FpuState`] and loads the next
//! task's with `fxrstor`. The kernel itself never uses these registers.
//!
//! With several CPUs, a task put back into the run queue can be picked by another CPU while
//! the old one is still saving it. So every task has an `on_cpu` flag (as in Linux): it is set
//! by the CPU that switches to the task, and cleared by [`switch`] only once the task's state
//! is saved and the CPU has left its stack. A CPU waits for the flag to clear before it runs
//! the task.
//!
//! A task that ends ([`exit_to`]) cannot free its own stack while running on it. It is parked
//! in the per-CPU `reaped` slot, and the task that runs next on that CPU drops it right after
//! the switch, when nothing uses the old stack any more.
//!
//! The fields a task's own CPU changes (saved stack pointer, FPU state, address space) are
//! only touched by the CPU that runs the task, or that has claimed it through `on_cpu`.

use alloc::boxed::Box;
use alloc::sync::Arc;
use core::any::Any;
use core::arch::naked_asm;
use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use crate::arch::exceptions::ExceptionFrame;
use crate::arch::{cpu, interrupts};
use crate::cpu_local;
use crate::mm::paging::{self, KernelStack};
use crate::mm::user::UserSpace;
use crate::sync::IrqMutex;
use crate::user_mode::{self, UserContext};

/// Stack size of a task: 32 KiB (debug builds use a lot of stack for formatting).
const STACK_PAGES: u64 = 8;

/// The kernel's own record of a task, opaque to `ochra-frame`.
pub type TaskData = Arc<dyn Any + Send + Sync>;

/// A kernel task. Shared as `Arc<Task>` between the scheduler's queues and the CPU running it.
pub struct Task {
    id: u64,
    name: &'static str,
    /// Saved stack pointer while the task is not running.
    saved_rsp: UnsafeCell<u64>,
    /// Saved x87/SSE registers while the task is not running.
    fpu: UnsafeCell<FpuState>,
    /// A CPU runs this task, or is still saving it (see the module documentation).
    on_cpu: AtomicBool,
    /// The user address space this task runs in; `None` for kernel-only tasks, which run on
    /// the kernel's page tables. `execve` replaces it ([`replace_space`]).
    space: UnsafeCell<Option<Arc<UserSpace>>>,
    /// Whether the task runs user code (then FS base and the address space matter).
    user: bool,
    /// For a new user task: how it enters user mode; taken when it starts.
    user_start: IrqMutex<Option<UserStart>>,
    /// The FS segment base of a user task: the thread pointer (TLS), set with `arch_prctl`.
    /// Loaded into the FS base MSR whenever the task is switched to.
    fs_base: AtomicU64,
    data: Option<TaskData>,
    /// Owned so it lives as long as the task.
    _stack: KernelStack,
}

/// How a user task enters user mode the first time.
enum UserStart {
    /// A new program: this instruction pointer and stack pointer, other registers zero.
    Fresh { entry: u64, stack: u64 },
    /// A copy of another task's registers (`fork`).
    Resume(Box<ExceptionFrame>),
}

/// The 512-byte area `fxsave` writes and `fxrstor` reads; it must be 16-byte aligned.
#[repr(C, align(16))]
struct FpuState([u8; 512]);

impl FpuState {
    /// The state a new program starts with, as on Linux: all registers zero, x87 control word
    /// 0x037F and MXCSR 0x1F80 (every floating point exception masked, round to nearest).
    fn initial() -> Self {
        let mut state = [0; 512];
        state[0..2].copy_from_slice(&0x037F_u16.to_le_bytes());
        state[24..28].copy_from_slice(&0x1F80_u32.to_le_bytes());
        Self(state)
    }
}

// SAFETY: the `UnsafeCell` fields are only accessed by the CPU running the task or by the CPU
// that claimed it through `on_cpu` (release/acquire), so at most one CPU at a time; see the
// module documentation. Everything else in `Task` is atomic, locked or immutable.
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

    /// The kernel's data for this task, given when it was created.
    pub fn data(&self) -> Option<&TaskData> {
        self.data.as_ref()
    }

    pub fn fs_base(&self) -> u64 {
        self.fs_base.load(Ordering::Relaxed)
    }

    /// The address space, read by the CPU that runs or has claimed the task.
    fn space(&self) -> Option<&Arc<UserSpace>> {
        // SAFETY: only that CPU writes the field (`replace_space` on the running task), and
        // the reference does not outlive the switch (see the module documentation).
        unsafe { (*self.space.get()).as_ref() }
    }
}

/// Model-specific register holding the FS segment base.
const FS_BASE_MSR: u32 = 0xC000_0100;

/// Sets the current task's FS base (its thread pointer), now and on every later switch to
/// it. Fails for an address outside the user half: a kernel address there would let user
/// code reach kernel memory through `fs:` (with SMAP off), and a non-canonical one would
/// fault in the kernel when loaded.
pub fn set_fs_base(value: u64) -> Result<(), user_mode::BadAddress> {
    if value >= crate::mm::user::USER_END {
        return Err(user_mode::BadAddress);
    }
    let were_enabled = interrupts::disable();
    let task = current_slot().lock().clone().expect("a task is running");
    task.fs_base.store(value, Ordering::Relaxed);
    // SAFETY: a canonical user address in FS base only affects `fs:` accesses, which the
    // kernel never makes.
    unsafe { cpu::write_msr(FS_BASE_MSR, value) };
    if were_enabled {
        interrupts::enable();
    }
    Ok(())
}

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

/// Creates a kernel task that will run `entry` (with interrupts on) the first time it is
/// switched to; it ends with [`exit_to`].
pub fn spawn(name: &'static str, entry: fn() -> !, data: Option<TaskData>) -> Arc<Task> {
    create(name, entry, None, None, data)
}

/// Creates a user task: it runs in `space` and starts in user mode at `entry` with the stack
/// pointer `stack`. Its kernel stack serves its syscalls and interrupts.
pub fn spawn_user(
    name: &'static str,
    space: Arc<UserSpace>,
    entry: u64,
    stack: u64,
    data: Option<TaskData>,
) -> Arc<Task> {
    let start = UserStart::Fresh { entry, stack };
    create(name, start_user_task, Some(space), Some(start), data)
}

/// Creates the child of `fork`: a user task in `space` that continues from the current
/// task's user registers `context`, but sees 0 as the syscall's result. It also inherits the
/// current task's FS base and x87/SSE registers. Call it from the parent's syscall.
pub fn fork_user(
    name: &'static str,
    space: Arc<UserSpace>,
    context: &UserContext,
    data: Option<TaskData>,
) -> Arc<Task> {
    let mut frame = context.frame().clone();
    frame.rax = 0;
    let start = UserStart::Resume(Box::new(frame));
    let task = create(name, start_user_task, Some(space), Some(start), data);
    let parent = current().expect("fork runs in a task");
    task.fs_base.store(parent.fs_base(), Ordering::Relaxed);
    let were_enabled = interrupts::disable();
    // SAFETY: the new task runs nowhere yet, so its FPU state is ours alone; the registers
    // hold the parent's user state, which the kernel never changes.
    unsafe { save_fpu(task.fpu.get()) };
    if were_enabled {
        interrupts::enable();
    }
    task
}

/// Replaces the current task's address space with `space` (for `execve`) and loads it. The
/// old one is dropped once nothing else holds it.
pub fn replace_space(space: Arc<UserSpace>) {
    let were_enabled = interrupts::disable();
    let task = current().expect("a task is running");
    load_space(Some(&space));
    // SAFETY: the running task's address space is only changed by itself, here, with
    // interrupts off, so no switch reads it meanwhile.
    let old = unsafe { (*task.space.get()).replace(space) };
    if let Some(old) = &old {
        old.note_unloaded();
    }
    if were_enabled {
        interrupts::enable();
    }
    drop(old);
}

/// Gives the current task the x87/SSE state of a new program (for `execve`).
pub fn reset_fpu() {
    let initial = FpuState::initial();
    // SAFETY: `initial` is a valid state (see `FpuState::initial`); it only replaces the
    // current task's live registers, which the kernel itself never uses.
    unsafe { load_fpu(&initial) };
}

/// First code of a user task, on its kernel stack: off to user mode.
fn start_user_task() -> ! {
    let task = current().expect("a task is running");
    let start = task
        .user_start
        .lock()
        .take()
        .expect("only user tasks start here");
    drop(task);
    match start {
        UserStart::Fresh { entry, stack } => user_mode::enter(entry, stack),
        UserStart::Resume(frame) => user_mode::resume(*frame),
    }
}

fn create(
    name: &'static str,
    entry: fn() -> !,
    space: Option<Arc<UserSpace>>,
    user_start: Option<UserStart>,
    data: Option<TaskData>,
) -> Arc<Task> {
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
        fpu: UnsafeCell::new(FpuState::initial()),
        on_cpu: AtomicBool::new(false),
        user: user_start.is_some(),
        space: UnsafeCell::new(space),
        user_start: IrqMutex::new(user_start),
        fs_base: AtomicU64::new(0),
        data,
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
    // SAFETY: `first` is claimed by this CPU, so its saved state is stable.
    let next_rsp = unsafe { *first.saved_rsp.get() };
    // SAFETY: as above, the claimed task's FPU state is ours to load.
    unsafe { load_fpu(first.fpu.get()) };
    load_space(first.space().map(Arc::as_ref));
    enter_task(&first);
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
    let next_rsp = prepare_switch(&previous, &next);
    // The per-CPU current task keeps `next` alive; `previous` stays alive in this stack frame
    // while suspended, and the scheduler holds it too.
    drop(next);
    // SAFETY: `previous_rsp` and the `on_cpu` flag point into the current task's `Task`,
    // alive (see above); `next_rsp` is the stack pointer `next` saved when it was switched
    // away from (or the one `create` prepared), and `next` is claimed by this CPU. Interrupts
    // are off, so nothing runs in between.
    unsafe { switch(previous_rsp, next_rsp, previous.on_cpu.as_ptr()) };
    drop(previous);
    finish_switch();
    if were_enabled {
        interrupts::enable();
    }
}

/// Ends the current task for good and continues `next`. The task is freed (with its kernel
/// stack) by the next task on this CPU, once nothing runs on that stack any more.
pub fn exit_to(next: Arc<Task>) -> ! {
    interrupts::disable();
    let previous = current_slot()
        .lock()
        .replace(next.clone())
        .expect("task::start has run");
    assert!(
        !Arc::ptr_eq(&previous, &next),
        "a task cannot exit to itself"
    );
    let previous_rsp = previous.saved_rsp.get();
    let previous_on_cpu = previous.on_cpu.as_ptr();
    let next_rsp = prepare_switch(&previous, &next);
    drop(next);
    // Kept alive until the switch is done; see `finish_switch`.
    let slot = reaped_slot();
    let _ = slot.lock().replace(previous);
    // SAFETY: `previous_rsp` and `previous_on_cpu` point into the exiting task, kept alive
    // by the `reaped` slot until `finish_switch` runs on the other side; `next` is claimed by
    // this CPU and its saved stack pointer is valid; interrupts are off.
    unsafe { switch(previous_rsp, next_rsp, previous_on_cpu) };
    unreachable!("switched back to a task that exited");
}

/// The common part of a switch: claims `next` and loads its FPU state, address space, FS base
/// and kernel stack. Returns `next`'s saved stack pointer.
fn prepare_switch(previous: &Task, next: &Task) -> u64 {
    // SAFETY: `previous` is the task running on this CPU (still claimed: its `on_cpu` is only
    // cleared by `switch`), so nothing else touches its FPU state.
    unsafe { save_fpu(previous.fpu.get()) };
    claim(next);
    // SAFETY: `next` is claimed by this CPU, so its saved state is stable.
    let next_rsp = unsafe { *next.saved_rsp.get() };
    // SAFETY: as above, the claimed task's FPU state is ours to load.
    unsafe { load_fpu(next.fpu.get()) };
    let same_space = match (previous.space(), next.space()) {
        (Some(a), Some(b)) => Arc::ptr_eq(a, b),
        (None, None) => true,
        _ => false,
    };
    if !same_space {
        load_space(next.space().map(Arc::as_ref));
        if let Some(space) = previous.space() {
            space.note_unloaded();
        }
    }
    enter_task(next);
    next_rsp
}

/// Runs right after every switch, on the new task's stack: frees a task that exited.
fn finish_switch() {
    let reaped = reaped_slot().lock().take();
    drop(reaped);
}

/// Loads `space` (or the kernel's tables) into CR3 on this CPU. Runs on a kernel stack, which
/// every address space maps.
fn load_space(space: Option<&UserSpace>) {
    let root = match space {
        Some(space) => {
            // Counted before loading, so a TLB shootdown cannot miss this CPU.
            space.note_loaded();
            space.root()
        }
        None => paging::kernel_root(),
    };
    // SAFETY: every address space maps the kernel half (shared with the kernel's tables), and
    // the code and stack running now are in the kernel half.
    unsafe { paging::write_cr3(root) };
}

/// Per-CPU state for the task about to run: its kernel stack (for interrupts and syscalls
/// from user mode) and, for a user task, its FS base. (Kernel tasks do not use FS.)
fn enter_task(task: &Task) {
    cpu_local::set_kernel_stack(task._stack.top());
    if task.user {
        // SAFETY: `fs_base` only ever holds user addresses (`set_fs_base`); see there.
        unsafe { cpu::write_msr(FS_BASE_MSR, task.fs_base()) };
    }
}

/// # Safety
///
/// `state` must be valid for writes, and no other CPU may access it.
unsafe fn save_fpu(state: *mut FpuState) {
    // SAFETY: `state` is a valid, 16-byte aligned 512-byte area (the caller and `FpuState`).
    unsafe { core::arch::asm!("fxsave64 [{}]", in(reg) state, options(nostack, preserves_flags)) };
}

/// # Safety
///
/// `state` must hold a state written by `fxsave` or [`FpuState::initial`], and no other CPU
/// may write to it.
unsafe fn load_fpu(state: *const FpuState) {
    // SAFETY: as above; an invalid MXCSR would raise #GP, but the state comes from `fxsave` or
    // from `initial`, whose MXCSR is valid.
    unsafe {
        core::arch::asm!(
            "fxrstor64 [{}]",
            in(reg) state,
            options(nostack, preserves_flags, readonly)
        )
    };
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

/// This CPU's slot for a task that has just exited. Interrupts must be off.
fn reaped_slot() -> &'static IrqMutex<Option<Arc<Task>>> {
    &cpu_local::this()
        .expect("per-CPU data is set up before tasks run")
        .reaped_task
}

/// Saves the callee-saved registers and the stack pointer into `*previous_rsp`, then loads
/// `next_rsp`, clears `*previous_on_cpu` (the previous task is now fully saved and its stack
/// no longer in use) and restores the registers that were saved on the new stack.
///
/// # Safety
///
/// `previous_rsp` and `previous_on_cpu` must be valid for a write; `next_rsp` must be a stack
/// pointer saved by this function or prepared by [`create`], and nothing else may be using
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

/// Where a new task starts: passes the entry function (in r12, see `create`) to `task_start`.
#[unsafe(naked)]
extern "C" fn task_trampoline() -> ! {
    naked_asm!("mov rdi, r12", "call {start}", "ud2", start = sym task_start)
}

/// `entry` is the task's `fn() -> !` as an address: a Rust `fn` type cannot cross the
/// `extern "C"` boundary of the trampoline directly.
extern "C" fn task_start(entry: usize) -> ! {
    // SAFETY: `create` put a `fn() -> !` into r12, and the trampoline passes it on unchanged;
    // a `fn` pointer round-trips through `usize`.
    let entry = unsafe { core::mem::transmute::<usize, fn() -> !>(entry) };
    // A new task also arrives right after a switch.
    finish_switch();
    // The task was switched to with interrupts off (from `start`, `switch_to`, `exit_to`, or
    // a timer interrupt handler); a new task begins with them on.
    interrupts::enable();
    entry()
}
