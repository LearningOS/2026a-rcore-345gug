//! Process management syscalls
use crate::config::PAGE_SIZE;
use crate::mm::{MapPermission, PTEFlags, PageTable, VirtAddr};
use crate::task::{
    change_program_brk, current_user_token, exit_current_and_run_next, mmap_current,
    munmap_current, suspend_current_and_run_next, TASK_MANAGER,
};
use crate::timer::get_time_us;

#[repr(C)]
#[derive(Debug)]
pub struct TimeVal {
    pub sec: usize,
    pub usec: usize,
}

/// Translate one user virtual address to the physical page backing it.
///
/// The kernel runs with the kernel page table while handling a syscall, so
/// a user pointer must never be dereferenced directly: it is translated
/// through the current task's page table instead (the physical page itself
/// is reachable because the kernel identity-maps all physical memory).
///
/// Returns `None` when the leaf PTE is missing/empty, when the page is not
/// user accessible (no `U` bit, e.g. the trampoline or the trap context),
/// or when the requested access is not permitted by the `R`/`W` bit.
fn user_page(addr: usize, for_write: bool) -> Option<&'static mut [u8]> {
    let va = VirtAddr::from(addr);
    let page_table = PageTable::from_token(current_user_token());
    let pte = page_table.translate(va.floor())?;
    if !pte.is_valid() || !pte.flags().contains(PTEFlags::U) {
        return None;
    }
    let allowed = if for_write {
        pte.writable()
    } else {
        pte.readable()
    };
    if !allowed {
        return None;
    }
    Some(pte.ppn().get_bytes_array())
}

/// Read one byte from the current user address space.
fn read_user_u8(addr: usize) -> Option<u8> {
    let offset = VirtAddr::from(addr).page_offset();
    user_page(addr, false).map(|page| page[offset])
}

/// Write one byte into the current user address space.
fn write_user_u8(addr: usize, value: u8) -> bool {
    let offset = VirtAddr::from(addr).page_offset();
    match user_page(addr, true) {
        Some(page) => {
            page[offset] = value;
            true
        }
        None => false,
    }
}

/// Copy `data` into the current user address space, crossing page
/// boundaries when necessary. Returns `false` if any page is invalid,
/// not user accessible or not writable.
fn write_user_bytes(addr: usize, data: &[u8]) -> bool {
    let mut written = 0;
    while written < data.len() {
        let cur = match addr.checked_add(written) {
            Some(v) => v,
            None => return false,
        };
        let offset = VirtAddr::from(cur).page_offset();
        let chunk = (PAGE_SIZE - offset).min(data.len() - written);
        match user_page(cur, true) {
            Some(page) => {
                page[offset..offset + chunk].copy_from_slice(&data[written..written + chunk]);
            }
            None => return false,
        }
        written += chunk;
    }
    true
}

/// task exits and submit an exit code
pub fn sys_exit(_exit_code: i32) -> ! {
    trace!("kernel: sys_exit");
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
///
/// With virtual memory enabled the user pointer `ts` is a virtual address:
/// the kernel cannot store through it directly (the MMU would translate it
/// in the kernel address space). The value is therefore serialised and
/// copied page by page through the user page table, which also covers the
/// case where the 16-byte `TimeVal` is split by a page boundary.
pub fn sys_get_time(ts: *mut TimeVal, _tz: usize) -> isize {
    trace!("kernel: sys_get_time");
    let us = get_time_us();
    let tv = TimeVal {
        sec: us / 1_000_000,
        usec: us % 1_000_000,
    };
    // SAFETY: `tv` is a live local and `TimeVal` is #[repr(C)], so reading
    // its object representation as bytes is valid.
    let bytes = unsafe {
        core::slice::from_raw_parts(
            &tv as *const TimeVal as *const u8,
            core::mem::size_of::<TimeVal>(),
        )
    };
    if write_user_bytes(ts as usize, bytes) {
        0
    } else {
        -1
    }
}

/// trace syscall (id 410)
///
/// - `trace_request == 0`: read one byte at user address `id`;
/// - `trace_request == 1`: write the low byte of `data` to `id`;
/// - `trace_request == 2`: number of invocations of syscall `id`;
/// - any other request or an invalid/forbidden address: `-1`.
///
/// Addresses are checked through the user page table, so kernel-only
/// mappings (trampoline/trap context) and unmapped pages are rejected,
/// and writes to non-writable pages (e.g. an `R`-only `mmap` page) fail.
pub fn sys_trace(trace_request: usize, id: usize, data: usize) -> isize {
    trace!("kernel: sys_trace");
    match trace_request {
        0 => match read_user_u8(id) {
            Some(byte) => byte as isize,
            None => -1,
        },
        1 => {
            if write_user_u8(id, data as u8) {
                0
            } else {
                -1
            }
        }
        2 => TASK_MANAGER.get_syscall_count(id),
        _ => -1,
    }
}

/// Anonymous mmap: allocate `len` bytes of physical memory and map it at
/// `start`.
///
/// `prot` uses the user ABI encoding: bit0 = readable, bit1 = writable,
/// bit2 = executable, all other bits must be zero. The range is rounded
/// up to whole pages, and the call fails if any page in the range is
/// already mapped or if physical memory runs out.
pub fn sys_mmap(start: usize, len: usize, prot: usize) -> isize {
    trace!("kernel: sys_mmap");
    // `start` must be page aligned.
    if start % PAGE_SIZE != 0 {
        return -1;
    }
    // Only bits 0..2 are defined and at least one of them must be set.
    if prot & !0x7 != 0 || prot & 0x7 == 0 {
        return -1;
    }
    // Round the length up to whole pages and reject address overflow.
    let page_count = len.div_ceil(PAGE_SIZE);
    let map_len = match page_count.checked_mul(PAGE_SIZE) {
        Some(v) => v,
        None => return -1,
    };
    if start.checked_add(map_len).is_none() {
        return -1;
    }
    // The user `prot` encoding is NOT the kernel MapPermission encoding
    // (R=1<<1, W=1<<2, X=1<<3, U=1<<4), so translate bit by bit instead
    // of reusing the raw value. Every user page also needs `U`.
    let mut permission = MapPermission::U;
    if prot & 0x1 != 0 {
        permission |= MapPermission::R;
    }
    if prot & 0x2 != 0 {
        permission |= MapPermission::W;
    }
    if prot & 0x4 != 0 {
        permission |= MapPermission::X;
    }
    if mmap_current(VirtAddr::from(start), map_len, permission) {
        0
    } else {
        -1
    }
}

/// Unmap `[start, start + len)`.
///
/// The range is rounded up to whole pages; every page must be mapped,
/// otherwise nothing is changed and `-1` is returned.
pub fn sys_munmap(start: usize, len: usize) -> isize {
    trace!("kernel: sys_munmap");
    // A non-aligned start cannot be expressed as a whole-page range.
    if start % PAGE_SIZE != 0 {
        return -1;
    }
    let page_count = len.div_ceil(PAGE_SIZE);
    let map_len = match page_count.checked_mul(PAGE_SIZE) {
        Some(v) => v,
        None => return -1,
    };
    if start.checked_add(map_len).is_none() {
        return -1;
    }
    if munmap_current(VirtAddr::from(start), map_len) {
        0
    } else {
        -1
    }
}

/// change data segment size
pub fn sys_sbrk(size: i32) -> isize {
    trace!("kernel: sys_sbrk");
    if let Some(old_brk) = change_program_brk(size) {
        old_brk as isize
    } else {
        -1
    }
}
