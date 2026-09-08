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
| 日志（文件 sink） | `./log/rencoder.YYYY-MM-DD.NN.log` | 日滚 + 10MB 大小滚（NN 自增），7 天保留（`max_age_days=0` 不限）；自定义 `MakeWriter` 实现 |
| 日志（span） | `#[instrument]` + `FmtSpan::CLOSE` | 关键函数（`encode_cmd` / `analyze_cmd`）记录 span 上下文，便于排障 |
| 配置 | `serde_json` | 替代 `jq` |
| 分析聚合 | `polars`（DataFrame `group_by` + `agg`） | 复用 Polars 生态；不再手写 BTreeMap 遍历 |
| 分析输出（默认） | per-file CSV 到 stdout + 一行 summary 到 stderr | 原始数据可管道到 DuckDB/Polars 二次分析；摘要与数据流分离 |
| 转码工作记录 | `EncodingRecord`（4 维度：source / process / result / status） | 工作分析（吞吐、压缩率、失败率），不是视频分析 |
| 工作记录存储 | `./logs/encoding-YYYY-MM-DDTHH-MM-SSZ.parquet`（按运行分文件） | 每跑一次产出一个自包含文件，多批分析用 `read_parquet(['./logs/*.parquet'])` glob |
| 工作记录序列化 | 单 DataFrame 中转（`records_to_dataframe`）+ 三种 writer（CSV / JSONL / Parquet） | schema 在 `records_to_dataframe` 一处定义；writer 都是 thin wrapper |
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
- **Parquet（`-o file.parquet`）**：列式压缩格式，schema 内嵌，Snappy 压缩；原生 DuckDB / Polars 输入。多批分析：`SELECT * FROM read_parquet('./logs/*.parquet')`。
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

**每跑一次产出一个文件**（`./logs/encoding-<UTC-timestamp>Z.parquet`）：

- ISO 8601 UTC（`Z` 后缀避免时区歧义），`:` 替换为 `-`（macOS / Windows 文件系统兼容）
- 时间戳文件名可字典序排序 = 时间顺序，`ls -t | head -1` 拿到最近一次
- 多批分析用 glob：`read_parquet(['./logs/*.parquet'])`
- 进程 ID 后缀（`a3f`）避免同一秒内并发冲突

> 注：`nvidia-smi` 的 `io::Error`（找不到可执行文件）和"执行成功但无 GPU"都归为同一降级路径，`init_encoder_cache` 不应因此返回 `EncodeError`。

不使用 `nvml-wrapper` crate。`nvidia-smi` 是唯一的外部检测工具，且仅在 `--nv` 时调用。

---

## 3. 架构设计

### 3.1 模块划分

#### 核心库（`rencoder_lib`）

```
rencoder_lib/
├── lib.rs                # 库入口
├── error.rs              # EncodeError（#[non_exhaustive]）
├── config.rs             # 配置结构
├── types.rs              # 公共数据结构
├── path_util.rs          # 路径规范化、输出路径生成
├── collect.rs            # 文件收集（walkdir，不跟随符号链接）
├── ffmpeg/
│   ├── mod.rs
│   ├── probe.rs          # 元数据探测（ffprobe 命令行，默认）
│   ├── encoder.rs        # 编码器选择
│   ├── encoder_cache.rs  # 编码器缓存 + NVIDIA 检测
│   └── command.rs        # FFmpeg 命令构建（含 MAX_FPS 参数映射）
├── fps/
│   ├── mod.rs
│   ├── parse.rs          # 帧率原始值解析
│   └── limit.rs          # MaxFpsConfig
├── analyzer/
│   ├── mod.rs
│   └── fps_stats.rs      # 分析器：probe → AnalysisRecord → Polars 聚合 → FpsStats / CSV
├── processor/
│   ├── mod.rs
│   └── batch.rs          # 批量处理（失败不中断）
└── ffi/                  # 可选：C 桥接层（c-probe 特性）
    ├── mod.rs
    └── video_probe.c
```

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
    /// 通过管道捕获（GUI 使用）
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

// ===== 帧率统计结果（视频分析模块） =====
#[derive(Clone, Debug, Default)]
pub struct FpsStats {
    pub groups: std::collections::BTreeMap<u32, Vec<PathBuf>>,
    pub total_files: usize,
    pub parsed_files: usize,
    pub failed_files: usize,
}

impl FpsStats {
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
/// 内部流程（对齐 encode.zsh 的 encode_single_file）：
///   1. 检查输出是否已存在
///   2. probe_video_metadata（一次 ffprobe）
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

/// Aggregate records into `FpsStats`. Internally backed by a Polars
/// DataFrame; the public `FpsStats` is the rendered view.
pub fn analyze_fps(files: &[PathBuf]) -> Result<FpsStats, EncodeError>;

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

```rust
/// 高性能元数据探测（直接调用 libavformat）
///
/// # 线程安全
/// 本函数无状态、可重入。每次调用创建独立的 AVFormatContext，
/// 不依赖任何全局/静态变量。可安全地在 rayon 中并发调用。
#[cfg(feature = "c-probe")]
pub fn probe_video_metadata_fast(input: &Path) -> Result<VideoMetadata, EncodeError>;

/// 自动选择探测方式，统一返回 Result
pub fn probe_video_metadata_auto(input: &Path) -> Result<VideoMetadata, EncodeError> {
    #[cfg(feature = "c-probe")]
    { return probe_video_metadata_fast(input); }

    #[cfg(not(feature = "c-probe"))]
    { return probe_video_metadata(input); }
}
```

---

### 3.4 CLI 前端职责

#### 转码模式参数

```
rencoder encode [选项] [--] [额外FFmpeg参数]

选项：
  -c, --format FORMAT     目标格式: x264|hevc|vp9|av1 (默认: av1)
  -b, --bitrate VALUE     码率(Kbps)或CRF(仅AV1)
  -f, --file FILE         指定单个文件
  -d, --dir DIR           指定目录（递归）
  -e, --ext EXT           扩展名过滤（可多次）
  -i, --interactive       启动交互模式
  --nv, --enable-nv       启用 NVIDIA 硬件加速
  --                      之后的所有参数作为FFmpeg原生参数透传
```

#### 分析模式参数

```
rencoder analyze [选项]

选项：
  -d, --dir DIR           指定目录（递归），默认当前目录
  -f, --file FILE         指定单个文件
  -e, --ext EXT           扩展名过滤（可多次）
  -p, --parallel          启用并行分析
  -v, --verbose           生成详细统计文件
  -o, --output FILE       详细输出文件路径（默认 fps_stats.txt）
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

#### 信号处理

```rust
use parking_lot::Mutex;
use std::sync::Arc;

let current_output: Arc<Mutex<Option<PathBuf>>> = Arc::new(Mutex::new(None));
let signal_output = current_output.clone();

ctrlc::set_handler(move || {
    // parking_lot::Mutex 无中毒语义
    if let Some(path) = signal_output.lock().take() {
        let _ = std::fs::remove_file(&path);
        eprintln!("⚠️  中断，已删除不完整文件：{}", path.display());
    }
    std::process::exit(1);
}).unwrap();
```

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

## 4. 依赖关系

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
ctrlc = "3"
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
| 17 | （可选）GUI：`ProcessOutput::Piped` + 控制台区域 |

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

---

如有异议或补充，请指出。无异议则进入编码阶段。
