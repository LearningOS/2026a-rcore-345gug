//! Process management syscalls
use crate::{
    config::MAXVA,
    task::{exit_current_and_run_next, suspend_current_and_run_next, TASK_MANAGER},
    timer::get_time_us,
};

#[repr(C)]
#[derive(Debug)]
pub struct TimeVal {
    pub sec: usize,
    pub usec: usize,
}

/// 校验用户指针 [addr, addr + len) 是否落在用户可访问地址范围内。
///
/// ch3 还没有分页机制，无法逐页确认映射关系，但至少要保证：
/// - 指针非空；
/// - 区间不发生 usize 溢出；
/// - 区间上界不超过 MAXVA。
/// 校验通过后才允许在 unsafe 块中解引用。
fn is_valid_user_ptr(addr: usize, len: usize) -> bool {
    addr != 0
        && addr
            .checked_add(len)
            .map_or(false, |end| end <= MAXVA)
}

/// task exits and submit an exit code
pub fn sys_exit(exit_code: i32) -> ! {
    trace!("[kernel] Application exited with code {}", exit_code);
    exit_current_and_run_next();
    panic!("Unreachable in sys_exit!");
}

/// current task gives up resources for other tasks
pub fn sys_yield() -> isize {
    trace!("kernel: sys_yield");
    suspend_current_and_run_next();
    0
}

/// get time with second and microsecond
pub fn sys_get_time(ts: *mut TimeVal, _tz: usize) -> isize {
    trace!("kernel: sys_get_time");
    // 先校验用户指针，非法地址直接返回错误码，绝不能直接解引用
    if !is_valid_user_ptr(ts as usize, core::mem::size_of::<TimeVal>()) {
        return -1;
    }
    let us = get_time_us();
    unsafe {
        *ts = TimeVal {
            sec: us / 1_000_000,
            usec: us % 1_000_000,
        };
    }
    0
}

/// trace 系统调用（系统调用号 410）
///
/// - `trace_request == 0`：读取当前用户地址 `id` 处的一个字节，返回该字节；
/// - `trace_request == 1`：把 `data` 的低 8 位写入当前用户地址 `id` 处，返回 0；
/// - `trace_request == 2`：查询当前任务对系统调用 `id` 的调用次数；
/// - 其它请求或非法地址：返回 -1。
pub fn sys_trace(trace_request: usize, id: usize, data: usize) -> isize {
    trace!("kernel: sys_trace");
    match trace_request {
        0 => {
            // Read：id 是用户地址
            if !is_valid_user_ptr(id, core::mem::size_of::<u8>()) {
                return -1;
            }
            unsafe { (id as *const u8).read_volatile() as isize }
        }
        1 => {
            // Write：id 是用户地址，data 是要写入的值
            if !is_valid_user_ptr(id, core::mem::size_of::<u8>()) {
                return -1;
            }
            unsafe {
                (id as *mut u8).write_volatile(data as u8);
            }
            0
        }
        2 => {
            // Syscall：id 是系统调用号
            TASK_MANAGER.get_syscall_count(id)
        }
        _ => -1,
    }
}
