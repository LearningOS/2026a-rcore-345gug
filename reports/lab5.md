# rCore 第八章实验报告（lab5）：ELO 分数更新与 `sys_eventfd2`

> 实验分支：`ch8`
> 实验环境：Rust `nightly-2024-05-02` + RISC-V SV39 + QEMU virt + rCore-Tutorial ch8
> 验收命令：
>
> ```bash
> make -C ci-user test CHAPTER=8
> ```
>
> 说明：本报告对应 ch8 实验的两部分作业。第一部分是用户态 C 程序
> （`lab8/elo.c`），第二部分是内核 Rust 系统调用（`sys_eventfd2`，调用号 290）。
> 为了通过 ch8 官方 CI，本实验还补齐了模板中尚未实现的 `sys_get_time` 和
> 互斥量/信号量的死锁检测（见第 4 节）。

---

## 1. 实验目标

1. 用生产者—消费者模型模拟 Saiblo 评测机与 worker 的 ELO 分数更新：
   - `p` 位选手，初始 1000 分；
   - `m` 个评测机线程各产生 `k` 场随机比赛结果；
   - `n` 个 worker 线程从结果队列取比赛并更新分数；
   - 验证零和性质：所有选手总分恒为 `1000 * p`。
2. 理解“worker 是哲学家、两位选手是筷子”的建模，使用**按编号有序加锁**
   消除循环等待，避免哲学家就餐死锁。
3. 在 rCore 内核中实现 Linux 风格的 `eventfd`：
   - `sys_eventfd2(initval, flags)`，syscall ID = 290；
   - 支持 `EFD_SEMAPHORE`(1)、`EFD_NONBLOCK`(2048) 及其组合；
   - 支持阻塞/非阻塞读写、64 位计数器、`fork` 继承共享计数器；
   - 非法 flags 与 fd 数量超限返回 -1。
4. 为通过官方 ch8 CI，补齐：
   - `sys_get_time`（`sleep()` 依赖它做毫秒计时）；
   - 阻塞互斥量与信号量的死锁检测（返回 `-0xdead`）。

---

## 2. 第一部分：银行家算法——分数更新（用户态 C）

### 2.1 文件与编译

| 文件 | 作用 |
| ---- | ---- |
| `lab8/elo.c` | 生产者—消费者 + 每选手一把锁的 ELO 模拟 |
| `lab8/Makefile` | `gcc -pthread` 构建脚本 |
| `lab8/README.md` | 参数与运行说明 |

```bash
cd lab8 && make
./elo 8 4 6 1000      # p=8 m=4 n=6 k=1000
```

### 2.2 并发设计

1. **结果队列**：容量 256 的环形缓冲区，用 `queue_lock` 保护，
   配 `q_not_empty` / `q_not_full` 两个条件变量。队列满时生产者阻塞，
   空时消费者阻塞。
2. **结束条件**：主线程先 `join` 全部生产者，再在队列锁内置
   `producers_done = 1` 并 `broadcast`。消费者只有在“队列为空且生产者
   全部结束”时才退出，因此不会漏处理结果。
3. **每选手一把锁**：`player_locks[1..p]`，等价于哲学家问题中的筷子。
   worker 同时持有两位选手的锁时才读取/修改/写回分数。
4. **死锁避免**：所有 worker 统一按**选手编号从小到大**加锁，破坏
   “循环等待”条件。注意加锁顺序只用于避免死锁，更新时仍按比赛结果
   中的 A/B 身份计算分数。
5. **零和校验**：每次更新后双方分数变动之和为 0，所以总分为
   `1000 * p`。若出现并发错误（丢更新/重复更新），总分立即偏离，
   程序输出 `FAILED` 并返回非 0。

### 2.3 用户态 C 代码（核心部分）

完整源码见 `lab8/elo.c`，关键代码如下：

```c
/* 每位选手一把锁：等价于哲学家问题中的“筷子”。 */
static pthread_mutex_t *player_locks;

/* 有界阻塞队列 */
static void queue_push(const Match *m) {
    pthread_mutex_lock(&q_lock);
    while (q_count == QUEUE_CAP) {
        pthread_cond_wait(&q_not_full, &q_lock);
    }
    queue[q_tail] = *m;
    q_tail = (q_tail + 1) % QUEUE_CAP;
    q_count++;
    pthread_cond_signal(&q_not_empty);
    pthread_mutex_unlock(&q_lock);
}

static int queue_pop(Match *m) {
    pthread_mutex_lock(&q_lock);
    while (q_count == 0 && !producers_done) {
        pthread_cond_wait(&q_not_empty, &q_lock);
    }
    if (q_count == 0 && producers_done) {
        pthread_mutex_unlock(&q_lock);
        return 0;                /* 不会再有新结果，消费者退出 */
    }
    *m = queue[q_head];
    q_head = (q_head + 1) % QUEUE_CAP;
    q_count--;
    pthread_cond_signal(&q_not_full);
    pthread_mutex_unlock(&q_lock);
    return 1;
}

/*
 * 按“选手编号从小到大”的顺序加锁：
 *   A=3,B=7 与 A=7,B=3 都先锁 3 再锁 7。
 * 所有 worker 的加锁顺序全局一致，因此不会形成循环等待。
 * 这是哲学家就餐问题的“按筷子编号顺序拿起”解法。
 */
static void update_scores(const Match *m) {
    int a = m->a;
    int b = m->b;
    int first  = (a < b) ? a : b;
    int second = (a < b) ? b : a;

    pthread_mutex_lock(&player_locks[first]);
    pthread_mutex_lock(&player_locks[second]);

    if (m->result == 0) {
        /* 平局：高分者 -10，低分者 +10；同分不变。 */
        if (scores[a] > scores[b]) {
            scores[a] -= 10;
            scores[b] += 10;
        } else if (scores[a] < scores[b]) {
            scores[a] += 10;
            scores[b] -= 10;
        }
    } else {
        int winner = (m->result == 1) ? a : b;
        int loser  = (m->result == 1) ? b : a;
        /* 胜者原分不低于败者：±20；否则爆冷：±30。 */
        int delta = (scores[winner] >= scores[loser]) ? 20 : 30;
        scores[winner] += delta;
        scores[loser]  -= delta;
    }

    /* 模拟数据库更新延时：题目要求持锁睡眠 1ms。 */
    usleep(1000);

    pthread_mutex_unlock(&player_locks[second]);
    pthread_mutex_unlock(&player_locks[first]);
}

static void *consumer(void *arg) {
    (void)arg;
    Match m;
    while (queue_pop(&m)) {
        update_scores(&m);       /* 取一场、更新一场 */
    }
    return NULL;
}
```

生产者部分：

```c
static void *producer(void *arg) {
    int id = (int)(intptr_t)arg;
    /* 每个线程独立 seed，避免共享随机数状态。 */
    unsigned int seed = 0x9e3779b9u ^ ((unsigned int)id * 2654435761u);

    for (int i = 0; i < K; i++) {
        Match m;
        m.a = (int)(rand_r(&seed) % (unsigned int)P) + 1;
        do {
            m.b = (int)(rand_r(&seed) % (unsigned int)P) + 1;
        } while (m.b == m.a);            /* 两位不同选手 */
        m.result = (int)(rand_r(&seed) % 3u); /* 0 平, 1 A 胜, 2 B 胜 */
        queue_push(&m);
    }
    return NULL;
}
```

主线程在 `join` 生产者之后置结束标志，再 `join` 消费者，最后打印成绩：

```c
for (int i = 0; i < M; i++) pthread_join(producers[i], NULL);

pthread_mutex_lock(&q_lock);
producers_done = 1;
pthread_cond_broadcast(&q_not_empty);
pthread_mutex_unlock(&q_lock);

for (int i = 0; i < N; i++) pthread_join(consumers[i], NULL);

long long total = 0;
for (int i = 1; i <= P; i++) {
    printf("  选手 %2d: %lld\n", i, scores[i]);
    total += scores[i];
}
printf("所有选手分数之和: %lld (期望 %lld) -> %s\n",
       total, 1000LL * P, total == 1000LL * P ? "OK" : "FAILED");
```

### 2.4 测试与结果

```bash
cd lab8
make
./elo 8 4 6 200
```

实测输出（节选）：

```text
配置: p=8 位选手, m=4 个评测机, n=6 个 worker, 每个评测机 200 场比赛 (共 800 场)

最终成绩:
  选手  1: 970
  选手  2: 970
  选手  3: 1030
  选手  4: 1010
  选手  5: 1020
  选手  6: 990
  选手  7: 1010
  选手  8: 1000
所有选手分数之和: 8000 (期望 8000) -> OK
总比赛数: 800, 用时: 0.631 s
```

总分严格等于 `1000 * p`，说明每场比赛都被且仅被一个 worker 处理，
两位选手的分数在持锁区间内原子更新，没有丢更新或重复更新。

也可以从 stdin 读取缺省参数：

```text
$ ./elo
请输入 p (选手数): 4
请输入 m (评测机数): 2
请输入 n (worker 数): 3
请输入 k (每个评测机的比赛数): 100
配置: p=4 位选手, ...
```

---

## 3. 第二部分：内核实现 `sys_eventfd2`（内核 Rust）

### 3.1 设计概述

`eventfd` 被实现为一个实现了 `File` trait 的内核对象，放入进程的
`fd_table`。因此：

- 用户态通过普通的 `read` / `write` 系统调用操作它；
- `fork` 时 `ProcessControlBlock::fork` 克隆 `fd_table` 中的 `Arc`，
  父子进程指向**同一个** `EventFd`，即共享同一个计数器；
- `close` 只是从 fd 表移除一个 `Arc`，其他引用仍有效。

核心结构（`os/src/fs/eventfd.rs`）：

```rust
/// eventfd 的 64 位计数器 + 读等待队列 + 写等待队列。
struct EventFdInner {
    count: u64,
    /// 因计数器为 0 而阻塞在 read 的任务
    read_queue: VecDeque<Arc<TaskControlBlock>>,
    /// 因计数器即将溢出而阻塞在 write 的任务
    write_queue: VecDeque<Arc<TaskControlBlock>>,
}

pub struct EventFd {
    /// 是否为 EFD_SEMAPHORE 模式
    semaphore: bool,
    /// 是否为 EFD_NONBLOCK 模式
    nonblock: bool,
    inner: UPSafeCell<EventFdInner>,
}
```

`flags` 校验：只允许 `EFD_SEMAPHORE | EFD_NONBLOCK`，未知位或负数返回
`None`，系统调用再返回 -1。

### 3.2 读语义

```rust
fn read(&self, buf: UserBuffer) -> isize {
    // eventfd 的读写缓冲区必须是 8 字节
    if buf.len() != core::mem::size_of::<u64>() {
        return -1;
    }
    loop {
        let task = current_task().unwrap();
        let mut inner = self.inner.exclusive_access();
        if inner.count > 0 {
            let value = if self.semaphore {
                // 信号量模式：消耗一个 token，读出值 1
                inner.count -= 1;
                1u64
            } else {
                // 普通模式：原子地读出并清零
                let value = inner.count;
                inner.count = 0;
                value
            };
            // 计数器减小，唤醒因溢出而阻塞的写者
            while let Some(waiter) = inner.write_queue.pop_front() {
                wakeup_task(waiter);
            }
            drop(inner);
            write_u64(buf, value);      // 按小端写回 8 字节
            return core::mem::size_of::<u64>() as isize;
        }
        drop(inner);
        if self.nonblock {
            return -2;                  // 非阻塞且计数器为 0
        }
        // 阻塞等待写者，被唤醒后重新检查（值可能被其他读者取走）
        let mut inner = self.inner.exclusive_access();
        if inner.count > 0 {
            continue;
        }
        inner.read_queue.push_back(task);
        drop(inner);
        block_current_and_run_next();
    }
}
```

要点：

- 普通模式：`read` 返回 8 字节，内容是**整个计数器**，随后清零；
- `EFD_SEMAPHORE`：返回 8 字节，内容为 **1**，计数器减一；
- 非阻塞且计数器为 0：返回 **-2**（题目约定）；
- 阻塞模式：加入 `read_queue`，`block_current_and_run_next` 让出 CPU。

### 3.3 写语义

```rust
fn write(&self, buf: UserBuffer) -> isize {
    if buf.len() != core::mem::size_of::<u64>() {
        return -1;                       // 写缓冲区必须 8 字节
    }
    let value = read_u64(&buf);
    loop {
        let task = current_task().unwrap();
        let mut inner = self.inner.exclusive_access();
        if let Some(new_count) = inner.count.checked_add(value) {
            let was_zero = inner.count == 0;
            inner.count = new_count;
            // 0 -> 非 0，唤醒所有等待的读者
            if was_zero && new_count > 0 {
                while let Some(waiter) = inner.read_queue.pop_front() {
                    wakeup_task(waiter);
                }
            }
            drop(inner);
            return core::mem::size_of::<u64>() as isize;
        }
        drop(inner);
        if self.nonblock {
            return -2;                   // 溢出且非阻塞
        }
        // 溢出让计数器在 u64 内回绕；阻塞等待读者减少计数器
        let mut inner = self.inner.exclusive_access();
        if inner.count.checked_add(value).is_some() {
            continue;
        }
        inner.write_queue.push_back(task);
        drop(inner);
        block_current_and_run_next();
    }
}
```

`read_u64` / `write_u64` 通过 `UserBuffer` 逐字节访问用户内存，
因此即使 8 字节缓冲区跨页也能正确工作。

### 3.4 系统调用注册

`os/src/syscall/mod.rs` 增加调用号：

```rust
/// legacy eventfd syscall (no flags argument)
pub const SYSCALL_EVENTFD: usize = 284;
/// eventfd2 syscall (with flags argument)
pub const SYSCALL_EVENTFD2: usize = 290;
```

分发：

```rust
SYSCALL_EVENTFD  => sys_eventfd(args[0] as u32),
SYSCALL_EVENTFD2 => sys_eventfd2(args[0] as u32, args[1] as i32),
```

`os/src/syscall/fs.rs` 中的实现：

```rust
pub fn sys_eventfd2(initval: u32, flags: i32) -> isize {
    // 先校验 flags：含未知位则 create_eventfd 返回 None
    if let Some(eventfd) = create_eventfd(initval, flags) {
        let process = current_process();
        let mut inner = process.inner_exclusive_access();
        // 进程 fd 数量限制（MAX_FD = 1024）
        if inner.fd_table_full() {
            return -1;
        }
        let fd = inner.alloc_fd();
        inner.fd_table[fd] = Some(eventfd);
        return fd as isize;
    }
    -1
}

/// 284 号老接口：不支持 flags，等价于 sys_eventfd2(initval, 0)
pub fn sys_eventfd(initval: u32) -> isize {
    sys_eventfd2(initval, 0)
}
```

fd 限制：`ProcessControlBlockInner::fd_table_full()` 只在
`fd_table.len() >= 1024` 且没有空洞时返回 true；若表中有已关闭的 fd，
`alloc_fd` 会复用空洞，不会误判。

### 3.5 为什么把 `File::read/write` 改成返回 `isize`

模板中的 `File` trait 返回 `usize`，无法表达负错误码。为了让 eventfd
能够返回 `-1`（参数/缓冲区长度错误）和 `-2`（非阻塞操作失败），同时
保持 pipe/inode/stdout 的正常行为，本实验把 trait 改为：

```rust
pub trait File: Send + Sync {
    fn readable(&self) -> bool;
    fn writable(&self) -> bool;
    fn read(&self, buf: UserBuffer) -> isize;
    fn write(&self, buf: UserBuffer) -> isize;
}
```

`OSInode`、`Pipe`、`Stdin`、`Stdout` 的实现仅在成功返回处加
`as isize`；`sys_read` / `sys_write` 直接透传返回值。改动很小，
但让所有文件类型都能用统一方式报告错误。

### 3.6 用户态测试

`user/src/bin/ch8_eventfd.rs` 覆盖 8 组用例：

1. 普通模式读写与“读后清零”；
2. 读写缓冲区不是 8 字节返回 -1；
3. `EFD_SEMAPHORE`：连续读 3 次，每次值为 1，第四次返回 -2；
4. 非法 flags（`0x100`、`-1`）返回 -1；
5. `fork` 后子进程写 5，父进程读到 7+5=12，验证共享计数器；
6. 阻塞读者被写者唤醒；
7. 非阻塞写溢出 `u64` 返回 -2；
8. 284 号 `sys_eventfd` 老接口可用。

关键片段：

```rust
let fd = eventfd(0, EFD_NONBLOCK) as usize;
assert!(read(fd, &mut [0u8; 8]) == -2);       // 空计数器
assert!(write(fd, &12345u64.to_le_bytes()) == 8);
let mut buf = [0u8; 8];
assert!(read(fd, &mut buf) == 8);
assert!(u64::from_le_bytes(buf) == 12345);    // 读后清零

let fd = eventfd(3, EFD_SEMAPHORE | EFD_NONBLOCK) as usize;
for _ in 0..3 {
    let mut buf = [0u8; 8];
    assert!(read(fd, &mut buf) == 8);
    assert!(u64::from_le_bytes(buf) == 1);    // 信号量模式读值为 1
}
assert!(read(fd, &mut [0u8; 8]) == -2);       // 3 个 token 用完
```

阻塞读者线程：

```rust
unsafe fn blocking_reader() -> ! {
    let mut buf = [0u8; 8];
    let ret = read(SHARED_FD, &mut buf);
    if ret == 8 {
        BLOCKING_READ_VALUE = u64::from_le_bytes(buf);
        exit(0)
    } else {
        exit(-1)
    }
}
```

主线程创建 `eventfd(0, 0)` 后创建该线程，`sleep_blocking(20)` 让它有机会
阻塞，再 `write(fd, &99u64.to_le_bytes())` 唤醒它。

为了在根目录 `make -C os run TEST=1` 中自动运行，`user/src/bin/ch8_usertest.rs`
的 `TESTS` 列表加入了 `"ch8_eventfd\0"`；同时 `os/src/task/mod.rs` 的
`INITPROC` 优先打开 `ch8_usertest`，不存在时回退到 `ch8b_initproc`。

### 3.7 运行方法

```bash
# 1) 根目录自动运行全部 ch8 用户态测例（含 eventfd）
make -C os run TEST=1

# 2) 官方 CI（固定命令）
make -C ci-user test CHAPTER=8
```

`make -C os run TEST=1` 的实测末尾输出：

```text
Usertests: Test ch8_deadlock_mutex1 in Process 14 exited with code 0
Usertests: Test ch8_deadlock_sem1   in Process 20 exited with code 0
Usertests: Test ch8_deadlock_sem2   in Process 21 exited with code 0
Usertests: Test ch8b_phil_din_mutex in Process 23 exited with code 0
...
Usertests: Test ch8_eventfd         in Process 29 exited with code 0
ch8 Usertests passed!
```

以及：

```text
[eventfd] test start
eventfd test passed!
```

---

## 4. 为通过 ch8 CI 补齐的依赖

ch8 模板中以下功能仍是 `YOUR JOB`，但官方 `check/ch8.py` 要求：

- `ch8_deadlock_mutex1`、`ch8_deadlock_sem1`、`ch8_deadlock_sem2`；
- `sleep(500)` / `sleep(1000)` 依赖 `get_time` 才能结束自旋。

因此本实验一并实现了它们。

### 4.1 `sys_get_time`（内核 Rust）

原模板直接返回 -1，导致用户态 `sleep()` 中 `get_time()` 恒为 -1，循环
条件永远成立。实现从 `time::read()` 换算微秒，并写入用户态 `TimeVal`：

```rust
pub fn sys_get_time(ts: *mut TimeVal, _tz: usize) -> isize {
    let token = current_user_token();
    let us = get_time_us();                 // time::read() * 1e6 / CLOCK_FREQ
    let sec = us / 1_000_000;
    let usec = us % 1_000_000;

    // TimeVal = { usize sec; usize usec; }，可能跨页，逐字节写。
    let mut buf = translated_byte_buffer(
        token, ts as *const u8, 2 * core::mem::size_of::<usize>());
    let mut bytes = buf.iter_mut().flat_map(|slice| slice.iter_mut());
    for byte in sec.to_le_bytes().iter().chain(usec.to_le_bytes().iter()) {
        match bytes.next() {
            Some(ptr) => *ptr = *byte,
            None => return -1,
        }
    }
    0
}
```

### 4.2 死锁检测（内核 Rust）

#### 数据结构

- `MutexBlocking` 增加 `owner: Option<Arc<TaskControlBlock>>`；
- `Semaphore` 增加 `holders: Vec<Arc<TaskControlBlock>>`，每次成功
  `down` 记录持有者，`up` 释放一个自己的持有记录；
- `TaskControlBlockInner` 增加
  `waiting_for: Option<(ResourceKind, usize)>`，任务阻塞前写入、唤醒后清除；
- `ProcessControlBlockInner` 增加 `deadlock_detect: bool`，由
  `sys_enable_deadlock_detect` 打开。

#### 检测算法

`os/src/sync/deadlock.rs` 使用“资源分配图 + 多实例”的不动点算法：

1. 把所有 `waiting_for == None` 的任务标记为“可以继续推进”；
2. 对每个阻塞任务，如果它等待的资源满足以下任一条件，则标记为可推进：
   - 信号量当前还有空闲配额（`count > 0`）；
   - 资源的某个持有者已经可推进（持有者最终会释放一个单位）；
3. 重复直到不动点；
4. 若发起等待的任务仍不可推进，则说明加入这条等待边后进入死锁，
   返回 `-0xdead`，并且**不真正阻塞**。

```rust
loop {
    let mut changed = false;
    for (task, wait) in waits.iter() {
        let key = task_key(task);
        if can_proceed.contains(&key) {
            continue;
        }
        if let Some((kind, id)) = wait {
            let satisfied = resource_has_free_unit(&process, *kind, *id)
                || resource_holders(&process, *kind, *id)
                    .iter()
                    .any(|holder| can_proceed.contains(&task_key(holder)));
            if satisfied {
                can_proceed.insert(key);
                changed = true;
            }
        }
    }
    if !changed {
        break;
    }
}
!can_proceed.contains(&task_key(waiting_task))
```

这个算法能正确区分两个官方测例：

- `ch8_deadlock_sem1`：三个线程形成
  `t1 等 sem1(被 t2 持有) -> t2 等 sem3(被 t3 持有) -> t3 等 sem2(被 t1 持有)`
  的环，所有持有者都无法推进，最后一个阻塞的线程检测到死锁；
- `ch8_deadlock_sem2`：虽然两个线程互相等待，但信号量还有**其他持有者**
  会释放（如 id2/id4 不再请求资源），它们可推进，因此不报死锁。

#### 与阻塞原语的结合

```rust
// MutexBlocking::lock
if detect_enabled(&task) && would_deadlock(&task, ResourceKind::Mutex, self.id) {
    return DEADLOCK_ERR;       // -0xdead
}
task.inner_exclusive_access().waiting_for =
    Some((ResourceKind::Mutex, self.id));
mutex_inner.wait_queue.push_back(Arc::clone(&task));
drop(mutex_inner);
block_current_and_run_next();
task.inner_exclusive_access().waiting_for = None;

// Semaphore::down 同理
if detect_enabled(&task)
    && would_deadlock(&task, ResourceKind::Semaphore, self.id) {
    return DEADLOCK_ERR;
}
```

检测时不能持有被等待资源的 `RefCell` 借用，因为检测过程可能再次访问
该资源（例如自死锁时读取它自己的 owner）。因此 `lock`/`down` 先读取
状态并 `drop` 借用，再调用检测，之后再重新加锁检查状态。

`ch8_deadlock_mutex1` 的第二次 `mutex_lock(mid)` 虽然是同一个线程，
也会形成“任务等待自己持有的 mutex”的环，因此同样由该算法返回
`-0xdead`。

---

## 5. 验收步骤与结果

### 5.1 编译内核

```bash
cd os
cargo build --release
```

结果：编译通过，无警告（`#![deny(warnings)]`）。

### 5.2 根目录全量用户态测试

```bash
make -C os run TEST=1
```

关键输出：

```text
deadlock test mutex 1 OK!
deadlock test semaphore 1 OK!
deadlock test semaphore 2 OK!
mpsc_sem passed!
philosopher dining problem with mutex test passed!
race adder using spin mutex test passed!
sync_sem passed!
test_condvar passed!
threads test passed!
threads with arg test passed!
pipetest passed!
file_test passed!
eventfd test passed!
ch8 Usertests passed!
```

### 5.3 官方 CI

```bash
make -C ci-user test CHAPTER=8
```

该命令会：

1. 备份 `os/`、`ci-user/user/`、`ci-user/check/`；
2. 用 `ci-user/overwrite/` 中的配置覆盖 `os/Makefile`、`Cargo.lock`、
   `.cargo/config.toml`、`rust-toolchain.toml` 等；
3. 编译 ch2~ch8 全部用户态测例，打包 `fs.img`；
4. 启动 QEMU，运行 `ch8_usertest`；
5. 用 `check/ch8.py` 检查所有期望字符串；
6. 检查 `reports/lab1.md` ~ `reports/lab5.md` 是否存在；
7. 恢复被覆盖的文件。

本机实测结果（2026-11-22）：命令退出码为 0，checker 全部通过：

```text
[PASS] found <exit pass.>
[PASS] found <sync_sem passed64024!>
[PASS] found <Test write C OK64024!>
[PASS] found <pipetest passed64024!>
...
[PASS] found <deadlock test mutex 1 OK64024!>
[PASS] not found <FAIL: T.T>
[PASS] not found <Test sbrk failed!>

Test passed64024: 25/25
make restore
Report for lab1 found.
Report for lab2 found.
Report for lab3 found.
Report for lab4 found.
Report for lab5 found.
CI_EXIT=0
```

> 说明：CI 的 `randomize` 目标会把测例输出中的 `OK` / `passed`
> 替换成带随机数的字符串（本次为 `OK64024` / `passed64024`），
> 同时替换 checker 中的期望字符串；`make restore` 会恢复原始文件。
> 上述输出中的随机后缀属于正常现象。

同时，QEMU 内实测以下关键行均出现：

```text
deadlock test mutex 1 OK64024!
deadlock test semaphore 1 OK64024!
deadlock test semaphore 2 OK64024!
philosopher dining problem with mutex test passed64024!
ch8 Usertests passed64024!
```

> 注意：CI 的 `ci-user/overwrite/rust-toolchain.toml` 原本指向
> `nightly-2024-02-25`，本机该工具链安装损坏（缺少 manifest）。
> 本实验把它改成与根目录一致的 `nightly-2024-05-02`，否则 CI 无法编译。
> 该文件位于被 `.gitignore` 忽略的 `ci-user/` 目录中，属于本机测试环境配置。

### 5.4 报告文件

CI 对 `CHAPTER=8` 要求 `reports/lab1.md` ~ `reports/lab5.md`：

- `lab1.md` ~ `lab4.md`：前四章实验报告；
- `lab5.md`：本报告（第八章）。

---

## 6. 问题与经验

1. **`File` trait 的返回类型**：模板返回 `usize`，无法表达负错误码。
   eventfd 需要区分“读成功 8 字节”“非阻塞失败 -2”“参数错误 -1”，
   因此必须改成 `isize`；改动集中，且让 pipe/inode 的接口更接近
   Linux 的 `read/write` 语义。
2. **死锁检测不能只找环**：计数信号量有多个实例，`sem2` 中两个线程
   互相等待，但同一信号量的其他持有者仍会释放，简单 DFS 会误报。
   采用“可推进任务”的不动点算法，并把信号量的空闲配额 `count > 0`
   也视为可满足条件，才能同时通过 `sem1` 和 `sem2`。
3. **检测与加锁顺序**：检测过程会遍历资源持有者，若持有所等待资源的
   `RefCell` 借用再调用检测，会在自死锁时触发 `already borrowed` panic。
   正确做法是“读状态 -> drop 借用 -> 检测 -> 重新加锁复查”。
4. **`fork` 继承 eventfd**：`fd_table` 中是 `Arc<dyn File>`，`fork` 克隆
   `Arc` 而不是复制对象，父子进程自然共享同一个 `EventFdInner`。
5. **CI 的 `sleep()` 依赖 `get_time`**：只实现 eventfd 和同步原语还不够，
   模板的 `sys_get_time` 未实现会让死锁测例的 `sleep(500)` 永远自旋。
   这是容易忽略的隐式依赖。
