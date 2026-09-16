//! Per-file metadata cache keyed by (agent, path), validated by size + mtime.
use crate::model::SessionMeta;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

const VERSION: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Stamp {
    pub size: u64,
    pub mtime_ms: i64,
}

impl Stamp {
    pub fn of(path: &Path) -> Option<Stamp> {
        let md = std::fs::metadata(path).ok()?;
        let mtime_ms = md.modified().ok()?.duration_since(UNIX_EPOCH).ok()?.as_millis() as i64;
        Some(Stamp { size: md.len(), mtime_ms })
    }
}

#[derive(Serialize, Deserialize)]
struct Entry {
    stamp: Stamp,
    meta: SessionMeta,
}

#[derive(Serialize, Deserialize)]
struct CacheFile {
    version: u32,
    entries: HashMap<String, Entry>,
}

pub struct Cache {
    path: Option<PathBuf>,
    entries: HashMap<String, Entry>,
    touched: HashSet<String>,
    dirty: bool,
}

fn key(agent: &str, file: &Path) -> String {
    format!("{agent}\t{}", file.display())
}

pub fn default_cache_path() -> Option<PathBuf> {
    dirs::cache_dir().map(|d| d.join("ccpick").join("meta.json"))
}

impl Cache {
    pub fn load(path: PathBuf) -> Cache {
        let entries = std::fs::read(&path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<CacheFile>(&bytes).ok())
            .filter(|file| file.version == VERSION)
            .map(|file| file.entries)
            .unwrap_or_default();
        Cache { path: Some(path), entries, touched: HashSet::new(), dirty: false }
    }

    pub fn in_memory() -> Cache {
        Cache { path: None, entries: HashMap::new(), touched: HashSet::new(), dirty: false }
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn get(&mut self, agent: &str, file: &Path, stamp: Stamp) -> Option<SessionMeta> {
        let k = key(agent, file);
        let hit = self.entries.get(&k).filter(|e| e.stamp == stamp).map(|e| e.meta.clone());
        if hit.is_some() {
            self.touched.insert(k);
        }
        hit
    }

    pub fn put(&mut self, agent: &str, file: &Path, stamp: Stamp, meta: SessionMeta) {
        let k = key(agent, file);
        self.touched.insert(k.clone());
        self.entries.insert(k, Entry { stamp, meta });
        self.dirty = true;
    }

    pub fn save(&mut self) -> std::io::Result<()> {
        let before = self.entries.len();
        let touched = &self.touched;
        self.entries.retain(|k, _| touched.contains(k));
        if self.entries.len() != before {
            self.dirty = true;
        }
        let Some(path) = self.path.clone() else { return Ok(()) };
        if !self.dirty {
            return Ok(());
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let file = CacheFile { version: VERSION, entries: std::mem::take(&mut self.entries) };
        let bytes = serde_json::to_vec(&file).map_err(std::io::Error::other);
        self.entries = file.entries;
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, bytes?)?;
        std::fs::rename(&tmp, &path)?;
        self.dirty = false;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn meta(id: &str) -> SessionMeta {
        SessionMeta {
            agent: "claude".into(),
            id: id.into(),
            path: PathBuf::from(format!("/s/{id}.jsonl")),
            title: format!("title {id}"),
            cwd: None,
            branch: None,
            first_ts: Some(1),
            last_ts: Some(2),
            first_prompt: String::new(),
            msg_count: 3,
        }
    }

    const STAMP: Stamp = Stamp { size: 10, mtime_ms: 100 };

    #[test]
    fn round_trip_hit() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("sub/meta.json");
        let mut c = Cache::load(path.clone());
        c.put("claude", Path::new("/s/a.jsonl"), STAMP, meta("a"));
        c.save().unwrap();
        let mut c2 = Cache::load(path);
        assert_eq!(c2.get("claude", Path::new("/s/a.jsonl"), STAMP), Some(meta("a")));
    }

    #[test]
    fn changed_stamp_or_agent_misses() {
        let mut c = Cache::in_memory();
        c.put("claude", Path::new("/s/a.jsonl"), STAMP, meta("a"));
        assert!(c.get("claude", Path::new("/s/a.jsonl"), Stamp { size: 11, mtime_ms: 100 }).is_none());
        assert!(c.get("claude", Path::new("/s/a.jsonl"), Stamp { size: 10, mtime_ms: 101 }).is_none());
        assert!(c.get("codex", Path::new("/s/a.jsonl"), STAMP).is_none());
    }

    #[test]
    fn untouched_entries_dropped_on_save() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("meta.json");
        let mut c = Cache::load(path.clone());
        c.put("claude", Path::new("/s/a.jsonl"), STAMP, meta("a"));
        c.put("claude", Path::new("/s/b.jsonl"), STAMP, meta("b"));
        c.save().unwrap();

        let mut c2 = Cache::load(path.clone());
        assert!(c2.get("claude", Path::new("/s/a.jsonl"), STAMP).is_some());
        c2.save().unwrap();
        let c3 = Cache::load(path);
        assert_eq!(c3.len(), 1);
    }

    #[test]
    fn version_mismatch_and_corrupt_file_are_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let old = tmp.path().join("old.json");
        std::fs::write(&old, r#"{"version":0,"entries":{}}"#).unwrap();
        assert_eq!(Cache::load(old).len(), 0);
        let bad = tmp.path().join("bad.json");
        std::fs::write(&bad, "{not json").unwrap();
        assert_eq!(Cache::load(bad).len(), 0);
    }

    #[test]
    fn stamp_of_real_file() {
        let tmp = tempfile::tempdir().unwrap();
        let f = tmp.path().join("x");
        std::fs::write(&f, "12345").unwrap();
        let s = Stamp::of(&f).unwrap();
        assert_eq!(s.size, 5);
        assert!(s.mtime_ms > 0);
        assert!(Stamp::of(&tmp.path().join("missing")).is_none());
    }
}
