//! Semaphore

use super::deadlock::{detect_enabled, would_deadlock, ResourceKind, DEADLOCK_ERR};
use crate::sync::UPSafeCell;
use crate::task::{block_current_and_run_next, current_task, wakeup_task, TaskControlBlock};
use alloc::{collections::VecDeque, sync::Arc, vec::Vec};

/// semaphore structure
pub struct Semaphore {
    /// resource id in the owning process' `semaphore_list`, used by the
    /// wait-for graph
    id: usize,
    /// semaphore inner
    pub inner: UPSafeCell<SemaphoreInner>,
}

pub struct SemaphoreInner {
    /// number of available permits (never negative in this implementation)
    pub count: isize,
    pub wait_queue: VecDeque<Arc<TaskControlBlock>>,
    /// Tasks that successfully acquired one permit and have not released it
    /// yet.  A task may appear multiple times if it called `down` several
    /// times (e.g. the barrier in the deadlock tests).  Only used by the
    /// deadlock detector.
    holders: Vec<Arc<TaskControlBlock>>,
}

impl Semaphore {
    /// Create a new semaphore with its id in `semaphore_list`
    pub fn new(res_count: usize, id: usize) -> Self {
        trace!("kernel: Semaphore::new");
        Self {
            id,
            inner: unsafe {
                UPSafeCell::new(SemaphoreInner {
                    count: res_count as isize,
                    wait_queue: VecDeque::new(),
                    holders: Vec::new(),
                })
            },
        }
    }

    /// Snapshot the tasks currently holding a permit (for deadlock detection).
    pub fn holders(&self) -> Vec<Arc<TaskControlBlock>> {
        self.inner.exclusive_access().holders.clone()
    }

    /// Number of free permits.  A task woken by `up` may still be marked as
    /// waiting until it is scheduled, so the detector treats a positive count
    /// as "this wait can be satisfied".
    pub fn available(&self) -> isize {
        self.inner.exclusive_access().count
    }

    /// up operation of semaphore
    pub fn up(&self) {
        trace!("kernel: Semaphore::up");
        let task = current_task().unwrap();
        let mut inner = self.inner.exclusive_access();
        // Release one permit held by the current task, if it has one.  An
        // `up` without a matching `down` (e.g. a producer signalling a
        // consumer) simply adds a permit and has no holder to remove.
        if let Some(pos) = inner
            .holders
            .iter()
            .rposition(|holder| Arc::ptr_eq(holder, &task))
        {
            inner.holders.remove(pos);
        }
        inner.count += 1;
        // Wake one waiter; it will re-check `count` and take the permit.
        if let Some(task) = inner.wait_queue.pop_front() {
            wakeup_task(task);
        }
    }

    /// down operation of semaphore
    ///
    /// Returns 0 after acquiring a permit, or [`DEADLOCK_ERR`] (`-0xdead`)
    /// when deadlock detection is enabled and this wait would deadlock.
    pub fn down(&self) -> i32 {
        trace!("kernel: Semaphore::down");
        let task = current_task().unwrap();
        loop {
            let mut inner = self.inner.exclusive_access();
            if inner.count > 0 {
                inner.count -= 1;
                inner.holders.push(Arc::clone(&task));
                return 0;
            }
            drop(inner);

            // No permit available.  Check the wait-for graph before sleeping.
            // As with mutexes, do not hold this semaphore's borrow while the
            // detector walks the graph (it may come back to this resource).
            if detect_enabled(&task) && would_deadlock(&task, ResourceKind::Semaphore, self.id) {
                trace!("kernel: Semaphore::down deadlock detected");
                return DEADLOCK_ERR;
            }

            let mut inner = self.inner.exclusive_access();
            if inner.count > 0 {
                continue;
            }
            task.inner_exclusive_access().waiting_for = Some((ResourceKind::Semaphore, self.id));
            inner.wait_queue.push_back(Arc::clone(&task));
            drop(inner);
            block_current_and_run_next();
            task.inner_exclusive_access().waiting_for = None;
            // Loop and race for the permit (another waiter may have taken it
            // in the meantime; blocking again is correct).
        }
    }
}
