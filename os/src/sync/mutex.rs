//! Mutex (spin-like and blocking(sleep))

use super::deadlock::{detect_enabled, would_deadlock, ResourceKind, DEADLOCK_ERR};
use super::UPSafeCell;
use crate::task::TaskControlBlock;
use crate::task::{block_current_and_run_next, suspend_current_and_run_next};
use crate::task::{current_task, wakeup_task};
use alloc::{collections::VecDeque, sync::Arc};

/// Mutex trait
pub trait Mutex: Sync + Send {
    /// Lock the mutex.
    ///
    /// Returns 0 on success.  A blocking mutex returns [`DEADLOCK_ERR`]
    /// (`-0xdead`) when deadlock detection is enabled and locking would
    /// deadlock; spin mutexes never block and always return 0.
    fn lock(&self) -> i32;
    /// Unlock the mutex
    fn unlock(&self);
    /// The task currently owning the mutex, if any.  Used by the deadlock
    /// detector; spin mutexes have no owner bookkeeping.
    fn owner(&self) -> Option<Arc<TaskControlBlock>> {
        None
    }
}

/// Spinlock Mutex struct
pub struct MutexSpin {
    locked: UPSafeCell<bool>,
}

impl MutexSpin {
    /// Create a new spinlock mutex
    pub fn new() -> Self {
        Self {
            locked: unsafe { UPSafeCell::new(false) },
        }
    }
}

impl Mutex for MutexSpin {
    /// Lock the spinlock mutex
    fn lock(&self) -> i32 {
        trace!("kernel: MutexSpin::lock");
        loop {
            let mut locked = self.locked.exclusive_access();
            if *locked {
                drop(locked);
                suspend_current_and_run_next();
                continue;
            } else {
                *locked = true;
                return 0;
            }
        }
    }

    fn unlock(&self) {
        trace!("kernel: MutexSpin::unlock");
        let mut locked = self.locked.exclusive_access();
        *locked = false;
    }
}

/// Blocking Mutex struct
pub struct MutexBlocking {
    /// resource id in the owning process' `mutex_list`, used by the wait-for
    /// graph
    id: usize,
    inner: UPSafeCell<MutexBlockingInner>,
}

pub struct MutexBlockingInner {
    locked: bool,
    /// Task holding the mutex.  Unlike a raw `locked` flag this lets the
    /// deadlock detector know who blocks whom.
    owner: Option<Arc<TaskControlBlock>>,
    wait_queue: VecDeque<Arc<TaskControlBlock>>,
}

impl MutexBlocking {
    /// Create a new blocking mutex with its id in `mutex_list`
    pub fn new(id: usize) -> Self {
        trace!("kernel: MutexBlocking::new");
        Self {
            id,
            inner: unsafe {
                UPSafeCell::new(MutexBlockingInner {
                    locked: false,
                    owner: None,
                    wait_queue: VecDeque::new(),
                })
            },
        }
    }
}

impl Mutex for MutexBlocking {
    /// lock the blocking mutex
    fn lock(&self) -> i32 {
        trace!("kernel: MutexBlocking::lock");
        let task = current_task().unwrap();
        loop {
            let mut mutex_inner = self.inner.exclusive_access();
            if !mutex_inner.locked {
                mutex_inner.locked = true;
                mutex_inner.owner = Some(Arc::clone(&task));
                return 0;
            }
            drop(mutex_inner);

            // The mutex is held.  Before sleeping, ask the detector whether
            // this new wait edge would close a cycle.  Detection must run
            // without holding this mutex' borrow: it may visit this very
            // mutex while walking the graph.
            if detect_enabled(&task) && would_deadlock(&task, ResourceKind::Mutex, self.id) {
                trace!("kernel: MutexBlocking::lock deadlock detected");
                return DEADLOCK_ERR;
            }

            let mut mutex_inner = self.inner.exclusive_access();
            // Re-check: it may have been unlocked while detection ran.
            if !mutex_inner.locked {
                continue;
            }
            task.inner_exclusive_access().waiting_for = Some((ResourceKind::Mutex, self.id));
            mutex_inner.wait_queue.push_back(Arc::clone(&task));
            drop(mutex_inner);
            block_current_and_run_next();
            // Woken by unlock (or by a signal); the wait edge is gone now.
            task.inner_exclusive_access().waiting_for = None;
        }
    }

    /// unlock the blocking mutex
    fn unlock(&self) {
        trace!("kernel: MutexBlocking::unlock");
        let mut mutex_inner = self.inner.exclusive_access();
        mutex_inner.locked = false;
        mutex_inner.owner = None;
        // Wake one waiter.  Others (if any) stay queued and will be woken by
        // the next unlock.
        if let Some(waking_task) = mutex_inner.wait_queue.pop_front() {
            wakeup_task(waking_task);
        }
    }

    fn owner(&self) -> Option<Arc<TaskControlBlock>> {
        self.inner.exclusive_access().owner.clone()
    }
}
