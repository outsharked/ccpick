//! Codex's own index of its sessions: `~/.codex/state_<n>.sqlite`. Read-only — Codex holds this
//! database open in WAL mode while it runs, and ccpick never writes to another tool's data.
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

/// Threads a person would resume: started from the CLI or the editor, with something in them.
///
/// Two thirds of a real database is subagent threads — children spawned by a parent run, which
/// nobody resumes — so they are excluded by `source`, along with threads that never got a title
/// or a first message.
pub fn listable_threads(db: &Path) -> anyhow::Result<Vec<Thread>> {
    // `mode=ro` on a URI: opening read-only cannot create or migrate the file, and is safe
    // alongside Codex's own writer.
    let uri = format!("file:{}?mode=ro", db.display());
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
               ('id-empty','/s/2026/09/18/rollout-d.jsonl','/home/me','',NULL,'',NULL,1300,2300,'cli',0);",
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
            vec!["id-named", "id-cli"],
            "newest first, no subagent, no stillborn"
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
