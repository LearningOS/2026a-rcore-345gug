//! File trait & inode(dir, file, pipe, stdin, stdout)

mod eventfd;
mod inode;
mod pipe;
mod stdio;

use crate::mm::UserBuffer;

/// trait File for all file types
///
/// NOTE: `read`/`write` return `isize` instead of `usize` so that a concrete
/// file (e.g. `EventFd`) can report a negative errno (`-1`/`-2`) through the
/// same interface.  The return value is the number of bytes transferred on
/// success.
pub trait File: Send + Sync {
    /// the file readable?
    fn readable(&self) -> bool;
    /// the file writable?
    fn writable(&self) -> bool;
    /// read from the file to buf, return the number of bytes read
    fn read(&self, buf: UserBuffer) -> isize;
    /// write to the file from buf, return the number of bytes written
    fn write(&self, buf: UserBuffer) -> isize;
}

/// The stat of a inode
#[repr(C)]
#[derive(Debug)]
pub struct Stat {
    /// ID of device containing file
    pub dev: u64,
    /// inode number
    pub ino: u64,
    /// file type and mode
    pub mode: StatMode,
    /// number of hard links
    pub nlink: u32,
    /// unused pad
    pad: [u64; 7],
}

bitflags! {
    /// The mode of a inode
    /// whether a directory or a file
    pub struct StatMode: u32 {
        /// null
        const NULL  = 0;
        /// directory
        const DIR   = 0o040000;
        /// ordinary regular file
        const FILE  = 0o100000;
    }
}

pub use eventfd::{create_eventfd, EventFd, EFD_NONBLOCK, EFD_SEMAPHORE};
pub use inode::{list_apps, open_file, OSInode, OpenFlags};
pub use pipe::{make_pipe, Pipe};
pub use stdio::{Stdin, Stdout};
