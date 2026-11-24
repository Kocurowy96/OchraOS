// SPDX-License-Identifier: GPL-2.0-only

//! The scheduler: which task runs next, and tasks that wait (block) and end. Round-robin —
//! every task gets one time slice (one timer tick) in turn. All CPUs share one run queue, so
//! a task may continue on a different CPU after every tick. The mechanism (task stacks, the
//! context switch, making sure two CPUs never run the same task) is in `ochra_frame::task`.
//!
//! Every task has a [`Thread`] record with its scheduling state. All state changes happen
//! under the run queue lock, so they are seen in one order by every CPU:
//!
//! ```text
//!            schedule()               wait queue: prepare_sleep()      block()
//!   Ready <-------------> Running ------------------------> Sleeping ---------> Blocked
//!     ^       picked                ^   wake() before block()  |                    |
//!     |                             +--------------------------+                    |
//!     +---------------------------------- wake(): back into the queue --------------+
//! ```
//!
//! "Sleeping" means: about to block, but still running. A wake-up that comes in that window
//! (from another CPU, or an interrupt) simply cancels the sleep, so it is never lost. Only a
//! "Blocked" task is off the CPU and out of the queue; waking it puts it back in the queue,
//! and `task::switch_to` makes the next CPU wait until the old one has really left it.

use alloc::collections::VecDeque;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU8, Ordering};

use ochra_frame::arch::interrupts;
use ochra_frame::mm::user::UserSpace;
use ochra_frame::sync::{IrqMutex, TryMutex, TryMutexGuard};
use ochra_frame::task::{self, Task};

use crate::errno::{ERESTARTSYS, Errno};
use crate::process::Process;

/// The kernel's record of a task.
pub struct Thread {
    /// One of the `state` constants; changed only under the `RUN_QUEUE` lock.
    state: AtomicU8,
    /// The process the task belongs to (user tasks; `None` for kernel tasks).
    pub process: Option<Arc<Process>>,
}

mod state {
    pub const RUNNING: u8 = 0;
    pub const READY: u8 = 1;
    pub const SLEEPING: u8 = 2;
    pub const BLOCKED: u8 = 3;
    pub const EXITED: u8 = 4;
}

impl Thread {
    fn new(process: Option<Arc<Process>>) -> Arc<Self> {
        Arc::new(Self {
            // Becomes `READY` when queued.
            state: AtomicU8::new(state::READY),
            process,
        })
    }
}

/// The scheduler's record of `task`.
pub fn thread(task: &Task) -> &Thread {
    task.data()
        .and_then(|data| data.downcast_ref::<Thread>())
        .expect("every task is created by the scheduler, with a Thread")
}

/// Tasks waiting for the CPU, in the order they will get it. The running task is not here.
static RUN_QUEUE: IrqMutex<VecDeque<Arc<Task>>> = IrqMutex::new(VecDeque::new());

/// Puts a new task at the end of the queue.
fn enqueue(task: Arc<Task>) {
    RUN_QUEUE.lock().push_back(task);
}

/// Creates a kernel task and puts it at the end of the queue.
pub fn spawn(name: &'static str, entry: fn() -> !) {
    enqueue(task::spawn(name, entry, Some(Thread::new(None))));
}

/// Creates a user task (see `task::spawn_user`) of `process` and queues it.
pub fn spawn_user(
    name: &'static str,
    space: Arc<UserSpace>,
    entry: u64,
    stack: u64,
    process: Option<Arc<Process>>,
) -> Arc<Task> {
    let task = task::spawn_user(name, space, entry, stack, Some(Thread::new(process)));
    enqueue(task.clone());
    task
}

/// Creates the task of a forked process (see `task::fork_user`) and queues it.
pub fn spawn_fork(
    space: Arc<UserSpace>,
    context: &ochra_frame::user_mode::UserContext,
    process: Arc<Process>,
) -> Arc<Task> {
    let task = task::fork_user("user", space, context, Some(Thread::new(Some(process))));
    enqueue(task.clone());
    task
}

/// Set by the boot CPU once the first tasks are queued; the other CPUs wait for it.
static READY: AtomicBool = AtomicBool::new(false);

/// Starts scheduling on this CPU: runs the first queued task, and the caller's code never
/// continues. The boot CPU calls it once the first tasks are queued; the other CPUs call it
/// right away and wait (taking interrupts) until the boot CPU gets here.
pub fn start(boot_cpu: bool) -> ! {
    if boot_cpu {
        READY.store(true, Ordering::Release);
    }
    while !READY.load(Ordering::Acquire) {
        interrupts::wait();
    }
    // Interrupts off until the task runs: a tick in between would find no current task and
    // do nothing, but the popped task must not wait outside the queue for long.
    interrupts::disable();
    let first =
        pick(&mut RUN_QUEUE.lock()).expect("fewer tasks than CPUs (every CPU needs an idle task)");
    task::start(first)
}

/// Takes the next task from the queue and marks it running.
fn pick(queue: &mut VecDeque<Arc<Task>>) -> Option<Arc<Task>> {
    let next = queue.pop_front()?;
    thread(&next).state.store(state::RUNNING, Ordering::Relaxed);
    Some(next)
}

/// Like [`pick`], when the current task cannot go on: there is always a task, because idle
/// tasks (one per CPU) never block, so at least one of them waits in the queue.
fn pick_any(queue: &mut VecDeque<Arc<Task>>) -> Arc<Task> {
    pick(queue).expect("the run queue holds an idle task")
}

/// Gives the CPU to the next task in the queue, if there is one. Called on every timer tick,
/// and by `sched_yield`.
pub fn schedule() {
    // Interrupts stay off from looking at the current task until the switch: a timer tick in
    // between would queue the current task twice.
    let were_enabled = interrupts::disable();
    if let Some(current) = task::current() {
        let next = {
            let mut queue = RUN_QUEUE.lock();
            let thread = thread(&current);
            match thread.state.load(Ordering::Relaxed) {
                state::RUNNING => pick(&mut queue).inspect(|_| {
                    thread.state.store(state::READY, Ordering::Relaxed);
                    queue.push_back(current.clone());
                }),
                // Preempted on its way to block: block now.
                state::SLEEPING => {
                    thread.state.store(state::BLOCKED, Ordering::Relaxed);
                    Some(pick_any(&mut queue))
                }
                _ => None,
            }
        };
        drop(current);
        // The queue lock is released before switching: the next task will need it.
        if let Some(next) = next {
            task::switch_to(next);
        }
    }
    if were_enabled {
        interrupts::enable();
    }
}

/// The current task is about to block: from now on a [`wake`] cancels the block.
fn prepare_sleep(current: &Task) {
    let _queue = RUN_QUEUE.lock();
    thread(current)
        .state
        .store(state::SLEEPING, Ordering::Relaxed);
}

/// The reason to block went away before blocking: keep running.
fn cancel_sleep(current: &Task) {
    let _queue = RUN_QUEUE.lock();
    let thread = thread(current);
    if thread.state.load(Ordering::Relaxed) == state::SLEEPING {
        thread.state.store(state::RUNNING, Ordering::Relaxed);
    }
}

/// Leaves the CPU until [`wake`], unless a wake-up already came since [`prepare_sleep`].
fn block(current: &Task) {
    let were_enabled = interrupts::disable();
    let next = {
        let mut queue = RUN_QUEUE.lock();
        let thread = thread(current);
        (thread.state.load(Ordering::Relaxed) == state::SLEEPING).then(|| {
            thread.state.store(state::BLOCKED, Ordering::Relaxed);
            pick_any(&mut queue)
        })
    };
    if let Some(next) = next {
        task::switch_to(next);
    }
    if were_enabled {
        interrupts::enable();
    }
}

/// Makes `task` run again if it sleeps or is blocked. May be called from any CPU and from
/// interrupt handlers.
pub fn wake(task: &Arc<Task>) {
    let mut queue = RUN_QUEUE.lock();
    let thread = thread(task);
    match thread.state.load(Ordering::Relaxed) {
        state::SLEEPING => thread.state.store(state::RUNNING, Ordering::Relaxed),
        state::BLOCKED => {
            thread.state.store(state::READY, Ordering::Relaxed);
            queue.push_back(task.clone());
        }
        _ => {}
    }
}

/// Ends the current task: it never runs again, and is freed once it has left the CPU.
pub fn exit_current() -> ! {
    interrupts::disable();
    let current = task::current().expect("a task is running");
    let next = {
        let mut queue = RUN_QUEUE.lock();
        thread(&current)
            .state
            .store(state::EXITED, Ordering::Relaxed);
        pick_any(&mut queue)
    };
    drop(current);
    task::exit_to(next)
}

/// Tasks waiting for something (data in a pipe, a child to exit…).
///
/// The waiter checks its condition, registers, marks itself sleeping, and checks again
/// before it blocks; whoever makes the condition true does so *before* calling
/// [`WaitQueue::wake_all`]. So a wake-up can never fall between the check and the block.
pub struct WaitQueue {
    waiters: IrqMutex<Vec<Arc<Task>>>,
}

impl WaitQueue {
    pub const fn new() -> Self {
        Self {
            waiters: IrqMutex::new(Vec::new()),
        }
    }

    /// Blocks until `ready` returns `Some`, and returns that. `ready` runs again after every
    /// wake-up (which may be for someone else's reason).
    pub fn wait_until<T>(&self, mut ready: impl FnMut() -> Option<T>) -> T {
        loop {
            if let Some(value) = ready() {
                return value;
            }
            let current = task::current().expect("only tasks wait");
            self.waiters.lock().push(current.clone());
            prepare_sleep(&current);
            if let Some(value) = ready() {
                cancel_sleep(&current);
                self.remove(&current);
                return value;
            }
            block(&current);
            self.remove(&current);
        }
    }

    /// Like [`wait_until`](Self::wait_until), but gives up with `ERESTARTSYS` when a signal
    /// is to be delivered to the current process: the syscall then ends (and may be run
    /// again, see `signal::on_return`). `signal::send` wakes the task itself, so a signal
    /// that comes while it waits is never missed.
    pub fn wait_interruptible<T>(&self, mut ready: impl FnMut() -> Option<T>) -> Result<T, Errno> {
        self.wait_until(|| match ready() {
            Some(value) => Some(Ok(value)),
            None if crate::signal::interrupted() => Some(Err(ERESTARTSYS)),
            None => None,
        })
    }

    fn register(&self, task: &Arc<Task>) {
        self.waiters.lock().push(task.clone());
    }

    /// Wakes every waiter.
    pub fn wake_all(&self) {
        let waiters = core::mem::take(&mut *self.waiters.lock());
        for waiter in &waiters {
            wake(waiter);
        }
    }

    fn remove(&self, task: &Arc<Task>) {
        self.waiters
            .lock()
            .retain(|waiter| !Arc::ptr_eq(waiter, task));
    }
}

/// Blocks until `ready` returns `Some`, waiting in all of `queues` at once (`poll`, a read
/// with a timeout): a wake-up from any of them makes `ready` run again.
///
/// The task is registered in every queue *before* `ready` runs, so an event that comes
/// while the registration goes on is never lost: either `ready` sees it, or the event's
/// `wake_all` finds the task in its queue and cancels the sleep.
pub fn wait_any<T>(queues: &[&WaitQueue], mut ready: impl FnMut() -> Option<T>) -> T {
    let current = task::current().expect("only tasks wait");
    loop {
        for queue in queues {
            queue.register(&current);
        }
        prepare_sleep(&current);
        let value = ready();
        if value.is_some() {
            cancel_sleep(&current);
        } else {
            block(&current);
        }
        for queue in queues {
            queue.remove(&current);
        }
        if let Some(value) = value {
            return value;
        }
    }
}

/// Like [`wait_any`], but gives up with `ERESTARTSYS` when a signal is to be delivered.
pub fn wait_any_interruptible<T>(
    queues: &[&WaitQueue],
    mut ready: impl FnMut() -> Option<T>,
) -> Result<T, Errno> {
    wait_any(queues, || match ready() {
        Some(value) => Some(Ok(value)),
        None if crate::signal::interrupted() => Some(Err(ERESTARTSYS)),
        None => None,
    })
}

/// A lock for work that may block (disk I/O, waiting for a page): a task that finds it taken
/// sleeps until it is free, instead of spinning with interrupts off as `IrqMutex` does. Only
/// for tasks, never in interrupt handlers.
pub struct Mutex<T> {
    inner: TryMutex<T>,
    /// Tasks waiting for the lock; woken when it is released.
    waiters: WaitQueue,
}

impl<T> Mutex<T> {
    pub const fn new(value: T) -> Self {
        Self {
            inner: TryMutex::new(value),
            waiters: WaitQueue::new(),
        }
    }

    pub fn lock(&self) -> MutexGuard<'_, T> {
        let guard = self.waiters.wait_until(|| self.inner.try_lock());
        MutexGuard {
            guard: Some(guard),
            waiters: &self.waiters,
        }
    }
}

/// Access to the data of a locked [`Mutex`]; unlocking (and waking the waiters) happens on
/// drop.
pub struct MutexGuard<'a, T> {
    /// Always `Some` until dropped.
    guard: Option<TryMutexGuard<'a, T>>,
    waiters: &'a WaitQueue,
}

impl<T> core::ops::Deref for MutexGuard<'_, T> {
    type Target = T;

    fn deref(&self) -> &T {
        self.guard.as_ref().expect("held until dropped")
    }
}

impl<T> core::ops::DerefMut for MutexGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        self.guard.as_mut().expect("held until dropped")
    }
}

impl<T> Drop for MutexGuard<'_, T> {
    fn drop(&mut self) {
        // Unlock first, then wake: a woken task must find the lock free.
        self.guard = None;
        self.waiters.wake_all();
    }
}
