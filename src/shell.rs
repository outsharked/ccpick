//! A LaunchPlan as a command the user can paste into a shell in the session's environment.
use crate::env::Env;
use crate::model::LaunchPlan;

pub fn resume_command(plan: &LaunchPlan, env: &Env) -> String {
    if env.is_windows() {
        powershell(plan)
    } else {
        posix(plan)
    }
}

fn is_plain(s: &str, extra: &str) -> bool {
    !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || extra.contains(c))
}

/// Single-quotes unless the string only has characters no POSIX shell treats specially.
///
/// `=` is excluded even though POSIX itself doesn't treat it specially, because zsh's "magic
/// equals" expansion rewrites a leading-`=` word as the path to that command on `$PATH` when
/// pasted at an interactive prompt.
pub fn posix_quote(s: &str) -> String {
    if is_plain(s, "_./:@%+-") {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', r"'\''"))
    }
}

/// Single-quotes unless the string only has characters PowerShell treats literally.
pub fn powershell_quote(s: &str) -> String {
    if is_plain(s, r"_.:\/-") {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', "''"))
    }
}

fn posix(plan: &LaunchPlan) -> String {
    let mut parts: Vec<String> = Vec::new();
    if !plan.env_remove.is_empty() {
        parts.push("env".into());
        for key in &plan.env_remove {
            parts.push(format!("-u {key}"));
        }
    }
    for (key, value) in &plan.env_set {
        parts.push(format!("{key}={}", posix_quote_always(value)));
    }
    parts.extend(plan.argv.iter().map(|a| posix_quote(a)));
    let command = parts.join(" ");
    if plan.cwd.as_os_str().is_empty() {
        command
    } else {
        format!(
            "cd {} && {command}",
            posix_quote_always(&plan.cwd.to_string_lossy())
        )
    }
}

fn posix_quote_always(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

fn powershell_quote_always(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

fn powershell(plan: &LaunchPlan) -> String {
    let mut statements: Vec<String> = Vec::new();
    if !plan.cwd.as_os_str().is_empty() {
        statements.push(format!(
            "Set-Location {}",
            powershell_quote_always(&plan.cwd.to_string_lossy())
        ));
    }
    for key in &plan.env_remove {
        statements.push(format!("Remove-Item Env:{key} -ErrorAction Ignore"));
    }
    for (key, value) in &plan.env_set {
        statements.push(format!("$env:{key}={}", powershell_quote_always(value)));
    }
    let mut argv: Vec<String> = plan.argv.iter().map(|a| powershell_quote(a)).collect();
    if let Some(first) = argv.first_mut()
        && first.starts_with('\'')
    {
        *first = format!("& {first}");
    }
    statements.push(argv.join(" "));
    statements.join("; ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn plan(cwd: &str, argv: &[&str], set: &[(&str, &str)], remove: &[&str]) -> LaunchPlan {
        LaunchPlan {
            cwd: PathBuf::from(cwd),
            argv: argv.iter().map(|s| s.to_string()).collect(),
            env_set: set
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            env_remove: remove.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn quotes_for_posix() {
        assert_eq!(posix_quote("c1"), "c1");
        assert_eq!(posix_quote("/home/me/proj"), "/home/me/proj");
        assert_eq!(posix_quote("has space"), "'has space'");
        assert_eq!(posix_quote("it's"), r"'it'\''s'");
        assert_eq!(posix_quote(""), "''");
        assert_eq!(posix_quote("=cat"), "'=cat'");
        assert_eq!(posix_quote("a=b"), "'a=b'");
    }

    #[test]
    fn quotes_for_powershell() {
        assert_eq!(powershell_quote("c2"), "c2");
        assert_eq!(powershell_quote(r"C:\x"), r"C:\x");
        assert_eq!(powershell_quote("a b"), "'a b'");
        assert_eq!(powershell_quote("it's"), "'it''s'");
    }

    #[test]
    fn posix_commands() {
        assert_eq!(
            resume_command(
                &plan("/home/me/proj", &["ccs", "c1", "--resume", "abc"], &[], &[]),
                &Env::Linux
            ),
            "cd '/home/me/proj' && ccs c1 --resume abc"
        );
        assert_eq!(
            resume_command(
                &plan(
                    "/p",
                    &["claude", "--resume", "abc"],
                    &[],
                    &["CLAUDE_CONFIG_DIR"]
                ),
                &Env::Wsl {
                    distro: "Ubuntu".into()
                }
            ),
            "cd '/p' && env -u CLAUDE_CONFIG_DIR claude --resume abc"
        );
        assert_eq!(
            resume_command(
                &plan(
                    "",
                    &["claude", "--resume", "abc"],
                    &[("CLAUDE_CONFIG_DIR", "/home/me/.claude work")],
                    &[]
                ),
                &Env::MacOs
            ),
            "CLAUDE_CONFIG_DIR='/home/me/.claude work' claude --resume abc"
        );
        assert_eq!(
            resume_command(
                &plan(
                    "/p",
                    &["claude", "--resume", "abc"],
                    &[("CLAUDE_CONFIG_DIR", "/x")],
                    &["OTHER"]
                ),
                &Env::Linux
            ),
            "cd '/p' && env -u OTHER CLAUDE_CONFIG_DIR='/x' claude --resume abc"
        );
    }

    #[test]
    fn powershell_commands() {
        assert_eq!(
            resume_command(
                &plan(
                    r"C:\Users\me\proj",
                    &["ccs", "c2", "--resume", "abc"],
                    &[],
                    &[]
                ),
                &Env::Windows
            ),
            r"Set-Location 'C:\Users\me\proj'; ccs c2 --resume abc"
        );
        assert_eq!(
            resume_command(
                &plan(
                    r"C:\p",
                    &["claude", "--resume", "abc"],
                    &[("CLAUDE_CONFIG_DIR", r"C:\Users\me\.claude")],
                    &["X"]
                ),
                &Env::Windows
            ),
            r"Set-Location 'C:\p'; Remove-Item Env:X -ErrorAction Ignore; $env:CLAUDE_CONFIG_DIR='C:\Users\me\.claude'; claude --resume abc"
        );
        assert_eq!(
            resume_command(
                &plan("", &[r"C:\Program Files\c.exe", "--resume", "a"], &[], &[]),
                &Env::Windows
            ),
            r"& 'C:\Program Files\c.exe' --resume a"
        );
    }
}
