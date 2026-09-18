//! A local web portal over the same catalog the TUI uses. Agent-neutral: nothing here may
//! know Claude file formats, ccs, or CLAUDE_CONFIG_DIR.
pub mod json;
pub mod route;
pub mod state;

/// The page, embedded so the binary is self-contained.
pub const PAGE: &str = include_str!("assets/index.html");
