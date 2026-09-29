//! SQLite store for vigil heartbeat/wakeup configurations.
//!
//! Vigil entries live in the per-project session DB (`.dirge/sessions/state.db`).
//! The store owns its schema via idempotent `CREATE TABLE IF NOT EXISTS` on open.

use std::path::Path;
use std::sync::Mutex;

use rusqlite::{Connection, OpenFlags, OptionalExtension, params};

/// Lifecycle states for a vigil.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VigilStatus {
    Active,
    Paused,
    Resting,
}

impl VigilStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            VigilStatus::Active => "active",
            VigilStatus::Paused => "paused",
            VigilStatus::Resting => "resting",
        }
    }

    /// Decode the on-disk status column. Unknown values fall back to
    /// `Active` rather than erroring, so a hand-edited row never breaks
    /// the whole list.
    fn from_db_str(s: &str) -> Self {
        match s {
            "active" => VigilStatus::Active,
            "paused" => VigilStatus::Paused,
            "resting" => VigilStatus::Resting,
            _ => VigilStatus::Active,
        }
    }
}

/// A stored vigil row.
pub struct VigilRow {
    pub name: String,
    pub payload_json: String,
    pub status: VigilStatus,
    // Read by the slice-2 keeper/TUI, not by this slice's CLI; kept here
    // because the schema already tracks them.
    #[allow(dead_code)]
    pub created_at: String,
    #[allow(dead_code)]
    pub updated_at: String,
}

/// SQLite-backed vigil store.
pub struct VigilStore {
    conn: Mutex<Connection>,
}

impl VigilStore {
    pub fn open(paths: &super::dirge_paths::ProjectPaths) -> Result<Self, String> {
        Self::open_at(&paths.session_db_path())
    }

    pub fn open_at(path: &Path) -> Result<Self, String> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("create vigil db dir {}: {e}", parent.display()))?;
        }
        let conn = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_CREATE,
        )
        .map_err(|e| format!("open vigil db at {}: {e}", path.display()))?;
        let _ = conn.busy_timeout(std::time::Duration::from_secs(5));
        let _ = conn.pragma_update(None, "journal_mode", "WAL");
        let store = Self {
            conn: Mutex::new(conn),
        };
        store.ensure_schema()?;
        Ok(store)
    }

    fn ensure_schema(&self) -> Result<(), String> {
        let conn = self.conn.lock().unwrap();
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS vigils (
                name        TEXT PRIMARY KEY NOT NULL,
                payload_json TEXT NOT NULL,
                status      TEXT NOT NULL DEFAULT 'active',
                created_at  TEXT NOT NULL DEFAULT (datetime('now')),
                updated_at  TEXT NOT NULL DEFAULT (datetime('now'))
            );
            CREATE INDEX IF NOT EXISTS idx_vigils_status ON vigils(status);
            CREATE TABLE IF NOT EXISTS vigil_verdicts (
                id         INTEGER PRIMARY KEY AUTOINCREMENT,
                vigil      TEXT NOT NULL,
                trigger    TEXT NOT NULL,
                verdict    TEXT NOT NULL,
                reason     TEXT,
                commands   TEXT,
                threshold  REAL,
                created_at TEXT NOT NULL DEFAULT (datetime('now'))
            );
             CREATE INDEX IF NOT EXISTS idx_vigil_verdicts_vigil ON vigil_verdicts(vigil);
             CREATE TABLE IF NOT EXISTS vigil_outcomes (
                 id         INTEGER PRIMARY KEY AUTOINCREMENT,
                 vigil      TEXT NOT NULL,
                 signal     TEXT,
                 outcome    TEXT NOT NULL,
                 useful     INTEGER NOT NULL DEFAULT 1,
                 created_at TEXT NOT NULL DEFAULT (datetime('now'))
             );
             CREATE INDEX IF NOT EXISTS idx_vigil_outcomes_vigil ON vigil_outcomes(vigil);",
        )
        .map_err(|e| format!("create vigils tables: {e}"))
    }

    pub fn upsert(&self, name: &str, payload_json: &str) -> Result<(), String> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO vigils (name, payload_json, status, updated_at)
             VALUES (?1, ?2, 'active', datetime('now'))
             ON CONFLICT(name) DO UPDATE SET
                 payload_json = excluded.payload_json,
                 status = 'active',
                 updated_at = datetime('now')",
            params![name, payload_json],
        )
        .map_err(|e| format!("upsert vigil {name}: {e}"))?;
        Ok(())
    }

    pub fn set_status(&self, name: &str, status: VigilStatus) -> Result<(), String> {
        let conn = self.conn.lock().unwrap();
        let affected = conn
            .execute(
                "UPDATE vigils SET status = ?1, updated_at = datetime('now') WHERE name = ?2",
                params![status.as_str(), name],
            )
            .map_err(|e| format!("set status for vigil {name}: {e}"))?;
        if affected == 0 {
            return Err(format!("vigil {name} not found"));
        }
        Ok(())
    }

    pub fn remove(&self, name: &str) -> Result<(), String> {
        let conn = self.conn.lock().unwrap();
        let affected = conn
            .execute("DELETE FROM vigils WHERE name = ?1", params![name])
            .map_err(|e| format!("remove vigil {name}: {e}"))?;
        if affected == 0 {
            return Err(format!("vigil {name} not found"));
        }
        Ok(())
    }

    /// Append a gate verdict. `verdict` is `shroud` | `rouse` | `toil`;
    /// `commands` is a JSON array of shell commands (only for `toil`).
    pub fn record_verdict(
        &self,
        vigil: &str,
        trigger: &str,
        verdict: &str,
        reason: Option<&str>,
        commands: Option<&str>,
        threshold: Option<f64>,
    ) -> Result<(), String> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO vigil_verdicts (vigil, trigger, verdict, reason, commands, threshold)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![vigil, trigger, verdict, reason, commands, threshold],
        )
        .map_err(|e| format!("record verdict for vigil {vigil}: {e}"))?;
        Ok(())
    }

    /// Append a finished observance's (signal, outcome) pair. `signal` is the
    /// coalesced event context that triggered the wake; `outcome` is a short
    /// token the plugin chose (`resolved`, `false-alarm`, ...); `useful` marks
    /// whether the wake was worth it and feeds the empirical prior. Only the
    /// plugin-gated `on-vigil-outcome` path calls this, so gate it the same
    /// way to stay warning-free in a `vigil`-without-`plugin` build.
    #[cfg(feature = "plugin")]
    pub fn record_outcome(
        &self,
        vigil: &str,
        signal: &str,
        outcome: &str,
        useful: bool,
    ) -> Result<(), String> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO vigil_outcomes (vigil, signal, outcome, useful)
             VALUES (?1, ?2, ?3, ?4)",
            params![vigil, signal, outcome, useful],
        )
        .map_err(|e| format!("record outcome for vigil {vigil}: {e}"))?;
        Ok(())
    }

    /// Empirical prior for `GateFail::Prior`: the fraction of a vigil's
    /// outcomes over the trailing seven days marked useful. `None` when the
    /// vigil has no recorded outcomes yet (uncalibrated, falls back to open).
    pub fn positive_rate(&self, vigil: &str) -> Result<Option<f64>, String> {
        let conn = self.conn.lock().unwrap();
        let (useful, total): (i64, i64) = conn
            .query_row(
                "SELECT COALESCE(SUM(useful), 0), COUNT(*)
                 FROM vigil_outcomes
                 WHERE vigil = ?1 AND created_at >= datetime('now', '-7 days')",
                params![vigil],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .map_err(|e| format!("positive rate for vigil {vigil}: {e}"))?;
        if total == 0 {
            return Ok(None);
        }
        Ok(Some(useful as f64 / total as f64))
    }

    /// Slice-2 keeper API: not yet called from this slice's CLI, so it would
    /// be dead code under `-D warnings`. Landed early so slice 2 can use it.
    #[allow(dead_code)]
    pub fn get(&self, name: &str) -> Result<Option<VigilRow>, String> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn
            .prepare(
                "SELECT name, payload_json, status, created_at, updated_at
                 FROM vigils WHERE name = ?1",
            )
            .map_err(|e| format!("prepare get vigil {name}: {e}"))?;
        let row = stmt
            .query_row(params![name], |row| {
                Ok(VigilRow {
                    name: row.get(0)?,
                    payload_json: row.get(1)?,
                    status: {
                        let s: String = row.get(2)?;
                        VigilStatus::from_db_str(&s)
                    },
                    created_at: row.get(3)?,
                    updated_at: row.get(4)?,
                })
            })
            .optional()
            .map_err(|e| format!("get vigil {name}: {e}"))?;
        Ok(row)
    }

    /// Slice-2 keeper API; landed early. See `get` above.
    #[allow(dead_code)]
    pub fn list_non_resting(&self) -> Result<Vec<VigilRow>, String> {
        self.query_rows(
            "SELECT name, payload_json, status, created_at, updated_at
             FROM vigils WHERE status != 'resting' ORDER BY name",
        )
    }

    /// All rows, including resting vigils. `dirge vigil list` needs the
    /// full picture so a vigil laid to rest still shows up with its status.
    pub fn list_all(&self) -> Result<Vec<VigilRow>, String> {
        self.query_rows(
            "SELECT name, payload_json, status, created_at, updated_at
             FROM vigils ORDER BY name",
        )
    }

    fn query_rows(&self, sql: &str) -> Result<Vec<VigilRow>, String> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn
            .prepare(sql)
            .map_err(|e| format!("prepare vigil list: {e}"))?;
        let rows: Vec<VigilRow> = stmt
            .query_map([], |row| {
                Ok(VigilRow {
                    name: row.get(0)?,
                    payload_json: row.get(1)?,
                    status: {
                        let s: String = row.get(2)?;
                        VigilStatus::from_db_str(&s)
                    },
                    created_at: row.get(3)?,
                    updated_at: row.get(4)?,
                })
            })
            .map_err(|e| format!("list vigils: {e}"))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| format!("read vigil row: {e}"))?;
        Ok(rows)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    static COUNTER: AtomicU32 = AtomicU32::new(0);

    /// Owns the scratch dir and removes it on drop, so a failed test does not
    /// leak temp directories.
    struct TempDb(std::path::PathBuf);

    impl Drop for TempDb {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn temp_db() -> (VigilStore, TempDb) {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir =
            std::env::temp_dir().join(format!("dirge-vigildb-test-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let store = VigilStore::open_at(&dir.join("state.db")).unwrap();
        (store, TempDb(dir))
    }

    #[test]
    fn upsert_then_get_roundtrips_payload() {
        let (store, _dir) = temp_db();
        store.upsert("poll", "{\"name\":\"poll\"}").unwrap();
        let row = store.get("poll").unwrap().expect("row exists");
        assert_eq!(row.name, "poll");
        assert_eq!(row.payload_json, "{\"name\":\"poll\"}");
        assert_eq!(row.status, VigilStatus::Active);
    }

    #[test]
    fn upsert_resets_status_to_active() {
        let (store, _dir) = temp_db();
        store.upsert("poll", "v1").unwrap();
        store.set_status("poll", VigilStatus::Paused).unwrap();
        store.upsert("poll", "v2").unwrap();
        let row = store.get("poll").unwrap().unwrap();
        assert_eq!(row.status, VigilStatus::Active);
        assert_eq!(row.payload_json, "v2");
    }

    #[test]
    fn list_non_resting_excludes_resting() {
        let (store, _dir) = temp_db();
        store.upsert("a", "1").unwrap();
        store.upsert("b", "2").unwrap();
        store.upsert("c", "3").unwrap();
        store.set_status("b", VigilStatus::Resting).unwrap();
        let names: Vec<String> = store
            .list_non_resting()
            .unwrap()
            .into_iter()
            .map(|r| r.name)
            .collect();
        assert_eq!(names, vec!["a", "c"]);
    }

    #[test]
    fn list_all_includes_resting() {
        let (store, _dir) = temp_db();
        store.upsert("a", "1").unwrap();
        store.upsert("b", "2").unwrap();
        store.set_status("b", VigilStatus::Resting).unwrap();
        let names: Vec<String> = store
            .list_all()
            .unwrap()
            .into_iter()
            .map(|r| r.name)
            .collect();
        assert_eq!(names, vec!["a", "b"]);
    }

    #[test]
    fn remove_deletes_row() {
        let (store, _dir) = temp_db();
        store.upsert("poll", "1").unwrap();
        store.remove("poll").unwrap();
        assert!(store.get("poll").unwrap().is_none());
    }

    #[test]
    fn status_and_remove_on_missing_name_error() {
        let (store, _dir) = temp_db();
        assert!(store.set_status("nope", VigilStatus::Paused).is_err());
        assert!(store.remove("nope").is_err());
    }

    #[cfg(feature = "plugin")]
    #[test]
    fn record_outcome_feeds_positive_rate() {
        let (store, _dir) = temp_db();
        assert_eq!(store.positive_rate("poll").unwrap(), None);
        store
            .record_outcome("poll", "{}", "resolved", true)
            .unwrap();
        store
            .record_outcome("poll", "{}", "false-alarm", false)
            .unwrap();
        store
            .record_outcome("poll", "{}", "resolved", true)
            .unwrap();
        let rate = store.positive_rate("poll").unwrap().unwrap();
        assert!((rate - 2.0 / 3.0).abs() < 1e-9);
    }

    #[test]
    fn record_verdict_persists_row() {
        let (store, _dir) = temp_db();
        store
            .record_verdict(
                "poll",
                "toll",
                "shroud",
                Some("low confidence"),
                None,
                Some(0.8),
            )
            .unwrap();
        let conn = store.conn.lock().unwrap();
        let (vigil, trigger, verdict, reason, threshold): (
            String,
            String,
            String,
            Option<String>,
            Option<f64>,
        ) = conn
            .query_row(
                "SELECT vigil, trigger, verdict, reason, threshold
                 FROM vigil_verdicts WHERE vigil = 'poll'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
            )
            .unwrap();
        assert_eq!(vigil, "poll");
        assert_eq!(trigger, "toll");
        assert_eq!(verdict, "shroud");
        assert_eq!(reason.as_deref(), Some("low confidence"));
        assert_eq!(threshold, Some(0.8));
    }
}
