# rCore 第四章实验报告（一）：虚拟内存下 `sys_get_time` 的重写

> 实验环境：Rust nightly-2024-05-02 + RISC-V SV39 + rCore-Tutorial（ch4-lab 分支）
>
> 涉及文件：`os/src/syscall/process.rs`、`os/src/syscall/mod.rs`、`os/src/trap/mod.rs`、`os/src/task/task.rs`、`os/src/task/mod.rs`

---

## 1. 实验目的

1. 理解 RISC-V SV39 分页机制下“用户地址空间”和“内核地址空间”的切换过程。
2. 分析第三章中直接通过用户指针读写内存的 `sys_get_time` 在开启虚拟内存后失效的根本原因。
3. 掌握“内核通过用户页表翻译用户虚拟地址，再访问物理页”的正确做法。
4. 处理 `TimeVal` 跨页（16 字节结构体被页边界切开）的情况，恢复 `sys_get_time` 的正常功能。
5. 通过官方测例 `ch3_sleep`、`ch3_sleep1`、`ch3_trace`、`ch4_trace1` 以及 `make -C ci-user test CHAPTER=4` 的全部检查。

> 说明：本章官方测例还要求恢复 `sys_trace`（`ch3_trace`、`ch4_trace1`），而原始模板中的 `sys_trace` 同样是“直接解引用用户指针”的第三章实现。它与 `sys_get_time` 失效的原因完全相同，因此本报告在核心代码部分一并给出其虚存版本；这也是通过 `make -C ci-user test CHAPTER=4` 的必要条件。

---

## 2. 问题分析：开启虚存后原 `get_time` 为什么会失效

### 2.1 第三章的实现方式

第三章没有分页机制，内核与用户程序共用物理地址空间（本质上是恒等映射）。用户库调用：

```rust
pub fn sys_get_time(time: &mut TimeVal, tz: usize) -> isize {
    syscall(SYSCALL_GETTIMEOFDAY, [time as *const _ as usize, tz, 0])
}
```

传给内核的 `time` 就是用户栈上的一个**物理地址**。因此内核可以直接：

```rust
let us = get_time_us();
unsafe {
    *ts = TimeVal { sec: us / 1_000_000, usec: us % 1_000_000 };
}
```

### 2.2 第四章的地址空间模型

第四章启用 SV39 后，每个任务拥有独立的 `MemorySet`（页表），而内核拥有自己的 `KERNEL_SPACE`。陷入内核的过程是：

1. 用户态执行 `ecall`，CPU 跳到 `stvec` 指向的 trampoline（`__alltraps`）。
2. `__alltraps` 把通用寄存器保存到用户的 `TrapContext`，然后从 `TrapContext` 中取出 `kernel_satp`，执行 `csrw satp, t0; sfence.vma`，**切换到内核页表**。
3. 之后才跳转到 `trap_handler()`，系统调用 `syscall()` 在**内核地址空间**中执行。

也就是说，`sys_get_time` 收到 `ts` 时，CPU 的 `satp` 已经指向内核页表。此时：

```rust
*ts = ...;   // 错误！
```

MMU 会把 `ts` 当作**内核虚拟地址**再次翻译：

- 如果该地址在内核页表中没有映射（常见情况，例如用户栈地址 `0xc000`），会产生内核态缺页异常。内核的异常入口 `trap_from_kernel()` 直接 `panic!("a trap ... from kernel!")`，整个操作系统停机；
- 如果该地址在内核页表中恰好有映射，则会写到另一个物理页上：轻则时间没有写回用户 `TimeVal`（`get_time()` 读到垃圾值），重则破坏内核数据。

因此，**“把用户指针当物理地址直接解引用”在开启虚存后必然失效**。必须先用当前任务的用户页表把用户虚拟地址翻译成物理页号，再通过内核恒等映射的物理内存写入。

### 2.3 另一个隐藏问题：`TimeVal` 可能跨页

`TimeVal` 在用户库和内核中都是：

```rust
#[repr(C)]
pub struct TimeVal {
    pub sec: usize,
    pub usec: usize,
}
```

两个 `usize` 共 16 字节。如果用户把它放在页末（例如地址 `0x...ff8`），`sec` 与 `usec` 会落在两个不同的物理页上。只翻译一次起始页、然后连续写 16 字节是错误且危险的。题目提示 “What if `TimeVal` is splitted by two pages?” 就是在提醒这一点。

### 2.4 `sys_trace` 的同类问题

`sys_trace` 同样接收用户地址：

- 请求 0：读取 `id` 处的一个字节；
- 请求 1：向 `id` 处写入一个字节；
- 请求 2：查询系统调用计数。

第三章可以直接解引用；第四章必须查页表，并且 `ch4_trace1` 还额外要求：

- `trace_read(isize::MAX)` 与 `trace_read(0x80200000)` 返回 `-1`（用户不可见）；
- 对只读 `mmap` 页执行 `trace_write` 返回 `-1`（用户不可写）；
- `munmap` 后 `trace_read` 返回 `-1`（页已不在页表中）。

---

## 3. 原理分析

### 3.1 SV39 页表项

SV39 的页表项（PTE）低 8 位为：

| 位 | 含义 |
| --- | --- |
| 0 | V（Valid，有效） |
| 1 | R（Readable，可读） |
| 2 | W（Writable，可写） |
| 3 | X（Executable，可执行） |
| 4 | U（User，用户态可访问） |
| 5 | G（Global） |
| 6 | A（Accessed） |
| 7 | D（Dirty） |

`rCore` 的 `PageTable::translate(vpn)` 返回叶子 PTE，但要注意：**叶子 PTE 不一定有效**。`find_pte()` 在遍历到最后一级时，只要前两级页表存在就返回 `Some(pte)`，即使叶子 PTE 是空项（V=0）。所以判断“某页是否已映射”必须用 `pte.is_valid()`，不能只用 `translate(vpn).is_some()`。

### 3.2 内核如何访问用户内存

系统调用期间 `satp` 是内核页表。`KERNEL_SPACE::new_kernel()` 把从 `ekernel` 到 `MEMORY_END` 的物理内存**恒等映射**为可读写。因此：

```rust
let token = current_user_token();          // 当前任务的用户页表 token
let page_table = PageTable::from_token(token); // 只读地“借用”用户页表
let pte = page_table.translate(vpn).unwrap();
let ppn = pte.ppn();                       // 得到物理页号
ppn.get_bytes_array()[offset] = ...;       // 通过恒等映射直接写物理页
```

`PageTable::from_token()` 不会切换 `satp`，只是在当前内核地址空间里按用户页表的根页号走一遍页表，这正是我们需要的。

### 3.3 为什么必须检查 U 位

`isize::MAX = 0x7FFF_FFFF_FFFF_FFFF` 在 `VirtAddr::from()` 中会被截断为 39 位：`0x7F_FFFF_FFFF`，其 VPN 恰好与 trampoline（`0xFFFF_FFFF_FFFF_F000` 截断后）相同。trampoline 在内核/用户页表里都是有效映射，但它是 `R|X`，**没有 U 位**。如果只检查 `is_valid()`，就会把 trampoline 的物理页内容当作“用户地址处的字节”返回，测试失败，而且语义上是越权访问。

同样，`TRAP_CONTEXT_BASE` 处的页是 `R|W`、无 U 位。因此：

> **凡是来自用户指针的访问，都必须要求叶子 PTE 带 U 位；写访问还要求 W 位；读访问要求 R 位。**

这是 `ch4_trace1` 能正确区分 `isize::MAX`、`0x80200000`（未映射）、普通用户变量、只读 mmap 页的关键。

### 3.4 系统的调用计数

`ch3_trace` 对计数有精确要求（例如 `count_syscall(SYSCALL_TRACE) == 7`），并且注释说明“这次 `sys_trace` 调用本身也计入”。因此计数必须在**分发系统调用之前**完成：

```rust
Trap::Exception(Exception::UserEnvCall) => {
    cx.sepc += 4;
    let syscall_id = cx.x[17];
    TASK_MANAGER.add_syscall_count(syscall_id); // 先计数，再分发
    cx.x[10] = syscall(syscall_id, [cx.x[10], cx.x[11], cx.x[12]]) as usize;
}
```

`add_syscall_count()` 内部获取的 `RefCell` 借用会在函数返回时释放；`sys_trace()` 再调用 `get_syscall_count()` 时不会发生重复借用。计数数组按任务保存在 TCB 中（`[usize; MAX_SYSCALL_NUM]`），而不是全局共享，保证每个测例进程的计数互不干扰。

---

## 4. 实现思路

### 4.1 `sys_get_time`

1. 用 `timer::get_time_us()` 取得微秒数，构造局部 `TimeVal { sec, usec }`。
2. 把局部结构体按字节序（小端）序列化为 16 字节切片。
3. 用 `current_user_token()` 取得当前任务的用户页表，逐页翻译 `ts`：
   - 计算当前地址所在页与页内偏移；
   - 校验 PTE 有效、带 U 位、带 W 位；
   - 计算本页还能写多少字节（`PAGE_SIZE - offset`），拷贝这一段；
   - 若还没写完，地址前进到下一页继续。
4. 全部写完返回 `0`；任何一页非法（未映射、无 U、无 W、地址溢出）返回 `-1`。

这样做同时解决了两个问题：用户指针不能直接解引用，以及 `TimeVal` 跨页。

### 4.2 配套的 `sys_trace`

1. 请求 0/1：翻译 `id` 所在页，校验 U 位以及 R（读）或 W（写）位，读写一个字节；失败返回 `-1`。
2. 请求 2：返回当前任务对 `id` 号系统调用的计数，`id >= MAX_SYSCALL_NUM` 返回 `-1`。
3. 在 `trap_handler()` 分发之前调用 `TASK_MANAGER.add_syscall_count(syscall_id)`。

### 4.3 复用与安全

- 所有用户内存访问集中到 `user_page()` 一个入口，避免各处重复写页表遍历代码。
- 不修改页表、不切换 `satp`，只借用用户页表做翻译，符合“使用 rCore 原有组件”的约束。
- 不使用 `translated_byte_buffer()` 的 `unwrap()` 版本处理可能非法的用户指针，避免恶意地址导致内核 panic。

---

## 5. 核心代码（带详细注释）

### 5.1 `os/src/timer.rs`：微秒时间源（已有，直接复用）

```rust
/// get current time in microseconds
#[allow(dead_code)]
pub fn get_time_us() -> usize {
    time::read() * MICRO_PER_SEC / CLOCK_FREQ
}
```

`time::read()` 是 `rdtime` 读取的时钟计数；`CLOCK_FREQ = 12_500_000`，`MICRO_PER_SEC = 1_000_000`，因此 `us = ticks * 1e6 / 12.5e6`。

### 5.2 `os/src/syscall/process.rs`：用户内存访问辅助函数

```rust
/// 把用户虚拟地址翻译为其后端的物理页（整页 4096 字节切片）。
///
/// 系统调用期间内核运行在内核页表中，因此绝不能直接解引用用户指针：
/// 必须通过当前任务的用户页表翻译，物理页本身是内核恒等映射可见的。
///
/// 返回 None 的情况：
/// - 叶子 PTE 不存在或 V=0；
/// - 页没有 U 位（如 trampoline、TrapContext 等内核专用映射）；
/// - 读访问没有 R 位，或写访问没有 W 位。
fn user_page(addr: usize, for_write: bool) -> Option<&'static mut [u8]> {
    let va = VirtAddr::from(addr);                       // 截断到 SV39 的 39 位
    let page_table = PageTable::from_token(current_user_token());
    let pte = page_table.translate(va.floor())?;         // 前两级缺页时直接 None
    if !pte.is_valid() || !pte.flags().contains(PTEFlags::U) {
        return None;                                     // 空 PTE 或非用户页
    }
    let allowed = if for_write {
        pte.writable()                                   // 写：需要 W
    } else {
        pte.readable()                                   // 读：需要 R
    };
    if !allowed {
        return None;
    }
    Some(pte.ppn().get_bytes_array())                    // 通过恒等映射访问物理页
}

/// 从当前用户地址空间读取一个字节。
fn read_user_u8(addr: usize) -> Option<u8> {
    let offset = VirtAddr::from(addr).page_offset();
    user_page(addr, false).map(|page| page[offset])
}

/// 向当前用户地址空间写入一个字节。
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

/// 把 data 拷贝到当前用户地址空间，必要时跨页。
/// 任意一页非法、非用户页或不可写都返回 false。
fn write_user_bytes(addr: usize, data: &[u8]) -> bool {
    let mut written = 0;
    while written < data.len() {
        // 防止 addr + written 溢出
        let cur = match addr.checked_add(written) {
            Some(v) => v,
            None => return false,
        };
        let offset = VirtAddr::from(cur).page_offset();
        // 本页最多还能写 PAGE_SIZE - offset 字节
        let chunk = (PAGE_SIZE - offset).min(data.len() - written);
        match user_page(cur, true) {
            Some(page) => {
                page[offset..offset + chunk]
                    .copy_from_slice(&data[written..written + chunk]);
            }
            None => return false,
        }
        written += chunk;                                // 继续下一页
    }
    true
}
```

### 5.3 `os/src/syscall/process.rs`：`sys_get_time`

```rust
/// 把当前时间写入用户指针 ts 指向的 TimeVal。
///
/// 开启虚存后 ts 是用户虚拟地址：内核不能直接解引用（MMU 会用内核页表
/// 翻译它），必须先经过用户页表翻译。TimeVal 共 16 字节，可能跨两页，
/// 因此按页分段拷贝。
pub fn sys_get_time(ts: *mut TimeVal, _tz: usize) -> isize {
    trace!("kernel: sys_get_time");

    // 以微秒为单位取得当前时间
    let us = get_time_us();
    let tv = TimeVal {
        sec: us / 1_000_000,
        usec: us % 1_000_000,
    };

    // SAFETY: tv 是仍然存活的局部变量，TimeVal 是 #[repr(C)]，
    // 按字节读取它的对象表示是合法的。
    let bytes = unsafe {
        core::slice::from_raw_parts(
            &tv as *const TimeVal as *const u8,
            core::mem::size_of::<TimeVal>(),
        )
    };

    // 通过用户页表逐页写入；非法指针返回 -1，而不是让内核崩溃
    if write_user_bytes(ts as usize, bytes) {
        0
    } else {
        -1
    }
}
```

### 5.4 `os/src/syscall/process.rs`：配套重写的 `sys_trace`

```rust
/// trace 系统调用（系统调用号 410）
///
/// - trace_request == 0：读取用户地址 id 处的一个字节；
/// - trace_request == 1：把 data 的低 8 位写入用户地址 id；
/// - trace_request == 2：返回当前任务对系统调用 id 的调用次数；
/// - 其它请求、非法地址、越权访问：返回 -1。
///
/// 地址一律经过用户页表检查，因此 trampoline / TrapContext 等
/// 内核专用映射、未映射页、只读页都会被正确拒绝。
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
```

### 5.5 `os/src/trap/mod.rs`：在分发前计数

```rust
Trap::Exception(Exception::UserEnvCall) => {
    // 跳过 ecall 指令
    cx.sepc += 4;
    let syscall_id = cx.x[17];
    // 先计数再分发：sys_trace 查询计数时会把“本次 sys_trace”也算进去，
    // 与官方测例的预期一致；add_syscall_count 内部已经释放锁，
    // 不会与 sys_trace 中的 get_syscall_count 形成 RefCell 重复借用。
    TASK_MANAGER.add_syscall_count(syscall_id);
    // 执行系统调用并把返回值写回 a0
    cx.x[10] = syscall(syscall_id, [cx.x[10], cx.x[11], cx.x[12]]) as usize;
}
```

### 5.6 `os/src/task/task.rs` 与 `os/src/task/mod.rs`：每任务计数

```rust
// task.rs：每个任务维护自己的计数数组
pub struct TaskControlBlock {
    // ...
    /// 每个系统调用的调用次数，下标为系统调用号，供 sys_trace 使用
    pub syscall_cnt: [usize; MAX_SYSCALL_NUM],
}

// 创建任务时清零
syscall_cnt: [0; MAX_SYSCALL_NUM],
```

```rust
// task/mod.rs：TaskManager 上的计数接口
pub fn add_syscall_count(&self, syscall_id: usize) {
    let mut inner = self.inner.exclusive_access();
    let cur = inner.current_task;
    if syscall_id < MAX_SYSCALL_NUM {
        inner.tasks[cur].syscall_cnt[syscall_id] += 1;
    }
}

pub fn get_syscall_count(&self, syscall_id: usize) -> isize {
    let inner = self.inner.exclusive_access();
    let cur = inner.current_task;
    if syscall_id < MAX_SYSCALL_NUM {
        inner.tasks[cur].syscall_cnt[syscall_id] as isize
    } else {
        -1
    }
}
```

注意 `add_syscall_count` 里的 `inner` 借用会在函数结束时自动释放，`syscall()` 才继续执行，因此 `sys_trace` 内部再次借用 `TASK_MANAGER` 是安全的。

### 5.7 `os/src/syscall/mod.rs`：系统调用注册

```rust
/// gettime syscall
const SYSCALL_GET_TIME: usize = 169;
/// sbrk syscall
const SYSCALL_SBRK: usize = 214;
/// munmap syscall
const SYSCALL_MUNMAP: usize = 215;
/// mmap syscall
const SYSCALL_MMAP: usize = 222;
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
        SYSCALL_MMAP => sys_mmap(args[0], args[1], args[2]),
        SYSCALL_MUNMAP => sys_munmap(args[0], args[1]),
        SYSCALL_SBRK => sys_sbrk(args[0] as i32),
        _ => panic!("Unsupported syscall_id: {}", syscall_id),
    }
}
```

---

## 6. 测试方法

### 6.1 在 `os` 目录下执行

```sh
# 题目指定的两条命令
make run TEST=1
make run TEST=2

# 覆盖本章 normal 测例（get_time / sleep / trace / mmap / munmap）的本地命令
make run TEST=4 BASE=0
```

> **关于 `TEST` 语义的说明（重要）**
>
> 本仓库模板的 `user/Makefile` 中：
> - `TEST=1` 是特殊值，表示编译 `user/src/bin` 下所有 `ch*.rs`，即第 2~9 章的全部测例；
> - `TEST=n (n≥2)` 配合 `BASE` 选择第 `BASE..n` 章的测例：`BASE=1` 为 basic 测例，`BASE=0` 为 normal 测例，`BASE=2` 为两者。
>
> 因此 `make run TEST=2` 实际选择的是第 1~2 章 basic 测例；本章的 `ch4_*` 测例不在其中。要单独跑本章的 normal 测例，应使用 `make run TEST=4 BASE=0`；官方 CI 会自动使用 `BASE=2, TEST=4`，同时覆盖 basic 与 normal 测例。`make run TEST=1` 因包含第 5 章及以后的测例，运行到第一个未实现的系统调用时会由模板的 `_ => panic!` 终止——这是本实验只实现到第 4 章的正常现象，第 2~4 章的相关输出均已正常出现。

### 6.2 在项目源码根目录下执行官方 CI

```sh
make -C ci-user test CHAPTER=4
```

该命令会：

1. 备份 `os/`、`ci-user/user/`、`ci-user/check/`；
2. 随机化测例中的 `OK` / `passed` 标记，防止硬编码输出；
3. 用 `BASE=2 TEST=4` 编译第 2~4 章 basic + normal 测例；
4. 在 QEMU 中运行，并用 `ci-user/check/ch4.py` 检查期望/非期望输出；
5. 检查 `reports/lab1.md`、`reports/lab2.md` 是否存在；
6. 恢复被备份的文件。

本仓库中 `report/report_gettime.md` 与 `report/report_mmap_munmap.md` 是正式报告，`reports/lab1.md`、`reports/lab2.md` 是指向它们的符号链接，用于满足 CI 的报告存在性检查。

### 6.3 推荐的完整验证顺序

```sh
cd os
make run TEST=4 BASE=0          # 本章 normal 测例全部通过
make run TEST=2                 # 题目指定的命令，验证 basic 测例未被破坏
make run TEST=1                 # 观察第 2~4 章全部输出（第 5 章起会 panic）
cd ..
make -C ci-user test CHAPTER=4  # 官方 16/16 检查
```

---

## 7. 实验现象与结果分析

### 7.1 `make run TEST=4 BASE=0` 的实际输出（节选）

多道程序轮转执行，输出会交错，完整运行到 `All applications completed!`：

```text
current time_msec = 117
Test 04_1 OK!
[kernel] PageFault in application, bad addr = 0x10000000, bad instruction = 0x42e, kernel killed it.
[kernel] PageFault in application, bad addr = 0x10000000, bad instruction = 0x42c, kernel killed it.
Test 04_4 test OK!
Test trace_1 OK!
Test 04_5 ummap OK!
Test 04_6 ummap2 OK!
get_time OK! 126
time_msec = 218 after sleeping 100 ticks, delta = 101ms!
Test sleep1 passed!
string from task trace test
Test trace OK!
Test sleep OK!
[kernel] Panicked at src/task/mod.rs:192 All applications completed!
```

结果分析：

- `get_time OK! 126` 说明 `sys_get_time` 已能正确把时间写回用户栈上的 `TimeVal`；若仍按第三章方式直接解引用，这里会变成内核缺页 panic 或写入垃圾值。
- `time_msec = 218 after sleeping 100 ticks, delta = 101ms!`：`sleep(100)` 以毫秒为单位，实际测得 101 ms，误差来自 100 Hz 的定时器中断粒度（每个 tick 10 ms），说明秒/微秒换算正确。
- `Test sleep OK!` 需要循环等待 3000 ms，说明 `get_time` 在多任务、多次系统调用下持续正确。
- `Test trace OK!` 与 `Test trace_1 OK!` 说明 `sys_trace` 的读、写、计数以及权限检查（U/R/W）均正确；`ch4_trace1` 中的 `isize::MAX`、`0x80200000`、只读 mmap 页等边界都返回了 `-1`。
- 两行 `PageFault ... bad addr = 0x10000000` 是 `ch4_mmap1` / `ch4_mmap2` 故意触发的写/读保护异常，属于预期现象；测例要求不出现 `Should cause error, Test 04_2/04_3 fail!`，实际输出中确实没有。

### 7.2 `make run TEST=2` 的实际输出（节选）

```text
[kernel] IllegalInstruction in application, kernel killed it.
[kernel] IllegalInstruction in application, kernel killed it.
Hello, world from user mode program!
Test power_5 OK!
[kernel] PageFault in application, bad addr = 0x0, bad instruction = 0x3a4, kernel killed it.
Test power_3 OK!
Test power_7 OK!
[kernel] Panicked at src/task/mod.rs:192 All applications completed!
```

该命令按 Makefile 语义选择第 1~2 章 basic 测例，全部通过；它不包含第 4 章 mmap 测例，mmap 的验证见 `TEST=1` / `TEST=4 BASE=0` / 官方 CI。

### 7.3 `make run TEST=1` 的实际输出（节选）

```text
get_time OK! 73
Test 04_1 OK!
Test 04_4 test OK!
Test trace_1 OK!
Test 04_5 ummap OK!
Test 04_6 ummap2 OK!
Test sbrk start.
Test sbrk almost OK!
[kernel] Panicked at src/syscall/mod.rs:45 Unsupported syscall_id: 172
```

`TEST=1` 编译了全部 86 个测例，因此第 5 章 `ch5_getpid` 调用 `sys_getpid`（172）时，模板的 `_ => panic!` 终止了内核。这是“只实现到第 4 章”的预期行为；第 2~4 章的全部相关输出都已在终止前出现。

### 7.4 官方 CI 结果

```text
[PASS] found <get_time OK####! (\d+)>
[PASS] found <Test sleep OK####!>
[PASS] found <current time_msec = (\d+)>
[PASS] found <time_msec = (\d+) after sleeping (\d+) ticks, delta = (\d+)ms!>
[PASS] found <Test sleep1 passed####!>
[PASS] found <string from task trace test>
[PASS] found <Test trace OK####!>
[PASS] found <Test 04_1 OK####!>
[PASS] found <Test 04_4 test OK####!>
[PASS] found <Test 04_5 ummap OK####!>
[PASS] found <Test 04_6 ummap2 OK####!>
[PASS] found <Test trace_1 OK####!>
[PASS] not found <FAIL: T.T>
[PASS] not found <Test sbrk failed!>
[PASS] not found <Should cause error, Test 04_2 fail!>
[PASS] not found <Should cause error, Test 04_3 fail!>

Test passed####: 16/16
Report for lab1 found.
Report for lab2 found.
```

其中 `####` 是 CI 的 `randomize` 目标在每次运行时生成的随机后缀（例如 `29976`），用于确认我们没有在代码里硬编码测例输出。命令最终退出码为 `0`。

### 7.5 测试结果汇总

| 测例 | 验证内容 | 结果 |
| --- | --- | --- |
| `ch3_sleep` | `get_time` 返回正值，循环等待 3000 ms | PASS |
| `ch3_sleep1` | 秒/微秒换算、100 ms 睡眠误差 | PASS（delta ≈ 101 ms） |
| `ch3_trace` | `sys_trace` 读/写/计数，计数含自身 | PASS |
| `ch4_trace1` | 非法地址、内核地址、只读 mmap 页、munmap 后访问 | PASS |
| 官方 CI `ch4.py` | 12 条期望 + 4 条非期望 | 16/16 PASS |

---

## 8. 遇到的问题与解决

### 问题 1：直接解引用用户指针导致内核 panic

**现象**：最初仍按第三章的方式写 `*ts = ...`，`ch3_sleep` 触发内核缺页；`trap_from_kernel()` 打印 `a trap ... from kernel!` 后停机。

**原因**：陷入内核后 `satp` 已切换到内核页表，`ts`（用户虚拟地址）在内核页表中没有对应映射。

**解决**：引入 `user_page()`，用 `PageTable::from_token(current_user_token())` 翻译用户页表，拿到物理页后通过恒等映射写入。

### 问题 2：`TimeVal` 跨页

**现象**：只翻译起始页并连续写 16 字节时，如果 `ts` 位于页末，第二个字段会写到下一页物理内存，可能破坏无关页面。

**解决**：`write_user_bytes()` 按 `PAGE_SIZE - offset` 分段，循环处理每一页；每一页都重新翻译、重新校验权限。

### 问题 3：`isize::MAX` 被截断后指向 trampoline

**现象**：`VirtAddr::from(isize::MAX)` 截断为 39 位后，VPN 与 trampoline 相同；只检查 `is_valid()` 会把 trampoline 的字节返回，`ch4_trace1` 失败。

**解决**：用户指针访问必须要求 PTE 带 `U` 位；trampoline 和 `TrapContext` 都是 `R|X` 或 `R|W`、无 `U`，因此被正确拒绝。`0x80200000` 在内核页表中有映射，但在用户页表中不存在，`translate` 返回 `None`，同样返回 `-1`。

### 问题 4：只读 mmap 页的写入

**现象**：`ch4_trace1` 要求 `trace_write` 对 `prot=1` 的只读页返回 `-1`。

**解决**：`user_page(addr, for_write)` 在写访问时检查 `pte.writable()`，读访问时检查 `pte.readable()`；U 位之外再校验 RW 权限。

### 问题 5：`sys_trace` 计数必须包含自身

**现象**：若在 `syscall()` 分发之后计数，`count_syscall(SYSCALL_TRACE)` 会少 1，`ch3_trace` 的 `assert_eq!(2, ...)`、`assert_eq!(7, ...)` 失败。

**解决**：在 `trap_handler()` 中、调用 `syscall()` 之前执行 `TASK_MANAGER.add_syscall_count(syscall_id)`；计数完成后立即释放 `RefCell` 借用，避免与 `sys_trace` 内的 `get_syscall_count()` 冲突。

### 问题 6：`translate()` 返回 `Some` 不等于已映射

**现象**：冲突检查若写成 `translate(vpn).is_some()`，会把“页表项存在但 V=0”的页误判为已映射，导致 mmap 误报 -1（本报告中 mmap 部分也用到这一点）。

**解决**：一律使用 `pte.is_valid()` 判断。

### 问题 7：在 `#![deny(warnings)]` 下保持零警告

新增的辅助函数、导入、字段都必须被实际使用或有 `#[allow(unused)]`；最终 `cargo fmt --check` 与 `cargo build --release` 均通过。

---

## 9. 总结

本实验完成了 `sys_get_time` 在 SV39 虚存环境下的重写，核心是：

1. **认清内核与用户地址空间的切换**：系统调用运行在内核页表中，用户指针不能直接解引用。
2. **通过用户页表翻译**：`PageTable::from_token(current_user_token())` + `translate()` + `phy page`，既能访问用户内存，又不需要切换 `satp`。
3. **逐页处理**：`TimeVal` 可能跨页，必须按页拆分拷贝。
4. **权限校验**：U 位、R/W 位缺一不可；这既是安全要求，也是 `ch4_trace1` 边界用例的通过条件。

同时，为满足本章官方测例，按同样的思路重写了 `sys_trace`，并在陷入内核时按任务维护系统调用计数。最终：

- `make run TEST=4 BASE=0`：第 3~4 章 normal 测例全部通过；
- `make run TEST=1` / `make run TEST=2`：题目指定的命令均可正常执行，第 2~4 章输出正确；
- `make -C ci-user test CHAPTER=4`：官方检查 **16/16 PASS**，退出码 0。

通过本实验，我对“地址空间隔离”“用户/内核页表切换”“页表项权限位”和“跨页访问”有了更具体的理解，也为后续实现 mmap/munmap 打下了基础。
