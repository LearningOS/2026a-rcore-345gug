//! `eventfd` file implementation (syscall 290 / `sys_eventfd2`).
//!
//! An eventfd is simply a 64-bit unsigned counter plus a read wait queue and a
//! write wait queue.  It is a `File`, therefore it lives in the process' fd
//! table and is inherited across `fork` (the `Arc` is cloned, so parent and
//! child share the *same* counter).
//!
//! Semantics implemented here (see the lab handout):
//! * normal mode: a non-zero counter is read out and cleared atomically;
//! * `EFD_SEMAPHORE` mode: a read returns the value 1 and decrements the
//!   counter by one, which makes eventfd usable as a counting semaphore;
//! * `EFD_NONBLOCK`: a read on an empty counter / a write that would overflow
//!   returns `-2` instead of blocking;
//! * a write must carry exactly 8 bytes (`u64`, little endian on RISC-V).
//!
//! The counter and the two wait queues are protected by one `UPSafeCell`, so
//! checking the condition and changing the counter happens atomically under
//! the uniprocessor kernel's non-preemptive rules.

use super::File;
use crate::mm::UserBuffer;
use crate::sync::UPSafeCell;
use crate::task::{block_current_and_run_next, current_task, wakeup_task, TaskControlBlock};
use alloc::collections::VecDeque;
use alloc::sync::Arc;

/// Create a signal-semaphore-style eventfd (`EFD_SEMAPHORE`).
pub const EFD_SEMAPHORE: i32 = 1;
/// Create a non-blocking eventfd (`EFD_NONBLOCK`).
pub const EFD_NONBLOCK: i32 = 0x800; // 2048

const EFD_VALID_FLAGS: i32 = EFD_SEMAPHORE | EFD_NONBLOCK;

struct EventFdInner {
    /// 64-bit counter.
    count: u64,
    /// Tasks blocked in `read` because the counter was zero.
    read_queue: VecDeque<Arc<TaskControlBlock>>,
    /// Tasks blocked in `write` because the counter would overflow.
    write_queue: VecDeque<Arc<TaskControlBlock>>,
}

/// An eventfd object.
pub struct EventFd {
    /// whether `EFD_SEMAPHORE` was requested
    semaphore: bool,
    /// whether `EFD_NONBLOCK` was requested
    nonblock: bool,
    inner: UPSafeCell<EventFdInner>,
}

impl EventFd {
    fn new(initval: u32, semaphore: bool, nonblock: bool) -> Self {
        Self {
            semaphore,
            nonblock,
            inner: unsafe {
                UPSafeCell::new(EventFdInner {
                    count: initval as u64,
                    read_queue: VecDeque::new(),
                    write_queue: VecDeque::new(),
                })
            },
        }
    }
}

/// Validate `flags` and create an `EventFd`.
///
/// Returns `None` when the flag word contains bits other than
/// `EFD_SEMAPHORE | EFD_NONBLOCK`, in which case the syscall must return -1.
pub fn create_eventfd(initval: u32, flags: i32) -> Option<Arc<EventFd>> {
    // Reject negative flag words and unknown bits.
    if flags < 0 || (flags & !EFD_VALID_FLAGS) != 0 {
        return None;
    }
    Some(Arc::new(EventFd::new(
        initval,
        flags & EFD_SEMAPHORE != 0,
        flags & EFD_NONBLOCK != 0,
    )))
}

/// Copy the first 8 bytes of a user buffer into a `u64` (little endian).
fn read_u64(buf: &UserBuffer) -> u64 {
    let mut value = 0u64;
    // `UserBuffer` may be split across pages, so walk its byte slices.
    for (i, byte) in buf.buffers.iter().flat_map(|s| s.iter()).enumerate() {
        value |= (*byte as u64) << (8 * i);
    }
    value
}

/// Store `value` into the 8-byte user buffer (little endian).
fn write_u64(buf: UserBuffer, value: u64) {
    for (i, ptr) in buf.into_iter().enumerate() {
        unsafe {
            *ptr = (value >> (8 * i)) as u8;
        }
    }
}

impl File for EventFd {
    fn readable(&self) -> bool {
        true
    }

    fn writable(&self) -> bool {
        true
    }

    /// Read the counter.
    ///
    /// * normal mode: returns the old counter value and clears it;
    /// * semaphore mode: returns 1 and decrements the counter;
    /// * empty counter + `EFD_NONBLOCK`: returns -2;
    /// * empty counter without `EFD_NONBLOCK`: blocks until a writer wakes us.
    ///
    /// The syscall layer reports the returned byte count (8) to user space.
    fn read(&self, buf: UserBuffer) -> isize {
        // Linux eventfd requires an 8-byte buffer.  The handout states the
        // same requirement for writes; enforcing it for reads keeps the
        // kernel from silently truncating a user value.
        if buf.len() != core::mem::size_of::<u64>() {
            return -1;
        }
        loop {
            let task = current_task().unwrap();
            let mut inner = self.inner.exclusive_access();
            if inner.count > 0 {
                let value = if self.semaphore {
                    // Semaphore mode: consume exactly one "token".
                    inner.count -= 1;
                    1u64
                } else {
                    // Normal mode: read-and-clear.
                    let value = inner.count;
                    inner.count = 0;
                    value
                };
                // The counter decreased, so writers blocked on overflow may
                // now make progress.  Wake all of them; each re-checks the
                // counter under the lock after being scheduled.
                while let Some(waiter) = inner.write_queue.pop_front() {
                    wakeup_task(waiter);
                }
                drop(inner);
                write_u64(buf, value);
                return core::mem::size_of::<u64>() as isize;
            }
            drop(inner);
            if self.nonblock {
                return -2;
            }
            // Block until a writer adds a value.  Re-check after waking: the
            // value may have been consumed by another reader in the meantime.
            let mut inner = self.inner.exclusive_access();
            if inner.count > 0 {
                continue;
            }
            inner.read_queue.push_back(task);
            drop(inner);
            block_current_and_run_next();
        }
    }

    /// Add the 8-byte value in `buf` to the counter.
    ///
    /// * overflow + `EFD_NONBLOCK`: returns -2;
    /// * overflow without `EFD_NONBLOCK`: blocks until a reader frees space;
    /// * wrong buffer length: returns -1.
    fn write(&self, buf: UserBuffer) -> isize {
        if buf.len() != core::mem::size_of::<u64>() {
            return -1;
        }
        let value = read_u64(&buf);
        loop {
            let task = current_task().unwrap();
            let mut inner = self.inner.exclusive_access();
            if let Some(new_count) = inner.count.checked_add(value) {
                let was_zero = inner.count == 0;
                inner.count = new_count;
                // A zero -> non-zero transition may unblock readers.
                if was_zero && new_count > 0 {
                    while let Some(waiter) = inner.read_queue.pop_front() {
                        wakeup_task(waiter);
                    }
                }
                drop(inner);
                return core::mem::size_of::<u64>() as isize;
            }
            drop(inner);
            if self.nonblock {
                return -2;
            }
            // The 64-bit counter would wrap; wait for a read to reduce it.
            let mut inner = self.inner.exclusive_access();
            if inner.count.checked_add(value).is_some() {
                continue;
            }
            inner.write_queue.push_back(task);
            drop(inner);
            block_current_and_run_next();
        }
    }
}
