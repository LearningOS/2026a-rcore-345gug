//!Implementation of [`TaskManager`]
use super::TaskControlBlock;
use crate::sync::UPSafeCell;
use alloc::collections::VecDeque;
use alloc::sync::Arc;
use lazy_static::*;
/// big stride used by the stride scheduler
const BIG_STRIDE: usize = 10000;

///A array of `TaskControlBlock` that is thread-safe
pub struct TaskManager {
    ready_queue: VecDeque<Arc<TaskControlBlock>>,
}

/// A stride scheduler: the task with the smallest pass value runs next.
impl TaskManager {
    ///Creat an empty TaskManager
    pub fn new() -> Self {
        Self {
            ready_queue: VecDeque::new(),
        }
    }
    /// Add process back to ready queue, advancing its stride pass
    pub fn add(&mut self, task: Arc<TaskControlBlock>) {
        {
            let mut inner = task.inner_exclusive_access();
            inner.pass += BIG_STRIDE / inner.priority;
        }
        self.ready_queue.push_back(task);
    }
    /// Take the process with the smallest pass value out of the ready queue
    pub fn fetch(&mut self) -> Option<Arc<TaskControlBlock>> {
        let mut chosen: Option<usize> = None;
        let mut min_pass = usize::MAX;
        for (i, task) in self.ready_queue.iter().enumerate() {
            let pass = task.inner_exclusive_access().pass;
            if pass < min_pass {
                min_pass = pass;
                chosen = Some(i);
            }
        }
        chosen.and_then(|i| self.ready_queue.remove(i))
    }
}

lazy_static! {
    /// TASK_MANAGER instance through lazy_static!
    pub static ref TASK_MANAGER: UPSafeCell<TaskManager> =
        unsafe { UPSafeCell::new(TaskManager::new()) };
}

/// Add process to ready queue
pub fn add_task(task: Arc<TaskControlBlock>) {
    //trace!("kernel: TaskManager::add_task");
    TASK_MANAGER.exclusive_access().add(task);
}

/// Take a process out of the ready queue
pub fn fetch_task() -> Option<Arc<TaskControlBlock>> {
    //trace!("kernel: TaskManager::fetch_task");
    TASK_MANAGER.exclusive_access().fetch()
}
