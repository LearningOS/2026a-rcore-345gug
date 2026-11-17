# rCore 第四章实验报告（二）：匿名映射 `sys_mmap` 与 `sys_munmap`

> 实验环境：Rust nightly-2024-05-02 + RISC-V SV39 + rCore-Tutorial（ch4-lab 分支）
>
> 涉及文件：`os/src/syscall/process.rs`、`os/src/mm/memory_set.rs`、`os/src/mm/page_table.rs`、`os/src/task/mod.rs`、`os/src/syscall/mod.rs`

---

## 1. 实验目的

1. 理解 SV39 下虚拟地址空间、页表映射和物理页框管理的基本原理。
2. 理解“匿名映射”（anonymous mapping）：只申请物理内存并建立页表映射，不关联任何文件。
3. 掌握 rCore 已有组件 `MemorySet`、`MapArea`、`PageTable`、`FrameAllocator`、`FrameTracker` 的使用方式。
4. 实现系统调用：

   ```rust
   fn sys_mmap(start: usize, len: usize, prot: usize) -> isize; // ID = 222
   fn sys_munmap(start: usize, len: usize) -> isize;            // ID = 215
   ```

5. 正确处理题目给出的所有错误分支：起止地址对齐、`prot` 编码、区间冲突、物理内存不足、munmap 区间内存在未映射页等。
6. 通过 `make run` 系列命令与官方 CI `make -C ci-user test CHAPTER=4` 的全部测例。

---

## 2. 相关原理

### 2.1 SV39 与页表

SV39 使用 39 位虚拟地址、56 位物理地址、4 KiB 页。虚拟地址被划分为：

```text
| 38..30 | 29..21 | 20..12 | 11..0 |
| VPN[2] | VPN[1] | VPN[0] | offset|
```

三级页表每级 512 项，每项 8 字节。页表项低 8 位：

| 位 | 名称 | 含义 |
| --- | --- | --- |
| 0 | V | 有效 |
| 1 | R | 可读 |
| 2 | W | 可写 |
| 3 | X | 可执行 |
| 4 | U | 用户态可访问 |
| 5 | G | 全局 |
| 6 | A | 已访问 |
| 7 | D | 已修改 |

rCore 中的 `PTEFlags` 与之一一对应；`MapPermission` 是“业务层权限”，只包含 `R/W/X/U` 四个标志，最终通过 `PTEFlags::from_bits(map_perm.bits)` 转成页表项。

### 2.2 `MemorySet`、`MapArea` 与物理页所有权

- `MemorySet` 表示一个地址空间：拥有一个 `PageTable` 和一组 `MapArea`。
- `MapArea` 表示一段连续的虚拟页区间（`VPNRange`），并保存 `MapType` 与 `MapPermission`。
- `MapType::Framed` 表示“每个虚拟页分配一个新物理页”，映射关系记录在 `data_frames: BTreeMap<VirtPageNum, FrameTracker>` 中。
- `FrameTracker` 实现了 `Drop`：当它被销毁时，物理页自动归还 `FrameAllocator`。因此**物理页的生命周期由 `MapArea` 中的 `FrameTracker` 决定**，而不是由页表项决定。

这一点对 mmap 的 OOM 分支非常重要：一旦某页已经建立了 PTE，就必须保证对应的 `FrameTracker` 仍然活着，否则页框会被回收，PTE 会指向可能被重新分配的物理页。

### 2.3 用户地址空间布局

`MemorySet::from_elf()` 为一个用户进程建立：

```text
0x0          ELF 各段（text/rodata/data/bss）
...          用户栈（R|W|U）
user_sp      用户栈顶
user_sp+4K   guard page（不映射）
...          TrapContext（R|W，无 U）
TRAMPOLINE   trampoline（R|X，无 U）
```

`sbrk` 在用户栈顶之上维护一个可增长的堆区域。mmap 则可以在用户指定的任意页对齐地址建立新的匿名映射，本实验的测例使用 `0x10000000` 附近。

### 2.4 匿名映射

Linux 的 `mmap` 可以把文件映射进地址空间；本实验做了简化：

- 不映射文件，只分配物理内存；
- 每个虚拟页分配一个物理页（`Framed`）；
- 权限由用户传入的 `prot` 决定；
- 成功返回 0，失败返回 -1。

### 2.5 页表项权限与用户/内核隔离

用户页必须带 `U` 位，否则用户态访问会触发缺页。mmap 建立的页一定是给用户用的，因此必须无条件加上 `MapPermission::U`。

RISC-V 规定：**W=1 时 R 必须为 1**，否则该 PTE 是保留编码。题目中的 `ch4_mmap2` 故意传入 `prot=2`（只写）并读取，预期触发 `LoadPageFault`。因此实现时**不能**“贴心地”为 W 自动补上 R，必须严格按 `prot` 转换。

### 2.6 TLB

修改页表后，用户态再次运行前需要刷新 TLB。rCore 的 `__restore` 在每次陷入返回时执行：

```asm
csrw satp, a1
sfence.vma
```

因此 munmap 之后返回用户态时，旧的地址翻译不会残留。

---

## 3. 需求梳理与全部错误分支

### 3.1 `sys_mmap(start, len, prot) -> isize`

功能：申请 `len` 字节物理内存，映射到 `start` 起始的虚拟地址区间；匿名、不映射文件。

约束与错误分支：

| 编号 | 条件 | 返回值 | 实现位置 |
| --- | --- | --- | --- |
| E1 | `start` 未按 `PAGE_SIZE` 对齐 | `-1` | `sys_mmap` |
| E2 | `prot` 的高位（bit3 及以上）非 0 | `-1` | `sys_mmap` |
| E3 | `prot & 0x7 == 0`（没有任何权限位） | `-1` | `sys_mmap` |
| E4 | `[start, start + ceil(len))` 内任意一页已被映射 | `-1` | `MemorySet::mmap` 冲突扫描 |
| E5 | 物理内存不足（页框或中间页表页分配失败） | `-1` | `MapArea::try_map_one` / `PageTable::map_checked` |
| E6 | 参数导致地址/长度乘法或加法溢出 | `-1` | `sys_mmap`（防御性检查） |
| S1 | 成功 | `0` | — |

其他约定：

- `len` 向上按页取整：`page_count = ceil(len / PAGE_SIZE)`。
- `len == 0` 时映射 0 页，视为成功（不涉及任何页）。
- `prot` 的 bit0/1/2 分别表示 R/W/X；mmap 页额外加 `U`。
- 简化约定：分配失败时**不回收**已经映射出去的页。实现中把“可能部分完成”的 `MapArea` 仍然 push 进 `areas`，从而保证已建立 PTE 的物理页不会被提前释放。

### 3.2 `sys_munmap(start, len) -> isize`

功能：取消 `[start, start + ceil(len))` 的映射。

约束与错误分支：

| 编号 | 条件 | 返回值 | 实现位置 |
| --- | --- | --- | --- |
| M1 | `start` 未按 `PAGE_SIZE` 对齐 | `-1` | `sys_munmap` |
| M2 | 区间内存在任意一个未映射的虚拟页 | `-1` | `MemorySet::munmap` 预检查 |
| M3 | 参数导致地址/长度溢出 | `-1` | `sys_munmap`（防御性检查） |
| S2 | 成功 | `0` | — |

其他约定：

- `len` 同样向上取整。
- 参数出错时按题目要求“不考虑内存回收与恢复”。本实现仍选择**先整体检查、后统一取消**，因此错误请求实际上不会改动任何映射，行为更安全。
- 成功时不仅要清空 PTE，还要删除 `MapArea::data_frames` 中对应的 `FrameTracker`，让物理页真正归还给 `FrameAllocator`。

### 3.3 边界语义

- `len == 0`：空区间，返回 0。
- 区间可以跨多个 `MapArea`（munmap 的实现支持）。
- munmap 区间可以只覆盖某个 `MapArea` 的前缀、后缀或中间部分；中间部分会触发 `MapArea` 拆分。

---

## 4. 整体实现流程（文字流程图）

### 4.1 `sys_mmap`

```text
用户调用 mmap(start, len, prot)
        │
        ▼
[1] start % PAGE_SIZE == 0 ? ──否──► return -1
        │是
        ▼
[2] prot & ~0x7 == 0 && prot & 0x7 != 0 ? ──否──► return -1
        │是
        ▼
[3] page_count = ceil(len / PAGE_SIZE)
    map_len    = page_count * PAGE_SIZE   （检查溢出）
    start + map_len 是否溢出？ ──是──► return -1
        │否
        ▼
[4] 把 prot 逐位转换成 MapPermission：
       U 必加；bit0→R，bit1→W，bit2→X
        │
        ▼
[5] 取得当前任务的 MemorySet
        │
        ▼
[6] 对 [start_vpn, start_vpn + page_count) 的每一页：
        translate(vpn)
          ├─ 不存在            → 空闲
          ├─ 存在但 !is_valid  → 空闲
          └─ is_valid          → return -1（E4）
        │
        ▼
[7] 创建 MapArea(Framed)（可能 0 页）
        │
        ▼
[8] 逐页 try_map_one：
        frame_alloc()
          ├─ None → 失败
          └─ Some(frame) → 记录到 data_frames
        page_table.map_checked(vpn, ppn, flags)
          ├─ 中间页表分配失败 → 失败
          └─ 成功 → 继续下一页
        失败时停止，但保留已映射的页（不回收）
        │
        ▼
[9] push MapArea，返回 0 / -1
```

### 4.2 `sys_munmap`

```text
用户调用 munmap(start, len)
        │
        ▼
[1] start % PAGE_SIZE == 0 ? ──否──► return -1（M1）
        │是
        ▼
[2] 计算 page_count、map_len，检查溢出 ──溢出──► return -1
        │
        ▼
[3] 预检查区间内每一页：
        translate(vpn) 存在且 is_valid ?
          ├─ 否 → return -1（M2，且不改动任何映射）
          └─ 是 → 继续
        │
        ▼
[4] 对每个 MapArea 调用 unmap_range：
        ┌─────────────────────────────────────────┐
        │ 计算与本区间的重叠 [lo, hi)              │
        │ 对重叠内每一页：                          │
        │   data_frames.remove(vpn)  // 归还物理页  │
        │   page_table.unmap(vpn)    // 清空 PTE    │
        │ 调整本 area 的 vpn_range：                │
        │   - 全部覆盖：变成空区间                  │
        │   - 覆盖前缀：start 后移                  │
        │   - 覆盖后缀：end 前移                    │
        │   - 覆盖中间：拆出一个 tail MapArea       │
        └─────────────────────────────────────────┘
        │
        ▼
[5] 把拆分出的 tail 追加到 areas，返回 0
```

### 4.3 系统调用注册

```text
用户 ecall (a7 = 222 / 215)
        │
        ▼
trap_handler: cx.sepc += 4; syscall(cx.x[17], ...)
        │
        ▼
syscall() match:
    SYSCALL_MMAP   => sys_mmap(args[0], args[1], args[2])
    SYSCALL_MUNMAP => sys_munmap(args[0], args[1])
        │
        ▼
sys_mmap / sys_munmap 参数校验
        │
        ▼
TaskManager::mmap_current / munmap_current
        │（获取 TASK_MANAGER 的 RefCell 借用）
        ▼
当前任务 MemorySet::mmap / munmap
        │
        ▼
PageTable / FrameAllocator
```

---

## 5. `prot` → `MapPermission` 权限转换逻辑

### 5.1 两套编码完全不同

用户 ABI 的 `prot`：

| 位 | 值 | 含义 |
| --- | --- | --- |
| bit0 | `0x1` | 可读 |
| bit1 | `0x2` | 可写 |
| bit2 | `0x4` | 可执行 |
| bit3.. | — | 必须为 0 |

内核 `MapPermission`：

| 标志 | 值 | 含义 |
| --- | --- | --- |
| `R` | `1 << 1 = 0x2` | 可读 |
| `W` | `1 << 2 = 0x4` | 可写 |
| `X` | `1 << 3 = 0x8` | 可执行 |
| `U` | `1 << 4 = 0x10` | 用户态可访问 |

两套编码整整错开一位，并且 `MapPermission` 还多一个 `U` 标志。**绝不能把 `prot` 直接当作 `MapPermission` 的 bits 使用**。

### 5.2 错误示例：直接复用 `prot`

假设写：

```rust
let permission = MapPermission::from_bits_truncate(prot); // 错误
```

- `prot = 1`（用户要求只读）：bit0 不是 `MapPermission` 的已定义位，`from_bits_truncate` 得到空权限，PTE 只有 V，用户访问立即缺页；
- `prot = 2`（用户要求只写）：bit1 被解释成 `MapPermission::R`，结果变成只读，而且没有 U；
- `prot = 3`（用户要求读写）：bit1 被解释成 R，bit0 被丢弃，结果只有 R；
- `prot = 7`（用户要求 RWX）：被解释成 R|W，丢失 X，且没有 U。

因此必须显式逐位转换。

### 5.3 正确实现

```rust
// 用户 prot：bit0=R, bit1=W, bit2=X
// 内核 MapPermission：R=1<<1, W=1<<2, X=1<<3, U=1<<4
// 两者编码不同，必须逐位翻译，不能直接复用 prot 的值。
let mut permission = MapPermission::U; // mmap 的页一定是用户页
if prot & 0x1 != 0 {
    permission |= MapPermission::R;    // bit0 -> R
}
if prot & 0x2 != 0 {
    permission |= MapPermission::W;    // bit1 -> W
}
if prot & 0x4 != 0 {
    permission |= MapPermission::X;    // bit2 -> X
}
```

转换结果对照：

| 用户 `prot` | 语义 | 生成的 `MapPermission` | 生成的 PTE 权限 |
| --- | --- | --- | --- |
| `0x1` | R | `U \| R` | V,R,U |
| `0x2` | W | `U \| W` | V,W,U（RISC-V 保留编码，读会缺页） |
| `0x3` | RW | `U \| R \| W` | V,R,W,U |
| `0x4` | X | `U \| X` | V,X,U |
| `0x5` | RX | `U \| R \| X` | V,R,X,U |
| `0x7` | RWX | `U \| R \| W \| X` | V,R,W,X,U |
| `0x0` / 高位非 0 | 非法 | — | 返回 -1 |

注意 `prot = 0x2` 时**不自动补 R**：题目 `ch4_mmap2` 正是要验证只写页在读取时触发 `LoadPageFault`。

---

## 6. 完整代码（逐块注释）

### 6.1 `os/src/syscall/process.rs`：`sys_mmap`

```rust
/// 匿名 mmap：申请 len 字节物理内存并映射到 start 起始的虚拟地址。
///
/// prot 为用户 ABI 编码：bit0=可读、bit1=可写、bit2=可执行，
/// 其余高位必须为 0。长度向上取整到整页；只要区间内任一页已被映射
/// 或物理内存不足，就返回 -1。
pub fn sys_mmap(start: usize, len: usize, prot: usize) -> isize {
    trace!("kernel: sys_mmap");

    // 题目要求：start 必须按页对齐
    if start % PAGE_SIZE != 0 {
        return -1;
    }
    // 题目要求：prot 只允许 bit0..bit2，且至少有一位
    if prot & !0x7 != 0 || prot & 0x7 == 0 {
        return -1;
    }

    // len 向上按页取整；同时做溢出保护（防御性）
    let page_count = len.div_ceil(PAGE_SIZE);
    let map_len = match page_count.checked_mul(PAGE_SIZE) {
        Some(v) => v,
        None => return -1,
    };
    if start.checked_add(map_len).is_none() {
        return -1;
    }

    // 用户 prot 编码与 MapPermission 编码不是同一套：
    // 用户 bit0/1/2 = R/W/X，内核 R=1<<1、W=1<<2、X=1<<3、U=1<<4。
    // 必须逐位转换；mmap 页一律加 U。
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

    // 交给当前任务的 MemorySet 完成冲突检查、分配与映射
    if mmap_current(VirtAddr::from(start), map_len, permission) {
        0
    } else {
        -1
    }
}
```

### 6.2 `os/src/syscall/process.rs`：`sys_munmap`

```rust
/// 取消 [start, start + len) 的映射。
///
/// 区间按页向上取整；只要其中有一页未映射，就返回 -1。
pub fn sys_munmap(start: usize, len: usize) -> isize {
    trace!("kernel: sys_munmap");

    // 起点必须页对齐，否则无法表达为整页区间；
    // 例如 ch4_unmap2 中 munmap(start + 1, len - 1) 必须返回 -1。
    if start % PAGE_SIZE != 0 {
        return -1;
    }

    // 与 mmap 相同：长度向上取整并检查溢出
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
```

### 6.3 `os/src/mm/memory_set.rs`：`MemorySet::mmap`

```rust
/// 把 [start, start + len) 映射为一个新的匿名（Framed）区域。
///
/// 返回 false 表示：区间内已有映射，或物理内存不足。
/// 按题目简化约定，分配失败时不回收已经建立的映射：把可能只完成
/// 一部分的 MapArea 仍然放入 areas，保证已安装 PTE 的物理页
/// 由 FrameTracker 持续持有，不会被提前释放。
pub fn mmap(&mut self, start: VirtAddr, len: usize, permission: MapPermission) -> bool {
    let page_count = len.div_ceil(PAGE_SIZE);
    if page_count == 0 {
        return true; // 空区间，直接成功
    }

    let start_vpn = start.floor();
    let end_vpn = VirtPageNum(start_vpn.0 + page_count);
    let mut area = MapArea::new(start, VirtAddr::from(end_vpn), MapType::Framed, permission);

    // 第一步：先扫描整个区间，任何一页已映射就整体拒绝。
    // 注意 translate 可能返回 Some(空 PTE)，所以必须再判断 is_valid()。
    for vpn in area.vpn_range {
        if self
            .page_table
            .translate(vpn)
            .map_or(false, |pte| pte.is_valid())
        {
            return false;
        }
    }

    // 第二步：逐页分配物理页并建立映射；中途失败则停止。
    let mut success = true;
    for vpn in area.vpn_range {
        if !area.try_map_one(&mut self.page_table, vpn) {
            success = false;
            break;
        }
    }

    // 即使只完成一部分，也保留该 area：它的 FrameTracker
    // 必须与已经建立的 PTE 同生命周期。
    self.areas.push(area);
    success
}
```

### 6.4 `os/src/mm/memory_set.rs`：`MemorySet::munmap`

```rust
/// 取消 [start, start + len) 的映射。
///
/// 只要区间内存在未映射的页就返回 false，并且不改变任何映射；
/// 全部页都有效时才逐页取消，并归还物理页。
pub fn munmap(&mut self, start: VirtAddr, len: usize) -> bool {
    let page_count = len.div_ceil(PAGE_SIZE);
    if page_count == 0 {
        return true;
    }

    let start_vpn = start.floor();
    let end_vpn = VirtPageNum(start_vpn.0 + page_count);

    // 预检查：整个区间必须全部已映射。
    // 这样参数错误的请求不会造成“取消了一半”的中间状态。
    for vpn in VPNRange::new(start_vpn, end_vpn) {
        match self.page_table.translate(vpn) {
            Some(pte) if pte.is_valid() => {}
            _ => return false,
        }
    }

    // 逐页取消，并让每个 MapArea 负责释放自己的 FrameTracker。
    let mut tails = Vec::new();
    for area in self.areas.iter_mut() {
        if let Some(tail) = area.unmap_range(&mut self.page_table, start_vpn, end_vpn) {
            tails.push(tail);
        }
    }
    self.areas.extend(tails);
    true
}
```

### 6.5 `os/src/mm/memory_set.rs`：`MapArea::try_map_one`

```rust
/// 尝试映射本 area 的一页；没有可用物理页时返回 false。
///
/// 即使 map_checked 失败，分配到的 frame 也已经记录在 data_frames 中，
/// 不会被释放——避免“PTE 已存在但物理页被回收”的悬空映射。
pub fn try_map_one(&mut self, page_table: &mut PageTable, vpn: VirtPageNum) -> bool {
    let ppn = match self.map_type {
        // Identical 映射直接使用虚拟页号作为物理页号（内核空间用）
        MapType::Identical => PhysPageNum(vpn.0),
        // Framed 映射：为每个虚拟页申请一个新物理页
        MapType::Framed => match frame_alloc() {
            Some(frame) => {
                let ppn = frame.ppn;
                self.data_frames.insert(vpn, frame); // 记录所有权
                ppn
            }
            None => return false, // 物理内存不足 → 上层返回 -1
        },
    };
    let pte_flags = PTEFlags::from_bits(self.map_perm.bits).unwrap();
    // map_checked：中间页表页分配失败时返回 false，而不是 panic
    page_table.map_checked(vpn, ppn, pte_flags)
}
```

### 6.6 `os/src/mm/memory_set.rs`：`MapArea::unmap_range`

```rust
/// 从本 area 中删除 [start_vpn, end_vpn)。
///
/// 调用者保证区间内每一页都已映射。如果删除区间严格位于本 area
/// 中间，则把剩余的后半段作为新的 MapArea 返回，保证 areas 中
/// 记录的 vpn_range 与真实映射保持一致。
fn unmap_range(
    &mut self,
    page_table: &mut PageTable,
    start_vpn: VirtPageNum,
    end_vpn: VirtPageNum,
) -> Option<MapArea> {
    let old_start = self.vpn_range.get_start();
    let old_end = self.vpn_range.get_end();

    // 与本 area 没有重叠
    if end_vpn <= old_start || old_end <= start_vpn {
        return None;
    }

    let remove_start = start_vpn.max(old_start);
    let remove_end = end_vpn.min(old_end);

    // 删除重叠部分：先移除 FrameTracker（归还物理页），再清空 PTE
    for vpn in VPNRange::new(remove_start, remove_end) {
        self.data_frames.remove(&vpn);
        page_table.unmap(vpn);
    }

    // 删除区间严格在中间：拆出后半段 [remove_end, old_end)
    let tail = if remove_start > old_start && remove_end < old_end {
        let mut tail = MapArea::new(
            VirtAddr::from(remove_end),
            VirtAddr::from(old_end),
            self.map_type,
            self.map_perm,
        );
        // 把后半段的物理页所有权转移到新 area
        for vpn in VPNRange::new(remove_end, old_end) {
            if let Some(frame) = self.data_frames.remove(&vpn) {
                tail.data_frames.insert(vpn, frame);
            }
        }
        Some(tail)
    } else {
        None
    };

    // 更新本 area 的虚拟页区间
    self.vpn_range = if remove_start == old_start && remove_end == old_end {
        // 整个 area 都被删除：保留一个空区间占位
        VPNRange::new(old_start, old_start)
    } else if remove_start == old_start {
        // 删除的是前缀
        VPNRange::new(remove_end, old_end)
    } else if remove_end == old_end {
        // 删除的是后缀
        VPNRange::new(old_start, remove_start)
    } else {
        // 删除的是中间，前半段保留
        VPNRange::new(old_start, remove_start)
    };
    tail
}
```

### 6.7 `os/src/mm/page_table.rs`：可失败的页表映射

原始的 `PageTable::map()` 在 `find_pte_create()` 中 `frame_alloc().unwrap()`，物理内存不足时会 panic。为了满足“物理内存不足返回 -1”的要求，新增可失败版本：

```rust
/// 与 find_pte_create 相同，但中间页表页分配失败时返回 None 而不是 panic。
fn find_pte_create_checked(&mut self, vpn: VirtPageNum) -> Option<&mut PageTableEntry> {
    let idxs = vpn.indexes();
    let mut ppn = self.root_ppn;
    let mut result: Option<&mut PageTableEntry> = None;
    for (i, idx) in idxs.iter().enumerate() {
        let pte = &mut ppn.get_pte_array()[*idx];
        if i == 2 {
            result = Some(pte);
            break;
        }
        if !pte.is_valid() {
            let frame = frame_alloc()?; // 失败则整体返回 None
            *pte = PageTableEntry::new(frame.ppn, PTEFlags::V);
            self.frames.push(frame);
        }
        ppn = pte.ppn();
    }
    result
}

/// 可失败版本的 map：已映射或页表页不足时返回 false。
pub fn map_checked(&mut self, vpn: VirtPageNum, ppn: PhysPageNum, flags: PTEFlags) -> bool {
    match self.find_pte_create_checked(vpn) {
        Some(pte) => {
            if pte.is_valid() {
                return false; // 冲突
            }
            *pte = PageTableEntry::new(ppn, flags | PTEFlags::V);
            true
        }
        None => false, // 物理内存不足
    }
}
```

### 6.8 `os/src/task/mod.rs`：当前任务地址空间的入口

```rust
/// 在当前任务的地址空间中尝试建立匿名映射。
pub fn mmap_current(&self, start: VirtAddr, len: usize, permission: MapPermission) -> bool {
    let mut inner = self.inner.exclusive_access();
    let cur = inner.current_task;
    inner.tasks[cur].memory_set.mmap(start, len, permission)
}

/// 在当前任务的地址空间中尝试取消映射。
pub fn munmap_current(&self, start: VirtAddr, len: usize) -> bool {
    let mut inner = self.inner.exclusive_access();
    let cur = inner.current_task;
    inner.tasks[cur].memory_set.munmap(start, len)
}
```

```rust
// 模块级包装函数，供 syscall 模块调用
pub fn mmap_current(start: VirtAddr, len: usize, permission: MapPermission) -> bool {
    TASK_MANAGER.mmap_current(start, len, permission)
}

pub fn munmap_current(start: VirtAddr, len: usize) -> bool {
    TASK_MANAGER.munmap_current(start, len)
}
```

### 6.9 `os/src/syscall/mod.rs`：系统调用注册

系统调用号在模板中已经给出，本实验只需保证分发到正确函数：

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

Linux/RISC-V 的系统调用 ABI 中，`mmap` 是 222、`munmap` 是 215；用户库的 `sys_mmap` / `sys_munmap` 通过 `ecall` 把 `a7` 设为对应编号，内核在 `trap_handler` 中从 `TrapContext.x[17]` 取出该编号。

---

## 7. 测试用例（全覆盖）

### 7.1 官方测例与覆盖的分支

| 测例 | 操作 | 覆盖内容 | 预期 |
| --- | --- | --- | --- |
| `ch4_mmap0` | `mmap(0x10000000, 4096, 3)`，逐字节写读 | 成功路径、R/W/U、整页映射 | `Test 04_1 OK!` |
| `ch4_mmap1` | `mmap(..., prot=1)`，写首字节 | 只读页写入保护 | `StorePageFault`，进程被杀；不得输出 `Should cause error, Test 04_2 fail!` |
| `ch4_mmap2` | `mmap(..., prot=2)`，读首字节 | W-only 保留编码、读保护 | `LoadPageFault`，进程被杀；不得输出 `Should cause error, Test 04_3 fail!` |
| `ch4_mmap3` | 先成功映射一页；再 `mmap(start-len, len+1)`；`mmap(start+len+1, len)`；`prot=0`；`prot\|8` | E1 未对齐、E2 高位、E3 零权限、E4 区间冲突（首末页覆盖） | 前一次 0，后四次均 `-1`；`Test 04_4 test OK!` |
| `ch4_unmap` | 映射两段；`munmap(start, len)`；`mmap(start-len, len+1)`；写读 4 页 | 成功 munmap、空洞重映射、跨 area | `Test 04_5 ummap OK!` |
| `ch4_unmap2` | 映射一页；`munmap(start, len+1)`；`munmap(start+1, len-1)` | M2 区间含未映射页、M1 未对齐起点 | 均 `-1`；`Test 04_6 ummap2 OK!` |
| `ch4_trace1` | 只读 mmap 页；`trace_write` 失败；`munmap` 后 `trace_read` 为 `None` | 页权限与映射生命周期 | `Test trace_1 OK!` |
| `ch4b_sbrk` | sbrk 申请/释放、越界释放、释放后写入 | 确认新实现未破坏原有 sbrk | `Test sbrk almost OK!`，不得出现 `Test sbrk failed!` |

### 7.2 错误分支自测清单

| 编号 | 自测输入 | 期望 |
| --- | --- | --- |
| T1 | `mmap(0x10000001, 4096, 3)` | `-1`（E1） |
| T2 | `mmap(0x10000000, 4096, 3 \| 8)` | `-1`（E2） |
| T3 | `mmap(0x10000000, 4096, 0)` | `-1`（E3） |
| T4 | 先 `mmap(A, 4096, 3)`；再 `mmap(A-4096, 4097, 3)` | 第二次 `-1`（E4，冲突页在区间尾部） |
| T5 | 先 `mmap(A, 4096, 3)`；再 `mmap(A, 4096, 3)` | 第二次 `-1`（E4，首页即冲突） |
| T6 | `mmap(A, 4096, 1)` 后写 | 用户态 `StorePageFault`（只读） |
| T7 | `mmap(A, 4096, 2)` 后读 | 用户态 `LoadPageFault`（W-only 保留编码） |
| T8 | `mmap(usize::MAX & !0xfff, 4096, 3)` 等极端长度/地址 | `-1`（溢出检查，防御性） |
| T9 | `munmap(A, 4097)`，其中第二页未映射 | `-1`（M2） |
| T10 | `munmap(A + 1, 4095)` | `-1`（M1） |
| T11 | `munmap(A, 0)` | `0`（空区间） |
| T12 | `mmap(A, 0, 3)` | `0`（空区间） |
| T13 | 成功 mmap 后 munmap 同一区间，再访问 | 用户态缺页 |
| T14 | 成功 mmap 一页后 munmap，再 mmap 同一页并写读 | 成功（验证物理页和 PTE 都已正确清理） |
| T15 | 大页数 mmap 直至物理内存耗尽（构造性测试） | `-1`（E5）；按约定已分配页不回收 |

官方测例覆盖了其中 T1~T4、T6、T7、T9、T10、T13；T5、T11、T12、T14、T15 通过实现逻辑与代码审查验证（T15 在 QEMU 默认内存下难以稳定构造，代码路径为 `frame_alloc() -> None` / `map_checked -> false`）。

### 7.3 运行方式

```sh
# 本章 normal 测例（mmap0..3 / unmap / unmap2 / trace1）
cd os
make run TEST=4 BASE=0

# 题目指定命令
make run TEST=1
make run TEST=2

# 官方 CI（自动选择 BASE=2, TEST=4）
cd ..
make -C ci-user test CHAPTER=4
```

---

## 8. 测试结果与现象分析

### 8.1 `make run TEST=4 BASE=0`（本章 normal 测例）

实际输出（节选，轮流调度导致顺序交错）：

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

现象分析：

- `Test 04_1 OK!`：mmap 成功，逐字节写入后读回完全一致，说明物理页确实分配、PTE 的 R/W/U 正确。
- 两条 `PageFault` 的 `bad addr = 0x10000000`：
  - 第一条 `bad instruction = 0x42e` 对应 `ch4_mmap1` 的写操作，触发 `StorePageFault`；
  - 第二条 `bad instruction = 0x42c` 对应 `ch4_mmap2` 的读操作，触发 `LoadPageFault`。
  两者都是测例故意触发的，内核按预期杀死进程；输出中没有 `Should cause error, Test 04_2/04_3 fail!`。
- `Test 04_4 test OK!`：四个错误分支全部返回 `-1`，包括“区间首部空闲、尾部冲突”的情况——这要求实现先扫描整个区间，而不是映射到冲突页时才 panic。
- `Test 04_5 ummap OK!`：munmap 之后原地址范围重新可写；跨过被取消的页重新 mmap 后，4 个页的读写都正确，说明 PTE 清理、物理页归还、重映射都正常。
- `Test 04_6 ummap2 OK!`：`len+1` 使区间覆盖到未映射的第二页，返回 `-1`；`start+1` 未对齐，返回 `-1`。
- `Test trace_1 OK!`：只读 mmap 页不可写、munmap 后不可读，说明 mmap/munmap 对页表和权限的影响符合预期。
- 运行结束时 `All applications completed!` 是模板在所有任务退出后的统一行为。

### 8.2 `make run TEST=1`

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

`TEST=1` 会编译 `user/src/bin` 下所有 `ch*.rs`，共 86 个测例（包含第 5~9 章）。本实验只实现到第 4 章，因此运行到第 5 章 `ch5_getpid` 的 `sys_getpid`（172）时被模板的 `_ => panic!` 终止。第 2~4 章的相关输出都已正确出现，mmap/munmap 测例全部通过。

### 8.3 `make run TEST=2`

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

按本仓库 `user/Makefile` 的语义，`TEST=2` 且默认 `BASE=1` 会选择第 1~2 章 basic 测例，不包含 `ch4_*`。该命令本身全部通过。本章 mmap/munmap 的完整验证由 `TEST=4 BASE=0` 与官方 CI 完成。

### 8.4 官方 CI `make -C ci-user test CHAPTER=4`

CI 会随机化测例中的 `OK` / `passed` 标记（例如变成 `OK29976`），防止硬编码输出。实际检查结果：

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

命令退出码为 `0`。

### 8.5 结果汇总

| 测试 | 结果 |
| --- | --- |
| `make run TEST=4 BASE=0` | 第 3~4 章 normal 测例全部通过 |
| `make run TEST=1` | 第 2~4 章输出正确，第 5 章因未实现 syscall 终止（预期） |
| `make run TEST=2` | 所选第 2 章 basic 测例全部通过 |
| `make -C ci-user test CHAPTER=4` | **16/16 PASS**，退出码 0 |
| `cargo fmt --check` | 通过 |
| `cargo build --release`（`#![deny(warnings)]`） | 通过，无警告 |

---

## 9. 踩坑点

### 9.1 `prot` 编码与 `MapPermission` 不匹配

这是本实验最容易出错的地方。两套权限编码错开一位：

```text
用户 prot:        bit0=R      bit1=W      bit2=X
MapPermission:    bit1=R      bit2=W      bit3=X      bit4=U
```

直接把 `prot` 当作 `MapPermission` 使用会导致：

- `prot=1` → 空权限（bit0 未定义）；
- `prot=2` → 只读（bit1 被当成 R）；
- `prot=3` → 只读（bit0 被丢弃）；
- 所有情况都缺少 `U`，用户态访问必然缺页。

必须逐位 `if prot & ...` 转换，并额外加上 `MapPermission::U`。

### 9.2 区间检查必须覆盖所有页

`ch4_mmap3` 的第二次调用是 `mmap(start-len, len+1, 3)`：

- 取整后是 2 页：`[start-len, start+len)`；
- 第一页 `start-len` 空闲；
- 第二页 `start` 已被第一次 mmap 映射。

如果实现“边检查边映射”，第一页会成功，第二页才触发 `PageTable::map()` 中的 `assert!(!pte.is_valid())`，内核直接 panic。正确做法是**先完整扫描整个区间**，任何一页冲突就返回 `-1`，不建立任何新映射。

同样，`translate(vpn).is_some()` 不等于已映射：叶子 PTE 可能是空项，必须 `pte.is_valid()`。

### 9.3 `len` 向上取整

测例中多次出现非整页长度：

- `mmap(start-len, len+1, prot)`：4097 字节 → 2 页；
- `munmap(start, len+1)`：4097 字节 → 2 页，第二页未映射 → `-1`。

实现中统一使用 `len.div_ceil(PAGE_SIZE)`，再乘以 `PAGE_SIZE` 得到实际映射长度。注意不要用 `[start, start+len]` 的闭区间直觉去映射 `len+1` 页——测例 `mmap(start-len, len+1)` 在 `ch4_unmap` 中期望只覆盖 2 页；如果多映射一页就会与已有映射冲突，返回 `-1`，测试失败。

### 9.4 页边界遍历与 `VPNRange`

- `MapArea::new(start_va, end_va, ...)` 内部使用 `start.floor()` 和 `end.ceil()`；传入的 `end_va` 应是 `start + page_count * PAGE_SIZE`，保证不会多算一页。
- `VPNRange::new(l, r)` 要求 `l <= r`。在实现 munmap 的区间更新时，最初对“整个 area 被删除”的情况处理错误：

  ```text
  start VPN:0x10001 > end VPN:0x10000!
  ```

  原因是同时把 `new_start` 设为 `remove_end`、`new_end` 设为 `remove_start`。修复方式是单独处理“全部覆盖”的情况，把区间设为 `[old_start, old_start)`；并分别处理前缀、后缀、中间三种情况。

### 9.5 物理页所有权与“不回收”约定

`FrameTracker` 的 `Drop` 会自动归还物理页。如果在映射失败时直接丢弃 `MapArea`：

- 已经建立的 PTE 仍指向这些物理页；
- 但这些物理页已经被归还，可能被下一次 `frame_alloc()` 重新分配；
- 结果是两个虚拟页共享同一物理页，数据互相覆盖。

题目允许“不处理分配失败时已经分配出去页面的回收”，因此实现选择把部分完成的 `MapArea` 仍然 push 到 `areas` 中，让 `FrameTracker` 与 PTE 同生命周期。这不是泄漏的“随意写法”，而是避免悬空映射的必要措施。

### 9.6 物理内存不足的处理

原始的 `PageTable::map()` 和 `MapArea::map_one()` 在分配失败时 `unwrap()`，会 panic。为了满足“物理内存不足返回 -1”：

- `MapArea::try_map_one()` 使用 `frame_alloc()` 的 `Option` 返回值；
- `PageTable::map_checked()` 使用可失败的 `find_pte_create_checked()`，中间页表页分配失败时返回 `false`。

这样 OOM 分支会沿 `try_map_one -> MemorySet::mmap -> sys_mmap` 返回 `-1`，而不是让内核崩溃。

### 9.7 munmap 的起点对齐

题目正文只写了“取消区间映射”，但 `ch4_unmap2` 的 `munmap(start + 1, len - 1)` 期望 `-1`。如果实现只按 `floor(start)` 取整，会认为该请求覆盖的是已映射的 `start` 页并返回 0，测试失败。因此 munmap 也必须要求 `start % PAGE_SIZE == 0`：非对齐的起点无法用整页区间表达。

### 9.8 先检查后取消 vs 边取消边检查

题目说“参数出错时不考虑内存回收与恢复”，因此边取消边检查在评测上也许能过；但实现选择**先预检查整个区间，再统一取消**：

- `munmap(start, len+1)` 中第二页未映射时，第一页不会被误取消；
- 行为更接近“要么全成功，要么全失败”，也更容易推理。

### 9.9 TLB 与页表一致性

munmap 清空 PTE 后，用户态可能仍缓存旧的 TLB 项。rCore 的 `__restore` 在每次返回用户态前执行 `csrw satp, a1; sfence.vma`，会刷新 TLB，因此测例中“munmap 后访问立即缺页”能够正确触发。如果自行修改返回路径，需要显式执行 `sfence.vma`。

### 9.10 `#![deny(warnings)]`

内核 crate 开启了 `#![deny(warnings)]`。新增的导入、辅助函数、结构体字段必须被使用，或者标注 `#[allow(unused)]`；最终 `cargo build --release` 无警告通过，`cargo fmt --check` 也通过。

---

## 10. 实验总结

本实验在 rCore 第四章的 SV39 虚存框架上实现了两个匿名映射系统调用：

1. **`sys_mmap`**
   - 严格校验 `start` 页对齐、`prot` 高位、`prot` 非零；
   - `len` 向上取整并做溢出检查；
   - 把用户 `prot` 逐位转换成 `MapPermission`（R/W/X 错位编码 + 必加 U）；
   - 先扫描整个区间拒绝冲突，再逐页分配物理页并建立 PTE；
   - 物理页不足时返回 `-1`，按约定不回收已映射页；
   - 用 `FrameTracker` 管理物理页生命周期。

2. **`sys_munmap`**
   - 要求 `start` 页对齐、`len` 向上取整；
   - 先预检查整个区间，存在未映射页则返回 `-1` 且不改动映射；
   - 成功时清空 PTE、删除 `FrameTracker`（真正归还物理页）；
   - 支持删除 `MapArea` 的前缀、后缀、全部，以及中间拆分。

3. **配套改动**
   - `PageTable::map_checked` / `find_pte_create_checked`：可失败的页表映射，避免 OOM panic；
   - `MemorySet` 上的 `mmap` / `munmap` 与 `TaskManager` 包装：把系统调用和原有内存管理组件连接起来；
   - 系统调用注册沿用模板的 215 / 222 分发。

最终结果：

- `make run TEST=4 BASE=0`：`Test 04_1`、`Test 04_4`、`Test 04_5`、`Test 04_6`、`Test trace_1` 以及第 3 章的 sleep/trace 测例全部通过；
- `make run TEST=1`、`make run TEST=2`：题目指定的命令可正常执行，第 2~4 章输出正确；
- `make -C ci-user test CHAPTER=4`：官方检查 **16/16 PASS**，退出码 0；
- `cargo fmt --check` 与 `cargo build --release` 均通过。

通过本实验，我掌握了页表映射的建立与拆除、物理页所有权的管理、权限位的转换，以及“先校验、后修改”的健壮性设计。这些经验对后续实现进程管理、文件系统等章节都有直接帮助。
