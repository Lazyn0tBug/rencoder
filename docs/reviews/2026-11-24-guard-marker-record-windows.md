# 问题：Windows 上占用标记里的 pid/since 记录读不出来（单实例守卫 §3.11.1.1）

- 状态：**待拍板**。本文件只陈述现象、根因、契约冲突与候选方案，**不含实现**。
- 发现日期：2026-11-24
- 失败测试：`rencoder_lib` → `guard::tests::taking_a_free_target_works_and_writes_the_record`
- 相关契约：`docs/design.md` §3.11.1.1（`docs/design.md:4238` 起）
- 严重度：互斥本身**不受影响**；受影响的是「另一个实例正在处理这个目录（pid 1234，15:00 起）」这条报错——Windows 上它**丢掉 pid 与开始时刻**。
- 影响面：Windows 用户的报错信息；Windows CI 的 core job 一直是红的。

---

## 一、现象

在 Windows 上跑核心测试：

```bash
cargo test -p rencoder_lib -p rencoder_cli
```

失败项：

```
---- guard::tests::taking_a_free_target_works_and_writes_the_record stdout ----
thread 'guard::tests::taking_a_free_target_works_and_writes_the_record' panicked at
rencoder_lib\src\guard.rs:425:43:
the record must be readable
```

对应断言（`rencoder_lib/src/guard.rs:415-433`）：

```rust
let guard = TargetGuard::acquire(&app).expect("the target is free");

let marker = marker_path(&app);
assert!(marker.exists(), "the marker must exist while held");   // ✅ 通过
let holder = read_holder(&marker).expect("the record must be readable");  // ❌ 失败
assert_eq!(holder.pid, std::process::id());
```

标记文件**存在**（第 424 行过了），但**内容读不出来**（第 425 行炸）。

同一测试在 macOS / Linux 上是绿的——这不是随机故障，是平台差异。

---

## 二、根因

### 2.1 机制

Windows 上 std 的 `File::try_lock()` 用 `LockFileEx` 对**整个字节区间**（等价于 `[0, u64::MAX)`）加独占锁。这个锁**覆盖文件里的每一个字节**，于是：

> **持锁期间，任何其他句柄都读不出这个文件的内容**（`ERROR_LOCK_VIOLATION`，错误码 33）。

Unix 上 `flock` 是**建议锁**（advisory）：它挡的是「另一个进程也去加锁」，不挡读。所以同样的代码在 Unix 上能读。

代码路径（`rencoder_lib/src/guard.rs`）：

| 位置 | 做什么 |
|---|---|
| `guard.rs:240` | `file.try_lock()` —— 独占锁，覆盖整个文件 |
| `guard.rs:262` | `let _ = write_holder(&mut file, &record);` —— 写记录（**能写**，写不受影响） |
| `guard.rs:383-386` | `read_holder()` —— `fs::read_to_string(path)` **开新句柄读** → Windows 上失败 → `None` |
| `guard.rs:176` | `Occupancy::Held(read_holder(&path))` —— 探测到占用时读记录 → Windows 上拿到 `None` |
| `guard.rs:245` | `acquire` 撞到占用时 `holder: read_holder(&marker)` → 同上 |

### 2.2 实证

用一个独立小程序验证（不依赖本仓库代码）：

```rust
let mut f = OpenOptions::new().read(true).write(true).create(true).truncate(false).open(&p)?;
f.try_lock()?;
let _ = f.set_len(0).and_then(|_| f.write_all(b"hello"));
let _ = f.seek(SeekFrom::Start(0));
println!("same handle, seek 0: {:?}", (&mut f).read_to_string(&mut buf));  // Ok(5) -> "hello"
println!("new handle: {:?}", std::fs::read_to_string(&p));                  // Err(code 33)
```

结果：

```
write on locked handle: Ok(())
read via NEW handle:    Err(Os { code: 33, kind: Uncategorized,
                               message: "另一个程序已锁定文件的一部分，进程无法访问。" })
read via SAME handle:   Ok(5) -> "hello"
```

三条事实：

1. **写**在锁上不受影响——记录确实写进去了；
2. **持锁者自己**用同一个句柄能读回自己的记录；
3. **别人**（任何新句柄）读不出来——这正是第 2 步失败的原因。

所以：**记录不是没写，是"只有写它的人读得到"**。测试自己持锁、却用新句柄读，所以读不到。

### 2.3 这不是测试写错了

`guard.rs:59` 的 `Occupancy` 注释确实写了「记录是尽力而为：可能读不出来」：

```rust
/// Another instance holds it. The record is best-effort: it may be
/// unreadable, or still name the previous holder.
Held(Option<Holder>),
```

但设计文档把「读不出记录」定成了**例外**而非常态，并且点名了要读出 pid（§3.11.1.1）：

| 出处 | 表述 |
|---|---|
| `design.md:4266` | 「**互斥交给内核的文件锁** ✅ ✅：锁在目标目录里的标记文件 ✅ ✅（`.rencoder-owner` ✅ ✅）上 ✅ ✅，独占且非阻塞 ✅ ✅。」 |
| `design.md:4276-4278` | 「标记文件里记什么 ✅ ✅：**只有 pid 与开始时刻** ✅ ✅ …… 这两个字段**只用于报错** ✅ ✅："另一个实例正在处理这个目录（pid 1234 ✅ ✅，15:00 起 ✅ ✅）" ✅ ✅。」 |
| `design.md:4280-4284` | 「**探测只读** ✅ ✅：问"这个目录有没有人占着"用**只读**打开标记文件 ✅ ✅ + **共享锁** ✅ ✅。」 |
| `design.md:4286-4288` | 「**用完就删** ✅ ✅：应用不该在用户的目录里留东西 ✅ ✅。释放的**顺序**是关键 ✅ ✅ —— **先删名字、后放锁** ✅ ✅。」 |
| `design.md:4582`（§8 row 99） | 「标记只记 pid 与开始时刻 ✅ ✅，用完就删 ✅ ✅（先删名字后放锁 ✅ ✅）」 |
| `design.md:1977`（布局树） | `<操作目录>/ ├── cancel ├── capabilities.json └── .rencoder-owner # 占用标记（跑批期间存在，跑完删掉）` |

**冲突点**：文档要求「读出错的人能拿到 pid」，而 Windows 的锁语义让「持锁期间读不出 pid」成为**必然**，不是偶然。文档把"读不出"写成了 best-effort 的兜底，代码里 `Occupancy::Held(None)` 正好能接住它——但那条路径在 Windows 上会**每次**命中，于是文档承诺的报错在 Windows 上永远缺 pid。

### 2.4 为什么 CI 一直是红的

- 守卫与其测试：`550c001`（2026-11-23，`feat(guard): 单例守卫`）
- CI 工作流：`f0e110e`（2026-11-23，晚 24 个提交）

CI 的 core job 自建立起就在 `windows-2022` 上跑 `cargo test -p rencoder_lib -p rencoder_cli`（`.github/workflows/ci.yml`）。守卫测试比 CI 早 24 个提交存在 ⇒ **Windows core job 从第一次跑起就是红的**，红的正是这条测试。

---

## 三、候选方案

三个方案都能让 `taking_a_free_target_works_and_writes_the_record` 在 Windows 上变绿。分歧在**要不要保住"报错带 pid"这条承诺**，以及**为此付什么代价**。

### A 案：记录另立文件（锁文件与记录文件分开）

`.rencoder-owner` 继续当**锁**（空文件，独占锁照旧），pid/since 记录挪到旁边的 `.rencoder-owner.json`。

- 记录文件**没被锁** → 任何进程任何时候都读得到 ⇒ **全平台**报错都带 pid。
- 纯 Rust，无 `unsafe`，lib 保持平台无关。
- 代价：运行期间目标目录里多一个文件 ⇒ 布局树、§3.11.1.1、§8 row 99 都要改；`APP_OWNED_FILE_NAMES`（`config/paths.rs:52-56`）从 3 项变 4 项（采集器要跳过它）。
- 代价（Windows 特有）：读记录的人开着记录文件时，持有者删它会失败（Windows 不允许删已打开的文件）⇒ 可能留垃圾文件。目前删除失败只记 warning（`guard.rs:295-303`），"不留垃圾"这条承诺要另想办法（例：下次 acquire 时顺手清掉）。
- 释放顺序要重新想：`先删名字、后放锁` 是针对**锁文件**说的（名字消失时锁还在，别人抢不走）。记录文件不在这个不变式里，可以先删记录再删锁文件。

### B 案：单文件，收窄契约

记录留在 `.rencoder-owner` 里，承认「Windows 上持锁期间读不出」。

- 改动最小：不改布局、不改公开 API、不需要平台代码。
- 代价：Windows 的报错**永远不带 pid**——文档承诺打折，得在 §3.11.1.1 写明。
- 测试要改：不能再用新句柄读，得让守卫**用它自己持有的句柄**读回记录（实证第 2 条：这样能读）。即给 `TargetGuard` 加一个读回记录的方法，测试改用它。这样测的仍是「记录确实写进了磁盘」，只是读法换了。
- `Occupancy::Held(None)` 这条兜底路径在 Windows 上变成常态。

### C 案：Windows 上锁一个稀疏区间

一个文件、一套布局都不变。Windows 上改用裸 `LockFileEx`，把锁区间放在**记录之后**（例如 `[2^62, 2^62+1)`），记录那几十字节就落在锁区间之外，照样能读。

- 布局、公开 API、文档结构全不动；**全平台**报错都带 pid。
- 代价：`rencoder_lib` 要新增 `windows-sys` 依赖 + 一段 `unsafe` 的 `LockFileEx`/`UnlockFileEx`。而 `rencoder_lib` **今天完全没有平台代码**，设计文档还专门强调过这点（`design.md:4273-4274`：「**不需要平台差异** ✅ ✅…… Unix flock ✅ ✅ / Windows LockFileEx ✅ ✅ **由标准库承担** ✅ ✅」）。C 案等于把 std 承担的那部分拿回自己写。
- 风险：`unsafe` FFI、区间算错会静默失去互斥。

### 对比

| | A 案：记录另立文件 | B 案：单文件收窄契约 | C 案：Windows 稀疏锁 |
|---|---|---|---|
| 目录里的文件 | `.rencoder-owner` + `.rencoder-owner.json` | `.rencoder-owner` | `.rencoder-owner` |
| 持锁期间记录可读 | ✅ | ❌（Windows） | ✅ |
| 报错带 pid | ✅ 全平台 | ❌ 仅 Windows 缺 | ✅ 全平台 |
| 需要平台代码 / `unsafe` | ❌ | ❌ | ✅ 需要 |
| 公开 API 变化 | 无（新增一个内部路径函数） | 新增「用自有句柄读回记录」的方法 | 无 |
| 设计文档改动 | §3.11.1.1 + 布局树 + §8 | §3.11.1.1 补一句 | §3.11.1.1 补锁区间说明 |
| 采集排除名单 | 3 → 4 项 | 不变 | 不变 |
| Windows 删除记录文件的边角 | 有（可能留垃圾） | 无 | 无 |

### 推荐

**倾向 A 案**：它是唯一既保住"报错带 pid"、又不往 lib 里塞平台代码的选项；代价（一个伴随文件）是**可见且可解释**的，也符合"标记只记 pid 与开始时刻"这条既定约定——记录还在，只是换了个文件装。

C 案保住了单文件布局，但要为它写 `unsafe` FFI，与 `rencoder_lib` 保持平台无关的现状相冲；B 案最省事，但让一条已写进文档的承诺在 Windows 上永久失效。

**这个取舍需要拍板**，不急着实施。

---

## 四、影响面（A 案）

| 文件 | 改动 |
|---|---|
| `rencoder_lib/src/guard.rs` | 新增记录文件名与路径；`write_holder`/`read_holder` 改指记录文件；`TargetGuard` 多带一个待删路径；释放顺序 |
| `rencoder_lib/src/config/paths.rs:52-56` | `APP_OWNED_FILE_NAMES` 3 → 4 项 |
| `rencoder_cli/src/main.rs:227` | 只调 `TargetGuard::acquire`，**大概率不动** |
| `docs/design.md` §3.11.1.1 | 锁/记录分家、释放顺序、记录文件的删除策略 |
| `docs/design.md:1974-1978` | 布局树加一行 |
| `docs/design.md` §8 row 99 | 同步 |

B 案 / C 案的改动面分别见上表。

---

## 五、验收口径（不论选哪个方案）

1. `cargo test -p rencoder_lib -p rencoder_cli` 在 **Windows**、**macOS**、**Linux** 三处全绿。
2. 守卫的其余测试（`a_held_target_refuses_the_second_taker`、`a_held_ancestor_refuses_the_child`、`held_descendant` 路径）行为不变。
3. 真正撞占用的场景要验到：**报错文案里带 pid 与开始时刻**（Windows 与 Unix 各验一次）——这是 §3.11.1.1 明写的验收点，只跑单测不够。
4. 占用期间目标目录里**不留垃圾**（A 案要专门验 Windows 上记录文件的删除）。
5. `cargo clippy --workspace --all-targets -- -D warnings` 与 `cargo fmt --check` 保持绿。

---

## 附录 A：同批已修的两个问题（本次会话，Windows 特有）

这两个已经在工作区修好并验过，列在这里是为了让本文件成为这次「修 clippy 与测试错误」的完整记录。

### A.1 `detect` 在 Windows 上启动即崩（信号处理器装了两遍）

- 现象：`detect_subcommand_prints_a_report` 失败，stderr 是
  `panicked at rencoder_cli\src\signal.rs:177:9: signal handler already installed`。
- 根因：`main()` 已经装过一次处理器（`rencoder_cli/src/main.rs:44-45`），`detect_cmd()` 又装一次（`main.rs:117-118`）。Windows 上控制台处理器的槽位是 `OnceLock`（`signal.rs:156`），第二次 `set` 直接 panic（`signal.rs:177-180`）。Unix 上 `signal-hook` 允许多次注册，只是多起一个线程，所以不炸。
- 顺带修掉的 Unix 行为问题：`detect` 原先装的是**自己的**槽位，于是 `main` 那个槽位的取消文件路径始终是空的 ⇒ Ctrl-C 会走 `signal.rs:86-88` 那条 `exit(130)`，而不是写取消文件走优雅取消。现在 `detect` 用 `main` 的槽位，`prepare()` 把取消文件路径填进去，优雅取消路径在 `detect` 上也通了。
- 改法：`detect_cmd` 改收 `&signal::CancelSlot`，不再自己建槽位、不再自己装处理器。`signal.rs` 未改——它那句"一个进程装一次"的断言是有价值的护栏，正是它抓出了这个 bug。

### A.2 分析 CSV 的路径断言写死了 POSIX 分隔符

- 现象：`analyze_subcommand_produces_csv_for_real_videos`、`analyze_subcommand_writes_csv_to_file` 失败，断言 `content.contains("/x.mp4,30,")` 不成立。
- 根因：CSV 里写的是**系统原生路径**（`rencoder_lib/src/analyzer/records.rs:411` 的 `r.path.to_string_lossy()`）——Windows 上是 `C:\...\x.mp4`，断言里的 `/` 匹配不上。**产品行为是对的**（路径就该是原生的），是测试写死了分隔符。
- 改法：测试里加一个 `with_slashes()` 把 `\` 归一成 `/` 再匹配，保留原断言"路径以分隔符 + 文件名结尾"的原意。

---

## 附录 B：本机环境备注（复现用）

这台机器跑不动默认的构建配置：`cargo test` 编译 `polars-core` 时 rustc OOM
（`rustc-LLVM ERROR: out of memory` / `STATUS_STACK_BUFFER_OVERRUN`），页面文件被固定在 15 GB 且已用掉 9.3 GB，改不了（无管理员权限）。

本机上验证用的命令：

```bash
CARGO_BUILD_JOBS=1 CARGO_PROFILE_DEV_DEBUG=0 cargo test -p rencoder_lib -p rencoder_cli --no-fail-fast
```

`CARGO_PROFILE_DEV_DEBUG=0` 关掉 debuginfo，polars 的 rlib 才编得出来；`--no-fail-fast` 才跑得到全部测试目标（默认遇到第一个失败就停，后面的目标根本没跑，`guard` 那条失败一开始就被藏住了）。

CI 的 Windows job 不受这个影响——GitHub runner 的内存宽裕得多。
