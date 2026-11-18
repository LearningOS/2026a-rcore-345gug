//! A simple spin lock used to serialize kernel execution on multiple harts.
use core::sync::atomic::{AtomicBool, Ordering};

/// spin lock based on an atomic flag
pub struct SpinLock {
    locked: AtomicBool,
}

impl SpinLock {
    /// create an unlocked spin lock
    pub const fn new() -> Self {
        Self {
            locked: AtomicBool::new(false),
        }
    }
    /// acquire the lock, spinning until it is free
    pub fn lock(&self) {
        while self
            .locked
            .compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            core::hint::spin_loop();
        }
    }
    /// release the lock
    pub fn unlock(&self) {
        self.locked.store(false, Ordering::Release);
    }
}

/// big kernel lock: only one hart executes in the kernel at a time
pub static BIG_KERNEL_LOCK: SpinLock = SpinLock::new();
