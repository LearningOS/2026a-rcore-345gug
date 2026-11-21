//! Wait-for-graph deadlock detection for mutexes and semaphores.
//!
//! The lab handout models a worker thread as a philosopher and the two
//! players it is updating as chopsticks.  A blocking mutex / semaphore `down`
//! must therefore be able to answer the question "if I block now, is the
//! whole process doomed?" and return `-0xdead` instead of sleeping forever
//! when the answer is yes.
//!
//! Only *blocking* mutexes and semaphores participate.  Spin mutexes never
//! block a task, so they cannot be part of a deadlock cycle.
//!
//! # Algorithm
//!
//! We build the process' wait-for graph on demand:
//!
//! * a task that is not waiting for a mutex/semaphore can eventually run and
//!   release everything it holds;
//! * a task waiting for a resource can eventually proceed if **any** holder
//!   of that resource can proceed (that holder will release its unit);
//! * iterate this rule until a fixed point is reached.
//!
//! This is the standard resource-allocation-graph deadlock detector with
//! multiple resource instances.  It handles the `ch8_deadlock_sem2` case,
//! where two threads wait on each other but *other* holders of the same
//! counting semaphores are still running and will release their units: a
//! plain cycle search would report a false positive there.

use crate::task::{ProcessControlBlock, TaskControlBlock};
use alloc::{collections::BTreeSet, sync::Arc, vec::Vec};

/// Error code returned by `mutex_lock` / `semaphore_down` when blocking
/// would create a deadlock (`-0xdead` == `-57005`).
pub const DEADLOCK_ERR: i32 = -0xdead;

/// The two resource types that can block a task.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ResourceKind {
    /// A blocking mutex.
    Mutex,
    /// A counting semaphore.
    Semaphore,
}

/// Whether deadlock detection was enabled for the process owning `task`.
pub fn detect_enabled(task: &Arc<TaskControlBlock>) -> bool {
    match task.process.upgrade() {
        Some(process) => process.inner_exclusive_access().deadlock_detect,
        None => false,
    }
}

/// Snapshot the tasks currently holding `(kind, id)`.
fn resource_holders(
    process: &Arc<ProcessControlBlock>,
    kind: ResourceKind,
    id: usize,
) -> Vec<Arc<TaskControlBlock>> {
    let inner = process.inner_exclusive_access();
    match kind {
        ResourceKind::Mutex => inner
            .mutex_list
            .get(id)
            .and_then(|mutex| mutex.as_ref())
            .and_then(|mutex| mutex.owner())
            .into_iter()
            .collect(),
        ResourceKind::Semaphore => inner
            .semaphore_list
            .get(id)
            .and_then(|sem| sem.as_ref())
            .map(|sem| sem.holders())
            .unwrap_or_default(),
    }
}

#[inline]
fn task_key(task: &Arc<TaskControlBlock>) -> usize {
    Arc::as_ptr(task) as usize
}

/// Whether `(kind, id)` currently has a free unit that can satisfy a waiter.
///
/// A mutex has no notion of a free unit (it is either locked or unlocked; the
/// caller only asks while it is locked).  A semaphore with `count > 0` can
/// satisfy one waiter even before that waiter has been scheduled.
fn resource_has_free_unit(
    process: &Arc<ProcessControlBlock>,
    kind: ResourceKind,
    id: usize,
) -> bool {
    if kind != ResourceKind::Semaphore {
        return false;
    }
    let inner = process.inner_exclusive_access();
    inner
        .semaphore_list
        .get(id)
        .and_then(|sem| sem.as_ref())
        .map(|sem| sem.available() > 0)
        .unwrap_or(false)
}

/// Check whether `waiting_task` blocking on `(kind, id)` would deadlock.
///
/// The prospective wait edge of `waiting_task` is included in the graph even
/// though the task has not been queued yet; this is exactly the edge the
/// caller is about to add.
pub fn would_deadlock(waiting_task: &Arc<TaskControlBlock>, kind: ResourceKind, id: usize) -> bool {
    let process = match waiting_task.process.upgrade() {
        Some(process) => process,
        None => return false,
    };
    // Copy the task list out of the process lock; the detection below walks
    // task wait edges and resource owners, and must not hold the process
    // inner borrow while doing so.
    let tasks: Vec<Arc<TaskControlBlock>> = {
        let inner = process.inner_exclusive_access();
        inner
            .tasks
            .iter()
            .filter_map(|task| task.as_ref().map(Arc::clone))
            .collect()
    };

    // (task, current/prospective wait edge)
    let mut waits: Vec<(Arc<TaskControlBlock>, Option<(ResourceKind, usize)>)> =
        Vec::with_capacity(tasks.len());
    for task in tasks {
        let wait = if Arc::ptr_eq(&task, waiting_task) {
            Some((kind, id))
        } else {
            task.inner_exclusive_access().waiting_for
        };
        waits.push((task, wait));
    }

    // Fixed point of "can this task eventually make progress?".
    let mut can_proceed: BTreeSet<usize> = BTreeSet::new();
    for (task, wait) in waits.iter() {
        if wait.is_none() {
            can_proceed.insert(task_key(task));
        }
    }
    loop {
        let mut changed = false;
        for (task, wait) in waits.iter() {
            let key = task_key(task);
            if can_proceed.contains(&key) {
                continue;
            }
            if let Some((kind, id)) = wait {
                // A free unit, or a single holder that can proceed, is
                // enough: one of them will let us through.
                let satisfied = resource_has_free_unit(&process, *kind, *id)
                    || resource_holders(&process, *kind, *id)
                        .iter()
                        .any(|holder| can_proceed.contains(&task_key(holder)));
                if satisfied {
                    can_proceed.insert(key);
                    changed = true;
                }
            }
        }
        if !changed {
            break;
        }
    }

    // If the waiting task itself cannot eventually proceed, adding this edge
    // closes a deadlock.
    !can_proceed.contains(&task_key(waiting_task))
}
