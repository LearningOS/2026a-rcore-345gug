# rCore-Tutorial 第六章实验报告（lab4）：硬链接

> 实验分支：`ch6`
> 实验环境：Rust `nightly-2024-05-02` + RISC-V SV39 + QEMU virt + rCore-Tutorial ch6 原版 easy-fs
> 验收命令：`make -C ci-user test CHAPTER=6`、`make -C os run TEST=1`

---

## 1. 实验目的

1. 理解文件系统中“目录项（DirEntry）— 磁盘 inode（DiskInode）— 数据块”三者的关系：目录项只是名字到 inode 编号的映射，真正的文件元数据和数据块由 inode 持有。
2. 理解硬链接的本质：**两个不同的目录项指向同一个磁盘 inode**，通过任一目录项打开文件，看到的都是同一份 inode 和数据块。
3. 在 easy-fs 的磁盘 inode 中加入 `nlink` 字段，并维护其计数。
4. 实现三个系统调用：
   - `sys_linkat`（syscall ID = 37）
   - `sys_unlinkat`（syscall ID = 35）
   - `sys_fstat`（syscall ID = 80）
5. 编写/运行用户态测例，保证官方 `ch6_file0~3` 与 CI 基础测例全部通过。

本实验严格按照实验文档实现，没有额外加入软链接、权限、时间戳、`nlink == 0` 时回收 inode/数据块等功能。

---

## 2. 硬链接原理

### 2.1 easy-fs 的磁盘布局

easy-fs 把一个块设备（QEMU 下是 `fs.img`）划分为：

```
+-----------+----------------+----------------+----------------+--------------+
| SuperBlock| Inode Bitmap   | Inode Area     | Data Bitmap    | Data Area    |
+-----------+----------------+----------------+----------------+--------------+
```

- **Inode Area**：按固定大小存放一个个 `DiskInode`，每个 `DiskInode` 有唯一的编号（inode id）。
- **Data Area**：存放文件数据块和间接块。
- **目录**在 easy-fs 中也是普通 inode，只是它的数据块内容是一串固定 32 字节的 `DirEntry`：

```rust
#[repr(C)]
pub struct DirEntry {
    name: [u8; 28],   // 27 字节名字 + 1 字节 '\0'
    inode_id: u32,    // 指向的 inode 编号
}
```

所以，“目录”本质上就是一张 `name -> inode_id` 表。

### 2.2 硬链接是什么

创建硬链接 `link("a", "b")` 时，**不复制文件内容，也不创建新 inode**，只是：

1. 在目录里追加一个目录项 `"b"`；
2. 让这个目录项的 `inode_id` 与 `"a"` 的目录项相同；
3. 把目标 inode 的 `nlink` 加 1。

```
        目录
   +-------------+
   | "a" -> 7    | --+
   +-------------+   |        DiskInode #7
   | "b" -> 7    | --+----->  size, nlink=2, direct[...]
   +-------------+            data blocks
```

删除一个名字 `unlink("a")` 时：

1. 从目录中删掉 `"a"` 的目录项；
2. 目标 inode 的 `nlink` 减 1。

只要 `nlink > 0`，就至少还有一个目录项引用这份 inode，数据仍然可以通过剩下的名字访问。

本实验的简化约定是：**`nlink` 减到 0 时不回收 inode 和数据块**（详见第 7 节）。

---

## 3. 数据结构修改：`DiskInode` 添加 `nlink`

### 3.1 `easy-fs/src/layout.rs`

```rust
/// A disk inode
#[repr(C)]
pub struct DiskInode {
    pub size: u32,
    /// 硬链接数量: 有多少个目录项指向该磁盘 inode
    pub nlink: u32,
    pub direct: [u32; INODE_DIRECT_COUNT],   // 28 个直接块号
    pub indirect1: u32,
    pub indirect2: u32,
    type_: DiskInodeType,
}

impl DiskInode {
    /// Initialize a disk inode, as well as all direct inodes under it
    /// indirect1 and indirect2 block are allocated only when they are needed
    pub fn initialize(&mut self, type_: DiskInodeType) {
        self.size = 0;
        // 新建的 inode 必然被创建它的那个目录项引用, 因此初始硬链接数为 1
        self.nlink = 1;
        self.direct.iter_mut().for_each(|v| *v = 0);
        self.indirect1 = 0;
        self.indirect2 = 0;
        self.type_ = type_;
    }
    // ...
}
```

要点：

- `nlink` 放在 `size` 之后，`#[repr(C)]` 保证内核与 `easy-fs-fuse` 的布局完全一致。
- `initialize()` 每新建一个 inode 就把 `nlink` 置为 1：创建它的目录项本身就是一个引用。根目录 `/` 也通过该函数初始化，因此其 `nlink` 为 1（本实验不维护目录的子目录计数）。
- **布局变化的影响**：原 `DiskInode` 为 128 字节（4 字节 `size` + 112 字节 `direct` + 4 + 4 + 1 字节类型 + 对齐），加入 `nlink` 后为 132 字节。每块 512 字节能放的 inode 数由 `512/128 = 4` 变为 `512/132 = 3`。`SuperBlock::create`、`get_disk_inode_pos()` 都使用 `size_of::<DiskInode>()` 计算，因此内核与 `easy-fs-fuse` 只要使用同一份 easy-fs 代码就自动一致；但**旧的 `fs.img` 必须重新生成**，不能复用。

### 3.2 `easy-fs/src/efs.rs`：由磁盘位置反推 inode 编号

easy-fs 的 `Inode` 只保存 `(block_id, block_offset)`，不保存 inode 编号；而 `link` 需要编号来写目录项，`fstat` 需要编号来填 `ino`。因此增加：

```rust
/// Get inode id from the position (block id and offset) of a disk inode.
pub fn get_inode_id(&self, block_id: u32, block_offset: usize) -> u32 {
    let inode_size = core::mem::size_of::<DiskInode>();
    let inodes_per_block = BLOCK_SZ / inode_size;
    (block_id - self.inode_area_start_block) * inodes_per_block as u32
        + (block_offset / inode_size) as u32
}
```

---

## 4. 三个系统调用的设计思路与执行流程

### 4.1 系统调用号与入口注册

`os/src/syscall/mod.rs` 中已存在以下常量和分发（本实验无需改动分发逻辑）：

```rust
const SYSCALL_UNLINKAT: usize = 35;
const SYSCALL_LINKAT: usize = 37;
const SYSCALL_FSTAT: usize = 80;

match syscall_id {
    SYSCALL_LINKAT   => sys_linkat(args[1] as *const u8, args[3] as *const u8),
    SYSCALL_UNLINKAT => sys_unlinkat(args[1] as *const u8),
    SYSCALL_FSTAT    => sys_fstat(args[0], args[1] as *mut Stat),
    // ...
}
```

RISC-V 调用约定下 `a0~a3` 被 `trap_handler` 收集为 `args[0..4]`：

| 系统调用 | a0 | a1 | a2 | a3 | a4 |
| --- | --- | --- | --- | --- | --- |
| `linkat` | olddirfd | oldpath | newdirfd | newpath | flags |
| `unlinkat` | dirfd | path | flags | — | — |
| `fstat` | fd | st | — | — | — |

按实验约定，`olddirfd/newdirfd/dirfd/flags` 全部忽略，只取路径/文件描述符。

### 4.2 `sys_linkat` 执行流程

用户态接口（`user/src/syscall.rs`）以 `syscall6(37, [AT_FDCWD, oldpath, AT_FDCWD, newpath, 0, 0])` 发起调用。

内核实现（`os/src/syscall/fs.rs`）：

```rust
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
```

流程：

1. 通过 `translated_str` 从用户地址空间读出两个以 `\0` 结尾的路径字符串。
2. `ROOT_INODE.find(oldpath)`：若源路径不存在，直接返回 `-1`。
3. 调用 easy-fs 的 `Inode::link(newpath, old_inode)`，在根目录追加目录项并把 `nlink` 加 1。
4. 成功返回 `0`，失败（新路径已存在）返回 `-1`。

### 4.3 `sys_unlinkat` 执行流程

```rust
/// Remove the directory entry `path`.
///
/// `dirfd`、`flags` 按实验约定忽略.
/// 返回值: 成功 0; 路径不存在返回 -1.
pub fn sys_unlinkat(name: *const u8) -> isize {
    trace!("kernel:pid[{}] sys_unlinkat", current_task().unwrap().pid.0);
    let token = current_user_token();
    let name = translated_str(token, name);
    ROOT_INODE.unlink(name.as_str())
}
```

流程：

1. 读出用户路径。
2. 调用 easy-fs 的 `Inode::unlink`。
3. 目录项不存在返回 `-1`；否则删除目录项、`nlink` 减 1，返回 `0`。

### 4.4 `sys_fstat` 执行流程

```rust
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
```

流程：

1. 在当前进程的 `fd_table` 中查找文件；`fd` 越界或对应项为空返回 `-1`。
2. 克隆 `Arc<dyn File>` 并释放 TCB 借用。
3. `File::stat()` 返回内核 `Stat`。对普通文件由 `OSInode` 实现：
   - `dev = 0`（实验固定）；
   - `ino` 由 easy-fs 的 `get_inode_id` 计算；
   - `nlink` 直接读磁盘 inode；
   - `mode` 根据 `is_dir()` 填 `StatMode::DIR` 或 `StatMode::FILE`；
   - `pad = [0; 7]`。
4. 因为内核运行在内核页表下，不能直接写用户指针 `st`，必须用 `translated_byte_buffer(token, st, size_of::<Stat>())` 把用户页翻译成内核可写的切片，再把 80 字节的 `Stat` 逐段拷贝过去。
5. 成功返回 `0`。

### 4.5 前置系统调用的最小移植

`ci-user` 对 `CHAPTER=6` 使用 `check/ch6.py`，它要求 `ch6_usertest` 的**完整**输出，其中包含前几章实验的测例（`get_time`、`sleep1`、`mmap/munmap`、`spawn` 等）。ch6 基线把这些系统调用留成了 `YOUR JOB` 桩，因此为了让验收命令通过，本实验从 ch5 分支**最小化移植**了以下前置能力（不包含多核、stride 调度等额外功能，也不改变硬链接实验本身的语义）：

| 系统调用 | 文件 | 说明 |
| --- | --- | --- |
| `sys_get_time` (169) | `os/src/syscall/process.rs` | 用 `timer::get_time_us()` 取时间；`TimeVal` 可能跨页，因此分别通过 `translated_refmut` 写 `sec`、`usec` |
| `sys_mmap` (222) | `os/src/syscall/process.rs` + `os/src/mm/memory_set.rs` | 仅要求 `start` 页对齐；新增 `MemorySet::mmap`，检查与已有区域不重叠后映射匿名 `Framed` 区域 |
| `sys_munmap` (215) | `os/src/syscall/process.rs` + `os/src/mm/memory_set.rs` | 新增 `MemorySet::munmap`，要求页范围与某个已映射区域**完全一致**才解除映射 |
| `sys_spawn` (400) | `os/src/syscall/process.rs` | 从 ELF 直接调用 `TaskControlBlock::new` 构造全新地址空间（不复制父进程），建立父子关系后加入调度队列 |

这些是 ch6 官方测试套件正常运行的**前置条件**，而不是硬链接实验的扩展功能；硬链接相关的实现仍然只有 `linkat/unlinkat/fstat` 和 `nlink`。

---

## 5. 核心接口实现说明（link / unlink）

### 5.1 `Inode::link`（`easy-fs/src/vfs.rs`）

```rust
pub fn link(&self, name: &str, inode: &Inode) -> isize {
    // 整个 link 操作需要在同一个文件系统锁下完成, 避免目录项和 nlink 不一致
    let mut fs = self.fs.lock();
    // 只有目录才能持有目录项
    if !self.read_disk_inode(|disk_inode| disk_inode.is_dir()) {
        return -1;
    }
    // 本实验只定义了 "链接同名文件" 这一种错误: 新路径已存在则失败.
    // 其它 "新路径已存在" 的情况属于未定义行为, 这里同样保守地返回 -1.
    if self
        .read_disk_inode(|disk_inode| self.find_inode_id(name, disk_inode))
        .is_some()
    {
        return -1;
    }
    // 由目标 inode 在磁盘上的位置反推它的 inode 编号
    let inode_id = fs.get_inode_id(inode.block_id as u32, inode.block_offset);
    // 两个目录项指向同一个磁盘 inode, 硬链接数加一
    inode.modify_disk_inode(|disk_inode| {
        disk_inode.nlink += 1;
    });
    // 在当前目录末尾追加一个指向同一 inode 的目录项
    self.modify_disk_inode(|disk_inode| {
        let file_count = (disk_inode.size as usize) / DIRENT_SZ;
        let new_size = (file_count + 1) * DIRENT_SZ;
        self.increase_size(new_size as u32, disk_inode, &mut fs);
        let dirent = DirEntry::new(name, inode_id);
        disk_inode.write_at(
            file_count * DIRENT_SZ,
            dirent.as_bytes(),
            &self.block_device,
        );
    });
    block_cache_sync_all();
    0
}
```

关键点：

1. **锁顺序**：先拿 `fs` 互斥锁，再按需拿块缓存锁；`inode_id` 用 `fs.get_inode_id(...)` 在已持有的 `fs` 锁内计算，而不是调用会再次 `self.fs.lock()` 的辅助函数，否则会自锁死。
2. **重名检查**：新路径已存在时返回 `-1`，覆盖“链接同名文件”这一实验定义的错误。
3. **追加目录项**：复用 `create()` 的思路，先 `increase_size` 扩容目录，再 `write_at` 写入 `DirEntry { name, inode_id }`。
4. **`nlink += 1`**：在目录项追加前完成；本实验不做失败回滚。
5. `block_cache_sync_all()` 把改动刷回块设备，保证掉电/重启后一致（教学实现）。

### 5.2 `Inode::unlink`（`easy-fs/src/vfs.rs`）

```rust
pub fn unlink(&self, name: &str) -> isize {
    let fs = self.fs.lock();
    // 只有目录才能持有目录项
    if !self.read_disk_inode(|disk_inode| disk_inode.is_dir()) {
        return -1;
    }
    // 找到待删除目录项的位置以及它指向的 inode
    let Some((entry_index, inode_id)) =
        self.read_disk_inode(|disk_inode| self.find_inode_entry(name, disk_inode))
    else {
        return -1;
    };
    // 目标 inode 的硬链接数减一. 注意: nlink 为 0 时不做任何回收.
    let (block_id, block_offset) = fs.get_disk_inode_pos(inode_id);
    get_block_cache(block_id as usize, Arc::clone(&self.block_device))
        .lock()
        .modify(block_offset, |disk_inode: &mut DiskInode| {
            if disk_inode.nlink > 0 {
                disk_inode.nlink -= 1;
            }
        });
    // 把后面的目录项依次前移, 覆盖被删除项, 并缩小目录大小.
    // 目录缩小时不会释放已经分配的目录数据块, 这是本实验的简化.
    self.modify_disk_inode(|disk_inode| {
        let file_count = (disk_inode.size as usize) / DIRENT_SZ;
        for i in entry_index + 1..file_count {
            let mut dirent = DirEntry::empty();
            assert_eq!(
                disk_inode.read_at(i * DIRENT_SZ, dirent.as_bytes_mut(), &self.block_device),
                DIRENT_SZ,
            );
            disk_inode.write_at(
                (i - 1) * DIRENT_SZ,
                dirent.as_bytes(),
                &self.block_device,
            );
        }
        disk_inode.size -= DIRENT_SZ as u32;
    });
    block_cache_sync_all();
    0
}
```

关键点：

1. `find_inode_entry` 返回 `(entry_index, inode_id)`，既拿到要删除的位置，也拿到目标 inode。
2. `nlink` 减 1，并用 `if nlink > 0` 防御性保护，避免下溢。
3. **目录项压缩**：删除中间项时，把后面的目录项依次前移，最后把目录 `size` 减 32 字节。否则目录中会留下“空槽”，`find` 会读到空名字或重复项，`ch6_file3` 的反复 create/unlink 会暴露问题。
4. 仍然**不回收** inode 和数据块（见第 7 节）。

### 5.3 `Inode::get_stat`（`easy-fs/src/vfs.rs`）

```rust
pub fn get_stat(&self) -> (u32, u32, bool) {
    // 先取得文件系统锁, 保证换算 inode 编号和读取磁盘 inode 时视图一致
    let fs = self.fs.lock();
    let inode_id = fs.get_inode_id(self.block_id as u32, self.block_offset);
    let (nlink, is_dir) =
        self.read_disk_inode(|disk_inode| (disk_inode.nlink, disk_inode.is_dir()));
    (inode_id, nlink, is_dir)
}
```

### 5.4 OS 层封装（`os/src/fs/mod.rs`、`os/src/fs/inode.rs`）

`File` trait 增加带默认实现的 `stat()`，标准输入输出无需改动：

```rust
fn stat(&self) -> Stat {
    Stat {
        dev: 0,
        ino: 0,
        mode: StatMode::NULL,
        nlink: 0,
        pad: [0; 7],
    }
}
```

`OSInode` 覆盖该实现：

```rust
fn stat(&self) -> Stat {
    let inner = self.inner.exclusive_access();
    // 直接读取磁盘 inode, 因此通过任意一个硬链接看到的 nlink 都是一致的
    let (ino, nlink, is_dir) = inner.inode.get_stat();
    Stat {
        dev: 0, // 本实验只有一个磁盘驱动器, 固定为 0
        ino: ino as u64,
        mode: if is_dir { StatMode::DIR } else { StatMode::FILE },
        nlink,
        pad: [0; 7],
    }
}
```

为了让系统调用层能操作根目录，`os/src/fs/mod.rs` 额外导出了 `ROOT_INODE`：

```rust
pub use inode::{list_apps, open_file, OSInode, OpenFlags, ROOT_INODE};
```

---

## 6. 错误处理逻辑

| 场景 | 检查位置 | 返回值 |
| --- | --- | --- |
| `linkat` 源文件不存在 | `sys_linkat` 中 `ROOT_INODE.find` 返回 `None` | `-1` |
| `linkat` 新路径已存在（含链接同名文件） | `Inode::link` 中 `find_inode_id(name)` 返回 `Some` | `-1` |
| `linkat` 当前 inode 不是目录 | `Inode::link` 中 `is_dir()` 检查 | `-1` |
| `unlinkat` 路径不存在 | `Inode::unlink` 中 `find_inode_entry` 返回 `None` | `-1` |
| `unlinkat` 重复删除同一路径 | 第一次已删掉目录项，第二次查找失败 | `-1` |
| `fstat` fd 越界 | `fd >= fd_table.len()` | `-1` |
| `fstat` fd 未打开（`None`） | `fd_table[fd]` 为 `None` | `-1` |
| `fstat` 成功 | 填充 80 字节 `Stat` | `0` |

说明：

- 实验文档规定“链接同名文件”是错误；本实现把“新路径已存在”统一视为错误，避免产生一个目录项重复的非法目录。
- 不对用户指针 `st` 做合法性探测；如果用户传入非法地址，`translated_byte_buffer` 中的页表翻译会失败并 panic。测例均传入合法的栈上 `Stat`。
- 没有实现权限、时间戳、跨目录、目录硬链接等检查。

---

## 7. 实验简化点说明（unlink 不回收 inode）

严格按实验要求，本实现只做“目录项层面的取消链接”：

1. `unlinkat` 删除目录项并把 `nlink` 减 1；
2. **即使 `nlink` 减到 0，也不释放 inode、不释放数据块、不清空 inode bitmap / data bitmap**；
3. 目录 `size` 减小时，已经分配的目录数据块也不回收。

这样做的后果：

- 反复 `create + unlink` 会泄漏 inode 和数据块。`ch6_file3` 做 10 轮，每轮泄漏 1 个 inode 和约 6 个数据块，远小于 14000 块的文件系统容量，测例可通过。
- 每次 `make run`/CI 都会重新生成 `fs.img`，泄漏不会跨运行累积。
- 若要支持真正的删除，需要在 `nlink == 0` 时：
  - 调用 `clear_size()` 释放 direct/indirect 数据块；
  - 通过 inode bitmap 释放 inode；
  - 删除目录项后视情况释放空目录块。
  这超出本次实验文档的范围，故未实现。

其它简化：

- 只支持根目录下的文件名（与 ch6 的 `open_file` 只做 `ROOT_INODE.find(name)` 一致），不处理 `/a/b` 这类路径。
- 忽略 `linkat` 的 `olddirfd/newdirfd/flags` 与 `unlinkat` 的 `dirfd/flags`。
- 不区分 `AT_FDCWD` 与普通目录 fd。
- 根目录 `nlink` 固定为 1，不统计子目录。

---

## 8. 测试方案与测试结果

### 8.1 测试程序

| 测例 | 覆盖点 | 关键输出 |
| --- | --- | --- |
| `ch6b_filetest_simple` | 基础 create/write/read | `file_test passed!` |
| `ch6_file0` | 打开、写入、读回 | `Test file0 OK!` |
| `ch6_file1` | `fstat` 的 `mode`、`nlink` 初值 | `Test fstat OK!` |
| `ch6_file2` | `link` 使 `nlink` 2→4，`unlink` 使 4→3→1；两个名字 `ino` 相同、数据相同 | `Test link OK!` |
| `ch6_file3` | 10 轮 create/write/close/unlink，unlink 后 open 必须失败 | `Test mass open/unlink OK!` |
| `ch6b_linktest`（新增） | 成功路径 + 错误路径（见下） | `Test linktest OK!` |

新增测例 `user/src/bin/ch6b_linktest.rs` 覆盖：

1. 创建文件后 `fstat`：`mode == FILE`、`nlink == 1`；
2. `link` 后 `nlink == 2`；
3. 再次链接同名文件返回 `-1`；
4. 链接不存在的源文件返回 `-1`；
5. 通过新名字读取，数据与 inode 编号和原文件一致；
6. `unlink` 一个名字后，另一个名字仍可打开，`nlink == 1`；
7. 删除最后一个名字后两个路径都无法打开；
8. `unlink` 不存在的路径返回 `-1`；
9. `fstat` 非法 fd 返回 `-1`。

该测例已同时加入 `ch6b_usertest` 与 `ch6_usertest` 的测试列表（`user/` 与 `ci-user/user/` 两份均同步），因此 CI 和 `make run TEST=1` 都会执行它。

### 8.2 `make -C os run TEST=1`

`os/src/task/mod.rs` 中做如下选择：

- 文件系统中存在 `ch6_usertest`（`make run TEST=1` 或 CI 的 `BASE=2 TEST=6` 会把全部测例打包）时，运行 `ch6_usertest`；
- 普通 `make run` 只有 `ch*b_*` 测例，此时仍运行 `ch6b_initproc`（交互 shell），保持原有行为。

实测输出节选（完整 `ch6_usertest`）：

```
Usertests: Running ch3_sleep
get_time OK! 203
Test sleep OK!
Usertests: Running ch3_sleep1
Test sleep1 passed!
Usertests: Running ch4_mmap0
Test 04_1 OK!
Test 04_4 test OK!
Test 04_5 ummap OK!
Test 04_6 ummap2 OK!
Usertests: Running ch5_spawn0
Test spawn0 OK!
Usertests: Running ch5_spawn1
Test wait OK!
Test waitpid OK!
Usertests: Running ch6b_filetest_simple
file_test passed!
Usertests: Running ch6_file0
Test file0 OK!
Usertests: Running ch6_file1
Test fstat OK!
Usertests: Running ch6_file2
Test link OK!
Usertests: Running ch6_file3
Test mass open/unlink OK!
Usertests: Running ch6b_linktest
Test linktest OK!
ch6 Usertests passed!
```

用 CI 的 `ci-user/check/ch6.py` 检查该输出：

```
Test passed: 31/31
```

### 8.3 官方硬链接测例

`ch6_usertest` 自身包含官方测例 `ch6_file0~3`，上述 `make run TEST=1` 与 CI 都会自动执行。结果：

```
Test file0 OK!
Test fstat OK!
Test link OK!
Test mass open/unlink OK!
```

四项官方硬链接相关测例全部通过；新增的 `ch6b_linktest` 也输出了 `Test linktest OK!`。

### 8.4 CI 检查

使用 CI 自带的检查器对 `make run TEST=1` 的输出做本地检查：

```
$ python3 ci-user/check/ch6b.py < /tmp/run_test1_b.log
...
Test passed: 18/18
```

`make -C ci-user test CHAPTER=6` 的完整 CI 流程（备份 → 随机化 → 构建 `BASE=2 TEST=6` → QEMU → `check/ch6.py` 检查 → 还原 → 报告检查）本地实测结果：

```
...
Test passed3924: 31/31
make restore
Restoring original files from temp-* dirs
Report for lab1 found.
Report for lab2 found.
Report for lab3 found.
Report for lab4 found.
make: Leaving directory '/home/karuia/2026a-rcore-345gug/ci-user'
```

- QEMU 输出中被 CI 随机化的是 `OK`/`passed` 字符串（例如 `OK3924`），检查器使用同一套随机化后的模式，31 项全部命中，`NOT_EXPECTED` 两项均未出现。
- 命令退出码为 `0`，报告检查也通过。
- 本机无外网，CI 使用 `OFFLINE=1` + `ci-user/vendor` 运行；标准联网环境下直接执行 `make -C ci-user test CHAPTER=6` 即可。

---

## 9. 踩坑总结

### 9.1 `spawn` 未实现时的“假成功”陷阱

最初只关注硬链接，没有注意到 `ch6_usertest` 依赖 `spawn`。运行日志显示所有子测例都是：

```
Usertests: Test xxx in Process -1 exited with code 0
```

且没有任何子测例自己的输出。原因是 ch6 基线的 `os/src/syscall/process.rs` 中 `sys_spawn` 仍是 `YOUR JOB` 桩并返回 `-1`；`ch6_usertest` 用 `spawn` 运行子测例，随后 `waitpid(-1)` 也返回 `-1`，`assert_eq!(pid, wait_pid)` 恰好通过，造成“全部成功”的假象。

进一步检查发现 `ci-user` 对 `CHAPTER=6` 使用 `check/ch6.py`（而不是 `ch6b.py`），要求完整 `ch6_usertest` 的输出，因此 `spawn`、`get_time`、`mmap/munmap` 是**必须**的前置能力。最终按照 ch5 分支的实现做了最小移植（见 4.5 节），并把 `INITPROC` 改为：存在 `ch6_usertest` 时运行它，否则回落到交互 shell。移植后 `make run TEST=1` 与 CI 均通过。

### 9.2 修改 `DiskInode` 后必须重新生成 `fs.img`

加入 `nlink` 使 `DiskInode` 从 128 字节变为 132 字节，每块可容纳的 inode 数从 4 变为 3。内核与 `easy-fs-fuse` 共用 `easy-fs` crate，布局自动一致，但复用旧 `fs.img` 会读到错位的 inode。测试前必须让 `easy-fs-fuse` 重新打包。

### 9.3 避免重复加锁导致死锁

最初想在 `Inode::link` 里调用会自己 `self.fs.lock()` 的 `inode.inode_id()`，但 `link` 已经持有 `self.fs` 锁，而 `spin::Mutex` 不可重入，会直接死锁。最终改为在 `link` 内直接用已持有的 `fs` guard 调用 `fs.get_inode_id(inode.block_id, inode.block_offset)`。

### 9.4 `fstat` 不能直接写用户指针

系统调用运行在内核页表下，用户地址不在当前页表中。直接 `*st = stat` 会触发页错误。必须使用 `translated_byte_buffer(token, st as *const u8, size_of::<Stat>())` 拿到内核可写的用户页切片，再按字节拷贝。`Stat` 的 `pad: [u64; 7]` 必须保留，保证用户态与内核态结构体大小一致（80 字节）。

### 9.5 目录项必须压缩

`unlink` 如果只是把目录项名字清零而不移动后续项，`find_inode_entry` 会在“空槽”处得到空名字，目录中还会留下重复/无效项。`ch6_file3` 的 10 轮 create/unlink 会迅速暴露问题。改为把后续目录项依次前移并减小 `size` 后通过。

### 9.6 新 inode 的 `nlink` 初值

所有 inode（包括根目录和 `fs::create` 新建的文件）都走 `DiskInode::initialize`，因此 `nlink = 1` 只需在一处设置。`ch6_file1` 断言新建文件 `nlink == 1`，若忘记初始化会得到 0。

### 9.7 CI 的离线运行

本机沙箱无外网，`make -C ci-user test CHAPTER=6` 默认需要访问 crates.io / GitHub 以及 CI 指定的 `nightly-2024-02-25` 工具链。本地验证时使用 `OFFLINE=1` 和 `ci-user/vendor` 的 vendored 依赖，并将缺组件的工具链临时指向已安装的 `nightly-2024-05-02`。这仅是本地验证环境适配，提交的内核代码与在线 CI 完全兼容；在有网络的标准环境中直接运行 `make -C ci-user test CHAPTER=6` 即可。

### 9.8 `#![deny(warnings)]` 与 `missing_docs`

导出 `ROOT_INODE` 后，`lazy_static!` 生成的公共静态项需要文档注释；`syscall/fs.rs` 中导入 `File` trait 会被判定为 `unused_import`（对 `dyn File` 方法调用不需要导入 trait）。这些在 `deny(warnings)` 下都是编译错误，需要一并处理。

---

## 10. 改动文件清单

| 文件 | 改动 |
| --- | --- |
| `easy-fs/src/layout.rs` | `DiskInode` 增加 `pub nlink: u32`；`initialize()` 置 1 |
| `easy-fs/src/efs.rs` | 新增 `get_inode_id(block_id, block_offset)` |
| `easy-fs/src/vfs.rs` | 新增 `find_inode_entry`、`link`、`unlink`、`get_stat` |
| `os/src/fs/mod.rs` | `File::stat()` 默认实现；导出 `ROOT_INODE` |
| `os/src/fs/inode.rs` | `OSInode` 实现 `stat()` |
| `os/src/syscall/fs.rs` | 实现 `sys_fstat`、`sys_linkat`、`sys_unlinkat` |
| `os/src/syscall/process.rs` | 最小移植前置 `sys_get_time`、`sys_mmap`、`sys_munmap`、`sys_spawn` |
| `os/src/mm/memory_set.rs` | 新增 `MemorySet::mmap`、`MemorySet::munmap` |
| `os/src/task/mod.rs` | `ch6_usertest` 存在时作为 initproc，否则回落到 `ch6b_initproc` |
| `user/src/bin/ch6b_linktest.rs` | 新增硬链接综合测例 |
| `user/src/bin/ch6b_usertest.rs` / `user/src/bin/ch6_usertest.rs` | 注册 `ch6b_linktest` |
| `ci-user/user/src/bin/*` | 同步新增测例与注册（CI 使用该副本） |
