pub mod claude;
pub mod codex;
#[cfg(test)]
pub mod fake;

use crate::model::Provider;

/// All built-in providers, in display priority order.
pub fn all() -> Vec<Box<dyn Provider>> {
    vec![Box::new(claude::ClaudeProvider)]
}
