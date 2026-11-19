//! File and filesystem-related syscalls
use crate::fs::{open_file, OpenFlags, Stat, ROOT_INODE};
use crate::mm::{translated_byte_buffer, translated_str, UserBuffer};
use crate::task::{current_task, current_user_token};

pub fn sys_write(fd: usize, buf: *const u8, len: usize) -> isize {
    trace!("kernel:pid[{}] sys_write", current_task().unwrap().pid.0);
    let token = current_user_token();
    let task = current_task().unwrap();
    let inner = task.inner_exclusive_access();
    if fd >= inner.fd_table.len() {
        return -1;
    }
    if let Some(file) = &inner.fd_table[fd] {
        if !file.writable() {
            return -1;
        }
        let file = file.clone();
        // release current task TCB manually to avoid multi-borrow
        drop(inner);
        file.write(UserBuffer::new(translated_byte_buffer(token, buf, len))) as isize
    } else {
        -1
    }
}

pub fn sys_read(fd: usize, buf: *const u8, len: usize) -> isize {
    trace!("kernel:pid[{}] sys_read", current_task().unwrap().pid.0);
    let token = current_user_token();
    let task = current_task().unwrap();
    let inner = task.inner_exclusive_access();
    if fd >= inner.fd_table.len() {
        return -1;
    }
    if let Some(file) = &inner.fd_table[fd] {
        let file = file.clone();
        if !file.readable() {
            return -1;
        }
        // release current task TCB manually to avoid multi-borrow
        drop(inner);
        trace!("kernel: sys_read .. file.read");
        file.read(UserBuffer::new(translated_byte_buffer(token, buf, len))) as isize
    } else {
        -1
    }
}

pub fn sys_open(path: *const u8, flags: u32) -> isize {
    trace!("kernel:pid[{}] sys_open", current_task().unwrap().pid.0);
    let task = current_task().unwrap();
    let token = current_user_token();
    let path = translated_str(token, path);
    if let Some(inode) = open_file(path.as_str(), OpenFlags::from_bits(flags).unwrap()) {
        let mut inner = task.inner_exclusive_access();
        let fd = inner.alloc_fd();
        inner.fd_table[fd] = Some(inode);
        fd as isize
    } else {
        -1
    }
}

pub fn sys_close(fd: usize) -> isize {
    trace!("kernel:pid[{}] sys_close", current_task().unwrap().pid.0);
    let task = current_task().unwrap();
    let mut inner = task.inner_exclusive_access();
    if fd >= inner.fd_table.len() {
        return -1;
    }
    if inner.fd_table[fd].is_none() {
        return -1;
    }
    inner.fd_table[fd].take();
    0
}

/// Get the status of the file referred to by `fd` into user memory `st`.
///
/// 返回值: 成功 0, fd 非法或未被打开返回 -1.
pub fn sys_fstat(fd: usize, st: *mut Stat) -> isize {
    trace!("kernel:pid[{}] sys_fstat", current_task().unwrap().pid.0);
    let token = current_user_token();
    let task = current_task().unwrap();
    let inner = task.inner_exclusive_access();
    if fd >= inner.fd_table.len() {
        return -1;
    }
    // 先把文件对象克隆出来并释放 TCB 的借用, 避免 copy_to_user 期间长期持锁
    let file = if let Some(file) = &inner.fd_table[fd] {
        file.clone()
    } else {
        return -1;
    };
    drop(inner);
    let stat = file.stat();
    // Stat 位于用户地址空间, 内核页表不能直接解引用, 必须通过用户 token 翻译
    let stat_size = core::mem::size_of::<Stat>();
    let mut user_buffers = translated_byte_buffer(token, st as *const u8, stat_size);
    let stat_bytes = unsafe {
        core::slice::from_raw_parts(&stat as *const Stat as *const u8, stat_size)
    };
    let mut copied = 0;
    for buffer in user_buffers.iter_mut() {
        let end = copied + buffer.len();
        buffer.copy_from_slice(&stat_bytes[copied..end]);
        copied = end;
    }
    0
}

/// Create a hard link `newpath` for the file `oldpath`.
///
/// `olddirfd`、`newdirfd`、`flags` 按实验约定忽略.
/// 返回值: 成功 0; 源文件不存在或新路径已存在返回 -1.
pub fn sys_linkat(old_name: *const u8, new_name: *const u8) -> isize {
    trace!("kernel:pid[{}] sys_linkat", current_task().unwrap().pid.0);
    let token = current_user_token();
    let old_name = translated_str(token, old_name);
    let new_name = translated_str(token, new_name);
    // 源路径必须存在
    let Some(inode) = ROOT_INODE.find(old_name.as_str()) else {
        return -1;
    };
    // 在根目录下为同一个 inode 追加一个新目录项
    ROOT_INODE.link(new_name.as_str(), &inode)
}

/// Remove the directory entry `path`.
///
/// `dirfd`、`flags` 按实验约定忽略.
/// 返回值: 成功 0; 路径不存在返回 -1.
pub fn sys_unlinkat(name: *const u8) -> isize {
    trace!(
        "kernel:pid[{}] sys_unlinkat",
        current_task().unwrap().pid.0
    );
    let token = current_user_token();
    let name = translated_str(token, name);
    ROOT_INODE.unlink(name.as_str())
}
