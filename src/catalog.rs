//! Builds the in-memory session list: sources → stores → scanned sessions (+ live info).
use crate::cache::{Cache, Stamp};
use crate::config::Settings;
use crate::model::{LaunchPlan, Message, Provider, SessionMeta, Source};
use rayon::prelude::*;
use std::collections::HashMap;
use std::path::PathBuf;

pub struct Session {
    pub meta: SessionMeta,
    pub provider: usize,
    /// Indices into `Catalog::sources` that can resume this session, in priority order.
    pub sources: Vec<usize>,
    pub default_source: usize,
    /// (pid, source index) when a process is currently running this session.
    pub live: Option<(i32, usize)>,
}

pub struct Store {
    pub provider: usize,
    pub path: PathBuf,
    pub sources: Vec<usize>,
}

pub struct Catalog {
    pub host: crate::env::HostContext,
    pub providers: Vec<Box<dyn Provider>>,
    pub sources: Vec<Source>,
    pub stores: Vec<Store>,
    pub sessions: Vec<Session>,
    pub warnings: Vec<String>,
}

impl Catalog {
    pub fn build(
        providers: Vec<Box<dyn Provider>>,
        settings: &Settings,
        homes: &[crate::homes::Home],
        cache: &mut Cache,
    ) -> anyhow::Result<Catalog> {
        let mut sources = Vec::new();
        let mut source_provider = Vec::new();
        let mut warnings = Vec::new();
        // A source already seen (by provider + canonical config_dir) is dropped, keeping the
        // first: the native home's pass runs first, so a configured `[[source]]` pointing at
        // another environment's config dir wins over the same directory turning up again via
        // that environment's own auto-discovered home.
        let mut seen_config_dirs: std::collections::HashSet<(usize, PathBuf)> =
            std::collections::HashSet::new();
        for home in homes {
            for (pi, provider) in providers.iter().enumerate() {
                let discovery = provider.discover_sources(settings, home)?;
                warnings.extend(discovery.warnings);
                for source in discovery.sources {
                    if !seen_config_dirs.insert((pi, source.config_dir.clone())) {
                        continue;
                    }
                    sources.push(source);
                    source_provider.push(pi);
                }
            }
        }

        // A `[[source]]` entry whose `agent` matches no registered provider
        // would otherwise be silently dropped by every provider's discovery
        // (each only looks at entries for its own id).
        for sc in &settings.file.sources {
            if !providers.iter().any(|p| p.id() == sc.agent) {
                let label = sc.name.as_deref().unwrap_or(sc.config_dir.as_str());
                warnings.push(format!(
                    "source {label} skipped: unknown agent \"{}\"",
                    sc.agent
                ));
            }
        }

        let mut stores: Vec<Store> = Vec::new();
        for (si, source) in sources.iter().enumerate() {
            let pi = source_provider[si];
            match providers[pi].store_for(source) {
                Some(path) => match stores
                    .iter_mut()
                    .find(|s| s.provider == pi && s.path == path)
                {
                    Some(store) => store.sources.push(si),
                    None => stores.push(Store {
                        provider: pi,
                        path,
                        sources: vec![si],
                    }),
                },
                None => warnings.push(format!("source {} has no session store", source.name)),
            }
        }

        // The Windows `tasklist.exe` snapshot (WSL hosts) is taken lazily inside the probe, only
        // once a Windows-domain record is actually checked, filtered by that call's process name.
        let probe = crate::process::ProcessProbe::from_host(&settings.host);

        // 1. Scan every store (cache hits + parallel scan of misses), remembering the store.
        let mut found: Vec<(usize, SessionMeta)> = Vec::new();
        for (store_idx, store) in stores.iter().enumerate() {
            let provider = &providers[store.provider];
            let agent = provider.id();
            let mut metas = Vec::new();
            let mut misses = Vec::new();
            for file in provider.list_session_files(&store.path) {
                let stamp = Stamp::of(&file);
                match stamp.and_then(|s| cache.get(agent, &file, s)) {
                    Some(meta) => metas.push(meta),
                    None => misses.push((file, stamp)),
                }
            }
            let scanned: Vec<(PathBuf, Option<Stamp>, SessionMeta)> = misses
                .into_par_iter()
                .filter_map(|(file, stamp)| provider.scan_file(&file).map(|m| (file, stamp, m)))
                .collect();
            for (file, stamp, meta) in scanned {
                if let Some(stamp) = stamp {
                    cache.put(agent, &file, stamp, meta.clone());
                }
                metas.push(meta);
            }
            found.extend(metas.into_iter().map(|meta| (store_idx, meta)));
        }

        // 2. Launch records, with liveness from the probe.
        let mut launches: HashMap<(usize, String), Vec<(i64, usize)>> = HashMap::new();
        let mut live: HashMap<(usize, String), (i32, usize)> = HashMap::new();
        for (si, source) in sources.iter().enumerate() {
            let pi = source_provider[si];
            for record in providers[pi].launch_records(source, &probe) {
                let k = (pi, record.session_id.clone());
                if record.alive {
                    live.insert(k.clone(), (record.pid, si));
                }
                launches
                    .entry(k)
                    .or_default()
                    .push((record.started_at_ms, si));
            }
        }

        // 3. Assemble sessions.
        let mut sessions = Vec::new();
        for (store_idx, meta) in found {
            let store = &stores[store_idx];
            let k = (store.provider, meta.id.clone());
            let default_source = launches
                .get(&k)
                .and_then(|l| {
                    l.iter()
                        .filter(|(_, si)| store.sources.contains(si))
                        .max_by_key(|(ts, _)| *ts)
                })
                .map(|(_, si)| *si)
                .unwrap_or(store.sources[0]);
            sessions.push(Session {
                live: live.get(&k).copied(),
                meta,
                provider: store.provider,
                sources: store.sources.clone(),
                default_source,
            });
        }
        sessions.sort_by_key(|s| std::cmp::Reverse(s.meta.last_ts));
        warnings.extend(probe.warnings());

        Ok(Catalog {
            host: settings.host.clone(),
            providers,
            sources,
            stores,
            sessions,
            warnings,
        })
    }

    /// True when ccpick can launch sessions from this source itself.
    pub fn is_launchable(&self, source: usize) -> bool {
        self.sources[source].env == self.host.env
    }

    /// A session's index by its bare id. Correct only because ccpick currently registers a
    /// single provider: the scan-time maps in `Catalog::build` (`launches`, `live`) key on
    /// `(provider, id)` since two providers could otherwise mint the same id, but nothing
    /// downstream of this lookup carries a provider alongside the id yet. Centralizing it here
    /// means a second provider only has to fix one place, not every call site that currently
    /// assumes ids are globally unique.
    pub fn find_by_id(&self, id: &str) -> Option<usize> {
        self.sessions.iter().position(|s| s.meta.id == id)
    }

    /// The session's project dir as a path ccpick can check on disk.
    pub fn host_cwd(&self, idx: usize, source: usize) -> Option<PathBuf> {
        let cwd = self.sessions[idx].meta.cwd.as_ref()?;
        self.host.to_host(cwd, &self.sources[source].env)
    }

    pub fn messages(&self, idx: usize) -> Vec<Message> {
        let s = &self.sessions[idx];
        self.providers[s.provider].messages(&s.meta.path)
    }

    pub fn may_contain(&self, idx: usize, needle_lower: &str) -> bool {
        let s = &self.sessions[idx];
        self.providers[s.provider].may_contain(&s.meta.path, needle_lower)
    }

    pub fn launch_plan(&self, idx: usize, source: usize) -> LaunchPlan {
        let s = &self.sessions[idx];
        self.providers[s.provider].launch_plan(&self.sources[source], &s.meta)
    }

    pub fn running_count(&self) -> usize {
        self.sessions.iter().filter(|s| s.live.is_some()).count()
    }
}

/// Discovers homes and builds the catalog from settings. Shared by the CLI entry point and
/// the web portal's refresh loop, which rebuilds on an interval.
pub fn build_from_settings(settings: &Settings, cache: &mut Cache) -> anyhow::Result<Catalog> {
    let providers = crate::providers::all();
    let markers: Vec<&str> = providers
        .iter()
        .flat_map(|p| p.home_markers().iter().copied())
        .collect();
    let (homes, home_warnings) =
        crate::homes::discover_homes(settings, &markers, crate::homes::running_distros);
    let mut catalog = Catalog::build(providers, settings, &homes, cache)?;
    catalog.warnings.extend(home_warnings);
    Ok(catalog)
}

#[cfg(test)]
pub fn fake_catalog() -> Catalog {
    use crate::model::{LaunchRecord, Role};
    use crate::providers::fake::FakeProvider;
    let mut p = FakeProvider::default();
    p.add_source("one", "/s");
    p.add_source("two", "/s");
    p.add_source("three", "/t");
    p.add_session(
        "/s",
        "a",
        "Docker build cache",
        3000,
        3000,
        &[(Role::User, "fix docker"), (Role::Assistant, "done")],
    );
    p.add_session(
        "/s",
        "b",
        "Kubernetes ingress",
        2000,
        2000,
        &[(Role::User, "the ingress needs a PINEAPPLE annotation")],
    );
    p.add_session("/s", "c", "Running thing", 1000, 1000, &[]);
    p.add_session("/t", "d", "Old notes", 9000, 500, &[]);
    p.set_cwd("/t", "d", Some("/nonexistent/ccpick-test"));
    p.records.insert(
        "two".into(),
        vec![LaunchRecord {
            pid: 4242,
            session_id: "c".into(),
            started_at_ms: 5,
            alive: true,
        }],
    );
    build_fake(p)
}

#[cfg(test)]
pub fn build_fake(p: crate::providers::fake::FakeProvider) -> Catalog {
    build_fake_on(p, crate::env::HostContext::default())
}

#[cfg(test)]
pub fn build_fake_on(
    p: crate::providers::fake::FakeProvider,
    host: crate::env::HostContext,
) -> Catalog {
    let settings = Settings {
        host,
        ..Default::default()
    };
    let homes = [crate::homes::Home::native(&settings.host, "/fake".into())];
    Catalog::build(
        vec![Box::new(p)],
        &settings,
        &homes,
        &mut Cache::in_memory(),
    )
    .unwrap()
}

#[cfg(test)]
pub fn fake_catalog_with_foreign() -> Catalog {
    use crate::env::{Env, HostContext};
    use crate::model::Role;
    use crate::providers::fake::FakeProvider;
    let ubuntu = Env::Wsl {
        distro: "Ubuntu".into(),
    };
    let mut p = FakeProvider::default();
    p.add_source_in("one", "/s", ubuntu.clone(), "/fake");
    p.add_source_in("win:c1", "/w", Env::Windows, r"C:\Users\me");
    p.add_session(
        "/s",
        "n",
        "Native session",
        2000,
        2000,
        &[(Role::User, "hello")],
    );
    p.add_session(
        "/w",
        "w",
        "Windows session",
        1000,
        1000,
        &[(Role::User, "from windows")],
    );
    p.set_cwd("/w", "w", Some(r"C:\Users\me\proj"));
    build_fake_on(
        p,
        HostContext {
            env: ubuntu,
            wsl_mount_root: PathBuf::from("/nonexistent-root/"),
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::LaunchRecord;
    use crate::providers::claude::ClaudeProvider;
    use crate::providers::fake::FakeProvider;

    fn ids(c: &Catalog) -> Vec<&str> {
        c.sessions.iter().map(|s| s.meta.id.as_str()).collect()
    }

    #[test]
    fn groups_shared_stores_and_sorts() {
        let c = fake_catalog();
        assert_eq!(ids(&c), vec!["a", "b", "c", "d"]);
        assert_eq!(c.stores.len(), 2);
        assert_eq!(c.sessions[0].sources, vec![0, 1]);
        assert_eq!(c.sessions[0].default_source, 0);
        assert_eq!(c.sessions[3].sources, vec![2]);
        assert_eq!(c.sessions[3].default_source, 2);
    }

    #[test]
    fn live_and_default_source_from_records() {
        let c = fake_catalog();
        let running = &c.sessions[2];
        assert_eq!(running.live, Some((4242, 1)));
        assert_eq!(running.default_source, 1);
        assert_eq!(c.running_count(), 1);
    }

    #[test]
    fn newest_eligible_record_wins() {
        let mut p = FakeProvider::default();
        p.add_source("one", "/s");
        p.add_source("two", "/s");
        p.add_source("other", "/o");
        p.add_session("/s", "x", "X", 1, 1, &[]);
        let rec = |ts| LaunchRecord {
            pid: 1,
            session_id: "x".into(),
            started_at_ms: ts,
            alive: false,
        };
        p.records.insert("one".into(), vec![rec(10)]);
        p.records.insert("two".into(), vec![rec(20)]);
        p.records.insert("other".into(), vec![rec(30)]);
        let c = build_fake(p);
        // "other" has the newest record but can't see store /s; "two" is newest among eligible.
        assert_eq!(c.sessions[0].default_source, 1);
        assert_eq!(c.sessions[0].live, None);
    }

    #[test]
    fn launch_plan_and_messages_delegate() {
        let c = fake_catalog();
        assert_eq!(
            c.launch_plan(0, 1).argv,
            vec!["fake", "two", "--resume", "a"]
        );
        assert_eq!(
            c.messages(1)[0].text,
            "the ingress needs a PINEAPPLE annotation"
        );
        assert!(c.may_contain(1, "anything"));
    }

    #[test]
    fn missing_store_warns() {
        let mut p = FakeProvider::default();
        p.add_source("one", "/s");
        p.stores.clear();
        let c = build_fake(p);
        assert!(c.sessions.is_empty());
        assert_eq!(c.warnings.len(), 1);
    }

    #[test]
    fn unknown_source_agent_warns() {
        let mut p = FakeProvider::default();
        p.add_source("one", "/s");
        let settings = Settings {
            file: crate::config::parse_config(
                "[[source]]\nagent = \"bogus\"\nname = \"ghost\"\nconfig_dir = \"/nowhere\"\n",
            )
            .unwrap(),
            ..Default::default()
        };
        let homes = [crate::homes::Home::native(
            &settings.host,
            settings.home.clone(),
        )];
        let c = Catalog::build(
            vec![Box::new(p)],
            &settings,
            &homes,
            &mut Cache::in_memory(),
        )
        .unwrap();
        assert_eq!(c.warnings.len(), 1);
        assert!(c.warnings[0].contains("ghost"));
        assert!(c.warnings[0].contains("unknown agent"));
        assert!(c.warnings[0].contains("bogus"));
    }

    #[test]
    fn real_files_use_cache() {
        let tmp = tempfile::tempdir().unwrap();
        let proj = tmp.path().join(".claude/projects/-work-proj");
        std::fs::create_dir_all(&proj).unwrap();
        let file = proj.join("basic.jsonl");
        std::fs::copy(
            crate::testutil::manifest_dir().join("tests/fixtures/claude/basic.jsonl"),
            &file,
        )
        .unwrap();
        let settings = Settings {
            home: tmp.path().to_path_buf(),
            ..Default::default()
        };
        let mut cache = Cache::in_memory();
        let homes = [crate::homes::Home::native(
            &settings.host,
            settings.home.clone(),
        )];

        let c1 = Catalog::build(
            vec![Box::new(ClaudeProvider)],
            &settings,
            &homes,
            &mut cache,
        )
        .unwrap();
        assert_eq!(c1.sessions[0].meta.title, "Docker build cache fix");
        assert_eq!(cache.len(), 1);

        // Plant a different title under the same stamp; a cache hit must return it.
        let canonical = dunce::canonicalize(&file).unwrap();
        let mut planted = c1.sessions[0].meta.clone();
        planted.title = "CACHED".into();
        cache.put(
            "claude",
            &canonical,
            Stamp::of(&canonical).unwrap(),
            planted,
        );
        let c2 = Catalog::build(
            vec![Box::new(ClaudeProvider)],
            &settings,
            &homes,
            &mut cache,
        )
        .unwrap();
        assert_eq!(c2.sessions[0].meta.title, "CACHED");
    }

    #[test]
    fn foreign_sources_are_not_launchable_and_cwd_translates() {
        let c = fake_catalog_with_foreign();
        let w = c.sessions.iter().position(|s| s.meta.id == "w").unwrap();
        let n = c.sessions.iter().position(|s| s.meta.id == "n").unwrap();
        assert!(c.is_launchable(c.sessions[n].default_source));
        assert!(!c.is_launchable(c.sessions[w].default_source));
        assert_eq!(
            c.host_cwd(w, c.sessions[w].default_source),
            Some(PathBuf::from("/nonexistent-root/c/Users/me/proj"))
        );
        assert_eq!(
            c.host_cwd(n, c.sessions[n].default_source),
            Some(PathBuf::from(env!("CARGO_MANIFEST_DIR")))
        );
    }

    #[test]
    fn cross_home_sources_pointing_at_the_same_config_dir_are_deduped() {
        use crate::env::Env;
        use crate::homes::Home;
        let mut p = FakeProvider::default();
        // Native pass discovers "one"; the "win" home's pass discovers "win:one", a configured
        // source pointing at the very same underlying directory the foreign home also sees.
        p.add_source_in("one", "/s", Env::Linux, "/fake");
        p.add_foreign_source_at(
            "win",
            "win:one",
            "/s",
            Env::Windows,
            r"C:\Users\me",
            "/fake/one",
        );
        let settings = Settings::default();
        let homes = [
            Home::native(&settings.host, settings.home.clone()),
            Home {
                env: Env::Windows,
                dir: PathBuf::from("/foreign"),
                env_dir: PathBuf::from(r"C:\Users\me"),
                label: Some("win".into()),
            },
        ];
        let c = Catalog::build(
            vec![Box::new(p)],
            &settings,
            &homes,
            &mut Cache::in_memory(),
        )
        .unwrap();
        assert_eq!(c.sources.len(), 1);
        assert_eq!(c.sources[0].name, "one");
    }

    #[test]
    fn scans_every_home() {
        use crate::homes::Home;
        let tmp = tempfile::tempdir().unwrap();
        for dir in ["native/.claude/projects/p", "win/.claude/projects/p"] {
            std::fs::create_dir_all(tmp.path().join(dir)).unwrap();
        }
        let fixture = crate::providers::claude::transcript::fixture_path("basic.jsonl");
        std::fs::copy(
            &fixture,
            tmp.path().join("native/.claude/projects/p/a.jsonl"),
        )
        .unwrap();
        std::fs::copy(&fixture, tmp.path().join("win/.claude/projects/p/b.jsonl")).unwrap();
        let settings = Settings {
            home: tmp.path().join("native"),
            ..Default::default()
        };
        let homes = [
            Home::native(&settings.host, settings.home.clone()),
            Home {
                env: crate::env::Env::Windows,
                dir: tmp.path().join("win"),
                env_dir: PathBuf::from(r"C:\Users\me"),
                label: Some("win".into()),
            },
        ];
        let c = Catalog::build(
            vec![Box::new(ClaudeProvider)],
            &settings,
            &homes,
            &mut Cache::in_memory(),
        )
        .unwrap();
        let names: Vec<&str> = c.sources.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, vec!["claude", "win:claude"]);
        assert_eq!(c.sessions.len(), 2);
    }

    #[test]
    fn build_from_settings_discovers_nothing_in_an_empty_home() {
        let tmp = tempfile::tempdir().unwrap();
        let settings = Settings {
            home: tmp.path().to_path_buf(),
            ..Default::default()
        };
        let mut cache = Cache::in_memory();
        let catalog = build_from_settings(&settings, &mut cache).unwrap();
        assert!(catalog.sessions.is_empty());
        assert!(catalog.sources.is_empty());
    }
}
