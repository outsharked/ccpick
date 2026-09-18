//! Codex's own index of its sessions: `~/.codex/state_<n>.sqlite`. Read-only — Codex holds this
//! database open in WAL mode while it runs, and ccpick never writes to another tool's data. See
//! `read_only_uri` for how the read stays read-only in both the running and not-running case.
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq)]
pub struct Thread {
    pub id: String,
    pub rollout_path: PathBuf,
    pub cwd: String,
    pub title: String,
    pub name: Option<String>,
    pub first_user_message: String,
    pub git_branch: Option<String>,
    pub created_at_ms: i64,
    pub recency_at_ms: i64,
}

/// The state database, newest schema wins. The number is part of the filename (`state_5`), so a
/// future `state_6` is picked up without a code change. Compared numerically: `state_10` is
/// newer than `state_9`, which a string sort would get wrong.
pub fn newest_state_db(config_dir: &Path) -> Option<PathBuf> {
    let mut best: Option<(u32, PathBuf)> = None;
    for entry in std::fs::read_dir(config_dir).ok()?.flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        let Some(n) = name
            .strip_prefix("state_")
            .and_then(|rest| rest.strip_suffix(".sqlite"))
            .and_then(|n| n.parse::<u32>().ok())
        else {
            continue;
        };
        if best.as_ref().is_none_or(|(seen, _)| n > *seen) {
            best = Some((n, path));
        }
    }
    best.map(|(_, path)| path)
}

/// Codex holds this database open in WAL mode while it runs, leaving a `-wal`/`-shm` sidecar
/// pair beside it. Reading a WAL database read-only still needs a shared-memory region to
/// establish a consistent snapshot, and SQLite creates one on demand — even under
/// `SQLITE_OPEN_READ_ONLY` — if it's missing. So a plain read-only open would write to Codex's
/// own directory the moment Codex is *not* running, which is the common case: this scans while
/// someone is picking a session to resume, not necessarily while Codex itself is running.
///
/// `immutable=1` tells SQLite the file can't change and skips the WAL machinery entirely, which
/// avoids that write — but it is only safe when there is nothing in the WAL to skip. Against a
/// database Codex is actively writing to, `immutable=1` risks a stale read at best and
/// `SQLITE_CORRUPT` at worst, per SQLite's own documentation. So the choice is made per file, not
/// once for all runs: a `-wal` sidecar present means Codex has this database open in WAL mode
/// right now — the sidecars already exist, so opening plain read-only creates nothing new, and
/// the WAL is honoured so the read is current. No `-wal` means the database was last closed
/// cleanly with nothing pending, so `immutable=1` is safe and, since it skips the WAL machinery
/// altogether, creates nothing either.
///
/// The path is interpolated into the URI unescaped: a path containing `?`, `#` or `%` would
/// break the parse. Not reachable today — this is always a dotfile config directory derived from
/// the OS home directory, never arbitrary user input — so this is a known limitation, not a bug
/// to fix here.
fn read_only_uri(db: &Path) -> String {
    let mut wal = db.as_os_str().to_owned();
    wal.push("-wal");
    if Path::new(&wal).exists() {
        format!("file:{}?mode=ro", db.display())
    } else {
        format!("file:{}?mode=ro&immutable=1", db.display())
    }
}

/// Threads a person would resume: started from the CLI or the editor, with something in them.
///
/// Two thirds of a real database is subagent threads — children spawned by a parent run, which
/// nobody resumes — so they are excluded by `source`, along with threads that never got a title
/// or a first message.
pub fn listable_threads(db: &Path) -> anyhow::Result<Vec<Thread>> {
    // A URI, not a plain path: `mode=ro` (and, per `read_only_uri`, sometimes `immutable=1`)
    // only take effect this way, and neither can create or migrate the file.
    let uri = read_only_uri(db);
    let conn = rusqlite::Connection::open_with_flags(
        uri,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_URI,
    )?;
    let mut stmt = conn.prepare(
        "SELECT id, rollout_path, cwd, title, name, first_user_message, git_branch,
                COALESCE(created_at_ms, 0), recency_at_ms
         FROM threads
         WHERE source IN ('cli', 'vscode')
           AND (COALESCE(title, '') <> '' OR COALESCE(name, '') <> ''
                OR COALESCE(first_user_message, '') <> '')
         ORDER BY recency_at_ms DESC",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok(Thread {
            id: row.get(0)?,
            rollout_path: PathBuf::from(row.get::<_, String>(1)?),
            cwd: row.get(2)?,
            title: row.get(3)?,
            name: row.get(4)?,
            first_user_message: row.get(5)?,
            git_branch: row.get(6)?,
            created_at_ms: row.get(7)?,
            recency_at_ms: row.get(8)?,
        })
    })?;
    Ok(rows.collect::<Result<Vec<_>, _>>()?)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fixture with the columns this reader uses. The real table has ~40 columns; these are
    /// the ones the provider depends on, so a schema change that drops one fails here.
    ///
    /// `pub(crate)` because Task 3's provider tests build on the same fixture: two fixtures that
    /// drift apart would let a provider test pass against a shape the reader never sees.
    pub(crate) fn tests_fixture(dir: &Path) -> PathBuf {
        let path = dir.join("state_5.sqlite");
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch(
            "CREATE TABLE threads (
                id TEXT PRIMARY KEY, rollout_path TEXT NOT NULL, cwd TEXT NOT NULL,
                title TEXT NOT NULL, name TEXT, first_user_message TEXT NOT NULL DEFAULT '',
                git_branch TEXT, created_at_ms INTEGER, recency_at_ms INTEGER NOT NULL DEFAULT 0,
                source TEXT NOT NULL, archived INTEGER NOT NULL DEFAULT 0);
             INSERT INTO threads VALUES
               ('id-cli','/s/2026/09/18/rollout-a.jsonl','/home/me/proj','Fix the parser','Parser',
                'fix the parser please','main',1000,2000,'cli',0),
               ('id-named','/s/2026/09/18/rollout-b.jsonl','/home/me','Generated title','My Name',
                'hello',NULL,1100,2100,'vscode',0),
               ('id-sub','/s/2026/09/18/rollout-c.jsonl','/home/me','Subagent work',NULL,
                'go',NULL,1200,2200,'{\"subagent\":{\"other\":\"guardian\"}}',0),
               ('id-empty','/s/2026/09/18/rollout-d.jsonl','/home/me','',NULL,'',NULL,1300,2300,'cli',0),
               ('id-null-created','/s/2026/09/18/rollout-e.jsonl','/home/me','Fallback title',NULL,
                'fallback message',NULL,NULL,900,'cli',0);",
        )
        .unwrap();
        path
    }

    #[test]
    fn subagent_and_stillborn_threads_are_not_listed() {
        let tmp = tempfile::tempdir().unwrap();
        let db = tests_fixture(tmp.path());
        let ids: Vec<String> = listable_threads(&db)
            .unwrap()
            .into_iter()
            .map(|t| t.id)
            .collect();
        assert_eq!(
            ids,
            vec!["id-named", "id-cli", "id-null-created"],
            "newest first, no subagent, no stillborn"
        );
    }

    #[test]
    fn nulls_decode_to_none_and_a_missing_created_at_becomes_zero() {
        let tmp = tempfile::tempdir().unwrap();
        let db = tests_fixture(tmp.path());
        let threads = listable_threads(&db).unwrap();
        // id-named has a NULL git_branch in the fixture; a NULL column must decode to `None`,
        // not `Some("")`.
        let named = threads.iter().find(|t| t.id == "id-named").unwrap();
        assert_eq!(named.git_branch, None);
        // id-null-created has a NULL name and a NULL created_at_ms. The query defends the latter
        // with `COALESCE(created_at_ms, 0)`; every other fixture row has a value, so without this
        // row that COALESCE could be deleted and nothing here would notice.
        let fallback = threads.iter().find(|t| t.id == "id-null-created").unwrap();
        assert_eq!(fallback.name, None);
        assert_eq!(
            fallback.created_at_ms, 0,
            "NULL created_at_ms coalesces to 0"
        );
    }

    #[test]
    fn reading_a_checkpointed_database_creates_no_wal_sidecars() {
        let tmp = tempfile::tempdir().unwrap();
        let db = tests_fixture(tmp.path());
        {
            // Switch the fixture into WAL mode and checkpoint it back to nothing pending, then
            // let the connection drop (closing it) — the state a real `~/.codex` is in whenever
            // Codex itself isn't running.
            let conn = rusqlite::Connection::open(&db).unwrap();
            conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA wal_checkpoint(TRUNCATE);")
                .unwrap();
        }
        let sidecar = |suffix: &str| {
            let mut p = db.as_os_str().to_owned();
            p.push(suffix);
            PathBuf::from(p)
        };
        assert!(
            !sidecar("-wal").exists() && !sidecar("-shm").exists(),
            "fixture setup should already be clean before the read under test"
        );

        listable_threads(&db).unwrap();

        assert!(!sidecar("-wal").exists(), "reading created a WAL sidecar");
        assert!(
            !sidecar("-shm").exists(),
            "reading created a shared-memory sidecar"
        );
    }

    #[test]
    fn a_thread_carries_what_the_list_shows() {
        let tmp = tempfile::tempdir().unwrap();
        let db = tests_fixture(tmp.path());
        let threads = listable_threads(&db).unwrap();
        let cli = threads.iter().find(|t| t.id == "id-cli").unwrap();
        assert_eq!(
            cli.rollout_path,
            PathBuf::from("/s/2026/09/18/rollout-a.jsonl")
        );
        assert_eq!(cli.cwd, "/home/me/proj");
        assert_eq!(cli.title, "Fix the parser");
        assert_eq!(cli.name.as_deref(), Some("Parser"));
        assert_eq!(cli.git_branch.as_deref(), Some("main"));
        assert_eq!(cli.created_at_ms, 1000);
        assert_eq!(cli.recency_at_ms, 2000);
    }

    #[test]
    fn the_highest_numbered_state_database_wins() {
        let tmp = tempfile::tempdir().unwrap();
        for name in [
            "state_2.sqlite",
            "state_10.sqlite",
            "state_9.sqlite",
            "notes.txt",
        ] {
            std::fs::write(tmp.path().join(name), b"").unwrap();
        }
        assert_eq!(
            newest_state_db(tmp.path()).unwrap().file_name().unwrap(),
            "state_10.sqlite",
            "10 beats 9 numerically, not alphabetically"
        );
        let empty = tempfile::tempdir().unwrap();
        assert_eq!(newest_state_db(empty.path()), None);
    }

    #[test]
    fn a_database_without_the_expected_columns_is_an_error_not_a_panic() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("state_1.sqlite");
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch("CREATE TABLE threads (id TEXT)")
            .unwrap();
        assert!(listable_threads(&path).is_err());
    }
}
