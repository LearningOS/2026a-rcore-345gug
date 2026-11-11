# rCore-Tutorial 第三章实验报告：sys_trace 系统调用与系统调用计数

## 1. 实验信息

| 项目 | 内容 |
| --- | --- |
| 实验章节 | 第三章 多道程序与分时多任务（lab1） |
| 分支 | `ch3` |
| 内核工具链 | `nightly-2024-05-02`，目标 `riscv64gc-unknown-none-elf` |
| 运行环境 | QEMU RISC-V `virt`，RustSBI |
| 官方检查器 | `LearningOS/rCore-Tutorial-Checker` |
| 测试程序 | `LearningOS/rCore-Tutorial-Test` |
| 验证命令 | `make -C ci-user test CHAPTER=3` |

## 2. 实验目标

1. 每次用户态执行 `ecall` 触发系统调用时，内核根据 `a7` 中的系统调用号，对当前任务的系统调用计数数组对应位置加一；`sys_trace` 自身的调用也要计入。
2. 实现 `sys_trace` 系统调用，支持按请求类型读取用户内存、写入用户内存、查询当前任务对某个系统调用的调用次数。
3. 计数数组放在 `TaskManagerInner` 内部，由 `UPSafeCell` 保护，正确上锁/解锁，不能死锁。
4. 所有用户指针必须先校验再解引用；成功返回 0，非法地址、越界、长度超限返回负数错误码。

## 3. 实验原理

### 3.1 Trap 入口与系统调用分发

用户程序通过 `ecall` 指令陷入 S 态后，硬件把返回地址写入 `sepc`，并跳到 `stvec` 指向的 `__alltraps`。`trap.S` 在内核栈上保存 34 个寄存器的 `TrapContext`，然后调用 `trap_handler`。

对于 `Exception::UserEnvCall`：

- `ecall` 指令长度为 4 字节，因此必须执行 `cx.sepc += 4`，否则 `sret` 后会再次执行同一条 `ecall`，造成死循环；
- RISC-V 系统调用约定：`a7`（`x[17]`）是系统调用号，`a0`~`a2`（`x[10]`~`x[12]`）是参数，返回值写回 `a0`；
- 取出系统调用号和参数后交给 `syscall()` 按系统调用号分发，最后把 `isize` 返回值写回 `cx.x[10]`。

### 3.2 系统调用计数

计数数组定义为 `[usize; MAX_SYSCALL_NUM]`，**直接以系统调用号为下标**。第三章用到的系统调用号包括 `64(write)`、`93(exit)`、`124(yield)`、`169(gettimeofday)`、`410(trace)`，所以数组长度必须大于 410，本实验与上游参考实现一致取 500。

计数数组是每个任务私有的，放在 `TaskControlBlock` 中，而所有 `TaskControlBlock` 组成 `TaskManagerInner.tasks`，因此它位于 `TaskManagerInner` 内部，并和 `current_task` 一起被 `UPSafeCell` 保护。

计数的时机选择在**系统调用分发之前**：

- 若在分发之后计数，`sys_trace` 查询计数时看不到本次调用，`ch3_trace` 中 `assert_eq!(2, count_syscall(SYSCALL_TRACE))` 会少 1；
- 在分发之前计数，每个系统调用恰好计一次，`sys_trace` 自身也会被统计；
- `sys_exit` 不会返回，若放在分发之后计数会漏计，放在分发之前则没有这个问题。

### 3.3 sys_trace 接口

测试程序使用 `user_lib` 中的封装：

```rust
pub enum TraceRequest {
    Read,    // 0
    Write,   // 1
    Syscall, // 2
}

pub fn trace(request: TraceRequest, id: usize, data: usize) -> isize;
pub fn trace_read(addr: *const u8) -> Option<u8>;   // trace(Read, addr, 0)
pub fn trace_write(addr: *const u8, data: u8) -> isize; // trace(Write, addr, data)
pub fn count_syscall(id: usize) -> isize;           // trace(Syscall, id, 0)
```

因此内核中 `sys_trace(trace_request, id, data)` 的语义为：

| `trace_request` | `id` 含义 | `data` 含义 | 返回值 |
| --- | --- | --- | --- |
| 0（Read） | 用户地址 | 忽略 | 该地址处的 1 字节（0~255） |
| 1（Write） | 用户地址 | 写入值 | 成功 0，非法地址 -1 |
| 2（Syscall） | 系统调用号 | 忽略 | 当前任务对该系统调用的调用次数 |
| 其它 | — | — | -1 |

`sys_trace` 的系统调用号为 **410**，与 `user/src/syscall.rs` 中的 `SYSCALL_TRACE` 一致。

### 3.4 用户指针校验

第三章尚未开启分页，内核与用户程序处于同一物理地址空间，无法像第四章那样逐页检查映射关系。但绝不能因此直接解引用用户裸指针，至少要保证：

1. 指针非空；
2. `addr + len` 不溢出 `usize`；
3. 区间上界不超过 `MAXVA`（`1 << 63`）。

校验通过后才进入 `unsafe` 块，并使用 `read_volatile` / `write_volatile`，避免编译器优化掉对用户内存的访问。非法地址统一返回 `-1`。

### 3.5 锁与死锁分析

`UPSafeCell::exclusive_access()` 内部是 `RefCell::borrow_mut()`，同一时刻只能存在一个 `RefMut`。实现时注意：

- `TASK_MANAGER` 是 `lazy_static!` 生成的不可变静态量，方法必须使用 `&self`，不能使用 `&mut self`；
- `add_syscall_count` 取得 `RefMut`、完成自增后立即 `drop(inner)`，再返回到 `trap_handler` 调用 `syscall()`；
- `sys_trace` 的 Syscall 分支中 `get_syscall_count` 重新获取 `RefMut`，此时前一个借用已经释放，不存在嵌套借用；
- 不能为了“省一次上锁”而把 `RefMut` 跨越 `syscall()` 持有，否则 `sys_trace` 内部再次借用会触发 `RefCell` 重复借用 panic。

## 4. 实现代码

### 4.1 `os/src/config.rs`

```rust
/// max syscall number
/// 系统调用号最大为 410（sys_trace），计数数组按系统调用号直接下标寻址，
/// 因此长度必须大于 410。这里与上游参考实现保持一致取 500。
pub const MAX_SYSCALL_NUM: usize = 500;
/// 用户态可访问地址的上界（不含），用于校验用户指针
pub const MAXVA: usize = 1 << (8 * 8 - 1);
```

### 4.2 `os/src/task/task.rs`

```rust
use super::TaskContext;
use crate::config::MAX_SYSCALL_NUM;

/// The task control block (TCB) of a task.
#[derive(Copy, Clone)]
pub struct TaskControlBlock {
    /// The task status in it's lifecycle
    pub task_status: TaskStatus,
    /// The task context
    pub task_cx: TaskContext,
    /// 当前任务各系统调用的调用次数，下标为系统调用号
    pub syscall_cnt: [usize; MAX_SYSCALL_NUM],
}
```

### 4.3 `os/src/task/mod.rs`

任务初始化时把计数数组清零：

```rust
lazy_static! {
    /// Global variable: TASK_MANAGER
    pub static ref TASK_MANAGER: TaskManager = {
        let num_app = get_num_app();
        let mut tasks = [TaskControlBlock {
            task_cx: TaskContext::zero_init(),
            task_status: TaskStatus::UnInit,
            syscall_cnt: [0; MAX_SYSCALL_NUM],
        }; MAX_APP_NUM];
        for (i, task) in tasks.iter_mut().enumerate() {
            task.task_cx = TaskContext::goto_restore(init_app_cx(i));
            task.task_status = TaskStatus::Ready;
        }
        TaskManager {
            num_app,
            inner: unsafe {
                UPSafeCell::new(TaskManagerInner {
                    tasks,
                    current_task: 0,
                })
            },
        }
    };
}
```

计数的自增与查询：

```rust
impl TaskManager {
    /// 当前任务对 `syscall_id` 的调用次数 +1。
    ///
    /// 注意：`TASK_MANAGER` 是不可变静态量，这里必须用 `&self`；
    /// `inner` 在函数返回时自动释放，绝不能跨越 `syscall()` 持有。
    pub fn add_syscall_count(&self, syscall_id: usize) {
        let mut inner = self.inner.exclusive_access();
        let curr_id = inner.current_task;
        if syscall_id < MAX_SYSCALL_NUM {
            inner.tasks[curr_id].syscall_cnt[syscall_id] += 1;
        }
        // 显式释放锁，避免后续 sys_trace 再次上锁时发生 RefCell 重复借用
        drop(inner);
    }

    /// 查询当前任务对 `syscall_id` 的调用次数，越界返回 -1。
    pub fn get_syscall_count(&self, syscall_id: usize) -> isize {
        let inner = self.inner.exclusive_access();
        let curr_id = inner.current_task;
        if syscall_id < MAX_SYSCALL_NUM {
            inner.tasks[curr_id].syscall_cnt[syscall_id] as isize
        } else {
            -1
        }
    }
}
```

### 4.4 `os/src/trap/mod.rs`

```rust
use crate::syscall::syscall;
use crate::task::{exit_current_and_run_next, suspend_current_and_run_next, TASK_MANAGER};
use crate::timer::set_next_trigger;

// ...

    match scause.cause() {
        Trap::Exception(Exception::UserEnvCall) => {
            cx.sepc += 4;
            let syscall_id: usize = cx.x[17];
            // 在分发之前计数：这样 sys_trace 自身也会被计入，
            // 且每个系统调用只会计数一次。add_syscall_count 内部已经
            // 获取并释放了 TASK_MANAGER 的锁，不会与 sys_trace 死锁。
            TASK_MANAGER.add_syscall_count(syscall_id);
            let ret: isize = syscall(syscall_id, [cx.x[10], cx.x[11], cx.x[12]]);
            cx.x[10] = ret as usize;
        }
        // ...
    }
```

### 4.5 `os/src/syscall/mod.rs`

```rust
/// write syscall
const SYSCALL_WRITE: usize = 64;
/// exit syscall
const SYSCALL_EXIT: usize = 93;
/// yield syscall
const SYSCALL_YIELD: usize = 124;
/// gettime syscall
const SYSCALL_GET_TIME: usize = 169;
/// trace syscall
const SYSCALL_TRACE: usize = 410;

/// handle syscall exception with `syscall_id` and other arguments
pub fn syscall(syscall_id: usize, args: [usize; 3]) -> isize {
    match syscall_id {
        SYSCALL_WRITE => sys_write(args[0], args[1] as *const u8, args[2]),
        SYSCALL_EXIT => sys_exit(args[0] as i32),
        SYSCALL_YIELD => sys_yield(),
        SYSCALL_GET_TIME => sys_get_time(args[0] as *mut TimeVal, args[1]),
        SYSCALL_TRACE => sys_trace(args[0], args[1], args[2]),
        _ => panic!("Unsupported syscall_id: {}", syscall_id),
    }
}
```

### 4.6 `os/src/syscall/process.rs`

```rust
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
```

## 5. 踩坑记录

### 5.1 计数数组长度取了 5

最初把 `MAX_SYSCALL_NUM` 理解成“本章支持的系统调用种类数”，设为 5，并在自增前判断 `syscall_id < MAX_SYSCALL_NUM`。但计数数组是按系统调用号**直接下标寻址**的，`64/93/124/169/410` 全部越界，所有计数恒为 0，`ch3_trace` 的断言全部失败。

**修正**：数组长度必须大于最大系统调用号，取 `MAX_SYSCALL_NUM = 500`。

### 5.2 对 lazy_static 静态量使用 `&mut self`

`add_syscall_count` 最初写成 `pub fn add_syscall_count(&mut self, ...)`，编译报错：

```
error[E0596]: cannot borrow data in dereference of `TASK_MANAGER` as mutable
```

`TASK_MANAGER` 只能通过 `Deref` 得到共享引用，因此方法改为 `&self`，可变性由内部的 `UPSafeCell` 提供。

### 5.3 锁的持有范围

如果为了“保证计数和查询看到同一份数据”而在 `trap_handler` 中先取得 `RefMut` 再调用 `syscall()`，`sys_trace` 内部的 `exclusive_access()` 会发生第二次 `borrow_mut()`，直接 panic。正确做法是让 `add_syscall_count` 自己上锁、自增、立即释放，`sys_trace` 查询时重新上锁，两次借用不重叠。

### 5.4 误用了另一套 trace 接口

最初把 `sys_trace` 实现成“查询任务信息并拷贝 `TaskInfo` 到用户缓冲区”，这与本章测试程序使用的 `TraceRequest::Read/Write/Syscall` 三元组接口不一致：

- `count_syscall` 传入的 `id` 是系统调用号，`trace_read/write` 传入的 `id` 是用户地址，都不是任务号；
- 原实现会把约 8KB 的 `TaskInfo` 写到 `data` 指针，而 `count_syscall` 的 `data` 为 0，会直接写空指针触发 StorePageFault；
- `[SyscallInfo; 500]` 的 `TaskInfo` 按值返回还会占用约 8KB 内核栈，而内核栈只有 8KB，存在栈溢出风险。

**修正**：删除 `TaskInfo/SyscallInfo/query_task_info/start_time` 等另一套接口的残留，按 `Read/Write/Syscall` 重新实现 `sys_trace`。

### 5.5 未使用的 import 被 `deny(warnings)` 拦截

`os/src/main.rs` 中开启了 `#![deny(warnings)]`，`trap/mod.rs` 里遗留的 `use crate::config::MAX_SYSCALL_NUM;` 未使用，导致 `unused_imports` 从警告升级为编译错误。删除该 import 后编译通过。

### 5.6 `sys_get_time` 直接解引用用户指针

原实现没有校验 `ts` 就执行 `*ts = ...`。用户传入非法地址时会触发 StorePageFault，内核直接杀掉应用而不是返回错误码。修正为先用 `is_valid_user_ptr` 校验非空、溢出和 `MAXVA` 上界，再进入 `unsafe`。

### 5.7 报告检查

测试本身 7/7 通过后，`make -C ci-user test CHAPTER=3` 仍可能以非 0 退出：不带 `passwd` 时 Makefile 会检查 `reports/lab1.pdf` 或 `reports/lab1.md`，文件不存在会输出 `Report for lab1 needed` 并返回 `Error 1`。本报告即为满足该检查而提交。

### 5.8 QEMU 与工具链

本地 QEMU 版本为 10.2.1，官方 CI 固定为 8.2.x，本次实测 10.2.1 也能正常完成测试。`cargo-binutils` 需为 0.4.0；若 `cargo install cargo-binutils` 卡住，属于网络/工具链环境问题，不是内核代码问题。

## 6. 测试结果

### 6.1 编译

```text
$ cargo build --release
Finished `release` profile [optimized] target(s) in 21.33s
```

### 6.2 官方检查器

```text
$ make -C ci-user test CHAPTER=3
...
get_time OK14613! 355
current time_msec = 369
time_msec = 571 after sleeping 100 ticks, delta = 202ms!
Test sleep1 passed14613!
string from task trace test
Test trace OK14613!
Test sleep OK14613!
...
Test passed14613: 7/7
```

其中 `14613` 是检查器 `randomize` 生成的随机后缀，用于防止硬编码输出。`check/ch3.py` 的 7 个模式全部匹配：

```
[PASS] found <get_time OK... (\d+)>
[PASS] found <Test sleep OK...>
[PASS] found <current time_msec = (\d+)>
[PASS] found <time_msec = (\d+) after sleeping (\d+) ticks, delta = (\d+)ms!>
[PASS] found <Test sleep1 passed...>
[PASS] found <string from task trace test>
[PASS] found <Test trace OK...>

Test passed: 7/7
```

`ch3_trace` 中的关键断言均成立：

- `count_syscall(SYSCALL_TRACE)` 第一次查询返回 2、第二次返回 7，说明 `sys_trace` 自身被正确计入且未重复计数；
- `count_syscall(SYSCALL_WRITE)` 在 `println!` 后返回 2，说明输出确实调用了两次 `write`；
- `count_syscall(SYSCALL_YIELD)` 大于 0，说明 `sleep` 循环中的 `yield` 被统计；
- `count_syscall(SYSCALL_EXIT)` 为 0，说明尚未调用 `exit`；
- `trace_read/trace_write` 能正确读写用户栈上的局部变量，并能读取 `main` 的指令字节。

### 6.3 检查清单

| 判分点 | 结果 |
| --- | --- |
| `sepc += 4`、`a7` 取号、`a0~a2` 取参、分发正确 | 通过 |
| 分发前计数，包含 `sys_trace`，无漏计/重复计数 | 通过 |
| 计数数组位于 `TaskManagerInner` 内，`UPSafeCell` 保护 | 通过 |
| `sys_trace` 地址校验、边界判断、读写语义、负数错误码 | 通过 |
| `isize` 返回值、`checked_add`、`unsafe` 受控 | 通过 |
| 无 QEMU 卡死、无判分相关 panic、输出与样例一致 | 通过（7/7） |
| 报告文件存在 | 通过 |

## 7. 小结

本实验在第三章任务管理框架上完成了系统调用计数与 `sys_trace`。核心点有三个：一是计数必须在系统调用分发之前完成，才能把 `sys_trace` 自身计入且保证每个调用只计一次；二是计数数组按系统调用号直接寻址，长度必须覆盖 410；三是 `UPSafeCell` 的借用范围要尽量短，绝不能跨 `syscall()` 持有，否则会在 `sys_trace` 中重复借用。用户指针必须经过非空、溢出和 `MAXVA` 上界校验后才能解引用。最终官方检查器 7/7 全部通过。
