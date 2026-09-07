//! Build script for the optional c-probe FFI bridge.
//!
//! When the `c-probe` feature is enabled, this builds `src/ffi/video_probe.c`
//! against the system libavformat (located via pkg-config) and links the
//! resulting static library into the Rust crate. The C bridge uses
//! libavformat's stateless API: each call creates its own
//! `AVFormatContext`, so concurrent calls from rayon are safe (R17).
//!
//! Note: the `pkg-config` crate's `.includes()` integration with `cc-rs`
//! was unreliable here (the include paths weren't always reaching clang),
//! so we invoke `pkg-config` directly and parse the flags ourselves.

fn main() {
    #[cfg(feature = "c-probe")]
    {
        let cflags = pkg_config_flags("--cflags", "libavformat");
        let libflags = pkg_config_flags("--libs", "libavformat");

        let mut build = cc::Build::new();
        build.file("src/ffi/video_probe.c");

        for flag in cflags.split_whitespace() {
            if let Some(path) = flag.strip_prefix("-I") {
                build.include(path);
            }
            // Other flags (-D, etc.) ignored — we don't need them.
        }

        build.compile("rencoder_probe");

        for flag in libflags.split_whitespace() {
            if let Some(lib) = flag.strip_prefix("-l") {
                println!("cargo:rustc-link-lib={}", lib);
            } else if let Some(dir) = flag.strip_prefix("-L") {
                println!("cargo:rustc-link-search=native={}", dir);
            }
        }
        println!("cargo:rerun-if-changed=src/ffi/video_probe.c");
    }
}

#[cfg(feature = "c-probe")]
fn pkg_config_flags(mode: &str, package: &str) -> String {
    let output = std::process::Command::new("pkg-config")
        .args([mode, package])
        .output()
        .unwrap_or_else(|e| panic!("failed to invoke pkg-config: {e}"));
    if !output.status.success() {
        panic!(
            "pkg-config {} {} failed: {}",
            mode,
            package,
            String::from_utf8_lossy(&output.stderr)
        );
    }
    String::from_utf8_lossy(&output.stdout).into_owned()
}
