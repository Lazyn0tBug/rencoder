# Rencoder – Rust 重建设计文档

**版本**：v5.2
**状态**：待确认
**变更基础**：`encode.zsh` v11.0.0（含 v10.2.0 帧率判定修复）+ `video_fps_stats.zsh` v2.3

---

## 1. 项目概述

Rencoder 是一个智能视频转码与分析工具，支持：

- 多种目标格式（AV1、H.264、H.265、VP9）
- CRF 或固定码率控制
- **最大帧率限制**（源帧率超标时自动降帧；判定依据为瞬时帧率上限 `r_frame_rate`）
- **一次 `ffprobe` 同时探测 codec 与帧率**（零外部进程解析）
- **编码器缓存**（运行期间只执行一次 `ffmpeg -encoders`）
- **NVIDIA 硬件加速**（`--nv` / 配置 `nv` 主动开启 + 能力实测，可用则用、不用则降级并告警）
- 单文件或批量目录处理（递归、扩展名过滤）
- 交互式配置（向导模式）
- 信号中断自动清理不完整输出文件
- 透传任意 FFmpeg 原生参数
- **视频分析模块**（帧率分布统计、可选并行、详细输出）

本项目采用 **CLI + Lib 双层架构**：

- **核心库（`rencoder_lib`）**：包含所有业务逻辑，不依赖任何前端交互。
- **CLI 前端（`rencoder_cli`）**：基于核心库构建，提供命令行接口。

### 1.1 外部工具约束

**除 `ffmpeg`/`ffprobe` 之外，不依赖任何外部命令行工具。**

| 工具 | 用途 | 状态 |
|---|---|---|
| `ffmpeg` | 转码 | ✅ 必需 |
| `ffprobe` | 元数据探测 | ✅ 必需 |
| ~~`nvidia-smi`~~ | ~~GPU 检测~~ | ❌ 不再需要：NVENC 能力改用 ffmpeg 的 1 帧试编码实测（§2.2） |
| ~~`jq`~~ | ~~读取配置~~ | ❌ 用 `serde_json` 替代 |
| ~~`fd`~~ | ~~文件遍历~~ | ❌ 用 `walkdir` 替代 |
| ~~`xargs`~~ | ~~并行~~ | ❌ 用 `rayon` 替代 |
| ~~`grep`/`cut`/`awk`~~ | ~~解析~~ | ❌ 用 Rust 字符串操作替代 |
| ~~`nproc`~~ | ~~CPU 核心数~~ | ❌ 用 `num_cpus` crate 替代 |

---

### 1.2 部署模型：单目录应用（U18）

Rencoder 是一个独立的第三方应用，不是发布到 crates.io 的通用库。交付与运行时都
采用**单目录应用（single-directory app）** 模式：

- **发布形态**：target 平台一份自包含目录，包含 binary + 配置 + 运行时资源（log /
  data 占位）。用户解压即用，不依赖系统路径、XDG 约定或 CWD。
- **运行时约束**：binary 自己 resolve 出父目录（`current_exe().parent()`）作为
  应用目录。**应用级**状态（config / log / data / 机器能力记录）从应用目录派生；
  **运行级**状态（取消请求、运行能力检查报告）落在**操作目录**（§3.6）。CWD 只承载
  用户输入的视频文件，与应用状态无关。
- **Source 仍是 workspace**：`rencoder_lib` 完成核心工作（业务逻辑、配置加载、
  ffmpeg 命令构造），`rencoder_cli`（及未来的 `rencoder_ui`，见 §3.8）
  是 thin wrapper。
- **Lib 内部约束**：`rencoder_lib` 加 `publish = false`，明确"不要 `cargo publish`"。

详见 §3.4.5 "App-level config + single-directory app" 和 §3.6 "部署布局"。

---

### 1.3 跨平台代码封装约定（U18）

涉及平台特定行为的代码（file mode bits 等）遵循"**定义处条件编译，使用处统一接口**"原则：

- **cfg 只出现在定义处**——写出平台特定逻辑的函数内部。
- **调用处只看到 uniform signature**——一个名字、一个签名，跨平台一致。
- **当前实现：Unix only**——我们目前只在 macOS 上构建和测试；Linux 路径
  跟 macOS 共享 `#[cfg(unix)]` 分支，覆盖到时一并验证。非 Unix 平台的
  等价行为**尚未实现**（future work）。
- **新增平台行为只改一个文件**——后续要落非 Unix 平台特定特性时，只改
  定义处的实现，调用方零改动。

#### 当前实现（Unix，macOS 已验证）

| 操作 | Unix (macOS / Linux) | 非 Unix | 模块 |
|------|----------------------|---------|------|
| config 文件 mode bits | `0o644` | no-op（TBD future work） | `rencoder_lib::config::fs_perms::set_config_file_mode` |
| log / data 目录 mode bits | `0o755` | no-op（TBD future work） | `rencoder_lib::config::fs_perms::set_app_dir_mode` |
| SIGINT/TERM/HUP → 清理半成品文件（§3.4.5） | `signal-hook`（INT / TERM / HUP） | console event（`SetConsoleCtrlHandler`，本仓实现） | `rencoder_cli::signal` + `rencoder_lib::pipeline::partial_output` |

**信号处理的 platform divergence 在定义处**：本节列出的 fs_perms 调用的是 Unix-only Rust stdlib
API（`PermissionsExt::from_mode`），所以需要 `#[cfg(unix)]` 切到 no-op 分支。信号处理同样有
platform divergence，但**只存在于 `rencoder_cli::signal` 一个文件里**：Unix 侧用 `signal-hook`
收 SIGINT / SIGTERM / SIGHUP，Windows 侧用 `windows-sys` 的 `SetConsoleCtrlHandler` 收 console
event。调用方（`main.rs`）看到的签名不变——`install_handler() -> CurrentOutput`。

**为什么这次换了方案**：2026-09 曾从 `signal-hook` revert 回 `ctrlc`，理由是 `ctrlc` 一个 crate
同时覆盖 Unix signal 与 Windows console event，源码里一处 platform-specific 调用都没有。代价是
**Unix 侧只剩 SIGINT**：`encode.zsh` 的 `trap _cleanup INT TERM HUP` 覆盖三个信号，
`kill <pid>`（SIGTERM，容器编排器的默认停止方式）与 SSH 断连（SIGHUP）直接杀进程，半成品留在
磁盘上。现在改为"Unix 用 signal-hook + Windows 用自己的 console handler"：两个平台各自覆盖
完整的信号集合，divergence 仍然关在定义处。

**GUI 不复用**：`rencoder_ui`（webview）有自己的 beforeunload / IPC 机制，不复用
`rencoder_cli::signal`，会单独写。

#### 调用范式

```rust
// 跨平台统一调用，调用方零 cfg
fs_perms::set_config_file_mode(&tmp)?;
```

非 Unix 分支显式写 `let _ = path; Ok(())` 而非省略——让"该平台未实现"
成为**编译期事实**，而不是运行期才发现调用缺失。落地非 Unix 平台
行为时只改 `fs_perms.rs` 一个文件，调用方零改动。

---

## 2. 技术决策

| 决策点 | 选择 | 理由 |
|---|---|---|
| 转码调用 | `std::process::Command` 调用 `ffmpeg` | 编码参数复杂，命令行表达最自然 |
| 元数据探测（默认） | `std::process::Command` 调用 `ffprobe` | 编译简单，无额外依赖 |
| 元数据探测（可选） | C FFI 桥接层（`c-probe` 特性） | 高性能批量分析；避免 `ffmpeg-next` 版本兼容问题 |
| 编码器列表 | `ffmpeg -encoders`（缓存一次） | 运行期间不变 |
| NVIDIA 检测 | CLI `--nv`/`--no-nv` > 配置 `[encode].nv`；启用时做**能力实测**（1 帧试编码）并进程内缓存 | `ffmpeg -encoders` + `nvidia-smi -L` 都不足以证明"这张卡能跑这个编码器"（AV1 NVENC 需 CC≥8.0）；不用 `nvml-wrapper` |
| 最大帧率 | `MaxFpsConfig::decide` 判定 + `build_ffmpeg_command` 参数映射 | 谓词与参数生成解耦；判定与参数映射同源，杜绝"记录说降了帧、命令里却没有 `-r`"的自相矛盾 |
| 帧率判定源 | `r_frame_rate`（瞬时上限），`avg_frame_rate` 仅用于展示与统计 | VFR 素材的平均帧率被大量长间隔拉低，用它判上限会漏掉局部超速（对齐 encode.zsh v10.2.0） |
| 帧率参数 | 未超限 `-fps_mode passthrough`；超限 `-r <cap> -fps_mode cfr` | 不写 `-fps_mode` 时 ffmpeg 会按容器标称帧率补帧/丢帧，VFR 源有损（对齐 encode.zsh v10.2.0） |
| 帧率容差 | `eps = 0.01`，判定式 `src > cap + eps`（严格大于，eps 在上限右侧） | 直接对齐 encode.zsh 的 `MAX_FPS_EPS` 与 `(( fps > MAX_FPS + MAX_FPS_EPS ))` |
| FFmpeg 输出 | `ProcessOutput::Inherit` 透传（默认） | 与脚本一致；未来 GUI 用 `Piped` |
| 文件遍历 | `walkdir` | 纯 Rust，不依赖外部工具；不跟随符号链接 |
| 线程模型 | 转码：单线程串行；分析：可选并行（`rayon`） | 转码与脚本一致 |
| 信号处理 | Unix `signal-hook`（INT/TERM/HUP）+ Windows `SetConsoleCtrlHandler` + `parking_lot::Mutex` | 无中毒语义；两个平台都覆盖完整信号集合，divergence 关在定义处 |
| 命令行解析 | `clap`（derive） | 仅 CLI 前端 |
| 交互模式 | `dialoguer` | 仅 CLI 前端 |
| 错误处理 | `anyhow`（CLI）+ `thiserror`（核心库） | 分层清晰 |
| 日志（核心库） | `tracing` | 核心库与 binary 同一栈（v1.2 follow-up #4）；`publish = false` 不再受外部兼容性约束 |
| 日志（CLI 前端） | `tracing` + `tracing-subscriber` | CLI 用结构化 tracing；无桥接（lib 直接 emit `tracing` 事件） |
| 日志（文件 sink） | `<app_dir>/log/rencoder.YYYY-MM-DD.NN.log`（U18） | 日滚 + 10MB 大小滚（NN 自增），7 天保留（`max_age_days=0` 不限）；自定义 `MakeWriter` 实现；`RENENCODER_LOG_DIR` 覆盖 |
| 日志（span） | `#[instrument]` + `FmtSpan::CLOSE` | 关键函数（`encode_cmd` / `analyze_cmd`）记录 span 上下文，便于排障 |
| 配置（用户面） | TOML via `AppConfig`（U18） | 替代 jq + 自写 JSON schema；unknown keys 立即报错（`#[serde(deny_unknown_fields)]`） |
| 配置（有效配置） | `EncodeParams` = 合并后的运行配置（§3.10.5） | 运行时 `Config` 退役（item 85）；"默认←文件←命令行"的合并收在一处仍是 future |
| 分析聚合 | `polars`（DataFrame `group_by` + `agg`） | 复用 Polars 生态；不再手写 BTreeMap 遍历 |
| 分析输出（默认） | per-file CSV 到 stdout + 一行 summary 到 stderr | 原始数据可管道到 DuckDB/Polars 二次分析；摘要与数据流分离 |
| 转码工作记录 | `EncodingRecord`（4 维度：source / process / result / status） | 工作分析（吞吐、压缩率、失败率），不是视频分析 |
| 工作记录存储 | `<app_dir>/data/encoding-YYYY-MM-DDTHH-MM-SSZ.parquet`（按运行分文件，U18） | 每跑一次产出一个自包含文件，多批分析用 `read_parquet(['<app_dir>/data/*.parquet'])` glob；`RENENCODER_DATA_DIR` 覆盖 |
| 工作记录序列化 | 单 DataFrame 中转（`records_to_dataframe`）+ 三种 writer（CSV / JSONL / Parquet） | schema 在 `records_to_dataframe` 一处定义；writer 都是 thin wrapper |
| 同路径保护 | 三层：`output_path_for` 命名避让 → `utils::fs::is_same_file` 判定 → 转码前拦截 | 输出覆盖输入会销毁源文件；取消清理按 `argv.last()` 删产物，输出等于输入时删的是输入（结构性失败） |
| 单目录应用部署 | `scripts/dist.sh` 把 target 产物 + LICENSE + README + 配置种子装配成单目录应用（U18） | 跨平台一致；解压即用；放弃 `cargo install` 的共享 `~/.cargo/bin/` 模式 |
| `rencoder_lib` publish 策略 | `publish = false`（U18） | 内部 lib，不发布到 crates.io；防止内部 API 泄露锁死 schema |
| 配置路径解析 | `$RENENCODER_CONFIG` > `<app_dir>/rencoder.toml`（U18） | 二级解析；env 覆盖用于 read-only fs / 调试 |
| 跨平台代码封装 | cfg 只在定义处（`fs_perms.rs`），调用处 uniform signature（U18） | 平台差异集中单文件维护；非 Unix 平台尚未实现（future work） |
| Rust 版本 | 2021 Edition | 稳定 |
| 目标平台 | Linux / macOS（Windows 后续） | 跨平台 |

### 2.1 关于 `ffmpeg-next` crate 的决策

**不采用 `ffmpeg-next`**：
- 对 FFmpeg 库版本有严格要求，版本滞后风险高
- 转码仍需 `ffmpeg` 命令行，引入 `ffmpeg-next` 只优化探测，收益有限
- 编译依赖复杂，跨平台困难
- `unsafe` 代码量大

**替代方案：C FFI 桥接层**（可选特性 `c-probe`）：
- C API 是 FFmpeg 唯一承诺稳定的接口，天然版本兼容
- 桥接层代码量极小（约 50 行）
- 编译时通过 `pkg-config` 自动适配
- 无状态、可重入，支持 `rayon` 并发

### 2.2 关于 NVIDIA 编码器选择的决策

**不通过 `ffmpeg -encoders` 输出推断硬件能力**：
- `ffmpeg -encoders` 包含 `h264_nvenc` 只说明编译时启用了该编码器，不代表当前机器有 GPU 或驱动可用
- 进一步，`ffmpeg -encoders` 里有 + `nvidia-smi -L` 非空也**不代表这张卡能跑这个编码器**：
  `av1_nvenc` 需要 compute capability ≥ 8.0（RTX 30 系 / Ada / Hopper），GTX 16xx（CC 7.5）
  上编码器在、卡也在，一跑就失败；驱动与 ffmpeg 版本不匹配、NVENC 会话额度耗尽同理

**优先级：命令行 > 配置项**：

```
--nv / --no-nv 给了吗？
├─ 给了 → 用它（--nv = 启用，--no-nv = 禁用）
└─ 没给 → [encode].nv（rencoder.toml），再没给就是禁用
```

`nv` 是**机器相关**的旋钮：同一份 `rencoder.toml` 拿到另一台机器上就该能改，所以它是
U17 "旋钮全进配置" 的刻意例外。

GUI 侧同样有开关（Setup 页 on/off，写进 `[encode].nv`）。`nv` 是**本次运行的选择**，
不是机器事实（§3.10.5）：所以切 `nv` **不需要重新探测能力**、**不需要重建编码器缓存**
——只需要在编参数时把 `nv` 与能力配置合并判断。下一个批次自然用新值
（参数在 StartBatch 时从当前配置组装）。这条旁路原本是"`nv` 变化 → 重探能力 → 换缓存"，
现在没有了。

UI 里配置只有一份事实来源：`state.config_draft`。Setup 页编辑它，编码参数从它
构建（`build_encode_params_from_draft`），保存时它就是 `Command::SaveConfig` 的载荷。
lib 启动时加载的那份只用于启动时的初值。

**启用时的检测：能力实测，不是存在性检查**：

对 `ffmpeg -encoders` 里出现的每个 NVENC 编码器各跑一次 1 帧试编码：

```
ffmpeg -hide_banner -loglevel error -f lavfi -i color=s=64x64:d=0.1 \
       -frames:v 1 -c:v <enc> -f null -
```

成功即"这台机器能用这个编码器"。这一条命令同时验证了驱动、ffmpeg build、NVENC 会话
额度与卡的计算能力——`nvidia-smi -L` 只证明"有卡"，`--query-gpu=compute_cap` 只覆盖
CC 门槛这一条规则。

- **结果进程内缓存**：同进程内多次批次复用（检测要 ~200-500ms × 编码器数，只付一次）
- **只在启用时付这个成本**：`nv` 关闭时路径与从前完全一致（零额外开销）
- **失败原因不分类**：ffmpeg 的报错文本不结构化，稳妥做法是把 stderr 原文带进 warn，
  维护"错误文本 → 原因"的映射表容易和 ffmpeg 版本脱节

**不可用则 fallback，且必须出声**：

```
选中格式的 NVENC 编码器不在可用集合？
├─ 是 → 回退软件编码 + warn（带探测时的 stderr 原文）+ 记录里照实写实际编码器
└─ 否 → 用 NVENC
```

`[encode.encoders]` 里显式指定的编码器同样按"不可用则 fallback"，但也出声（warn：
你指定的 `av1_nvenc` 不可用，改用 `libsvtav1`）。静默替换是这里最坏的行为——用户以为
在用硬件，其实在用软件。

**NVENC 编码失败 = 当场 fallback（先核实能力）**：

探测结论可能过期（驱动升级、ffmpeg 换版本、GPU 被禁用），于是"探测说可用、实际跑
不起来"。但**失败的编码不等于能力缺失**：磁盘满、透传参数非法、输出目录不存在，
都会让 NVENC 编码失败，而降级对这些问题毫无帮助——只会双倍耗时，还把真问题掩盖
掉。所以失败后先**核实**这台机器到底还能不能跑这个编码器，再决定降不降：

```
用了 NVENC 编码器的这次 ffmpeg 调用失败了？
├─ 能力核实通过（这台机器确实能跑）→ 失败是别的原因 → 按原样记失败，不降级
└─ 能力核实失败（这台机器确实不能跑）→ 该编码器在本进程内标记为不可用
   → 用软件编码器重建命令 → 重跑这个文件
```

降级对本进程后续文件同样生效（避免每个文件都先失败一次）。

**"能力核实"是对该编码器再跑一次 1 帧试编码**（~200-500ms，只发生在失败路径上）。
这不是刷新存下来的记录——记录不动，只是在失败的当下确认一次能力。结论记在**能力
对象**里（§3.10.5）：同一个编码器第二次失败不再重复核实；而**检查的结论说它不可用
时直接采纳**（同一个问题，启动时已经问过）——只有"检查说能用、编码却失败了"才需要
现场再探一次（设备可能刚刚掉线）。

**为什么核实而不看失败文本**：CLI 默认让 ffmpeg 的 stderr 直接透传到终端
（`ProcessOutput::Inherit`），而 `Command::output()` **不会**捕获已显式 inherit 的
stderr（实测捕获到 0 字节），所以失败路径拿不到 ffmpeg 的报错文本，无法区分
"显卡跑不了"和"磁盘满了"。试编码核实是唯一在所有路径（CLI 透传 / GUI 管道 /
Null）上都能给出确定答案的办法。

**记录里要留得下"曾经降级"这件事**：

| 情况 | 记录 |
|---|---|
| 降级后软件编码成功 | `outcome = Succeeded`，`encoder_selected` = 实际用的软件编码器，`ffmpeg_stderr_tail` = `[nvenc fallback] <编码器> failed: <原因>` + 那次失败的 stderr |
| 降级后软件编码也失败 | `outcome = Failed`，`error.message` = `NVENC <编码器> failed: <原因>; software fallback failed: <原因>`，stderr 尾把两次都带上 |

只看日志会漏掉这个信息——事后翻记录时分辨不出"原来是 NVENC 挂了才降级"。

**检测结果跨进程保存**：

硬件不会天天变，探测结论没必要每次运行都重做。结论存到
`<app_dir>/nvenc-capability.json`（应用目录根部，见 §3.6）：

```json
{
  "version": 2,
  "ffmpeg_version": "ffmpeg version 9.0.1 ...",
  "ffmpeg_fingerprint": "<版本串 + 编码器列表的哈希>",
  "probed_at": "2026-11-17T14:23:01Z",
  "next_review_at": "2026-12-17T14:23:01Z",
  "encoders": [
    { "encoder": "h264_nvenc", "usable": true,  "reason": null },
    { "encoder": "hevc_nvenc", "usable": true,  "reason": null },
    { "encoder": "av1_nvenc",  "usable": false, "reason": "No NVENC capable devices found" }
  ]
}
```

- **每个候选都有一项**：记录存的是"测试结果"，形状不随机器变化——构建里没有的
  那个编码器同样列出来，结论是不可用，原因是构建缺失（两级判定见 §3.10.5）。
  构建一个 NVENC 都没有时，三条同样的记录在展示时合成一行
  "this ffmpeg build has no NVENC encoders"。

- **指纹必须绑定 ffmpeg 构建**：探测回答的是"**这个 ffmpeg** 能不能在这里跑
  av1_nvenc"。用户换了 ffmpeg（brew 升级、另一个环境的二进制）答案就变了。指纹 =
  `ffmpeg -version` 版本串 + 编码器列表的哈希；加载时对不上就当没有缓存。
- **过期只是建议**：30 天后（`next_review_at`）提示用户"检测结果可能过期，是否重新
  检测？"，**用户选 yes 才重测**——没有人比用户更清楚这台机器的状态。
  - **复审周期由 lib 掌管**：解析发现记录到期时，lib 自己把 `next_review_at` 往后推
    30 天并落盘，前端只负责**显示**。状态机只有一份——两个前端，以及以后要加的
    第三个，都不必各自记着"显示完还得延期"这件事。
  - **CLI**：在终端里问 y/N；选 no 就不再打扰（非交互环境只提示不追问）。
  - **GUI**：没有 stdin，问不了——UI 把它显示成一行提示 + 一个"重新检测"按钮。
    用户点按钮就刷新，不点就一直用旧记录。两条路最后是同一个状态机：
    提示一次 → 30 天内不再提示。
  - 提示与本次运行**用不用 NVENC 无关**：这条记录说的是这台机器的能力，不是这次
    任务的一个开关。
- **懒首次**：没有可用缓存时，普通运行顺带探测一次并写入，用户不需要先跑 `detect`。

**detect 命令**：

```
rencoder detect              # 独立使用：探测 + 写入 + 打印报告
rencoder --detect encode ... # 作为命令行选项：本次强制探测（并更新缓存）
```

GUI 走同一条路：`Command::Detect { force }` → lib 用同一个记录文件解析 → 发
`Event::NvencCapability` 回报。UI 的 Setup 页在 NVENC 开关旁显示能力清单
（每个编码器能不能用、不能用是什么原因），并提供"重新检测"按钮；启动时 lib
也会解析一次并回报，所以用户不点按钮也知道这台机器支持什么。

`--detect` 是全局选项，出现在子命令前后都认。`detect` 的报告本身就是这项设计的
主要价值——把"这台机器能用哪些硬件编码器"变成用户能主动查的事实：

```
NVENC capability (probed 2026-11-17T14:23:01Z)
  ffmpeg      : ffmpeg version 9.0.1 (homebrew)
  h264_nvenc  : usable
  hevc_nvenc  : usable
  av1_nvenc   : UNUSABLE — No NVENC capable devices found
```

**码控按编码器家族区分**：

NVENC 用的是恒定质量（`-cq N -b:v 0`），不是 x265 的 CRF（`-crf N`）。对齐
`encode.zsh` 的 `ENC_SPEC`：选中 NVENC 编码器时推 `-cq N -b:v 0`，其余编码器仍是
`-crf N` / `-b:v Nk`。两条量纲不能混用——混用等于悄悄换掉用户画质参数的含义。

```
用户传了 --nv？
├─ 否 → 软件编码
└─ 是 → 能力实测（进程内缓存）
        ├─ 探测不可用 → 软件编码（warn，不视为错误）
        └─ 探测可用 → ffmpeg 编译了对应编码器？
                ├─ 否 → 软件编码
                └─ 是 → nvenc ✓
```

### 2.3 关于日志栈的决策

**核心库与 binary 全部用 `tracing`**（v1.2 follow-up #4，替代之前的 `log` + 桥接方案）：

- `rencoder_lib` 是 `publish = false`（U18，internal lib）—— 没有外部消费者，不再受 "API surface 不应依赖 tracing" 这条约束限制。
- 关键收益：日志**结构化字段**（`output = %path`, `source_fps = f`, `batch_id = id`）—— 替代之前 `log::info!("...{} {}", a, b)` 这种把字段名丢进字符串拼接的做法。`tracing-subscriber` 的 `fmt::Layer().json()` 输出可以按字段聚合 / 检索。
- 关键收益：`#[instrument]` 自动给函数挂 span context —— `run_ffmpeg_with_progress` 加 instrument 后，子事件（progress / stderr / cancel）自动继承函数名 + 参数作为字段。
- 统一栈：`RUST_LOG` 一套规则控制 lib + CLI + UI，不需要 `tracing-log::LogTracer` 桥接。
- 旧方案 `log` + 桥接的唯一好处是"库可被不依赖 tracing 的项目使用" —— 我们没有这种外部消费者，桥接是 dead weight。

**CLI / UI 用 `tracing-subscriber`**：

- `Registry` + 多 `Layer` 模式天然支持双 sink（stdout + 文件）。
- CLI: stdout + 自写 file sink（§"文件 sink" 段）。
- UI: stdout（debug 时）+ `crossbeam_channel::Sender<LogLine>` 桥到 `state.log`，由专用 drain thread 推给 footer / `[LOG]` 页（§3.8.7.5）。

**文件 sink 用自写 `MakeWriter` 而非 `tracing_appender`**：

- `tracing_appender::rolling` 只支持按 `Rotation`（DAILY / HOURLY / NEVER）滚动，**不支持大小阈值**。我们需要"日滚 + 10 MB 大小滚"，必须自己实现 `MakeWriter`。
- 文件名 `rencoder.YYYY-MM-DD.NN.log`：日期每天更换，NN 在当天文件超过 `max_size` 时自增。
- 启动时根据 `max_age_days` 清理过期文件（`0` 表示不清理）。
- `FileSinkHandle` 仅持有 `&Mutex<Inner>`，利用 `Mutex` 的内部可变性避开 `MakeWriter` 给的 `&'a self` 限制。

**为什么不用 `colored` crate**：

- 颜色化是终端能力，`tracing-subscriber` 的 `fmt` 层默认不染色，且 ANSI 颜色在重定向到文件时会变成噪音；如需彩色输出，让终端通过 `RUST_LOG_STYLE=always` 等环境变量启用，避免库内硬编码。

### 2.5 关于 batch size 上限的决策（v1.2 follow-up #5）

**为什么需要上限**：
- `walkdir::WalkDir` 遍历目录收集输入——没有上限时，用户误选根目录（`/`、`/Users` 等）会收集 10 万+ 文件，actor 的 `cmd_tx` channel 一次性写入 `Vec<InputPath>` 序列化 payload，actor 端反序列化占用大量内存。
- 每文件启动 1 个 ffmpeg 子进程（v1.2 batch 处理是顺序的），10000 文件 → 5+ 小时运行；中途 cancel 留大量 partial。
- 没有上限的 dry-run 也耗资源：每文件 ffprobe 一次，10k probe × ~50ms = 8+ 分钟。

**上限来源**：
- `[encode] max_batch_size = 5000` 在 `rencoder.toml`，默认 5000。
- 环境变量 `RENENCODER_MAX_BATCH_SIZE` 覆盖（CI / 临时调大场景）。
- 验证：`max_batch_size >= 1`（0 禁用上限，但不推荐——明确写一个超大数表示"我不想要上限"）。

**强制位置**：`collect_files`（`rencoder_lib/src/collect.rs`）返回 `Result<Vec<InputPath>, EncodeError::BatchTooLarge { count, max }`。在 walkdir 完成填充前就 short-circuit：已经收满 `max_batch_size + 1` 个文件就停，**不**全量收集再判。

**错误返回**：
- CLI：打印 `error: batch too large: 5001 files (max 5000); narrow extensions or split the dir` 到 stderr，exit 1。
- UI：toast `Batch too large: 5001 files (max 5000). Narrow extensions or split the dir.`，**不** dispatch `Command::StartBatch`（defense in depth——在 reducer 也要拒绝）。

**为什么不放到 actor 的 busy-gate**：busy-gate 是"已有 batch 进行中就拒绝下一个"，但 collect_files 的批量收集发生在 dispatch 之前——busy-gate 防不到。cap 必须在 collect_files 层面。

### 2.4 关于工作记录数据模型与存储的决策

**这是工作分析工具，不是视频分析工具。** 用户问的是「这次跑得怎么样？哪几条慢？哪些失败？」——这是工作流问题，不是媒体属性问题。数据模型按四个维度组织：

1. **Source（输入）**：source_path、source_size_bytes、source_codec、source_container、source_resolution、source_fps、source_duration_secs、source_bit_rate_bps
2. **Process（工作过程）**：started_at、ended_at、encoding_duration_secs、encoder_selected、encoder_settings、ffmpeg_argv、ffmpeg_stderr_tail
3. **Result（输出）**：output_path、output_size_bytes、output_duration_secs、output_codec、compression_ratio
4. **Status（独立监测）**：outcome、error_category、error_message

前三个维度基于工作流（每次跑一个文件产出一条记录）；最后一个是独立的监测数据。

**存储格式按场景选**：

- **JSONL（默认 stdout）**：每条记录一行 JSON，stream-friendly，所有字段保留，可直接 pipe 到 `jq` 或 `duckdb`。
- **Parquet（`-o file.parquet`）**：列式压缩格式，schema 内嵌，Snappy 压缩；原生 DuckDB / Polars 输入。多批分析：`SELECT * FROM read_parquet('./data/*.parquet')`。
- **CSV（`-o file.csv`，兼容旧脚本）**：仅保留最常用的列（path、fps、codec、resolution、duration、file_bytes、outcome）。

**单一 DataFrame 中转，writer 是 thin wrapper**：

```
records → records_to_dataframe（schema 在这一处定义）
                ↓
    ┌───────────┼──────────┐
    ↓           ↓          ↓
df_to_csv  df_to_jsonl  df_to_parquet
```

`records_to_dataframe` 是唯一的 schema 定义点。writer 都是 2-5 行的 thin wrapper，schema 变化时只改一处。不做 `records_to_jsonl → records_from_jsonl_to_parquet` 这种 round-trip（浪费一次 parse，且丢失 Polars 的列类型信息）。

**每跑一次产出一个文件**（`./data/encoding-<UTC-timestamp>Z.parquet`）：

- ISO 8601 UTC（`Z` 后缀避免时区歧义），`:` 替换为 `-`（macOS / Windows 文件系统兼容）
- 时间戳文件名可字典序排序 = 时间顺序，`ls -t | head -1` 拿到最近一次
- 多批分析用 glob：`read_parquet(['./data/*.parquet'])`
- 进程 ID 后缀（`a3f`）避免同一秒内并发冲突

> 注：NVENC 探测失败（驱动缺失、卡不支持该编码器、会话额度耗尽）一律归为降级路径，
> `init_encoder_cache` 不因此返回 `EncodeError`——不可用就是不可用，不是错误。
> 能力探测**与 `nv` 开关无关**（§3.10.5）：`nv` 是"本次运行用不用 GPU"，
> 能力是"这台机器行不行"——两回事。

不使用 `nvml-wrapper` crate，也不再调用 `nvidia-smi`：NVENC 能力由 ffmpeg 的 1 帧试编码
实测得出（§2.2），它同时覆盖驱动、ffmpeg build、会话额度与卡的计算能力，比"查一张卡
在不在"更准。

### 2.6 关于帧率上限判定的决策（对齐 encode.zsh v10.2.0）

**判定源从平均帧率换成瞬时帧率上限**：

ffprobe 一次探测给出两个帧率：`avg_frame_rate`（平均速率）与 `r_frame_rate`（real base frame rate，由最短帧间隔反推的瞬时上限）。两者在 CFR 素材上相等，在 VFR 素材上可能差很远。

原实现用 `best_fps()`（优先 avg）判定是否超限。VFR 素材的平均帧率会被大量长间隔拉低——例如一段实录大部分时间 24fps、局部 60fps，平均值约 30fps，上限 30 时判定为"未超限"，于是 60fps 段落原样保留在产物里，容器帧率标签仍是 59.94。用户看到的是"设了上限却没降帧"。

对齐脚本 v10.2.0：上限判定改用 `r_frame_rate`（`VideoMetadata::nominal_fps`），平均帧率退居展示与统计用途（分析器的 fps 列、记录表的 `source_fps` 仍用 avg，语义不变）。

**判定与参数映射必须同源**：

同一个"是否降帧"的判断散落在三处（帧率动作判定、argv 参数映射、结果记录）。脚本在这上面栽过两次：v10.2.0 之前用"`MAX_FPS_ARGS` 数组是否为空"当判据，而加入 `-fps_mode passthrough` 后该数组恒非空，判据失效。

因此引入 `FpsDecision` 枚举：判定一次得到动作（`Disabled` / `Unknown` / `Passthrough` / `Reduce`），动作直接决定注入的参数与结果记录里的 `ReducedFps` 标记。三处读同一个动作，不可能自相矛盾。

**必须显式声明 `-fps_mode`**：

ffmpeg 不带 `-r` 时并不等于"不干预帧率"——默认 vsync 行为会按容器标称帧率补帧或丢帧，VFR 源因此有损。脚本 v10.2.0 起显式声明：

- 未超上限：`-fps_mode passthrough`（保留源时间戳，不复制不丢帧，容器帧率标签仍是源 tbr）
- 超上限：`-r <cap> -fps_mode cfr`（输出转 CFR，容器帧率标签同步降为 cap）

未启用上限（`max_fps == 0`）时不注入任何帧率参数，与脚本一致。

**容差**：`eps = 0.01`，判定式 `src > cap + eps`（严格大于，eps 加在上限右侧）。对齐脚本的 `MAX_FPS_EPS` 与 `(( fps > MAX_FPS + MAX_FPS_EPS ))`。29.97 的源在 cap=30 下不动（29.97 < 30.01）。

**暂不实现的两项脚本能力**：两趟编码（脚本 pass1/pass2 同步帧率参数）——Rust 端当前是单趟编码，无 pass1/pass2 结构可对齐；产物校验层（脚本 v11 转码后回读产物核对编码/帧率/时长/音频）——需要扩展 C 探测层暴露 `nb_frames` 等字段，单独排期。

---

## 3. 架构设计

### 3.1 模块划分

#### 核心库（`rencoder_lib`）

模块划分按工作流组织。`data/` 是工作记录层（不是视频分析层），`encoder/` 是 ffmpeg
构建与执行层，`pipeline/` 是 CLI→任务→记录的编排层，`utils/` 是跨模块的共享工具，
`config/` 是数据驱动的配置层。

**最终状态**（U16 + U17 之后）：

- `data/extract.rs` 拥有 probe 实现 + fps 解析（`probe_video_metadata*`、`parse_fps_value`）
- `data/store.rs` 拥有 Polars DataFrame writers（`records_to_dataframe`、`records_to_csv/jsonl/parquet`、`dataframe_to_*`）
- `data/record.rs` 拥有 `EncodingRecord` 组装 helpers（`encoder_settings_from_params`）
- `encoder/{command,select}.rs` 拥有 `build_ffmpeg_command` + `select_encoder`
- `pipeline/{batch,cancel,cache}.rs` 拥有 `transcode_batch` + `CancelToken` + `init_encoder_cache`
- `utils/{fs,time}.rs` 拥有 `output_path_for` + `is_same_file` + `utc_timestamp_filename`
- `config/mod.rs` 拥有配置的重导出（运行时 `Config` 已退役 —— §8 item 83）
- `config/app.rs` 拥有 user-facing `AppConfig`（U18 重命名自 `project.rs`；TOML 加载在 U17 新增）

```
rencoder_lib/
├── lib.rs                       # 库入口，pub mod 声明
├── analyzer/
│   └── records.rs              # probe → AnalysisRecord + aggregate → VideoStats
├── collect.rs                   # 文件收集（按目录 / 单文件 / 扩展名过滤）
├── config/
│   ├── mod.rs                   # 重导出
│   └── app.rs              # AppConfig + Encode/Analyze/Encoders/FormatArg/AnalyzeFormatArg
├── data/
│   ├── mod.rs
│   ├── extract.rs               # probe_video_metadata*, parse_fps_value, stderr_tail, output_size, output_duration
│   ├── record.rs                # encoder_settings_from_params
│   └── store.rs                 # records_to_dataframe, records_to_csv/jsonl/parquet, dataframe_to_*
├── encoder/
│   ├── mod.rs
│   ├── command.rs               # build_ffmpeg_command（参数映射，含 -r 输出选项位置）
│   ├── ffmpeg_arg.rs            # 类型级 ffmpeg argv 安全边界（§3.x.y）
│   └── select.rs                # select_encoder（cache + Config → 编码器名）
├── error.rs                     # EncodeError（thiserror）
├── job.rs                       # GUI actor 入口（§3.8）：Command/Event + start_controller()
├── pipeline/
│   ├── mod.rs
│   ├── batch.rs                 # 编排：collect_files → transcode_batch（按文件循环，写记录+日志）
│   ├── cache.rs                 # init_encoder_cache（ffmpeg -encoders + NVENC 能力实测）
│   └── cancel.rs                # CancelToken（§3.8；Arc<AtomicBool> + cancel()/is_cancelled()）
├── types.rs                     # 跨模块共享类型（VideoFormat、EncodingRecord、MaxFpsConfig 等）
└── utils/
    ├── mod.rs
    ├── fs.rs                    # 路径规范化、output_path_for、is_same_file
    └── time.rs                  # UTC 时间戳格式化（ISO 8601）

# 仍在 c-probe feature 门控下（独立于上面的分层）：
└── c-probe/                     # 旧 ffi/ 重命名；C 桥接层 + Rust 包装
    ├── mod.rs
    └── video_probe.c
```

**为什么这样分层**：

- **data/** 是工作记录层（work-record layer）。四种状态（source / process / result / status）
  都是工作分析的维度，不是视频分析。`extract` 拿到原始数据（probe + fps parse + stderr），
  `record` 把它组装成 `EncodingRecord`，`store` 把它写到 `data/` 目录供下游 DuckDB / Polars 查询。
- **encoder/** 是 ffmpeg 特定的实现细节。`command` 把 `EncodeParams` 转成 argv，`select` 选编码器。
  这层**只**对 ffmpeg 关心；不包含 cache（cache 是工作流层的事）。
- **pipeline/** 是 CLI → 任务 → 记录的编排。它收集文件、循环每个文件、调用 encoder、
  收集 capture 写入 record、调 log。`batch.rs` 是这一层的主要实现，`cache.rs` 是辅助。
- **utils/** 是跨模块共享的最小工具集。`fs` 路径规范化，`time` UTC 时间戳。
  日志（CLI 端 `tracing_init`）**不**在这一层 —— 日志是跨切的关注点。
- **config/** 只有一层：`app.rs`（user-facing TOML schema）。运行时 `Config` 已退役 ✓
  ——`encoders` 与 `ffmpeg_args` 进了 `EncodeParams` ✓，所以"这次运行用了什么配置"
  只有**一个**答案 ✓（`EncodeParams` ✓）。CLI 与 GUI 都走 `from_app_config` ✓；
  "默认 ← 文件 ← 命令行"的合并收在一处是 §3.10.5 的 **future** ✓。

**为什么 `analyzer/` 还在，`fps/` 已删除**：

- `analyzer/records.rs` 是 U7 起的视频聚合分析的现状。U20 把模块从 `fps_stats` 重命名为
  `records`（旧名出现在历史 U7/U7.5 段落里），把 `FpsStats` 重命名为 `VideoStats`（见下）。
  CLI 的 `analyze` 子命令直接用。
  当真有 `--query "SELECT fps, COUNT(*) ..."` 这种 CLI 时再考虑迁移到 `data engine/`；目前
  Pass 1-2 没有这条路径。
- U20 重命名决策记录：`FpsStats` 原本是 U7 起的 fps 分组聚合类型，U7.5 加 Polars 表之后字段
  扩展到 fps + codec + duration + bytes + 计数 + failed_paths，名字已经不准确（不只是 fps）。
  重命名为 `VideoStats`，反映它实际代表的是"一个目录的 video 分析聚合结果"。`analyze_fps`
  在 U20 同次清理中删除：它只是 `aggregate_records(analyze_records(files))` 的便利包装，
  CLI 不调用（CLI 走 TaskManager 后自己组装），只有 2 个 integration test 在用，删了之后
  那 2 个 test 一起删。**不删 `aggregate_records`** -- 它是真正的聚合逻辑，CLI Text 格式
  仍依赖 `VideoStats::summary()` 渲染 Polars 交叉表（fps × codec × files × duration × bytes）。
  把聚合逻辑塞进 `main.rs` 是分层错误（main 是 thin call layer，聚合属于 lib）。
- `fps/` 已删除（option B）。`parse_fps_value` 移入 `data/extract.rs`（probe 时用 fps
  字符串解析，逻辑上属于「数据采集」层）；`fps/limit` 是空壳，整个 `fps::limit` 概念已不存在
  （`MaxFpsConfig::need_reduce` 直接放在 `types.rs`）。

**U16 Stage 6（已完成）**：`ffmpeg/` 和 `encoding/` 模块已删除。

- 这两个模块是 U16 重构的「implementation 旧址」，option B 时保留了 `pub use` 桥接以
  避免破坏潜在的下游消费者。U16 Stage 6 确认内部已无外部引用后彻底删除。
- `pub use` 桥接完成历史使命；唯一一处曾用 `rencoder_lib::encoding::*` 的内部 caller
  （`cli/main.rs::write_records`）已改用 `rencoder_lib::data::store::*`。
- 现在的 public API 没有 `ffmpeg/` / `encoding/`；probe 实现是
  `data::extract::{probe_video_metadata, probe_video_metadata_auto, parse_fps_value}`，
  writers 是 `data::store::{records_to_csv, records_to_jsonl, records_to_parquet, records_to_dataframe, ...}`，
  记录类型是 `types::{EncodingRecord, EncodingOutcome, ...}`。这是 breaking change —
  任何使用旧路径的外部代码必须迁移。

**当前不在分层里的概念（按需再加）**：

- `data engine/{express, process, I/O}` —— Pass 2 之前的占位符，YAGNI。Polars 本身
  就是引擎；我们不需要自己的 wrapper 直到有 CLI 查询接口（`rencoder analyze-query` 之类）。
- `validation/` —— 配置校验逻辑足够小，跟着 `config/app.rs` 的 `validate()` 走就好。
- `c-probe/` 的拆分 —— 旧 `ffi/` 重命名即可，不要再分。

#### CLI 前端（`rencoder_cli`）

```
rencoder_cli/
├── main.rs
├── args.rs               # clap 参数（含 --nv）
├── interact.rs           # dialoguer 交互
├── signal.rs             # signal-hook (Unix) / console handler (Windows) + parking_lot::Mutex
├── tracing_init.rs       # tracing-subscriber 初始化 + 双 sink
└── file_sink.rs          # 自定义 MakeWriter：日/大小双轴滚动
```

#### GUI 前端（`rencoder_ui`，U21 计划）

```
rencoder_ui/
├── main.rs               # Dioxus desktop 入口
├── app.rs                # 顶层 AppState + reducer + App 组件
├── view/                 # 组件（按 view-model 分文件）
│   ├── mod.rs
│   ├── start_view.rs     # 选择目录 / 配置 / 开始
│   ├── progress_view.rs  # 当前文件、ETA、批进度
│   └── log_view.rs       # VecDeque<LogLine> 渲染
│   └── history_view.rs   # 历史记录列表（v1.1——见 §3.8.7 v1 limitations）
└── msg.rs                # Msg 枚举 + reduce() 函数
```

详见 §3.8。

---

### 3.2 核心数据结构

```rust
use std::path::PathBuf;

// ===== 目标格式 =====
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VideoFormat {
    X264,
    Hevc,
    Vp9,
    Av1,
}

// ===== 视频元数据（一次探测，多字段） =====
#[derive(Clone, Debug, Default)]
pub struct VideoMetadata {
    pub codec: Option<String>,
    /// 平均帧率（ffprobe 的 avg_frame_rate）。VFR 素材会被长间隔拉低，
    /// 不能用于上限判定，只用于展示与统计。
    pub avg_fps: Option<f64>,
    /// 瞬时帧率上限（ffprobe 的 r_frame_rate）。上限判定用这个。
    pub nominal_fps: Option<f64>,
}

impl VideoMetadata {
    /// 优先 avg，fallback 到 nominal。用于展示与统计。
    pub fn best_fps(&self) -> Option<f64> {
        self.avg_fps.or(self.nominal_fps)
    }
    /// 帧率上限判定用的帧率：优先瞬时上限（r_frame_rate），回退平均。
    /// 上限判定的所有调用点都必须走这里。
    pub fn cap_fps(&self) -> Option<f64> {
        self.nominal_fps.or(self.avg_fps)
    }
    pub fn is_valid(&self) -> bool {
        self.codec.is_some() || self.avg_fps.is_some() || self.nominal_fps.is_some()
    }
}

// ===== 帧率上限的处理动作 =====
///
/// 判定结果与参数映射同源：判定一次，动作直接决定 argv 与结果记录。
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum FpsDecision {
    /// 未启用上限（max_fps == 0）：不注入任何帧率参数。
    Disabled,
    /// 启用了上限但探测不到源帧率：不注入任何帧率参数
    /// （宁可不降帧，也不用错误的帧率假设覆盖输出）。
    Unknown,
    /// 源帧率未超上限：注入 `-fps_mode passthrough`，保留源时间戳。
    Passthrough,
    /// 源帧率超上限：注入 `-r <target> -fps_mode cfr`，输出转为 CFR。
    Reduce { target: f64 },
}

// ===== 最大帧率配置 =====
/// 只负责数值判断与动作判定。参数映射由 build_ffmpeg_command 处理。
#[derive(Clone, Debug)]
pub struct MaxFpsConfig {
    pub max_fps: f64,
    /// 容差带（对齐 encode.zsh 的 MAX_FPS_EPS = 0.01）。
    pub eps: f64,
}

impl MaxFpsConfig {
    pub fn enabled(&self) -> bool {
        self.max_fps > 0.0
    }
    /// 谓词：源帧率是否超过最大帧率（带容差）
    /// 前置条件：调用方已确认 src_fps 有效、max_fps > 0
    pub fn need_reduce(&self, src_fps: f64) -> bool {
        src_fps > self.max_fps + self.eps
    }
    /// 判定一次，得出帧率处理动作。
    /// src_fps 取自 `VideoMetadata::cap_fps()`（瞬时上限优先）。
    pub fn decide(&self, src_fps: Option<f64>) -> FpsDecision {
        if !self.enabled() {
            return FpsDecision::Disabled;
        }
        let Some(src) = src_fps else {
            return FpsDecision::Unknown;
        };
        if self.need_reduce(src) {
            FpsDecision::Reduce { target: self.max_fps }
        } else {
            FpsDecision::Passthrough
        }
    }
}

// ===== FFmpeg 进程输出处理 =====
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ProcessOutput {
    /// 透传到父进程（默认，与脚本一致）
    #[default]
    Inherit,
    /// 通过管道捕获。CLI 当前不用;§3.8 GUI 路径绕开了这个枚举,
    /// 用 `cmd.spawn()` + reader 线程直接消费 stderr。
    Piped,
    /// 丢弃
    Null,
}

// ===== 编码器缓存 =====
/// 不派生 Clone，通过 &EncoderCache 引用传递
#[derive(Debug)]
pub struct EncoderCache {
    pub encoders_output: String,
    /// 通过能力实测的 NVENC 编码器（1 帧试编码成功才算）。
    /// 空 = 本机没有可用的 NVENC 硬件编码。
    pub nvenc_encoders: Vec<String>,
}

impl EncoderCache {
    /// 子串匹配（与 encode.zsh 行为一致）。
    /// 已知限制：若某编码器名恰好是另一编码器名的子串，可能误判；
    /// 目前认为该风险可接受，与脚本保持一致。
    pub fn has_encoder(&self, name: &str) -> bool {
        self.encoders_output.contains(name)
    }
}

// ===== 编码参数 =====
#[derive(Clone, Debug)]
pub struct EncodeParams {
    pub format: VideoFormat,
    pub bitrate_or_crf: u32,
    pub is_crf: bool,
    pub target: Target,
    pub extensions: Vec<String>,
    /// 用户通过 CLI `--` 后的额外 ffmpeg 参数,或 config `[encode] extra_args`。
    /// 元素类型 [`ExtraArg`](§3.x.y) 是 parser 校验过的:非法值
    /// 在 parse 阶段就被拒,运行时无法塞进未校验的 raw `String`。
    pub extra_args: Vec<ExtraArg>,
    pub max_fps: MaxFpsConfig,
    /// 用户是否传了 --nv
    pub enable_nv: bool,
}

#[derive(Clone, Debug)]
pub enum Target {
    File(PathBuf),
    Dir(PathBuf),
}

// ===== 单个转码任务 =====
#[derive(Clone, Debug)]
pub struct TranscodeTask {
    /// 输入路径,经过 [`InputPath`](§3.3.8) 验证——dash 前缀文件名在
    /// 构造时就被拒,ffmpeg 不会把它当 option 解析
    pub input: InputPath,
    pub output: PathBuf,
}

// ===== 转码结果状态 =====
#[derive(Debug)]
pub enum TranscodeResult {
    Success,
    SkippedSameCodec,
    SkippedExists,
    ReducedFps,
    Failed(EncodeError),
}

/// Canonical name for `TranscodeResult`（U18 重命名以表达"outcome 包含跳过"）。
/// 在 §3.8 / 新代码中优先使用 `EncodingOutcome`；`TranscodeResult` 保留作
/// backwards-compat alias。
///
/// `Skipped` 是**只读历史**：现在没有"跳过"这个概念（§3.3.4），
/// 它只为读旧 Parquet 保留，没有产生者。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EncodingOutcome {
    Succeeded,
    ReducedFps,
    Skipped(SkippedReason),
    Failed,
}

/// `#[non_exhaustive]`（U21）：未来 variant 不破坏现有 exhaustive match。
/// `PartialOutput` 曾用于"partial-output 检测成功但删除失败"(罕见)的场景;
/// happy path 是"detect → remove → reencode"——产生正常 encode record,不是 Skipped。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum SkippedReason {
    SameCodec,
    OutputExists,
    /// Output 存在且检测为 partial(截断 / 损坏),但删除失败
    /// (如权限拒绝)。encode 无法继续(output 在那挡着)。
    PartialOutput,
}

/// Fatal / per-file 失败分类（§3.8 `Event::Error` 和 `EncodingRecord.error` 都引用）。
/// 完整定义在 `types.rs`，此处列出以让 §3.2 自完备。
///
/// `#[non_exhaustive]`（U21）：与 `Command` / `Event` 对齐（见 §3.8.3 item 33）。
/// 新增 variant 不破坏 `Event::Error { category, .. }` 的 exhaustive match。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum ErrorCategory {
    ProbeFailed,
    EncoderSelectionFailed,
    FfmpegSpawnFailed,
    FfmpegNonzeroExit,
    ConfigError,
    Cancelled,
    IoError,
}

/// `#[non_exhaustive]`（U21）：外部 crate（如 `rencoder_ui`）不能直接构造本类型，
/// 但仍可读 `EncodingRecord.error` 携带的字段值。与 `Command` / `Event` /
/// `ErrorCategory` 对齐（见 §3.8.3 item 33 / §8 item 36）。
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct ErrorContext {
    pub category: ErrorCategory,
    pub message: String,
    pub ffmpeg_stderr_tail: String,
}

/// 单文件转码的工作记录（4 维度：source / process / result / status）。
/// 通过 `transcode_batch` 产出，写入 `<app_dir>/data/encoding-*.parquet`（CLI 路径）
/// 或经 `Event::FileCompleted` 推送（GUI 路径，§3.8）。
#[derive(Clone, Debug)]
pub struct EncodingRecord {
    // 1. Source
    pub source_path: String,
    pub source_size_bytes: u64,
    pub source_codec: Option<String>,
    pub source_container: Option<String>,
    pub source_width: Option<u32>,
    pub source_height: Option<u32>,
    pub source_fps: Option<f64>,
    pub source_duration_secs: Option<f64>,
    pub source_bit_rate_bps: Option<u64>,

    // 2. Process
    pub started_at: chrono::DateTime<chrono::Utc>,
    pub ended_at: Option<chrono::DateTime<chrono::Utc>>,
    pub encoding_duration_secs: Option<f64>,
    pub encoder_selected: Option<String>,
    pub encoder_settings: EncoderSettings,
    pub ffmpeg_argv: Vec<String>,
    pub ffmpeg_stderr_tail: String,

    // 3. Result
    pub output_path: Option<String>,
    pub output_size_bytes: Option<u64>,
    pub output_duration_secs: Option<f64>,
    pub output_codec: Option<String>,
    pub compression_ratio: Option<f64>,

    // 4. Status
    pub outcome: EncodingOutcome,
    pub error: Option<ErrorContext>,
}

#[derive(Clone, Debug, Default)]
pub struct EncoderSettings {
    pub is_crf: bool,
    pub crf_or_bitrate: u32,
    pub preset: Option<String>,
    pub max_fps_target: Option<f32>,
}

// ===== 帧率统计结果（视频分析模块） =====
#[derive(Clone, Debug, Default)]
pub struct VideoStats {
    pub groups: std::collections::BTreeMap<u32, Vec<PathBuf>>,
    pub total_files: usize,
    pub parsed_files: usize,
    pub failed_files: usize,
}

impl VideoStats {
    pub fn new(total_files: usize) -> Self {
        Self { total_files, ..Default::default() }
    }
    pub fn add(&mut self, fps: u32, path: PathBuf) {
        self.groups.entry(fps).or_default().push(path);
        self.parsed_files += 1;
    }
    pub fn add_failed(&mut self) {
        self.failed_files += 1;
    }
    pub fn summary(&self) -> String { /* ... */ }
    pub fn detailed(&self) -> String { /* ... */ }
}

// ===== 分析配置 =====
#[derive(Clone, Debug)]
pub struct AnalyzeParams {
    pub target: Target,
    pub extensions: Vec<String>,
    pub parallel: bool,
    pub verbose: bool,
    pub output_file: PathBuf,
}
```

---

### 3.3 核心库 API

#### 元数据探测

```rust
/// 一次 ffprobe 调用，获取 codec + avg_frame_rate + r_frame_rate
/// 对齐 encode.zsh 的 probe_video_metadata()
pub fn probe_video_metadata(input: &Path) -> Result<VideoMetadata, EncodeError>;
```

#### 帧率解析

```rust
/// 支持：分数 "30000/1001"、整数 "25"、小数 "29.97"
/// 对齐 encode.zsh 的 parse_fps_value()
pub fn parse_fps_value(raw: &str) -> Result<f64, EncodeError>;
```

#### 文件收集

```rust
/// 仅使用 walkdir，不依赖外部工具。
/// 不跟随符号链接（避免循环链接）。
pub fn collect_files(
    target: &Target,
    extensions: &[String],
) -> Result<Vec<PathBuf>, EncodeError>;
```

#### 编码器缓存与选择

```rust
/// 初始化编码器缓存（一次，每进程）
/// 能力实测**不受 `nv` 开关影响**（§3.10.5）：能力是机器事实，总是形成。
/// 对 ffmpeg -encoders 里出现的每个 NVENC 编码器跑一次 1 帧试编码，成功即"这台机器能用"。
/// 结果进程内缓存，同进程内多次批次复用；`nv` 只在**选参数**时与能力结果合并判断。
/// 探测失败 / 不可用一律降级到软件编码（warn，不作为 EncodeError 传播）。
/// 对齐 encode.zsh 的 init_encoder_cache()
pub fn init_encoder_cache() -> Result<EncoderCache, EncodeError>;

/// 选择编码器
/// 优先级：配置覆盖 > 可用的 NVENC 变体 > 软件默认。
/// NVENC 变体只有在该编码器通过能力实测时才可用；不可用则回退软件
/// 并 warn（带探测时的 stderr 原文）。
pub fn select_encoder(
    format: VideoFormat,
    cache: &EncoderCache,
    config_path: Option<&Path>,
) -> Result<String, EncodeError>;
```

#### 命令构建

```rust
/// 构建 FFmpeg 命令。
///
/// **重要**：帧率参数（`-r` / `-fps_mode`）是**输出选项**，必须放在
/// `-i <input>` 之后、输出文件路径之前。放在 `-i` 之前会强制解释输入帧率，
/// 导致音画不同步。
///
/// 帧率参数由 `MaxFpsConfig::decide` 的动作决定（src_fps 取自
/// `VideoMetadata::cap_fps()`，即瞬时帧率上限）：
///
/// | 动作 | 注入的参数 | 语义 |
/// |---|---|---|
/// | `Disabled` | 无 | 未启用上限 |
/// | `Unknown` | 无 | 探测不到源帧率，不做任何假设 |
/// | `Passthrough` | `-fps_mode passthrough` | 未超上限：保留源时间戳，不复制不丢帧 |
/// | `Reduce { target }` | `-r <target> -fps_mode cfr` | 超上限：输出转 CFR，容器帧率标签同步降为 target |
///
/// `src_fps` 为 `None`（探测未能获取帧率）时，直接忽略 `MaxFpsConfig`，
/// 不注入任何帧率参数——宁可不降帧，也不能用错误的帧率假设覆盖输出。
///
/// 示例（MAX_FPS=30，源 60fps 超限）：
/// ```text
/// ffmpeg -y -i input.mp4 \
///   -c:v libsvtav1 -preset 6 -crf 45 \
///   -c:a libopus -b:a 32k \
///   -r 30 -fps_mode cfr \
///   output.mkv
/// ```
///
/// 示例（MAX_FPS=30，源 25fps 未超限）：
/// ```text
/// ffmpeg -y -i input.mp4 \
///   -c:v libsvtav1 -preset 6 -crf 45 \
///   -c:a libopus -b:a 32k \
///   -fps_mode passthrough \
///   output.mkv
/// ```
pub fn build_ffmpeg_command(
    task: &TranscodeTask,
    params: &EncodeParams,
    encoder: &str,
    src_fps: Option<f64>,
) -> Vec<String>;
```

#### ffmpeg argv 安全边界（类型级）

`EncodeParams.extra_args` 接收用户控制值（CLI `--` 后的 passthrough、
`rencoder.toml` `[encode] extra_args`、未来其他来源），由
`build_ffmpeg_command` 拼到 argv 末尾——这给了用户一个**任意字符串插入
ffmpeg argv 的口子**,必须严格守边界。

**以前的做法**:运行期 `is_safe_extra_arg` 跑 `chars()` 检查 + 一个
`validate_extra_args()` 包装器。问题是**runtime check 可被遗忘**:
任何忘了调 `validate_extra_args()` 的代码路径都能塞 unsafe 值进去,
仅靠 lint / 测试兜底。

**新做法（类型级边界,review #15 升级 + Option B 落地）**:
`extra_args: Vec<ExtraArg>`,`ExtraArg` 是带 `#[non_exhaustive]` 的
enum + validated factory,只有 `ExtraArg::parse(&str)` 能构造。
`String` 无法直接 push 进去——编译器拒绝非法构造。

```rust
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ExtraArg {
    /// 纯 flag,例 `-y`、`-vf`、`-metadata`、`-ss`
    Flag(String),
    /// `flag=value` 形式,flag 和 value 都过 char-class 校验
    FlagEqValue { flag: String, value: String },
    /// 裸 value(前一 flag 的参数),例 `-metadata` 后的 `title=foo`
    BareValue(String),
}

impl ExtraArg {
    /// Parse + validate 一条 argv 字符串。失败原因(空、长选项、
    /// 非法字符、shell 元字符、命中 deny-list)通过错误消息返回,
    /// 调用方(CLI 解析、TOML 反序列化)决定怎么 surface 给上游。
    pub fn parse(s: &str) -> Result<Self, ExtraArgParseError>;
}
```

**字符类**(集中放 `ffmpeg_arg::chars`,validator 和未来其他
argv 处理代码共用):

- `is_flag_char(c)`:`ASCII alphanumeric + '_'`
- `is_value_char(c)`:`is_flag_char` + `. : / - _ @ =`
- **隐式禁止**:空格、`;`、`&`、`|`、`>`、`<`、`"`、`'`、`$`、
  `` ` ``、`\n`、`\t`——任一出现 → parse 失败。

**Deny-list**(`ExtraArg::parse` 强制):

- `i` — 重新注入输入
- `y` — `build_ffmpeg_command` 永远硬编码,不允许用户关
- `c` — 与硬编码 codec 冲突(`-c:v` / `-c:a` 也命中)
- `o` — 重新注入输出路径
- `vn` / `hide_banner` — 与硬编码选项冲突

**拒绝的形态**(同 review #15 的具体规则):

- 长选项 `--foo`——allowlist 太散
- `flag=value` 里 value 含 shell 元字符 / 路径里的 `;`
- flag 后面跟路径字符但没 `=`(例 `-vf_negate.mp4`,可能
  被 ffmpeg 解析成 `-vf negate.mp4`)
- 裸 value 不含 `=`(例 `preset`、`5`)——ffmpeg 可能当 input
  path 或 filter arg,意图在 wrapper 层不可判定

**与 `build_ffmpeg_command` 的接口**:
消费者按 enum 分支展开成 `String`,无需再做运行时校验:

```rust
for arg in &params.extra_args {
    match arg {
        ExtraArg::Flag(f)         => argv.push(f.clone()),
        ExtraArg::FlagEqValue{flag,value} => argv.push(format!("{flag}={value}")),
        ExtraArg::BareValue(v)    => argv.push(v.clone()),
    }
}
```

**配套改动**:

- `EncodeParams::validate_extra_args` **删除**;parse 阶段已经守边界
- CLI 解析阶段(`rencoder_cli::args`)调 `ExtraArg::parse`,
  错误消息直接 surface 给用户
- 未来的 TOML `[encode] extra_args` 字段用 `Vec<ExtraArg>`
  反序列化,serde 自动跑 parse,非法 config 在 load 阶段就 fail

#### 输入路径安全边界（类型级,review #109 收尾）

跟 `ExtraArg` 同样的策略——把路径合法性提到类型层。

**问题**:ffmpeg 把 argv 当作选项解析。如果 input 路径以
`-` 开头（例如 `-vf_negate.mp4`、`.config/-secret`）,
ffmpeg 可能解释成选项（`-vf negate.mp4`）或报错。`collect.rs`
已经在收集阶段过滤掉 dash 前缀的文件（review #1 partial）,
但**边界是 collect_dir 内部**,`TranscodeTask.input: PathBuf`
仍然是 raw 类型——任何未来 caller（CLI 直接 `-f`、测试、
其他 source）只要绕过 collect 就能塞 unsafe 路径。

**新做法**:`TranscodeTask.input: InputPath`,`InputPath` 是
newtype + validated factory,只有 `InputPath::new(&Path)`
能构造,工厂里 reject dash 前缀 / 空字符串。

```rust
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
#[repr(transparent)]
pub struct InputPath(String);

impl InputPath {
    /// Validate + wrap. Rejects:
    ///   - empty path
    ///   - filename starting with `-` (ffmpeg would parse as option)
    ///   - any other form that `build_ffmpeg_command` could mis-parse
    pub fn new(p: &Path) -> Result<Self, InputPathError>;
    pub fn as_path(&self) -> &Path;
}
```

**配套改动**:

- `collect_files` 返回 `Vec<Result<InputPath, InputPathError>>` 或
  `Vec<InputPath>`（前者把校验错误传给 caller 决定 surface 方式;
  v1 用后者,先 warn + skip 跟现有行为对齐）
- CLI `-f` 路径走 `InputPath::new` 校验,失败时 bail
- `build_ffmpeg_command` 拿 `&InputPath`,转成 `&Path` 拼 argv
- 删除 `collect_dir` 里的 dash-prefix 检查分支(类型层已经兜底)

**为什么不让 OutputPath 也走同样路径**:`output` 是 wrapper 写
出去的目标,ffmpeg 不会把它当 option 解析(它在 argv 末尾,位置
上是 output file)。加同款保护只是过度防御;保留 `Option<PathBuf>`
就够了。

#### 同路径保护

输出路径指向输入文件会**直接销毁用户的源文件**;更糟的是取消清理按
`argv.last()` 删除半成品,输出等于输入时删掉的是输入本身——结构性失败。

三层防护,层层独立:

1. **命名避让**(`output_path_for`):默认扩展名与输入扩展名相同时加
   `_encoded` 后缀(既有行为)。
2. **同一文件判定**(`utils::fs::is_same_file`):规范化父目录 + 文件名
   大小写不敏感比较。macOS / Windows 默认文件系统大小写不敏感,
   `movie.MKV` 与 `movie.mkv` 是同一个文件,字符串比较发现不了;
   ffmpeg 的"输入输出同路径"保护也是字符串比较,同样发现不了。
3. **转码前拦截**(`encode_prepare`):判定输入与输出是同一文件时,该文件
   记 `Failed`(错误 `Invalid parameter: input and output paths must differ`),
   **不调用 ffmpeg**。按 per-file 失败模型处理,批次不中断,其余文件正常转码。

第 3 层是兜底:即使将来新增了一条生成输出路径的路径(用户指定输出名、
新的容器映射),也不会绕过前两层。

#### 转码：批里的每个文件都会被转码

**行为**：每次运行，批里的**每个文件都会被转码**。输出文件已存在 → 直接覆盖写
（ffmpeg 的 `-y`）。

这里**没有"跳过 / 重编"这套判断**：不比较源编码、不比较配置、不给产物写指纹。
少一个概念就少一处能自相矛盾的地方——多一套判断，就多一个"这套判断自己跟自己
打架"的机会。

| 情况 | 结果 |
|---|---|
| 输出已存在 | 照常转码（覆盖） |
| 源编码已匹配目标格式 | 照常转码（覆盖） |
| 源编码匹配 **且**需要降帧 | 转码，输出标记 `ReducedFps` |
| 输入与输出是同一个文件 | 该文件记 `Failed`（硬错误，不转码） |

最后一条是**防呆**，不是另一套判断：输入和输出指向同一个文件时 ffmpeg 会把源文件
截断，所以这一条必须挡住。

**配置读几次**：**每进程一次**。整个批次共用这一份快照（`BatchContext`），
**不按文件重读**——按文件重读既慢又会让同一批里的文件用不同配置。

#### 单文件转码

```rust
/// 内部流程（对齐 encode.zsh 的 encode_single_file + partial output 重转）：
///   0. 同路径检查：输出与输入指向同一个文件?→ 该文件记 Failed（不调 ffmpeg）
///   1. probe_video_metadata（一次 ffprobe,源）→ 拿 duration
///   2. MaxFpsConfig::decide（帧率动作判定，源帧率取 cap_fps）
///   3. select_encoder（缓存）
///   4. build_ffmpeg_command（帧率参数映射）
///   5. 执行（ProcessOutput 控制输出）——输出已存在也照常写（覆盖）
pub fn transcode_single(
    task: &TranscodeTask,
    params: &EncodeParams,
    encoder_cache: &EncoderCache,
    output: ProcessOutput,
) -> Result<TranscodeResult, EncodeError>;
```

**Partial output 检测**：**已不需要**。

输出已存在就照常转码（覆盖）——半成品会被直接覆盖，检测没有意义，所以检测本身
已移除（§3.3.4 ✓）。

**事后清理**不变：ffmpeg exit 0 但 output 缺失 / 0 字节 ✓，且 source 有 known
duration ✓ → 立即 `remove_file` 并记录 `Failed` ✓（review #114 ✓）。不留 0 字节
ghost 等下一轮兜底 ✓。
- **stderr 内存上限**:ffmpeg verbose 跑可能喷出数百 MiB
  stderr,`run_ffmpeg_with_progress` 的 reader 线程有 256 KiB
  上限（`STDERR_READER_MAX_BYTES`）,超出部分丢入
  `[stderr truncated; N bytes dropped]` 注解。reader 持续 drain
  pipe 到 EOF,因此 `stderr_thread.join()` 不会卡住（review
  #112 + #116）。
#### 批量转码

```rust
pub fn transcode_batch(
    params: &EncodeParams,
    files: &[PathBuf],
    encoder_cache: &EncoderCache,
    output: ProcessOutput,
    result_callback: Option<&dyn Fn(&Path, &TranscodeResult)>,
) -> Result<Vec<(PathBuf, TranscodeResult)>, EncodeError>;
```

#### 视频分析

```rust
/// Raw per-file records. The CLI emits these as CSV; downstream tools
/// (DuckDB / Polars) consume the CSV for arbitrary analysis.
pub struct AnalysisRecord {
    pub path: PathBuf,
    pub fps: Option<u32>,
    pub codec: Option<String>,
    pub duration_secs: Option<f64>,
    pub file_bytes: u64,
}

/// Build per-file records by probing each input. Failed probes still
/// produce a row (with `fps`/`codec`/`duration_secs` = None).
pub fn analyze_records(files: &[PathBuf]) -> Result<Vec<AnalysisRecord>, EncodeError>;

/// Aggregate records into `VideoStats`. Internally backed by a Polars
/// DataFrame; the public `VideoStats` is the rendered view.
pub fn aggregate_records(records: &[AnalysisRecord]) -> VideoStats;

/// Render records as CSV (header + rows, with proper quoting).
pub fn records_to_csv(records: &[AnalysisRecord]) -> String;

#[cfg(feature = "parallel-analyze")]
pub fn analyze_records_parallel(files: &[PathBuf]) -> Result<Vec<AnalysisRecord>, EncodeError>;

/// U22 / Tier 3 #9: 进度上报的并行变体。`F` = probe 闭包(同
/// `analyze_records_with_probe_parallel` 的 test seam),`P` = 进度闭包,
/// 在每个 rayon worker 完成自己任务时被调一次(per-file,不是 per-packet)。
///
/// 顺序非确定:rayon worker 完成顺序与输入顺序无关。`P` 收到的 `done` 计数
/// 单调递增;`last` 字段让 closure 知道"刚完成哪个文件"以便发 event。
///
/// 进度闭包不返回错误;如果想做背压(throttle / 缓冲)在 closure 内部做,
/// 不在 lib 接口。
#[cfg(feature = "parallel-analyze")]
pub fn analyze_records_with_probe_and_progress_parallel<F, P>(
    files: &[PathBuf],
    probe: F,
    progress: P,
) -> Vec<AnalysisRecord>;

/// U22: 进度闭包的 payload。`done` / `total` / `failed` 三个 usize +
/// `last: Option<AnalyzedFile>` 携带最近一个文件的结果(成功 path / 失败
/// path + error)。`AnalyzedFile` 是独立 type——不复用 AnalysisRecord,
/// 后者没 `ProbeFailed` outcome 字段,复用会污染 success path。
pub struct AnalyzeProgress {
    pub done: usize,
    pub total: usize,
    pub failed: usize,
    pub last: Option<AnalyzedFile>,
}

pub struct AnalyzedFile {
    pub path: PathBuf,
    pub outcome: FileOutcome,  // Ok | ProbeFailed(EncodeError)
}

pub enum FileOutcome {
    Ok,
    ProbeFailed(EncodeError),
}
```

CLI 输出双轨：
- **CSV to stdout**（默认）：每行一个文件，列 `path, fps, codec, duration_secs, file_bytes`。
- **Summary to stderr**：单行汇总（`Analyzed X files (Y parsed, Z failed)`）。
- `--format text`：人类可读的汇总（旧 `FpsStats::summary()` 行为）。
- `-o FILE`：CSV 写到文件而不是 stdout。

聚合在 Polars 中完成：`group_by(fps).agg([count, sum(duration_secs)])`。失败探测的行有空 `fps`/`codec`/`duration_secs`，保留 `path`/`file_bytes` 便于下游 `WHERE fps IS NULL` 筛选。

**并行分析进度上报**（U22 / Tier 3 #9）：GUI 路径需要在 `analyze_records_parallel` 跑的时候给 UI 推
进度。设计选择见 §3.4 "分析进度上报"——核心是 closure-injection 到 rayon `par_iter()` 层（不是 c-probe
FFI 层），per-file 完成后回调发 `Event::AnalyzeProgress`。

#### C FFI 桥接层（可选特性 `c-probe`）

高性能元数据探测：直接调 libavformat，跳过 `ffprobe` 进程 spawn。**默认关闭**
（opt-in）：默认构建不依赖 libavformat 开发库，任何机器都能直接 `cargo build`，
探测走 `ffprobe` 子进程——这与 §2 决策表里"元数据探测（默认）= `std::process::Command`
调用 `ffprobe`"一致。开发机上想要快路径就显式开启（见下文）。

**Rust 端 API**：

```rust
#[cfg(feature = "c-probe")]
pub fn probe_video_metadata_fast(input: &Path) -> Result<VideoMetadata, EncodeError>;

/// 自动选择探测方式，统一返回 Result。c-probe 启用时优先 FFI，失败 fallback 到 ffprobe CLI。
pub fn probe_video_metadata_auto(input: &Path) -> Result<VideoMetadata, EncodeError>;
```

**线程安全**：FFI 桥 stateless，每次调用自己 `avformat_open_input` / `avformat_close_input`，
不依赖任何全局/静态变量。rayon 中并发调用安全。

**编译配置**（`rencoder_lib/Cargo.toml`）：

```toml
[features]
default = ["parallel-analyze"]           # 默认不开 c-probe：免 libavformat 开发库
c-probe = ["dep:cc", "dep:pkg-config"]   # 需要 cc + pkg-config build-deps
```

**为什么默认关闭**：

- `build.rs` 在 `c-probe` 开启时调 `pkg-config --cflags --libs libavformat`，找不到就直接
  **panic**。把它放在默认 feature 里，意味着"clone 下来 cargo build"这件事需要额外的系统
  包，源码分发与 release 构建都被绑在这一个 host 依赖上。
- release CI 的 runner（ubuntu-22.04 / macos-14 / windows-2022）默认没有 libavformat 的
  开发头文件，默认开启会直接构建失败——发布流水线不该依赖 runner 上手工装 ffmpeg-dev。
- 探测慢的代价可控：单文件 probe 30-100ms 的进程启动开销（§3.4 "分析进度上报" 的讨论里
  已量化），换来的是"任何机器都能构建"。

**开启 c-probe 的场景**：

```bash
# host 装了 libavformat 开发库，想要更快的批量探测
cargo build --features c-probe
# 探测直接调 libavformat，不 spawn ffprobe
```

**依赖（host 系统）**：

| 平台 | 安装方式 |
|---|---|
| macOS | `brew install pkg-config ffmpeg`（ffmpeg 提供 libavformat 的 .pc 文件） |
| Debian/Ubuntu | `sudo apt-get install pkg-config libavformat-dev` |
| Fedora/RHEL | `sudo dnf install pkg-config ffmpeg-devel` |

构建时 `build.rs` 调 `pkg-config --cflags --libs libavformat` 取头文件 + 库路径，
用 `cc` 编译 `src/ffi/video_probe.c` 成静态库 `librencoder_probe.a` 链进
`rencoder_lib`。

**Runtime 检查**：

- c-probe 失败时自动 fallback 到 ffprobe CLI，warn 一行后继续：

  ```
  WARN c-probe failed for /path/in.mp4, falling back to ffprobe CLI: <reason>
  ```

- pkg-config 找不到 libavformat 时 `build.rs` 直接 panic，提示"pkg-config libavformat
  failed: <stderr>"。修复：装 ffmpeg-dev / libavformat-dev 包，或者不开这个 feature。

---

### 3.4 CLI 前端职责

#### 转码模式参数（U17：CLI / config split 后）

```
rencoder encode [选项] [--] [额外FFmpeg参数]

选项（CLI 上只剩 target + action + 少数 per-run 覆盖）：
  -f, --file FILE         指定单个文件
  -d, --dir DIR           指定目录（递归）
  -i, --interactive       启动交互模式
  --config PATH           TOML 配置路径（默认 `<app_dir>/rencoder.toml`；`$RENENCODER_CONFIG` 覆盖）
  -o, --output FILE       详细输出文件路径（默认 `<app_dir>/data/encoding-<UTC-ISO8601>Z.parquet`；`$RENENCODER_DATA_DIR` 覆盖）
  --nv                    强制启用 NVIDIA 硬件编码（覆盖配置项；**不触发**能力实测——能力判定每进程形成一次）
  --no-nv                 强制禁用 NVIDIA 硬件编码（覆盖配置项）
  --                      之后的所有参数作为FFmpeg原生参数透传

剩余的 format / bitrate / max-fps / crf / extensions / encoders 全部在
`rencoder.toml` 的 `[encode]` 段。`nv` 默认也在配置里，但 CLI 可以覆盖
（命令行 > 配置项）——它是最"机器相关"的旋钮。详见 §3.4.5 App-level config +
single-directory app。

**命令行覆盖怎么合并**：**当前是逐字段 mutate** ✓——CLI 读完 config 后直接改
`params.nv` ✓ / `target` ✓ / `extra_args` ✓。"默认 ← 文件 ← 命令行"的合并收在一处
是 §3.10.5 的 **future** 设计 ✓（现在优先级是巧合而不是规则 ✓）。
```

#### 全局选项与 detect 子命令

```
rencoder [全局选项] <子命令> [子命令选项]

全局选项（子命令前后都可以写）：
  --detect                  强制重新检测 NVENC 能力并更新本地结果

子命令：
  encode                    转码
  analyze                   分析帧率分布
  detect                    探测 NVENC 能力：写入本地结果并打印报告
```

- `--detect` 是**全局**选项：`rencoder --detect encode ...` 与
  `rencoder encode --detect ...` 等价。
- `rencoder detect` 独立使用：不依赖任何目标文件，只做探测 + 存盘 + 报告。
- 检测结果落在 `<app_dir>/nvenc-capability.json`（应用目录根部）。普通运行时
  优先读它；没有、指纹对不上、或者用户给了 `--detect`，才现场探测。
  超过 30 天（`next_review_at`）只**提示**用户是否重新检测，用户选 yes 才重测，
  选 no 就把 `next_review_at` 往后推 30 天（§2.2）。

#### 分析模式参数（U17：CLI / config split 后）

```
rencoder analyze [选项]

选项：
  -d, --dir DIR           指定目录（递归），默认当前目录
  -f, --file FILE         指定单个文件
  --config PATH           TOML 配置路径（默认 `<app_dir>/rencoder.toml`；`$RENENCODER_CONFIG` 覆盖）
  -o, --output FILE       详细输出文件路径（默认 - 即 stdout；`-o file.parquet` 时写入 `<app_dir>/data/`）

`format` / `parallel` / `extensions` 在 `rencoder.toml` 的 `[analyze]` 段。
```

#### 交互模式

任务摘要新增最大帧率和硬件加速显示：

```
任务摘要
---------------------------------------------
目标格式: av1
CRF: 45
目标: . (dir)
扩展名过滤: mp4, avi
待处理文件数: 128
最大帧率: 30
NVIDIA 加速: 未启用
=============================================
```

#### Encode summary 输出（U21 partial-output 修正）

CLI 在 `rencoder encode` 批结束后打印 6 行 summary 到 stdout：

```
Summary:
  Total:      N
  Success:    X
  Reduced:    Y    (FPS-reduced)
  Failed:     F
```

（2026-11-15 起不再有 `Skipped` / `PartialSkip` 两类：批里的每个文件都转码，§3.3.4。
`SkippedReason` 的枚举值仍在记录 schema 里，但不再产生。）

**（历史）为什么 PartialSkip 曾是独立分类**：partial 输出留在磁盘上、partial 检测
又删不掉时（权限拒绝 / 文件被占用），需要一个独立分类让用户在 summary 里看到它。
partial 检测本身已移除（§3.3.4），这个分类也不再产生；`SkippedReason`
的枚举值保留在记录 schema 里（历史数据可能带它），但新记录不会有。

#### 信号处理（U18 + U18.1）

**意图**：用户中断转码时，handler 读取当前正在写入的 output path，删除半成品文件，避免留下
损坏的 `.mkv`/`.mp4`。这跟 `encode.zsh` 里的 `trap _cleanup INT TERM HUP` 行为对应。

**信号覆盖**：

| 平台 | 覆盖 | 实现 |
|------|------|------|
| Unix（macOS / Linux） | `SIGINT` / `SIGTERM` / `SIGHUP` | `signal-hook` 的 `Signals::forever()` |
| Windows | `CTRL_C` / `CTRL_BREAK` / `CTRL_CLOSE` / `CTRL_LOGOFF` / `CTRL_SHUTDOWN` | `windows-sys` 的 `SetConsoleCtrlHandler` |

**为什么 2026-09 的 `ctrlc` 方案被换掉**：`ctrlc` 一个 crate 覆盖两个平台、源码零
platform-specific 调用，代价是 Unix 侧只剩 `SIGINT`——`kill <pid>`（SIGTERM，容器编排器
的默认停止方式）与 SSH 断连（SIGHUP）直接杀进程，半成品留在磁盘上。现在的方案两边都覆盖
完整集合，platform divergence 关在 `rencoder_cli::signal` 一个文件里（§1.3 的
"cfg at definition, uniform signature at call"）。

**已知限制**：Windows 的 console handler 路径无法在 macOS / Linux 上验证（没有 Windows
runner 参与日常开发）；`windows-sys` 只在 `cfg(windows)` 下编译，Unix 侧完全不感知这个依赖。

**Cross-platform story**：`install_handler() -> CurrentOutput` 的签名跨平台一致，调用方
（`main.rs`）零 cfg。`Arc<Mutex<TaskContext>>` + `std::fs::remove_file` +
`std::process::exit` + `tracing::warn!` 都是 stdlib 跨平台 API，两侧共用同一段清理逻辑
（`cleanup_and_exit`）。GUI build（§3.8）**不复用** `rencoder_cli::signal`（webview 有自己的
beforeunload / IPC 机制），会单独写。

**完整 lifecycle**（U18 wiring fix + U18.1 TaskContext 增强）：

1. **install**（`main` 启动时一次）：
   - `signal::install_handler()` → 装 SIGINT/SIGTERM/SIGHUP listener + 返回 `CurrentOutput` handle
2. **register per file**（`TaskManager::run` 每个 task ffmpeg 调用前）：
   - `partial_output::Guard::new(handle, task_ctx)` → RAII guard 写入 `TaskContext`
3. **clear after ffmpeg**（guard drop 时自动）：
   - `partial_output::clear(handle)` → 清空（成功时避免下一次 SIGINT 误删好文件；失败时
     下一次 register 会覆盖当前 path）
4. **SIGINT/SIGTERM/SIGHUP**（用户中断）：
   - listener `take()` 当前 `TaskContext` → emit task-level scaffold `    task N/M interrupt`
   - `fs::remove_file(ctx.output)`（analyze path 无 output，跳过）
   - emit batch-level scaffold `{label} interrupt at {path}, M files successed` (M = ctx.index - 1)
   - `exit(1)`

**U19 scaffold log**（batch + task 两层，每层只 log 自己 level 的 knowledge）：

| Level | 正常结束 | 中断 |
|---|---|---|
| Batch | `{label}, end at {path}, {N} files successed` | `{label} interrupt at {path}, {M} files successed` |
| Task | `    task N/M end` | `    task N/M interrupt` |
| Batch open | `{label} start at {path}, {N} files` | (same) |

每层只 log 自己的 knowledge：batch 知道 path / total / completed count；task 知道 index / phase。
跨层 leak 会让 post-mortem 工具 regex 复杂化，所以严格分层。

完整示例（encode 5 个文件，跑到第 3 个时 SIGINT）：
```
encoding start at /data/in, 5 files
    task 1/5 start
    task 1/5 end
    task 2/5 start
    task 2/5 end
    task 3/5 start
    task 3/5 interrupt              <- signal handler
encoding interrupt at /data/in, 2 files successed   <- signal handler
```

**Shared type**（U19：TaskContext 搬到 `pipeline::task`，`partial_output` re-export 保持 import path 兼容）：

```rust
// rencoder_lib/src/pipeline/task.rs
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TaskContext {
    pub index: usize,            // 1-based; signal handler 用 index - 1 算 completed
    pub total: usize,
    pub batch_label: String,     // "encoding" / "analyze" -- 给 batch-level scaffold 用
    pub batch_path: PathBuf,     // batch target -- 给 batch-level scaffold 用
    pub source: PathBuf,         // 当前文件路径（保留给未来扩展；scaffold 不显示）
    pub output: Option<PathBuf>, // None for analyze（read-only）
}

pub type PartialOutputHandle = Arc<Mutex<Option<TaskContext>>>;
pub struct Guard<'a> { handle: Option<&'a PartialOutputHandle> }
impl<'a> Guard<'a> { pub fn new(handle: ..., ctx: TaskContext) -> Self; }
```

**Why the type lives in lib, not CLI**：lib 的 `transcode_batch` 需要接受 `current_output`
参数（不能依赖 CLI 的 signal module，否则 lib 反向依赖 CLI）。CLI 的 `signal.rs` 只
保留 `install_handler()` + `ctrlc` 依赖，type 重新声明为 `pub type CurrentOutput =
rencoder_lib::pipeline::partial_output::PartialOutputHandle;`。

**Handler 安装**（CLI `signal.rs`）：

逻辑骨架：listener 线程在 `Signals::forever()` 上阻塞，每次信号到达：

1. `signal_output.lock().take()` 拿到当前 `TaskContext`（如果没在 batch 中就跳过 cleanup）
2. emit task-level scaffold `    task N/M interrupt`
3. `fs::remove_file(ctx.output)`（analyze path 的 output = None，跳过）
4. emit batch-level scaffold `{label} interrupt at {path}, M files successed` (M = ctx.index - 1)
5. `process::exit(1)`

完整代码见 `rencoder_cli/src/signal.rs`（70 行左右）。

**Log via tracing, not eprintln**（U18.1）：信号事件用 `tracing::warn!` 而非 `eprintln!`，
事件经 global subscriber 同时落到 stderr 和滚动 log file。`process::exit(1)` 跳析构但
`fmt::Layer` 同步写 FileSink（无内部 buffer），kernel 在进程 teardown 时刷 page cache --
日志行在 exit 之前已经落盘。

**Batch integration**（lib `pipeline/batch.rs`，per-task scaffolding 见 §3.4.6
TaskManager 提取）：

```rust
pub fn transcode_batch(
    params: &EncodeParams,
    files: &[PathBuf],
    cache: &EncoderCache,
    config: &Config,
    output: ProcessOutput,
    current_output: Option<&PartialOutputHandle>,
    result_callback: Option<&ResultCallback>,
) -> Vec<EncodingRecord> {
    let total = files.len();
    (0..total).map(|idx| { ... })
        .map(|idx| {
            let output_path = output_path_for(input, params.format);
            let task = TranscodeTask { input, output: output_path };
            let ctx = TaskContext {
                index: idx + 1, total,
                source: task.input.clone(),
                output: task.output.clone(),
            };
            transcode_single(task, params, cache, config, output, current_output, &ctx)
        })
        .collect()
}
```

**Bug 修史**（U18 audit 发现）：`signal.rs` 早期实现了 install + clear + register + remove 四个
pub fn，但 `register_partial` / `clear_partial` 只在 tests 里被调用，production 代码
只调了 `install_handler` —— `current_output` 永远是 `None`，SIGINT handler 读到 `None` 啥也不做，
**比 zsh 脚本的行为还差**（zsh 的 `trap _cleanup` 至少能删半成品）。修法是上述 lifecycle wiring。

**U18.1 增强**：把 `Option<PathBuf>` 升级为 `Option<TaskContext>`，让 signal handler 能
log "task N/M (source) interrupted" 而不是光秃秃的 path。同步把 `eprintln!` 换成
`tracing::warn!`，让 interrupt 事件进 log file。

#### App-level config + single-directory app（U18）

**设计意图**：CLI 只承载「per-run」语义（目标文件 / 目录 + 一次性 override）；所有
「per-app」的偏好（格式、码率、最大帧率、CRF、NV、扩展名、编码器覆盖、分析子命令的
格式/并行/扩展名）都在应用专属位置的 `rencoder.toml` 里。应用以**单目录**方式交付
与部署——binary、config、log、data 全部在 `<app_dir>` 下，不依赖 CWD 或系统路径。

**架构契约**：
- **Source 仍是 workspace**（`rencoder_lib` + `rencoder_cli` + `rencoder_ui`，§3.8）。
  `rencoder_lib` 加 `publish = false`——明确"内部 lib，不要
  `cargo publish`"。
- **lib 完成核心工作**：业务逻辑、配置加载、FFmpeg 命令构造全部在 `rencoder_lib`；
  `rencoder_cli`（及未来的 GUI / Tauri）是 thin wrapper，只负责 args 解析 + 编排。
  CLI / GUI / Tauri 共享同一份 `rencoder.toml`，UX 一致。
- **运行时是单目录应用**：binary 自己 resolve 出父目录（`current_exe().parent()`），
  所有持久化状态（config / log / data）从 `<app_dir>` 派生。U17 的 `./rencoder.toml`
  / `./log/` / `./data/` CWD 相对路径不再是默认行为。
- **dist 镜像运行时布局**：`scripts/dist.sh` 把 `rencoder_cli` 和
  `rencoder_ui` 两份 target 产物都打成 tarball，各自配 LICENSE +
  README + config 模板拷进 `dist/<app>-<triple>/`，形成 ready-to-run
  单目录应用。解压后直接 `./rencoder ...`（CLI）或 `./rencoder-ui`
  （UI），无需 mkdir 或额外配置。两个 tarball 是独立的（每份带自己
  的 `rencoder.toml` + `log/` + `data/`）—— 用户挑一个装；不合并
  成单 tarball 是因为"一个 app_dir 一个 binary"是 U18 的部署形状
  约束（docs/smart_code.md §3.6），两份 binary 共享状态是另一种
  部署模式，不在 U18 范围。CLI 与 UI 二进制都可独立跑。
  - `rencoder_ui` 需要在 `rencoder_ui/Cargo.toml` 显式
    `[package.metadata.dist] dist = true`（U21: `publish = false`，
    cargo-dist 0.32 默认跳过 `publish = false` 的 package）。

**配置文件路径解析顺序**（`config::paths::config_file`）：
1. `$RENENCODER_CONFIG`（最高优先级）—— 显式覆盖，read-only fs / 调试用
2. `<app_dir>/rencoder.toml`（默认）—— `current_exe().parent().join("rencoder.toml")`

**log / data 路径解析**（同理）：
- `$RENENCODER_LOG_DIR` > `<app_dir>/log/`
- `$RENENCODER_DATA_DIR` > `<app_dir>/data/`

**Auto-create 行为**（`AppConfig::load`）：
- 路径不存在 → 写入默认 `rencoder.toml`（所有字段 = `Default::default()`，每个字段
  有 inline 注释），atomic write（tmp + rename），stderr 日志提示
- 路径存在但 parse / validate 失败 → 报错（**绝不覆盖**用户已有文件）

**schema**：见 `rencoder_lib::config::app::AppConfig` 和 `examples/rencoder.toml.example`
（dev-only 参考，dist 用户无需手动拷贝）。schema 校验在 `AppConfig::load`；文件读出来的
`AppConfig` 进 `EncodeParams::from_app_config` ✓。"默认 ← 文件 ← 命令行"的合并收在
一处是 §3.10.5 的 **future** ✓。

**CRF 字段**（U18）：`crf: u32`，默认 `45`（libsvtav1 中等质量），`0` = 退回
bitrate 模式（用 `bitrate` 字段的 Kbps 值）。范围 `0..=63`（libsvtav1 max=63，
x264 max=51）。这跟 ffmpeg 的 `-crf N` 1:1 对齐，避免 U17 那种"bool 切换 + bitrate
字段复用为 crf 值"的隐式 context-dependent typing。

---

#### TaskManager — pipeline 任务编排（U19，已实现）

**意图**：把 batch 循环里的"任务管理"关注点（index/total 编号、batch/task 两层 lifecycle log、
signal-handler Guard 注册、结果聚合）从 encode / analyze 的领域逻辑里抽出来，让
`transcode_single` 和 `analyze_records_with_probe` 只关心自己的领域工作（探测 + 转码，或
探测 + 解析），不再混合"我在 batch 里是第几个 / 我要在 log 里写什么 / 我要把自己注册给
signal handler"。

**SRP 三层切分**：

| 关注点 | U18.1 散落位置 | U19 归属 |
|---|---|---|
| 任务编号、lifecycle log、Guard 注册、结果聚合 | `transcode_batch_with_processor` 内联 | `pipeline::task::TaskManager::run` |
| 探测、argv 构造、ffmpeg 执行 | `transcode_single` | `transcode_single`（不变） |
| signal 处理（listener thread + cleanup） | `cli::signal::install_handler` | 不变 |
| analyze 的 probe | `analyze_records_with_probe` | 不变（drop log helpers） |

**模块 `rencoder_lib/src/pipeline/task.rs`**（U19 新增）：

- `TaskContext`（带 `batch_label` + `batch_path`）：signal handler 用 `batch_label`/`batch_path`
  拼 batch-level scaffold line；用 `ctx.index - 1` 算 completed count，不需要跨线程共享 counter
- `TaskManager::run(tasks, label, batch_path, processor)`：emit batch open + per-task start/end
  + batch close（或中断时由 signal handler emit batch-level interrupt line）

**`TranscodeTask.output` 改为 `Option<PathBuf>`**：None for analyze（read-only），Some
for encode。signal handler 看到 None 就跳过 `fs::remove_file`。

**`transcode_single` 简化**（U19 step 3）：去掉 `current_output: Option<&PartialOutputHandle>`
和 `task_ctx: &TaskContext` 两个参数以及 4 个 `log_task_*` helper。变成纯 encode：
`task + params + cache + config → record`。

**`analyze_records_with_probe` 简化**（U19 step 5）：去掉 3 个 `log_analyze_*` helper。
变成纯 probe。rayon 并行版本（`analyze_records_parallel`）继续走自己的 log 路径，因为
worker 池无顺序保证。

**调用链**（encode）：

```
main::encode_cmd
  → transcode_batch               # thin wrapper（U19 step 4）
      → TaskManager::run(tasks, "encoding", target_path, |task, ctx| transcode_single(task, params, ...))
```

**调用链**（analyze, serial）：

```
main::analyze_cmd
  → TaskManager::run(tasks, "analyze", target_path, |task, ctx| probe_one(task))
  → aggregate_records
```

**Log 格式**（span-style，batch + task 两层各 log 自己 level 的 knowledge）：

```
{label} start at {batch_path}, {N} files
    task 1/{N} start
    task 1/{N} end
    ...
{label}, end at {batch_path}, {N} files successed
```

每层只 log 自己的 knowledge：batch 知道 path / total / completed count；task 知道
index / phase。signal handler emit 中断的 batch-level summary 时用 `ctx.index - 1` 算
completed，不需要跨线程共享 counter。完整示例见 §3.4.5。

**Test 收益**：
- `TaskManager::run` 可独立 unit-test（mock processor closure）
- `transcode_single` / `analyze_records_with_probe` 回归"输入 → 输出"的纯函数测试
- 信号 handler 不影响 `transcode_single` 测试（无 mock signal handler）

**GUI actor 也走 TaskManager**（review #120）：`Command::StartBatch` 在
`run_actor` 里的内联 per-file 循环被替换成 `TaskManager::new(None).run(...)`
调用。processor closure 负责 UI 协议（`Event::FileStarted` /
`FileCompleted` / `Progress` / `Error`），TaskManager 负责 batch/task
lifecycle log + `partial_output::Guard` 注册。返回类型从 `Vec<R>` 改成
`Result<Vec<R>, (usize, EncodeError)>`（abort-mid-batch 不再 panic）。
`DryRun` / `Analyze` arm 不动——前者走 `dry_run_batch`，后者走 rayon
并行 helper，两者都不需要 TaskManager 的 lifecycle log 编排。

---

### 3.5 数据流

#### 转码流程

```
CLI 解析参数 → EncodeParams
    ↓
collect_files → 文件列表（walkdir）
    ↓
init_encoder_cache(capability) → EncoderCache（一次，每进程）
    ├─ ffmpeg -encoders（始终）
    └─ NVENC 能力实测（**不受 nv 开关影响**；1 帧试编码 / 进程内缓存）
    ↓
设置信号处理（parking_lot::Mutex）
    ↓
transcode_batch / transcode_single
    ↓
每个文件内部：
    1. 检查输出是否已存在
    2. probe_video_metadata（一次 ffprobe）
    3. MaxFpsConfig::decide（帧率动作）
    4. should_skip_encoding（决策表）
    5. select_encoder（缓存）
    6. build_ffmpeg_command（-r / -fps_mode 输出选项）
    7. Command 执行（ProcessOutput::Inherit）
    ↓
回调 → CLI 输出
```

#### 分析流程

```
CLI 解析参数 → AnalyzeParams
    ↓
collect_files → 文件列表
    ↓
analyze_fps / analyze_fps_parallel
    ↓
每个文件：
    probe_video_metadata_auto → best_fps → round → u32
    ↓
按帧率分组 → FpsStats
    ↓
CLI 输出 summary
    ↓
若 -v：detailed() → 写入输出文件
```

#### 分析进度上报（U22 / Tier 3 #9）

GUI 路径需要在并行 analyze 跑的时候给 UI 推进度。三个设计选择必须说清：

**1. 进度上报在哪一层？**——c-probe FFI 层 vs rayon orchestrator 层。

选项 A：在 `video_probe.c` 里加 progress callback（C → Rust closure）。每包回调一次，
per-packet 精度。**否决**——c-probe 桥的单一设计规则是"stateless, 每次 open/close 自己的
`AVFormatContext`"，加 callback 意味着跨 FFI 边界的 `void*` userdata + `Box<dyn FnMut + Send>`，
破坏 stateless 约束。更关键的是：单文件 probe 的耗时极低（c-probe 10-50ms / CLI 30-100ms），
c-probe 一次函数调用的开销本身就占大头，加 callback 路径会让"小文件 N 个"的总耗时被 callback
穿越放大。"大文件慢"的情况同样在 C 侧（libavformat 读 I/O 才是耗时来源，Rust FFI 怎么绕都绕不过），
per-packet 进度改变不了这个事实。

选项 B：在 rayon `par_iter()` 层加 closure-injection 的 progress 回调。每个 worker 在自己的
任务完成后调一次 `(done, total, failed, last)` 进 Rust closure，闭包捕获 `Sender<Event>` 直
接发到 lib actor 的 channel。**采纳**——pure Rust，backend-agnostic（c-probe 和 CLI fallback
都拿到进度），复用 `analyze_records_with_probe_parallel` 的 test-seam 模式。

**2. 多文件展示什么？**——逐文件 echo vs 整体计数器。

不是二选一。**复合**：
- **整体计数器**（B）= UI 进度面板的内容。`done` / `total` / `failed` 三个整数 + `last`
  字段（最近一个完成的文件 path + outcome）。文本形态 `47/100 (3 failed)`，v1 先不上视觉
  进度条（CSS bar 后续迭代）。
- **逐文件 echo**（A）= LogPanel / stderr 的内容。每文件一条 `log::warn!`（失败时）或
  `log::info!`（成功时）。**v1 默认仅 failure log**——成功 case 在 `Event::AnalyzeFinished`
  里给 records 表 summary，不在进度过程中逐条噪扰。失败 log 复用 `analyze_records_with_probe_parallel`
  已有的 `log::warn!("probe failed for {}: {e}", path.display())`，新代码不增加新日志路径。
- 两者通过同一个 actor 出来，但语义不同：A 是"结果/错误流"，B 是"进度信号"。

**3. 失败怎么数？**——按文件每任务计一次。rayon `par_iter().map()` 每个文件只跑一次（没有
重试逻辑），所以"一文件一 failure"是天然语义；同文件多次 probe 失败是实现 bug，不该
出现在接口语义里。`failed` 计数等于 `records` 里 `outcome = ProbeFailed` 的行数，UI 端
可以直接对账。

**v1 限制**（明确写进 §3.8.7）：`Command::Analyze` 不响应 `Command::Cancel`。
analyze 本身快（最坏 5-10s/文件 × 1000 文件 / N cores ≈ 几分钟），cancel 价值低；加 cancel
路径要让 rayon worker 在每个文件开始前 poll `AtomicBool`，测试矩阵翻倍。v2 再加。SIGINT
也不影响 analyze 批——`Command::Analyze` 走 `Event::AnalyzeFinished` 干净结束，而不是
`BatchCancelled`。

**协议 shape**（细节在 `rencoder_lib/src/job.rs` rustdoc）：

```rust
// Command
Command::Analyze { files: Vec<PathBuf> },

// Event —— 1 × Started + N × Progress + 1 × Finished
Event::AnalyzeStarted { total: usize },
Event::AnalyzeProgress {
    done: usize, total: usize, failed: usize,
    last: Option<AnalyzedFile>,           // 新 type（不复用 AnalysisRecord，
                                          // 不污染 success path 的字段）
},
Event::AnalyzeFinished { records: Vec<AnalysisRecord>, failed_count: usize },
```

为什么不复用 `EncodingRecord` / `AnalysisRecord` 当 `last` 字段：前者是 encode 路径的输出，
后者是"成功 + None-metric"行，没有 "ProbeFailed" 的 outcome 字段。`AnalyzedFile { path,
outcome: FileOutcome }` 是新 type，`FileOutcome` 只两个 variant（`Ok` / `ProbeFailed(EncodeError)`），
干净不污染。

**为什么不 chunked 进度**：按 1000 文件算，1000 × ~24 byte channel 消息 = 24 KB 总流量，
crossbeam unbounded channel 1M msg/s 量级，总开销 ~0.001s。不 chunked 让 UI 不需要自己
throttle state 更新——简单胜过微小优化。

**最终结果的展示约定（U22 #9e）**：

| Surface | 顺序 | 理由 |
|---|---|---|
| `Event::AnalyzeProgress.last` 字段 | 完成序（乱） | "just completed" 指示符，operator 看到的是"刚跑完哪个"，不是"按输入序该跑完哪个"。重排需 buffer，复杂度高，价值低 |
| `Event::AnalyzeProgress.done` / `failed` 计数 | 单调递增 | 主进度信号，反映实时工作流。必须保留实时性 |
| `log::warn!` per-file failure 行 | 线程交错 | CLI 端，只在 failure 时出现；post-mortem 用时间戳或 `[file_index]` 前缀排序 |
| 最终 `Vec<AnalysisRecord>` (`Event::AnalyzeFinished.records`) | **输入顺序** | `par_iter().collect()` 保证；CSV / 下游工具 / 调试都需要 |
| `AnalyzePanel` 渲染 (`state.analyze_results`) | **failed-first** | operator 跑完看结果时最关心"哪几个挂了"。`Vec::partition` by `r.fps.is_none()`，stable partition 保持组内输入序。**只动 UI 渲染层,不动 lib / CLI**(灰度发布:CLI summary 维持现状,以后需要时再统一) |
| `LogPanel` (`state.log`) | 时间序 | live 流,runtime 行为。failure 通过 `Event::LogLine` 进来时按时间序,与 `AnalyzePanel` 的"结果优先"分工不冲突 |

分工:**live = time order, final = failure-first**。两个约定分别服务 operator 的不同动作(看 live tail 抓异常上下文,看 final panel 找失败文件),不互相干扰。

---

### 3.6 部署布局（U18）

**Source 视图**（workspace，单一 git 仓库）：

```
rencoder/
├── Cargo.toml                 # workspace root
├── rencoder_lib/              # 核心库（publish = false）
├── rencoder_cli/              # CLI 前端（thin wrapper）
├── examples/
│   └── rencoder.toml          # dev-only 参考；dist 用户无需
├── scripts/
│   └── dist.sh                # 把 target 产物 + assets 装配成单目录应用（augment tarball with rencoder.toml + log/ + data/）
└── docs/
    └── design.md              # 设计契约（本文件）
```

**dist 视图**（`scripts/dist.sh` 产出，按目标平台一份）：

```
rencoder-cli-<triple>/
├── rencoder                   # 编译产物
├── rencoder.toml              # 默认配置（auto-create 模板的种子）
├── LICENSE
├── README.md
├── log/                       # 占位，首次运行写入时自动创建
└── data/                      # 占位，首次运行写入时自动创建
```

**文件放在哪里：一条原则** ✅ ✅

每个文件都放在**离它被访问、离它起作用最近的地方** ✅ ✅。这条原则落到两个目录 ✅ ✅：

| 目录 ✅ | 是什么 ✅ | 装什么 ✅ |
|---|---|---|
| **应用目录** ✅ ✅ | 应用自己所在的目录（binary 的父目录，文档里写作 `<app_dir>` ✅ ✅）✅ ✅ | 配置 ✅、日志 ✅、机器能力记录 ✅、工作记录 ✅ |
| **操作目录** ✅ ✅ | **本次操作作用的目录** ✅ ✅ | 取消请求 ✅、运行能力检查报告 ✅、占用标记 ✅ |

**应用目录**装的是"应用自己的东西" ✅ ✅ —— 配置与日志是应用的偏好与痕迹 ✅ ✅，换一批
文件跑也不换地方 ✅ ✅；机器能力记录是**这台机器**的结论 ✅ ✅（探测一次几百毫秒 ✅ ✅，
要跨进程复用 ✅ ✅）；工作记录是**跨会话**的历史 ✅ ✅（历史面板一次看全 ✅ ✅）。

**操作目录**装的是"这一趟的东西" ✅ ✅ —— 取消请求与检查报告描述的正是"这一趟、这个目录
能不能干" ✅ ✅，放在操作目录里，它旁边就是它描述的那个目录 ✅ ✅。

**操作目录怎么定** ✅ ✅：

| 前端 ✅ | 操作目录 ✅ |
|---|---|
| CLI ✅ ✅ | 目标所在目录 ✅ ✅ —— `-d DIR` → `DIR` ✅；`-f FILE` → `FILE` 的父目录 ✅ ✅ |
| GUI ✅ ✅ | 用户选定的工作目录 ✅ ✅ |

没有目标的操作（`rencoder detect` ✅）没有操作目录 ✅ ✅ —— 它唯一的产物（机器能力记录 ✅）
是应用级的 ✅ ✅，留在应用目录 ✅ ✅。

**运行时视图**（用户解压 + 首次运行后）：

```
<user-chosen-install-dir>/     # = 应用目录 = current_exe().parent()
├── rencoder                   # 用户调用的入口
├── rencoder.toml              # 用户编辑的偏好（auto-create 后存在）
├── nvenc-capability.json      # 机器能力记录（`rencoder detect` 写入）
├── log/                       # 日志：rencoder.YYYY-MM-DD.NN.log
└── data/                      # 工作记录：encoding-<ISO8601>Z.parquet

<操作目录>/                    # 本次操作作用的目录
├── cancel                     # 取消请求（存在即取消）
├── capabilities.json          # 运行能力检查报告（这一趟的结论）
└── .rencoder-owner            # 占用标记（跑批期间存在，跑完删掉）
```

**路径写进配置，不写死在代码里** ✅ ✅。配置文件自己在哪里仍然是入口显式值 ✅ ✅
（`--config` ✅ > `$RENENCODER_CONFIG` ✅ > `<应用目录>/rencoder.toml` ✅ ✅）—— 配置
不能配置自己的位置 ✅ ✅。其余路径写在 `[paths]` 段 ✅ ✅：

```toml
[paths]
# 相对「应用目录」
log_dir = "log"
records_dir = "data"
capability_record = "nvenc-capability.json"
# 相对「操作目录」
cancel_file = "cancel"
capability_report = "capabilities.json"
```

优先级 ✅ ✅：入口显式值（env ✅ / CLI ✅）> 文件显式值（`[paths]` ✅）> 内建默认值 ✅ ✅。

**一处决定** ✅ ✅：路径不再散落在各处现算 ✅ ✅ —— lib 有一个 `Paths` ✅ ✅
（`rencoder_lib::config::paths` ✅ ✅），一次解析出全部路径 ✅ ✅，前端拿着它用 ✅ ✅。

**操作目录里的应用文件不会被当成输入** ✅ ✅：应用写进操作目录的文件在采集时按名字跳过
✅ ✅（`cancel` ✅、`capabilities.json` ✅ —— 名字由 lib 的 `Paths` 给出 ✅ ✅，各处不硬编码
✅ ✅；占用标记 `.rencoder-owner` ✅ ✅ 同理 ✅ ✅ —— 它没有视频扩展名 ✅ ✅，采集的扩展名
过滤本来就挡得住 ✅ ✅，仍然显式跳过 ✅ ✅）。

**scripts/dist.sh 职责**：

本项目把"打包"流程切成两层——`cargo-dist` 做 archive（archive format + checksum），
shell script 做 U18 augmentation（塞 `rencoder.toml` + `log/` + `data/` 占位）。

| 步骤 | 谁做 | 什么 |
|------|------|------|
| 1. 编译 binary | `cargo-dist` | `dist build --target <triple>` |
| 2. 打包 archive | `cargo-dist` | `.tar.xz` (Unix) / `.zip` (Windows)；含 binary + LICENSE + README + sha256 |
| 3. 抽 host tarball 到 `dist/` | `scripts/dist.sh` | `tar -xJf ... -C dist/` |
| 4. Augment tarball（U18 单目录 layout） | `scripts/dist.sh` | extract → cp `rencoder.toml` + mkdir `log/` + mkdir `data/` → re-pack in place |

`cargo-dist` 不做 step 4（augment）的原因是它不识别 U18 单目录应用模型——它只产出 binary + LICENSE + README。
augment 必须 post-process，由 `scripts/dist.sh` 在 host 抽 tarball 后做，并 re-pack 写回原 tarball。
这一步是 [§3.4.5](#345-app-level-config--single-directory-app-u18) "dist 镜像运行时布局" 的实现。

**Auto-create 流程**（`AppConfig::load` 内部）：

```
AppConfig::load(path)
    │
    ├─ path 存在？ ─→ 读 + parse + validate
    │                   └─ 失败 → 报错（绝不覆盖用户已有文件）
    │
    └─ path 不存在？ ─→ 写默认配置（atomic tmp + rename）
                        ├─ 父目录创建（如需要）
                        ├─ 写所有 Default 字段 + inline 注释
                        └─ stderr 提示：created <path>
                       然后继续 parse 流程
```

**env var override 矩阵**（优先级从高到低）：

| 变量 | 用途 | 默认 |
|------|------|------|
| `RENENCODER_CONFIG` | 显式指定 config 文件路径 | `<app_dir>/rencoder.toml` |
| `RENENCODER_LOG_DIR` | 日志目录 | `<app_dir>/log/` |
| `RENENCODER_DATA_DIR` | 数据/工作记录目录 | `<app_dir>/data/` |

---

### 3.7 持续部署（GitHub Actions U18）

release 流程由 `.github/workflows/build.yml` 编排，触发条件为 tag push
匹配 `v[0-9]*.[0-9]*.[0-9]*`（**GitHub glob 不是正则**：`+` 是字面字符，
`v[0-9]+.[0-9]+.[0-9]+*` 匹配不到 `v1.2.3`），或 workflow_dispatch 手动
指定一个**已存在**的 tag。

```
tag push v1.2.3
  ↓
verify: HEAD == refs/tags/<tag>（防止发布错代码）
  ↓
build (matrix: 3 targets)
  ├─ aarch64-apple-darwin   -> rencoder_cli-*.tar.xz / rencoder_ui-*.tar.xz (cargo-dist) + U18 augmentation
  ├─ x86_64-unknown-linux-gnu -> *.tar.xz (cargo-dist) + U18 augmentation
  └─ x86_64-pc-windows-msvc  -> *.zip (cargo-dist) + U18 augmentation (Python helper)
  ↓
macos-dmg (needs build)
  ├─ download aarch64-apple-darwin artifact
  ├─ ad-hoc codesign -s -<binary> (mountable on Apple Silicon)
  └─ hdiutil → rencoder_cli-aarch64-apple-darwin.dmg (+ .sha256)
  ↓
release (needs build + macos-dmg)
  ├─ verify: sha256sum -c 每个 *.sha256
  └─ softprops/action-gh-release --generate-notes --fail-on-unmatched-files
```

**为什么不用 cargo-dist 生成 GitHub Actions workflow**：

cargo-dist 0.32 的 `dist init --ci github` 会生成一个 600+ 行 Jinja2 模板的 release.yml，
带 partials、prerelease logic、SBOM、attestations、cross-platform runner matrix 等大量
默认 feature。本项目只产 3 targets + 1 个 DMG，**手写两百多行比承载 cargo-dist 的全部 feature set
更可维护**。如果未来需要 homebrew / npm 发布，再考虑 cargo-dist 的 partials 系统。

**cargo-dist 0.32 限制 + 应对**：

cargo-dist 0.32 不支持 `dmg` installer 类型（只支持 msi/shell/powershell/npm/homebrew）。
应对：单独的 `macos-dmg` job 用 macOS 自带的 `hdiutil`（不依赖 homebrew，
CI 上不安装 create-dmg）+ ad-hoc codesign（`--sign -`）让 DMG 在 Apple Silicon 上能挂载。
正式签名 + notarization 需要 Developer ID + Apple ID secrets，那是独立的 story。

**Windows 布局**：三平台包现在都带 U18 的 app-dir 布局（`rencoder.toml` +
空 `log/` + 空 `data/`）。zip 的 augmentation 由 `scripts/augment_zip.py`
（Python `zipfile`）完成——GitHub 的 windows runner 上 `zip` / `unzip`
不可靠，而 Python 3 是所有 runner 的预装组件；tarball 仍由 `tar -xJf`
在 Unix 上就地重打包。空目录用 `.gitkeep` 占位，否则 zip / tar 都不保存空目录。

**发布前的三道校验**：

1. **tag ↔ HEAD**：dispatch 时指定的 tag 必须指向当前 HEAD，否则不发布。
2. **校验和**：release job 在创建 release 前 `sha256sum -c` 每个 `*.sha256`；
   DMG 也产出 `.sha256`。
3. **文件匹配**：`fail_on_unmatched_files: true` + 上传侧
   `if-no-files-found: error`——产物为空时直接失败，而不是发一个空 release。

另外 build job 断言**产物不链接 `libav*`**：`c-probe` 是 opt-in，发布二进制走 ffprobe
子进程探测，不依赖版本匹配的 libavformat / libavcodec / libavutil。链接了它们，用户的
ffmpeg 一升级（macOS 定期抬 dylib major，Linux 各发行版 major 不同）二进制就会起不来。

**权限与并发**：workflow 顶层 `contents: read`，只有 release job 拿
`contents: write`（最小权限）；concurrency group 按 tag 分组且
`cancel-in-progress: false`，避免两个 release 同时写同一个 tag。

---

### 3.8 GUI 前端（rencoder_ui，U21 计划）

**Tier breakdown**:

| Tier | 内容 | 状态 |
|------|------|------|
| **U21 (v1)** | actor 模型 + Command/Event 协议 + 三按钮 UI (Start / Cancel) + batch + progress 极简显示 + AppConfig 加载 + actor thread 错误处理 | bootstrap 完成（rencoder_ui crate） |
| **v1.1** | DirPicker + EncodeParams 从 AppConfig + 扩展名过滤 + 三个 Panel (Progress/Log/History) + History 回读 + Preview/Dry Run (`Command::DryRun`) | §3.8.7 limitations 表 |
| **v1.2** | Native 文件对话框 + preset/max_fps passthrough | future work |
| **Future** | 多批 + 非 Unix 平台 | deferred |

本节后续内容（§3.8.1–§3.8.8）描述全部 v1+v1.1+v1.2 的完整设计意图;具体哪些项属于哪一档,见 §3.8.7 limitations 表。

#### 3.8.1 框架与形态

- **Dioxus desktop (0.6)**：纯 Rust、wry webview 后端，与 Tauri 同层但避免 IPC JSON 序列化税（每条 `Progress` 事件不跨 IPC 边界）。
- **单 batch per process**：编码是 CPU 密集，多任务无益于效率。沿用 §3.4 的 `current_output` cell 单例约束——UI 一次只能有一个活跃批。**不**设计 `JobRegistry` / 多 job 抽象。
- **位置**：作为第三个 crate 加入 workspace（`rencoder_ui`），依赖 `rencoder_lib`（`publish = false`，零外部发布）。**不**复用 `rencoder_cli::signal`（webview 自带 beforeunload / IPC 机制）。
- **平台范围**：遵循 §1.3，当前实现 Unix-only（macOS 已验证；Linux 跟 macOS 共享 `#[cfg(unix)]` 分支，覆盖到时一并验证）。wry/Dioxus 的非 Unix 平台支持属于 future work——本节设计不阻止,但当前不投入测试。

#### 3.8.2 Actor 模型：UI 与 lib 是两个 actor

```
UI actor (Dioxus)  ──Command──►  lib actor (BatchController bg thread)  ──起 worker──►  batch worker
                 ◄───Event────                              ◄────done 信号────
```

- 两个 actor 完全独立，**只**通过消息通信。无共享变量、无回调、无跨边界函数调用。
- ffmpeg 子进程、`PartialOutputHandle`、`Child` handle、SIGINT 全部封装在 lib actor 内部。
- 这把 lib 内部的复杂性收敛到一处，UI 端只看到消息流。
- **lib actor 不跑批**（§3.10.1.1）：批在 worker 线程上跑，actor 全程可响应。这是 UI 的
  Cancel 按钮能真正中止编码（kill 在飞的 ffmpeg + 删半成品）的前提。

**Channel 上限（review #111）**:两个 actor 之间的 mpsc 是有界的。
- `cmd_tx` (UI → lib):容量 16,`try_send` 语义。人类节奏下足够,
  满了就丢——重复的 `StartBatch` 本来就会被 busy-gate 拒绝。
- `evt_tx` (lib → UI):容量 1024,按 variant 分流:结构性事件
  (`BatchStarted` / `FileCompleted` / `BatchCompleted` / `Error`)
  满了就 block(不丢,准确性优先);进度类事件(`Progress` /
  `AnalyzeProgress` / `LogLine`)满了就丢 + 计数(`#[non_exhaustive]`
  让未来新增 variant 默认走"结构性"安全路径)。
- 公共 API (`ControllerHandle.command_sender()`) 类型不变,
  `Sender<Command>` 的内部容量是构造细节。

#### 3.8.3 消息协议

**Command（UI → lib）**

```rust
#[non_exhaustive]
pub enum Command {
    StartBatch { params: EncodeParams, files: Vec<PathBuf> },
    /// 中止当前批（包括正在处理的单文件）：等效于 SIGINT 但由 UI 触发。
    /// 单文件跳过不暴露——用户要么让当前文件跑完，要么按 Cancel 中整批。
    /// **不**影响 `Command::Analyze` 批（v1 限制——见 §3.4 "分析进度上报"）。
    Cancel,
    /// 从 `<app_dir>/data/encoding-*.parquet` 回读历史。
    /// - `limit`：返回最多 N 条最新记录；`None` 表示全部
    /// - `after`：仅返回 `started_at >= after` 的记录；`None` 表示不限时间
    LoadHistory {
        limit: Option<usize>,
        after: Option<chrono::DateTime<chrono::Utc>>,
    },
    /// 并行分析一批文件（U22 / Tier 3 #9）——只 probe 不 encode。
    /// 协议：`AnalyzeStarted { total }` → N × `AnalyzeProgress { done, total,
    /// failed, last }` → `AnalyzeFinished { records, failed_count }`。
    /// 协议 shape 与"为什么这样"见 §3.4 "分析进度上报"。
    Analyze { files: Vec<PathBuf> },
    /// 把 `AppConfig` 写回原路径（U21 / Tier 2 #8）—— Setup 页每次改动都
    /// 走这条。**载荷是 UI 编辑后的那份**（`Box<AppConfig>`）：UI 编辑的是自己
    /// 的 draft（一份 clone），没有载荷的话 lib 只会把它启动时加载的那份写回去
    /// ——编辑被静默丢掉，文件被改写成原值，而 UI 显示"已保存"。
    /// lib 内部 `AppConfig::save(&config_path)`，发 `Event::ConfigSaved` 或
    /// `Event::Error`。`[encode].nv` 变化时顺带刷新编码器缓存（见下）。
    SaveConfig { config: Box<AppConfig> },
    /// 探测这台机器的 NVENC 能力并回报。逻辑与 CLI 的 `rencoder detect` 相同：
    /// 结果存进 `<app_dir>/nvenc-capability.json`，与 CLI 共用同一份记录。
    /// - `force = true`：总是重新探测（UI 的"重新检测"按钮）
    /// - `force = false`：记录新鲜时直接复用（启动时解析）
    Detect { force: bool },
}
```

**Event（lib → UI）**

按 topic 隐式分类（通过 enum variant，无需独立 `topic` 字段）：

```rust
#[non_exhaustive]
pub enum Event {
    // Lifecycle topic（低频，批级 / 文件级）
    BatchStarted { id: u64, total_files: usize },
    FileStarted { index: usize, total: usize, source: PathBuf },
    FileCompleted { index: usize, record: EncodingRecord },
    BatchCompleted { records: Vec<EncodingRecord> },
    /// 由 `Command::Cancel` 或 SIGINT 触发：`completed` 是取消前已完成的文件数。
    /// partial 输出由 `PartialOutputHandle` 在 SIGINT 路径清理；Cancel 路径
    /// 沿用同一清理逻辑。
    BatchCancelled { completed: usize, total: usize },

    // Progress topic（高频，~2/sec/文件）
    Progress {
        file_index: usize,
        frame: u64, fps: f64, speed: f64,
        time_secs: f64, eta_secs: Option<f64>,
    },

    // Log topic（中频，~10/sec）
    LogLine { file_index: usize, line: String },

    // State topic
    HistoryLoaded { records: Vec<EncodingRecord> },
    /// Config save round-trip（U21 / Tier 2 #8）—— UI Save 按钮触发；
    /// path 是 lib 实际写到的文件（`ControllerHandle::config_path`）。
    /// 失败走 `Event::Error` 而不是这个 variant。
    ConfigSaved { path: PathBuf },
    /// 这台机器的 NVENC 能力（启动解析或 `Command::Detect` 之后）。
    /// UI 把它显示在 NVENC 开关旁边：哪些硬件编码器真能跑、哪些不行及原因。
    NvencCapability(nvenc::Capability),

    // Analyze topic (U22 / Tier 3 #9) —— 见 §3.4 "分析进度上报"
    /// `Command::Analyze` 接收后、第一个文件 probe 之前发。`total` =
    /// `files.len()`，给 UI 端一个清晰的"开始"信号和总数。
    AnalyzeStarted { total: usize },
    /// 每个文件 probe 完成后发一次。`done` / `total` / `failed` 单调
    /// 递增；`last` 让 UI 能显示"刚完成哪个文件"（结果流，A 模式）。
    /// 顺序非确定（rayon worker race），UI 只看 `done` 计数。
    AnalyzeProgress {
        done: usize,
        total: usize,
        failed: usize,
        last: Option<AnalyzedFile>,
    },
    /// 整批结束。`failed_count` 应等于 `records` 里 `outcome =
    /// ProbeFailed` 的行数（设计约束——UI 端可对账）。
    AnalyzeFinished {
        records: Vec<AnalysisRecord>,
        failed_count: usize,
    },

    // Lifecycle topic (actor shutdown)
    /// Actor thread is shutting down. **Last** event sent before the
    /// channel closes. `reason` distinguishes clean vs error vs cancelled
    // NOTE: an `ActorExited { reason }` variant was specced here but
    // never implemented. The UI notices the actor's exit by the event
    // channel closing (`evt_rx.recv()` returning `Err`) -- an implicit
    // signal that carries no reason. Revisit if the UI ever needs to
    // tell "the controller stopped" apart from "the UI closed".

    // Rejection topic（命令被拒，但什么都没坏）
    /// 命令被拒：闸门拒绝（已经有批在跑）、参数不合法这类**良性**拒绝。
    /// 与 `Event::Error` 分开是必须的——`Error` 是 fatal-only，UI 收到它
    /// 会把正在跑的那批 finalize 成 Done。一次良性的"重复 StartBatch 被拒"
    /// 不该被当成"这批失败了"。
    CommandRejected { reason: String },

    // Error topic（仅 fatal；per-file 失败走 EncodingRecord.error）
    /// 仅 fatal 错误；payload 故意精简为 `(category, message)`。
    ///
    /// **作用域**：fatal 是"这一批 / 这个子系统出事了"，不是"有什么失败了"。
    /// UI 收到它要能判断该不该动当前正在跑的那批——与批无关的 fatal
    /// （例如配置保存失败）不能 finalize 活动任务（§3.11.3）。
    ///
    /// **不**携带 `ErrorContext.ffmpeg_stderr_tail`——大多数 fatal 类别
    /// (`ProbeFailed` / `EncoderSelectionFailed` / `FfmpegSpawnFailed` /
    /// `ConfigError` / `IoError`) ffmpeg 根本没跑,stderr 为空;
    /// `FfmpegNonzeroExit` 才有 stderr,但 GUI 如需展示,可在收到
    /// `BatchCompleted` 后从对应 `EncodingRecord.error` 读 `stderr_tail`
    /// (`EncodingRecord.error` 路径才有完整 `ErrorContext` 三字段)。
    Error { category: ErrorCategory, message: String },
}

/// Fatal-only 错误的分类（per-file 失败不通过这里，走 `EncodingRecord.error`）。
/// 在 §3.2 的运行时类型之外再列出是为了 §3.8 自完备——`types.rs` 是真源。
pub enum ErrorCategory {
    ProbeFailed,             // 探测阶段就失败的批（无法启动）
    EncoderSelectionFailed,  // 编码器不可用且无 fallback
    FfmpegSpawnFailed,       // ffmpeg 子进程无法 spawn（如 binary 不存在）
    FfmpegNonzeroExit,       // ffmpeg 子进程异常退出且不可恢复
    ConfigError,             // 配置解析/校验失败
    Cancelled,               // 批被取消（Cancelled 错误 vs `Event::BatchCancelled` 生命周期事件）
    IoError,                 // 磁盘 / 权限 / path 错误无法继续
}

/// Actor thread shutdown reason — last event before `evt_tx` drops.
/// `Clean` = actor finished a batch and has no more work (UI can
/// restart by re-calling `start_controller()` if needed). `Error`
/// = fatal — actor thread exited because something broke. `Cancelled`
/// = the actor received `Command::Cancel` and decided to exit (rare;
/// normally a Cancel just stops the in-flight batch, the actor
/// stays alive waiting for next Start).
///
/// `#[non_exhaustive]` (consistent with ErrorCategory / SkippedReason).
pub enum ExitReason {
    Clean,
    Error(crate::error::EncodeError),
    Cancelled,
}

Topic 设计意图：dispatch 由 UI 端 reducer 用 `match` 完成，路由成本零；不需要单独的 `topic: Topic` 字段或多 channel。

#### 3.8.4 Controller

```rust
/// 控制器句柄:同时持有 lib 端的"actor 入参"(cmd_tx 给 UI 发命令)
/// 和"actor 出参"(evt_rx 让 UI 收事件),以及一次性 `start_controller`
/// 加载完的 `AppConfig` + 路径(给 SettingsPanel 显示 + Save 回写用)。
/// 字段类型与每个 accessor 的签名见 `rencoder_lib::job::ControllerHandle` 的 rustdoc。
pub struct ControllerHandle { /* 见 rustdoc */ }

pub fn start_controller() -> ControllerHandle;

/// 用注入的环境起 actor(测试缝):同一个 actor,不同的依赖。
pub fn start_controller_with(env: ActorEnv) -> ControllerHandle;
```

UI 启动时调一次 `start_controller()` 拿到 `ControllerHandle`,然后 `into_split()` 把两端分给不同持有者:cmd_tx 给多个 UI 组件(可克隆),evt_rx 给 UI 顶层的事件消费线程(独占消费,见 §3.9 "UI 运行时 / Dioxus 适配")。

- `cmd_tx`：可克隆,供多个 UI 组件使用(开始按钮、取消按钮、历史按钮)。
- `evt_rx`：由 UI 顶层**专用 OS 线程**独占消费;不广播、不多订阅者——保证单消费者的因果序。`crossbeam_channel::Receiver` 是 single-consumer,无法 clone——这正是我们想要的(避免事件分发歧义)。具体 UI 端如何消费(为何不用 `use_coroutine` + async 块)见 §3.9。
- Channel 策略:`crossbeam unbounded`(`std::sync::mpsc` 无 unbounded 变体)。单 batch 下每秒最多 ~50 个事件,UI 永远消费得过来;如未来有背压风险再换 `bounded + DropOldest`。
- Actor 线程命名:`rencoder-controller`,便于 thread 列表里识别。
- **批次 worker 线程命名:`rencoder-batch`**。actor 收到 `StartBatch` / `DryRun` /
  `Analyze` 后起一个 worker 跑这一批,批结束 worker 退出(§3.10.1.1)。actor 本身不跑批,
  因此它在批处理期间照样能处理 `Command::Cancel`。

**Lib 内部细节**:actor 线程启动时**一次性**加载配置(经 `resolve_config` + `AppConfig::load`)+ **一次性**解析能力(`detect` 优先，命中 `<app_dir>/nvenc-capability.json` 就不重探)→ 初始化 `EncoderCache`。能力解析**与 `encode.nv` 无关**（§3.10.5）——`nv` 是本次运行的选择，在 `EncodeParams` 里。失败 → emit `Event::Error(ConfigError)` + 线程退出(UI 看到错误 + 之后无事件,知道 actor 死了)。

#### 3.8.5 状态与持久化的双轨

| 通道 | 形式 | 生命周期 |
|------|------|----------|
| 实时 | mpsc 事件流 | 临时，重启即丢 |
| 持久 | `<app_dir>/data/encoding-*.parquet` | 永久 |

- 批结束：lib 写 Parquet + 发 `Event::BatchCompleted`。
- **History 回读（v1.1 启用）**:UI 启动时发 `Command::LoadHistory { limit, after }` → lib 读 Parquet → 发 `Event::HistoryLoaded` 填充历史视图。v1 不展示历史(见 §3.8.7 limitations),但 Parquet 仍在写,数据可用。
- 这是 event sourcing 的两面：state 是 event log 的 fold，event log 是 append-only 真相源。
- **Realtime 通道的消费方**:UI 端的具体消费机制——`crossbeam_channel::unbounded` 在专用 OS 线程里阻塞 recv,然后通过 `use_signal_sync` 的 `SyncSignal` 改 Dioxus state——见 §3.9 "UI 运行时 / Dioxus 适配"。Channel 策略:UI 侧用 `Arc<Mutex<>>` 共享给子组件;lib 侧不感知。背压:两侧都是 unbounded;send 永不 block。

#### 3.8.6 lib 内部实现要点

本节描述 **设计意图**(WHAT/WHY),不重复实现细节(签名、struct 字段、work-flow 步骤 都在代码注释里)。

**不修改**（CLI 路径零回归）：

- `EncodingRecord` / `EncodingOutcome` / `ErrorContext`(见 §3.2)
- `TaskManager` / `PartialOutputHandle` / signal handler(CLI 端)
- `EncodeError`(保持 `thiserror` 原样,不加 `Serialize`);`ProcessOutput` 枚举(见 §3.2)
- `AppConfig` 加载/校验
- CLI 路径的 ffmpeg 编排(含 stderr 收集 + partial output 清理)——保留 `cmd.output()` 同步路径

> `transcode_batch` 签名**已作废**（§3.10.4 段 A：参数包收成 `BatchContext`）。
> `process_one_file` 签名同理——它早已不存在，现在由 `BatchContext` + `FileEncoder`
> 缝承担（§3.10.4）。

**新增**(U21):

| 位置 | 抽象 | 职责 |
|------|------|------|
| `pipeline/cancel.rs` | `CancelToken` | `Arc<AtomicBool>` + cancel 标志(`run_ffmpeg_with_progress` 轮询) |
| `pipeline/batch.rs` | `run_ffmpeg_with_progress` | GUI 路径的 cancellable ffmpeg 调用——`cmd.spawn()` + reader thread + progress 回调 |
| `pipeline/batch.rs` | `encode_prepare` | CLI / GUI 共享的 probe + partial detect + skip + argv build 逻辑 |
| `pipeline/batch.rs` | `transcode_single_with_progress` | GUI 路径的 per-file 入口(共享 `encode_prepare`,ffmpeg 调用换成 spawn 路径) |
| `pipeline/batch.rs` | `build_encode_record` | CLI / GUI 共享的 record 构造(成功 / 失败两种 outcome) |
| `pipeline/batch.rs` | `FfmpegProgress` struct | `Event::Progress` 的源数据 |
| `pipeline/batch.rs` | `dry_run_batch(ctx, files)` | Dry-run 路径(V1.1):跑 `encode_prepare` per file 但**不调 ffmpeg**,返回 planned `EncodingRecord`(Ready → Succeeded,Skipped/FailedProbe → as-is)。`encode.nv` 故意忽略——dry-run 不知道也不关心 GPU 是否启用 |
| `error.rs` | `EncodeError::category() -> ErrorCategory` | 编码错误 → UI 展示分类的映射（review #121 从 `job.rs` 迁来，`pipeline` 不再依赖 `job`） |
| `job.rs` | `Command::DryRun { params, files }` | Dry-run 入口(V1.1):跟 StartBatch 同样的 busy-gate,但 emit `Event::DryRunFinished` 而不是 `BatchCompleted` |
| `pipeline/context.rs` | `BatchContext` | 一批要带的东西(参数/编码器缓存/运行时配置/NVENC 判定)——编码链往下传的是它,不是它的零件(§3.10.4) |
| `pipeline/context.rs` | `FilePlan` | 一个文件的现场(探测结果/argv/输出路径/两个时间基准)——取代 `EncodePrepareResult::Ready` |
| `pipeline/batch.rs` | `FileEncoder` | "编一个文件"这一步的单方法 trait(测试缝)——生产走真 ffmpeg,测试替身只替换这一步,事件由共享代码发 |
| `job.rs` | `ActorEnv` | actor 的注入依赖(配置/路径/编码器/探测 runner)——`start_controller_with` 让测试驱动同一个 actor |
| `config/paths.rs` | `Paths` struct | 一次解析出这次运行用到的全部路径(应用目录 + 操作目录)——各处不再各自现算(§3.6) |
| `config/paths.rs` | `config_file()` / `app_dir()` | 配置文件与应用目录的解析——入口显式值 > 内建默认值(§3.6) |
| `config/paths.rs` | `APP_OWNED_FILE_NAMES` | 应用自己写进操作目录的文件名——输入采集器按名字跳过它们(§3.6) |
| `types.rs` | `Target::operation_dir()` | 目标所在目录 = 这一趟的操作目录(§3.6) |
| `capability.rs` | `MachineFacts` struct | 这一趟查明的事实（ffmpeg 能不能跑 / NVENC 能力）——**查一次**，链上共享(§3.10.7) |
| `guard.rs` | `TargetGuard` struct | 目标目录的占用：标记文件 + 内核文件锁——同一目标同一时刻只有一个实例(§3.11.1.1) |
| `guard.rs` | `GuardError` / `Occupancy` / `Holder` | 占用探测的三个答案：有人占着(良性拒绝) / 文件系统不支持锁(拒绝这一趟) / 谁在占(§3.11.1.1) |
| `guard.rs` | `common_ancestor()` | 批的范围 = 它所有输入所在目录的公共祖先(§3.11.1.1) |
| `utils/fs.rs` | `plan_outputs()` | 整批的输出路径规划——撞车的那一组改名(`<词干>_<输入扩展名>_<序号>`)，序号按排序后的路径(§3.11.1) |
| `capability.rs` | `Capabilities` struct | 检查的**整体**结果：检查项 + 机器能力；报告文件是它的落盘形态(§3.10.7) |
| `job.rs` | `Command::SetWorkingDir { dir }` | GUI 换工作目录 → 重新检查(复用机器事实)+ 报告落到新目录(§3.10.7) |
| `job.rs` | `Event::CapabilityChecked { report }` | `Event` 新增 variant：报告写好了，路径在这儿——前端读文件(§3.10.7) |
| `encoder/nvenc.rs` | `resolve_with` / `resolve_and_store_with` | NVENC 探测的 runner 缝——注入 ffmpeg 调用，测试不必真跑探测；后者把"探测 + 落盘"一起换掉 |
| `job.rs` | `Event::DryRunFinished { records }` | Dry-run 结果:planned outcomes,**没有**实际 ffmpeg 输出 |
| `analyzer/records.rs` | `analyze_records_with_probe_and_progress_parallel<F, P>` | U22 / Tier 3 #9：并行 + 进度 closure-injection。`F` 是 probe 闭包（同 `analyze_records_with_probe_parallel`），`P: Fn(AnalyzeProgress) + Send + Sync` 在每个 worker 完成时调一次。`AnalyzeProgress { done, total, failed, last }` 4 字段 |
| `analyzer/records.rs` | `AnalyzeProgress` struct + `AnalyzedFile { path, outcome: FileOutcome }` + `FileOutcome { Ok, ProbeFailed(EncodeError) }` | U22：进度回调 payload。`AnalyzedFile` 是新 type（不复用 `AnalysisRecord`——后者没有 `ProbeFailed` outcome 字段，复用得加 `error: Option<_>` 污染 success path）|
| `job.rs` | `Command::Analyze { files }` arm | U22：调用 `analyze_records_with_probe_and_progress_parallel` with `probe = probe_video_metadata_auto`，`progress = |p| evt_tx.send(Event::AnalyzeProgress { ... })`。先发 `AnalyzeStarted { total }`，批结束发 `AnalyzeFinished`。**走** busy-gate（v1 限制——UI 不能同时跑 analyze + encode；analyze 是 read-only，理论上可与 encode 并存，但 v1 简化起见跟 DryRun 一致复用 busy-gate）|
| `job.rs` | `Event::AnalyzeStarted` / `Event::AnalyzeProgress` / `Event::AnalyzeFinished` | U22：见 §3.8.3。`Event::AnalyzeProgress` 在 mock actor 里是 no-op（test seam，ref-count 即可）|
| `job.rs` | `Event::CommandRejected { reason }` | 命令被拒（闸门拒绝等）——良性，**不是** fatal：UI 弹一条 toast 即可，正在跑的那批不受影响（§3.8.7.6）|
| `data/store.rs` | `read_records(path)` | 单 Parquet 文件回读(v1 不使用,v1.1 启用) |
| `encoder/ffmpeg_arg.rs` | `ExtraArg` enum + `ExtraArg::parse` factory + char-class 模块 | 类型级 ffmpeg argv 安全边界(§3.3.7)——非法 argv 在 parse 阶段被拒,运行时无法塞 unsafe 值。替换 v1 的 runtime `validate_extra_args` |
| `config/app.rs` | `EncodersConfig::for_format` | 按格式取编码器覆盖；空字符串视为未设（否则 `has_encoder("")` 恒真） |
| `pipeline/cache.rs` | `init_encoder_cache_with_capability_runner` / `init_encoder_cache_async_with_capability_runner` | 能力解析的 runner 缝——测试不必真跑探测 |
| `types.rs` | `InputPath` newtype + `InputPath::new(&Path)` factory + `InputPathError` | 输入路径类型级安全边界(§3.3.8)——dash 前缀文件名在构造时就被拒,`build_ffmpeg_command` 不可能拿到 `-vf_negate.mp4`。替换 `collect_dir` 里的运行时 dash-prefix 检查(review #109 收尾) |

**per-file 的可测试性**(§8 item 43 的意图)——当年抽 `process_one_file` 是为了给测试一个
注入点；那个函数早已不存在，今天的注入点是 `FileEncoder` 缝（§3.10.4 规则 2）：测试替身
只替换"编一个文件"这一步，事件由共享代码发。

其他签名(`run_ffmpeg_with_progress`、`FfmpegProgress` 字段、`EncodeError→ErrorCategory` 映射表) 在代码注释里。**`FfmpegProgress` struct + `run_ffmpeg_with_progress` 工作流 + ETA 计算细节** 都在 `pipeline/batch.rs` 的 doc comment 里——读代码注释,不在这里重复。

**`EncodeError → ErrorCategory` 映射**(per-file 错误路径使用):

| `EncodeError` variant | `ErrorCategory` | 理由 |
|---------------------|----------------|------|
| `Io(_)` | `IoError` | 1:1 |
| `Ffmpeg(_)` | `FfmpegSpawnFailed` | 错误来自 `transcode_single_with_progress` 内部——per-file ffmpeg 错误已转 `Ok(Failed record)` 不走这条路径,剩下的都是 spawn/setup 失败 |
| `Probe(_)` | `ProbeFailed` | 1:1 (但 `transcode_single_with_progress` 通常把 probe 失败转 `Ok(Failed record)`) |
| `InvalidParam(_)` | `ConfigError` | 配置 / 参数错误 |
| `EncoderNotFound(_)` | `EncoderSelectionFailed` | 1:1 |
| `FpsParse(_)` | `ConfigError` | 配置 / 解析错误 |
| `UserCancel` | `Cancelled` | 1:1 |

`Event::Error` 是 fatal-only,所以"per-file encode 失败"走 `EncodingRecord.error` 字段,这里只处理**真 fatal**(setup / 系统错误)。映射函数在 `job.rs` 里,作为 `pub(crate) fn encode_error_category(e: &EncodeError) -> ErrorCategory` 存在,便于测试。

| `job.rs` | `start_controller() -> ControllerHandle` | 编排：后台线程 + mpsc + `run_ffmpeg_with_progress`；与 CLI 路径**共享** `encode_prepare` / `build_encode_record`（不 fork），但**不**走 `transcode_batch`（那是 CLI 专用的批入口）；job.rs 是 thin glue，把现有 pieces 装成 actor |

**Controller 启动时加载配置**（U21 增量，§3.10.5）：
- `config::paths::config_file()` 拿路径（`$RENENCODER_CONFIG` > `<app_dir>/rencoder.toml`）
- `AppConfig::load(&path)` 读 TOML（缺失时自动创建默认文件，与 CLI 一致）→ **每进程一次** ✓
- 能力解析一次（`detect` 优先：复用 `<app_dir>/nvenc-capability.json`，没有就探测并落盘）✓
  → `EncoderCache` ✓（**不绑 `nv`** ✓）
- `nv` 是**本次运行的选择** ✓：Setup 页的开关写进 config draft ✓，下一批从 draft
  组装参数时带上 ✓——切 `nv` **不重探能力、不换缓存** ✓
- 失败 → `Event::Error(ConfigError)` 后线程退出
- 这把 controller 与 CLI 拉到同一配置源——GUI 不再绕开 `rencoder.toml` ✓ |
| `job.rs` | `pub enum Command` / `pub enum Event` | 消息协议（见 §3.8.3） |

**CancelToken 与 SIGINT 的关系**(修正之前的错误描述):**没有"共享 token"**——两个 actor 完全独立(§3.8.2 的承诺),SIGINT 在 GUI 路径里**不是特殊路径**。

GUI 路径(`rencoder_ui`):

```
[SIGINT window event] ─┐
                        ├─► UI Msg::UserClickedCancel ─► reduce() ─► Command::Cancel ─► lib actor
[Cancel button click]  ─┘                                                                              │
                                                                                                         ▼
                                                                                         cancel_token.cancel()
                                                                                                         │
                                                                                                         ▼
                                                                                         run_ffmpeg_with_progress 轮询
                                                                                                         │
                                                                                                         ▼
                                                                                         child.kill() + partial output 由
                                                                                         encode_prepare partial detection
                                                                                         处理(下次启动自动重转)
```

- SIGINT 是 **UI event**,跟 button click 同级——dioxus/wry 把它翻译成 window event,UI reducer 转成 `Msg::UserClickedCancel`,再发 `Command::Cancel`。
- `cancel_token` 是 **lib actor 内部状态**,**只** 由 `Command::Cancel` 置位,没有跨 actor 共享。
- **lib actor 不安装 SIGINT handler**(§3.8.1 "不 复用 rencoder_cli::signal")——Ctrl-C 由 wry 在 webview 层捕获,变成 window event 后走标准 UI event 路径。

CLI 路径(`rencoder_cli`,对照):

```
[terminal SIGINT] ─► ctrlc::set_handler (lib 安装) ─► fs::remove partial output + exit(1)
                                                                       │
                                                                       ▼
                                                          ffmpeg 子进程被 process exit 杀掉
```

- CLI 路径走 lib 的 `ctrlc::set_handler` 直接做清理(§3.4.5),不走 cancel_token。
- 两条路径独立,各自清理:CLI 用 signal handler 删 partial + exit;GUI 用 cancel_token 杀 ffmpeg + 下次启动自动 partial 检测。

**为什么这样分**:两层 actor 各自处理自己的"取消"事件,不共享内存,符合 §3.8.2 "无共享变量"。GUI 的 cancel_token 完全属于 lib 内部;SIGINT 在 GUI 里只是众多 UI event 之一(其他还有 button click、key press、window resize 等)。

**log 过滤/节流**：lib 透传所有 ffmpeg stderr 行（不解析、不丢弃），上限/截断由 UI reducer 在 state 层做（`VecDeque` 限 1000 行）。

#### 3.8.7 UI 端模式："everything is msg"

Dioxus 端用 reducer 模式（Elm 架构）。所有用户输入转成 `Msg::User*`,所有 lib 反馈包成 `Msg::Lib(Event)`,统一走 reducer。

**Dioxus-controlled inputs**:表单 input 字段(如 `DirPicker` 的目录路径、未来的 config editor)使用 `use_signal<String>` 而**不是** `value: "..."` 属性。`value=` 属性只在首次 render 时设置,不响应后续 state 变化;`use_signal` + 双向同步才能让 state → UI → state 流正确(M3 review finding)。

**UI-side state types**（`rencoder_ui/src/msg.rs`）:

```rust
/// 顶层 UI state。整个 app 一个,所有视图组件读它。
///
/// **v1 task queue capacity = 1**(`tasks.capacity`): 同一时刻最多一个
/// active task。架构上已经为 v1.1 多 task queue 准备好——`TaskQueue`
/// 用 `active + pending` 二元结构,capacity 提升到 N 时只需把新 task
/// push 进 `pending` 而不是立刻 dispatch。v1 always-runs immediately
/// 所以 `pending` 永远为空,但数据结构已经在位。
struct AppState {
    /// Task queue(v1: capacity = 1)。所有 task 共享同一个 lib actor,
    /// 通过 lib 的 busy-gate 串行执行。Guard 走 `tasks.active.is_some()`
    /// ——task 是 gate 的 unit,**不是 batch id**。Batch id 是 lib-side
    /// 给当前 active task 分配的映射(per `Event::BatchStarted.id`),
    /// 不参与 gating 决策(per review 反馈,避免 id-as-coincidence 反模式)。
    tasks: TaskQueue,
    /// Session 内已完成任务的索引(newest-first)。Done transition 时
    /// reducer 从 `tasks.active` 取出 task,**保留 TaskId**,insert
    /// 到这里——queue 和 history 共享同一 ID 空间,跨两者的查找用
    /// 同一 TaskId 即可。
    ///
    /// **重要**: `task_history` 是 session 内 UX 缓存,**不是**记录
    /// 的 primary store。Per-file 的 EncodingRecord 已经由 lib 在
    /// `BatchCompleted` 时写入 Parquet(`encoding-*.parquet`),v1.1
    /// `Command::LoadHistory` 启动时 reload 到 `state.history`。
    /// 所以 v1 "进程退出时 task_history 丢失" 不是数据丢失——
    /// records 在 Parquet 里,只是 session 内 task 维度的导航 cache
    /// 没了。v1.1 加 cross-session task 导航(把 Parquet records
    /// group by batch_id 渲染成 TaskInfo 形态)即可。
    task_history: Vec<TaskInfo>,
    /// 当前 active page tab(`§3.8.7.2`)。`Setup` 是 app 启动默认值;
    /// 4 个 page 之间不自动切换(用户手动点 tab)。在 `[SETUP]` 点
    /// Start 后 batch 在后台跑,UI 停留在 `[SETUP]`,用户自己点
    /// `[TASK]` tab 看进度——避免误选目录后被强制离开 picker 页
    /// (`§3.8.7.1` product-lens review 反馈)。
    active_page: Page,
    /// 当前 Task 页展示哪个 task(v1: 总是 `tasks.active`,因为只有
    /// 一个 slot)。v1.1:用户可在多 active 间切换。
    viewing_task: Option<TaskId>,
    /// per-file 进度,key 是 file_index(§3.8.3 `Progress.file_index`)
    progress: HashMap<usize, ProgressInfo>,
    /// stderr log ring buffer(§3.8.6:上限 1000 行)
    log: VecDeque<LogLine>,
    /// 历史记录(v1.1 启用——v1 不展示历史)
    #[allow(dead_code)]
    history: Vec<EncodingRecord>,
    /// UI 侧选中的输入目录(由 DirPicker 设置,Start 按钮读取)
    input_dir: Option<PathBuf>,
    /// 最近一次 fatal 错误(由 `Event::Error` 设置;UI 在 header 顶部展示)
    last_error: Option<String>,
    // NOTE: v1 之前有两个 analyze-related 字段(`analyze_progress`,
    // `analyze_results`)和对应的 `AnalyzeProgressInfo` struct。
    // §3.8.7.2 redesign 把 `Command::Analyze` 路径删了(Start
    // 自动 DryRun + Confirm 已经能完成同样的用户意图),这俩字段
    // 同步移除;U22 analyze state 假设作废。

    /// 当前 active page tab。`Setup` 是初始值;4 个 page 之间
    /// 不自动切换(用户手动点 tab)。Reducer 的 `UserClickedTab`
    /// 直接写这个字段(可测试)。
    pub active_page: Page,
    /// 当前 Task 页展示哪个 task(v1: 总是 `tasks.active`,因为只有
    /// 一个 slot)。v1.1:用户可在多 active 间切换。
    pub viewing_task: Option<TaskId>,
    /// per-file 进度,key 是 file_index(§3.8.3 `Progress.file_index`)。
    /// v1: 单一 active task 的进度;v1.1: `HashMap<TaskId, HashMap<...>>`。
    pub progress: HashMap<usize, ProgressInfo>,
    /// stderr log ring buffer(newest last)。混合 ffmpeg stderr
    /// (`Event::LogLine` → `LogSource::FfmpegStderr`)和 lib 内部
    /// tracing 事件(`Msg::LibLogLine` → `LogSource::LibInternal`),
    /// 上限 `LOG_RING_CAP = 1000`。
    pub log: VecDeque<LogLine>,
    /// Parquet-loaded history(per-file EncodingRecord)。和
    /// `task_history` 概念不同:这个是 cross-session 的 Parquet
    /// reload 结果(`Command::LoadHistory`),v1.1 启用。
    pub history: Vec<EncodingRecord>,
    /// 当前选中的输入目录(DirPicker 设置)。
    pub input_dir: Option<PathBuf>,
    /// 最近一次 fatal error(任何来源)。UI 在 header 下方展示
    /// 红色 banner。
    pub last_error: Option<String>,
}

/// 当前批的元信息;由 `Event::BatchStarted` 写入、`Event::BatchCompleted`/`BatchCancelled` 关闭。
///
/// **Phase-driven lifecycle** (§3.8.7.1): `phase` is set by the
/// reducer based on dispatched commands and incoming events. v1
/// 单一 task 的元信息。Task 是 UI 端的 gating unit(per review 反馈
/// ——guard 走 task 存在性,不是 batch id 巧合)。Lib-side batch id 是
/// task 的映射字段,在 `Event::BatchStarted` 时填充。
///
/// **Phase-driven lifecycle** (§3.8.7.1): `phase` 由 reducer 设置
///(基于派发的 command + 收到的 event)。v1 capacity = 1,所以这个
/// struct 总是出现在 `TaskQueue.active`;v1.1 可以出现在 `pending`。
struct TaskInfo {
    /// UI-side task id。reducer 自己分配的递增 counter(从 0 开始)。
    /// **Guard 走 TaskId 比较 + TaskQueue capacity 检查,跟 lib 的
    /// batch id 解耦**——lib 的 `next_batch_id` 怎么跳不影响 UI 的
    /// gating 决策(per review feedback:避免 id-as-coincidence 反模式)。
    task_id: TaskId,
    /// Lib-side batch id 映射(`Event::BatchStarted.id`)。`None`
    /// 直到 `Event::BatchStarted`——task 已 submit 但 lib 还没确认
    /// 时为 None。v1 单 task 模式下通常 0,但 task queue 不依赖
    /// 此值的连续性。
    batch_id: Option<u64>,
    /// Lifecycle phase — drives the Task page header badge and
    /// the contextual button label.
    phase: BatchPhase,
    /// Input directory captured at Start time. Self-describing
    /// (the Task page can show the working path even if the user
    /// has navigated elsewhere via the now-disabled dir picker).
    dir: PathBuf,
    /// Total file count discovered by DryRun (after extensions
    /// filter). Equals `files.len()` passed to `Command::DryRun`.
    total_files: usize,
    /// Files that would actually be encoded (outcome = `Succeeded`) —
    /// DryRun doesn't invoke ffmpeg, so `ReducedFps` cannot appear
    /// (per §3.8.2). Set on `Event::DryRunFinished`; displayed on
    /// Task page during `AwaitingConfirm` / `Encoding` / `Done`.
    to_encode: usize,
    /// Files that would NOT be encoded. Since re-encoding is the rule
    /// (§3.3.4), these are the files the prepare step failed on —
    /// nothing is skipped any more. Set on `Event::DryRunFinished`.
    to_failed: usize,
    /// Files encoded so far during `Encoding`. Updated by
    /// `Event::FileCompleted`; displayed in the progress bar.
    encoded: usize,
    /// Set to true once Cancel lands; reducer ignores subsequent
    /// `Event::Progress` / `FileCompleted` until `BatchCancelled`
    /// or new `BatchStarted`.
    cancelled: bool,
    /// Set to true once `BatchCompleted` / `BatchCancelled` /
    /// `Event::Error` fires. Combined with `phase = Done` to
    /// gate re-arming.
    finished: bool,
    /// Records accumulated during `Encoding` (post-confirm).
    /// `FileCompleted` pushes one per file; `BatchCompleted`
    /// replaces with the lib's authoritative vec.
    records: Vec<EncodingRecord>,
    /// Records from the DryRun phase. Kept until the next task
    /// lifecycle starts so the user can scroll back through the
    /// plan during `Encoding` / `Done`. Overwritten on each new
    /// `Event::DryRunFinished`.
    dry_run_records: Vec<EncodingRecord>,
}

/// UI-side task id — timestamp-based(`yyyymmddhhmmss` 格式 per 你的
/// spec),queue 和 history 共享同一 ID 空间。任务从 active → history
/// 移动时 TaskId 保持不变(没有 re-key),所以跨 queue/history 查找
/// 同一任务的代码路径统一。
///
/// **Format**: `chrono::Local::now().format("%Y%m%d%H%M%S")` → 14
/// char 字符串如 `20260916142301`。Lexicographic 排序 = 时间排序。
///
/// **Collision handling**: 同秒内多次 `TaskId::new()`(rapid click
/// Start 后 Cancel 又 Start)用 `-N` 后缀区分——`20260916142301`,
/// `20260916142301-1`, `20260916142301-2`。reducer 维护一个
/// `current_second_counts: HashMap<String, usize>` 来分配后缀。
///
/// **Scope**: `TaskId` 只在 session 内唯一。Cross-session 不需要
/// 唯一——`state.history` 通过 Parquet reload 拿到 EncodingRecord
/// 列表,每条 record 带 `batch_id`(lib-side 计数,重启清零),v1.1
/// 按 batch_id group by 渲染成 TaskInfo 视图。所以**不**需要
/// 跨进程跨重启的 TaskId 全局唯一,也不需要把 task_history 写到
/// 另一个 SQLite / JSONL store——现有的 Parquet 已经是数据后端
/// (per review 反馈,避免引入新 store 类型)。
///
/// **Why not u64 counter**: u64 counter 也行(单调递增,简单),但
/// timestamp 形式对用户可读(看 TaskId 就知道任务大概什么时候开始),
/// 也方便 log 排序。tradeoff:14 chars vs 8 bytes。
///
/// **Time zone**: 用 `chrono::Local` 不是 `Utc`。单用户 desktop app,
/// 看 ID 的人 = 生成 ID 的机器,本地时间更有意义。Cross-timezone
/// 场景(旅行)不适用(用户不会在两台机器上跑同一个 task queue)。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct TaskId(String);

impl TaskId {
    fn new() -> Self {
        TaskId(chrono::Local::now().format("%Y%m%d%H%M%S").to_string())
    }
}

/// Task queue,v1 capacity = 1。`active` 是当前正在跑(Analyzing /
/// AwaitingConfirm / Encoding)或刚完成(Done)的 task;v1.1 加
/// `pending: Vec<TaskInfo>` 之后,新提交的 task 在 `pending` 排队,
/// active 完成后再 promote。
///
/// **Gating unit 是 TaskId / TaskQueue capacity**,不是 batch id——
/// reducer 的 gate 检查是 `tasks.active.is_some() || tasks.pending.len() >= capacity`,
/// 然后再决定:capacity 没满 → dispatch DryRun(立即执行, v1)或 push 到
/// pending(v1.1)。v1 没 pending,所以永远是 `tasks.active.is_some()` 检查。
struct TaskQueue {
    /// v1: 1。v1.1: lift to N (configurable; `RENENCODER_MAX_CONCURRENT`
    /// env var 或 AppConfig 字段)。
    capacity: usize,
    /// 当前 active task(v1: 最多 1)。`None` = queue empty。
    active: Option<TaskInfo>,
    /// Pending tasks(v1: 永远空)。v1.1: 按 `submitted_at` 顺序 FIFO。
    pending: Vec<TaskInfo>,
}

/// Lifecycle phase of a task (per task, not global — different tasks
/// in the queue can be in different phases in v1.1). Drives the Task
/// page header badge, the contextual button label, and the dir
/// picker's enabled state (§3.8.7.1 state machine).
/// Picker's enabled state (§3.8.7.1 state machine).
///
/// v1.2 redesign: dropped `AwaitingConfirm` phase. The dry-run
/// + encode flow is now 1-click:
///   - Pick directory → auto-dispatch `Command::DryRun` (Analyzing).
///   - On `Event::DryRunFinished` with `to_encode > 0`, auto-dispatch
///     `Command::StartBatch` (Encoding). Button was already visible
///     and clickable during the Analyzing→Encoding gap; the click
///     is now a no-op because we're already Encoding.
///   - On `to_encode == 0`, leave phase as Analyzing and surface
///     the count summary; the button stays disabled.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BatchPhase {
    /// `Command::DryRun` dispatched; awaiting `Event::DryRunFinished`.
    /// Dir picker disabled. Start button disabled (analyze in flight).
    Analyzing,
    /// `Command::StartBatch` dispatched; awaiting `BatchCompleted` /
    /// `BatchCancelled` / `Event::Error`. Progress bar active.
    /// Dir picker disabled. Start button shows "Cancel ✕".
    Encoding,
    /// Final state after the batch lifecycle ends. Dir picker
    /// re-enables. Start button shows "Re-run ▸" (clicking
    /// transitions back to Analyzing for a new cycle).
    Done,
}

/// 单文件进度;由 `Event::Progress` 更新
struct ProgressInfo {
    frame: u64,
    fps: f64,
    speed: f64,
    time_secs: f64,
    eta_secs: Option<f64>,
}

/// 单行 stderr;由 `Event::LogLine` 推送。
struct LogLine {
    file_index: usize,
    line: String,
    /// 接收时间(chrono::DateTime<Utc>);UI 按此排序展示
    at: DateTime<Utc>,
}

enum Msg {
    /// User picked a directory via native folder dialog (§3.8.7
    /// `DirPicker` row). Updates `state.input_dir`. Rejected if
    /// a batch is active (no effect, no error — the picker is
    /// disabled in the UI during active phases anyway).
    UserSelectedDir(PathBuf),
    /// **2-phase semantic** (§3.8.7.1 state machine). The Start
    /// button label changes per phase, but the message stays the
    /// same — the reducer dispatches based on current `BatchPhase`:
    ///   - `None` / `Done`  → `Command::DryRun` (begin analysis)
    ///   - `AwaitingConfirm` → `Command::StartBatch` (user confirmed)
    ///   - `Analyzing` / `Encoding` → ignored (busy-gate)
    UserClickedStart,
    /// Cancel current operation. Best-effort during `Analyzing`
    /// (lib's Analyze handler doesn't poll cancel_token — see
    /// §3.8.7.4 limitations). During `Encoding`, dispatches
    /// `Command::Cancel` to the lib actor (which kills ffmpeg
    /// via cancel_token). During `AwaitingConfirm`, clears the
    /// local batch state (no command needed).
    UserClickedCancel,
    /// Reload history records from disk. Manual dispatch via the
    /// Refresh button on the `[HISTORY]` page. Auto-dispatch on
    /// app start is deferred to v1.1 (§3.8.7.4 limitations).
    #[allow(dead_code)]
    UserClickedLoadHistory,
    /// User clicked a top-bar tab(§3.8.7.2 Page structure).
    /// Reducer writes `state.active_page`. No lib command; purely
    /// UI-side routing.
    UserClickedTab(Page),
    /// Wrapper for lib → UI events. All lib events flow through
    /// here; reducer routes them in `apply_event`.
    Lib(Event),
}

/// 唯一的状态变更入口。所有视图只读 state;所有写都走这里。
///
/// **Phase-aware `UserClickedStart`** (v1.2 — single Task-tab button):
///
/// ```text
///   ┌─ phase == None + tasks.active.to_encode == 0
///   │   → build params, collect files,
///   │     tasks.active = Some(Analyzing { to_encode: 0, .. }),
///   │     send Command::DryRun
///   │   (typical "first pick" case: button shows "Analyze…" disabled)
///
///   ├─ phase == None + tasks.active.to_encode > 0
///   │   → set phase = Encoding,
///   │     tasks.active.records.clear(),
///   │     send Command::StartBatch
///   │   (typical "second pick" or post-DryRun case: button shows
///   │   "Encode N files ▸"; user re-confirms the plan)
///
///   ├─ phase == Done      → same as None + to_encode == 0
///   │                     (re-arm via "Re-run ▸" button)
///
///   └─ phase == Analyzing | Encoding
///                         → set last_error "task in progress",
///                           return early (busy-gate)
/// ```
///
/// **Auto-dispatch on dir pick** (v1.2): `Msg::UserSelectedDir`
/// triggers `Command::DryRun` immediately if no active task is
/// in flight. This is what makes the "1 click" experience work —
/// the user picks a dir, the dry-run starts, and the button
/// transitions to "Encode N files ▸" without any further input.
///
/// **`Event::DryRunFinished` post-processing** (v1.2):
/// 1. Compute `to_encode` / `to_failed` from the records.
/// 2. **If `to_encode > 0`**: auto-dispatch `Command::StartBatch`,
///    transition `phase = Encoding`, clear records. The UI button
///    flips to "Cancel ✕".
/// 3. **If `to_encode == 0`**: leave `phase = Analyzing` (no auto
///    encode), surface the count summary on the Task page. The
///    "Encode N files ▸" button stays disabled — this is the bug
///    fix that makes "encode 0 files" structurally impossible.
///
/// **Phase-aware `UserClickedCancel`** (v1.2 — analyze stays
/// non-cancellable per design decision):
///   - `Encoding`: send `Command::Cancel`. The lib polls
///     cancel_token between progress reads.
///   - `Analyzing`: button is **disabled** during analyze (no
///     review #100 implementation — best-effort only).
///   - `None` / `Done`: ignored (no-op).
fn reduce(state: &mut AppState, msg: Msg, cmd_tx: &Sender<Command>, config: &AppConfig) {
    match msg {
        Msg::UserSelectedDir(path) => {
            // Reject while a batch is active (UI also disables
            // the picker — defense-in-depth in case the user
            // manages to click via keyboard nav).
            if let Some(t) = &state.tasks.active {
                if !matches!(t.phase, BatchPhase::Done) {
                    return;
                }
            }
            state.input_dir = Some(path);
        }
        Msg::UserClickedStart => {
            // (full implementation lives in rencoder_ui/src/msg.rs;
            // the skeleton below documents the dispatch logic)
            let phase = state.tasks.active.as_ref().map(|t| t.phase);
            match phase {
                None | Some(BatchPhase::Done) => {
                    // Collect files, build params, dispatch DryRun,
                    // optimistically transition to Analyzing.
                }
                Some(BatchPhase::AwaitingConfirm) => {
                    // Dispatch StartBatch, transition phase to Encoding.
                }
                Some(BatchPhase::Analyzing) | Some(BatchPhase::Encoding) => {
                    state.last_error = Some("task in progress".into());
                }
            }
        }
        Msg::UserClickedCancel => {
            let phase = state.tasks.active.as_ref().map(|t| t.phase);
            match phase {
                Some(BatchPhase::Analyzing) | Some(BatchPhase::Encoding) => {
                    // Mark locally so DryRunFinished / BatchStarted
                    // arriving after Cancel know to ignore (per
                    // §3.8.7.1 cancel-during-Analyzing race fix).
                    if let Some(t) = state.tasks.active.as_mut() {
                        t.cancelled = true;
                    }
                    let _ = cmd_tx.send(Command::Cancel);
                }
                Some(BatchPhase::AwaitingConfirm) => {
                    state.tasks.active = None;  // abandon the analysis
                }
                None | Some(BatchPhase::Done) => {}
            }
        }
        Msg::UserClickedLoadHistory => {
            let _ = cmd_tx.send(Command::LoadHistory { limit: Some(100), after: None });
        }
        Msg::UserClickedTab(page) => {
            // No lib command; pure UI routing.
            state.active_page = page;
        }
        Msg::Lib(Event::DryRunFinished { records }) => {
            // Transition Analyzing → AwaitingConfirm.
            // Compute to_encode / to_failed from records.
            //
            // **EncodingOutcome match scope**: DryRun records only
            // carry `Succeeded` / `Failed` —
            // `ReducedFps` requires running ffmpeg to detect
            // source-fps > cap, so it cannot appear here (DryRun
            // doesn't invoke ffmpeg, see §3.8.2). Matching only
            // `Succeeded` is correct.
            //
            // **Cancel-during-Analyzing guard**: if user clicked
            // Cancel during Analyzing (set `cancelled = true`),
            // DryRunFinished still arrives — discard it so the
            // user doesn't end up on AwaitingConfirm for a batch
            // they thought they cancelled.
            if let Some(t) = state.tasks.active.as_mut() {
                if t.cancelled {
                    state.tasks.active = None;
                    return;
                }
                t.dry_run_records = records.clone();
                t.to_encode = records.iter().filter(|r| matches!(r.outcome,
                    EncodingOutcome::Succeeded)).count();
                t.to_failed = t.total_files - t.to_encode;
                t.phase = BatchPhase::AwaitingConfirm;
            }
        }
        Msg::Lib(Event::BatchStarted { id, total_files }) => {
            // Transition AwaitingConfirm → Encoding (lib accepted
            // the StartBatch). Record lib's batch_id on the active
            // task (id-as-mapping, **不**参与 gating 决策——per review
            // feedback,避免 id-as-coincidence 反模式)。
            if let Some(t) = state.tasks.active.as_mut() {
                t.batch_id = Some(id);
                t.total_files = total_files;
                t.phase = BatchPhase::Encoding;
                t.records.clear();
            }
        }
        Msg::Lib(Event::Progress(p)) => { state.progress.insert(p.file_index, p); }
        Msg::Lib(Event::LogLine(l)) => {
            state.log.push_back(l);
            while state.log.len() > 1000 { state.log.pop_front(); }
        }
        Msg::Lib(Event::HistoryLoaded { records }) => { state.history = records; }
        Msg::Lib(Event::BatchCompleted { records }) => {
            // **Move to history on Done** (per spec: queue 和 history
            // 共享 TaskId 空间,完成的任务归入 history 不 re-key)。
            // `tasks.active.take()` 移除 task 并返回 ownership;insert
            // 到 `task_history` 前置(newest-first)。
            let done = state.tasks.active.take();
            if let Some(mut t) = done {
                t.records = records;
                t.finished = true;
                t.phase = BatchPhase::Done;
                state.task_history.insert(0, t);
            }
            state.progress.clear();
        }
        Msg::Lib(Event::BatchCancelled { completed, total }) => {
            // Same move-to-history pattern as BatchCompleted。
            let done = state.tasks.active.take();
            if let Some(mut t) = done {
                t.cancelled = true;
                t.finished = true;
                t.phase = BatchPhase::Done;
                state.task_history.insert(0, t);
            }
            state.last_error = Some(format!("cancelled after {completed}/{total}"));
        }
        Msg::Lib(Event::Error { message, .. }) => {
            state.last_error = Some(message.clone());
            // During Encoding: task 跑到一半失败——同样归入 history
            // (per spec:queue 和 history 共享 TaskId,失败也算完成)。
            // During Analyzing: lib 拒绝了 DryRun dispatch (busy-gate
            // 失败),task 从未真正开始——不归入 history(per spec:
            // "当一个任务完成后"——Analyzing 阶段不算完成)。
            // During AwaitingConfirm: no-op (lib 还没收到任何 command)。
            match state.tasks.active.take() {
                Some(mut t) if matches!(t.phase, BatchPhase::Encoding) => {
                    t.finished = true;
                    t.phase = BatchPhase::Done;
                    state.task_history.insert(0, t);
                }
                Some(t) if matches!(t.phase, BatchPhase::Analyzing) => {
                    // Drop without history entry.
                    drop(t);
                }
                Some(t) => {
                    // AwaitingConfirm (or other) — restore.
                    state.tasks.active = Some(t);
                }
                None => {}
            }
        }
        // Event::ActorExited handler intentionally absent — see
        // §3.9 (Strategy 2: drain thread exits naturally when
        // lib drops its sender).
        _ => {}
    }
}
```

- 所有用户输入转成 `Msg` → reducer 决定是否反发 `Command`。
- 所有 lib 事件包成 `Msg::Lib(Event)` → reducer 更新 `state`。
- 组件只读 `state`，不直接调 lib。
- 测试：构造 `Vec<Msg>` 灌进 reducer 断言 `state`——不需 Dioxus runtime。

#### 3.8.7.1 Workflow state machine

> **TL;DR** — 一个 task 走六个 phase(`None` / `Analyzing` /
> `AwaitingConfirm` / `Encoding` / `Done` / `Cancelled`),`BatchPhase`
> 是唯一真相。Cancel 是**乐观的**:用户点 Cancel 的瞬间
> `phase = Cancelled`,lib 的延迟 `Event::BatchCancelled` 被 phase
> filter 丢弃——避免"badge 还闪 encoding"的矛盾 UI。

**State diagram** (v1.2 follow-up #2 — AwaitingConfirm restored
2-click Confirm; Cancelled phase added for terminal-cancel
semantics):

```text
                    UserSelectedDir / UserClickedStart
   [None] ──────────────────────────────────────────────> [Analyzing]
      ▲                                                          │
      │                                                          │ Event::DryRunFinished
      │                                                          │ (to_encode > 0)
      │                                                          ▼
      │                                                  [AwaitingConfirm] ─┐
      │                                                          │           │ UserClickedStart
      │                                                          ▼           │ (dispatches StartBatch)
      │ UserClickedCancel (optimistic) ┌──────────> [Cancelled]   │           ▼
      │                                │                ▲          │      [Encoding] ────────────┐
      │                                │                │          │         │                    │
      │                                │ Event::BatchCancelled     │         │ Event::BatchCompleted│
      │                                │ (LATE, ignored by filter) │         │ Event::BatchCancelled│
      │                                │                           │         │ Event::Error         │
      │                                │                           │         ▼                    │
      │                                └───────────────────────────┴───> [Done] ◄─────────────────┘
      │                                                                       │
      └───────────────────── UserSelectedDir / Re-pick ──────────────────────┘
```

**Per-phase semantics** (v1.2 follow-up #2 — 6 states; 2-click
Confirm restored; cancel is optimistic):

| Phase | 来源 | Dir picker | Button label | Button enabled | File list |
|-------|------|-----------|--------------|----------------|-----------|
| `None` + dir 未选 | initial | enabled | (placeholder) | **disabled** | empty |
| `Analyzing` | `Command::DryRun` in flight | enabled (re-pick resets) | "Analyze…" | **disabled** | empty (dry-run results in toast, not list) |
| `AwaitingConfirm` | `Event::DryRunFinished` (to_encode > 0) | enabled | **"Encode"** | **enabled** | empty (records → toast only) |
| `Encoding` | `Event::BatchStarted` (lib accepted `Command::StartBatch`) | enabled (re-pick cancels + resets) | "Cancel ✕" | **enabled** (dispatches `Command::Cancel`) | records (Pending / Encoding / Succeeded / Failed) |
| `Done` | `BatchCompleted` / late `BatchCancelled` (filtered to terminal) | enabled | (placeholder) | **disabled** (re-arm via dir re-pick) | records (final outcomes) |
| `Cancelled` | `Msg::UserClickedCancel` 立即设;`Event::BatchCancelled` late events ignored | enabled | **"Encode"** | **enabled** (re-click re-dispatches `StartBatch`) | records at cancellation point (Succeeded up to cancel, in-flight record absent — deleted by lib cleanup) |

**Key semantic decisions**:

1. **2-click Confirm restored** (v1.2 follow-up #2). After
   `DryRunFinished`, phase = `AwaitingConfirm`. User reviews the
   dry-run summary (delivered via **toast**, not persistent file
   list — see `§3.8.7.6`), then clicks "Encode" to dispatch
   `StartBatch`. We do NOT auto-encode on positive dry-run count —
   the user must opt in.

2. **Button label = "Encode" (no "N files")**. We don't design for
   encoding a subset of files; the user either encodes all
   eligible or none. Drop the count from the button label; the
   count is in the toast summary.

3. **Cancel is optimistic** (`§3.8.7.6`). On `Msg::UserClickedCancel`,
   reducer sets `phase = Cancelled` **immediately** and dispatches
   `Command::Cancel` to lib. All subsequent `Event::*` for this
   batch are filtered out by a phase guard at the top of
   `apply_event`. This eliminates the ~1.5s window where the
   badge flickered "encoding" after user clicked cancel — see
   `§3.8.7.6` for the lib cancel latency breakdown.

4. **`Done` requires dir re-pick** to re-arm. The Done phase
   leaves the button disabled; picking a directory (or re-picking
   the same one) transitions back to `Analyzing` via a fresh
   dry-run. This gives the user an explicit "I'm starting over"
   gesture and avoids stale dry-run records being silently re-used.

5. **`Cancelled` → "Encode"** (not "Re-run"). After cancel, the
   button shows "Encode" enabled. Clicking re-dispatches
   `StartBatch` with the dry-run records already in
   `state.tasks.active.dry_run_records` — no second dry-run
   unless the user changes the directory.

**Phase guard for late lib events** (`apply_event`):

```rust
fn apply_event(state: &mut AppState, evt: Event) {
    // Phase guard: once a batch is Cancelled, the UI trusts the
    // user's click over lib's eventual confirmation. All subsequent
    // batch-scoped events for the current task are dropped. This
    // is the optimistic-cancel invariant.
    if state.tasks.active.as_ref().map_or(false, |t| t.phase == BatchPhase::Cancelled) {
        match evt {
            Event::BatchStarted { .. }
            | Event::FileStarted { .. }
            | Event::Progress { .. }
            | Event::FileCompleted { .. }
            | Event::BatchCompleted { .. }
            | Event::BatchCancelled { .. }
            | Event::LogLine { .. }
            | Event::Error { .. } => return,
            // History / config / dry-run events are not batch-scoped
            // — fall through.
            _ => {}
        }
    }
    match evt { /* ... */ }
}
```

The guard is conservative (drops everything for the cancelled batch
including Error / LogLine) because the user's intent has already
been recorded; lib cleanup happens silently in the background.

**No auto-switch** (carried over from v1.1):用户在 `[SETUP]` 编辑配置后,UI **不自动切换**到 `[TASK]` 页——auto-switch 会把误改配置的用户困在 TASK 页。Setup 编辑是 real-time (每个字段的 `oninput` 直接调 `AppConfig::save`),所以切换到 Task 之前配置已落盘。Dir picker 在所有 phase 都 enabled (per table above)——用户随时可以 re-pick 触发 reset。

**TaskQueue capacity** (v1: 1, v1.1: N):guard 走 `tasks.active.is_some() || tasks.pending.len() >= tasks.capacity`,**不**走 batch id 比较(per review 反馈 ——task 是 gating unit,batch id 只是映射字段)。v1 capacity = 1 意味着最多一个 active task——空 queue 时 `UserClickedStart` 派 DryRun(填 `tasks.active`);active 非空 + phase != Done 时,start 被拒(reducer 设 `last_error`)。v1.1 提升 capacity 到 N 后,多余的 task push 进 `tasks.pending`(FIFO),active 完成时 promote。详见 §3.8.7 `TaskQueue` 结构。

**Move-on-Done transition** (active → history):task 进入 `Done` phase 时(`BatchCompleted` / `BatchCancelled` / Encoding-phase `Error`),reducer 用 `tasks.active.take()` 取出 task,改 phase = Done,然后 `task_history.insert(0, t)` 前置——queue 和 history 共享 `TaskId`,跨两者查找统一。**Error during Analyzing 不归入 history**(DryRun 都没成功,task 从未开始)。Done 的 task 仍展示在 `[TASK]` 页(viewing_task 路由),但 `tasks.active = None` 让 capacity check 通过——用户可以立即 Start 下一个 task。

**Cancel during Analyzing** 是 v1 限制(`§3.8.7.4`):lib 的 `Command::Analyze` handler 跑 `analyze_records_with_probe_and_progress_parallel` 全程不轮询 cancel_token,所以 UI 派 `Command::Cancel` 时 lib 不真正中断分析。`Event::AnalyzeFinished` 仍会到达,UI 把它当正常结果处理(显示完整 records)。v1.1 在 analyze handler 里加 cancel_token 轮询。

**Reducer 路由表**(`Msg::UserClickedStart` 的 dispatch 逻辑)——见上面 `reduce` 代码块注释里的 phase match。

#### 3.8.7.2 Page structure(4 pages + footer log)

> **TL;DR** — 4 个独立 page + 全局 top-bar(tabs + Exit),每 page 一个
> contextual 按钮(在 page header top-right)。Log 在所有 page 底部
  都有 footer,full log 在 `[LOG]` page。

**Top-bar 结构**:

```text
┌─ RENENCODER ───────────────────────────────[Exit]─┐
│ ┌─────┐ ┌─────┐ ┌─────┐ ┌─────┐  │
│ │SETUP│ │TASK │ │HIST │ │LOG │   │
│ └─────┘ └─────┘ └─────┘ └─────┘          │
├──────────────────────────────────────────────┤
```

- 4 个 page tab 横排,active tab 下划线 + 加粗
- Tab click 派 `Msg::UserClickedTab(Page)`,reducer 写 `state.active_page`(per §3.8.7 design);App() 通过 `use_signal_sync` 镜像此字段给 Dioxus 渲染层
- `[Exit]` 在最右侧,所有 page 可见。**v1 行为**(per §3.8.7.4):点 Exit 先派 `Command::Cancel`(若 batch active),等 `Event::BatchCancelled`,再 `window().close()`;若 no batch active,直接 `window().close()`

**Per-page layout** (active tab 在 TopBar 已命名,page body 不再带 `──[ NAME ]────` header——冗余):

| Page | Body | 按钮位置 | 按钮 states |
|------|------|----------|-------------|
| `[SETUP]` | live-edit form(format radios / crf / max fps / extensions) + save-status 行 | (无 contextual button;setup page 是配置编辑页,不是 action 页) | — |
| `[TASK]` | DirPicker + count summary + `[● phase] badge` + scrollable file list + ProgressPanel | **底部 sticky**(随 task phase 动态 show / hide) | `Analyze ▸`(dir 选了无 history) / `Re-run ▸`(history 非空) / 禁用 `Analyze…`(Analyzing 中) / `Cancel ✕`(Encoding 中) |
| `[HISTORY]` | history table(source / outcome / size) | body 顶部 inline(无 page header 占用 row) | `[ Refresh ⟳ ]`(per v1.1 deferred——见 §3.8.7.4) |
| `[LOG]` | 完整 log scrollback(全 1000 行) | body 顶部 inline | `[ Clear ]` |

**Why drop the page headers**: `[TASK]` / `[HISTORY]` / `[LOG]` 文字标签与 TopBar 的同名 tab 重复。原 layout 把 `[NAME]────` 放在 page header 是为了给 top-right button 找一个 row;但 button 完全可以放到 page 底部(随 phase 动态 show / hide)。这样省 ~50px 垂直空间,且按钮位置贴近用户的视线终点(file list 看完即按按钮,不需要回头找)。`[SETUP]` 已经在 v1.2 早期工作里去掉 header(review #133 part 3b)——这次扩展到剩余 3 页保持一致。

**Task page bottom button states** (v1.2 follow-up #2 — 6 states; see
`§3.8.7.1` for the canonical table):

```
phase == None                -> placeholder (disabled)
phase == Analyzing           -> DISABLED 'Analyze…'
phase == AwaitingConfirm     -> enabled 'Encode'           (no "N files" — see §3.8.7.1 decision 2)
phase == Encoding            -> enabled 'Cancel ✕'
phase == Done                -> placeholder (disabled)     (re-arm via dir re-pick)
phase == Cancelled           -> enabled 'Encode'           (re-click dispatches StartBatch with cached dry-run)
```

**Bottom-button CSS** (`rencoder_ui/assets/style.css`): `--task-button-bar` sticky 到 viewport 底部 (`position: sticky; bottom: 0`),背景 + 上边框与 main panel 区分,占 ~50px。Footer log 在 button bar 下方(footer 不动——per spec)。

**Footer log** (所有 page 都有):

```text
├──────────────────────────────────────────────┤
│ LOG · last 5 lines                            │
│ 14:23:01 [0] probing file 1/12                │
│ 14:23:02 [0] av1 1920x1080 @ 30fps             │
│ ...                                           │
└──────────────────────────────────────────────┘
```

- 永远在 page content 下方,占底部 ~5 行高度
- 同一份 `state.log` 数据(`[LOG]` page 渲染全 buffer;footer 渲染末 5 行)
- 滚动 `[LOG]` page 时 footer 不动(footer 只是 viewport)

**Toast notifications** (v1.2,所有 page 都有,叠加在内容上方):

Transient UI feedback for user-action outcomes that should NOT occupy body space。区别于 `state.last_error` 的全局 banner:

| 通道 | 用途 | 显示位置 | 持久性 |
|------|------|----------|--------|
| `state.last_error` banner | lib-side / 编码失败 / 系统错误(`Event::Error` 派生的) | TopBar 下方一行 | 持久(用户手动 dismiss 或新事件覆盖) |
| Toast | user-action feedback(点 Analyze / pick dir / save 等的反馈) | Viewport 右上,叠加在内容上方 | 4 秒后自动消失,带 × 手动 dismiss 按钮 |

**Toast 触发的 reducer paths** (v1.2):

| 用户动作 / lib 事件 | Toast 文案 | kind |
|---------------------|-----------|------|
| 点 Analyze / Re-run, input_dir = None | `"no input dir selected"` | error |
| 点 Analyze / Re-run, dir 不可读 | `"cannot read {dir} (permission denied or path missing)"` | error |
| 点 Analyze / Re-run, dir 空 | `"no files in {dir} matching extensions {exts}"`(或 `"no files in {dir}"`) | warning |
| DryRunFinished, to_encode == 0 | `"0 encodable files ({n} failed); pick a different dir"` | warning |
| DryRunFinished, dir 中途变空 | `"no files in {dir} (file list changed since DryRun)"` | error |
| DryRunFinished, dir 中途消失 | `"input dir missing or unreadable after dry-run"` | error |
| Setup page save 失败(`save failed:` 前缀) | `"Save failed: {detail}"` | error |
| Setup page save 成功(`Event::ConfigSaved`) | (不弹 toast;Setup 页已有 save-status 指示) | — |
| Cancel 完成 | (不弹 toast;`state.last_error = "cancelled after {n}/{m}"` 已经显示) | — |
| lib Event::Error (非以上 save-failed) | 走 `state.last_error` banner(不弹 toast) | — |

**Toast 数据模型** (`AppState`):

```rust
pub toasts: VecDeque<Toast>,

pub struct Toast {
    pub id: u64,                  // monotonic counter for keying
    pub message: String,
    pub kind: ToastKind,          // Info | Warning | Error
    pub created_at: DateTime<Utc>,
    pub duration_ms: u32,         // default 4000
}

pub enum ToastKind { Info, Warning, Error }
```

- `VecDeque` (不是 `Vec`)— `pop_front` 用于 prune 过期 toast
- Capacity: 5(超过时 pop_front 最老的;够用就好,不阻塞 UI)

**Reducer helper**:

```rust
fn push_toast(state: &mut AppState, msg: impl Into<String>, kind: ToastKind) {
    state.toasts.push_back(Toast {
        id: state.next_toast_id,
        message: msg.into(),
        kind,
        created_at: Utc::now(),
        duration_ms: 4000,
    });
    state.next_toast_id = state.next_toast_id.wrapping_add(1);
    while state.toasts.len() > 5 {
        state.toasts.pop_front();
    }
}
```

**Auto-dismiss** (`Toaster` 组件):

每 250ms 跑一次 `setInterval` (Dioxus `use_future` + `gloo_timers` 或自旋)— 扫描 `state.toasts`,把 `now - created_at >= duration_ms` 的项 `pop_front`。4s 默认寿命下,4 次扫描轮询后过期 toast 消失,延迟 < 250ms。

**Toast UI** (右上角 stacked):

```text
                                    ┌──────────────────────────┐
                                    │ ⚠ no input dir selected  ×│
                                    ├──────────────────────────┤
                                    │ ⚠ 0 encodable files...   ×│
                                    └──────────────────────────┘
```

- `position: fixed; top: 60px; right: 16px;` (60px 避开 sticky TopBar)
- z-index: 20(高于 TopBar 的 10,也高于 button bar 的 10)
- Stack 竖排,新 toast 在上(`flex-direction: column-reverse` 或插入到前端)
- 每条 toast: ~40px 高 + padding,左侧 colored stripe 表 kind,右侧 × dismiss 按钮
- 颜色:Info 用 accent(#16a34a),Warning 用 amber(#d97706),Error 用 red(#dc2626)
- 不阻塞 main content(fixed + 透明背景)— 用户可以同时操作 button bar 和 file list

**为什么不用 `state.last_error` 做这事**:`last_error` 是**全局** banner(顶部),`Event::Error` 派生,持续显示直到新事件覆盖或用户切 tab。Toast 是**局部**反馈(右上角),user-action outcome 专用,自动消失。两者并存:
- 编码过程中遇到 ffmpeg 错误 → last_error 顶部 banner(用户应该注意到)
- 用户点 Analyze,选了空目录 → toast 右上角(用户立刻看到,4s 后自动消失,不占据顶部 banner)

**v1.2 limitations table 新行** (§3.8.7.4):"User-action feedback 占 body 空间" — v1.2 fixed via toast。

**Active state 默认值**: app 启动时 active tab = `[SETUP]`,除非有未完成的 batch(没 v1 不存在)。Tab 切换走 reducer(`Msg::UserClickedTab(Page)` → `state.active_page = page`),保持 testable。

**Concrete 组件** (按 page 分组):

| Page | 组件 | 输入 | 渲染什么 |
|------|------|------|----------|
| global | `TopBar` | `active: Page`, `cmd_tx` | 4 tabs + Exit |
| global | `LogFooter` | `state` | 5 行 log |
| `[SETUP]` | `DirPicker` | `state`, `cmd_tx` | native folder dialog + 当前 path + disabled when batch active |
| `[SETUP]` | `SettingsPanel` | `config` | 只读 `[encode]` key/value list |
| `[TASK]` | `BatchStatus` | `state` | BATCH #N + phase badge (`[● ANALYZING]` / `[● READY]` / `[● ENCODING]` / `[● DONE]` / `[● CANCELLED]`) + ongoing counts |
| `[TASK]` | `ProgressPanel` | `state` | 进度条 + per-file table(Encoding 时) |
| `[TASK]` | `TaskFileList` | `state.tasks.active.records` (or `dry_run_records` during `AwaitingConfirm` pre-v1.2 follow-up #2) | per-file outcome 标签。**v1.2 follow-up #2**: `Analyzing` / `AwaitingConfirm` phase renders empty — dry-run records surface only as a toast (§3.8.7.6); `Encoding` / `Done` / `Cancelled` renders encode records |
| `[HISTORY]` | `HistoryPanel` | `state.history` | history records 表 |
| `[LOG]` | `LogPanel` (full version) | `state.log` | 全 1000 行 scrollback |

**Removed**:
- `UserClickedAnalyze` / `UserClickedPreview` Msg variants(Start 流程 auto-runs analyze)
- `DryRunPanel`(内容并入 `TaskFileList`)
- `AnalyzePanel`(无 standalone analyze action)
- `AnalyzeProgressInfo` / `state.analyze_progress` / `state.analyze_results`(subsumed)
- 7-button controls bar(替换为 per-page buttons)
- **v1.2 follow-up #2**: persistent dry-run file list rendering — `dry_run_records` surface only via toast (`§3.8.7.6`); the `AwaitingConfirm` phase has no file list area

#### 3.8.7.3 Visual design tokens

> **TL;DR** — 浅色背景 + 全 mono + 方括号 label + status badge。
> 设计思路:CLI power-user 的 GUI,不是 SaaS dashboard。

**Theme: 浅色 + warm off-white**(vs dark "ops center"——本项目走白天可用、像 README / 论文样的视觉)。

| Token | Value | 用途 |
|-------|-------|------|
| `--bg-app` | `#fafaf9` | app 背景,warm off-white |
| `--bg-surface` | `#ffffff` | 卡片 / section 背景 |
| `--bg-elev` | `#f4f4f3` | hover / active surface |
| `--bg-input` | `#ffffff` | input 字段 |
| `--border` | `#d4d4d2` | section border (1px) |
| `--border-strong` | `#a3a3a0` | table row separator |
| `--border-active` | `#6b6b68` | focus / hover |
| `--text` | `#1c1c1a` | primary text |
| `--text-dim` | `#6b6b68` | secondary text |
| `--text-mute` | `#9a9a97` | labels, hints |
| `--status-idle` | `#9a9a97` | gray dot |
| `--status-active` | `#16a34a` | green — ANALYZING / AWAITING / ENCODING |
| `--status-done` | `#2563eb` | blue — DONE |
| `--status-warning` | `#d97706` | amber — empty / partial |
| `--status-error` | `#dc2626` | red — error |
| `--accent` | `#16a34a` | button primary |
| `--accent-dim` | `#dcfce7` | button hover bg |

**Typography**:**JetBrains Mono** 全场(variable weight 200–800)。无 sans-serif fallback。Hierarchy 用 weight + size + letter-spacing 实现,不用 font family 切换。

| Token | Size / Weight / Tracking | 用途 |
|-------|--------------------------|------|
| `.brand` | 13px / 700 / +0.12em uppercase | top-bar "RENENCODER" |
| `.section-label` | 11px / 700 / +0.12em uppercase | `[ SETUP ]` `[ TASK ]` 等方括号 label |
| `.heading` | 13px / 600 | page header line |
| `.body` | 13px / 400 | 正文 |
| `.dim` | 12px / 400 / `--text-dim` | secondary |
| `.mute` | 11px / 400 / `--text-mute` | hints, hints |
| `.code` | 12px / 500 | 技术值("12 files", "45 crf") |
| `.path` | 13px / 400 / `--text-dim` | file path(略淡,有 prominence) |

**Status badge 格式**:`[● <phase>]` — 内嵌 dot,color 由 `--status-*` 控制;active phase 的 dot 用 `pulse 2s ease-in-out infinite` keyframe 慢 pulse 动画(`@keyframes pulse { 0%, 100% { opacity: 1; } 50% { opacity: 0.4; } }`)。

**Section label 格式**:`──[ SETUP ]──────[● AWAITING]───` — 两个 dash line 把 section 头包起来,左边 label,右边 status badge / page button(看 page 类型)。

**动画**:仅必要动画
- Phase transition:TASK section body fade + slide 4px,180ms ease-out
- Progress bar width:200ms transition on update
- Start button hover:bg shift to `--bg-elev`,无 glow / 无 scale
- 无 scroll-trigger / 无 parallax / 无 first-paint entry animation

**Anti-slop 检查**(为什么 NOT 这些):
- ❌ Inter / Roboto / system font(本项目用 mono)
- ❌ purple gradient on white(本项目无 gradient)
- ❌ glassmorphism / blur / shadow(深度用 bg tone shift,1px border)
- ❌ emoji icons in code(系统 no-emoji rule + skill 规则)
- ❌ centered card layouts(本项目用 header + 2-col grid)

**Spacing 节奏**:4 / 8 / 12 / 16 / 24px。所有 gap / padding 用这五个值之一。

#### 3.8.7.4 v1 已知 limitations

| 项 | 状态 | 后续 |
|----|------|------|
| Dir picker 锁 during whole batch lifecycle | **v1.2 follow-up #2 fixed**: dir picker 在所有 phase 都 enabled(per §3.8.7.1),包括 `Encoding` / `Done`。Encoding 中 re-pick = 取消当前 batch + reset;Done 中 re-pick = 触发新的 dry-run。AwaitingConfirm 中 re-pick = 同样 reset 到 None + 新的 dry-run | — |
| AwaitingConfirm phase (v1 2-click flow) | **v1.2 follow-up #2 restored** — after `DryRunFinished` (to_encode > 0), phase = `AwaitingConfirm` 不是 auto-encode。用户 review dry-run summary(toast)后点 `Encode`。Dry-run 0 结果还是走 toast 警告 + `None`,按钮 placeholder disabled | — |
| Cancel during Analyzing 是 best-effort | v1.2 **decision**: button is **disabled** during Analyzing (no cancel during analyze). lib 的 `Command::Analyze` handler 不轮询 cancel_token,UI 发 `Command::Cancel` 时 lib 不真正中断。`Event::AnalyzeFinished` 仍到达并被 reducer 当正常结果处理 | v1.3 加 analyze cancel_token 轮询 |
| "encode 0 files" 按钮可点击 | **v1.2 fixed**: Start button is **disabled** when `to_encode == 0`. `Event::DryRunFinished` only auto-dispatches `Command::StartBatch` when `to_encode > 0` | — |
| `state.analyze_progress` / `state.analyze_results` 字段保留但未 populate | workflow 改造后 Start 自动调 `Command::DryRun`(不是 `Command::Analyze`),analyze state 字段未 populate。代码里还在(`#[allow(dead_code)]`),变成 dead field——AppState 代码块里保留定义以备 v1.1 / U22 重启用 | v1.1 移除 dead 字段 + 删除对应 `Event::Analyze*` reducer arm |
| `UserClickedLoadHistory` 自动派 on app start | app 启动时 `use_hook` 派一次(per §3.8.7.2 history page);但目前没有显式 init code,history 是空直到用户手动 reload | v1.1 在 `use_hook` 里加 `cmd_tx.send(Command::LoadHistory { limit: Some(100), after: None })` |
| Cross-session task 导航缺失 | `task_history` 是 session 内 cache(进程退出即丢);`state.history` 是 cross-session 但以 per-file EncodingRecord 为单位。**没有** task 维度的 cross-session 视图——v1 看不到昨天跑的 batch 用了哪个 dir / 多少文件 | v1.1 启动时 load Parquet records → group by batch_id → 渲染成 TaskInfo 列表。数据后端已经是 Parquet(`encoding-*.parquet`),不引新 store |
| Settings inline edit | **v1.2 implemented**: `[SETUP]` 页每个字段 `<input>` `oninput` 直接调 `AppConfig::save` (real-time TOML atomic write via `write_atomic`). 没有 Save 按钮 | — |
| Page-header `[NAME]` 文字冗余 (`[TASK]` / `[HISTORY]` / `[LOG]` 与 TopBar tab 重名) | **v1.2 fixed**: drop `[TASK]` / `[HISTORY]` / `[LOG]` header rows。原 layout 把 header 留在 page 是为了给 top-right button 找一个 row——这版 button 改成 sticky 底部(随 phase 动态 show / hide),header 没有存在的必要。省 ~50px 垂直空间,按钮位置贴近 file list 看完后的视线终点。`[SETUP]` header 在 v1.2 早期已去(review #133 part 3b),这次扩展到剩余 3 页保持一致 | — |
| User-action feedback 占据 main body 空间 | **v1.2 fixed**: 加 toast 通知系统(右上角 fixed,4s 自动 dismiss,带 × 按钮)。`state.last_error` banner 仍负责 lib-side 错误(`Event::Error` 派生,持久);toast 负责 user-action 反馈(点 Analyze 后 dir 不可读 / DryRun 0 encodable / save 失败等),不占 body 空间。两通道并存:lib 错误 → banner;action feedback → toast。容量 5(FIFO) | — |
| Cancel UI flicker (`badge = ENCODING` while progress shows "cancelled") | **v1.2 follow-up #2 fixed**: optimistic cancel(`§3.8.7.6`)。`Msg::UserClickedCancel` 立即设 `phase = Cancelled`;所有后续 batch-scoped `Event::*` 被 `apply_event` 的 phase guard 丢弃。Badge 切换瞬时,与 progress 行同步 | — |
| Late `Event::BatchCancelled` 把 phase 翻成 `Done`(违反 cancel 终态语义) | **v1.2 follow-up #2 fixed**: phase guard 在 `apply_event` 入口丢弃 `BatchCancelled`(已经 Cancelled),不再触发 `phase = Done`。Button 保持 `Encode` 不变 | — |
| Cancel 不删 in-flight partial output | **v1.2 follow-up #2 fixed**(`§3.8.7.6`):`run_ffmpeg_with_progress` 在 SIGTERM cancel 分支 `std::fs::remove_file(argv.last())` 删 partial output。CLI 路径已经接 `PartialOutputHandle` Guard,这次把 actor 路径补齐 | — |
| Dry-run records 占据 body file list 空间 | **v1.2 follow-up #2 fixed**(`§3.8.7.6`):Dry-run records 只通过 toast 显示(`Found N to encode, M to skip`)。`Analyzing` / `AwaitingConfirm` 阶段 file list area 渲染为空,`Encoding` / `Done` / `Cancelled` 才显示 encode records | — |

**v1 安全保证**(从 v1.1 deferred 提前到 v1 实现,per §3.8.7 product-lens / scope-guardian review 反馈):

| 项 | 实现 |
|----|------|
| UI drain thread panic recovery | `use_hook` 里 `std::thread::spawn` 的 drain loop 用 `std::panic::catch_unwind` 包裹(per §3.9.4);panic → 设 `state.last_error = "drain thread panicked"` + 暴露 `controller_alive: false` 在 v2 才需要, v1 只设 last_error |
| Exit button 先 Cancel | Exit button onclick 分两阶段:active phase → 先 `cmd_tx.send(Command::Cancel)`,等 `Event::BatchCancelled`,再 `window().close()`;None/Done → 直接 `window().close()` |

这些限制不影响协议契约（Command/Event 已端到端测试通过 mock_tests），只是 UI 渲染层不完整。

#### 3.8.7.5 Unified log pipeline (UI side)

> **TL;DR** — lib 与两个 binary 都用 `tracing`(v1.2 follow-up #4 替代之前的 `log` + `tracing-log` 桥接)。CLI 写到 stderr + 文件;UI 写到 `crossbeam_channel::Sender<LogLine>` → 专用 drain thread → `state.log`,footer / `[LOG]` 页统一显示。`RUST_LOG` 一套规则同时控制两层。

**Why this architecture**: lib 现在与 binary 用同一套 logging API,不再需要 `tracing-log` 桥接。`rencoder_lib` 是 `publish = false`,没有外部消费者依赖 `log` 兼容性,所以可以安全地让 lib 直接 emit `tracing` 事件。`tracing` 的结构化字段(`output = %path`, `source_fps = f`, `batch_id = id`)把 `log::info!("...{}...", arg)` 这种把字段名丢进字符串拼接的做法替代掉 —— `tracing-subscriber` 的 `fmt::Layer().json()` 输出可以按字段聚合 / 检索。`#[instrument]` 自动给函数挂 span context,子事件自动继承。

**Flow**:

```
         lib (tracing::info! / tracing::warn!)
                  │
                  ▼ (direct, no bridge)
                  │
       ┌──────────┴──────────┐
       ▼                     ▼
   CLI subscriber        UI subscriber
       │                     │
 ┌─────┴─────┐                ▼
 stderr  file sink    MakeWriter (custom)
                         │
                         ▼ crossbeam_channel::Sender<LogLine>
                                  │
                                  ▼
                         dedicated log drain thread
                                  │
                                  ▼
                         state.with_mut(|s| {
                             s.log.push_back(LogLine { ... })
                         })
                                  │
                                  ▼
                     footer log (last 5) + [LOG] page (last 1000)
                     (also receives Event::LogLine from ffmpeg stderr
                      via the lib's main event channel)
```

**Two sources, one VecDeque**:

- `Event::LogLine { file_index, line }` = ffmpeg stderr per file
  (per-batch context, 1 line per stderr chunk)
- tracing events from lib = lib internal logging
  ("no NVENC encoder passed the capability probe",
  "processing task N/M" etc.)
  - 这些**没有 file context** —— `LogLine.file_index` 用 `usize::MAX` sentinel
  - 渲染时显示 `(global)` 而不是 `[N]`

`LogSource` enum 区分这两类,方便后续 v1.2 给 lib events 加视觉
区分(颜色 / icon),不影响 log panel 的合并显示。

**Threading model**:

- Lib 的 tracing 事件从 lib 内部线程发出(actor thread + rayon worker threads)
- `crossbeam_channel` 是 MPMC,任意线程可 send
- UI 端一个专用 drain thread 阻塞 `recv()`,push 到 state.log
- state.log 的 `VecDeque<LogLine>` 用 `with_mut` 写,触发 Dioxus 响应式 re-render

**v1 不做的**:

- Visual 区分 ffmpeg stderr vs lib internal(都用同一灰底色)
  —— v1.2 加 color / icon
- Filter controls(hide lib events / only show errors)—— v1.2
- Persist log to disk on UI side(CLI 的 file sink 不覆盖 UI;
  UI session 退出即丢;CLI 不受影响)—— v1.2 if needed

**Why not just write tracing events to stderr from the UI?** —
stderr 不适合 desktop app(GUI 程序没控制台)。Sink 必须接到 UI
state,才能在 footer / `[LOG]` 页显示。custom `MakeWriter` 把
`tracing_subscriber::fmt::Layer` 接到 `crossbeam_channel` 是最小
开销的方案。

#### 3.8.7.6 Cancel semantics + dry-run records (v1.2 follow-up #2)

> **TL;DR** — Cancel 是**乐观 UI**:用户 click 瞬间 `phase =
> Cancelled`,lib 在背景慢慢清。In-flight partial output 由 lib
> 在 ffmpeg kill 时删。Dry-run records 不进 file list,只通过
> toast 反馈。

**Cancel latency breakdown** (为什么需要乐观 UI):

| 步骤 | 位置 | 耗时 |
|---|---|---|
| 用户 click → `Msg::UserClickedCancel` → `phase = Cancelled` (乐观) + `try_send(Command::Cancel)` | `msg.rs:602` reducer | <1ms |
| cmd_tx channel → actor cmd_rx | `job.rs` actor 循环 | <1ms |
| `cancel_token.cancel()` 写入 `Arc<AtomicBool>` | actor（`Command::Cancel` 分支） | <1ms |
| 等下一行 ffmpeg progress 输出(cancel poll 在 stdout 循环里,每 ~0.5s/line) | `batch.rs` `run_ffmpeg_with_progress` | 0–500ms |
| `child.kill()` + `child.wait()`(SIGTERM 等 ffmpeg 退出) | 同上 | 200ms–1s |
| Processor closure 返回 `Err(UserCancel)`(cancel poll between files) | worker 的 processor | <1ms |
| `TaskManager::run` 返回 `Err((idx, _))` → worker 发 `Event::BatchCancelled` | worker | <1ms |
| UI evt_rx → reducer 看到 `Event::BatchCancelled`(但 phase guard 丢弃,不处理) | `msg.rs` | <1ms |

**总延迟 ≈ 1–2s**。如果 UI 等 `Event::BatchCancelled` 才改 phase,这段时间里 badge = `ENCODING`,progress 行 = `cancelled`(因为 `t.cancelled = true`)→ 矛盾 flicker(用户反馈 issue #4)。

**前提：第 2 步必须真的发生**——actor 在批处理期间要能取 `cmd_rx`。这一条曾经不成立：
actor 同步跑整批(`TaskManager::run` 是个 for 循环),`Command::Cancel` 排在队列里直到整批
跑完才被处理,于是"取消"实际上从未中止过任何编码。批改到 worker 线程上跑之后
(§3.10.1.1),这一步才是 <1ms。

**Optimistic cancel invariant**: reducer `Msg::UserClickedCancel` 在 Encoding phase 时立即设 `phase = Cancelled`,**不等 lib**。`apply_event` 入口加 phase guard:任何 batch-scoped `Event::*`(`BatchStarted` / `FileStarted` / `Progress` / `FileCompleted` / `BatchCompleted` / `BatchCancelled` / `LogLine` for batch indices / `Error`)在 `state.tasks.active.phase == Cancelled` 时直接 return。Non-batch events(`HistoryLoaded` / `ConfigSaved` / `AnalyzeStarted` etc.) 不受 guard 影响。

**In-flight partial output cleanup** (lib 侧):

`run_ffmpeg_with_progress` 在 cancel branch kill child 后,提取 argv 最后一项作为 output path,调 `std::fs::remove_file`。CLI 路径已经走 `PartialOutputHandle` Guard(actor 路径之前没接,这次补齐)。这保证 partial `.mkv` 不留在磁盘上——下一次跑同样文件时,`encode_prepare` 的 partial-output detector 也不会再误判。

**被取消的文件 = 整批取消**:ffmpeg 被 kill 后,那个文件的 record 是 `Failed` 且
`error.category = Cancelled`。actor 的 processor 看到这一条就**中止整批**(`Err`),
于是 worker 发的是 `Event::BatchCancelled`,而不是"跑完了,其中一个文件失败"。
用户按了取消,回给他的就该是"取消",不是"完成但有一个失败"。


```rust
// run_ffmpeg_with_progress cancel branch (v1.2 follow-up #2)
if cancel.is_cancelled() {
    let _ = child.kill();
    let _ = child.wait();
    drop(stderr_thread);
    // v1.2 follow-up #2: clean up the partial output file ffmpeg
    // was writing when cancel hit. argv layout is
    // `[ffmpeg, -y, -i, INPUT, ...encoders..., OUTPUT]`; output is
    // the LAST element. Tolerant of missing file (already removed,
    // e.g. by a parallel cleaner) — `remove_file` returns Err, we
    // log + ignore.
    if let Some(out) = argv.last() {
        if let Err(e) = std::fs::remove_file(out) {
            if e.kind() != std::io::ErrorKind::NotFound {
                log::warn!("    cancel cleanup: failed to remove {out}: {e}");
            }
        }
    }
    return Err((ErrorCategory::Cancelled, ...));
}
```

**Dry-run records → toast only**:

v1.2 follow-up #2: `Analyzing` / `AwaitingConfirm` 阶段不渲染 file list。`dry_run_records` 字段保留在 state(toast 文案需要从中数 `to_encode` / `to_failed`),但 UI 不画。`Encoding` / `Done` / `Cancelled` 阶段渲染 `records`(encode 出来的真实结果)。

`Event::DryRunFinished` reducer 路径:

```rust
Event::DryRunFinished { records } => {
    // 批里的每个文件都会转码：没转成的只剩"prepare 阶段就失败"的那些（§3.3.4）。
    let to_encode = records.iter().filter(|r| matches!(r.outcome, EncodingOutcome::Succeeded)).count();
    let to_failed = total_files.saturating_sub(to_encode);

    // 不管 to_encode 多少都 toast — 用户操作的反馈
    if to_encode > 0 {
        push_toast(state, format!("Found {to_encode} to encode ({to_failed} failed)"), ToastKind::Info);
    } else {
        push_toast(state, format!("0 encodable files ({to_failed} failed); pick a different dir"), ToastKind::Warning);
    }

    // records 仍存进 state(toast 已经消费了 counts,这是冗余存)
    // 但 UI 不渲染它
    state.tasks.active.as_mut().unwrap().dry_run_records = records;
    state.tasks.active.as_mut().unwrap().to_encode = to_encode;
    state.tasks.active.as_mut().unwrap().to_failed = to_failed;

    if to_encode == 0 {
        // 没文件可编码 — finalize + phase 回到 None,让用户重选 dir
        finalize(state, |t| { t.cancelled = false; });
        return;
    }
    state.tasks.active.as_mut().unwrap().phase = BatchPhase::AwaitingConfirm;
}
```

**为什么 records 还要存进 state**(不直接丢):

1. Toast 文案只展示 counts,但 future 可能有"显示哪些 file 被 skip"的展开(toast 上加 details 按钮 → modal)。那时直接读 `dry_run_records` 即可。
2. 调试时可以通过 state inspector 看 dry-run 决策。
3. Cancelled 之后重 click "Encode",会用 `dry_run_records` 决定 `Command::StartBatch` 的 `files` 参数——避免重新跑 dry-run。

**Tests** (v1.2 follow-up #2):

| Test | Covers |
|---|---|
| `user_clicked_cancel_in_encoding_sets_phase_to_cancelled_immediately` | Optimistic cancel — phase = Cancelled in <1ms |
| `cancelled_phase_filter_drops_late_batch_events` | After cancel, FileStarted / Progress / FileCompleted / BatchCompleted / BatchCancelled / Error arriving late are dropped by phase guard |
| `cancelled_phase_button_resets_to_encode_label` | AppState final state matches the button render decision |
| `dry_run_finished_pushes_summary_toast` | `Found N to encode (M to skip)` toast fires; records stored but UI doesn't render them |
| `lib_run_ffmpeg_cleans_up_partial_output_on_cancel` | `run_ffmpeg_with_progress` cancel branch removes argv.last() before returning Err |
| `lib_run_ffmpeg_cancel_missing_output_is_tolerated` | `remove_file` returns NotFound — handled silently (no panic) |

#### 3.8.8 与 §3.4 CLI 前端的关系

### 3.9 UI 运行时 / Dioxus 适配

> **TL;DR** — 禁用 `use_coroutine` 消费 `crossbeam` 事件(阻塞 `recv()` 会
> block Dioxus runtime,首帧不渲染);改用 `use_signal_sync` + 专用 OS 线程 +
> 直接 `with_mut()`。完整 bisect 历史见 `docs/debug-journal/event-pump-blocking.md`。

#### 3.9.1 架构总览

```text
[rayon/polars worker thread]    (lib 内)
        │ crossbeam_tx.send
        ▼
[rencoder-controller thread]    (lib 启,管 1;持有 evt_tx)
        │ 写入 crossbeam
        ▼
[crossbeam evt_rx]              (lib boundary,unbounded)
        │
        │ UI App() use_hook: std::thread::spawn 一个专用 OS 线程
        ▼
[rencoder-ui-drain thread]      (UI 启;阻塞 rx.recv() 循环)
        │ read cfg / read cmd_tx (SyncSignal,跨线程 OK)
        │ state.with_mut(|s| reduce(...))
        ▼
[SyncSignal state storage]      (Dioxus 0.6,parking_lot RwLock 后端)
        │ 触发 reactive re-render
        ▼
[App() next render]
```

线程命名: `rencoder-controller`(lib)+ `rencoder-ui-drain`(UI)。其余(rayon workers, ffmpeg stdio reader)未命名。

#### 3.9.2 关键决策

- **std::thread,不用 `use_coroutine`** — `crossbeam_channel::Receiver::recv()` 是阻塞调用,放进 `async move` 块会 block Dioxus runtime 的 task poller,首帧永远不渲染。
- **`use_signal_sync` 不用 `use_signal`** — 默认 `use_signal` 返回 `Signal<T, UnsyncStorage>`(`RefCell` 后端,`!Send`),`std::thread::spawn` 编译报错 `E0277`。`use_signal_sync` 返回 `Signal<T, SyncStorage>`(`parking_lot::RwLock` 后端),`Send + Sync`。
- **不桥接到 `futures::channel::mpsc`** — 多一个桥接线程、一个中间队列、`futures` 依赖。阻塞 recv 跑在专用线程上就够。
- **Lib API 不动** — `start_controller()` 还是返回 crossbeam `ControllerHandle + into_split()`。Lib 仍是 100% sync/runtime-agnostic(CLI 路径走 `transcode_batch` 同步)。

#### 3.9.3 Alternatives considered

| 策略 | 结论 |
|---|---|
| A. `use_signal` + `use_coroutine` + `async { recv() }` | 阻塞 recv() block executor — 同 §3.9.2 第 1 项。|
| B. bridge `crossbeam` → `futures::mpsc` via std::thread + `use_coroutine` | 功能 OK,架构冗余(多 1 线程 + 1 队列 + 1 依赖)。|
| C. Lib 改用 `async-channel::Sender<Event>` | 拒绝 — Lib 保持 100% sync 是项目约束;CLI 已用 `transcode_batch` 同步路径,actor 内部从 sync 漂到 async 友好会扩大差异面。|
| D. 默认 `use_signal` + `Sender<Event>` 直接给 actor | 拒绝 — `use_signal` 所有权从 `App()` 移到 actor 闭包意味着 state 不再是 Dioxus state;buttons 改 state 必须经 `cmd_tx` 绕一圈,增加间接层。|

#### 3.9.4 v1 限制

见 §3.8.7 "UI drain thread panic recovery"行。完整 bisect 与"为什么不桥接 futures"等决策历史在 `docs/debug-journal/event-pump-blocking.md`,本节不复述。

---





#### 核心库

```toml
[dependencies]
walkdir = "2"
serde_json = "1"
log = "0.4"
thiserror = "1"
num_cpus = "1"
rayon = { version = "1", optional = true }
polars = { version = "0.46", features = ["lazy", "csv"] }

[build-dependencies]
cc = { version = "1", optional = true }
pkg-config = { version = "0.3", optional = true }

[features]
default = []
parallel-analyze = ["rayon"]
c-probe = ["cc", "pkg-config"]
```

#### CLI 前端

```toml
[dependencies]
rencoder_lib = { path = "../rencoder_lib", features = ["parallel-analyze"] }
clap = { version = "4", features = ["derive"] }
dialoguer = "0.11"
parking_lot = "0.12"

# 信号处理按平台分层（§3.4.5）：Unix 走 signal-hook，Windows 走 windows-sys
[target.'cfg(unix)'.dependencies]
signal-hook = "0.3"

[target.'cfg(windows)'.dependencies]
windows-sys = { version = "0.61", features = ["Win32_Foundation", "Win32_System_Console"] }
tracing = "0.1"
tracing-subscriber = { version = "0.3", features = ["env-filter"] }
chrono = { version = "0.4", default-features = false, features = ["clock", "std"] }
anyhow = "1"
```

---

### 3.10 并发模型、参数流与公共功能归属

> **TL;DR** — 三条硬性要求：**(1) 选同步原语之前先写清"几个读者、几个写者、锁要持有多久"；
> (2) 一个配置字段只能有一个地方把它变成运行时值；(3) 两个前端都要用的东西放 lib。**
> 这三条是踩过坑之后补写的，不是事后总结。

#### 3.10.1 并发：先写需求，再选原语

**为什么要有这一节**：编码器缓存原来用 `RwLock`，理由是"让它可替换"。但读写锁的收益来自
"多读者并行读同一份数据"，而这里只有一个读者（actor 线程）——原语选错了，而**需求从没被写
下来**，于是没人能判断它对不对。副作用是读锁被持到整个文件编码结束（几分钟），刷新线程要等
当前文件跑完才能写进去。

**选原语之前必须先回答三个问题**：

| 问题 | 答不上来就别动手 |
|---|---|
| 几个读者？几个写者？ | 单一写者 → `Mutex`。读写之分只在"多读者并发读同一份不可变数据"时才有意义 |
| 锁要持有多久？期间会做 IO 吗？ | 会做 IO（spawn 进程、跑 ffmpeg、读写文件）→ 锁内只取 `Arc`，锁外用 |
| 状态会被替换吗，还是只写一次？ | 只写一次 → `OnceLock`；会被替换 → 可替换的 `Arc` 句柄 |

**本项目的五条规则**：

1. **单一写者**：每个共享状态只有一个写者 → 用 `Mutex`，不用 `RwLock`。
2. **锁内不做长 IO**：持锁期间不得 spawn 进程、不得跑 ffmpeg、不得等另一个锁。需要长时间
   持有的共享数据就放进 `Arc`，读者克隆指针后立刻放锁。
   **例外**：当锁的**目的就是**序列化一段短而有界的 IO 时（例：日志文件写入——不持锁就会
   行交错、轮转 bookkeeping 也会乱），IO 在锁内是可以的，但必须满足：单次写而不是整批、
   失败不重试、持锁期间不做别的事。
   已知的一处代价：CLI 的日志 sink 用 `Mutex<Inner>` 串行化写入，所以 **signal handler
   的日志会等在途的那一次写**（单次本地写，有界）。清理半成品的动作在日志之后，延迟也
   在这个量级。
   这条例外**不能推广到"日志走通道 + 写者线程"**：signal handler 打日志之后立刻
   `process::exit(1)`，走通道的话这行 interrupt 日志可能还排在队列里没落盘。CLI 的
   日志 sink 必须同步写。
3. **可替换的共享状态用 Arc 句柄**：`Arc<Mutex<Option<Arc<T>>>>` —— 替换整个句柄，读路径
   是指针克隆。
4. **跨线程只走通道**：命令 / 事件走 channel，不共享可变状态；只写一次的状态用 `OnceLock`。
5. **中枢不阻塞**：actor 只做"收命令 → 判闸门 → 起 worker → 回话"。跑一批的活一律放
   worker 线程（见 §3.10.1.1）。actor 阻塞 = 中枢失效。

**共享状态清单**（谁写、谁读、怎么保护）：

| 状态 | 写者 | 读者 | 保护 |
|---|---|---|---|
| `PartialOutputHandle`（当前输出路径） | CLI 每轮转码登记 / 清除 | CLI signal handler | `Arc<Mutex<Option<TaskContext>>>`（parking_lot）——**只有 CLI 路径用**；actor 路径的 cancel 分支自己删半成品，不需要它 |
| `CancelToken` | actor（`Command::Cancel`） | worker（在飞行的 ffmpeg 上轮询） | `Arc<AtomicBool>`——**唯一正当的跨线程 bit**（§3.10.1.1） |
| busy 闸门（单批次） | actor | actor | **无锁**——actor 本地 `bool`（批次在 worker 上跑之后，闸门只有 actor 读写） |
| 批次完成信号 | worker | actor | 通道 `bounded(1)`——回话走通道，不共享可变状态 |
| 降级回报通道 | worker（发现降级时） | worker 自己收，批次结束时并进 done 信号 | 通道（无界） |
| 编码器缓存（**可替换**） | 初始化线程 / nv 变化时的刷新线程 | worker（等初始化落地后取一张快照） | `Arc<Mutex<Option<CacheSlot>>>` |
| NVENC 能力对象（检查结论 + 本次运行内的修正） | worker（发现降级时）/ actor（汇总） | `select_encoder`（选编码器时） | **无锁**——持有者独占：actor 持权威副本，每批给 worker 一份快照，修正经 done 通道回报 |
| 丢弃事件计数 | 各发送方自己 | 只有它自己 | 每个发送方一个 `DropCounter`——不再跨发送方共享 |
| 日志文件写入 | tracing sink | 各线程的日志 | `Mutex<Inner>`（`std::sync`）——**序列化 IO 的例外**，见规则 2 |
| UI 日志接收端 | `tracing_init::init` | UI 的 use_hook | `OnceLock<Receiver<String>>`（只写一次） |

**线程清单**（**主要**线程——不是穷举；新增线程时记得补一行，也记得看它有没有引入新的共享
状态）：

| 线程 | 起于 | 职责 | 生命周期 |
|---|---|---|---|
| actor | `start_controller` | 处理 Command、发 Event、起 worker | 进程 |
| 批次 worker | actor（`StartBatch` / `DryRun` / `Analyze`） | 跑一批文件（`TaskManager::run` 的同步循环） | 一批，结束即退 |
| 缓存初始化 | `start_controller` | `ffmpeg -encoders` + NVENC 能力解析 | 一次，结束即退 |
| NVENC 探测 | `Command::Detect` / nv 变化 | 能力实测 + 写记录 | 一次，结束即退 |
| stderr 读取 | 每次 ffmpeg | 收集 stderr 尾部（有上限） | 随 ffmpeg 子进程 |
| signal 监听 | `install_handler` | 收 SIGINT/TERM/HUP → 清理半成品 | 进程 |
| UI 事件泵 | `App()` 的 use_hook | 阻塞 `recv()` → `state.with_mut` | 进程 |
| UI 日志泵 | `App()` 的 use_hook | lib 日志行 → UI 日志条 | 进程 |

**"锁内不做长 IO"** 在 stderr 读取线程上尤其要紧：它跟 ffmpeg 子进程同时活着，此时持有任何
共享锁都会把编码拖慢。

**共享状态清单是约束，不是快照**：它列的是"加了共享状态就得按这五条规则来"的那几处。
测试专用的锁（env 变量串行化、聚合表的收集）不列——它们不参与生产路径的并发结构。

#### 3.10.1.1 中枢：actor 不阻塞，批在 worker 上跑

**为什么要有这一节**：actor 是中枢——所有跨线程的请求都从它过。但如果 actor 自己跑批
（`TaskManager::run` 是一个同步 for 循环），它就在整批期间卡住：`cmd_rx` 全程只有
actor 一个读者，`Command::Cancel` 排在队列里没人取。UI 的 Cancel 按钮按下后，
UI 乐观地显示"已取消"，lib 却把整批继续编完，在飞的 ffmpeg 也不会被 kill，半成品留在
磁盘上。**这不是"取消慢"，是"取消没发生"**。

**要求**：

1. **actor 不跑批**。`StartBatch` / `DryRun` / `Analyze` 的实际工作放到 worker 线程上；
   actor 只做：收命令 → 判闸门 → 起 worker → 回话。
2. **一批一个 worker**（不是线程池）。一批 = 一次 `TaskManager::run` 的串行循环；
   worker 起于该批、止于该批。
3. **一批的事件由同一个发送者发**。`FileStarted` / `Progress` / `FileCompleted` /
   `BatchCompleted` / `BatchCancelled` 都由 worker 发——单发送者 ⇒ 通道 FIFO ⇒ UI
   看到的顺序不会错位。
4. **批结束由 worker 回报**：一条 `bounded(1)` 的 done 通道。worker 发完最后一个事件后
   发一个"这批完了"，actor 收到才放开 busy 闸门。闸门放开前 actor 先把 done 通道排空，
   免得"刚跑完的那批"被误判成"还在跑"。
5. **准备工作也在 worker 上做**：编码器缓存的一次性初始化要 spawn ffmpeg，可能比一批
   的启动更早发生。worker 拿缓存时**等初始化落地**（有上限），而不是报"初始化进行中"
   让这批失败——后者是真实 bug：启动后第一批根本起不来。等发生在 worker 上，actor 不受
   影响。

**中枢化之后，跨线程共享可变状态只剩两样**：

| 东西 | 方向 | 为什么正当 |
|---|---|---|
| `CancelToken`（1 bit） | actor 写 / worker 轮询 | 轮询型取消：worker 每 ~0.5s 查一次 flag。走通道往返太贵，一个 bit 不值得一个锁 |
| done 信号 | worker 写 / actor 读 | 通道——天然同步，不需要锁 |

其余的（busy 闸门、批 id、编码器缓存的取用）都是 actor 独占的普通变量；worker 只拿到
**不可变快照**（`Arc<EncoderCache>` 之类）。

**读法**：看到一个新的 `Mutex` / `Atomic` 时先问一句——这份状态有第二个拥有者吗？
没有的话，多出来的原语就是负担。

**（已落地）**：NVENC 能力判定表（`OnceLock<Mutex<HashMap>>`）已经折进 actor 独占的
状态——降级写入发生在 worker 里，worker 通过通道回报 actor，actor 独占这份判定。

**运行期的判定也要有持有者**：不只是"锁"，还有"这份状态归谁"。原来 NVENC 降级判定
（"这个编码器在这次运行里坏过"）放在进程级全局表里——语义上它的生命周期确实是"进程"
（CLI 一次命令 = 一个进程，GUI 一个会话 = 一个进程），但**归属是隐形的**：没人"拥有"
它，于是它既逃出了测试的边界，也没法说清谁在读、谁在写。

改成显式的持有者（**现状，已落地**）：

- GUI：actor 持权威副本；每批开工时给 worker 一份**快照**（worker 拿到的是值，不是引用）。
- CLI：一次命令持一个本地副本。
- worker 在批里发现降级 → 写自己的快照（同一批的后续文件立刻看到）→ 同时把降级
  **通过 done 通道回报** actor → actor 合并进权威副本 → 下一批的快照带上它。

于是全局表消失，判定变成一份**值**：它住在**能力对象**里（§3.10.5），随上下文传下去。
选型拿到的是整个能力对象（整体结论 + 逐编码器细节 + 运行时的修正），不是从里面拆出来
的一个零件。写只发生在一处（NVENC 失败的当场），读只有一处（选编码器），所以不需要锁。

（运行时修正的继承：actor 合并后下一批继承——这是现状。**目标语义**把它绑到能力代次
上：actor 只合并当前代次的回报，重检发布新代次后旧修正不再继承。两者对「重检后是否
重试一个刚失败的编码器」的答案不同，以目标语义为准，见本节末尾的「目标设计的共享状态
与线程补充」。）

**计数也要有归属**：丢弃事件的计数原来是一个所有发送方共享的 `Arc<AtomicU64>`——写的地方
多、读的地方**一个都没有**（只加不读）。改成每个发送方自己一份（`DropCounter`）：第一次丢的
时候打一条 warn（"UI 落后了"），之后每 100 次再打一条（带累计数）。省掉一个跨发送方共享的
原语，顺带让这个信息第一次真的被人看见。

（`DropCounter` 内部是一个 atomic：analyze 路径的进度闭包会被 rayon 的多个 worker 调用，
那一个发送方内部确实是多写者——这是唯一还需要 atomic 的地方。）

**已知限制**：UI 断开（`cmd_tx` 全部 drop）时 actor 直接退出，**不**取消在跑的批——
worker 自己跑完，事件发给没人接收的通道。这与拆分前的行为一致（拆分前 actor 堵在批里，
UI 关掉后批也照样跑完），且关窗口时进程本来就要退出，ffmpeg 子进程会被遗留成孤儿
（既有问题，不在本次范围内）。

**目标设计的共享状态与线程补充（§3.10.6）**。actor 单写探测请求序号及已发布能力代次。探测线程返回带请求序号的候选结果，actor 校验序号后统一发布结果和安排持久化，旧结果不得覆盖新快照或新磁盘缓存。执行 worker 在构造上下文时读取一次同代的能力对象，批内更新它的运行时修正，回传携带代次。actor 只合并当前代次的回报。重检失败保留当前快照并反馈对应操作。**已知限制**：重检发布新代次后，旧代次里实测到的编码器故障不再被继承——下一批可能重新尝试一个刚因硬件故障回退的编码器，结果仍是回退成功（不损坏产物，只多一次重编）。

#### 3.10.2 参数流：一个字段只有一个转换入口

**要求**：一个配置字段只能有**一个**地方把它变成运行时值。

之前有三条路径：

```text
AppConfig ─┬─ CLI 手写映射 ─────────────────────────→ EncodeParams
            ├─ EncodeParams::from_app_config ───────→ EncodeParams   （UI 用）
            └─ Config::from_encode_config → Config → max_fps_config() → EncodeParams.max_fps
```

第三条里的 `max_fps_eps` 是硬编码的 `0.05`，第二条用的是 `MaxFpsConfig::default().eps`
（`0.01`）——**同一个帧率容差在 CLI 和 UI 上不一样**，而且没人发现。

**现在的规则**：

- `EncodeParams::from_app_config()` 是**唯一**的"配置 → 每轮参数"映射入口。CLI 和 UI
  都用它 ✓，各自只补自己特有的部分（CLI 补 target 与 extra_args ✓）。
- lib 的运行时 `Config` **退役** ✓：`encoders` 与 `ffmpeg_args` 进了 `EncodeParams` ✓
  → grab-bag 类型整体消失 ✓，"这次运行用了什么配置"只有一个答案 ✓。
- 常量（容差带、默认 crf、默认码率）只有一处定义，别处引用。

**还差的**（§3.10.5 的 future 设计）："默认 ← 文件 ← 命令行"的**合并**还没收在一处 ✓
——CLI 读完 config 后逐字段 mutate `params.nv` / `target` / `extra_args` ✓，
优先级是巧合而不是规则 ✓。

```text
现在：

AppConfig ──EncodeConfig──→ EncodeParams::from_app_config ──→ EncodeParams
                                   （CLI 与 UI 共用这一条）
                                   CLI 随后再 mutate 三个字段

目标（future）：

默认 ← 配置文件 ← 命令行
        └─ 合并（唯一一处）──→ 有效配置
```



#### 3.10.3 公共功能归属：两个前端都要用的放 lib

**要求**：同一份事实的**措辞 / 格式 / 映射**放 lib；**交互方式**（怎么问、怎么显示）各前端
自己实现。

| 内容 | 归属 | 例子 |
|---|---|---|
| 同一份事实的措辞 | lib | NVENC 能力清单的每一行（`nvenc::Capability::lines()`） |
| 同一份映射 | lib | 配置 → 每轮参数（`EncodeParams::from_app_config`） |
| 同一份判定 | lib | 能力是否过期（`review_due`） |
| 能力解析 + 存盘 | lib | `nvenc::resolve_and_store*`：唯一会写能力记录的地方 |
| 解析结果 → 能力摘要 | lib | `Resolution::capability()`：唯一把解析变成"前端显示什么"的地方 |
| 记录放在哪、要不要强制重探 | 前端 | CLI 的 `Paths::capability_record`；GUI 的 `ActorEnv.paths` |
| 记录怎么落盘 | lib | `data::store::write_records`（格式分派）与 `default_encoding_path`（默认路径） |
| 摘要的计数与措辞 | lib | `data::store::summary_lines`；前端各自排版 |
| 什么时候落盘 | 前端 | CLI 在编码命令结束时；actor 在批次终态时 |
| 怎么问用户 | 前端 | CLI 在终端问 y/N；GUI 给一个"重新检测"按钮 |
| 怎么显示 | 前端 | CLI 打印报告；GUI 渲染成行 |

反面教材：能力清单的每一行，CLI 拼成 `av1_nvenc   : UNUSABLE — 原因`，GUI 拼成
`av1_nvenc  unusable — 原因`——同一件事两种措辞，隔几轮就漂移。

#### 3.10.4 上下文用对象传：别让参数包长成影子实现

**症状（改造前）**：函数签名越来越长——最深的一个 12 个参数，六个函数带
`#[allow(clippy::too_many_arguments)]`（那是信号，不是许可）。同时 `job.rs` 里
**整个 actor 被实现了两遍**：生产版和测试版各一份，后者把闸门、worker 起停、
事件序列、cancel 接线全部重写了一遍。**参数包是重复实现的成因，不是它的症状。**

**病根**：不是"参数多"，是**没有上下文对象**。同一批东西（params / cache /
config / verdicts）被拆成散参数，在每一层重新组装一遍；每加一个需求（比如
NVENC 能力），就要给八个函数各加一个参数。因为上下文是隐式的，任何想复用
这段逻辑的人——包括测试替身——只能把参数重新拼一遍，拼出来的还是**一份副本**。

**分层**（每层只有一种该共享的东西）：

| 层 | 共享什么 | 不共享什么 |
|---|---|---|
| 机制 | worker 起停、busy 闸门、done 信号、cancel bit（§3.10.1.1） | —— |
| 上下文 | 一批要带的东西 | 每个文件的现场 |
| 协议 | —— | 每个作业发哪些事件 |

**两个上下文对象**（另外两个被否，见下）：

- **`BatchContext`（批级）**：这一批要带的东西——参数、解析好的编码器缓存、
  运行时配置、本次运行的 NVENC 判定。一批一份，**在 worker 里构造**
  （缓存就绪之后，见 §3.10.1.1 规则 5），随 worker 一起跑完这一批。
  编码链往下传的是**这个对象**，不是它的零件。
  - 只装**两端都要用**的东西：CLI 特有的 `PartialOutputHandle` +
    `ProcessOutput`、GUI 特有的 `CancelToken` + 进度 sink，都由调用方在缝的
    那一层单独传，不进上下文。
  - 只有编码与预演两条路径用它；分析作业不用（它不碰编码链）。
  - 叶子函数（`select_encoder`）保持平铺参数——上下文在 `build_argv` 边界
    解包一次。否则 `encoder` 模块会反过来依赖 `pipeline`。
- **`FilePlan`（文件级）**：一个文件的现场——探测结果、算好的 argv、解析后的
  输出路径、两个时间基准。它**取代** `EncodePrepareResult::Ready`（现在那三个
  字段就是它的前身），不是并存的新类型。计时是 prepare 的**输入**（调用方
  建的），prepare 把它带进 plan 里。

**否掉的两个**：

- ~~`ActorState`~~ —— 它不是任何函数的参数，**一个参数也省不掉**；而 busy 闸门在
  生产 actor 里本来就是集中的一处（§3.10.1.1 规则 4），再包一层只是把一个 `bool`
  变成两个方法。actor 的运行期状态留在 actor 的局部变量里。
- ~~`FileClock`~~ —— 两个时间基准从来不是被当作一个整体用的，抽成类型只是换了个
  名字；它们直接放进 `FilePlan`。

**只新开一条缝**：`FileEncoder`——"编一个文件"这一步（**包含 prepare**）。
生产实现走真 ffmpeg，测试实现走 mock；**事件由共享代码发**，替身只提供这一步，
于是**协议只有一份实现**。

- 它是一个**单方法 trait**（`fn encode(&self, …)`），不是 `FnMut` 套在锁里：
  后者会把锁持到整个编码结束（几秒到几分钟），违反 §3.10.1 规则 2。
  取 `&self` 而不是 `&mut self`：actor 一批一个 worker，但**一个 actor 会跑很多批**，
  所以编码器必须能共享（`Arc<dyn FileEncoder>`）；测试替身把自己的状态放在
  `Arc<AtomicUsize>` 之类里面即可。
- 它坐在 **prepare 之上**（替身整个替换"编一个文件"这一步），否则 mock 会先
  去 probe 不存在的素材文件，永远走不到缝。
- 缝只服务 GUI 路径；CLI 路径直接调 `transcode_single`（共享 `BatchContext`，
  不共享缝）——两条路径的 ffmpeg 调用方式本来就不同（同步 vs 可取消）。
- NVENC 探测**不另开缝**：lib 已经有 `nvenc::resolve_with(path, force, runner)`
  这个 runner 缝（§3.8.6 的"新增"表里已列），注入路径与 runner 就够
  （配套加了 `resolve_and_store_with`：探测 + 落盘要一起换掉，否则测试会把
  记录写进开发者的真实 app 目录）。
- **换缝就要换路径**：路径也是缝的一部分。`path` 与 `runner` 一起注入——
  只换 runner 而让 lib 自己去解析路径，测试就会把记录写进开发者的真实 app
  目录（`Paths::capability_record` = `<current_exe 的父目录>/…`，而测试二进制的
  父目录是 `target/debug/deps/`）。lib 不再自己解析记录路径：路径由调用方给
  （CLI 给 `Paths::capability_record`，GUI 给 `ActorEnv.paths`），lib
  里的能力解析入口一律是 `..._at(path, ...)`。前端自己的测试（CLI 集成测试
  spawn 的是真二进制）用 `$RENENCODER_*` 环境变量把子进程指向临时目录。
- 编码器缓存**不需要缝**：`run_actor` 本来就收一条 `cache_rx`，而 `CacheInit`
  的字段全是 `pub`——测试往通道里发一条预置的缓存即可。

**规则**：

1. **上下文用对象传，不用参数包**：同一批东西在层与层之间传的是对象，不是零件。
   什么时候该抽成对象？三个条件都满足：(a) 同一组东西出现在 ≥3 个签名里；
   (b) 它只有一个拥有者（一批 / 一个文件）；(c) 它不在多处被改。
   ——"参数超过 N 个就抽对象"是坏判据：它分不清"四个总是一起走的参数"和
   "四个恰好挨着的参数"，而且会不停地生类型。
2. **测试替身走同一条路**：mock 不复制 actor——它造一个 mock 编码器，驱动
   **同一个** actor。复制 actor = 协议有两份实现 = 迟早漂移。
   （step 1 的 cancel 修复就是这样被遮住的：生产 actor 堵着不处理 Cancel，
   而 mock 那份 actor 是另一套代码——测试全绿，线上不生效。见 §3.10.1.1。）
   替身不是第二份实现，是同一条路上可替换的一段——所以它的接口只有一个实现
   也是合法的。
3. **注入的依赖集中写明**：`ActorEnv` 里放"从哪来"的东西（配置、历史数据
   目录、NVENC 记录路径、编码器、探测 runner），不藏在函数体深处
   （`Paths` 这种 env 读取不该藏在 actor 里——它让测试没法并行）。
   `ActorEnv` 构造后不变；运行期会变的东西（闸门、批 id、判定、cancel token）
   留在 actor 的局部变量里。
   生产环境由 `start_controller()` 造；测试环境由 `start_controller_with(env)` 造
   ——**同一个 actor，不同依赖**。

**本节取代 §3.8.6 "不修改" 清单中的两条**：`transcode_batch` 签名与
`process_one_file` 签名（§3.8.6 那两行标注"见 §3.10.4，已作废"）。
其余"不修改"条目（`EncodingRecord` / `EncodeError` / `TaskManager` /
`PartialOutputHandle` / AppConfig 加载 / CLI 的行为面）不受影响。

**闸门拒绝走 `Event::CommandRejected`，不走 `Event::Error`**：`Event::Error` 是
fatal-only（§3.8.3），UI 收到它会把正在跑的那批 finalize 成 Done——一次良性的
"重复 StartBatch 被拒"因此会被当成"这批失败了"。拒绝是**良性**的：命令被丢掉，
正在跑的那批照跑不误，UI 弹一条 toast 告诉用户"已经有任务在跑了"就够了。

**迁移顺序**：

| 段 | 改什么 | 怎么验证 | 状态 |
|---|---|---|---|
| A | 编码链引入 `BatchContext` 与 `FilePlan`（后者取代 `EncodePrepareResult::Ready`），参数包收成对象 | 全量测试 + 真 ffmpeg 集成测试（CLI 与 GUI 共用的热路径） | ✅ 已落地 |
| B' | actor 的依赖可注入（`ActorEnv` + 编码器缝 + 探测 runner + 缓存走已有的 `cache_rx`）**并**删掉 mock actor，全部调用改跑生产 actor | 原有协议测试保留并改跑在生产 actor 上；**四条断言含义变了**（mock 忽略命令里的 params、Analyze 的闸门、预演用默认配置、Detect 返回假能力）——前三条改对，最后一条由注入的假 runner 兜住 | ✅ 已落地 |

B' 必须一次落地：缝先于第二个实现出现，就是"只有一个实现的接口"——
本项目自己反对这种形状。

#### 3.10.5 分层配置与一次解析（目标设计）

每个字段按以下优先级取第一个显式设置的值：

1. 本次入口显式指定的值，例如 CLI 支持的覆盖选项或 GUI 本次提交的编辑。
2. 本次选定的配置文件中显式填写的值。
3. 系统内建默认值。

配置文件的选择与字段优先级分别处理。CLI 每次运行重新读取选定的配置文件。GUI 以最近加载或保存成功的文件配置为基线，未保存的显式编辑作为本次入口覆盖。保存成功才更新文件层基线，失败保留基线和草稿。保存回执只清除该次已保存的编辑，后续编辑继续保留。「哪些字段被显式编辑过」由前端的编辑跟踪判定（逐字段修订号，保存回执携带保存时的修订号），不靠草稿字段是否有值来推断——草稿里的值表达的是文件层，与入口层的「用户动过」是两回事。入口只覆盖自己支持且明确指定的字段。配置文件缺省和入口缺省均向下一层取值，显式的 0、false 和按字段约定有效的空列表照常生效。已有的按字段环境变量覆盖（例如采集上限）归入口层。同一字段同时有显式入口参数和环境变量时，显式参数优先。

schema 层仅对需要区分缺省的字段保留存在性：只有「缺省」与「显式写出」会得出不同结果的字段才需要——集合类（`extensions` 的空列表与默认列表含义不同，`ffmpeg_args` 的 `pre_input` / `post_input` 显式空列表与缺省含义不同）与多义标量（如 `crf = 0` 表示码率模式）必须保留；其余标量（如 `crf` 缺省得 45 与显式写 45 解析结果相同）保留存在性只为区分来源，可有可无。解析后运行时仍使用具体值。新增模板将可继承的示例值写成注释，已有文件中的显式值照常生效。crf 缺省和显式 45 的来源不同，显式 0 仍表示码率模式。

例如系统默认 max_fps 为 0，文件显式设置为 30，入口只指定输入时本批使用 30。若入口支持该字段并显式指定为 60，本批使用 60。

在文件采集前通过同一个入口完成解析，生成有效配置快照。采集、预演及执行使用同一结果。复用预演前比较任务清单、有效规划参数和相关能力代次。预演依赖媒体探测时还需检查输入是否变化，无法确认时重新规划。复用 EncodeParams、BatchContext 和 FilePlan。逐文件只按媒体信息和运行能力派生执行参数，编码器回退时重建相关参数组。

字段按消费位置作必要区分，只有共享校验或消除实际重复有收益时才提取结构。集合按字段约定替换、追加或按键合并。FFmpeg argv 按槽位构造，配置文件和入口的透传使用同一冲突规则。帧率、编码器和质量等受管选项的冲突应报错，记录以最终执行计划为依据。

验收覆盖三层优先级、显式 0 / false、等效 CLI 和 GUI 参数产生相同计划、同一批次快照稳定、透传冲突及回退参数一致性。


##### NVENC 与配置解析的接口

nv 和编码器偏好按上述三层解析。有效配置显式禁用 NVENC 时，任何 NVENC 候选均不可选。若有效配置同时显式指定 NVENC 编码器，批次接纳前报告配置冲突。能力检测接收设备和 FFmpeg 环境，产出一个**能力对象**。接纳时冻结有效参数，worker 在执行前取得能力对象，再构造 BatchContext，FilePlan 结合两者记录实际选中的编码器。重检更新后续批次，硬件回退只重建本文件的编码器相关参数。

##### 能力是一个对象：一次检查，整条链共享

能力这条链上本来有三处检查、两份同形的容器：

- **三处检查**：解析时（这台机器能跑什么）、运行时回退时（刚才那次失败是不是能力没了）、以及一个只会自己探一次的独立入口（已淘汰）。
- **两份容器**：`EncoderCache` 里的 NVENC 列表（检查的结论）和运行时的判定表（这次运行里实测坏掉的编码器）。同形、同源，却各管各的。

现在**只有一个类型**：

```
Capability {
    support:    Vec<NvencSupport>   // 检查的结论：逐编码器能不能用 + 为什么
    probed_at:  Option<DateTime>     // 什么时候探的
    review_due: bool                 // 结论过期了（30 天复审）
    verdicts:   CapabilityVerdicts   // 运行时的修正：这次运行里实测坏掉的
}
```

它是"这台机器的能力"这**一个概念的一个类型**——检查的结论和运行的修正在里面各占一部分，而不是两张同形的表。展示用的措辞（能力清单的每一行）也是它的一个方法，CLI 和 GUI 共用同一份。

**归属**：

| 谁 | 拿什么 | 干什么 |
|---|---|---|
| 解析（`nvenc::resolve_and_store*`） | —— | 产出它（整条链唯一的一次检查） |
| `BatchContext` | 一份 | 选型读它；回退写它 |
| actor | 权威副本 | 每批给 worker 一份快照，worker 的修改回报回来合并 |
| CLI / GUI | 一份 | 显示（`lines()`、`review_advisory()`） |

**读它的三条规则**：

1. **整体优先**：选型先看整体结论（`any_usable()`：这台机器做不了硬件编码就直接走软件，不必逐个编码器地问），再看逐编码器细节。
2. **回退的核实分三种情况**：
   - 运行时的判定里已经有 → 用它，不再探（同一个编码器第二次失败不重复探）；
   - **检查的结论说它不可用** → 采纳，不再探（同一个问题，启动时已经问过，答案是否定）；
   - 检查说它能用、编码却失败了 → 设备可能刚刚掉线（驱动重置、显卡被拔）→ **现场再探一次**。
   第三种必须保留：失败不等于能力缺失（磁盘满、参数非法同样会让编码失败），而降级对那些问题毫无帮助。
3. **回退写回同一份**：核实的结果更新的是这份能力，不是另一张并行的表。

**淘汰**：`EncoderCache` 只剩 `ffmpeg -encoders` 的输出（它本来就是"ffmpeg 有哪些编码器"）；那个自己探一次的独立入口删除——它只会让能力检查多一个来源。

**能力判定分两级**。**第一级**：这个 ffmpeg 构建本身带不带 NVENC 编码器（`ffmpeg -encoders`）。**第二级**：这台机器能不能真的跑起来（1 帧试编码）。两级是**一体的一次判定**，不是两个可选模式：一次判定里，第一级先回答"有没有得试"，第二级回答"试不试得过"。

**记录是这次检查结论的落盘形态**：能力对象在内存里，能力记录在磁盘上（跨进程复用）。
两者是同一份结论的两种存放位置——记录存的是测试结果：每个 NVENC 候选一条"能不能用"的测试结论，结果是可用或不可用。第一级失败时，这条候选的测试结论同样是**不可用**，**一级失败是它的原因**，而不是把"一级测试失败"另存成一条结论。记录的形状因此恒定——每个候选都有一项。

**记录不每次启动都重测**：能力记录按 30 天复审周期更新，运行之间复用；构建指纹不匹配或用户要求重测时才重新探测。复审周期由 lib 掌管（§2）：记录到期时 lib 推后日期并落盘，前端只负责显示提示。

两级分开带来一个对缓存键的要求：**只有第二级需要绑定设备**。第一级的结论只随 ffmpeg 构建变化（匹配键里已有构建信息）；第二级的结论随设备变化，设备信息取不到时，旧的否定结论必须重新验证或允许受控重试。

#### 3.10.6 NVENC 能力检测夹具与统一实施步骤（目标设计）

能力检测复用生产 runner 接口。测试夹具提供可编排的 FFmpeg 返回值、临时缓存目录和可控时钟。通过现有入口驱动缓存解析、重检、选型及回退。涉及超时、进程回收和静默子进程的验证使用可控子进程。真实 GPU 只用于验证实际驱动及编码器组合。

1. **先建立夹具和基线。** 复用现有 runner 测试，记录缓存首次建立、同一能力缓存匹配键复用、探针基础可用、选择软件编码器等行为；一级判定否定（构建不带 NVENC）时直接给结论、不做硬件检测，也要作为基线记下来。为后续步骤增加针对具体缺陷的回归场景。
2. **收口配置解析。** 在采集前解析三层有效值，沿用现有结构。验收入口覆盖、缺省继承、显式 false、0、CLI/GUI 等效输入，以及入口禁用 NVENC 与文件指定 NVENC 的冲突。预演和执行使用同一解析规则。
3. **把能力合成一个对象。** 检查的结论与运行时的修正合成一个 `Capability`（§3.10.5）：`EncoderCache` 只剩 ffmpeg 的编码器列表；选型读整体结论优先；回退的三种情况（已知 / 检查否定 / 现场再探）写进同一份；自己探一次的独立入口删除。验收：选型与回退读的是同一份能力；检查否定时回退不再探；"检查说能用却失败"仍会现场再探。
4. **修正能力刷新。** 探测设置有限超时并区分可用、确认不可用和暂无法判定。超时和短暂资源故障可重试。磁盘缓存使用由探测到的 FFmpeg 构建与设备信息计算的能力缓存匹配键，该键只用于能力缓存；设备或驱动信息只在第一级通过（构建带 NVENC）时才需要参与匹配，第一级不通过时结论只随构建变化。无法识别设备变化时，过期的否定结果必须重新验证或允许受控重试。仅实际探测更新探测时间。记录的形状恒定：每个 NVENC 候选都有一项，构建缺失记为不可用并写明原因；磁盘格式随形状变化升版本，旧记录作废重探。
5. **把探测代次接入批次。** 成功重检发布新代次，并使旧代的运行时修正失效。重复重检按请求序号防止较早结果覆盖较新结果。检测失败保留已有快照并单独反馈，避免结束活动编码批次。验收强制重检成功后下一批能恢复 NVENC、旧批迟到的禁用结论无法污染新代。
6. **核对回退和记录。** 取消直接结束本次执行。文件参数错误和普通 I/O 错误不改变硬件能力。确认为硬件初始化或可用性故障时才更新能力对象的运行时修正。软件回退按实际编码器重新生成并校验参数。验收取消无复验、超时可重试、同代的禁用与恢复、日志与实际 argv 一致。真实 GPU 探测和短片编码单列为发布验收，验证实际驱动和编码器组合。

---

#### 3.10.7 运行能力检查：开工之前先知道能不能干

一次运行开工之前，lib 检查这台机器上的**运行能力**：越基础的越靠前 ✅ ✅。

| 优先级 | 检查 | 内容 | 不成立时 |
|---|---|---|---|
| 1 | **文件系统** ✅ ✅ | 应用目录**可写** ✅ ✅（配置 ✅、日志 ✅、工作记录 ✅ 都在那儿 ✅ ✅）+ 操作目录**可写** ✅ ✅（取消请求 ✅、检查报告 ✅ 都在那儿 ✅ ✅）✅ ✅ | 报出来 ✅ ✅ —— 写不了产物，这一趟也留不下东西 ✅ ✅ ✅ |
| 2 | ffmpeg ✅ ✅ | 可执行 ✅、编码器列表拿得到 ✅ ✅ | 报出来 ✅ ✅ —— 没有 ffmpeg 编不了 ✅ ✅ ✅ |
| 3 | NVENC ✅ ✅ | 两级判定（§3.10.5 ✅ ✅）✅ ✅ | **不算失败** ✅ ✅ —— 只是"这次用软件编码 ✅ ✅"✅ ✅ ✅ |

**顺序不是随便排的** ✅ ✅：文件系统在第一位 ✅ ✅，因为它最基础 ✅ ✅ —— 后面每一项
都要往那儿写东西 ✅ ✅ ✅（工作记录 ✅、取消请求 ✅ ✅ ✅）；ffmpeg 在第二位 ✅ ✅，因为没
有它这一趟根本编不了 ✅ ✅ ✅；NVENC 垫底 ✅ ✅ —— 它最贵 ✅ ✅（探针 ✅ ✅），而且
**不成立也不失败** ✅ ✅ ✅（只是退回软件编码 ✅ ✅ ✅）。

**这是一个检查集 ✅ ✅，不是一次性的检查** ✅ ✅：lib 暴露**一个聚合入口** ✅ ✅，
按优先级依次跑完 ✅ ✅ ✅，产出一份**结果报告** ✅ ✅ —— 哪几项过了 ✅ ✅、哪几项没过
✅ ✅、为什么 ✅ ✅。

**结果写进文件 ✅ ✅**（和取消请求同一个方向 ✅ ✅）：前端**读文件** ✅ ✅，不各自再算
一遍 ✅ ✅ —— 多个前端 ✅ ✅、多个进程 ✅ ✅（将来的 Tauri UI ✅ ✅）看到的是同一份
✅ ✅ ✅。报告落在**操作目录** ✅ ✅（§3.6 ✅ ✅）—— 它描述的就是这个目录能不能干 ✅ ✅。

**一次检查，整体共享 ✅ ✅**：检查的结果是**一个整体** ✅ ✅，不是"某一项"的答案 ✅ ✅ ——
机器的事实（ffmpeg 能不能跑 ✅ ✅、NVENC 能不能用 ✅ ✅）**只查一次** ✅ ✅：解析查出来
✅ ✅，交给检查 ✅ ✅，检查再交给编码器缓存 ✅ ✅、UI ✅ ✅、报告 ✅ ✅；后面的环节**不再
自己查一遍** ✅ ✅ —— 查第二遍既慢 ✅ ✅，又可能跟第一遍**结论不一致** ✅ ✅（报告说"没
有能力记录" ✅ ✅，这一趟却在用硬件编码 ✅ ✅）。

**会变的那部分每次现查 ✅ ✅**：文件系统跟着操作目录走 ✅ ✅（换一次目录就换一次 ✅ ✅），
所以每次检查都重查它 ✅ ✅；机器没变 ✅ ✅，机器的事实就**沿用** ✅ ✅ —— 重新探测一遍
既慢 ✅ ✅，又没有新信息 ✅ ✅。

**检查没过不拒绝启动 ✅ ✅**：那是**信息** ✅ ✅，不是闸门 ✅ ✅ ✅ —— 桌面应用因为
目录不可写就打不开 ✅ ✅，那是敌意 ✅ ✅ ✅。用户看到"这里写不了 ✅ ✅"✅ ✅，自己
决定 ✅ ✅ ✅。

**GUI 的工作目录会换 ✅ ✅** —— 每换一次目录，检查的输入就变了 ✅ ✅ ✅（目标目录 ✅ ✅、
路径别名 ✅ ✅、能不能写 ✅ ✅），所以**每次切换都重新检查 ✅ ✅** ✅ ✅，报告也落到新的操作目录里 ✅ ✅。
重查**只重查会变的部分** ✅ ✅：机器没变 ✅ ✅（还是那台机器 ✅ ✅、那个 ffmpeg ✅ ✅），
机器的事实**沿用** ✅ ✅ ✅。

**取消请求的可写性属于文件系统检查 ✅ ✅，不是它自己的一件事** ✅ ✅：取消文件就在
操作目录里 ✅ ✅，那里写不了 ✅ ✅ → 产物也写不进去 ✅ ✅ → 这一趟本来就完了 ✅ ✅。
所以它是**运行的前置条件** ✅ ✅，在开工前查一次 ✅ ✅ —— 不是等到信号来了才发现写
不进去 ✅ ✅ ✅。

**请求通道的生命周期 ✅ ✅**：批启动时删掉遗留的取消请求 ✅ ✅ —— 否则上一次的中断
会取消这一趟 ✅ ✅。取消请求在**操作目录**里 ✅ ✅（§3.6 ✅ ✅），所以清理也在那里做
✅ ✅：CLI 开工前删一次 ✅ ✅，GUI 每批开工前删一次 ✅ ✅（工作目录会换 ✅ ✅，换过之后
旧目录里那份就不再相关 ✅ ✅）。

### 3.11 任务模型与执行契约

本节定义目标设计，实施以各节验收条件为准。此前章节的事件示例与本节冲突时，以本节为准。

#### 3.11.1 原语：任务、批次和计划

- 编码使用 `EncodeTask { input: InputPath, output: PathBuf }`，分析使用 `AnalyzeTask { input: InputPath }`。编码和预演共用一个任务构造入口及输出命名规则，分析保留独立的并行执行路径。
- 批次接纳时冻结有效配置。worker 在开始执行前取得能力对象，再用两者构造 `BatchContext`。整批固定能力代次，批内能力对象的运行时修正可以更新。
- `FilePlan` 承载探测结果、编码决策和最终路径。预演只读取输入并生成计划，使用 `Ready / Failed` 表示准备状态。真正执行后才产生成功的编码记录。
- 在启动写入前**规划**整批最终路径 ✅ ✅：两个输入可能指向同一个输出 ✅ ✅（同一目录里的 `movie.mp4` ✅ 与 `movie.avi` ✅ 都想要 `movie.mkv` ✅ ✅）—— **改名**而不是让整批终止 ✅ ✅：撞车的那一组各自叫 `<词干>_<输入扩展名>_<序号>.<目标扩展名>` ✅ ✅（`movie_mp4_1.mkv` ✅ ✅、`movie_avi_2.mkv` ✅ ✅）。序号按**排序后的路径**分配 ✅ ✅，所以名字不随采集顺序变 ✅ ✅（否则两次运行给出不同的名字 ✅ ✅，旧文件就留在那儿了 ✅ ✅）。规划要考虑路径别名和目标文件系统的路径语义 ✅ ✅（同一个目录 ✅ ✅、大小写不敏感 ✅ ✅）；输出不得指向任何批内输入 ✅ ✅。

#### 3.11.1.1 单实例守卫：同一目标同一时刻只有一个实例

**占用的粒度是目标，不是动作。** 目标一旦被指定给本进程，本进程就持有该目标的占用，直到目标变更或进程退出；编码、预演、分析这些动作都在已持有的占用下执行。

- **CLI**：启动解析出目标后立即占用，早于配置读取、文件采集与编码。
- **GUI**：**只在会往目标目录写产物的批次期间占用** —— 编码批次开工时占用，批次结束（完成 / 取消 / 失败）时释放；空闲不占。预演与分析是只读的，不写产物，不占用。

冲突要在用户还没开始干活之前暴露，而不是等编码到一半才发现。

所有**以目标为输入**的动作在开始前都要做一次**占用探测**：这个目标是不是已经有人在处理。有就停下来——**整趟拒绝**并报告（提醒当前已有别的实例在操作），不清理、不开始执行。

这条约束的不只是清理：两个实例同时处理同一目录会互相破坏产物（抢同一个输出、删掉对方刚写下的文件），所以**探测是入口条件**，不是清理环节的附加步骤。不涉及目标目录的动作（如 `detect` 探测硬件能力）不受此约束。

**范围是一棵目录树，不是一个目录** ✅ ✅：采集是**递归**的 ✅ ✅（§3.11.2 ✅ ✅）—— 父目录
这一趟会把子目录里的**输入**收进来 ✅ ✅，还会把子目录那一趟刚产出的**输出**也收进来 ✅ ✅
（输出也是视频 ✅ ✅，扩展名匹配 ✅ ✅）。两边同时跑 ✅ ✅ → 同一批文件被两个 ffmpeg 写 ✅ ✅
→ 产物坏掉 ✅ ✅。所以三段都要查 ✅ ✅：

| 方向 ✅ | 查什么 ✅ | 代价 ✅ | 怎么查 ✅ |
|---|---|---|---|
| **自己** ✅ ✅ | 目标目录 ✅ ✅ | — ✅ ✅ | 独占锁 ✅ ✅ |
| **祖先** ✅ ✅ | 目标往上的每一层 ✅ ✅ | 便宜（几层）✅ ✅ | 对祖先的标记**非阻塞试锁** ✅ ✅：文件不存在 = 没人占 ✅ ✅；锁不到 = 有祖先在跑 ✅ ✅ → 拒绝 ✅ ✅ |
| **后代** ✅ ✅ | 目标子树里的每个目录 ✅ ✅ | 要扫树 ✅ ✅ | 搭**采集的遍历** ✅ ✅（本来就要扫 ✅ ✅）→ 撞见活跃持有者 ✅ ✅ → **整趟拒绝** ✅ ✅ |

两个方向的竞态都闭合 ✅ ✅：晚来的那个撞见早就在的那一个 ✅ ✅ —— 子查祖先是廉价的 ✅ ✅、
每次都查 ✅ ✅；父查后代搭采集的遍历 ✅ ✅。祖先与后代的判断**用规范化路径** ✅ ✅
（canonicalize ✅ ✅），否则软链 ✅ ✅、挂载别名 ✅ ✅ 会让两条路径各走各的 ✅ ✅，漏判 ✅ ✅。

**互斥交给内核的文件锁** ✅ ✅：锁在目标目录里的标记文件 ✅ ✅（`.rencoder-owner` ✅ ✅）
上 ✅ ✅，独占且非阻塞 ✅ ✅。为什么是内核锁而不是"标记文件 + 判 pid 存活" ✅ ✅：

- **进程死了内核放锁** ✅ ✅（含 SIGKILL ✅ ✅）→ 不需要判活 ✅ ✅，也不需要处理 pid 复用 ✅ ✅；
- **取得占用是原子的** ✅ ✅ → 不会两个实例同时认为自己占着 ✅ ✅；
- **不需要"同时启动的仲裁"** ✅ ✅（谁拿到锁谁继续 ✅ ✅，另一个直接被挡 ✅ ✅，不会出现双方都
  停下的僵局 ✅ ✅）；
- **不需要平台差异** ✅ ✅：进程存活查询 ✅ ✅、进程启动时刻查询 ✅ ✅ 这两块平台代码整个不
  出现 ✅ ✅（Unix flock ✅ ✅ / Windows LockFileEx ✅ ✅ 由标准库承担 ✅ ✅）。

标记文件里记什么 ✅ ✅：**只有 pid 与开始时刻** ✅ ✅ —— 它在哪个目录 ✅ ✅，范围就是哪个目录
✅ ✅，位置本身已经说了 ✅ ✅，不必再记一遍路径 ✅ ✅。这两个字段**只用于报错** ✅ ✅："另一
个实例正在处理这个目录（pid 1234 ✅ ✅，15:00 起 ✅ ✅）" ✅ ✅。

**探测只读 ✅ ✅**：问"这个目录有没有人占着"用**只读**打开标记文件 ✅ ✅ + **共享锁** ✅ ✅
（共享锁只有在没有独占持有者时才拿得到 ✅ ✅）✅ ✅。理由 ✅ ✅：读权限就够问这个问题 ✅ ✅，
而写权限不够时**不能当成"被占着"** ✅ ✅ —— 别人留下的标记（崩了没删 ✅ ✅）会让这个目录
对所有人**永久拒绝** ✅ ✅。拿占用时同理 ✅ ✅：写不进去就只读地锁上 ✅ ✅（锁才是状态 ✅ ✅，
记录只是给报错看 ✅ ✅）。

**用完就删** ✅ ✅：应用不该在用户的目录里留东西 ✅ ✅。释放的**顺序**是关键 ✅ ✅ ——
**先删名字、后放锁** ✅ ✅：名字消失的那一刻锁还攥着 ✅ ✅，别人要么拿不到这把锁 ✅ ✅
（退回重试 ✅ ✅），要么拿到的是新建的 inode ✅ ✅（那时我们已收工 ✅ ✅）。反过来（先放锁
再删名字 ✅ ✅）就坏透了 ✅ ✅：别人刚拿到锁 ✅ ✅，我们的删除把名字从他脚下抽走 ✅ ✅。

**两种"拿不到锁"要分开报** ✅ ✅：

| 情形 ✅ | 行为 ✅ |
|---|---|
| **有人占着** ✅ ✅ | **良性拒绝** ✅ ✅ —— 不是致命错误 ✅ ✅：正在跑的那一批不受影响 ✅ ✅ |
| **文件系统不支持锁** ✅ ✅（ENOLCK ✅ ✅ / EOPNOTSUPP ✅ ✅ / ENOSYS ✅ ✅）✅ ✅ | **报错拒绝这一趟** ✅ ✅ —— "这个文件系统不支持文件锁 ✅ ✅，无法保证同一目标只有一个实例 ✅ ✅" ✅ ✅。不降级成"警告后照跑" ✅ ✅：那种文件系统上两个实例并发就是**静默损坏产物** ✅ ✅ |

**谁在哪一步拿** ✅ ✅：CLI 解析出目标后立刻拿 ✅ ✅（早于配置读取 ✅ ✅、文件采集 ✅ ✅、编码
✅ ✅）；GUI 在**批次开工时**拿 ✅ ✅ —— actor 先做**廉价探测** ✅ ✅（祖先 ✅ ✅ + 目标自身
✅ ✅，不扫树 ✅ ✅），冲突就是**良性拒绝** ✅ ✅（命令被拒 ✅ ✅，不是错误 ✅ ✅）；真正的拿
（加上后代扫描 ✅ ✅）放在 worker 线程里 ✅ ✅ —— 那是一次树遍历 ✅ ✅，不能占住 actor ✅ ✅。
worker 拿不到时**这一批开不了工** ✅ ✅ → 终态错误事件 ✅ ✅（跟"编码器缓存没就绪"同一类
✅ ✅）：冲突在写任何产物之前暴露 ✅ ✅。

**批的范围 = 它的输入所在目录的公共祖先** ✅ ✅：GUI 的批拿到的是**文件清单** ✅ ✅，不是
目录 ✅ ✅ —— 批把产物写在输入旁边 ✅ ✅，所以范围是覆盖这些目录的那一个 ✅ ✅。

**只读动作（预演 ✅ ✅、分析 ✅ ✅）不占用** ✅ ✅ —— 它们不写产物 ✅ ✅ —— 但开工前也
**探测** ✅ ✅：目标本身 ✅ ✅ 或祖先 ✅ ✅ 被占着 ✅ ✅ → 拒绝 ✅ ✅（读一个正在写的目录 ✅ ✅，
数字不可靠 ✅ ✅）。子树里被占着的情况 ✅ ✅ 只读动作查不出来 ✅ ✅（那要扫树 ✅ ✅）✅ ✅ ——
但写产物的动作会查 ✅ ✅，所以**不会有两个实例写同一个目录** ✅ ✅ ✅。

**守卫与清理** ✅ ✅：守卫保证同一目标同一时刻只有一个实例 ✅ ✅ → 目录里的产物只可能是
这一趟写的 ✅ ✅，或者上一趟留下的完整产物 ✅ ✅（写坏的那份当场删掉 ✅ ✅）—— 所以"清理
目录"这件事不需要归属信息 ✅ ✅。

#### 3.11.2 执行：子进程和输出所有权

- 编码器**直接写目标路径** ✅ ✅ —— 不经过中间文件 ✅ ✅。守卫（§3.11.1.1 ✅ ✅）保证同一目标同一时刻只有一个实例 ✅ ✅，没有人会和这一趟抢同一个输出文件 ✅ ✅；再套一层临时文件 ✅ ✅，只是让磁盘同时存着旧产物和新产物 ✅ ✅（那个文件两倍空间 ✅ ✅），换来的只是"输出路径上不会出现半截文件" ✅ ✅ —— 而失败与中断本来就会删掉写坏的文件 ✅ ✅。
- **写坏了就删掉** ✅ ✅：中断时删掉半截输出 ✅ ✅；执行失败时删掉**这一次**写下的输出 ✅ ✅（只删这一次写下的 ✅ ✅ —— ffmpeg 还没打开输出就失败的话 ✅ ✅，旧产物完好 ✅ ✅，不能删 ✅ ✅）；NVENC 重试前那次尝试留下的东西由重试覆盖 ✅ ✅。所以目标目录里留下的是完整产物 ✅ ✅ —— 错了就重来 ✅ ✅。
- CLI 信号和 GUI 取消触发同一取消契约。信号入口只发出取消请求。进程监督独立于 stdout/stderr 读取，负责停止并回收子进程，再清理半成品。外部探测和等待编码器准备也必须检查取消或具有明确的超时。
- **中断要留下记录**。取消不是一个"马上退出"的动作：信号入口只**置位**取消令牌，
  批在文件边界停下，**已经编好的文件照样落盘**，然后进程才以非零码退出
  （130 = 128 + SIGINT 的惯例）。中断前的记录一份都不丢——这正是 `BatchReport`
  存在的理由。
- **信号处理器是进程的第一件事**。取消令牌在 `main` 的第一行就创建，信号处理器
  紧接着安装——**早于**配置读取、能力探测、文件采集这些"启动期"的工作。
  理由：信号处理器没装好之前到达的信号走的是**默认处置**（进程被杀），那半成品
  留在盘上、记录一份不落，等于绕过了整个取消契约。**启动窗口必须不存在**，
  所以安装点不能在"要开始编码了"的地方，只能在进程入口。
- **启动期也要能被取消**：能力探测、文件采集这些阶段要么检查取消令牌，要么有明确的
  时间上限——不能让 Ctrl-C 在那里干等着。
- **取消走状态文件，信号只是它的前端**。取消请求就是**操作目录**里 `cancel` 这个文件的
  **存在与否**（`$RENENCODER_CANCEL_FILE` 可覆盖 ✅）。批在轮询里看它 —— 于是：
  - **时限 = 一个轮询间隔**（≤ 100ms ✅ ✅），**与平台无关** ✅ ✅：靠 OS 信号接收的
    话，保证取决于平台（Unix 三个信号 ✅ / Windows 五个控制台事件 ✅ ✅，各有各的坑
    ✅ ✅），而且"信号在接收方就绪之前到达"✅ → **保证为零** ✅ ❌；
  - **谁能取消一个正在跑的批** ✅ ✅：写那个文件就行 ✅ ✅（脚本 ✅、另一个进程 ✅、
    UI ✅ ✅）；
  - Ctrl-C 的平台差异缩到一行 ✅ ✅（捕获信号 → 把取消写出来 ✅ ✅）。

  内容不重要（只看存在与否）✅ ✅，所以创建不需要原子写 ✅ ✅。遗留请求的清理属于
  请求通道的生命周期 ✅ ✅（§3.10.7 ✅ ✅）。

- **两条信号路径，同一个契约**：Unix 的 SIGINT / SIGTERM / SIGHUP 与 Windows 的
  控制台事件（CTRL_C / CTRL_BREAK / CTRL_CLOSE / CTRL_LOGOFF / CTRL_SHUTDOWN）
  都只做一件事——**把取消写出来**（写取消文件 ✅ ✅），然后立刻返回。

  写不进去**不是信号路径的附带问题** ✅ ✅：那是 app 目录不可写 ✅ ✅ → 记录也写不
  进去 ✅ ✅ → 这一趟本来就完了 ✅ ✅。而"app 目录可写 ✅ ✅"是**运行能力检查**的
  一项 ✅ ✅（§3.10.7 ✅ ✅），开工前就查过 ✅ ✅ —— 所以这里写失败只说明前提条件
  破了 ✅ ✅，不是要在这里兜底的事 ✅ ✅。
  **Windows 的回调必须返回 TRUE**（"已处理"）：返回 FALSE 会让系统按默认处置把
  进程杀掉，整个取消契约被绕过。Unix 那条路没有这个坑——`signal-hook` 直接接管了
  信号的处置权。
- **Windows 的两个约束**（决定测试怎么写）：
  - `CTRL_C_EVENT` **不能**投递给特定的进程组（Windows 的限制），所以测试只能用
    `CTRL_BREAK_EVENT`，而目标进程必须用 `CREATE_NEW_PROCESS_GROUP` 创建，否则
    控制台事件投不出去；
  - `CTRL_CLOSE` / `CTRL_LOGOFF` / `CTRL_SHUTDOWN` 有超时：系统会在几秒后强杀
    进程。所以回调里只做"置位 + 返回"，不做可能阻塞的事——半成品删除与记录落盘
    都在批自己的线程上做 ✅。（回调跑在系统创建的独立线程上，不在信号处理器
    上下文里，所以一行日志是安全的 ✅；要避免的是会等很久的 IO ✅。）
- **半成品只有一个地方删**：取消时由 lib 的执行路径删（按 argv 找到那个输出文件），
  信号处理器不再自己删。于是 CLI 不再需要"当前输出路径"那个注册表
  （`PartialOutputHandle`）——它曾经存在的唯一理由就是让信号处理器去删文件。
- **终端上仍看得到 ffmpeg 的输出**：CLI 的 ffmpeg 是继承终端的（不是管道），
  它的监督靠轮询子进程的状态，取消时杀掉并回收它。GUI 走管道（要解析进度），
  两条路径的监督各自实现，但**取消契约是同一个**。
- 取消在提交开始前生效，提交进入临界区后以真实提交结果记账。已提交的文件保留，后续文件停止派发。取消不触发编码器回退。CLI 和 GUI 可共用进程监督，前端各自决定如何展示进度。

#### 3.11.3 交互：命令、事件和状态

- GUI 命令提交方提供 `RequestId`。CLI 直接调用共享执行层。接纳和拒绝均回传该 ID，接纳时绑定 `BatchId`。所有批次事件带 `BatchId`，文件事件还带批次内稳定的文件索引。`Cancel { batch_id }` 只影响匹配的活动批次。
- UI 在命令成功入队后进入 `Starting` 或 `Cancelling`。发送失败显示错误并保留原状态。启动拒绝恢复提交前状态。UI 以匹配批次的终态确认收尾。执行端释放运行名额后再发布终态，允许用户收到确认后直接重启。
- 生命周期、命令确认和结果可靠传递，进度可以合并或丢弃。进度背压不得阻塞取消入口或子进程监督。每批只有一个终态。
- 文件失败进入文件记录，批次级失败进入匹配批次的终态，配置保存等非批次操作用带 `RequestId` 的独立失败事件。UI 根据事件类型和标识处理，无需解析错误文案。

#### 3.11.4 结果：保留已完成工作

批次执行返回 `BatchReport<R> { records, termination }`（`R` 是记录元素类型，编码批为 `EncodingRecord`）。`termination` 区分 `Completed`、`Cancelled` 和 `Failed(error)`。逐文件错误写入记录，允许其余文件继续。任何终态都保留已完成文件的记录，由 CLI 和 GUI 按同一规则保存。执行层组装唯一的报告，CLI 直接消费，actor 以该报告生成带批次 ID 的终态事件。未开始的任务不计为完成，正常处理完毕的失败文件计为完成但不计为成功。

**报告覆盖所有批**：编码批、预演批、分析批都返回 `BatchReport`——"这一批怎么结束"
只有一种理解。GUI 的分析路径有自己的一套事件（每文件进度），那是**进度**的协议，
不是终态的协议：它结束时同样给出一个报告。

**一批做完要落盘，两个前端都一样**。记录的落盘是 lib 的事（§3.10.3）：格式分派（parquet / jsonl / csv，按扩展名）、默认路径（`<data_dir>/encoding-<时间戳>.parquet`）、目录与权限，都在 `data::store` 里。**谁决定落盘**是前端的事——CLI 在编码命令结束时落，actor 在批次终态时落——但**怎么落**只有一份实现。

不落盘的后果很具体：GUI 的历史面板永远是空的（它读的是 `<data_dir>/encoding-*.parquet`，而只有 CLI 在写）。这不是"两个前端风格不同"，是少了一个功能。

**摘要的措辞也在 lib**：`Summary: Total / Success / Reduced / Failed` 的计数与措辞是同一份事实，CLI 打印它、GUI 渲染它（排版各自）。CLI 现在自己数一遍 Succeeded / Reduced / Failed，GUI 自己数一遍——两遍数字迟早对不上。

#### 3.11.5 实施顺序与验收

1. NVENC 夹具、配置解析与能力对象按 §3.10.6 步骤 1-3 推进。GUI 草稿语义迁移及预演失效单列实现和验收。
2. 单例守卫与批内路径冲突校验一起实施。验收路径别名、编码失败删掉写坏的文件而旧产物不动；验收目标指定即占用、并发实例的探测与拒绝，以及换目录时先释放再占用。
3. 同步实施取消监督和批次报告。验收静默子进程和 stdout 关闭后仍运行的进程都能取消并回收。取消与提交竞争时保留已提交结果和记录。取消的 e2e 测试必须等一个**可观察的标记**（批真的开始了）再发信号，不能用固定 sleep——信号可能落在信号处理器装好之前，那样测的是默认处置，不是取消契约。e2e 在**两个平台上各跑一遍**：Unix 发 SIGINT，Windows 发 `CTRL_BREAK_EVENT`（目标进程用 `CREATE_NEW_PROCESS_GROUP` 创建）。两边等的是同一个标记，断言的是同一组结果——退出码 130、记录已落盘、已完成的文件在记录里。
4. 在监督层支持超时后接入 NVENC 刷新和代次。执行层契约稳定后接入 GUI 命令和事件，验收乱序、拒绝、取消后重新启动以及操作失败隔离。
5. 收紧任务类型，统一编码与预演构造入口。验收预演无写入、三平台构建和输出提交。GPU 冒烟纳入发布验收。

---

## 5. 错误处理策略

```rust
#[derive(thiserror::Error, Debug)]
#[non_exhaustive]
pub enum EncodeError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("FFmpeg error: {0}")]
    Ffmpeg(String),

    #[error("Probe error: {0}")]
    Probe(String),

    #[error("Invalid parameter: {0}")]
    InvalidParam(String),

    #[error("Encoder not found: {0}")]
    EncoderNotFound(String),

    #[error("FPS parse error: {0}")]
    FpsParse(String),

    #[error("User cancelled")]
    UserCancel,
}
```

CLI 前端使用 `anyhow::Result` 包裹，在 `main` 中打印错误链。

---

## 6. 测试策略

| 类型 | 覆盖范围 |
|---|---|
| 单元测试 | `parse_fps_value`、`MaxFpsConfig::need_reduce`、`MaxFpsConfig::decide` 动作表、`VideoMetadata::cap_fps` 取值、`probe_video_metadata`、`FpsStats`、`AnalysisRecord` / Polars 聚合、`collect_files`、`should_skip_encoding` 决策表、NVENC 能力探测（探测次数 / 可用标记 / 失败原因）、NVENC 码控走 `-cq` |
| 集成测试 | CLI 黑盒：encode 端到端产出 `.mkv`；analyze 默认输出 CSV 到 stdout（一行一文件，列 `path, fps, codec, duration_secs, file_bytes`）；`--format text` 旧汇总；`-o FILE` 写文件 |
| 最大帧率测试 | 25fps 不降帧、30fps 不降帧、29.97fps 不降帧、60fps 降帧 |
| 视频分析测试 | `analyze_records` 产出 per-file records；Polars `group_by(fps).agg(count, sum(duration_secs))` 与手写 `FpsStats::summary` 数值一致；`records_to_csv` 输出可被标准 CSV 解析 |
| 编码器缓存测试 | 验证只执行一次 `ffmpeg -encoders` |
| NVIDIA 测试 | NVENC 能力探测：探测只对 ffmpeg 里存在的编码器跑、探测失败标记为不可用并带原因、不可用时回退软件编码 |
| 并发安全测试 | `c-probe` + `parallel-analyze` 同时启用，100+ 文件并行探测 |

---

## 7. 开发计划

| Phase | 内容 |
|---|---|
| 1 | 创建两个 crate，设置 workspace |
| 2 | 实现核心库基础功能（错误类型、路径工具、数据结构） |
| 3 | 实现元数据探测 + 帧率解析 |
| 4 | 实现编码器缓存 + 编码器选择（含 NVENC 能力探测） |
| 5 | 实现最大帧率控制（`MaxFpsConfig`） |
| 6 | 实现命令构建（含 `-r` 输出选项 + MAX_FPS 参数映射） |
| 7 | 实现单文件转码（`ProcessOutput::Inherit`） |
| 8 | 实现批量转码（失败不中断） |
| 9 | 实现视频分析模块（串行） |
| 10 | 实现文件收集（`walkdir`） |
| 11 | 实现 CLI 前端（参数解析、信号处理、日志） |
| 12 | 实现交互模式 |
| 13 | 实现分析子命令 |
| 14 | 集成测试与文档 |
| 15 | （可选）`parallel-analyze`：`rayon` 并行分析 |
| 16 | （可选）`c-probe`：C FFI 桥接层 |
| 17 | GUI 前端（`rencoder_ui`，U21）：Dioxus desktop（0.6），actor 模型（§3.8），`Command` / `Event` 双通道；lib 新增 `start_controller` + `CancelToken` + `run_ffmpeg_with_progress` + Parquet 回读 |

---

## 8. 待确认项

| # | 确认点 | 状态 |
|---|---|---|
| 1 | 外部依赖仅 `ffmpeg` + `ffprobe`（NVENC 探测也只用 ffmpeg，不再需要 `nvidia-smi`） | ✅ |
| 2 | 不采用 `ffmpeg-next`，C FFI 作为可选高性能路径 | ✅ |
| 3 | `MaxFpsConfig` 只做数值判断，参数映射在 `build_ffmpeg_command` | ✅ |
| 4 | `-r` 是输出选项，放在 `-i` 之后、输出文件之前 | ✅ |
| 5 | `should_skip_encoding` 有完整决策表 | ✅ |
| 6 | `need_reduce` 只作为 `MaxFpsConfig` 方法，无自由函数 | ✅ |
| 7 | `EncoderCache` 不派生 `Clone`，通过 `&` 引用传递 | ✅ |
| 8 | `TranscodeResult::Failed(EncodeError)` 结构化错误 | ✅ |
| 9 | `probe_video_metadata_auto` 返回 `Result` | ✅ |
| 10 | `collect_files` 用 `walkdir`，不跟随符号链接，不用 `fd` | ✅ |
| 11 | 并行分析用 `rayon`，不用 `xargs` | ✅ |
| 12 | NVIDIA 检测：CLI `--nv`/`--no-nv` > 配置；启用时做能力实测（1 帧试编码），不用 `ffmpeg -encoders` 推断硬件 | ✅ |
| 13 | `ProcessOutput::Inherit` 透传，未来 GUI 用 `Piped` | ✅ |
| 14 | 信号处理用 `parking_lot::Mutex`（无中毒） | ✅ |
| 15 | `EncodeError` 加 `#[non_exhaustive]` | ✅ |
| 16 | `FpsStats::new(total_files)` 设置总数 | ✅ |
| 17 | `c-probe` 桥接层无状态、可重入，支持 `rayon` 并发 | ✅ |
| 18 | 视频分析模块支持串行和可选并行 | ✅ |
| 19 | CLI 提供 `encode` 和 `analyze` 两个子命令 | ✅ |
| 20 | NVENC 探测失败视为"该编码器不可用"降级，不作为 `EncodeError` 传播 | ✅ |
| 21 | `build_ffmpeg_command` 在 `src_fps = None` 时忽略 `MaxFpsConfig`，不注入 `-r` | ✅ |
| 22 | GUI 框架：Dioxus desktop (0.6)，纯 Rust + wry 后端，避免 IPC 序列化税 | ✅ |
| 23 | 单 batch per process：UI 不支持多任务，`current_output` 单例约束保持 | ✅ |
| 24 | UI 与 lib 通过 `Command`（in）+ `Event`（out）消息通信，actor 模型，互不直接调用 | ✅ |
| 25 | lib 不为 UI 新增 `Serialize` 派生（保持 `thiserror` 原样，actor 间传 Rust 类型无需序列化） | ✅ |
| 26 | `Event::Error` 仅 fatal；per-file 失败走 `EncodingRecord.error` 字段 | ✅ |
| 27 | 双轨 state：mpsc 实时事件 + Parquet 持久化；`Command::LoadHistory` 触发回读（v1 不触发,v1.1 启用） | ✅ |
| 28 | Channel 策略：`crossbeam_channel::Sender<Command>`（`unbounded()` 返回的类型；`Sender<T>` 是 `UnboundedSender<T>` 的别名，UI 与 lib 都用 `Sender` 简洁写法） | ✅ |
| 29 | lib 内部 `CancelToken`（`Arc<AtomicBool>`）由 UI `Command::Cancel` 与 SIGINT handler 共享 | ✅ |
| 30 | Log 行在 lib 透传；UI reducer 层设上限（`VecDeque` 1000 行） | ✅ |
| 31 | `Command::Cancel` 中止整批（包括当前文件），与 SIGINT 等价；不暴露"仅跳过当前文件" | ✅ |
| 32 | `Command::LoadHistory { limit: Option<usize>, after: Option<DateTime<Utc>> }` 含 payload，避免后续加字段成 breaking change（v1 不发,v1.1 启用） | ✅ |
| 33 | `pub enum Command` 和 `pub enum Event` 加 `#[non_exhaustive]`，新增 variant 不破坏 exhaustive match | ✅ |
| 34 | `ErrorCategory` 在 §3.8.3 内联列出 7 个 variant（§3.2 类型定义同步） | ✅ |
| 35 | §3.8 GUI 范围 Unix-only（matches §1.3）；wry/Dioxus 非 Unix 支持 deferred | ✅ |
| 36 | `ErrorCategory` 加 `#[non_exhaustive]`（U21，对齐 `Command` / `Event`）；`Event::Error` 故意只带 `(category, message)` 不带 stderr_tail | ✅ |
| 37 | `ErrorContext` 加 `#[non_exhaustive]`（U21，对齐 `ErrorCategory`）；外部 crate 不可构造,字段读取仍可 | ✅ |
| 38 | §3.8.7 定义 `TaskInfo` / `TaskQueue` / `TaskId` / `ProgressInfo` / `LogLine` UI state struct,让 reducer 可独立 unit-test | ✅ |
| 39 | `Command::LoadHistory` 不带 path 字段；controller 用 `config::paths::Paths` 解析（honors `$RENENCODER_DATA_DIR`,v1.1 启用） | ✅ |
| 40 | Partial-output 检测(U21)：output duration < source - 1s → partial → 删 + 重转；probe 失败视作 partial；删除失败 → `Skipped(PartialOutput)`——**2026-11-15 起移除**（输出已存在就照常转码，半成品被直接覆盖，检测没有意义，§3.3.4） | ⚠️ 已作废 |
| 41 | CLI encode summary 增加 `PartialSkip` 独立分类(`PartialOutput` 不计入 `Skipped` 也不计入 `Failed`)——**2026-11-15 起 summary 不再输出 `Skipped` / `PartialSkip` 两行**（§3.3.4、§3.4）；`SkippedReason` 的变体留在记录 schema 里（历史数据），但不再产生 | ⚠️ 已作废 |
| 42 | Per-frame Progress 事件接线：`transcode_single_with_progress` + controller 把 `FfmpegProgress` 包成 `Event::Progress`；CancelToken 真正能 kill 当前 ffmpeg 子进程 | ✅ |
| 43 | Mock 测试层：`process_one_file` + `run_actor` 抽出允许注入 encode 闭包，验证消息协议契约不需要 ffmpeg；覆盖 event sequence / busy rejection / cancel / LoadHistory 过滤（LoadHistory 测试 v1.1 触发）——**注入点已由 §3.10.4 的 `FileEncoder` 缝取代**（`process_one_file` 不再存在） | ✅ |
| 44 | `run_ffmpeg_with_progress` 签名带 `total_duration_secs: Option<f64>`(ETA 计算输入)；`FfmpegProgress` 5 字段 struct 定义;cancel 阈值 `speed >= 0.001` 才返回 Some(eta) | ✅ |
| 45 | `process_one_file` 抽出 `encode_fn: &mut F` 闭包参数让 mock 测试注入；`ProcessError` enum 区分 Encode vs EventSend——**同 43，已被 `FileEncoder` 缝取代** | ✅ |
| 46 | `load_history(data_dir, limit, after)` 显式接受 data_dir 参数(不读 env var)——让 mock 测试 parallel-safe,env vars 是 process-global 会 race（v1.1 启用） | ✅ |
| 47 | `rencoder_lib::job::EncodingRecord` 重新导出——UI 需要命名 `Event::FileCompleted` / `BatchCompleted` 携带的 record 类型 | ✅ |
| 48 | `rencoder_ui` crate bootstrap(Dioxus desktop 0.6)：reducer + 9 个 unit test;App 在 `use_hook` 里 `std::thread::spawn` 做阻塞 `crossbeam::recv()` 循环,通过 `use_signal_sync` 的 SyncSignal 直接改 Dioxus state(详见 §3.9,不用 `use_coroutine` 因阻塞 recv 会 block executor);v1 三按钮 (Start/Cancel)+ 极简状态显示;Load History 按钮存在但 v1 不显示历史(v1.1 启用 HistoryPanel) | ✅ |
| 49 | Actor 退出显式信号:`Event::ActorExited { reason: ExitReason }`——**未实现**(这个 variant 从不存在);UI 靠 evt_rx 收到 `Err` 察觉通道关闭 | ❌ 未实现 |
| 50 | Preview / Dry Run (Tier 2 #6):`Command::DryRun { params, files }` 跑 `encode_prepare` 但**不调 ffmpeg**,emit `Event::DryRunFinished { records }`;`dry_run_batch` lib helper 复用 probe + partial + skip 逻辑。UI `Preview` 按钮 → reducer arm → DryRunPanel 渲染 planned outcomes。`encode.nv` 故意忽略——dry-run 不知道也不关心 GPU 是否启用 | ✅ |
| 51 | Parallel analyze progress (Tier 3 #9):`Command::Analyze { files }` 走 `analyze_records_with_probe_and_progress_parallel` 闭包注入层(不是 c-probe FFI 回调层),emit `Event::AnalyzeStarted { total }` + N × `Event::AnalyzeProgress { done, total, failed, last: Option<AnalyzedFile> }` + `Event::AnalyzeFinished { records, failed_count }`;复合 A+B 显示(进度计数 + per-file log)、failure-only log、AnalyzedFile 新 type(不复用 AnalysisRecord 避免污染 success path)、v1 不响应 `Command::Cancel` 和 SIGINT、v1 走 busy-gate 不与 encode 并发。why 详见 §3.4 "分析进度上报" | ✅ |
| 52 | UI 事件泵架构:UI 在 `use_hook` 里 `std::thread::spawn` 一个专用 OS 线程做阻塞 `crossbeam::Receiver::recv()`,通过 `use_signal_sync` 暴露的 `SyncSignal` 直接 `state.with_mut()` 改 Dioxus state。不用 `use_coroutine`(阻塞 recv() 会 block Dioxus executor,首帧不渲染);不桥接 `futures::mpsc`(冗余);lib API(`start_controller` 返回 crossbeam ControllerHandle)不动。详见 §3.9;bisect 历史见 `docs/debug-journal/event-pump-blocking.md` | ✅ |
| 53 | 帧率上限判定源用 `r_frame_rate`（`VideoMetadata::cap_fps`），`avg_frame_rate` 仅展示/统计 —— 对齐 encode.zsh v10.2.0,修 VFR 素材"设了上限却没降帧" | ✅ |
| 54 | 帧率动作判定收敛为 `MaxFpsConfig::decide` → `FpsDecision`,跳过判定 / argv 映射 / 结果记录三处同源 | ✅ |
| 55 | 帧率参数显式声明:未超限 `-fps_mode passthrough`,超限 `-r <cap> -fps_mode cfr`,未启用上限不注入 —— 对齐 encode.zsh v10.2.0 | ✅ |
| 56 | 帧率容差 `eps = 0.01`,判定式 `src > cap + eps`（严格大于,eps 在上限右侧）—— 对齐 encode.zsh `MAX_FPS_EPS` | ✅ |
| 57 | NVENC 选择：CLI `--nv`/`--no-nv` > 配置 `[encode].nv`；启用时做能力实测（1 帧试编码）并进程内缓存；不可用则 fallback + warn（带 stderr 原文） | ✅ |
| 58 | NVENC 码控走 `-cq N -b:v 0`（恒定质量），其余编码器仍是 `-crf N` —— 对齐 encode.zsh `ENC_SPEC` | ✅ |
| 59 | NVENC 检测结果跨进程保存到 `<app_dir>/nvenc-capability.json`；指纹绑定 ffmpeg 构建；30 天过期只是建议，CLI 问 y/N、GUI 由 lib 推后日期，提示一次后 30 天内不再提示 | ✅ |
| 60 | `rencoder detect` 独立子命令 + 全局 `--detect` 选项（子命令前后都认） | ✅ |
| 61 | NVENC 编码失败即当场 fallback（软件编码器重跑该文件），不重新探测；降级对本进程后续文件生效 | ✅ |
| 62 | 选同步原语前先写清"几个读者 / 几个写者 / 锁持有多久"：单一写者用 `Mutex`，锁内不做 IO，可替换状态用 `Arc` 句柄（§3.10.1） | ✅ |
| 63 | 一个配置字段只有一个转换入口：`EncodeParams::from_app_config`；运行时 `Config` 不再重复 `max_fps` / `max_fps_eps`（§3.10.2） | ✅ |
| 64 | 同一份事实的措辞 / 映射 / 判定放 lib，交互方式各前端自己实现（§3.10.3） | ✅ |
| 65 | 同路径保护三层:`output_path_for` 命名避让 → `utils::fs::is_same_file`（父目录规范化 + 文件名大小写不敏感）→ 转码前拦截记 `Failed` 不调 ffmpeg | ✅ |
| 66 | 批在 worker 线程上跑，actor 不阻塞（§3.10.1.1 规则 5）：`Command::Cancel` 在批处理期间真正被处理 → kill 在飞的 ffmpeg + 删半成品 | ✅ |
| 67 | 一批的事件由同一个发送者发（worker 发），通道 FIFO 保住 UI 看到的顺序；批结束由 `bounded(1)` done 通道回报，actor 收到才放开 busy 闸门 | ✅ |
| 68 | busy 闸门从 `Arc<AtomicBool>` 降为 actor 本地 `bool`；跨线程可变状态只剩 cancel 的 1 bit + done 通道 | ✅ |
| 69 | NVENC 降级判定从进程级全局表改为显式持有者（actor 持权威副本，worker 拿快照，降级经 done 通道回报；CLI 持本地副本）——全局可变状态消失 | ✅ |
| 70 | 丢弃事件计数从跨发送方共享的 `Arc<AtomicU64>` 改为每个发送方一份 `DropCounter` + 首丢告警——原来只加不读 | ✅ |
| 71 | 编码链用上下文对象传参：`BatchContext`（批级）+ `FilePlan`（文件级，取代 `EncodePrepareResult::Ready`）；`select_encoder` 保持平铺参数（边界解包） | ✅ |
| 72 | 测试替身走同一条路：mock 不复制 actor，改为注入 mock 编码器驱动**同一个** actor；缝是单方法 trait（不套锁），且只在"编一个文件"这一步（prepare 之上） | ✅ |
| 73 | 抽对象的判据是"≥3 个签名 + 唯一拥有者 + 不在多处被改"，不是"参数超过 N 个"——后者会不停生类型（`ActorState` / `FileClock` 因此被否） | ✅ |
| 74 | 缝 = 单方法 trait `FileEncoder`（`&self`，可跨批共享）+ 注入的 `ActorEnv`；测试替身驱动**同一个** actor，协议只有一份实现（mock actor 删除，-280 行） | ✅ |
| 75 | `nvenc::resolve_and_store_with`：探测与落盘一起换掉 runner——否则测试会把 NVENC 记录写进开发者的真实 app 目录 | ✅ |
| 76 | 闸门拒绝走 `Event::CommandRejected`（良性），不再复用 fatal-only 的 `Event::Error`——否则 UI 会把正在跑的那批 finalize 成 Done | ✅ |
| 77 | 任务模型与执行契约（§3.11） | future |
| 78 | per-file 失败走 `EncodingRecord.error`，**不再**发 fatal-only 的 `Event::Error`（协议 §3.8.3 早就这么写，代码违反了两轮） | ✅ |
| 79 | 编码器找不到 = 一个文件的失败（`Failed` record），不是 panic——GUI 不再卡死在 Analyzing，CLI 不再 exit 101 丢光记录 | ✅ |
| 80 | partial-output 只在真的要写输出时登记——信号处理器不会删掉上一轮已完成的文件 | ✅ |
| 81 | 批里的每个文件都转码，输出已存在就覆盖写；**不引入"跳过 / 重编"这套判断**（不比较源编码、不比较配置、不给产物写指纹）——多一个概念就多一处能自相矛盾的地方。配置**每进程读一次**，整批共用同一份快照，**不按文件重读** | ✅ |
| 82 | **能力判定每进程一次 + 与 `nv` 解耦**：能力是机器事实（总是形成，`detect` 优先复用 `<app_dir>/nvenc-capability.json`），`nv` 是本次运行的选择（在 `EncodeParams` 里），选编码器时合并判断 → 切 `nv` 只重组参数，不重探能力、不换缓存（GUI 的"SaveConfig → 比新旧 → 刷缓存"旁路随之消失） | ✅ |
| 83 | **运行时 `Config` 退役**：`encoders` 与 `ffmpeg_args` 进了 `EncodeParams` → 启动时算一次的 grab-bag 类型整体消失，"这次运行用了什么配置"只有一个答案 | ✅ |
| 84 | 分层配置与一次解析（§3.10.5）：入口显式值 > 文件显式值 > 系统内建默认值，整批共用有效快照 | future |
| 85 | **编码器找不到 = 一个文件的失败**，不是 panic（§3.11.3）——`build_argv` 返回 `Result`，GUI 不再卡死在 Analyzing，CLI 不再 exit 101 丢光记录 | ✅ |
| 86 | GUI 草稿覆盖、保存回执及预演失效（§3.10.5） | future |
| 87 | NVENC 夹具及真实 GPU 发布验收（§3.10.6） | future |
| 88 | **能力判定分两级**（§3.10.5）：一级=构建带不带 NVENC，二级=机器能不能跑；记录存的是测试结果，一级失败是它的原因；每个候选都有一项，记录形状不随机器变化（磁盘格式升到 v2，旧记录作废重探） | ✅ |
| 89 | **复审周期由 lib 掌管**（§2）：记录到期时解析把它推后 30 天并落盘，前端只负责显示提示；两个前端（及以后的前端）不必各自记着延期这件事 | ✅ |
| 90 | **取消走状态文件**（§3.11.2）：取消请求 = **操作目录**里 `cancel` 是否存在；批在轮询里看它，时限 = 一个轮询间隔且与平台无关；信号只是它的前端；谁能取消一个正在跑的批 = 写那个文件 | ✅ |
| 91 | **运行能力检查**（§3.10.7）：一个检查集的聚合入口 ✅ ✅，按优先级依次跑 ✅ ✅ ✅，结果写进文件 ✅ ✅（前端读文件 ✅ ✅，不各自再算 ✅ ✅）；**检查没过不拒绝启动** ✅ ✅ ✅ —— 是信息不是闸门 ✅ ✅ ✅；GUI 换工作目录就重新检查 ✅ ✅ ✅。取消请求的可写性属于其中的文件系统检查 ✅ ✅，不是独立的一件事 ✅ ✅；**一次检查、整体共享** ✅ ✅：机器的事实（ffmpeg / NVENC）只查一次 ✅ ✅，检查、编码器缓存、UI、报告共用同一份 ✅ ✅，换工作目录只重查会变的文件系统部分 ✅ ✅ | ✅ |
| 90 | **能力是一个对象**（§3.10.5）：检查的结论与运行时的修正合成一个 `Capability`，整条链共享（选型读、回退写、actor 持权威副本）；`EncoderCache` 只剩 ffmpeg 的编码器列表；自己探一次的独立入口删除 | ✅ |
| 91 | **记录落盘归 lib**（§3.11.4）：格式分派与默认路径进 `data::store`；actor 在批次终态时落盘——**GUI 的历史面板不再永远是空的** | future |
| 92 | **摘要的计数与措辞归 lib**（§3.10.3）：两个前端不再各数一遍 | future |
| 93 | **`BatchReport` 落地**（§3.11.4）：执行层组装唯一的报告，CLI 直接消费，actor 据此发终态事件——"这一批结束了"只有一种理解 | ✅ |
| 94 | **中断要留下记录**（§3.11.2）：信号入口只置位取消令牌，批在文件边界停下，已完成的文件照样落盘，进程以 130 退出；半成品只由 lib 删，`PartialOutputHandle` 随之退休 | ✅ |
| 95 | **报告覆盖所有批**（§3.11.4）：编码、预演、分析都返回 `BatchReport` | ✅ |
| 96 | **文件位置原则**（§3.6）：每个文件放在离它被访问、离它起作用最近的地方 —— 应用级（config / log / data / 机器能力记录）在应用目录，运行级（取消请求、运行能力检查报告）在操作目录（CLI = 目标所在目录，GUI = 选定的工作目录） | ✅ |
| 97 | **路径写进配置**（§3.6）：`[paths]` 段覆盖各路径，优先级 = 入口显式值（env / CLI）> 文件显式值 > 内建默认值；配置文件自身的位置仍由 `--config` / `$RENENCODER_CONFIG` 决定 | ✅ |
| 98 | **路径一处决定**（§3.6）：lib 的 `config::paths::Paths` 一次解析出全部路径，前端不再各自现算；操作目录里的应用文件（`cancel`、`capabilities.json`）按名字跳过，不被当成输入 | ✅ |
| 99 | **单例守卫**（§3.11.1.1）：范围是一棵目录树（自己 ✅ ✅、祖先 ✅ ✅、后代 ✅ ✅）；互斥交给内核文件锁 ✅ ✅（进程死了内核放锁 ✅ ✅、取得占用是原子的 ✅ ✅、不需要 pid 判活 ✅ ✅）；标记只记 pid 与开始时刻 ✅ ✅，用完就删 ✅ ✅（先删名字后放锁 ✅ ✅）；冲突整趟拒绝 ✅ ✅，文件系统不支持锁则报错拒绝这一趟 ✅ ✅；GUI 空闲不占 ✅ ✅（只在会写产物的批次期间持有 ✅ ✅） | ✅ |
| 101 | **批内输出规划**（§3.11.1）：两个输入指向同一输出时**改名**而不是整批终止 ✅ ✅ —— `<词干>_<输入扩展名>_<序号>.<目标扩展名>` ✅ ✅；序号按排序后的路径分配 ✅ ✅（名字不随采集顺序变 ✅ ✅）；输出不落在任何输入上 ✅ ✅ | ✅ |
| 100 | **输出直写 + 写坏了就删**（§3.11.2）：编码器直接写目标路径 ✅ ✅（守卫保证没有第二个实例 ✅ ✅；中间文件只会多占一倍磁盘 ✅ ✅，不值 ✅ ✅）；中断删掉半截输出 ✅ ✅，失败删掉**这一次**写下的输出 ✅ ✅（ffmpeg 没动过输出就失败的话 ✅ ✅，旧产物留着 ✅ ✅）✅ ✅ | ✅ |

---

如有异议或补充，请指出。无异议则进入编码阶段。

## Configuration on every encode invocation

Reload the selected TOML file on every invocation. Existing outputs and matching source codecs must not bypass the current encoding settings (including CRF, bitrate, encoder and max_fps). Re-encode using the current configuration; reject input/output identity before invoking FFmpeg. Keep the existing allowed max_fps values.

## Portable distribution and tag releases

The application directory means the directory containing the executable, never a system application-data directory. Every Unix archive, Windows zip and macOS disk image includes rencoder.toml adjacent to the binary, with log/ and data/ subdirectories. Explicit --config and RENENCODER_CONFIG remain supported. Each invocation reloads the selected config.

Use scripts/dist.sh to build with Cargo and scripts/package.py to package identically on every platform. Hash final archives after adding configuration. CI checks out and validates the requested tag, builds all three targets, verifies checksums, and publishes a GitHub Release for every pushed tag (including non-SemVer tags). Manual releases require an existing tag.

## Native probing is opt-in

Default features include parallel-analyze only. The c-probe feature must be explicitly enabled when building against native FFmpeg development libraries. Default CLI and portable CI builds use the existing ffprobe subprocess implementation, avoiding a mandatory pkg-config/native FFmpeg build dependency on all platforms.

## Cross-platform interrupt registration

Keep one public install_handler API. Unix registers SIGINT, SIGTERM and SIGHUP with signal-hook. Windows registers console control events with SetConsoleCtrlHandler. Both invoke shared task logging and partial-output cleanup. Windows console close/shutdown cleanup is best-effort under OS termination deadlines.
