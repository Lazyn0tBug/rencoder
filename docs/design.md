# Rencoder – Rust 重建设计文档

**版本**：v5.1
**状态**：待确认
**变更基础**：`encode.zsh` v10.0.3 + `video_fps_stats.zsh` v2.3

---

## 1. 项目概述

Rencoder 是一个智能视频转码与分析工具，支持：

- 多种目标格式（AV1、H.264、H.265、VP9）
- CRF 或固定码率控制
- **最大帧率限制**（源帧率超标时自动降帧）
- **一次 `ffprobe` 同时探测 codec 与帧率**（零外部进程解析）
- **编码器缓存**（运行期间只执行一次 `ffmpeg -encoders`）
- **NVIDIA 硬件加速**（`--nv` 主动开启 + `nvidia-smi` 检测，双重条件）
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
| `nvidia-smi` | GPU 检测 | ⚠️ 可选（仅 `--nv` 时；未安装或执行失败一律降级为软件编码，不报错；macOS 上通常不存在，会静默走软件编码路径） |
| ~~`jq`~~ | ~~读取配置~~ | ❌ 用 `serde_json` 替代 |
| ~~`fd`~~ | ~~文件遍历~~ | ❌ 用 `walkdir` 替代 |
| ~~`xargs`~~ | ~~并行~~ | ❌ 用 `rayon` 替代 |
| ~~`grep`/`cut`/`awk`~~ | ~~解析~~ | ❌ 用 Rust 字符串操作替代 |
| ~~`nproc`~~ | ~~CPU 核心数~~ | ❌ 用 `num_cpus` crate 替代 |
| ~~`nvidia-smi`~~ | ~~推断编码能力~~ | ❌ 不再通过 `ffmpeg -encoders` 推断硬件 |

---

### 1.2 部署模型：单目录应用（U18）

Rencoder 是一个独立的第三方应用，不是发布到 crates.io 的通用库。交付与运行时都
采用**单目录应用（single-directory app）** 模式：

- **发布形态**：target 平台一份自包含目录，包含 binary + 配置 + 运行时资源（log /
  data 占位）。用户解压即用，不依赖系统路径、XDG 约定或 CWD。
- **运行时约束**：binary 自己 resolve 出父目录（`current_exe().parent()`）作为
  `app_dir`。所有持久化状态（config / log / data）从 `app_dir` 派生。CWD 只承载
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
| SIGINT → 清理半成品文件（§3.4.5） | 跨平台（`ctrlc` crate 抽象） | 跨平台（同） | `rencoder_cli::signal` + `rencoder_lib::pipeline::partial_output` |

**信号处理为什么不需要 `#[cfg]`**：本节列出的 fs_perms 调用的是 Unix-only Rust stdlib
API（`PermissionsExt::from_mode`），所以需要 `#[cfg(unix)]` 切到 no-op 分支。信号处理不同：
`ctrlc::set_handler` 在内部统一处理 Unix signal 和 Windows console event；`Arc<Mutex<PathBuf>>`、
`std::fs::remove_file`、`std::process::exit`、`eprintln!` 都是 stdlib 跨平台 API。源码里
**没有任何 platform-specific 调用**——不需要 `#[cfg]`。

**为什么不预先加防御性 `#[cfg]`**：加 `#[cfg(unix)]` + `#[cfg(not(unix))]` 重复 no-op 模式是
cargo-cult —— 真正的 platform divergence 在 `ctrlc` 内部已经被它处理。如果未来加 GUI/Tauri
build，**不复用** `rencoder_cli::signal`（webview 有自己的 beforeunload / IPC 机制），
会单独写。**所以这个 module 的 cross-platform story 是 "lib 内置 + CLI-only 作用域 + 不复用"，不是
"加 `#[cfg]` 防御"**。

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
| NVIDIA 检测 | `--nv` 参数 + `nvidia-smi` 双重条件 | 不通过 `ffmpeg -encoders` 推断；不用 `nvml-wrapper` |
| 最大帧率 | `MaxFpsConfig` 数值判断 + `build_ffmpeg_command` 参数映射 | 谓词与参数生成解耦 |
| FFmpeg 输出 | `ProcessOutput::Inherit` 透传（默认） | 与脚本一致；未来 GUI 用 `Piped` |
| 文件遍历 | `walkdir` | 纯 Rust，不依赖外部工具；不跟随符号链接 |
| 线程模型 | 转码：单线程串行；分析：可选并行（`rayon`） | 转码与脚本一致 |
| 信号处理 | `ctrlc` + `parking_lot::Mutex` | 无中毒语义 |
| 命令行解析 | `clap`（derive） | 仅 CLI 前端 |
| 交互模式 | `dialoguer` | 仅 CLI 前端 |
| 错误处理 | `anyhow`（CLI）+ `thiserror`（核心库） | 分层清晰 |
| 日志（核心库） | `log` | 核心库用 `log` 宏，保持库 API 稳定 |
| 日志（CLI 前端） | `tracing` + `tracing-subscriber` + `tracing-log` | CLI 用结构化 tracing；`tracing-log` 桥接库内 `log` 事件 |
| 日志（文件 sink） | `<app_dir>/log/rencoder.YYYY-MM-DD.NN.log`（U18） | 日滚 + 10MB 大小滚（NN 自增），7 天保留（`max_age_days=0` 不限）；自定义 `MakeWriter` 实现；`RENENCODER_LOG_DIR` 覆盖 |
| 日志（span） | `#[instrument]` + `FmtSpan::CLOSE` | 关键函数（`encode_cmd` / `analyze_cmd`）记录 span 上下文，便于排障 |
| 配置（用户面） | TOML via `AppConfig`（U18） | 替代 jq + 自写 JSON schema；unknown keys 立即报错（`#[serde(deny_unknown_fields)]`） |
| 配置（runtime 派生） | `Config` 由 `AppConfig::encode` 经 `Config::from_encode_config` 派生（U18） | JSON `Config::load` 路径仅保留在 lib 内部测试 |
| 分析聚合 | `polars`（DataFrame `group_by` + `agg`） | 复用 Polars 生态；不再手写 BTreeMap 遍历 |
| 分析输出（默认） | per-file CSV 到 stdout + 一行 summary 到 stderr | 原始数据可管道到 DuckDB/Polars 二次分析；摘要与数据流分离 |
| 转码工作记录 | `EncodingRecord`（4 维度：source / process / result / status） | 工作分析（吞吐、压缩率、失败率），不是视频分析 |
| 工作记录存储 | `<app_dir>/data/encoding-YYYY-MM-DDTHH-MM-SSZ.parquet`（按运行分文件，U18） | 每跑一次产出一个自包含文件，多批分析用 `read_parquet(['<app_dir>/data/*.parquet'])` glob；`RENENCODER_DATA_DIR` 覆盖 |
| 工作记录序列化 | 单 DataFrame 中转（`records_to_dataframe`）+ 三种 writer（CSV / JSONL / Parquet） | schema 在 `records_to_dataframe` 一处定义；writer 都是 thin wrapper |
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

**选择方案：`--nv` + `nvidia-smi` 双重条件**：

```
用户传了 --nv？
├─ 否 → 软件编码
└─ 是 → nvidia-smi 可执行且检测到 GPU？
        ├─ 未安装 / spawn 失败 → 软件编码（日志提示，不视为错误）
        ├─ 已执行但未检测到 GPU → 软件编码（日志提示）
        └─ 检测到 GPU → ffmpeg 编译了对应编码器？（内部隐式检查）
                ├─ 否 → 软件编码
                └─ 是 → nvenc ✓
```

### 2.3 关于日志栈的决策

**核心库仍用 `log`**：

- 库对外暴露的是稳定的 `Result<_, EncodeError>` API；把 `tracing` 写进 API 表面会强制所有调用方依赖 `tracing`。
- 库的日志语义是“底层发声”，固定为 `log` 即可，没必要结构化。

**CLI 用 `tracing`**：

- `tracing` 提供结构化字段和 span 上下文（`#[instrument]` 自动给每个事件挂上函数名 + 字段），排障时比纯文本日志更清晰。
- `tracing-subscriber` 的 `Registry` + 多 `Layer` 模式天然支持双 sink（stdout + 文件）。
- `tracing-log::LogTracer::init()` 把库内 `log` 事件桥接到 `tracing` 的 subscriber，调用方无需关心两边日志格式差异，`RUST_LOG` 一套规则同时控制两层。

**文件 sink 用自写 `MakeWriter` 而非 `tracing_appender`**：

- `tracing_appender::rolling` 只支持按 `Rotation`（DAILY / HOURLY / NEVER）滚动，**不支持大小阈值**。我们需要“日滚 + 10 MB 大小滚”，必须自己实现 `MakeWriter`。
- 文件名 `rencoder.YYYY-MM-DD.NN.log`：日期每天更换，NN 在当天文件超过 `max_size` 时自增。
- 启动时根据 `max_age_days` 清理过期文件（`0` 表示不清理）。
- `FileSinkHandle` 仅持有 `&Mutex<Inner>`，利用 `Mutex` 的内部可变性避开 `MakeWriter` 给的 `&'a self` 限制。

**为什么不用 `colored` crate**：

- 颜色化是终端能力，`tracing-subscriber` 的 `fmt` 层默认不染色，且 ANSI 颜色在重定向到文件时会变成噪音；如需彩色输出，让终端通过 `RUST_LOG_STYLE=always` 等环境变量启用，避免库内硬编码。

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

> 注：`nvidia-smi` 的 `io::Error`（找不到可执行文件）和"执行成功但无 GPU"都归为同一降级路径，`init_encoder_cache` 不应因此返回 `EncodeError`。

不使用 `nvml-wrapper` crate。`nvidia-smi` 是唯一的外部检测工具，且仅在 `--nv` 时调用。

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
- `data/record.rs` 拥有 `EncodingRecord` 组装 helpers（`make_skipped_record`、`make_failed_probe_record`、`encoder_settings_from_params`）
- `encoder/{command,select}.rs` 拥有 `build_ffmpeg_command` + `select_encoder`
- `pipeline/{batch,cancel,cache}.rs` 拥有 `transcode_batch` + `CancelToken` + `init_encoder_cache`
- `utils/{fs,time}.rs` 拥有 `output_path_for` + `utc_timestamp_filename`
- `config/mod.rs` 拥有 runtime `Config`（JSON 加载 → `from_value` 转换；U17+ 由 `from_encode_config` 从 `AppConfig` 派生）
- `config/app.rs` 拥有 user-facing `AppConfig`（U18 重命名自 `project.rs`；TOML 加载在 U17 新增）

```
rencoder_lib/
├── lib.rs                       # 库入口，pub mod 声明
├── analyzer/
│   └── records.rs              # probe → AnalysisRecord + aggregate → VideoStats
├── collect.rs                   # 文件收集（按目录 / 单文件 / 扩展名过滤）
├── config/
│   ├── mod.rs                   # runtime Config + Config::from_encode_config + 重导出
│   └── app.rs               # AppConfig + Encode/Analyze/Encoders/FormatArg/AnalyzeFormatArg
├── data/
│   ├── mod.rs
│   ├── extract.rs               # probe_video_metadata*, parse_fps_value, stderr_tail, output_size, output_duration
│   ├── record.rs                # make_skipped_record, make_failed_probe_record, encoder_settings_from_params
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
│   ├── cache.rs                 # init_encoder_cache（ffmpeg -encoders + nvidia-smi）
│   └── cancel.rs                # CancelToken（§3.8；Arc<AtomicBool> + cancel()/is_cancelled()）
├── types.rs                     # 跨模块共享类型（VideoFormat、EncodingRecord、MaxFpsConfig 等）
└── utils/
    ├── mod.rs
    ├── fs.rs                    # 路径规范化、output_path_for
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
- **config/** 分两层：`app.rs`（user-facing TOML schema）+ `mod.rs`（runtime `Config`，
  由 `app::EncodeConfig` 经 `Config::from_encode_config` 转换而来）。CLI 加载 TOML，构造
  runtime `Config` 喂给 pipeline；batch 和 select 看到的 `Config` 是窄的 runtime 形态。

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
├── signal.rs             # ctrlc + parking_lot::Mutex
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
    pub avg_fps: Option<f64>,
    pub nominal_fps: Option<f64>,
}

impl VideoMetadata {
    /// 优先 avg，fallback 到 nominal
    pub fn best_fps(&self) -> Option<f64> {
        self.avg_fps.or(self.nominal_fps)
    }
    pub fn is_valid(&self) -> bool {
        self.codec.is_some() || self.avg_fps.is_some() || self.nominal_fps.is_some()
    }
}

// ===== 最大帧率配置 =====
/// 只负责数值判断。参数映射由 build_ffmpeg_command 处理。
#[derive(Clone, Debug)]
pub struct MaxFpsConfig {
    pub max_fps: f64,
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
    pub has_nvidia: bool,
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
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EncodingOutcome {
    Succeeded,
    ReducedFps,
    Skipped(SkippedReason),
    Failed,
}

/// `#[non_exhaustive]`（U21）：未来 variant 不破坏现有 exhaustive match。
/// `PartialOutput` 用于 partial-output 检测成功但删除失败(罕见)的场景;
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
/// 初始化编码器缓存（一次）
/// enable_nv = true 时调用 nvidia-smi -L 检测；
/// nvidia-smi 不存在或执行失败均视为"无 GPU"降级到软件编码，
/// 不作为 EncodeError 向上传播。
/// 对齐 encode.zsh 的 init_encoder_cache()
pub fn init_encoder_cache(enable_nv: bool) -> Result<EncoderCache, EncodeError>;

/// 选择编码器
/// 对齐 encode.zsh 的 select_encoder()
/// 配置读取使用 serde_json（不依赖 jq）
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
/// **重要**：`-r <MAX_FPS>` 是**输出选项**，必须放在 `-i <input>` 之后、
/// 输出文件路径之前。放在 `-i` 之前会强制解释输入帧率，导致音画不同步。
///
/// `src_fps` 为 `None`（探测未能获取帧率）时，直接忽略 `MaxFpsConfig`，
/// 不注入 `-r` 参数——宁可不降帧，也不能用错误的帧率假设覆盖输出。
///
/// 示例（MAX_FPS=30）：
/// ```text
/// ffmpeg -y -i input.mp4 \
///   -c:v libsvtav1 -preset 6 -crf 45 \
///   -c:a libopus -b:a 32k \
///   -r 30 \
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

#### 转码跳过判断

```rust
/// 判断源编码是否匹配目标格式
pub fn should_skip_encoding(codec: &str, format: VideoFormat) -> bool;
```

**决策表**：

```
┌──────────────────┬────────────────┬──────────────────────────────┐
│ 源编码匹配目标？  │ 需要降帧？      │ 结果                         │
├──────────────────┼────────────────┼──────────────────────────────┤
│ 否               │ 不关心         │ 正常转码                     │
│ 是               │ 否             │ SkippedSameCodec（跳过）     │
│ 是               │ 是             │ ReducedFps（继续转码以降帧） │
└──────────────────┴────────────────┴──────────────────────────────┘
```

#### 单文件转码

```rust
/// 内部流程（对齐 encode.zsh 的 encode_single_file + partial output 重转）：
///   1. probe_video_metadata（一次 ffprobe,源）→ 拿 duration
///   2. 输出存在?→ probe output duration 比较:
///       a. output + 1s < source → partial → remove + 继续到 7
///       b. output probe 失败 / 无 duration → 视作 partial → remove + 继续
///       c. 其他 → `Skipped(OutputExists)` 提前返回
///       d. remove 失败 → `Skipped(PartialOutput)` 提前返回
///   3. MaxFpsConfig::need_reduce（谓词）
///   4. should_skip_encoding（编码匹配 + 帧率判断）
///   5. select_encoder（缓存）
///   6. build_ffmpeg_command（MAX_FPS 参数映射）
///   7. 执行（ProcessOutput 控制输出）
pub fn transcode_single(
    task: &TranscodeTask,
    params: &EncodeParams,
    encoder_cache: &EncoderCache,
    output: ProcessOutput,
) -> Result<TranscodeResult, EncodeError>;
```

**Partial output 检测**（U21 引入,§3.4.5 "注意"段的对应实现）：

- **目的**:SIGTERM/SIGHUP 现在不再清半成品（见 §3.4.5）。下次启动时如果
  output 存在且是 partial,自动删 + 重转,不让用户踩坑（以为"已完成"实际是损坏）。
- **检测方法**:`probe_video_metadata_auto` 跑 output,对比 source duration。
- **阈值**:output duration < source duration - 1.0 秒 → partial。
  1 秒容忍 codec delay + 容器 metadata 误差。
- **保守 fallback**:output probe 失败 / 无 duration → 视作 partial。
  这是合理的:video 输出一定有 duration;没有 = 文件损坏。
- **删除失败 fallback**:`fs::remove_file` 失败（如权限拒绝）→
  `Skipped(PartialOutput)`,encode 不继续（output 在那挡着）。
- **Post-run 0 字节清理**:ffmpeg exit 0 但 output 缺失 / 0 字节,且
  source 有 known duration → 立即 `remove_file` 并记录 `Failed`。
  不留 0 字节 ghost 等下次 partial detector 兜底（review #114）。
- **stderr 内存上限**:ffmpeg verbose 跑可能喷出数百 MiB
  stderr,`run_ffmpeg_with_progress` 的 reader 线程有 256 KiB
  上限（`STDERR_READER_MAX_BYTES`）,超出部分丢入
  `[stderr truncated; N bytes dropped]` 注解。reader 持续 drain
  pipe 到 EOF,因此 `stderr_thread.join()` 不会卡住（review
  #112 + #116）。
- **代价**:每个 skipped file 多一次 ffprobe 调用（~10ms）。
  1000-file 二次跑额外 ~10s,可接受。

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

高性能元数据探测：直接调 libavformat，跳过 `ffprobe` 进程 spawn。**默认开启**（在
`Cargo.toml` 的 `default = ["c-probe", "parallel-analyze"]` 里），无 libavformat 的环境
可手动关掉（见下文）。

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
default = ["c-probe", "parallel-analyze"]    # 默认开启 c-probe
c-probe = ["dep:cc", "dep:pkg-config"]        # 需要 cc + pkg-config build-deps
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

**关闭 c-probe 的场景**：

```bash
# host 没有 libavformat（pure-CLI fallback only）
cargo build --no-default-features --features parallel-analyze
# 探测走 spawn ffprobe，每次多 30-100ms 进程启动开销（per file）
```

**Runtime 检查**：

- c-probe 失败时自动 fallback 到 ffprobe CLI，warn 一行后继续：

  ```
  WARN c-probe failed for /path/in.mp4, falling back to ffprobe CLI: <reason>
  ```

- pkg-config 找不到 libavformat 时 `build.rs` 直接 panic，提示"pkg-config libavformat
  failed: <stderr>"。修复：装 ffmpeg-dev / libavformat-dev 包。

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
  --                      之后的所有参数作为FFmpeg原生参数透传

剩余的 format / bitrate / max-fps / crf / nv / extensions / encoders 全部在
`rencoder.toml` 的 `[encode]` 段。详见 §3.4.5 App-level config + single-directory app。
```

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
  Skipped:    Z    (SameCodec | OutputExists)
  PartialSkip: P   (PartialOutput —— partial detected 但删除失败)
  Failed:     F
```

**为什么 PartialSkip 是独立分类（不是 Skipped 也不是 Failed）**：

- 不是正常 skip:用户没看到完整文件,partial 留在磁盘上挡着后续 encode
- 不是 failed:ffmpeg 没启动,没有尝试 encode
- 独立分类让用户在 summary 里看到这个异常状态;典型场景是权限拒绝或文件被占用
- §8 item 40 锁定决策

**实现位置**:`rencoder_cli/src/main.rs::print_encode_summary` 显式 match 每个
`SkippedReason` 变体,新 variant 加进 `PartialSkip` 计数(不进 `Skipped` 桶)。

#### 信号处理（U18 + U18.1）

**意图**：用户按 Ctrl-C 中断转码时，handler 读取当前正在写入的 output path，删除半成品
文件，避免留下损坏的 `.mkv`/`.mp4`。`ctrlc` crate 跨平台处理 SIGINT（Unix signal + Windows
console event）。这跟 `encode.zsh` 里的 `trap _cleanup INT` 行为对应。

**信号覆盖**：
- `SIGINT`（Ctrl-C / 中断键）

**注意**：`encode.zsh` 还 trap 了 TERM/HUP；`ctrlc` 只覆盖 SIGINT。TERM/HUP 路径下当前
直接 kill 进程、不清半成品文件——这是 `ctrlc` 选择的已知 tradeoff（保留 Windows 跨平台
支持；U18.1.1 的 `signal-hook` 路径曾覆盖这三个信号，2026-09 revert 回 `ctrlc` 以保留
Windows extension）。如需回到 TERM/HUP cleanup，需要 `libc::signal` 的 C-style handler
（不能访问 `Arc<Mutex>`，与当前 `TaskContext` 设计不兼容）——future work。

**Cross-platform story**：源码无 platform-specific 调用 —— `ctrlc` 内部统一处理
Unix signal / Windows console event，`Arc<Mutex<TaskContext>>` + `std::fs::remove_file` +
`std::process::exit` + `tracing::warn!` 都是 stdlib 跨平台 API。所以本节代码**不需要 `#[cfg]` 分支**，
跟 §1.3 表里 `fs_perms`（有 `#[cfg(unix)]`）不同。详细原因见 §1.3 末尾的"为什么信号处理不需要
`#[cfg]`"小节。GUI build（§3.8）**不复用** `rencoder_cli::signal`（webview 有自己的
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

**配置文件路径解析顺序**（`config_path::resolve`）：
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
（dev-only 参考，dist 用户无需手动拷贝）。schema 校验在 `AppConfig::load`；runtime
`Config` 通过 `Config::from_encode_config(&app_config.encode)` 派生。

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
| 探测、跳过判断、argv 构造、ffmpeg 执行 | `transcode_single` | `transcode_single`（不变） |
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

---

### 3.5 数据流

#### 转码流程

```
CLI 解析参数 → EncodeParams
    ↓
collect_files → 文件列表（walkdir）
    ↓
init_encoder_cache(enable_nv) → EncoderCache（一次）
    ├─ ffmpeg -encoders（始终）
    └─ nvidia-smi -L（仅 --nv 时）
    ↓
设置信号处理（parking_lot::Mutex）
    ↓
transcode_batch / transcode_single
    ↓
每个文件内部：
    1. 检查输出是否已存在
    2. probe_video_metadata（一次 ffprobe）
    3. MaxFpsConfig::need_reduce
    4. should_skip_encoding（决策表）
    5. select_encoder（缓存）
    6. build_ffmpeg_command（-r 输出选项）
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

**运行时视图**（用户解压 + 首次运行后）：

```
<user-chosen-install-dir>/     # = app_dir = current_exe().parent()
├── rencoder                   # 用户调用的入口
├── rencoder.toml              # 用户编辑的偏好（auto-create 后存在）
├── log/                       # 日志：rencoder.YYYY-MM-DD.NN.log
└── data/                      # 工作记录：encoding-<ISO8601>Z.parquet
```

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

release 流程由 `.github/workflows/release.yml` 编排，触发条件为 tag push
匹配 `v[0-9]+.[0-9]+.[0-9]+*`（cargo SemVer style）。

```
tag push v0.x.y
  ↓
build (matrix: 3 targets)
  ├─ aarch64-apple-darwin   -> rencoder_cli-aarch64-apple-darwin.tar.xz (cargo-dist) + U18 augmentation
  ├─ x86_64-unknown-linux-gnu -> rencoder_cli-x86_64-unknown-linux-gnu.tar.xz (cargo-dist) + U18 augmentation
  └─ x86_64-pc-windows-msvc  -> rencoder_cli-x86_64-pc-windows-msvc.zip  (cargo-dist; portable)
  ↓
macos-dmg (needs build; only on aarch64-apple-darwin success)
  ├─ download aarch64-apple-darwin artifact
  ├─ brew install create-dmg
  ├─ ad-hoc codesign -s -<binary> (mountable on Apple Silicon)
  └─ create-dmg → rencoder_cli-aarch64-apple-darwin.dmg
  ↓
release (needs build + macos-dmg)
  └─ softprops/action-gh-release --generate-notes
```

**为什么不用 cargo-dist 生成 GitHub Actions workflow**：

cargo-dist 0.32 的 `dist init --ci github` 会生成一个 600+ 行 Jinja2 模板的 release.yml，
带 partials、prerelease logic、SBOM、attestations、cross-platform runner matrix 等大量
默认 feature。本项目只产 3 targets + 1 个 DMG，**手写 224 行比承载 cargo-dist 的全部 feature set
更可维护**。如果未来需要 homebrew / npm 发布，再考虑 cargo-dist 的 partials 系统。

**cargo-dist 0.32 限制 + 应对**：

cargo-dist 0.32 不支持 `dmg` installer 类型（只支持 msi/shell/powershell/npm/homebrew）。
应对：单独的 `macos-dmg` job 用 `create-dmg`（homebrew formula，macos-latest runner 自带）
+ ad-hoc codesign（`--sign -`）让 DMG 在 Apple Silicon 上能挂载。
正式签名 + notarization 需要 Developer ID + Apple ID secrets，那是独立的 story。

**Windows 限制**：

`scripts/dist.sh` 里的 augmentation 用 Unix-only `tar -xJf` 语法，Windows runner
跑同样脚本会失败。`build` job 用 `if [ "$target" != ...windows... ]` 跳过 Windows 的 augmentation，
所以 Windows zip 不含 `rencoder.toml` / `log/` / `data/`。U18 在 Windows 上的最终 layout
需要 U19 加 PowerShell augmentation step（或用 git-bash 的 tar）。

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
UI actor (Dioxus)  ──Command──►  lib actor (BatchController bg thread)
                 ◄───Event────
```

- 两个 actor 完全独立，**只**通过消息通信。无共享变量、无回调、无跨边界函数调用。
- ffmpeg 子进程、`PartialOutputHandle`、`Child` handle、SIGINT 全部封装在 lib actor 内部。
- 这把 lib 内部的复杂性收敛到一处，UI 端只看到消息流。

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
    /// 把当前加载的 `AppConfig` 写回原路径（U21 / Tier 2 #8）—— Save 按钮
    /// 走这条；lib 内部 `AppConfig::save(&config_path)`，发 `Event::ConfigSaved`
    /// 或 `Event::Error`。
    SaveConfig,
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
    /// shutdown — lets the UI display "controller stopped: <reason>"
    /// instead of going blank on channel close.
    ///
    /// Without this, UI only learns lib exited via `evt_rx.recv()`
    /// returning `Err` (channel disconnected). That's an implicit
    /// signal that carries no reason, no error category, no
    /// recovery affordance.
    ///
    /// Lifecycle symmetry: `Start` is construction (no message),
    /// `Exit` is one explicit message before disconnect.
    ActorExited { reason: ExitReason },

    // Error topic（仅 fatal；per-file 失败走 EncodingRecord.error）
    /// 仅 fatal 错误；payload 故意精简为 `(category, message)`。
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
```

UI 启动时调一次 `start_controller()` 拿到 `ControllerHandle`,然后 `into_split()` 把两端分给不同持有者:cmd_tx 给多个 UI 组件(可克隆),evt_rx 给 UI 顶层的事件消费线程(独占消费,见 §3.9 "UI 运行时 / Dioxus 适配")。

- `cmd_tx`：可克隆,供多个 UI 组件使用(开始按钮、取消按钮、历史按钮)。
- `evt_rx`：由 UI 顶层**专用 OS 线程**独占消费;不广播、不多订阅者——保证单消费者的因果序。`crossbeam_channel::Receiver` 是 single-consumer,无法 clone——这正是我们想要的(避免事件分发歧义)。具体 UI 端如何消费(为何不用 `use_coroutine` + async 块)见 §3.9。
- Channel 策略:`crossbeam unbounded`(`std::sync::mpsc` 无 unbounded 变体)。单 batch 下每秒最多 ~50 个事件,UI 永远消费得过来;如未来有背压风险再换 `bounded + DropOldest`。
- Actor 线程命名:`rencoder-controller`,便于 thread 列表里识别。

**Lib 内部细节**:actor 线程启动时一次性加载 `AppConfig`(经 `resolve_config` + `AppConfig::load` + `Config::from_encode_config`)+ 初始化 `EncoderCache`(从 `encode.nv`)。失败 → emit `Event::Error(ConfigError)` + 线程退出(UI 看到错误 + 之后无事件,知道 actor 死了)。

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

- `transcode_batch` 签名;`EncodingRecord` / `EncodingOutcome` / `ErrorContext`(见 §3.2)
- `TaskManager` / `PartialOutputHandle` / signal handler(CLI 端)
- `EncodeError`(保持 `thiserror` 原样,不加 `Serialize`);`ProcessOutput` 枚举(见 §3.2)
- `AppConfig` 加载/校验
- CLI 路径的 ffmpeg 编排(含 stderr 收集 + partial output 清理)——保留 `cmd.output()` 同步路径

**新增**(U21):

| 位置 | 抽象 | 职责 |
|------|------|------|
| `pipeline/cancel.rs` | `CancelToken` | `Arc<AtomicBool>` + cancel 标志(`run_ffmpeg_with_progress` 轮询) |
| `pipeline/batch.rs` | `run_ffmpeg_with_progress` | GUI 路径的 cancellable ffmpeg 调用——`cmd.spawn()` + reader thread + progress 回调 |
| `pipeline/batch.rs` | `encode_prepare` | CLI / GUI 共享的 probe + partial detect + skip + argv build 逻辑 |
| `pipeline/batch.rs` | `transcode_single_with_progress` | GUI 路径的 per-file 入口(共享 `encode_prepare`,ffmpeg 调用换成 spawn 路径) |
| `pipeline/batch.rs` | `build_encode_record` | CLI / GUI 共享的 record 构造(成功 / 失败两种 outcome) |
| `pipeline/batch.rs` | `FfmpegProgress` struct | `Event::Progress` 的源数据 |
| `pipeline/batch.rs` | `dry_run_batch(params, cache, config, files)` | Dry-run 路径(V1.1):跑 `encode_prepare` per file 但**不调 ffmpeg**,返回 planned `EncodingRecord`(Ready → Succeeded,Skipped/FailedProbe → as-is)。`encode.nv` 故意忽略——dry-run 不知道也不关心 GPU 是否启用 |
| `job.rs` | `process_one_file` | controller StartBatch 循环里的 per-file 逻辑;接受泛型 `encode_fn` 闭包,让测试注入 mock encoder |
| `job.rs` | `encode_error_category(&EncodeError) -> ErrorCategory` | 编码错误 → UI 展示分类的映射 |
| `job.rs` | `Command::DryRun { params, files }` | Dry-run 入口(V1.1):跟 StartBatch 同样的 busy-gate,但 emit `Event::DryRunFinished` 而不是 `BatchCompleted` |
| `job.rs` | `Event::DryRunFinished { records }` | Dry-run 结果:planned outcomes,**没有**实际 ffmpeg 输出 |
| `analyzer/records.rs` | `analyze_records_with_probe_and_progress_parallel<F, P>` | U22 / Tier 3 #9：并行 + 进度 closure-injection。`F` 是 probe 闭包（同 `analyze_records_with_probe_parallel`），`P: Fn(AnalyzeProgress) + Send + Sync` 在每个 worker 完成时调一次。`AnalyzeProgress { done, total, failed, last }` 4 字段 |
| `analyzer/records.rs` | `AnalyzeProgress` struct + `AnalyzedFile { path, outcome: FileOutcome }` + `FileOutcome { Ok, ProbeFailed(EncodeError) }` | U22：进度回调 payload。`AnalyzedFile` 是新 type（不复用 `AnalysisRecord`——后者没有 `ProbeFailed` outcome 字段，复用得加 `error: Option<_>` 污染 success path）|
| `job.rs` | `Command::Analyze { files }` arm | U22：调用 `analyze_records_with_probe_and_progress_parallel` with `probe = probe_video_metadata_auto`，`progress = |p| evt_tx.send(Event::AnalyzeProgress { ... })`。先发 `AnalyzeStarted { total }`，批结束发 `AnalyzeFinished`。**走** busy-gate（v1 限制——UI 不能同时跑 analyze + encode；analyze 是 read-only，理论上可与 encode 并存，但 v1 简化起见跟 DryRun 一致复用 busy-gate）|
| `job.rs` | `Event::AnalyzeStarted` / `Event::AnalyzeProgress` / `Event::AnalyzeFinished` | U22：见 §3.8.3。`Event::AnalyzeProgress` 在 mock actor 里是 no-op（test seam，ref-count 即可）|
| `data/store.rs` | `read_records(path)` | 单 Parquet 文件回读(v1 不使用,v1.1 启用) |
| `encoder/ffmpeg_arg.rs` | `ExtraArg` enum + `ExtraArg::parse` factory + char-class 模块 | 类型级 ffmpeg argv 安全边界(§3.3.7)——非法 argv 在 parse 阶段被拒,运行时无法塞 unsafe 值。替换 v1 的 runtime `validate_extra_args` |
| `types.rs` | `InputPath` newtype + `InputPath::new(&Path)` factory + `InputPathError` | 输入路径类型级安全边界(§3.3.8)——dash 前缀文件名在构造时就被拒,`build_ffmpeg_command` 不可能拿到 `-vf_negate.mp4`。替换 `collect_dir` 里的运行时 dash-prefix 检查(review #109 收尾) |

**`process_one_file` testability surface**(§8 item 43)——这是 §3.8 唯一保留的函数签名,因为它**解释了为何要抽出这个 helper**(测试性):

```rust
fn process_one_file<F>(
    task: &TranscodeTask,
    params: &EncodeParams,
    cache: &EncoderCache,
    config: &Config,
    cancel: &CancelToken,
    index: usize, total: usize,
    encode_fn: &mut F,
    emit: &Sender<Event>,
) -> Result<EncodingRecord, ProcessError>
where
    F: FnMut(&TranscodeTask, &EncodeParams, &EncoderCache, &Config, &CancelToken, &mut dyn FnMut(FfmpegProgress))
        -> Result<EncodingRecord, EncodeError>;
```

其他签名(`run_ffmpeg_with_progress`、`FfmpegProgress` 字段、`EncodeError→ErrorCategory` 映射表) 在代码注释里。**`FfmpegProgress` struct + `run_ffmpeg_with_progress` 工作流 + ETA 计算细节** 都在 `pipeline/batch.rs` 的 doc comment 里——读代码注释,不在这里重复。

**`EncodeError → ErrorCategory` 映射**(process_one_file 错误路径使用):

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

| `job.rs` | `start_controller() -> ControllerHandle` | 编排：后台线程 + mpsc + 现有 `transcode_batch`（**复用 CLI 路径的纯函数，不 fork**）+ 新 `run_ffmpeg_with_progress`；job.rs 是 thin glue，把现有 pieces 装成 actor |

**Controller 启动时加载 AppConfig**（U21 增量）：
- `config::path::resolve_config()` 拿路径（`$RENENCODER_CONFIG` > `<app_dir>/rencoder.toml`）
- `AppConfig::load(&path)` 读 TOML（缺失时自动创建默认文件，与 CLI 一致）
- `Config::from_encode_config(app_config.encode.as_ref())` 派生 runtime Config
- `init_encoder_cache(app_config.encode.as_ref().and_then(|e| e.nv).unwrap_or(false))`
- 失败 → `Event::Error(ConfigError)` 后线程退出
- 这把 controller 与 CLI 拉到同一配置源——GUI 不再绕开 `rencoder.toml` |
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
    /// Files that would NOT be encoded (outcome = `Skipped(...)`
    /// or `Failed`). Set on `Event::DryRunFinished`.
    to_skip: usize,
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
#[allow(clippy::large_enum_variant)] // only Copy + 4 variants, no payload
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BatchPhase {
    /// `Command::DryRun` dispatched; awaiting `Event::DryRunFinished`.
    /// Dir picker disabled. Cancel button shown (best-effort).
    Analyzing,
    /// DryRun done; user must explicitly confirm before encoding
    /// begins. Start button label flips to "encode N files".
    /// Dir picker disabled.
    AwaitingConfirm,
    /// `Command::StartBatch` dispatched; awaiting `BatchCompleted` /
    /// `BatchCancelled` / `Event::Error`. Progress bar active.
    /// Dir picker disabled. Cancel button shown.
    Encoding,
    /// Final state after the batch lifecycle ends. Dir picker
    /// re-enables. Start button re-arms (clicking it transitions
    /// back to `Analyzing` for a new cycle).
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
/// **Phase-aware `UserClickedStart`** (§3.8.7.1):
///
/// ```text
///   ┌─ phase == None      → build params, collect files,
///   │                       tasks.active = Some(Analyzing { .. }),
///   │                       send Command::DryRun
///   │
///   ├─ phase == Done      → same as None (re-arm)
///   │
///   ├─ phase == AwaitingConfirm
///   │                     → tasks.active.as_mut().unwrap().phase = Encoding,
///   │                       tasks.active.as_mut().unwrap().records.clear(),
///   │                       send Command::StartBatch
///   │
///   └─ phase == Analyzing | Encoding
///                         → set last_error "task in progress",
///                           return early (busy-gate)
/// ```
///
/// **Phase-aware `UserClickedCancel`**:
///   - `Analyzing` / `Encoding`: send `Command::Cancel`. The lib
///     respects cancel_token only during encoding (the analyze
///     path doesn't poll cancel — best-effort; see §3.8.7.4).
///   - `AwaitingConfirm`: clear `tasks.active` locally (no
///     command needed — DryRun already finished, lib is idle).
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
            // Compute to_encode / to_skip from records.
            //
            // **EncodingOutcome match scope**: DryRun records only
            // carry `Succeeded` / `Skipped(...)` / `Failed` —
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
                t.to_skip = t.total_files - t.to_encode;
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

> **TL;DR** — 一个 task 走四个 phase(Analyzing → AwaitingConfirm →
> Encoding → Done),`BatchPhase` 是唯一真相。Dir picker 在任何 active
> phase(Analyzing / AwaitingConfirm / Encoding)期间被锁;只有 `Done`
> 之后才能再选目录。

**State diagram**:

```text
                UserClickedStart                          Event::DryRunFinished
   [None] ─────────────────────────────> [Analyzing] ───────────────────> [AwaitingConfirm]
      ▲                                     │                                       │
      │                                     │ Cancel (best-effort)                  │ UserClickedStart
      │                                     │                                       │  dispatches Command::StartBatch
      │                                     ▼                                       ▼
      │                                  [None] (local clear)                  [Encoding]
      │                                                                         │
      │                              ┌──────────────────────────────────────────┤
      │                              │ Event::BatchCompleted                    │
      │                              │ Event::BatchCancelled                    │
      │                              │ Event::Error                             │
      │                              ▼                                          │
      │                          [Done] ◄────────────────────────────────────────┘
      │                              │
      └──────────────── UserClickedStart (re-arm) ─────────────────────────────────┘
```

**Per-phase semantics**:

| Phase | 来源 | Dir picker | Start btn | Cancel btn | Task page 显示 |
|-------|------|-----------|-----------|------------|---------------|
| `None` | initial / post-Cancel | enabled | "Start ▸"(disabled if dir not picked) | hidden | "No task running. Pick a directory and click Start." |
| `Analyzing` | reducer dispatch DryRun | **disabled** | "Analyzing…" (disabled) | "Cancel ✕" (best-effort — lib's Analyze handler 不轮询 cancel_token) | "probing N files…" + spinner |
| `AwaitingConfirm` | `Event::DryRunFinished` | **disabled** | "encode N files ▸" (enabled) | "Cancel ✕" (best-effort local clear, no lib command — lib idle after DryRun) | `BATCH #1` + 12 files / 10 encode / 2 skipped breakdown + TASK PLAN 表 |
| `Encoding` | `Event::BatchStarted` | **disabled** | "Encoding M/N" (disabled) | "Cancel ✕" (dispatches `Command::Cancel`, lib 终止 ffmpeg via cancel_token) | progress bar + per-file table + ETA |
| `Done` | `BatchCompleted` / `BatchCancelled` / `Error` | **enabled** | "Start ▸" (re-arms, transitions back to `Analyzing`) | hidden | "BATCH #1 · N encoded / M skipped / 0 failed" |

**No auto-switch**:用户在 `[SETUP]` 页面点 Start 后,**UI 不自动切换**到 `[TASK]` 页(per §3.8.7.1 product-lens review 反馈——之前的 auto-switch 会把误选目录的用户困在 TASK 页,recovery 需要等分析结束 + 取消 + 切回 SETUP)。Batch 在后台跑,UI 停留在 `[SETUP]`;用户看到 SettingsPanel 下方的 phase 状态变化或自己点 `[TASK]` tab 看进度。Dir picker 锁仍然生效(per `None / AwaitingConfirm` 的 phase 规则)。

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

**Per-page layout** (active tab 在 page header 的 badge 反映 phase):

| Page | Header | Body | Top-right 按钮 |
|------|--------|------|---------------|
| `[SETUP]` | `──[ SETUP ]──────────────────────────` | INPUT DIRECTORY + `[ pick directory ]` + SETTINGS list | `[ Start ▸ ]`(dir 未选时 disabled) |
| `[TASK]` | `──[ TASK ]────[● <phase> badge]` | BATCH# + 统计 + TASK PLAN + ProgressPanel | `[ Cancel ✕ ]`(active 时)/ `[ Start ▸ ]`(Done 时 re-arm) |
| `[HISTORY]` | `──[ HISTORY ]───────────────────` | history table(source / outcome / size) | `[ Refresh ⟳ ]` |
| `[LOG]` | `──[ LOG ]────────────────────────` | 完整 log scrollback(全 1000 行) | `[ Clear ]`(清 log buffer) |

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

**Active state 默认值**: app 启动时 active tab = `[SETUP]`,除非有未完成的 batch(没 v1 不存在)。Tab 切换走 reducer(`Msg::UserClickedTab(Page)` → `state.active_page = page`),保持 testable。

**Concrete 组件** (按 page 分组):

| Page | 组件 | 输入 | 渲染什么 |
|------|------|------|----------|
| global | `TopBar` | `active: Page`, `cmd_tx` | 4 tabs + Exit |
| global | `LogFooter` | `state` | 5 行 log |
| `[SETUP]` | `DirPicker` | `state`, `cmd_tx` | native folder dialog + 当前 path + disabled when batch active |
| `[SETUP]` | `SettingsPanel` | `config` | 只读 `[encode]` key/value list |
| `[TASK]` | `BatchStatus` | `state` | BATCH #N + phase badge + 12 files / 10 encode / 2 skipped 列表 |
| `[TASK]` | `ProgressPanel` | `state` | 进度条 + per-file table(Encoding 时) |
| `[TASK]` | `TaskPlanPanel` (NEW,替换 `DryRunPanel`) | `tasks.active.dry_run_records` | post-DryRun 的 file list,per-file outcome 标签(ENCODE / SKIP / FAIL) |
| `[HISTORY]` | `HistoryPanel` | `state.history` | history records 表 |
| `[LOG]` | `LogPanel` (full version) | `state.log` | 全 1000 行 scrollback |

**Removed**:
- `UserClickedAnalyze` / `UserClickedPreview` Msg variants(Start 流程 auto-runs analyze)
- `DryRunPanel`(内容并入 `TaskPlanPanel`)
- `AnalyzePanel`(无 standalone analyze action)
- `AnalyzeProgressInfo` / `state.analyze_progress` / `state.analyze_results`(subsumed)
- 7-button controls bar(替换为 per-page buttons)

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
| Dir picker 锁 during whole batch lifecycle | v1:dir picker 在 `Analyzing` / `AwaitingConfirm` / `Encoding` 期间都被 disable(per §3.8.7.1),防止用户对 in-flight task 换目录 | v1.1 解锁 Encoding 期间的 picker(per spec point 6)——允许选下一个目录排队 |
| Cancel during Analyzing 是 best-effort | lib 的 `Command::Analyze` handler 不轮询 cancel_token,UI 发 `Command::Cancel` 时 lib 不真正中断。`Event::AnalyzeFinished` 仍到达并被 reducer 当正常结果处理 | v1.1 在 analyze handler 里加 cancel_token 轮询 |
| `state.analyze_progress` / `state.analyze_results` 字段保留但未 populate | workflow 改造后 Start 自动调 `Command::DryRun`(不是 `Command::Analyze`),analyze state 字段未 populate。代码里还在(`#[allow(dead_code)]`),变成 dead field——AppState 代码块里保留定义以备 v1.1 / U22 重启用 | v1.1 移除 dead 字段 + 删除对应 `Event::Analyze*` reducer arm |
| `UserClickedLoadHistory` 自动派 on app start | app 启动时 `use_hook` 派一次(per §3.8.7.2 history page);但目前没有显式 init code,history 是空直到用户手动 reload | v1.1 在 `use_hook` 里加 `cmd_tx.send(Command::LoadHistory { limit: Some(100), after: None })` |
| Cross-session task 导航缺失 | `task_history` 是 session 内 cache(进程退出即丢);`state.history` 是 cross-session 但以 per-file EncodingRecord 为单位。**没有** task 维度的 cross-session 视图——v1 看不到昨天跑的 batch 用了哪个 dir / 多少文件 | v1.1 启动时 load Parquet records → group by batch_id → 渲染成 TaskInfo 列表。数据后端已经是 Parquet(`encoding-*.parquet`),不引新 store |
| Settings 只读 | `[SETUP]` 页 settings 只显示 `[encode]` 当前值;不支持 inline edit | v1.2 加 inline edit 模式(`<input type="number">` for crf 等)+ dispatch `Command::SaveConfig` |

**v1 安全保证**(从 v1.1 deferred 提前到 v1 实现,per §3.8.7 product-lens / scope-guardian review 反馈):

| 项 | 实现 |
|----|------|
| UI drain thread panic recovery | `use_hook` 里 `std::thread::spawn` 的 drain loop 用 `std::panic::catch_unwind` 包裹(per §3.9.4);panic → 设 `state.last_error = "drain thread panicked"` + 暴露 `controller_alive: false` 在 v2 才需要, v1 只设 last_error |
| Exit button 先 Cancel | Exit button onclick 分两阶段:active phase → 先 `cmd_tx.send(Command::Cancel)`,等 `Event::BatchCancelled`,再 `window().close()`;None/Done → 直接 `window().close()` |

这些限制不影响协议契约（Command/Event 已端到端测试通过 mock_tests），只是 UI 渲染层不完整。

#### 3.8.8 与 §3.4 CLI 前端的关系

CLI 和 GUI 共享同一份 `rencoder.toml`（§1.2）和同一份 lib 实现：

- **CLI 路径不变**：直接调 `transcode_batch`，写 Parquet，写终端日志。
- **GUI 路径**：发 `Command` → 收 `Event` → reducer → 渲染。
- 两条路径都触发 Parquet 写入；GUI 加载历史 = 读 Parquet（同一文件路径）。

#### 3.8.7.5 Unified log pipeline (UI side)

> **TL;DR** — lib 用 `log` facade 发事件;两个 binary 各自 init
> `tracing-log::LogTracer` + 自己的 tracing subscriber。CLI 写到
> stderr + 文件;UI 写到 `crossbeam_channel::Sender<LogLine>` → 专用
> drain thread → `state.log`,footer / `[LOG]` 页统一显示。

**Why this architecture**: lib 不知道也不该知道是哪个 binary 在消费
log——`log` facade 是标准做法,`tracing-log` 是 bridge。两个 binary
各装各的 subscriber,各自的 sink。Lib 一行代码不改。

**Flow**:

```
         lib (log::info! / log::warn!)
                  │
                  ▼ tracing-log::LogTracer::init() (per-binary)
                  │
                  ▼
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
  ("task skipped: source codec matches target",
  "nvidia-smi unavailable", "processing task N/M" etc.)
  - 这些**没有 file context** —— `LogLine.file_index` 用 `usize::MAX` sentinel
  - 渲染时显示 `(global)` 而不是 `[N]`

`LogSource` enum 区分这两类,方便后续 v1.2 给 lib events 加视觉
区分(颜色 / icon),不影响 log panel 的合并显示。

**Threading model**:

- Lib 的 log 事件从 lib 内部线程发出(actor thread + rayon worker threads)
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
ctrlc = "3.4"
parking_lot = "0.12"
tracing = "0.1"
tracing-subscriber = { version = "0.3", features = ["env-filter"] }
tracing-log = "0.2"
chrono = { version = "0.4", default-features = false, features = ["clock", "std"] }
anyhow = "1"
```

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
| 单元测试 | `parse_fps_value`、`MaxFpsConfig::need_reduce`、`probe_video_metadata`、`FpsStats`、`AnalysisRecord` / Polars 聚合、`collect_files`、`should_skip_encoding` 决策表 |
| 集成测试 | CLI 黑盒：encode 端到端产出 `.mkv`；analyze 默认输出 CSV 到 stdout（一行一文件，列 `path, fps, codec, duration_secs, file_bytes`）；`--format text` 旧汇总；`-o FILE` 写文件 |
| 最大帧率测试 | 25fps 不降帧、30fps 不降帧、29.97fps 不降帧、60fps 降帧 |
| 视频分析测试 | `analyze_records` 产出 per-file records；Polars `group_by(fps).agg(count, sum(duration_secs))` 与手写 `FpsStats::summary` 数值一致；`records_to_csv` 输出可被标准 CSV 解析 |
| 编码器缓存测试 | 验证只执行一次 `ffmpeg -encoders` |
| NVIDIA 测试 | `--nv` + `nvidia-smi` 双重条件验证 |
| 并发安全测试 | `c-probe` + `parallel-analyze` 同时启用，100+ 文件并行探测 |

---

## 7. 开发计划

| Phase | 内容 |
|---|---|
| 1 | 创建两个 crate，设置 workspace |
| 2 | 实现核心库基础功能（错误类型、路径工具、数据结构） |
| 3 | 实现元数据探测 + 帧率解析 |
| 4 | 实现编码器缓存 + 编码器选择（含 `--nv` + `nvidia-smi`） |
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
| 1 | 外部依赖仅 `ffmpeg` + `ffprobe`（`nvidia-smi` 仅 `--nv` 时） | ✅ |
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
| 12 | NVIDIA 检测：`--nv` + `nvidia-smi` 双重条件，不用 `ffmpeg -encoders` 推断 | ✅ |
| 13 | `ProcessOutput::Inherit` 透传，未来 GUI 用 `Piped` | ✅ |
| 14 | 信号处理用 `parking_lot::Mutex`（无中毒） | ✅ |
| 15 | `EncodeError` 加 `#[non_exhaustive]` | ✅ |
| 16 | `FpsStats::new(total_files)` 设置总数 | ✅ |
| 17 | `c-probe` 桥接层无状态、可重入，支持 `rayon` 并发 | ✅ |
| 18 | 视频分析模块支持串行和可选并行 | ✅ |
| 19 | CLI 提供 `encode` 和 `analyze` 两个子命令 | ✅ |
| 20 | `nvidia-smi` 未安装/spawn 失败视为"无 GPU"降级，不作为 `EncodeError` 传播 | ✅ |
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
| 39 | `Command::LoadHistory` 不带 path 字段；controller 用 `config::path::resolve_data_dir()` 自解析（honors `$RENENCODER_DATA_DIR`,v1.1 启用） | ✅ |
| 40 | Partial-output 检测(U21)：output duration < source - 1s → partial → 删 + 重转；probe 失败视作 partial；删除失败 → `Skipped(PartialOutput)` | ✅ |
| 41 | CLI encode summary 增加 `PartialSkip` 独立分类(`PartialOutput` 不计入 `Skipped` 也不计入 `Failed`) | ✅ |
| 42 | Per-frame Progress 事件接线：`transcode_single_with_progress` + controller 把 `FfmpegProgress` 包成 `Event::Progress`；CancelToken 真正能 kill 当前 ffmpeg 子进程 | ✅ |
| 43 | Mock 测试层：`process_one_file` + `run_actor` 抽出允许注入 encode 闭包，验证消息协议契约不需要 ffmpeg；覆盖 event sequence / busy rejection / cancel / LoadHistory 过滤（LoadHistory 测试 v1.1 触发） | ✅ |
| 44 | `run_ffmpeg_with_progress` 签名带 `total_duration_secs: Option<f64>`(ETA 计算输入)；`FfmpegProgress` 5 字段 struct 定义;cancel 阈值 `speed >= 0.001` 才返回 Some(eta) | ✅ |
| 45 | `process_one_file` 抽出 `encode_fn: &mut F` 闭包参数让 mock 测试注入；`ProcessError` enum 区分 Encode vs EventSend(后者表示 UI dropped receiver,batch 应 abort) | ✅ |
| 46 | `load_history(data_dir, limit, after)` 显式接受 data_dir 参数(不读 env var)——让 mock 测试 parallel-safe,env vars 是 process-global 会 race（v1.1 启用） | ✅ |
| 47 | `rencoder_lib::job::EncodingRecord` 重新导出——UI 需要命名 `Event::FileCompleted` / `BatchCompleted` 携带的 record 类型 | ✅ |
| 48 | `rencoder_ui` crate bootstrap(Dioxus desktop 0.6)：reducer + 9 个 unit test;App 在 `use_hook` 里 `std::thread::spawn` 做阻塞 `crossbeam::recv()` 循环,通过 `use_signal_sync` 的 SyncSignal 直接改 Dioxus state(详见 §3.9,不用 `use_coroutine` 因阻塞 recv 会 block executor);v1 三按钮 (Start/Cancel)+ 极简状态显示;Load History 按钮存在但 v1 不显示历史(v1.1 启用 HistoryPanel) | ✅ |
| 49 | Actor 退出显式信号:`Event::ActorExited { reason: ExitReason }` 作为最后一个 event 在 lib drop `evt_tx` 前发出;UI drain thread 收到后下一次 `rx.recv()` 自然返回 `Err` 退出(v1 不维护 `controller_alive`,这是 §3.9.4 v1 限制)。Lifecycle 对称:start = 构造,exit = 显式消息,中间交互 = Command/Event | ✅ |
| 50 | Preview / Dry Run (Tier 2 #6):`Command::DryRun { params, files }` 跑 `encode_prepare` 但**不调 ffmpeg**,emit `Event::DryRunFinished { records }`;`dry_run_batch` lib helper 复用 probe + partial + skip 逻辑。UI `Preview` 按钮 → reducer arm → DryRunPanel 渲染 planned outcomes。`encode.nv` 故意忽略——dry-run 不知道也不关心 GPU 是否启用 | ✅ |
| 51 | Parallel analyze progress (Tier 3 #9):`Command::Analyze { files }` 走 `analyze_records_with_probe_and_progress_parallel` 闭包注入层(不是 c-probe FFI 回调层),emit `Event::AnalyzeStarted { total }` + N × `Event::AnalyzeProgress { done, total, failed, last: Option<AnalyzedFile> }` + `Event::AnalyzeFinished { records, failed_count }`;复合 A+B 显示(进度计数 + per-file log)、failure-only log、AnalyzedFile 新 type(不复用 AnalysisRecord 避免污染 success path)、v1 不响应 `Command::Cancel` 和 SIGINT、v1 走 busy-gate 不与 encode 并发。why 详见 §3.4 "分析进度上报" | ✅ |
| 52 | UI 事件泵架构:UI 在 `use_hook` 里 `std::thread::spawn` 一个专用 OS 线程做阻塞 `crossbeam::Receiver::recv()`,通过 `use_signal_sync` 暴露的 `SyncSignal` 直接 `state.with_mut()` 改 Dioxus state。不用 `use_coroutine`(阻塞 recv() 会 block Dioxus executor,首帧不渲染);不桥接 `futures::mpsc`(冗余);lib API(`start_controller` 返回 crossbeam ControllerHandle)不动。详见 §3.9;bisect 历史见 `docs/debug-journal/event-pump-blocking.md` | ✅ |

---

如有异议或补充，请指出。无异议则进入编码阶段。
