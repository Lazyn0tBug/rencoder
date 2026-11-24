discuss 1:
我的结论是：**选 A 案（锁文件和记录文件分开），但文档里 A 案的方案要补几处**。另外，我认为你们漏掉了一个问题：那条"测试缺口"才是这个 bug 一直没被发现的真正原因。

---

## 一、根因判断：成立

文档里的分析是对的，我核对了几个关键点：

- **std 的 `try_lock` 确实锁整个文件。** Windows 上 std 调 `LockFileEx` 时，偏移是 0，长度两个 32 位字段都填 `u32::MAX`，所以文件里每个字节都在锁区间内。Windows 的区间锁是**强制锁**，别的句柄读这个区间就会返回错误码 33。
- **探测路径在 Windows 上必然读不到记录。** 探测时用共享锁去撞独占锁，失败后判定为 `Held`，然后开新句柄读记录，又失败，最后得到 `Held(None)`。这条路径每次都会走到，不是偶发。
- **所以问题不只是测试红了。** 文档 §3.11.1.1 承诺的报错带 pid，在 Windows 上**一次都没兑现过**。

## 二、被忽略的问题：测试为什么没早点抓到它

`Occupancy::Held(Option<Holder>)` 把"读不到记录"定义成了合法状态，于是 `a_held_target_refuses_the_second_taker` 这类测试很可能只断言了"被拒绝"，没断言"拒绝时带着 pid"。真正失守的那条承诺，从来没有测试守着。这次碰巧是 `taking_a_free_target...` 用新句柄读才暴露出来。

修复时应该把这个缺口一起补上：

- **把验收第 3 条写成单测，不用手工验。** Windows 的 `LockFileEx` 按句柄生效，Unix 的 `flock` 按打开的文件描述生效，所以**同一个进程里开第二个句柄**去探测或 acquire，在两个平台上都会真的撞到占用。测试可以直接断言 `Held(Some(h))` 且 `h.pid == std::process::id()`，三个平台上都能跑，不需要另起进程。
- 有了这条测试，以后谁把实现改回单文件，Windows CI 会马上变红。

## 三、对三个方案的评估

### B 案：不建议

它等于承认这条承诺在 Windows 上永久失效。改成用持有者自己的句柄读回记录之后，测的是"能写进磁盘"，不是"别人能读到"，测试没有守住真正的需求。

### C 案：代价比文档写的还高

- `unsafe` FFI 和平台代码，这一点文档已经说了。
- 还有一个文档没提的坑：**探测方也必须改用同一个稀疏区间。** 如果探测方继续用 std 的 `try_lock_shared`，它会对整个文件加共享锁。Windows 的共享锁会禁止所有句柄写这个区间，持有者自己的句柄也一样。结果是探测期间持有者写记录会失败。也就是说，C 案要求所有加锁的地方都换成自己写的 FFI，std 那部分全部不能再用。
- 换一个第三方区间锁库（比如 `file-guard`）可以避免自己写 `unsafe`，但这类库在 Unix 上走 `fcntl` 记录锁。`fcntl` 锁有个经典问题：同一进程里关掉**任何一个**指向该文件的 fd，就会释放这个进程在该文件上的所有锁。同进程里的探测会把持有者的锁弄丢，单测里正好就会这样。所以只能在 Windows 上用这个库，Unix 继续用 std，平台分支还是躲不掉。

### 第四个方案：NTFS 备用数据流（考虑过，不推荐）

把记录写进 `.rencoder-owner:record` 这个备用数据流。Windows 的区间锁按数据流生效，锁住主流不影响读备用流。这样只需要纯 std，不用 `unsafe`，目录里看不到第二个文件，删主文件时备用流也会一起删掉。

不推荐的原因是：**exFAT 和 FAT32 不支持备用数据流**，而转码的目标目录放在外接 exFAT 盘上很常见。到了那种盘上又会退化成 `None`，等于把问题换成另一种形式。

### A 案：推荐

它是唯一一个"全平台带 pid、lib 里不加平台代码、不写 `unsafe`"的方案，和设计文档里"平台差异由标准库承担"的原则也一致。

## 四、A 案需要补的地方

**1. "Windows 上记录文件删不掉、会留垃圾"这个风险大概率被高估了。**
Rust std 在 Windows 上打开文件时，默认共享模式包含 `FILE_SHARE_DELETE`（读、写、删除三项都共享）。所以只要读记录的一方也是用 std 打开的（`fs::read_to_string` 就是），持有者照样能删掉记录文件。传统删除语义下，名字会以"待删除"状态多留一小会儿，等读的一方关掉句柄就消失。真正可能挡住删除的，只有不带删除共享模式去打开文件的第三方程序，比如杀毒软件或索引服务。
建议仿照 2.2 节写个小程序实测一下：一个句柄读，另一个句柄 `remove_file`，看结果。如果实测成立，A 案对比表里"可能留垃圾"那一格可以改掉。

**2. 时序和读写语义要写进 §3.11.1.1：**

```
acquire:  打开 .rencoder-owner → try_lock 独占 → 截断并写 .rencoder-owner.json
release:  删 .rencoder-owner.json → 删 .rencoder-owner 名字 → 放锁
probe:    共享锁失败 → 读 .rencoder-owner.json → 解析失败或文件不存在 → None
```

- 记录文件不在"先删名字、后放锁"这条不变式里，最先删掉它就行。
- `None` 从此只会在两个很窄的窗口里出现：一是刚抢到锁、还没写记录；二是已经删了记录、还没放锁。这正好对应 `Held(None)` 注释里"尽力而为"的本意，那条注释可以保留，但要说明 None 只出现在这两个窗口里。
- 写记录时**不必**用"先写临时文件再改名"。临时文件会多出第 5 个属于应用的文件名，而记录只有几十字节，一次 `write_all` 基本不会被读到写了一半的状态。万一读到半截，解析失败返回 `None` 就够了。
- **进程崩溃后的残留**：上一个持有者崩溃时两个文件都会留下来。下一次 acquire 时截断重写记录，就把旧记录顺手清掉了。窗口期里可能读到旧的 pid，这和注释里"可能还写着前一个持有者"是一致的。

**3. 顺带要检查的一个已有问题（不属于这次的范围，但 A 案的改动会碰到这段代码）。**
Windows 传统删除语义下，持有者删掉 `.rencoder-owner` 之后、关闭句柄之前，这个文件处于待删除状态。这期间新实例去 `open(create)` 会拿到 `ERROR_ACCESS_DENIED`。要确认 `acquire` 对这个错误的处理：它应该判定为"被占用"或者重试，而不是作为一个莫名其妙的 I/O 错误抛给用户。

**4. 改动范围**：和文档第四节一致。另外第二节说的那条"同进程第二句柄必须拿到 pid"的测试，也要加进去。

## 五、CI 的问题

- 建议 CI 的 `cargo test` 也加上 `--no-fail-fast`。附录 B 已经说明了，默认遇到第一个失败就停，后面的失败会被藏住。这次 A.1 和 A.2 能被找出来，靠的就是这个参数。
- Windows core job 从建立起就是红的却没人发现，说明它可能没被设成合并 PR 前必须通过的检查项。修好之后建议把它设成必过项。

---

**需要你拍板的有两件事：**

1. 是否接受 A 案带来的那个伴随文件，也就是采集器要跳过的文件名从 3 个变成 4 个。
2. 要不要先花几分钟跑一下第四节第 1 条的删除实测。如果结论成立，A 案就没有明显的短板了。

question 2: A方案是唯一可行的吗，如果我不想分立文件增加复杂性，保持单文件是否可行

discuss 2： 单文件可以做到，而且有一个比 C 案干净得多的做法：**在 Windows 上不用字节区间锁来做互斥，改用打开文件时的"共享模式"**。它只用 std 的安全 API，不需要 `unsafe`，也不加新依赖。不过要先说清一个前提：在单文件前提下，想让 Windows 报错带 pid，Windows 上总得有几行 `cfg(windows)` 的平台代码，区别只在于这几行是安全的 std 调用还是 FFI。

## 为什么"零平台代码 + 单文件 + 带 pid"做不到

std 的 `try_lock` 在 Windows 上一定锁整个文件，而 Windows 的区间锁是强制锁，没有参数可以调。所以只要 Windows 上还用 std 的锁，持锁期间别人就读不到记录。要保住单文件，就必须在 Windows 上换掉 std 的锁。

## 推荐做法：D 案，Windows 上用共享模式互斥

Windows 打开文件时可以声明"在我开着这个文件期间，允许别人对它做什么"（`share_mode`）。这个约束由内核强制执行，进程退出、句柄关闭后自动解除，效果上和锁一样。std 直接提供了这个接口：`std::os::windows::fs::OpenOptionsExt::share_mode`，是安全 API，Rust 1.10 起就是稳定版。

三类角色在 Windows 上可以这样对应：

| 角色 | 现在（std 锁） | D 案（Windows） | 
|---|---|---|
| 持有者 | 读写打开 + `try_lock` 独占锁 | **读写**打开，共享模式 = 读 + 删除（**不允许别人写**） |
| 探测者 | 只读打开 + `try_lock_shared` | **只读**打开，共享模式 = 读 + 删除（**不允许别人写**） |
| 读记录的人 | `fs::read_to_string` | 不变，std 默认共享模式是读 + 写 + 删除 |

逐一推演冲突关系：

- **第二个想抢占的进程**：它要申请写权限，但持有者不允许别人写，所以打开失败，返回 `ERROR_SHARING_VIOLATION`（错误码 32），判定为被占用。✅
- **探测者**：它不允许别人写，可持有者已经以写权限开着这个文件，两边冲突，打开失败，判定为 `Held`。这个打开仍然是**只读**的，符合设计文档"探测只读"的要求。✅
- **读记录的人**：它只申请读权限，持有者允许别人读；它自己的共享模式也包含写，容得下持有者已有的写权限。所以**能读到记录**。✅
- **两个探测者同时探测**：双方都只申请读、都不允许别人写，彼此兼容。✅
- **有探测者开着文件时来了个抢占者**：抢占者申请写权限被拒，判定为被占用。这和现在"共享锁挡住独占锁"的语义完全一致。✅
- **持有者删除自己的标记文件**：它的共享模式包含删除，所以删得掉，"先删名字、后放锁"的顺序照旧。

Unix 上完全不动，继续用 `flock`。

### 代价

- 大约十来行 `#[cfg(windows)]` 代码，集中在两处：打开标记文件的地方，以及错误映射（错误码 32 视为被占用）。
- 设计文档 §3.11.1.1 里"平台差异由标准库承担"这句要改成："Windows 上用共享模式而不是区间锁，因为区间锁是强制锁，会挡住读记录。"这样改仍然是标准库在承担平台差异，只是换了标准库的另一个接口。
- 布局、公开 API、采集器跳过的文件名清单都不变。

### 要注意的边角

- **待删除窗口**：持有者删掉名字之后、关闭句柄之前，别人再打开这个文件会得到 `ERROR_ACCESS_DENIED`（错误码 5）。这个错误也要映射成"被占用"。
- **第三方程序**：备份或索引类软件如果以"禁止写共享"的方式打开这个文件，会让我们的抢占误判为被占用。这和现在第三方去锁这个文件的风险是同一个量级，可以接受。
- **SMB 网络盘**：共享模式由服务器端强制执行，可以正常工作，但最好实测一次。

## 先跑实证再拍板

可以仿照文档 2.2 节写个小程序，在 Windows 上跑这五步（`share_mode` 的参数值：读 = `0x1`，写 = `0x2`，删除 = `0x4`）：

```rust
use std::os::windows::fs::OpenOptionsExt;
// 1. 持有者：read+write+create, share_mode(0x1|0x4)，写入 "pid=1234"
// 2. 第二个持有者：同样打开          → 期望 Err(raw_os_error == 32)
// 3. 探测者：read, share_mode(0x1|0x4) → 期望 Err(32)
// 4. fs::read_to_string(&p)           → 期望 Ok("pid=1234")
// 5. 持有者句柄 drop 后重复第 2 步     → 期望 Ok
```

五步都符合预期，D 案就成立。

## 单文件方案对比

| | D 案：共享模式 | C 案：稀疏区间锁 | 备用数据流 | B 案：收窄契约 |
|---|---|---|---|---|
| 单文件 | ✅ | ✅ | ✅ | ✅ |
| Windows 报错带 pid | ✅ | ✅ | ⚠️ exFAT/FAT32 盘上没有 | ❌ |
| `unsafe` / 新依赖 | 无 | 有 | 无 | 无 |
| 平台代码 | 约十行安全 std | FFI，而且所有加锁点都要换掉 | 一个路径后缀 | 无 |
| 互斥会不会静默失效 | 低 | 区间算错就会 | 低 | 低 |

**结论**：如果你想保持单文件，选 **D 案**，它比 A 案改动更小，也没有 A 案在 Windows 上删伴随文件的问题。A 案只剩一个优势：lib 里完全没有 `cfg(windows)`。所以要比较的是"一个伴随文件"和"十来行 Windows 专用的安全代码"哪个更能接受。我会选 D。

另外，上次说的那条测试不管选哪个方案都要补上：同一进程开第二个句柄去探测，必须拿到 `Held(Some)`，且 pid 等于本进程的 pid。

question 3: 实际上，这有可能是更通用的做法，而不仅限于windows, 我们只需要知道是否有锁，并不需要锁是独占的，这个防御策略来自应用，而不是os, 所以，理论上，任何锁都可以，甚至其他能够提供拥有保证的方式都可以，是这样吗

对了一大半，但有一处需要纠正：**锁不需要挡住读，但必须在抢占者之间互斥**。"只要知道有没有锁"如果理解成"先看一眼、没人再占"，就有竞态。

## 你说对的部分

- **互斥是应用的策略，不是 OS 的需求。** 所有实例都遵守同一套协议，所以建议锁（advisory）就够了。强制锁（mandatory）是多给的，Windows 的区间锁正是因为多给了"挡读"，才把记录挡住了。
- **机制可以换。** 只要能提供"占有"的保证，不一定非得是文件锁。
- **所以这个思路是通用的。** 它不该是"Windows 的特例"，而是一条设计原则：先定义原语要满足什么性质，再在每个平台上挑一个满足这些性质的实现。

## 需要纠正的部分

**1. 互斥要求本身不能省，只是互斥的对象要选对。**
两个实例同时启动时，如果流程是"检查有没有人占 → 没人 → 我占上"，两边可能都看到"没人"，然后都占上。所以抢占这一步必须是**原子的测试并设置**：内核保证两个抢占者里只有一个能成功。
真正要的是：**抢占者之间互斥，对读者完全不设防**。Unix 的 `flock` 正好是这样，Windows 的区间锁则是对谁都设防。

**2. 不是"任何锁"都行。** 比如共享锁也是锁，但两个进程可以同时拿到，所以给不了占有保证。

## 一个原语要满足的性质

| # | 性质 | 为什么需要 |
|---|---|---|
| 1 | **原子抢占**：同时抢只有一个能成功 | 避免两个实例都以为自己占上了 |
| 2 | **进程退出后自动释放**，包括崩溃和被 kill | 否则就要判断"旧主人还活着吗"，会遇到 pid 复用、跨机器无法判断等问题 |
| 3 | **能探测、不干扰**：不抢占也能知道有没有人占 | 对应 §3.11.1.1 的"探测只读" |
| 4 | **不挡读**：持有期间别人能读到记录 | 报错要带 pid |
| 5 | **按路径定位**：能在任意目录上探测 | 守卫要检查祖先目录和后代目录有没有被占 |

## 拿这几条去衡量各种候选

| 机制 | 1 原子 | 2 自动释放 | 3 可探测 | 4 不挡读 | 5 按路径 | 结论 |
|---|---|---|---|---|---|---|
| Unix `flock` | ✅ | ✅ | ✅ | ✅ | ✅ | Unix 上现在就满足 |
| Windows std 锁（区间锁） | ✅ | ✅ | ✅ | ❌ | ✅ | 这次的问题 |
| Windows 共享模式（D 案） | ✅ | ✅ | ✅ | ✅ | ✅ | Windows 上满足 |
| `O_CREAT\|O_EXCL` 或 `mkdir` | ✅ | ❌ | ✅ | ✅ | ✅ | 崩溃后会留下"死锁"，只能靠 pid 判断死活，不可靠 |
| 命名互斥体 / 抽象 socket / 端口 | ✅ | ✅ | ✅ | 不涉及 | ⚠️ | 作用范围是整台机器，要用路径哈希来命名；查后代目录得枚举所有名字，做不到 |
| 本地 IPC 服务（持有者监听，别人来问） | ✅ | ✅ | ✅ | ✅ | ⚠️ | 能直接问出 pid，但对一个守卫来说太重了 |

这张表说明了两件事：

- **"把标记文件放在目标目录里"这个选择本身是对的。** 第 5 条性质只有基于文件的方案能自然满足，祖先和后代目录的检查靠的就是它。
- **你想要的"通用做法"在两个平台上的实现不同**，因为 Unix 没有共享模式，Windows 的区间锁又会挡读。通用的是**抽象层**，不是某一个具体机制：

```
fn try_own(path) -> Result<OwnHandle, Occupied>   // 性质 1、2
fn probe(path)   -> Free | Held                   // 性质 3
// 记录：持有者通过 OwnHandle 写入，任何人用普通 open 读取   // 性质 4
// Unix 用 flock 实现；Windows 用共享模式实现
```

## 我的建议

把上面这五条性质写进 §3.11.1.1，作为守卫原语的**契约**，再加一句"Unix 用 `flock`，Windows 用共享模式"作为当前实现。这样有两个好处：

- 这次的 bug 会被描述成"Windows 的 std 锁违反了第 4 条"，原因一目了然。
- 以后再有人想换机制（比如改成 `O_EXCL`），可以直接对照这张表，看出它在第 2 条上不满足。

question 4: 所以，如果我现在需要一个更轻量，更简洁，也许还可以更统一的方案，至少满足单文件、带 pid/since, 如果平台代码更少是最好的 ，你是否能提供

我先写一个完整的实现草稿，并在 Unix 和 Windows 两个目标上做编译检查。

这里装不了 Windows 编译目标，所以我只能在 Linux 上编译并跑测试，Windows 那部分要你在本机验证。下面写草稿。

可以做到：单文件，记录里带 pid 和 since，不用 `unsafe`，不加依赖。完整草稿在 `owner_marker_sketch.rs` 里。Linux 上 5 条测试和 `clippy -D warnings` 都过了；Windows 部分我这里装不了编译目标，**还没编译过，也没跑过**，需要你在本机验证。

## 核心思路

两个平台的写记录、读记录、释放、解析代码全部共用，差异集中在一个 `sys` 模块里的三个函数：

| | Unix | Windows |
|---|---|---|
| `open_owned`（抢占） | 打开后调 std 的 `try_lock`（flock，不挡读） | 以写权限打开，共享模式只允许别人读和删 |
| `open_probe`（探测） | 只读打开后调 `try_lock_shared` | 只读打开，共享模式同样拒绝别人写 |
| `is_busy`（判定被占） | 错误类型是 `WouldBlock` | 系统错误码是 32 |

- **读记录**两个平台都直接用 `fs::read_to_string`，持有期间也读得到，这次失败的就是这一步。
- **释放**两个平台一样：Drop 时先删文件名，再关句柄（关句柄即放锁）。
- **Windows 平台代码约 40 行**，只用到 std 的 `share_mode` 接口和几个常量。

Windows 上用共享模式而不用 std 的锁，原因是 Unix 没有共享模式，而 std 的锁在 Windows 上是会挡读的强制锁。所以"完全一套代码"做不到，能做到的统一程度就是这样。

## 写草稿时发现的问题：Unix 上可能出现两个持有者

"先删名字、后放锁"这个释放顺序挡不住下面这种时序：

1. 新来的 B 打开了标记文件，拿到旧文件 X，还没加锁。
2. 当前持有者 A 删掉文件名，然后放锁。
3. B 加锁成功，但锁住的是一个已经没有名字的文件 X。
4. 再来一个 C，按同样的路径新建了文件 Y，也加锁成功。

结果 B 和 C 都认为自己占有这个目录。

草稿里的修法是：加锁成功后，检查这个路径现在指向的是不是自己锁住的那个文件（比较设备号和 inode 号），不是就重来。约十行代码，有对应的单测。

建议你查一下现有的 `guard.rs` 有没有做这个检查，没有的话这是一个**独立于 Windows 问题的真 bug**。Windows 上没有这个问题，因为抢占在打开文件那一步就原子完成了。

## 草稿里的其他处理

- **Windows 的待删除窗口**：前一个持有者删了名字、还没关句柄时，打开会返回错误码 5。草稿里短暂重试 5 次，每次间隔 20 毫秒；还是 5 就照实报错，不一律当成被占用，因为它也可能是真的没有权限。
- **崩溃残留**：留下的文件不会挡住下一个实例，新持有者会截断并重写记录。有对应测试。
- **测试用同一进程里的第二个句柄**去验"拒绝时带 pid"，两个平台上都会真的撞到占用，不需要起子进程。

## 在 Windows 上要做的事

1. 用你本机的命令跑一遍这 5 条测试：
   ```bash
   CARGO_BUILD_JOBS=1 CARGO_PROFILE_DEV_DEBUG=0 cargo test --no-fail-fast
   ```
   可以直接把草稿当一个小 crate 跑，不用先并进仓库。
2. 重点看 `record_is_readable_by_anyone_while_held` 和 `probe_sees_held_with_pid_then_free_after_release`：前者就是原来失败的那一步，后者验证释放后文件真的删掉了。
3. 都过了，再按草稿去改 `guard.rs`，并把上次说的五条原语性质写进 §3.11.1.1。


```rust
//! 单文件占用标记：互斥 + pid/since 记录，全平台持有期间可读。
//!
//! 原语契约（§3.11.1.1 建议写入）：
//!   1. 原子抢占：并发抢占只有一个成功
//!   2. 进程退出（含崩溃 / kill）自动释放
//!   3. 可探测、不干扰：不抢占也能知道是否被占
//!   4. 不挡读：持有期间任何人能读到记录
//!   5. 按路径定位：标记文件在目标目录里
//!
//! 实现：Unix 用 std 的 flock（建议锁，本来就不挡读）；
//!       Windows 不用区间锁（强制锁，会挡读），改用打开时的共享模式。
//! 两个平台只在 `sys` 里的三个函数上有差异，其余全部共用。

use std::fs::{self, File};
use std::io::{self, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Holder {
    pub pid: u32,
    /// Unix 秒。
    pub since: u64,
}

#[derive(Debug)]
pub enum Occupancy {
    Free,
    /// 记录仍是尽力而为：只在「刚抢到、还没写」和「已删名、未放锁」
    /// 两个窄窗口里为 None——不再是 Windows 上的常态。
    Held(Option<Holder>),
}

#[derive(Debug)]
pub enum AcquireError {
    Held(Option<Holder>),
    Io(io::Error),
}

/// 持有期间标记文件存在；Drop 时先删名字、后放锁。
#[derive(Debug)]
pub struct OwnerMarker {
    // 字段在 drop() 函数体之后才释放 ⇒ 先删名字、后放锁。
    _file: File, // 只为持有：句柄在即锁在
    path: PathBuf,
}

impl OwnerMarker {
    pub fn acquire(path: &Path) -> Result<Self, AcquireError> {
        let mut file = match sys::open_owned(path) {
            Ok(f) => f,
            Err(e) if sys::is_busy(&e) => return Err(AcquireError::Held(read_holder(path))),
            Err(e) => return Err(AcquireError::Io(e)),
        };
        let record = Holder {
            pid: std::process::id(),
            since: now_secs(),
        };
        // 尽力而为：写失败不影响互斥本身。
        let _ = write_holder(&mut file, &record);
        Ok(Self {
            _file: file,
            path: path.to_owned(),
        })
    }
}

impl Drop for OwnerMarker {
    fn drop(&mut self) {
        if let Err(e) = fs::remove_file(&self.path) {
            eprintln!("warning: failed to remove {}: {e}", self.path.display());
        }
        // 函数体结束后 self.file 才释放 ⇒ 放锁 / 关闭句柄。
    }
}

/// 只读探测，不创建文件。
pub fn probe(path: &Path) -> io::Result<Occupancy> {
    match sys::open_probe(path) {
        Ok(_f) => Ok(Occupancy::Free), // _f 立即释放
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Occupancy::Free),
        Err(e) if sys::is_busy(&e) => Ok(Occupancy::Held(read_holder(path))),
        Err(e) => Err(e),
    }
}

/// 任何人都能调用：普通 open 读。两个平台都不会被持有者挡住。
pub fn read_holder(path: &Path) -> Option<Holder> {
    parse_holder(&fs::read_to_string(path).ok()?)
}

fn write_holder(file: &mut File, h: &Holder) -> io::Result<()> {
    file.set_len(0)?;
    file.seek(SeekFrom::Start(0))?;
    // 几十字节一次写完；读到半截时 parse 失败 ⇒ None，可接受。
    file.write_all(format!("pid={}\nsince={}\n", h.pid, h.since).as_bytes())
}

fn parse_holder(s: &str) -> Option<Holder> {
    let mut pid = None;
    let mut since = None;
    for line in s.lines() {
        match line.split_once('=')? {
            ("pid", v) => pid = v.parse().ok(),
            ("since", v) => since = v.parse().ok(),
            _ => {}
        }
    }
    Some(Holder {
        pid: pid?,
        since: since?,
    })
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

// ───────────────────────── 平台差异只在这里 ─────────────────────────

#[cfg(unix)]
mod sys {
    use std::fs::{self, File, OpenOptions, TryLockError};
    use std::io;
    use std::os::unix::fs::MetadataExt;
    use std::path::Path;

    pub fn open_owned(path: &Path) -> io::Result<File> {
        loop {
            let f = OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .open(path)?;
            to_io(f.try_lock())?;
            // flock 锁的是 inode。若在 open 与 try_lock 之间前一个持有者
            // 删了名字并放锁，我们锁住的是一个已脱链的 inode，而后来者会在
            // 同名新 inode 上也锁成功 ⇒ 两个持有者。核对名字仍指向我们锁住的
            // 那个 inode，不是就重来。
            if still_named(&f, path)? {
                return Ok(f);
            }
        }
    }

    pub fn open_probe(path: &Path) -> io::Result<File> {
        let f = File::open(path)?;
        to_io(f.try_lock_shared())?;
        Ok(f)
    }

    pub fn is_busy(e: &io::Error) -> bool {
        e.kind() == io::ErrorKind::WouldBlock
    }

    pub(crate) fn still_named(f: &File, path: &Path) -> io::Result<bool> {
        let mine = f.metadata()?;
        match fs::metadata(path) {
            Ok(m) => Ok(m.dev() == mine.dev() && m.ino() == mine.ino()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(e),
        }
    }

    fn to_io(r: Result<(), TryLockError>) -> io::Result<()> {
        match r {
            Ok(()) => Ok(()),
            Err(TryLockError::WouldBlock) => Err(io::ErrorKind::WouldBlock.into()),
            Err(TryLockError::Error(e)) => Err(e),
        }
    }
}

#[cfg(windows)]
mod sys {
    //! 互斥靠「打开即抢占」：持有者以写权限打开并拒绝别人写；
    //! 探测者以只读打开并拒绝别人写（与持有者的写权限冲突 ⇒ 32）；
    //! 读记录的人用 std 默认共享模式（读+写+删）⇒ 与持有者兼容 ⇒ 读得到。
    //! 句柄由内核在进程退出时关闭 ⇒ 崩溃自动释放。
    //! 抢占在 open 这一步原子完成，没有 Unix 那种「锁住脱链 inode」的窗口。
    use std::fs::{File, OpenOptions};
    use std::io;
    use std::os::windows::fs::OpenOptionsExt;
    use std::path::Path;
    use std::thread::sleep;
    use std::time::Duration;

    const FILE_SHARE_READ: u32 = 0x1;
    const FILE_SHARE_DELETE: u32 = 0x4;
    /// 允许别人读、允许持有者删自己；拒绝任何其他写者。
    const DENY_WRITE: u32 = FILE_SHARE_READ | FILE_SHARE_DELETE;
    const ERROR_ACCESS_DENIED: i32 = 5;
    const ERROR_SHARING_VIOLATION: i32 = 32;

    pub fn open_owned(path: &Path) -> io::Result<File> {
        retry_delete_pending(|| {
            OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .share_mode(DENY_WRITE)
                .open(path)
        })
    }

    pub fn open_probe(path: &Path) -> io::Result<File> {
        retry_delete_pending(|| {
            OpenOptions::new()
                .read(true)
                .share_mode(DENY_WRITE)
                .open(path)
        })
    }

    pub fn is_busy(e: &io::Error) -> bool {
        e.raw_os_error() == Some(ERROR_SHARING_VIOLATION)
    }

    /// 前一个持有者「已删名、未关句柄」时文件处于 delete-pending，
    /// 此时 open 返回 5。窗口极短，短暂重试；仍是 5 就照实报错
    /// （可能是真的没权限，不能一律当成被占用）。
    fn retry_delete_pending(mut f: impl FnMut() -> io::Result<File>) -> io::Result<File> {
        for _ in 0..5 {
            match f() {
                Err(e) if e.raw_os_error() == Some(ERROR_ACCESS_DENIED) => {
                    sleep(Duration::from_millis(20))
                }
                r => return r,
            }
        }
        f()
    }
}

// ───────────────────────────── 测试 ─────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("owner-sketch-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d.join(".rencoder-owner")
    }

    #[test]
    fn record_is_readable_by_anyone_while_held() {
        let p = tmp("readable");
        let _g = OwnerMarker::acquire(&p).expect("free");
        // 新句柄读 —— 原来在 Windows 上失败的那一步
        let h = read_holder(&p).expect("record must be readable while held");
        assert_eq!(h.pid, std::process::id());
    }

    #[test]
    fn second_taker_is_refused_and_sees_the_pid() {
        // 同进程第二句柄：Windows 共享模式按句柄、Unix flock 按打开文件描述，
        // 两边都会真的撞上 ⇒ 不用起子进程就能验「报错带 pid」。
        let p = tmp("second");
        let _g = OwnerMarker::acquire(&p).expect("free");
        match OwnerMarker::acquire(&p) {
            Err(AcquireError::Held(Some(h))) => assert_eq!(h.pid, std::process::id()),
            other => panic!("expected Held(Some), got {other:?}"),
        }
    }

    #[test]
    fn probe_sees_held_with_pid_then_free_after_release() {
        let p = tmp("probe");
        assert!(matches!(probe(&p).unwrap(), Occupancy::Free));
        let g = OwnerMarker::acquire(&p).expect("free");
        match probe(&p).unwrap() {
            Occupancy::Held(Some(h)) => assert_eq!(h.pid, std::process::id()),
            other => panic!("expected Held(Some), got {other:?}"),
        }
        drop(g);
        assert!(!p.exists(), "marker must be removed on release");
        assert!(matches!(probe(&p).unwrap(), Occupancy::Free));
        let _again = OwnerMarker::acquire(&p).expect("free again after release");
    }

    #[test]
    fn crash_leftover_is_reclaimed() {
        // 模拟崩溃残留：文件在、没人持有、记录是旧的
        let p = tmp("leftover");
        fs::write(&p, "pid=1\nsince=0\n").unwrap();
        assert!(matches!(probe(&p).unwrap(), Occupancy::Free));
        let _g = OwnerMarker::acquire(&p).expect("leftover must not block");
        assert_eq!(read_holder(&p).unwrap().pid, std::process::id());
    }

    #[cfg(unix)]
    #[test]
    fn unlinked_inode_is_detected() {
        let p = tmp("unlinked");
        let f = File::create(&p).unwrap();
        fs::remove_file(&p).unwrap();
        assert!(!sys::still_named(&f, &p).unwrap());
        fs::write(&p, "").unwrap(); // 同名新 inode
        assert!(!sys::still_named(&f, &p).unwrap());
    }
}
```
