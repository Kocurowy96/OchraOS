// SPDX-License-Identifier: GPL-2.0-only

//! The scheduler: which task runs next. Round-robin — every task gets one time slice (one
//! timer tick) in turn. The mechanism (task stacks, the context switch) is in
//! `ochra_frame::task`.

use alloc::collections::VecDeque;
use alloc::sync::Arc;

use ochra_frame::arch::interrupts;
use ochra_frame::sync::IrqMutex;
use ochra_frame::task::{self, Task};

/// Tasks waiting for the CPU, in the order they will get it. The running task is not here.
static RUN_QUEUE: IrqMutex<VecDeque<Arc<Task>>> = IrqMutex::new(VecDeque::new());

/// Creates a task and puts it at the end of the queue.
pub fn spawn(name: &'static str, entry: fn() -> !) {
    RUN_QUEUE.lock().push_back(task::spawn(name, entry));
}

/// Runs the first queued task; the boot code does not continue after this.
pub fn start() -> ! {
    let first = RUN_QUEUE.lock().pop_front().expect("no task to start");
    task::start(first)
}

/// Gives the CPU to the next task in the queue, if there is one. Called on every timer tick.
pub fn schedule() {
    // Interrupts stay off from looking at the current task until the switch: a timer tick in
    // between would queue the current task twice.
    let were_enabled = interrupts::disable();
    if let Some(current) = task::current() {
        let next = {
            let mut queue = RUN_QUEUE.lock();
            queue.pop_front().inspect(|_| queue.push_back(current))
        };
        // The queue lock is released before switching: the next task will need it.
        if let Some(next) = next {
            task::switch_to(next);
        }
    }
    if were_enabled {
        interrupts::enable();
    }
}
