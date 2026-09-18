pub mod claude;
pub mod codex;
#[cfg(test)]
pub mod fake;

use crate::model::Provider;

/// All built-in providers, in display priority order. Claude stays first, so its sources keep
/// priority in the default-source ordering.
pub fn all() -> Vec<Box<dyn Provider>> {
    vec![
        Box::new(claude::ClaudeProvider),
        Box::new(codex::CodexProvider::default()),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_provider_is_registered_once() {
        let ids: Vec<&str> = all().iter().map(|p| p.id()).collect();
        assert_eq!(ids, vec!["claude", "codex"]);
    }
}
