# rCore 第五章实验报告（lab3）：进程管理与 `sys_spawn`

> 实验环境：Rust nightly-2024-05-02 + RISC-V SV39 + QEMU virt + rCore-Tutorial（ch5 实验分支）
>
> 涉及文件：`os/src/syscall/process.rs`、`os/src/syscall/mod.rs`、`os/src/task/task.rs`、`os/src/task/manager.rs`、`os/src/task/processor.rs`、`os/src/task/id.rs`、`os/src/task/mod.rs`、`os/src/mm/memory_set.rs`、`os/src/mm/frame_allocator.rs`、`os/src/mm/mod.rs`、`os/src/sync/{mod.rs,spin.rs}`、`os/src/trap/mod.rs`、`os/src/main.rs`、`os/src/entry.asm`、`os/src/sbi.rs`、`os/src/config.rs`、`os/Makefile`

---

## 1. 实验目的

1. 理解进程（任务）控制块 TCB 的组织方式，以及 `fork`、`exec`、`spawn` 三者在地址空间处理上的本质区别。
2. 实现新的系统调用 `sys_spawn(path)`（syscall ID = 400）：**不复制父进程地址空间**，直接从 ELF 构造新进程。
3. 完善 `spawn` 的健壮性：对无效程序名、进程池满、物理内存不足三类错误返回 `-1`，而不是让内核 panic。
4. 补齐本章测例依赖的其余功能：`sys_set_priority` 与 stride 调度（`ch5_setprio`、`ch5_stride`），以及第三章/第四章实验中的 `sys_get_time`、`sys_mmap`、`sys_munmap` 在 ch5 内核上的移植。
5. （挑战）实现多核支持：让内核在 QEMU `-smp N` 下启动其余 hart，并保证用户进程可以被多个 hart 并行调度执行。
6. 通过 `make run TEST=1` 与 `make -C ci-user test CHAPTER=5` 的全部检查。

---

## 2. 实验内容概览

本章的主任务是 `sys_spawn`。由于官方 `ch5_usertest` 还会一次性运行 ch2b、ch3、ch4、ch5 的测例，因此为了让两条验收命令全部通过，还需要在本章内核中补齐：

| 功能 | 相关测例 | 说明 |
| --- | --- | --- |
| `sys_spawn` (400) | `ch5_spawn0`、`ch5_spawn1`、`ch5_usertest` | 本章核心 |
| `sys_set_priority` (140) | `ch5_setprio`、`ch5_stride*` | 优先级与 stride 调度 |
| `sys_get_time` (169) | `ch3_sleep`、`ch3_sleep1`、`ch5_stride*` | 前一实验成果移植 |
| `sys_mmap` (222) / `sys_munmap` (215) | `ch4_mmap0~3`、`ch4_unmap`、`ch4_unmap2` | 前一实验成果移植 |
| 多核支持（挑战） | 额外验证 | QEMU `-smp 2/4/8` |

---

## 3. `sys_spawn` 的设计与实现

### 3.1 `fork + exec` 与 `spawn` 的区别

- `fork` 调用 `MemorySet::from_existed_user`，逐页复制父进程的整个用户地址空间；
- `exec` 在当前进程上替换 `memory_set`；
- `spawn` 则应为新进程**直接构造**一个全新的地址空间，只加载 `path` 对应的 ELF，不复制父进程的任何用户页。

因此 `spawn` 不能简单实现为 `fork` 后再 `exec`：那样会先做一次无意义的整地址空间复制。正确做法是在 `TaskControlBlock` 上新增 `spawn` 方法，内部调用 `MemorySet::from_elf(elf_data)`，这与初始化 `initproc` 的 `TaskControlBlock::new` 类似，区别在于需要设置父进程并把新 TCB 挂到父进程的 `children` 链表中。

### 3.2 新进程的构造

`sys_spawn` 的处理流程：

1. 通过 `translated_str(current_user_token(), path)` 从用户地址空间读取以 `\0` 结尾的程序名；
2. 通过 `get_app_data_by_name` 在内核内置的 ELF 表中查找程序，找不到即返回 `-1`（无效文件名）；
3. 预估加载该 ELF 所需物理页，检查剩余页框是否足够，不足返回 `-1`（内存不足）；
4. 调用 `current_task().spawn(elf_data)`：
   - `MemorySet::from_elf` 建立用户页表（代码段、数据段、用户栈、TrapContext、trampoline）；
   - 分配 PID 与内核栈（失败返回 `None`）；
   - 构造 TCB：`parent = Some(Arc::downgrade(self))`，`children` 为空，`task_cx = goto_trap_return(kernel_stack_top)`；
   - 写入 `TrapContext::app_init_context(entry_point, user_sp, kernel_token, kernel_sp, trap_handler)`；
   - 把新 TCB 加入父进程的 `children`；
5. 成功后将新任务 `add_task` 放入就绪队列，返回 `pid`。

由 `from_elf` 构造的地址空间与父进程完全独立，满足“不复制父进程地址空间”的约束。

### 3.3 错误处理

题目要求处理三类错误，统一返回 `-1`：

**(1) 无效文件名**

`get_app_data_by_name` 返回 `None` 时直接返回 `-1`，不创建任何资源。

**(2) 进程池满**

ch5 模板中的 `RecycleAllocator` 是无上限的，进程数只受物理内存限制。本实验在 `config.rs` 中引入进程池上限：

```rust
pub const MAX_PROCESS: usize = 128;
```

并给 `RecycleAllocator` 增加 `try_alloc() -> Option<usize>`：只有 `current < MAX_PROCESS` 时才允许分配新编号，回收的 PID 优先复用。`pid_alloc/kstack_alloc` 保留原语义（满时 panic），而 `spawn` 路径使用新增的 `try_pid_alloc()/try_kstack_alloc()`；任一失败都返回 `None`，`sys_spawn` 转为 `-1`。若 PID 分配成功而内核栈分配失败，`PidHandle` 的 `Drop` 会自动归还 PID，不泄漏资源。

**(3) 内存不足**

`MemorySet::from_elf` 内部大量使用 `frame_alloc().unwrap()`，一旦页框耗尽会 panic。为了让 `spawn` 可以优雅失败，在真正创建地址空间之前做一次保守的容量检查：

- `frame_allocator.rs` 新增 `available_frames()`：返回 `recycled.len() + (end - current)`，即当前可分配的物理页框数；
- `memory_set.rs` 新增 `elf_load_frames(elf_data)`：解析 ELF 的 `PT_LOAD` 段，累加 `ceil(mem_size / PAGE_SIZE)`，再加上页表页、用户栈、TrapContext、内核栈以及 16 页安全余量；
- `sys_spawn` 在 `available_frames() < elf_load_frames(...)` 时返回 `-1`。

这是“先检查后分配”的保守方案：单核下检查与分配之间不会插入其它分配者，因此不会误判成功；极端情况下内核堆分配失败仍可能 panic，但物理页框不足这一主要 OOM 场景已被覆盖。更彻底的做法是把 `PageTable::new`、`MapArea::map_one` 等整条分配链改成返回 `Result`，本实验选择了对现有代码侵入更小的方案。

### 3.4 与退出、等待机制的配合

`spawn` 产生的子进程与 `fork` 一样挂在父进程 `children` 中，仍使用既有的僵尸进程与 `waitpid` 机制：子进程退出时把 `task_status` 置为 `Zombie`、记录 `exit_code`，父进程 `waitpid` 取走并释放 TCB。`ch5_spawn1` 同时验证了按 PID 等待（`waitpid`）与任意子进程等待（`wait`）的返回值和退出码。

---

## 4. 优先级与 stride 调度

`ch5_setprio` 要求：

- `set_priority(10) == 10`
- `set_priority(isize::MAX) == isize::MAX`
- `set_priority(0/1/-10) == -1`

即合法优先级必须 `>= 2`，成功时返回设置后的优先级，非法时返回 `-1`。为此在 `TaskControlBlockInner` 中增加：

```rust
pub priority: usize,
pub pass: usize,
```

并实现 `set_priority`：优先级 `< 2` 返回 `-1`，否则写入并返回新优先级。`fork` 继承父进程优先级，新进程默认优先级为 16。

调度器由 FIFO 改为 stride 调度：

- 常量 `BIG_STRIDE = 10000`；
- 任务入队时 `pass += BIG_STRIDE / priority`；
- `fetch` 遍历就绪队列，选择 `pass` 最小的任务；
- 时间片用完（时钟中断）或主动 `yield` 时任务重新入队，`pass` 随之推进。

这样每个进程获得的 CPU 时间与其优先级近似成正比。`ch5_stride` 的实测输出中，`exitcode / priority` 基本稳定在同一量级，说明比例公平性符合预期。注意：`ch5_stride0~5` 本身不包含断言，最终以 `ch5_usertest` 的退出码与输出为准。

---

## 5. 前序系统调用在 ch5 内核上的移植

### 5.1 `sys_get_time`

不能直接解引用用户指针，必须通过当前进程的页表翻译。实现要点：

- 用 `current_user_token()` 取得 satp，通过 `translated_refmut` 写入；
- `TimeVal { sec, usec }` 占 16 字节，可能跨页；将其拆成两个 `usize` 分别翻译写入，保证每个字段都只落在一个物理页内；
- 返回 `0`，时间单位为微秒（`get_time_us`）。

### 5.2 `sys_mmap` / `sys_munmap`

`MemorySet` 增加两个方法：

- `mmap(start, end, perm) -> bool`：要求 `start` 页对齐、范围非空，并与所有已有 `MapArea` 做区间重叠检查（`start_vpn < area.end && area.start < end_vpn`）；通过后放入一个新 `MapArea`（`MapType::Framed`，权限 `U | R/W/X`）。
- `munmap(start, end) -> bool`：要求 `start` 页对齐，查找起止 VPN 完全相同的区域，移除并解除映射。

系统调用层：

- `sys_mmap`：`start` 必须页对齐、`len > 0`、`port ∈ [1,7]`；`port` 的 1/2/4 位分别对应 R/W/X，通过 `(port << 1)` 转换为 `MapPermission` 的位定义；成功返回 0，否则 -1。
- `sys_munmap`：`start` 页对齐、`len > 0`，且必须与某个已映射区域**完全一致**才成功。

这里有一个由测例驱动的关键细节：`ch4_unmap` 中 `mmap(start - len, len + 1, prot)` 期望成功，说明 **`len` 本身不需要页对齐**，只有 `start` 需要；结束地址按 `ceil` 对齐。最初把 `len` 也要求对齐导致该测例失败（`Test 04_5` 未通过），修正后 `ch4_unmap` 正常。

---

## 6. 挑战：多核支持

### 6.1 启动流程

原 `entry.asm` 只有一份启动栈，`_start` 直接把 `a0` 丢弃。多核改造如下：

- `_start` 把 RustSBI 传入的 hart id 保存到 `tp`；
- 新增 `secondary_entry`：由 SBI HSM 启动的 hart 从这里开始执行，`a0` 为 hart id，用 `sp = secondary_stack_top - (hartid << 16)` 选取每 hart 独立的 16 KiB 启动栈，然后跳入 `secondary_main`；
- `.bss.stack` 中为 boot hart 保留 16 KiB 栈，并为最多 8 个 hart 各保留 16 KiB 次级栈。

`rust_main` 在完成 `clear_bss`、内存管理初始化、`initproc` 创建、时钟与陷阱初始化之后：

1. 取得大内核锁；
2. 调用 SBI HSM (`EID = 0x48534D`, `FID = 0`) 的 `hart_start(hartid, secondary_entry, hartid)` 启动 1..8 号 hart；对不存在的 hart，SBI 返回 `SBI_ERR_INVALID_PARAM`，忽略即可；
3. 进入 `run_tasks`。

`secondary_main` 先取得大内核锁，然后：

- `KERNEL_SPACE.activate()`：HSM 启动的 hart 初始 `satp = 0`（裸模式），需要切换到内核页表；
- 重新设置本 hart 的 `stvec`、使能 S 态时钟中断并设置下一次触发；
- 进入 `run_tasks`，与 boot hart 共享同一个就绪队列。

### 6.2 每 hart 的 Processor

原实现只有一个全局 `PROCESSOR`。现改为：

```rust
pub static ref PROCESSORS: Vec<UPSafeCell<Processor>> =
    (0..MAX_HARTS).map(|_| UPSafeCell::new(Processor::new())).collect();
```

`hart_id()` 通过 `mv {}, tp` 读取当前 hart 编号，`current_task/take_current_task/schedule/run_tasks` 都访问本 hart 的 `Processor`，从而每个 hart 有独立的 `current` 任务与 idle 上下文。各 hart 还拥有独立的 `sscratch`、`stvec` 和 SBI 定时器。

### 6.3 大内核锁（Big Kernel Lock）

模板中的 `UPSafeCell` 基于 `RefCell`，只在单核下成立。直接多核并发访问 `TASK_MANAGER`、`FRAME_ALLOCATOR`、`PID_ALLOCATOR`、`KERNEL_SPACE`、内核堆等全局结构会产生数据竞争。

本实验采用经典的大内核锁方案：

- `sync/spin.rs` 新增基于 `AtomicBool` 的自旋锁 `SpinLock`，以及全局 `BIG_KERNEL_LOCK`；
- **进入内核时加锁**：`trap_handler` 的第一条语句获取大内核锁；
- **返回用户态前解锁**：`trap_return` 在完成 `set_user_trap_entry`、读取用户 token、计算 trampoline 入口之后、执行 `jr __restore` 之前释放锁；
- **调度切换期间保持加锁**：`suspend_current_and_run_next`、`exit_current_and_run_next` 引起的 `__switch` 都在同一 hart 的内核执行流中完成，锁不释放；
- **空闲 hart 释放锁**：`run_tasks` 在没有就绪任务时先解锁、自旋，再重新加锁，避免空闲 hart 长期持锁导致其它 hart 无法进入内核。

正确性论证：

1. 所有内核态代码路径（系统调用、缺页处理、调度、进程创建/退出、物理页与堆分配）都处于大内核锁保护下，全局结构不会并发访问；
2. 用户态代码可以同时在多个 hart 上运行，因为用户态不访问内核全局数据；
3. 某个进程运行时其 TCB 只会在该进程陷入内核时被访问，而访问需要持有大内核锁；
4. `__restore` 在切换 satp 后执行 `sfence.vma`，进程在不同 hart 间迁移时不会读到陈旧的 TLB 项。

代价是内核态没有真正的并行（任意时刻只有一个 hart 在内核中），但用户态计算、页面错误、系统调用仍可在多个 hart 间重叠，足以体现多核调度；这是从单核内核过渡到多核的一个低风险起点。

### 6.4 测试方式

`os/Makefile` 新增 `SMP ?= 1`，并把 `-smp $(SMP)` 传入 QEMU。默认值保持 1，保证官方两条验收命令的行为与单核完全一致；需要验证多核时：

```sh
make run SMP=4 TEST=1
```

实测结果（使用官方 `check/ch5.py` 检查器对 QEMU 输出做匹配）：

| 配置 | 启动的次级 hart 数 | 检查结果 |
| --- | ---: | --- |
| `-smp 1` | 0 | 15/15 |
| `-smp 2` | 1 | 15/15 |
| `-smp 4` | 3 | 15/15 |
| `-smp 8` | 6 | 15/15 |

多核运行时 `ch5 Usertests passed!` 均正常输出，没有出现 `src/bin` 层断言失败或内核 panic。

---

## 7. 测试结果

### 7.1 `make run TEST=1`

在 `os` 目录执行 `make run TEST=1`：构建全部用户程序，内核以 `ch5_usertest` 作为 initproc（`INITPROC` 优先查找 `ch5_usertest`，不存在时回退到 `ch5b_initproc`），自动依次运行：

- `ch2b_hello_world`、`ch2b_power_3/5/7`
- `ch3b_yield0/1/2`、`ch3_sleep`、`ch3_sleep1`
- `ch4_mmap0~3`、`ch4_unmap`、`ch4_unmap2`
- `ch5_spawn0`、`ch5_spawn1`、`ch5_setprio`
- `ch5_stride`

最终输出：

```
Test spawn0 OK!
Test wait OK!
Test waitpid OK!
Test set_priority OK!
ch5 Usertests passed!
[kernel] Idle process exit with exit_code 0 ...
[kernel] Panicked at src/task/mod.rs:71 All applications completed!
```

最后的内核 panic 是 initproc（idle 进程）退出的既有设计，属于正常结束路径。用官方检查器核对该日志的输出，结果为 `Test passed: 15/15`。

### 7.2 `make -C ci-user test CHAPTER=5`

官方检查器流程（备份源码 → 覆盖测试程序 → 随机化输出标记 → 构建 → 运行 QEMU → `check/ch5.py` → 恢复源码 → 检查报告）全部通过：

```
[PASS] found <Test 04_1 OK...!>
[PASS] found <Test 04_4 test OK...!>
[PASS] found <Test 04_5 ummap OK...!>
[PASS] found <Test 04_6 ummap2 OK...!>
[PASS] found <Test wait OK...!>
[PASS] found <Test waitpid OK...!>
[PASS] found <Test spawn0 OK...!>
[PASS] found <Test set_priority OK...!>
...
Test passed...: 15/15
Report for lab1 found.
Report for lab2 found.
Report for lab3 found.
```

命令成功退出（退出码 0）。

---

## 8. 问题与分析

1. **`spawn` 的失败路径必须“无副作用”**：PID、内核栈、页框都要么成功建立并挂到新进程，要么在返回 `-1` 前由 Rust 的 `Drop` 自动回收。`try_pid_alloc()` 配合 `PidHandle` 的 `Drop`、以及先做页框容量检查，是实现这一点的关键。
2. **`mmap` 的 `len` 不要求页对齐**：这是由 `ch4_unmap` 测例反推出来的语义。最初按“start 与 len 都对齐”实现，导致 `mmap(start-len, len+1)` 被错误拒绝；修正为只检查 `start` 对齐、结束地址向上取整后通过。
3. **OOM 的彻底处理**：当前采用“剩余页框 + ELF 页数预估”的保守检查，避免绝大多数 OOM panic；若要完全消除内核堆分配失败带来的 panic，需要把页表/区域映射整条链改为可失败接口，这是后续可改进的方向。
4. **大内核锁的取舍**：实现简单、正确性容易论证，适合作为从单核到多核的第一步；但它牺牲了内核态并行度，且自旋等待会浪费 CPU。进一步优化可以按子系统拆分锁（任务管理器、页框分配器、内核堆各用一把锁），并采用 per-hart 就绪队列 + 工作窃取。
5. **进程池上限**：`MAX_PROCESS = 128` 远大于测例的并发需求（`ch5_spawn0` 峰值约 41 个进程，`ch5_usertest` 约 20 个），既能覆盖测例，又能在异常大量创建时返回 `-1` 而不是耗尽内核栈虚拟地址空间。
6. **多核下的输出**：多个用户进程并发 `println!` 时字符级输出可能交错；单核下同样存在交错，检查器使用正则匹配整行，默认 `SMP=1` 的官方验收路径不受影响；多核验证时 2/4/8 hart 也均能匹配到全部模式。

---

## 9. 总结

本实验以 `sys_spawn` 为核心，完成了：

- 不复制父进程地址空间、直接从 ELF 创建新进程的 `sys_spawn`，并完整处理无效文件名、进程池满、内存不足三类错误；
- 支撑本章测例的 `sys_set_priority` 与 stride 调度；
- 将 `sys_get_time`、`sys_mmap`、`sys_munmap` 移植到 ch5 内核；
- 挑战性的多核支持：SBI HSM 启动次级 hart、每 hart 调度器、大内核锁，在 `-smp 2/4/8` 下通过全部官方检查。

两条验收命令 `make run TEST=1` 与 `make -C ci-user test CHAPTER=5` 均成功，检查器结果 15/15，报告齐全。
