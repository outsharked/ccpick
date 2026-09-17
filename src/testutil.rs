//! Test-only helpers.
use std::path::PathBuf;

/// The repository root while tests run. Prefers the runtime `CARGO_MANIFEST_DIR` so Windows
/// test binaries started from WSL (with `WSLENV=CARGO_MANIFEST_DIR/p`) get a Windows path.
pub fn manifest_dir() -> PathBuf {
    std::env::var_os("CARGO_MANIFEST_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")))
}
