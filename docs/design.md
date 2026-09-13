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
│   ├── log_view.rs       # VecDeque<LogLine> 渲染
│   └── history_view.rs   # 历史记录列表
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
    pub extra_args: Vec<String>,
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
    pub input: PathBuf,
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
```

CLI 输出双轨：
- **CSV to stdout**（默认）：每行一个文件，列 `path, fps, codec, duration_secs, file_bytes`。
- **Summary to stderr**：单行汇总（`Analyzed X files (Y parsed, Z failed)`）。
- `--format text`：人类可读的汇总（旧 `FpsStats::summary()` 行为）。
- `-o FILE`：CSV 写到文件而不是 stdout。

聚合在 Polars 中完成：`group_by(fps).agg([count, sum(duration_secs)])`。失败探测的行有空 `fps`/`codec`/`duration_secs`，保留 `path`/`file_bytes` 便于下游 `WHERE fps IS NULL` 筛选。

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
- **dist 镜像运行时布局**：`scripts/dist.sh` 把 target 产物 + LICENSE +
  README + config 模板拷进 `dist/<app>-<triple>/`，形成 ready-to-run 单目录应用。
  解压后直接 `./rencoder ...`，无需 mkdir 或额外配置。

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
    Cancel,
    /// 从 `<app_dir>/data/encoding-*.parquet` 回读历史。
    /// - `limit`：返回最多 N 条最新记录；`None` 表示全部
    /// - `after`：仅返回 `started_at >= after` 的记录；`None` 表示不限时间
    LoadHistory {
        limit: Option<usize>,
        after: Option<chrono::DateTime<chrono::Utc>>,
    },
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

Topic 设计意图：dispatch 由 UI 端 reducer 用 `match` 完成，路由成本零；不需要单独的 `topic: Topic` 字段或多 channel。

#### 3.8.4 Controller

```rust
pub struct ControllerHandle {
    cmd_tx: UnboundedSender<Command>,
    evt_rx: UnboundedReceiver<Event>,
}

pub fn start_controller() -> ControllerHandle;
```

UI 启动时调一次，拿到 `(cmd_tx, evt_rx)`，然后再不管 lib 内部。

- `cmd_tx`：可克隆供多个 UI 组件使用（开始按钮、取消按钮、历史按钮）。
- `evt_rx`：由顶层 `use_coroutine` 独占消费；不广播、不多订阅者——保证单消费者的因果序。
- Channel 策略：`UnboundedSender`（单 batch 下每秒最多 ~50 个事件，UI 永远消费得过来；如未来有背压风险再换 `bounded + DropOldest`）。

#### 3.8.5 状态与持久化的双轨

| 通道 | 形式 | 生命周期 |
|------|------|----------|
| 实时 | mpsc 事件流 | 临时，重启即丢 |
| 持久 | `<app_dir>/data/encoding-*.parquet` | 永久 |

- 批结束：lib 写 Parquet + 发 `Event::BatchCompleted`。
- UI 启动：发 `Command::LoadHistory` → lib 读 Parquet → 发 `Event::HistoryLoaded` 填充历史视图。
- 这是 event sourcing 的两面：state 是 event log 的 fold，event log 是 append-only 真相源。

#### 3.8.6 lib 内部实现要点

**不修改**（CLI 路径零回归）：

- `transcode_batch` 签名
- `EncodingRecord` / `EncodingOutcome` / `ErrorContext`（见 §3.2）
- `TaskManager` / `PartialOutputHandle` / signal handler（CLI 端）
- `EncodeError`（保持 `thiserror` 原样，不加 `Serialize`）
- `ProcessOutput` 枚举（见 §3.2）
- `AppConfig` 加载/校验
- CLI 路径的 ffmpeg 编排（含 stderr 收集 + partial output 清理）——保留 `cmd.output()` 同步路径，仅在 lib 内部为 GUI 加 spawn-based sibling

**新增**：

| 位置 | 类型 / 函数 | 职责 |
|------|-------------|------|
| `pipeline/cancel.rs`（新） | `CancelToken` | `Arc<AtomicBool>` + `cancel()` / `is_cancelled()` |
| `pipeline/batch.rs` | `run_ffmpeg_with_progress(argv, cancel, on_progress)` | `cmd.spawn()` 替代 `cmd.output()`，支持 cancel + progress 回调；CLI 路径的同步 ffmpeg 编排保留不动 |
| `data/store.rs` | `read_records(path) -> Vec<EncodingRecord>` | 单 Parquet 文件回读；多文件 glob + limit/after 过滤在 controller |
| `config/path.rs` | `resolve_data_dir() -> PathBuf` | controller 在响应 `Command::LoadHistory` 时调此拿到数据目录(`$RENENCODER_DATA_DIR` > `<app_dir>/data`);**不**走 `Command::LoadHistory` 的 payload,UI 不需要知道路径 |
| `job.rs`（新） | `start_controller() -> ControllerHandle` | 编排：后台线程 + mpsc + 现有 `transcode_batch`（**复用 CLI 路径的纯函数，不 fork**）+ 新 `run_ffmpeg_with_progress`；job.rs 是 thin glue，把现有 pieces 装成 actor |
| `job.rs`（新） | `pub enum Command` / `pub enum Event` | 消息协议（见 §3.8.3） |

**CancelToken 共享**：UI `Command::Cancel` 与 SIGINT handler（lib 内部安装，可选）共享同一个 `Arc<AtomicBool>`，等价语义——UI 按钮和 Ctrl-C 都把同一个 token 置位。

**log 过滤/节流**：lib 透传所有 ffmpeg stderr 行（不解析、不丢弃），上限/截断由 UI reducer 在 state 层做（`VecDeque` 限 1000 行）。

#### 3.8.7 UI 端模式："everything is msg"

Dioxus 端用 reducer 模式（Elm 架构）。UI 端的状态类型：

```rust
/// 顶层 UI state。整个 app 一个,所有视图组件读它。
struct AppState {
    /// 当前活跃批的元信息;`None` = 没有批在跑
    batch: Option<BatchInfo>,
    /// per-file 进度,key 是 file_index(§3.8.3 `Progress.file_index`)
    progress: HashMap<usize, ProgressInfo>,
    /// stderr log ring buffer(§3.8.6:上限 1000 行)
    log: VecDeque<LogLine>,
    /// 历史记录(由 `Command::LoadHistory` 填充)
    history: Vec<EncodingRecord>,
}

/// 当前批的元信息;由 `Event::BatchStarted` 写入、`Event::BatchCompleted`/`BatchCancelled` 关闭
struct BatchInfo {
    id: u64,
    total_files: usize,
    /// `cancelled: true` 后 reducer 忽略后续的 `Event::Progress` / `FileCompleted`
    /// 直到收到 `BatchCancelled` 或新 `BatchStarted`
    cancelled: bool,
}

/// 单文件进度;由 `Event::Progress` 更新
struct ProgressInfo {
    frame: u64,
    fps: f64,
    speed: f64,
    time_secs: f64,
    eta_secs: Option<f64>,
}

/// 单行 stderr;由 `Event::LogLine` 推送
struct LogLine {
    file_index: usize,
    line: String,
}

enum Msg {
    UserClickedStart,
    UserClickedCancel,
    UserOpenedHistory,
    Lib(Event),
}

fn reduce(state: &mut AppState, msg: Msg, cmd_tx: &UnboundedSender<Command>) {
    match msg {
        Msg::UserClickedStart => drop(cmd_tx.send(Command::StartBatch { /* ... */ })),
        Msg::UserClickedCancel => drop(cmd_tx.send(Command::Cancel)),
        Msg::UserOpenedHistory => drop(cmd_tx.send(Command::LoadHistory { limit: Some(100), after: None })),
        Msg::Lib(Event::Progress(p)) => { state.progress.insert(p.file_index, p); }
        Msg::Lib(Event::LogLine(l)) => {
            state.log.push_back(l);
            while state.log.len() > 1000 { state.log.pop_front(); }
        }
        Msg::Lib(Event::HistoryLoaded { records }) => { state.history = records; }
        // ...
    }
}
```

- 所有用户输入转成 `Msg` → reducer 决定是否反发 `Command`。
- 所有 lib 事件包成 `Msg::Lib(Event)` → reducer 更新 `state`。
- 组件只读 `state`，不直接调 lib。
- 测试：构造 `Vec<Msg>` 灌进 reducer 断言 `state`——不需 Dioxus runtime。

#### 3.8.8 与 §3.4 CLI 前端的关系

CLI 和 GUI 共享同一份 `rencoder.toml`（§1.2）和同一份 lib 实现：

- **CLI 路径不变**：直接调 `transcode_batch`，写 Parquet，写终端日志。
- **GUI 路径**：发 `Command` → 收 `Event` → reducer → 渲染。
- 两条路径都触发 Parquet 写入；GUI 加载历史 = 读 Parquet（同一文件路径）。

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
| 27 | 双轨 state：mpsc 实时事件 + Parquet 持久化；`Command::LoadHistory` 触发回读 | ✅ |
| 28 | Channel 策略：`UnboundedSender`（单 batch 下背压风险极低） | ✅ |
| 29 | lib 内部 `CancelToken`（`Arc<AtomicBool>`）由 UI `Command::Cancel` 与 SIGINT handler 共享 | ✅ |
| 30 | Log 行在 lib 透传；UI reducer 层设上限（`VecDeque` 1000 行） | ✅ |
| 31 | `Command::Cancel` 中止整批（包括当前文件），与 SIGINT 等价；不暴露"仅跳过当前文件" | ✅ |
| 32 | `Command::LoadHistory { limit: Option<usize>, after: Option<DateTime<Utc>> }` 含 payload，避免后续加字段成 breaking change | ✅ |
| 33 | `pub enum Command` 和 `pub enum Event` 加 `#[non_exhaustive]`，新增 variant 不破坏 exhaustive match | ✅ |
| 34 | `ErrorCategory` 在 §3.8.3 内联列出 7 个 variant（§3.2 类型定义同步） | ✅ |
| 35 | §3.8 GUI 范围 Unix-only（matches §1.3）；wry/Dioxus 非 Unix 支持 deferred | ✅ |
| 36 | `ErrorCategory` 加 `#[non_exhaustive]`（U21，对齐 `Command` / `Event`）；`Event::Error` 故意只带 `(category, message)` 不带 stderr_tail | ✅ |
| 37 | `ErrorContext` 加 `#[non_exhaustive]`（U21，对齐 `ErrorCategory`）；外部 crate 不可构造,字段读取仍可 | ✅ |
| 38 | §3.8.7 定义 `BatchInfo` / `ProgressInfo` / `LogLine` 三个 UI state struct,让 reducer 可独立 unit-test | ✅ |
| 39 | `Command::LoadHistory` 不带 path 字段；controller 用 `config::path::resolve_data_dir()` 自解析（honors `$RENENCODER_DATA_DIR`） | ✅ |
| 40 | Partial-output 检测(U21)：output duration < source - 1s → partial → 删 + 重转；probe 失败视作 partial；删除失败 → `Skipped(PartialOutput)` | ✅ |
| 41 | CLI encode summary 增加 `PartialSkip` 独立分类(`PartialOutput` 不计入 `Skipped` 也不计入 `Failed`) | ✅ |

---

如有异议或补充，请指出。无异议则进入编码阶段。
