//! In-memory provider used to test the agent-neutral layers.
use crate::config::Settings;
use crate::model::{
    Discovery, LaunchPlan, LaunchRecord, LaunchSpec, Message, Provider, Role, SessionMeta, Source,
};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

#[derive(Default)]
pub struct FakeProvider {
    pub sources: Vec<Source>,
    /// Sources returned for a foreign (non-native) home, keyed by the home's label. Lets tests
    /// simulate a source that's discoverable from more than one home (e.g. a configured
    /// `[[source]]` in the native home's pass pointing at the same directory an auto-discovered
    /// foreign home also finds).
    pub foreign_sources: HashMap<String, Vec<Source>>,
    pub stores: HashMap<String, PathBuf>,
    pub files: HashMap<PathBuf, (SessionMeta, Vec<Message>)>,
    pub records: HashMap<String, Vec<LaunchRecord>>,
}

impl FakeProvider {
    pub fn add_source(&mut self, name: &str, store: &str) {
        self.add_source_in(name, store, crate::env::Env::Linux, "/fake");
    }

    /// A source in a specific environment (e.g. Windows seen from WSL).
    pub fn add_source_in(&mut self, name: &str, store: &str, env: crate::env::Env, env_home: &str) {
        let config_dir = format!("/fake/{name}");
        let source = Self::build_source(name, &config_dir, env, env_home);
        self.sources.push(source);
        self.stores.insert(name.into(), PathBuf::from(store));
    }

    /// A source returned only when discovering the home labeled `label` (native home sources use
    /// `add_source`/`add_source_in`), at an explicit `config_dir` so it can collide with another
    /// home's source for cross-home dedup tests.
    pub fn add_foreign_source_at(
        &mut self,
        label: &str,
        name: &str,
        store: &str,
        env: crate::env::Env,
        env_home: &str,
        config_dir: &str,
    ) {
        let source = Self::build_source(name, config_dir, env, env_home);
        self.foreign_sources
            .entry(label.into())
            .or_default()
            .push(source);
        self.stores.insert(name.into(), PathBuf::from(store));
    }

    fn build_source(name: &str, config_dir: &str, env: crate::env::Env, env_home: &str) -> Source {
        let config_dir = PathBuf::from(config_dir);
        Source {
            agent: "fake".into(),
            name: name.into(),
            env_config_dir: config_dir.clone(),
            config_dir,
            env,
            env_home: PathBuf::from(env_home),
            launch: LaunchSpec {
                argv_prefix: vec!["fake".into(), name.into()],
                ..Default::default()
            },
        }
    }

    pub fn add_session(
        &mut self,
        store: &str,
        id: &str,
        title: &str,
        first_ts: i64,
        last_ts: i64,
        messages: &[(Role, &str)],
    ) {
        let path = PathBuf::from(store).join(format!("{id}.jsonl"));
        let meta = SessionMeta {
            agent: "fake".into(),
            id: id.into(),
            path: path.clone(),
            title: title.into(),
            cwd: Some(PathBuf::from("/tmp")),
            branch: None,
            first_ts: Some(first_ts),
            last_ts: Some(last_ts),
            first_prompt: String::new(),
            msg_count: messages.len() as u32,
        };
        let msgs = messages
            .iter()
            .map(|(role, text)| Message {
                role: *role,
                text: text.to_string(),
                ts: None,
            })
            .collect();
        self.files.insert(path, (meta, msgs));
    }

    pub fn set_cwd(&mut self, store: &str, id: &str, cwd: Option<&str>) {
        let path = PathBuf::from(store).join(format!("{id}.jsonl"));
        if let Some(entry) = self.files.get_mut(&path) {
            entry.0.cwd = cwd.map(PathBuf::from);
        }
    }
}

impl Provider for FakeProvider {
    fn id(&self) -> &'static str {
        "fake"
    }
    fn discover_sources(
        &self,
        _settings: &Settings,
        home: &crate::homes::Home,
    ) -> anyhow::Result<Discovery> {
        let sources = match &home.label {
            None => self.sources.clone(),
            Some(label) => self.foreign_sources.get(label).cloned().unwrap_or_default(),
        };
        Ok(Discovery {
            sources,
            warnings: vec![],
        })
    }
    fn store_for(&self, source: &Source) -> Option<PathBuf> {
        self.stores.get(&source.name).cloned()
    }
    fn list_session_files(&self, store: &Path) -> Vec<PathBuf> {
        let mut files: Vec<PathBuf> = self
            .files
            .keys()
            .filter(|p| p.parent() == Some(store))
            .cloned()
            .collect();
        files.sort();
        files
    }
    fn scan_file(&self, path: &Path) -> Option<SessionMeta> {
        self.files.get(path).map(|f| f.0.clone())
    }
    fn messages(&self, path: &Path) -> Vec<Message> {
        self.files
            .get(path)
            .map(|f| f.1.clone())
            .unwrap_or_default()
    }
    fn launch_records(
        &self,
        source: &Source,
        _probe: &crate::process::ProcessProbe,
    ) -> Vec<LaunchRecord> {
        self.records.get(&source.name).cloned().unwrap_or_default()
    }
    fn launch_plan(&self, source: &Source, session: &SessionMeta) -> LaunchPlan {
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
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fake_provider_round_trip() {
        let mut p = FakeProvider::default();
        p.add_source("one", "/s");
        p.add_session("/s", "a", "Title", 1, 2, &[(Role::User, "hi")]);
        let src = &p
            .discover_sources(
                &Settings::default(),
                &crate::homes::Home::native(&Default::default(), "/fake".into()),
            )
            .unwrap()
            .sources[0];
        let store = p.store_for(src).unwrap();
        let files = p.list_session_files(&store);
        assert_eq!(files.len(), 1);
        let meta = p.scan_file(&files[0]).unwrap();
        assert_eq!(p.messages(&files[0])[0].text, "hi");
        assert_eq!(
            p.launch_plan(src, &meta).argv,
            vec!["fake", "one", "--resume", "a"]
        );
    }
}
