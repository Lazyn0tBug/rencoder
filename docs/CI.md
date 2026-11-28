# CI 问题集

这一页是**故障档案**：本仓库的 CI 与发布工作流踩过的坑，每一条写清
**现象 → 原因 → 修法 → 下次怎么早发现**。

它不是设计文档（那是 `docs/design.md`），也不是"怎么在别的机器上跑测试"
（那是 `docs/TESTING.md`）。这一页只回答一件事：**CI 红了、卡住了、或者
"绿了但我不信"，先看哪一条**。

条目里的编号（CI-1、CI-2……）是稳定的：往后翻这页时直接说编号。

---

## 元规则：四条比条目更重要的

**1. 不会红的闸门不是闸门。** 本仓库有过三个**从来没对代码说过一句话**的
闸门：Windows 的门死在 shell 语法上（CI-1，它红着，但红的原因和代码无
关）、macOS 的 job 标签匹配不到任何镜像、于是永远排队（CI-2，它既不绿
也不红）、查 libav 的循环匹配了一个不存在的目录（CI-9，它是**绿的**——
空转通过）。一个跑不起来的检查比没有检查更糟：它给出的是虚假的安心。
**改闸门的时候，先问"它坏了会红吗"**——把标签写错、把路径写错、把 shell
写错，得到的都是这样的闸门。

**2. 两秒就红 = 环境问题；永远排队 = 标签问题。** job 失败得快，通常不是
代码的问题，是它根本没开始跑（CI-1）。job 一直 queued 也不是"队列忙"——
**标签匹配不到镜像的 job 会永远排队，而不是失败**（CI-2）。

**3. 绿灯也要看它是不是真跑了。** job 状态是 success，但 `runner_name` 是
空的 = 没人接（被取消，或者从没被调度）。查：

```bash
gh run view <run-id> --json jobs \
  --jq '.jobs[] | "\(.name) \(.status)/\(.conclusion) runner=\(.runner_name)"'
```

**4. 发布构建比 CI 多干一件事。** `dist build` 编的是**工作区里的每一个
包**（包括 GUI），而 CI 的 `core` 那趟只编 lib + CLI。所以发布的依赖清单
必须**比 CI 更全**，不是照抄 CI 的（CI-4）。反过来，CI 里有的系统依赖，
发布里也必须一模一样地装一遍。

---

## 症状速查

| 你看到的 | 多半是 | 条目 |
|---|---|---|
| job 两秒就红，日志里一行 cargo 都没有 | shell 不是 bash | CI-1 |
| 一个 job 一直 queued，别的都跑完了 | runner 标签匹配不到镜像 | CI-2 |
| `You can't enable both async-std & tokio features at once` | `rfd` 的默认 feature | CI-3 |
| `Package gdk-3.0 was not found in the pkg-config search path` | 发布 job 少装了 webview 栈 | CI-4 |
| `expected exactly one top-level directory, found []` | cargo-dist 的 zip 是平的 | CI-5 |
| `sha256sum -c` 说对不上 | 校验和描述的是加料**之前**的字节 | CI-7 |
| `dist: command not found`，可 job 是绿的 | 安装器管道给 `sh` 是静默空跑 | CI-8 |
| 标记"从来没出现过"，可日志里明明有 | 日志不是 UTF-8 / ANSI 转义 | CI-13、CI-14 |
| 退出码 130 但**记录是空的** | 请求落在了文件内部 | CI-11、CI-12 |
| Windows 上半成品删不掉 | `ffmpeg.exe` 是 shim / 文件被占着 | CI-15、CI-16 |
| 同一个包在发布页上出现两次，或者 `sha256sum -c` 说压缩包不存在 | 发布的格式集合在几处分叉了 | CI-19 |

---

## 逐条

### CI-1 Windows 的门从来没跑过一行测试

**现象**：两个 Windows job 秒红，日志里是

```
Set-Variable: A parameter cannot be found that matches parameter name 'euo'.
##[error]Process completed with exit code 1.
```

**原因**：Windows runner 的默认 shell 是 **PowerShell**，而闸门脚本第一行是
`set -euo pipefail`——那是 bash 的语法。脚本死在第一行，`cargo` 从来没被
调用过。后果不只是"两个 job 是红的"：**`#[cfg(windows)]` 的分支从来没被
编译过**（Windows 的控制台事件处理器、`keys.rs` 的 `ReadConsoleInput`、
新加的作业对象代码……）。

**修法**：给 step 加 `shell: bash`（runner 上有 Git Bash），三个平台跑同一份
脚本。`build.yml` 一直这么写，只有 `ci.yml` 漏了。

**下次怎么早发现**：job 的第一条日志是 `##[group]Run set -euo pipefail`
而不是 cargo 的输出 → 门没跑起来。**任何"两秒就红"的 job 都先看它到底
执行了几条命令**。

---

### CI-2 标签匹配不到镜像 → 永远排队，不失败

**现象**：某个 job 排队几十分钟到几小时，别的 job 都跑完了，它还在
`queued`。

**原因**：runner 标签写错了。GitHub 托管 runner 的 macOS 标签是：

| 镜像 | 架构 | 标签 |
|---|---|---|
| macOS 26 Arm64 | arm64 | `macos-latest`、`macos-26`、`macos-26-xlarge` |
| macOS 26 | x64 | `macos-latest-large`、`macos-26-intel`、`macos-26-large` |
| macOS 15 Arm64 | arm64 | `macos-15`、`macos-15-xlarge` |
| macOS 15 | x64 | `macos-15-large`、`macos-15-intel` |

也就是说：**`macos-15` 是 arm64**，x64 要写 `-intel` / `-large`；而
`macos-14` 已经退役、`macos-18` 从来没存在过（苹果从 15 直接跳到 26）。
**标签匹配不到任何镜像的 job 不会失败，它会一直排队**——所以错标签看起来
像"队列忙"。

**修法**：所有 macOS job 用 `macos-15`（arm64，本仓库是 public 仓库）。

**下次怎么早发现**：`runner_name` 是空的 → 没人接它，不是排队慢。
另外：**一个新加的 job 要先看它有没有真的跑过一次**，再相信它的绿灯。

---

### CI-3 GUI 从来没在 Linux 上编过

**现象**：`ui (ubuntu)` job 失败在

```
error: You can't enable both async-std & tokio features at once
error: could not compile `ashpd` (lib)
```

**原因**：`ashpd`（Linux 上的 portal 客户端）不能同时开 async-std 和
tokio。而**我们自己**的依赖声明 `rfd = "0.14"` 开了 rfd 的默认 feature
（`["xdg-portal", "async-std"]`），Dioxus 那一侧却是 tokio。feature 是全
图合并的，两边打架。

**修法**：照抄 `dioxus-desktop` 自己的声明：

```toml
rfd = { version = "0.14", default-features = false, features = ["xdg-portal", "tokio"] }
```

**下次怎么早发现**：Cargo feature 冲突只在**真编**的时候才炸。本仓库把
GUI 拆成单独的 job，就是为了让它每趟都编一遍——但那一趟自己红着，就等于
没有。

---

### CI-4 发布的依赖清单必须比 CI 更全

**现象**：Linux 发布构建失败在

```
Package gdk-3.0 was not found in the pkg-config search path.
```

**原因**：`dist build` 编的是**每一个**包，其中一个是 Dioxus 桌面端：Linux
上它链 webkit2gtk → gtk3 → gdk-3.0。CI 的 `ui` job 装了这套系统包，
**发布 job 没装**。

**修法**：发布 job 装和 CI 的 `ui` job **一模一样**的清单：

```
libwebkit2gtk-4.0-dev libwebkit2gtk-4.1-dev libxdo-dev
libayatana-appindicator3-dev librsvg2-dev
```

（`libwebkit2gtk-4.0-dev` 会带进 `libgtk-3-dev`，也就带进 `gdk-3.0.pc`。）

**下次怎么早发现**：改依赖时，问一句"发布那趟要装什么"——发布是 CI 的
**超集**，不是它的子集。

---

### CI-5 cargo-dist 的 Windows zip 是**平**的

**现象**：Windows 发布包失败在

```
expected exactly one top-level directory, found []
```

**原因**：cargo-dist 给 Windows 打的 zip 把文件放在**压缩包根目录**（没有
`<pkg>-<triple>/` 那一层），而 Unix 的 tarball 是带那一层的。`scripts/
augment_zip.py` 要求"恰好一个顶层目录"，于是直接退出——Windows 的包一个
都没打出来。

**修法**：两种布局都处理——平的就**建一个**顶层目录（按压缩包名命名，和
tarball 一个规矩），Windows 写的 zip 可能带**反斜杠**分隔符，也一并归一成
zip 规范要的正斜杠。

**下次怎么早发现**：`dist build` 的输出里列的是压缩包**内容**，看不出目录
前缀；想知道真实布局，看 `python3 -c "import zipfile;print(zipfile.ZipFile(p).namelist())"`。

---

### CI-6 tar 那条路以前会**静悄悄**跳过加料

**现象**：没有现象——这正是问题。

**原因**：`scripts/dist.sh` 的 tar 分支遇到"里面没有 `rencoder_*` 目录"
时，是**警告后跳过**。于是包照发，只是里面没有 `rencoder.toml`、没有
`log/`、没有 `data/`——用户拿到的东西和文档承诺的不一样，而且**没有任何
地方报错**。

**修法**：和 CI-5 一样，平布局就建目录，不再跳过。

**下次怎么早发现**：发布之前把产物解开看一眼（见下面的检查单）。**跳过型
的兜底比硬失败更危险**——它把错误搬到了用户那边。

---

### CI-7 校验和描述的是加料**之前**的字节

**现象**：发布 job 的 `sha256sum -c` 说对不上。

**原因**：cargo-dist 在加料（rencoder.toml / log/ / data/）**之前**就算好了
校验和。加料改了压缩包，校验和就过期了。

**修法**：`scripts/dist.sh` 在加料**之后**重算 `*.sha256` 和 `sha256.sum`
（格式是 `<sum>  <裸文件名>`，和 `sha256sum -c` 对得上 ✅）。

---

### CI-8 cargo-dist 的安装器管道给 `sh` 是**静默空跑**

**现象**：`dist` 不存在，可那一步是绿的。

**原因**：安装器的下载 URL 是 302 跳转。`curl ... | sh` 里，`curl` 把**空的
跳转响应体**存了下来，`sh` 读一个空文件 → 退出 0 → 什么都没装。

**修法**：下载到文件，检查非空，再当**文件**跑：

```bash
curl --proto '=https' --tlsv1.2 -sSfL -o "$installer" <url>
test -s "$installer" || { echo "::error::empty installer"; exit 1; }
sh "$installer"
```

**下次怎么早发现**：装完跑一次 `dist --version`——没有这一步，"装好了"
只是假设。

---

### CI-9 查 libav 的循环匹配了一个不存在的目录

**现象**：没有现象——检查**空转**通过。

**原因**：cargo-dist 把产物放在 `target/<triple>/dist/`，而那段脚本 glob 的是
`target/<triple>/release/`（那个目录根本不存在）→ 循环一次都没跑 → 永远
"通过"。

**修法**：glob 改成 `target/${{ matrix.target }}/dist/rencoder*`。

**下次怎么早发现**：**给检查本身加一个能失败的输入**——故意让它匹配一个
已知会存在的文件，确认它真的报得出来。

---

### CI-10 缓存键少了架构

**现象**：两个架构共用一份 cargo 缓存（或者反过来，缓存一直不命中）。

**原因**：`actions/cache` 的 key 里只有 `runner.os`，没有 `runner.arch`。
macOS 从 x64 换成 arm64 之后，两边共用一份缓存。

**修法**：key 里加 `${{ runner.arch }}`。

---

### CI-11 取消请求是**全局**通道，不是这一趟的

**现象**：两个并发跑的测试互相取消；退出码 130（取消确实生效），但**记录是
空的**。

**原因**：取消请求 = "操作目录里 `cancel` 这个文件存在"（设计文档 §3.11.2）。
两个测试把 `RENENCODER_CANCEL_FILE` 指向同一个路径 → 一个的请求把另一个
的批也停了。

**修法**：**每个被中断的批带自己的取消文件**（放在各自的夹具目录里）。

**下次怎么早发现**：设计文档里凡是"谁能取消一个正在跑的批"的规则，都要
问一句"这份状态是**这一趟**的，还是**这个目录**的"。

---

### CI-12 断言写在了产品**不保证**的前提上

这一类占了本仓库 e2e 失败的绝大多数，四条同源：

| 断言 | 产品实际保证的 |
|---|---|
| "这个 ffmpeg 有 AV1 编码器" | "**列出了**" ≠ "**能用**"（CI 的 ffmpeg 列了 `av1_nvenc`，机器却没有 GPU） |
| "第二个文件是 b-long.mp4" | 采集顺序是**文件系统**的 readdir 顺序，不是文件名顺序 |
| "`/tmp` 是绝对路径" | Windows 上 `/tmp` 既不绝对也不存在 |
| "请求落在第一个文件里" | 契约是"**批在文件边界停下 + 已完成的文件照样落盘**"，落点本身是竞态 |

**修法**：前提要写成**产品保证的那句话**。比如"能选出编码器"（= 硬件可用
**或**软件编码器存在），而不是"列出了编码器"；比如"批开始了第二个任务"
（`task_idx=2`），而不是"第二个文件的名字出现了"。断言的前提一旦写成
产品不管的事，它就是一次竞态。

**下次怎么早发现**：写断言时问一句：**这句话是契约，还是我对实现的猜测？**

---

### CI-13 日志不能按 UTF-8 读

**现象**：等一个标记，等了 120 秒，报"标记从来没出现过"——可日志尾巴里
明明有它。

**原因**：夹具用 `read_to_string` 读子进程的 stderr。ffmpeg 的输出**不保证
是 UTF-8**，一个坏字节就让整次读取失败，而 `unwrap_or_default()` 把失败
变成了"空文件" → 看起来像时序问题。

**修法**：按**字节**读（`std::fs::read`），标记用字节比较；打印给人看的
时候再 `from_utf8_lossy`。

**下次怎么早发现**：失败消息里**必须带证据**（日志尾巴）——CI-13 就是因为
有了尾巴才看出"标记其实在"。

---

### CI-14 ANSI 转义让标记不是子串

**现象**：标记写的是 `task_idx=2`，日志里明明显示 `task_idx=2`，但按字节
找不到。

**原因**：`tracing` 把字段名和值**分别**包在 ANSI 转义里，原始字节是
`task_idx<ESC>[0m<ESC>[2m=<ESC>[0m2`——终端里看着是 `task_idx=2`，字节上
不是它的子串。

**修法**：标记用**消息正文**（`task start`），不要用字段的渲染结果。

---

### CI-15 Windows 上 `ffmpeg.exe` 是个 **shim**

**现象**：取消之后半成品删不掉（`the partial output must be removed`）。

**原因**：Windows runner 上 PATH 里的 `ffmpeg.exe` 是 Chocolatey 的
**shim**——它把真正的 ffmpeg 当成**自己的子进程**跑。杀掉 shim 只留下真
编码器继续写那个输出，而 Windows **删不掉任何进程打开着的文件** → 半成品
留在盘上。

**修法**：取消要结束**整棵进程树**：`platform::process::ProcessTree`
（Windows 用**作业对象**，且子进程要**挂起创建**——先入作业再恢复，否则
机器忙的时候它能先把自己的子进程生出来、跑在作业外面；Unix 的 shim 会
`exec`，一个进程就是全部，`Child::kill` 就够）。

**下次怎么早发现**：契约是"取消之后**没有进程还占着输出文件**"——写实现
时按这句话想，而不是按"杀掉那个我启动的进程"。

---

### CI-16 Windows 上**开着的文件删不掉**

**现象**：同上，但子进程确实死了，文件还是删不掉。

**原因**：Windows 的删除语义：任何进程打开着的文件都不能删。写它的进程
刚死，但扫描器 / 索引器可能还短暂地握着它。

**修法**：删除**重试**一小段时间（有上限：批已经停了，这只拖慢退出；窗口
过了还握着就是失败，报出来，不无限重试）。

---

### CI-17 退出码散在六处

**现象**：不是 CI 现象，但同源：**一条规则散在几个地方，每个地方自己解释
一遍**。

**原因**：CLI 有六处"停下来"的出口，其中四处**写死 130**，绕过了策略函数。
今天写死也对（主动退出键只在批次进行中才被读，那几处拿不到"主动退出"这个
理由）——但那是巧合。

**修法**：一个 `exit_cancelled(slot)` 决定所有停止的退出码；再加一个盯着
源码的测试（CLI 里不许再出现字面退出码）。**规则住在原语里**（设计文档
§3.10.4）。

---

### CI-18 发版的时候 CI 可能是红的

**现象**：发布出去的东西，测试其实是红的。

**原因**：发布工作流**不跑测试**（它只构建和打包）。tag 推上去的时候，CI
可能还在跑、或者已经红了。

**修法**：发布工作流的 `verify` job 要求**这个 commit 上的 CI 是绿的**，
是"pending"就等（最多 20 分钟）。红或被取消 → 停下，先修 CI，再用同一个
tag 手动 dispatch 发布。

**下次怎么早发现**：推 tag 之前先看一眼 CI 那一趟的状态。

---

### CI-19 通配符匹配到的比你以为的多

**现象**：同一个包在发布页上出现了**两次**；或者 dmg 是从"随便哪一份"里
做出来的；或者发布那一步在 `sha256sum -c` 上失败，说某个压缩包"不存在"。

**原因**：**发布的格式集合必须在**一处**决定** ✅ —— 生产者（cargo-dist）
产出什么 ✅、`scripts/dist.sh` 加料和算校验和算哪些 ✅、工作流上传哪些 ✅、
release job 验哪些 ✅，这四处必须说的是同一批文件 ✅ ✅。任何一处用
"格式家族"而不是"具体格式"来写通配符 ✅ ✅，集合就会分叉 ✅：

- 上传写 `dist/*.tar.*` ✅ 而生产者只出一种格式 ✅ → 校验和文件里躺着
  一个**没人上传**的压缩包的校验和 ✅ → release job 自己的
  `sha256sum -c` 在 `set -euo pipefail` 下直接失败 ✅ → **发不出去** ✅；
- 生产者换了格式 ✅ 而脚本还按老的算 ✅ → 同样分叉 ✅；
- dmg 那一步写 `ls ...tar.* | head -1` ✅ → 取哪一个取决于排序 ✅，不是取
  你要发的那一份 ✅。

**事实**：cargo-dist 0.32 给包产出的是 **`.tar.xz`** ✅，`.tar.gz` 只有源码
包 ✅（不发布 ✅）。所以发布的集合就是 `.tar.xz` + Windows 的 `.zip` ✅。

**修法**：生产侧和上传侧用**同一个**具体格式 ✅ —— `scripts/dist.sh` 只处理
`.tar.xz` ✅（加料 ✅、算校验和 ✅、解开 ✅ 都只认它 ✅），工作流上传
`dist/*.tar.xz` ✅，dmg 那一步写死扩展名 ✅ ✅，三处注释也写明同一个 ✅。
**注释和通配符不一致 ✅，下一个人会照注释改回去** ✅。

**下次怎么早发现**：发版之前看一眼资产列表 ✅，以及**校验和文件和解压包
对得上** ✅：

```bash
gh release view <tag> --json assets --jq '.assets[].name'
# 本地：校验和文件提到的每个文件都存在
cd dist && for f in *.sha256; do sha256sum -c "$f"; done
```

同一个包出现两次 ✅，或者某个 `.sha256` 指着一个不存在的压缩包 ✅，就是
集合分叉了 ✅。

---

## 发版前检查单

产物从哪来：本地跑一次 `bash scripts/dist.sh --target <triple>`（它会生成
`dist/`），或者从一次发布运行的 artifact 里下载
（`gh run download <run-id> -n artifacts-<triple>`）。**新克隆的仓库里没有
`dist/`** —— 下面第 2、3 步要的是产物，不是源码。

```bash
# 1. CI 在这个 commit 上是绿的
gh run list --workflow ci.yml --commit "$(git rev-parse HEAD)" \
  --json status,conclusion --jq '.[0]'

# 2. 解开一个产物，看它是不是"一个 app 目录"
tar -tzf dist/rencoder_cli-*.tar.xz | head -20
python3 -c "import zipfile,sys;print(zipfile.ZipFile(sys.argv[1]).namelist())" \
  dist/rencoder_cli-*.zip

# 3. 校验和描述的是加料之后的字节
cd dist && sha256sum -c *.sha256
```

三条都过了再推 tag。**发布是外向的动作**——发出去的包撤不回来。

---

## 维护

新踩一个坑就往这里加一条：编号往后排（下一条是 **CI-20**），**别改旧编号**
（别的文档和提交信息里引用了它们）。同一条有多个症状就都写进"症状速查"
那张表。
