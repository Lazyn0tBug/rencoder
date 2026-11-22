//! 路径：一次解析，各处不再现算（设计文档 §3.6）。
//!
//! **放置原则**：每个文件都放在**离它被访问、离它起作用最近的地方**。这条原则落到
//! 两个目录：
//!
//! - **应用目录**（`app_dir`）：应用自己所在的地方（binary 的父目录）。配置、日志、
//!   工作记录、机器能力记录都在这儿——它们是应用级的，跟这一趟操作无关。
//! - **操作目录**（`op_dir`）：**本次操作作用的目录**。取消请求与运行能力检查报告
//!   在这儿——它们描述的正是"这一趟、这个目录能不能干"。
//!
//! 优先级（§3.10.5 的分层）：**入口显式值**（env / CLI）> **文件显式值**
//! （`[paths]` 段）> **内建默认值**。
//!
//! 配置文件自身的位置是入口显式值（`--config` / `$RENENCODER_CONFIG`）——配置不能
//! 配置自己的位置。

use std::path::{Path, PathBuf};

use super::app::{AppConfig, PathsConfig};

/// 应用目录：binary 所在目录。
///
/// `current_exe().parent()`——跟应用一起走的那份状态就住这儿。
pub fn app_dir() -> Option<PathBuf> {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(Path::to_path_buf))
}

/// 配置文件的位置：显式路径 > `$RENENCODER_CONFIG` > `<应用目录>/rencoder.toml`。
///
/// 解析不出来（`current_exe()` 失败）返回 `None`——调用方给明确错误，不偷偷退回
/// CWD。
pub fn config_file(explicit: Option<&Path>) -> Option<PathBuf> {
    if let Some(p) = explicit {
        return Some(p.to_path_buf());
    }
    if let Some(raw) = std::env::var_os("RENENCODER_CONFIG") {
        if !raw.is_empty() {
            return Some(PathBuf::from(raw));
        }
    }
    app_dir().map(|d| d.join("rencoder.toml"))
}

/// 取消请求的文件名（在操作目录里）。
pub const CANCEL_FILE_NAME: &str = "cancel";

/// 应用自己写进操作目录的文件名——输入采集器按名字跳过它们。
///
/// 它们是这一趟的状态，不是用户的视频：被当成输入去转码就是错的。
pub const APP_OWNED_FILE_NAMES: [&str; 3] = [
    CANCEL_FILE_NAME,
    "capabilities.json",
    crate::guard::OWNER_FILE_NAME,
];

/// 一次运行用到的全部路径。
///
/// 解析一次，之后各处拿着它用——路径不再散落成六七个 `resolve_*` ✅。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Paths {
    /// 应用目录：应用自己的东西。
    pub app_dir: PathBuf,
    /// 操作目录：这一趟的东西。
    pub op_dir: PathBuf,
    /// 配置文件。
    pub config_file: PathBuf,
    /// 日志目录。
    pub log_dir: PathBuf,
    /// 工作记录目录。
    pub records_dir: PathBuf,
    /// 机器能力记录文件。
    pub capability_record: PathBuf,
    /// 取消请求文件。
    pub cancel_file: PathBuf,
    /// 运行能力检查报告。
    pub capability_report: PathBuf,
    /// 配置里 `[paths]` 的原值——换操作目录时用它重算运行级路径。
    overrides: PathsConfig,
}

impl Paths {
    /// 解析全部路径。
    ///
    /// - `config_file` —— 已经解析好的配置文件位置（配置不能配置自己）。
    /// - `app_dir` —— 应用目录。
    /// - `op_dir` —— 操作目录（没有目标的操作用应用目录）。
    /// - `cfg` —— 已加载的配置；`[paths]` 段在这里面。
    pub fn resolve(
        config_file: PathBuf,
        app_dir: PathBuf,
        op_dir: PathBuf,
        cfg: &AppConfig,
    ) -> Self {
        let overrides = cfg.paths.clone().unwrap_or_default();
        Self {
            log_dir: env_or(
                "RENENCODER_LOG_DIR",
                overrides.log_dir.as_ref(),
                &app_dir,
                "log",
            ),
            records_dir: env_or(
                "RENENCODER_DATA_DIR",
                overrides.records_dir.as_ref(),
                &app_dir,
                "data",
            ),
            capability_record: env_or(
                "RENENCODER_NVENC_CACHE",
                overrides.capability_record.as_ref(),
                &app_dir,
                "nvenc-capability.json",
            ),
            cancel_file: env_or(
                "RENENCODER_CANCEL_FILE",
                overrides.cancel_file.as_ref(),
                &op_dir,
                "cancel",
            ),
            capability_report: env_or(
                "RENENCODER_CAPABILITY_REPORT",
                overrides.capability_report.as_ref(),
                &op_dir,
                "capabilities.json",
            ),
            app_dir,
            op_dir,
            config_file,
            overrides,
        }
    }

    /// 换一个操作目录——应用级的路径不动，运行级的跟着走。
    ///
    /// GUI 换工作目录时用：那一趟的状态落在新的工作目录里（§3.10.7 ✅）。
    pub fn with_op_dir(&self, op_dir: &Path) -> Self {
        let mut rebased = self.clone();
        rebased.cancel_file = env_or(
            "RENENCODER_CANCEL_FILE",
            self.overrides.cancel_file.as_ref(),
            op_dir,
            "cancel",
        );
        rebased.capability_report = env_or(
            "RENENCODER_CAPABILITY_REPORT",
            self.overrides.capability_report.as_ref(),
            op_dir,
            "capabilities.json",
        );
        rebased.op_dir = op_dir.to_path_buf();
        rebased
    }
}

/// env > 配置 > 默认值。
///
/// 配置里的相对路径相对 `base`（各自的基准目录）解析，绝对路径原样使用。
fn env_or(env_key: &str, configured: Option<&PathBuf>, base: &Path, default_name: &str) -> PathBuf {
    if let Some(raw) = std::env::var_os(env_key) {
        if !raw.is_empty() {
            return PathBuf::from(raw);
        }
    }
    match configured {
        Some(p) if p.is_absolute() => p.clone(),
        Some(p) => base.join(p),
        None => base.join(default_name),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg_with(paths: PathsConfig) -> AppConfig {
        AppConfig {
            paths: Some(paths),
            ..AppConfig::default()
        }
    }

    #[test]
    fn defaults_put_state_files_in_the_operation_directory() {
        let paths = Paths::resolve(
            PathBuf::from("/app/rencoder.toml"),
            PathBuf::from("/app"),
            PathBuf::from("/videos"),
            &AppConfig::default(),
        );
        assert_eq!(paths.log_dir, PathBuf::from("/app/log"));
        assert_eq!(paths.records_dir, PathBuf::from("/app/data"));
        assert_eq!(
            paths.capability_record,
            PathBuf::from("/app/nvenc-capability.json")
        );
        assert_eq!(paths.cancel_file, PathBuf::from("/videos/cancel"));
        assert_eq!(
            paths.capability_report,
            PathBuf::from("/videos/capabilities.json")
        );
    }

    #[test]
    fn configured_paths_win_over_the_defaults() {
        let paths = Paths::resolve(
            PathBuf::from("/app/rencoder.toml"),
            PathBuf::from("/app"),
            PathBuf::from("/videos"),
            &cfg_with(PathsConfig {
                log_dir: Some(PathBuf::from("/tmp/logs")),
                records_dir: Some(PathBuf::from("records")),
                capability_record: None,
                cancel_file: Some(PathBuf::from("stop")),
                capability_report: None,
            }),
        );
        assert_eq!(paths.log_dir, PathBuf::from("/tmp/logs"));
        // 相对路径相对各自的基准目录解析。
        assert_eq!(paths.records_dir, PathBuf::from("/app/records"));
        assert_eq!(paths.cancel_file, PathBuf::from("/videos/stop"));
    }

    #[test]
    fn switching_the_operation_directory_moves_only_the_run_level_paths() {
        let paths = Paths::resolve(
            PathBuf::from("/app/rencoder.toml"),
            PathBuf::from("/app"),
            PathBuf::from("/videos"),
            &AppConfig::default(),
        );
        let moved = paths.with_op_dir(Path::new("/movies"));
        assert_eq!(moved.cancel_file, PathBuf::from("/movies/cancel"));
        assert_eq!(
            moved.capability_report,
            PathBuf::from("/movies/capabilities.json")
        );
        // 应用级的路径不动。
        assert_eq!(moved.log_dir, PathBuf::from("/app/log"));
        assert_eq!(moved.records_dir, PathBuf::from("/app/data"));
        assert_eq!(
            moved.capability_record,
            PathBuf::from("/app/nvenc-capability.json")
        );
    }
}
