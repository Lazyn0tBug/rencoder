//! Logging initialization.
//!
//! `init` is called once at process start. Tests can use `builder` to
//! obtain a configured `env_logger::Builder` without triggering the
//! global init (which would panic on re-init).

/// Build a configured `env_logger::Builder` with our standard settings:
/// RUST_LOG env var with `info` as default, no timestamp prefix.
pub fn builder() -> env_logger::Builder {
    let mut b = env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"));
    b.format_timestamp(None);
    b
}

/// Initialize the global logger. Panics on re-init (env_logger's
/// documented behavior); main calls this exactly once.
pub fn init() {
    builder().init();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builder_does_not_init_global_state() {
        // Constructing the builder is a pure value operation. Calling
        // .init() would panic on re-init; we deliberately avoid that.
        let _b = builder();
    }

    #[test]
    fn init_can_be_called_once() {
        // Best-effort: this test is the first to call init() in this
        // test binary, so it succeeds. Other tests in the same binary
        // must not call init() again.
        init();
    }
}
