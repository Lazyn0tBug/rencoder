//! The logging setup both frontends share (design doc §2.3 / §3.10.5).
//!
//! Two things live here, and only two:
//!
//! - **the level vocabulary** — [`level_filter`] turns the config's
//!   [`LogLevel`] into a subscriber filter. The vocabulary is the
//!   lib's: a typo in `rencoder.toml` fails the parse instead of
//!   silently turning the logs off.
//! - **the rotating file sink** — [`file_sink::FileSink`] writes
//!   `<app_dir>/log/rencoder.<date>.<NN>.log`. Both frontends use
//!   it, so it lives here (one implementation, two callers).
//!
//! What the frontends keep is the *sink set*: the CLI writes to
//! stderr + the file, the GUI writes to its log pane + the file
//! (§3.8.7.5). The **level** is not theirs to choose — it comes from
//! the config file, so a release build can be made as talkative as
//! a debug build by editing one line of `rencoder.toml`.

pub mod file_sink;

use tracing_subscriber::EnvFilter;

use crate::config::app::{AppConfig, LogLevel};

/// Default log-file size before it rolls over to the next one (10 MB).
pub const DEFAULT_MAX_BYTES: u64 = 10 * 1024 * 1024;

/// Default log-file retention (days). `0` would mean "keep forever";
/// the built-in default keeps a week.
pub const DEFAULT_MAX_DAYS: u32 = 7;

/// The logging settings this run uses.
///
/// Resolved **once**, from the config file -- the same values for
/// both frontends (design doc §3.10.5).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LogSettings {
    /// How much the app says.
    pub level: LogLevel,
    /// When the log file rolls over to the next one (bytes).
    pub max_bytes: u64,
    /// How long log files are kept (days).
    pub max_days: u32,
}

impl LogSettings {
    /// Resolve the settings from a loaded config.
    ///
    /// A missing `[log]` section is not an error -- it means the
    /// built-in defaults.
    pub fn from_config(config: &AppConfig) -> Self {
        let log = config.log.as_ref();
        Self {
            level: log.and_then(|l| l.level).unwrap_or(LogLevel::Info),
            max_bytes: log.and_then(|l| l.max_bytes).unwrap_or(DEFAULT_MAX_BYTES),
            max_days: log.and_then(|l| l.max_days).unwrap_or(DEFAULT_MAX_DAYS),
        }
    }
}

/// Build the subscriber filter for a configured level.
///
/// The level is the whole filter: one verbosity for the whole app.
/// `EnvFilter` still understands the directive syntax, so the
/// config value is exactly what the filter sees.
pub fn level_filter(level: LogLevel) -> EnvFilter {
    EnvFilter::new(level.as_directive())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::app::LogConfig;
    use tracing_subscriber::filter::LevelFilter;

    #[test]
    fn every_level_becomes_a_filter_with_that_maximum() {
        // The filter must let the level's own events through -- a
        // filter that swallows the level it was built from would
        // make "debug" mean "silent".
        for (level, expected) in [
            (LogLevel::Error, LevelFilter::ERROR),
            (LogLevel::Warn, LevelFilter::WARN),
            (LogLevel::Info, LevelFilter::INFO),
            (LogLevel::Debug, LevelFilter::DEBUG),
            (LogLevel::Trace, LevelFilter::TRACE),
        ] {
            assert_eq!(
                level_filter(level).max_level_hint(),
                Some(expected),
                "{level:?} must keep its own verbosity"
            );
        }
    }

    #[test]
    fn off_silences_everything() {
        assert_eq!(
            level_filter(LogLevel::Off).max_level_hint(),
            Some(LevelFilter::OFF)
        );
    }

    #[test]
    fn a_missing_log_section_means_the_built_in_defaults() {
        let settings = LogSettings::from_config(&AppConfig::default());
        assert_eq!(settings.level, LogLevel::Info);
        assert_eq!(settings.max_bytes, DEFAULT_MAX_BYTES);
        assert_eq!(settings.max_days, DEFAULT_MAX_DAYS);
    }

    #[test]
    fn the_config_overrides_every_default() {
        let config = AppConfig {
            log: Some(LogConfig {
                level: Some(LogLevel::Debug),
                max_bytes: Some(1024),
                max_days: Some(0),
            }),
            ..AppConfig::default()
        };
        let settings = LogSettings::from_config(&config);
        assert_eq!(settings.level, LogLevel::Debug);
        assert_eq!(settings.max_bytes, 1024);
        assert_eq!(settings.max_days, 0);
    }
}
