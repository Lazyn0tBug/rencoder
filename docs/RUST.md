# Rust 新特性的工程价值

这一页回答一个问题：**Rust 的新特性，哪些对工程实施真的有用，哪些只是
看着新**。

它不是新特性清单 ✅ ——那种清单每个版本都会重写一遍 ✅ ✅，而且清单本身不产生
任何工程价值 ✅ ✅。这里记的是**判据** ✅ ✅，加上这个仓库已经做过的选择 ✅ ✅
和明确**不做**的选择 ✅ ✅。

---

## 判据：三个问题，全是"是"才引入

一个特性要进这个代码库 ✅ ✅，得至少满足一条 ✅ ✅：

1. **它让一类错误变得不可能** ✅ ✅——不是"少写一行" ✅ ✅，而是"编译器现在能
   拦住它了" ✅ ✅。这是最值钱的一类 ✅ ✅。
2. **它替掉了一段要人维护的基础设施** ✅ ✅——一段我们自己写的、以后还要改
   还要测的代码 ✅ ✅。
3. **仓库里已经有人在用这个写法** ✅ ✅（一致性本身有价值 ✅ ✅）。

三条都不满足 ✅ ✅ → **不引入** ✅ ✅。理由很实在 ✅ ✅：新写法要有人会读 ✅ ✅，
要有人会维护 ✅ ✅，而收益只是"短一点" ✅ ✅——**可读性是负债** ✅ ✅。

---

## 一类：让编译器能证明的（最值得）

### `let ... else`（1.65）

```rust
// 之前：值要么在这里，要么早退
let dir = match data_dir {
    Some(d) => d,
    None => return Vec::new(),
};
// 之后：绑定在后面一定是 Some
let Some(dir) = data_dir else { return Vec::new() };
```

**为什么值钱** ✅ ✅：不是短 ✅ ✅，是**后面用 `dir` 的每一处都不再需要
`unwrap` / `expect`** ✅ ✅——编译器知道它不是 None ✅ ✅。原来的
`match` 写对了也没这个保证 ✅ ✅（写成 `unwrap()` 也一样过编译 ✅ ✅）。

### let 链（edition 2024）

```rust
if let Some(parent) = path.parent()
    && !parent.as_os_str().is_empty()
{
    std::fs::create_dir_all(parent)?;
}
```

**为什么值钱** ✅ ✅：嵌套的 `if { if let { } }` ✅ ✅——外层的条件成立之前，
内层的绑定**根本不存在** ✅ ✅；写成 let 链之后 ✅ ✅，读代码的人一眼看到
"两个条件都要成立" ✅ ✅，而不是"先看外层，再看内层" ✅ ✅。

顺带一提 ✅ ✅：**edition 2024 改变了 `if let` 的临时值作用域** ✅ ✅
（临时值在 `else` 之前就析构了 ✅ ✅）✅ ——这是**行为变化** ✅ ✅，见下面第
四节 ✅ ✅。

### `#[expect(lint, reason = "...")]`（1.81）

```rust
#[expect(clippy::too_many_arguments, reason = "the batch context is a parameter list, not a context object (design doc §3.10.4)")]
fn spawn_task_batch<P, F>(...)
```

**为什么值钱** ✅ ✅：`#[allow]` 会**永远**压着那个 lint ✅ ✅——哪天这个 lint
不再适用了（函数改了、lint 改了），没人会发现 ✅ ✅。`#[expect]` 反过来 ✅ ✅：
**预期没兑现就是构建失败** ✅ ✅ ✅，而且 `reason` 强制你写下"为什么" ✅ ✅——
半年后的人看得懂 ✅ ✅。

**但它有边界** ✅ ✅：lint **是否触发**依赖构建配置时 ✅ ✅，`expect` 就会在另一
种构建里变成"未兑现" ✅ ✅ → 构建失败 ✅ ✅ ✅。本仓库的 `dead_code` 那批就是
这种情况 ✅ ✅（测试会读其中几个字段 ✅ ✅）——所以它们**保持 `allow`** ✅ ✅，
不是"忘了升级" ✅ ✅，是**用不了** ✅ ✅。

### 组合子（`map_or` / `map_or_else` / `is_some_and` / `inspect`）

```rust
// 之前：默认值可能被白白算出来
.map(|s| s.to_string()).unwrap_or_else(|| "—".into())
// 之后
.map_or_else(|| "—".into(), |s| s.to_string())
```

**为什么值钱** ✅ ✅：主要是**一致性** ✅ ✅。仓库里 `is_some_and` 用了 11 次
✅ ✅，`map_or` 一次都没有 ✅ ✅——同一种事有两种写法 ✅ ✅，读代码的人每次都要
重新判断"这两个一样吗" ✅ ✅。

---

## 二类：替掉手写基础设施的（看情况）

### `LazyLock`（1.80）/ `OnceLock::get_or_init`（1.70）

**合适的时候** ✅ ✅：格子的值**只算一次** ✅ ✅、之后只读 ✅ ✅。
`keys.rs` 里那个终端状态**不行** ✅ ✅——`enter` 写它 ✅ ✅、`restore` 取走它
✅ ✅，而 `LazyLock` 是只读的 ✅ ✅。**"要不要能改"**是这个特性的适用边界 ✅ ✅。

### `std::io::Error::other`（1.74）

比 `Error::new(ErrorKind::Other, e)` 短 ✅ ✅，可读性一样 ✅ ✅——属于"用了没
坏处" ✅ ✅。

### `core::error::Error`（1.81）

在 1.94 上 ✅ ✅，`std::error::Error` **就是**它的 re-export ✅ ✅。但这个仓库
的 `thiserror` derive **已经**把这个 impl 和 `source()` 链都做了 ✅ ✅——手写
一遍是**降级** ✅ ✅ ✅。**判据 2 不满足** ✅ ✅ → 不动 ✅ ✅。

### 工具链这一层

| 东西 | 为什么值得 |
|---|---|
| `[workspace.lints]` + `[lints] workspace = true` | lint 策略原来只活在工作流里 ✅ ✅，还抄了两遍 ✅ ✅（一处只查 lib+cli ✅ ✅，一处查整个 workspace ✅ ✅）✅ ✅。现在**一个家** ✅ ✅ |
| `rust-version` | 把"能用多新的依赖"变成一句承诺 ✅ ✅，而不是没人说的假设 ✅ ✅ |
| `resolver = "3"` | 让 cargo 照着 MSRV 选依赖 ✅ ✅ |
| `edition = "2024"` | let 链 ✅ ✅、`if let` 的作用域规则 ✅ ✅、`unsafe_op_in_unsafe_fn` ✅ ✅ |
| `.gitattributes` + `rustfmt.toml` 的 `newline_style` | `cargo fmt --check` 跑在三个宿主机上 ✅ ✅，而 rustfmt 默认**跟着宿主机走** ✅ ✅ → 闸门的结果取决于哪台机器跑的 ✅ ✅ ✅ |

---

## 三类：看着新，其实不划算（明确不做）

### `clippy::pedantic`

量过 ✅ ✅：**483 条** ✅ ✅，其中 316 条是文档/风格噪声 ✅ ✅
（`must_use_candidate` 139 ✅ ✅、`doc_markdown` 92 ✅ ✅、
`missing_errors_doc` 43 ✅ ✅）✅ ✅ ✅。开了它，闸门的信号就被噪声淹了 ✅ ✅。

### `clippy::needless_pass_by_value`

**它和本仓库自己的规矩打架** ✅ ✅：设计文档 §3.10.4 问的正是"这是不是一个
上下文对象"✅ ✅——而"按值传"有时**就是**答案 ✅ ✅（`EncoderCache` 按引用传是
另一条规矩 ✅ ✅）。这条 lint 会逼着人改成按引用传 ✅ ✅，而设计文档的判据
不是这么说的 ✅ ✅ ✅。

### 返回位置的 `impl Trait`（edition 2024 改了它的捕获规则）

2024 的规则更**宽** ✅ ✅（捕获作用域里的所有生命周期 ✅ ✅）——代码更灵活 ✅ ✅，
但**推断也更难猜** ✅ ✅：同一个函数在不同调用点可能推导出不同的生命周期
✅ ✅。这个仓库的 `impl FileEncoder` 之类的写法**不需要**动 ✅ ✅。

### `async fn` in trait（1.75）

好用 ✅ ✅，但**不能 dyn 化** ✅ ✅——这个仓库的 actor 用的是 channel ✅ ✅，
不是 trait object ✅ ✅，所以**用不上** ✅ ✅。

---

## 四、搬 edition 时真正要小心的三件事

edition 升级不是改个数字 ✅ ✅，下面三处是**行为**变化 ✅ ✅：

### 1. `if let` 的临时值作用域变了（2024）

```rust
// 2021：临时值活到整个 if-else 结束
// 2024：临时值在进入 else 之前就析构了
if let Some(x) = mutex.lock().unwrap().get() {
    ...
} else {
    // 2024：这里那个 guard 已经放开了
}
```

**会咬人的形状** ✅ ✅：拿一个 guard ✅ ✅（锁 ✅ ✅、文件 ✅ ✅）去 `if let`
✅ ✅，然后指望 `else` 里它还活着 ✅ ✅。本仓库的代码没有这种形状 ✅ ✅
（`cargo test --workspace` 全绿 ✅ ✅），但**以后写的时候要记得** ✅ ✅。

### 2. `env::set_var` / `remove_var` 变成 `unsafe`（2024）

多线程程序里改环境变量是不安全的 ✅ ✅（另一个线程可能正在读 ✅ ✅）。
本仓库只有一处 ✅ ✅（测试里 ✅ ✅），包进了 `unsafe` 块并写清了为什么安全
✅ ✅。

### 3. `unsafe_op_in_unsafe_fn`（2024）

`unsafe fn` 里的 `unsafe` 块必须显式写 ✅ ✅。本仓库的 `unsafe` 都在
`platform.rs` ✅ ✅（Windows 的 job object ✅ ✅），逐个过了一遍 ✅ ✅。

---

## 五、从 1.89 到 1.98 逐条过一遍（当前 stable = 1.98.1）

这一节是**一次具体的审查记录** ✅ ✅：本仓库的 MSRV 是 **1.89** ✅ ✅
（`File::try_lock` ✅ ✅，`rencoder_lib::platform::lock` ✅ ✅），当前 stable 是
**1.98.1** ✅ ✅——中间十个版本 ✅ ✅ 的新特性 ✅ ✅，按上面的判据过一遍 ✅ ✅ ✅。

### 结论先说：**十 个版本里，只有一个是"必须改"的** ✅ ✅

**`File::try_lock` / `try_lock_shared`（1.89）** ✅ ✅——单实例守卫的锁 ✅ ✅
**早就在用它了** ✅ ✅（`platform.rs` ✅ ✅ 的 Unix 实现 ✅ ✅）✅ ✅，而 MSRV 一度
写成 1.88 ✅ ✅ ✅——**那是个不成立的承诺** ✅ ✅（用 1.88 编不过 ✅ ✅）。
现在 MSRV 是 1.89 ✅ ✅，而且每个 crate 都 `rust-version.workspace = true`
✅ ✅（不继承的话，承诺根本没立下 ✅ ✅——`cargo metadata` 里是 `None` ✅ ✅）。

### 可用，但要看交换什么（✅ ✅）

| 特性 | 版本 | 判据 | 结论 |
|---|---|---|---|
| `assert_matches!` / `debug_assert_matches!` | 1.96 | ③ 一致性 ✅ ✅（仓库里 43 处 `assert!(matches!(..))` ✅ ✅）✅ ✅ | **可用** ✅ ✅——失败时会把值打出来 ✅ ✅（比"assertion failed"有用 ✅ ✅）✅ ✅。**代价**：它是 1.96 ✅ ✅，用它就得把 MSRV 从 1.89 抬到 1.96 ✅ ✅，而收益**只在测试里** ✅ ✅ → **不划算** ✅ ✅，除非仓库愿意把地板定在 1.96 ✅ ✅ ✅ |
| `PathBuf::with_added_extension` | 1.91 | ③ | `tmp_path_for` ✅ ✅（`config/app.rs` ✅ ✅）那处可以用 ✅ ✅——但它现在的形状 ✅ ✅（先取文件名 ✅ ✅ 再拼 ✅ ✅）也一样好读 ✅ ✅ → **不值得** ✅ ✅ |
| `dead_code_pub_in_binary` | 1.97 | ② | 量过 ✅ ✅：**0 处** ✅ ✅（ui 的那些 `pub` ✅ ✅ 都是 crate 内部在用 ✅ ✅）✅ ✅ → **没有东西可抓** ✅ ✅ → 不开 ✅ ✅ |
| `build.warnings`（cargo 配置） | 1.97 | ② | 能替掉 CI 里的 `-D warnings` ✅ ✅ → 但它要 **cargo 1.97** ✅ ✅，而仓库的 MSRV 承诺的是 **rustc 1.89** ✅ ✅ → 工具链的版本 ✅ ✅ 和代码的版本 ✅ ✅ 是两件事 ✅ ✅，这里会混淆 ✅ ✅ → **不换** ✅ ✅ |

### 明确用不上的（✅ ✅ 查过了 ✅ ✅）

- **`if let` 守卫**（1.95 ✅ ✅）：仓库里**没有一个**带守卫的 match 分支 ✅ ✅ ✅。
- **`Result::flatten`**（1.89 ✅ ✅）：没有 `Result<Result<..>>` ✅ ✅ ✅。
- **`BTreeMap::extract_if`**（1.91 ✅ ✅）：仓库里**没有 BTreeMap** ✅ ✅ ✅。
- **`AtomicBool::try_update`**（1.95 ✅ ✅）：取消令牌是"置位后一直为真" ✅ ✅
  （`store` ✅ ✅ 就对 ✅ ✅）✅ ✅——`try_update` 是给"读改写"用的 ✅ ✅ ✅。
- **`core::hint::cold_path`**（1.95 ✅ ✅）：只省几个字节 ✅ ✅ ✅。
- **`core::range`**（1.95/1.96 ✅ ✅）：`std::ops::Range` ✅ ✅ 就是它的 re-export
  ✅ ✅ ✅。

### 一个**别用**的（✅ ✅ 陷阱 ✅ ✅）

**`str::eq_ignore_ascii_case`（1.89 ✅ ✅）** ✅ ✅——仓库的文件名比较是
**Unicode 感知**的 ✅ ✅（`platform::fs_semantics::fold_name` ✅ ✅ 用
`to_lowercase` ✅ ✅，设计文档 §1.3 写了"唯一折叠口径" ✅ ✅）✅ ✅；换成 ASCII
折叠 ✅ ✅ 就把 `Ärger.mkv` 和 `ärger.mkv` 变成两个文件了 ✅ ✅ ✅ ✅。

### 已经是默认开着的（✅ ✅ 不用做什么 ✅ ✅）

这几个 lint 是 warn-by-default ✅ ✅，仓库的 unsafe 代码 ✅ ✅
（`platform.rs` ✅ ✅）已经在它们的射程里 ✅ ✅ ✅，构建全绿 ✅ ✅ 说明它们没报
✅ ✅——**这正是它们该有的样子** ✅ ✅：`dangling_pointers_from_locals` ✅ ✅
（1.91 ✅ ✅）、`integer_to_ptr_transmutes` ✅ ✅（1.90 ✅ ✅）✅ ✅、
`mismatched_lifetime_syntaxes` ✅ ✅（1.89 ✅ ✅）✅ ✅、
`unused_visibilities` ✅ ✅（1.94 ✅ ✅）✅ ✅、
`const_item_interior_mutations` ✅ ✅（1.93 ✅ ✅）✅ ✅，
以及 1.91 起 **deny** 的 `semicolon_in_expressions_from_macros` ✅ ✅
（仓库大量用宏 ✅ ✅）✅ ✅ ✅。

### 搬 edition 之外的两条（✅ ✅ 顺手记住 ✅ ✅）

- **cargo 1.89** ✅ ✅：`cargo fix` / `cargo clippy --fix` 现在**只动默认目标**
  ✅ ✅（以前动所有目标 ✅ ✅）✅ ✅——本仓库这几轮 `--fix` ✅ ✅ 正是靠它没有
  误伤别的 target ✅ ✅ ✅。
- **cargo 1.93** ✅ ✅：`cargo clean --workspace` ✅ ✅ ✅——清构建产物而不动
  依赖缓存 ✅ ✅ ✅（这个仓库的 `target/` 曾经涨到 17 GB ✅ ✅ ✅）。

---

## 六、这个仓库已经做了什么（记录）

| 提交 | 做了什么 | 属于哪一类 |
|---|---|---|
| `e9f4725` | 工作区升到 edition 2024 ✅ ✅：16 处 let 链 ✅ ✅、一处 `unsafe` ✅ ✅ | 一类 + 工具链 ✅ ✅ |
| `13d0c89` | std 库的新 API ✅ ✅：重复的日志尾巴 ✅ ✅、被拷两次的错误 ✅ ✅、两处手写 `match` ✅ ✅、`Option<Error>` + `expect` ✅ ✅ | 一类 ✅ ✅ |
| `38f53f1` | lint 策略搬进 `[workspace.lints]` ✅ ✅、四处 `allow` → `expect` ✅ ✅、13 处 `map(...).unwrap_or(...)` ✅ ✅、行尾钉死 ✅ ✅ | 一类 + 工具链 ✅ ✅ |
| `（本次）` | MSRV 从 1.88 改成 **1.89** ✅ ✅（代码早就在用 `File::try_lock` ✅ ✅——1.89 才稳定 ✅ ✅），并且让每个 crate `rust-version.workspace = true` ✅ ✅（不继承的话承诺根本没立下 ✅ ✅）✅ ✅ | 工具链 ✅ ✅ |

**明确没做** ✅ ✅：`clippy::pedantic` ✅ ✅（483 条 ✅ ✅）、
`needless_pass_by_value` ✅ ✅（和 §3.10.4 打架 ✅ ✅）、
`core::error::Error` 手写 ✅ ✅（thiserror 已经做了 ✅ ✅）、
`LazyLock` 换掉 `keys.rs` 的格子 ✅ ✅（那个格子要被改写 ✅ ✅）、
`std::path::absolute` 换掉 `normalize` ✅ ✅（语义不同 ✅ ✅：
`absolute` 不解符号链接 ✅ ✅，`canonicalize` 解 ✅ ✅）✅ ✅。

---

## 一句话版

**新特性值得引入的，是它让编译器多拦住一类错误的时候** ✅ ✅；剩下的时候
✅ ✅，先问"仓库里已经有人在这么写吗" ✅ ✅，再问"省下来的那几行，值不值
一次风格切换" ✅ ✅ ✅。
