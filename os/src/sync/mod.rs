//! Synchronization and interior mutability primitives

mod spin;
mod up;

pub use spin::{SpinLock, BIG_KERNEL_LOCK};
pub use up::UPSafeCell;
