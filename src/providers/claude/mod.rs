pub mod live;
pub mod sources;
pub mod transcript;

use crate::config::Settings;
use crate::model::{Discovery, LaunchPlan, LaunchRecord, Message, Provider, SessionMeta, Source};
use std::path::{Path, PathBuf};

pub struct ClaudeProvider;

/// `<store>/<project>/<session>.jsonl`, following symlinked project dirs, skipping deeper files.
pub fn list_jsonl(store: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Ok(projects) = std::fs::read_dir(store) else {
        return out;
    };
    for project in projects.flatten() {
        let dir = project.path();
        if !dir.is_dir() {
            continue;
        }
        let Ok(files) = std::fs::read_dir(&dir) else {
            continue;
        };
        for file in files.flatten() {
            let path = file.path();
            if path.extension().is_some_and(|e| e == "jsonl") && path.is_file() {
                out.push(path);
            }
        }
    }
    out.sort();
    out
}

pub fn launch_plan(source: &Source, session: &SessionMeta) -> LaunchPlan {
    let mut argv = source.launch.argv_prefix.clone();
    argv.push("--resume".into());
    argv.push(session.id.clone());
    LaunchPlan {
        cwd: session.cwd.clone().unwrap_or_default(),
        argv,
        env_set: source.launch.env_set.clone(),
        env_remove: source.launch.env_remove.clone(),
    }
}

fn is_simple_needle(needle: &str) -> bool {
    !needle.is_empty()
        && needle
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b' ' | b'-' | b'_' | b'.'))
}

impl Provider for ClaudeProvider {
    fn id(&self) -> &'static str {
        transcript::AGENT
    }
    fn discover_sources(&self, settings: &Settings) -> anyhow::Result<Discovery> {
        sources::discover(settings)
    }
    fn store_for(&self, source: &Source) -> Option<PathBuf> {
        std::fs::canonicalize(source.config_dir.join("projects"))
            .ok()
            .filter(|p| p.is_dir())
    }
    fn list_session_files(&self, store: &Path) -> Vec<PathBuf> {
        list_jsonl(store)
    }
    fn scan_file(&self, path: &Path) -> Option<SessionMeta> {
        transcript::scan_file(path)
    }
    fn messages(&self, path: &Path) -> Vec<Message> {
        transcript::messages(path)
    }
    /// Raw-byte, ASCII-case-insensitive scan. Only used for needles that JSON never escapes.
    fn may_contain(&self, path: &Path, needle_lower: &str) -> bool {
        if !is_simple_needle(needle_lower) {
            return true;
        }
        match std::fs::read(path) {
            Ok(mut bytes) => {
                bytes.make_ascii_lowercase();
                memchr::memmem::find(&bytes, needle_lower.as_bytes()).is_some()
            }
            Err(_) => false,
        }
    }
    fn launch_records(&self, source: &Source) -> Vec<LaunchRecord> {
        live::launch_records(&source.config_dir)
    }
    fn launch_plan(&self, source: &Source, session: &SessionMeta) -> LaunchPlan {
        launch_plan(source, session)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::LaunchSpec;
    use std::fs;

    #[test]
    fn symlinked_projects_share_a_store() {
        let tmp = tempfile::tempdir().unwrap();
        let shared = tmp.path().join("shared/projects");
        fs::create_dir_all(&shared).unwrap();
        for n in ["a", "b"] {
            fs::create_dir_all(tmp.path().join(n)).unwrap();
            std::os::unix::fs::symlink(&shared, tmp.path().join(n).join("projects")).unwrap();
        }
        let src = |n: &str| Source {
            agent: "claude".into(),
            name: n.into(),
            config_dir: tmp.path().join(n),
            launch: LaunchSpec::default(),
        };
        let p = ClaudeProvider;
        let a = p.store_for(&src("a")).unwrap();
        assert_eq!(a, p.store_for(&src("b")).unwrap());
        assert_eq!(a, fs::canonicalize(&shared).unwrap());
        assert!(p.store_for(&src("missing")).is_none());
    }

    #[test]
    fn lists_only_project_level_jsonl() {
        let tmp = tempfile::tempdir().unwrap();
        let proj = tmp.path().join("-work-proj");
        fs::create_dir_all(proj.join("s1/subagents")).unwrap();
        fs::write(proj.join("s1.jsonl"), "{}").unwrap();
        fs::write(proj.join("notes.txt"), "x").unwrap();
        fs::write(proj.join("s1/subagents/agent.jsonl"), "{}").unwrap();
        fs::write(tmp.path().join("stray.jsonl"), "{}").unwrap();
        assert_eq!(list_jsonl(tmp.path()), vec![proj.join("s1.jsonl")]);
    }

    fn meta(id: &str, cwd: &str) -> SessionMeta {
        SessionMeta {
            agent: "claude".into(),
            id: id.into(),
            path: PathBuf::from("/x.jsonl"),
            title: "t".into(),
            cwd: Some(PathBuf::from(cwd)),
            branch: None,
            first_ts: None,
            last_ts: None,
            first_prompt: String::new(),
            msg_count: 0,
        }
    }

    #[test]
    fn launch_plans_per_source_kind() {
        let mk = |launch: LaunchSpec| Source {
            agent: "claude".into(),
            name: "n".into(),
            config_dir: PathBuf::from("/c"),
            launch,
        };
        let m = meta("abc", "/work/proj");

        let ccs = launch_plan(
            &mk(LaunchSpec {
                argv_prefix: vec!["ccs".into(), "c1".into()],
                ..Default::default()
            }),
            &m,
        );
        assert_eq!(ccs.argv, vec!["ccs", "c1", "--resume", "abc"]);
        assert_eq!(ccs.cwd, PathBuf::from("/work/proj"));

        let home = launch_plan(
            &mk(LaunchSpec {
                argv_prefix: vec!["claude".into()],
                env_remove: vec!["CLAUDE_CONFIG_DIR".into()],
                ..Default::default()
            }),
            &m,
        );
        assert_eq!(home.argv, vec!["claude", "--resume", "abc"]);
        assert_eq!(home.env_remove, vec!["CLAUDE_CONFIG_DIR"]);

        let dir = launch_plan(
            &mk(LaunchSpec {
                argv_prefix: vec!["claude".into()],
                env_set: vec![("CLAUDE_CONFIG_DIR".into(), "/alt".into())],
                ..Default::default()
            }),
            &m,
        );
        assert_eq!(
            dir.env_set,
            vec![("CLAUDE_CONFIG_DIR".to_string(), "/alt".to_string())]
        );
    }

    #[test]
    fn may_contain_prefilters_raw_bytes() {
        let fixture =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/claude/basic.jsonl");
        let p = ClaudeProvider;
        assert!(p.may_contain(&fixture, "dockerfile"));
        assert!(!p.may_contain(&fixture, "zzzzqqq"));
        // Non-simple needles skip the prefilter.
        assert!(p.may_contain(&fixture, "\"zzzzqqq"));
    }

    #[test]
    fn registry_contains_claude() {
        let all = crate::providers::all();
        assert_eq!(
            all.iter().map(|p| p.id()).collect::<Vec<_>>(),
            vec!["claude"]
        );
    }
}
