//! Local SQLite data layer (Phase 1: foundation).
//!
//! The database is owned by the Rust side and lives in the Tauri app-data dir as
//! `gaze20.db`. It uses an explicit, ordered schema-migration chain so the schema
//! can evolve safely: each upgrade runs in its own transaction, the previous DB is
//! backed up before any upgrade, and a failed migration rolls back without touching
//! `schema_version` — old data is never overwritten on failure.
//!
//! Later phases add fact tables (activity/reminder/symptom events), aggregate tables
//! (daily/hourly stats), and a one-time import of the legacy JSON store.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Mutex;

use rusqlite::{params, Connection, Transaction};

/// Bump this when adding a migration below. The chain runs from the DB's current
/// version up to here, one transaction per step.
pub const SCHEMA_VERSION: i64 = 4;

/// Managed Tauri state: the single owned connection behind a mutex (rusqlite's
/// `Connection` is not `Sync`).
pub struct Database {
    pub conn: Mutex<Connection>,
}

/// Open (creating if needed) the database at `path`, apply pragmas, ensure the
/// meta table exists, and run any pending migrations. Returns the migrated
/// connection ready to be managed by Tauri.
pub fn open(path: &Path) -> Result<Connection, String> {
    let mut conn = Connection::open(path).map_err(|e| format!("open db: {e}"))?;

    // WAL for durability + concurrent readers; NORMAL sync is the right
    // durability/throughput trade-off for a local desktop app.
    conn.pragma_update(None, "journal_mode", "WAL")
        .map_err(|e| format!("pragma wal: {e}"))?;
    conn.pragma_update(None, "synchronous", "NORMAL")
        .map_err(|e| format!("pragma synchronous: {e}"))?;
    conn.pragma_update(None, "foreign_keys", "ON")
        .map_err(|e| format!("pragma foreign_keys: {e}"))?;

    ensure_meta(&conn)?;

    let current = current_version(&conn);
    if current < SCHEMA_VERSION {
        // Back up any existing populated DB before changing its shape.
        if current > 0 {
            backup_before_migrate(path, current)?;
        }
        for version in (current + 1)..=SCHEMA_VERSION {
            let tx = conn
                .transaction()
                .map_err(|e| format!("begin migration v{version}: {e}"))?;
            apply_migration(&tx, version)?;
            tx.execute(
                "UPDATE app_meta SET value = ?1 WHERE key = 'schema_version'",
                params![version.to_string()],
            )
            .map_err(|e| format!("bump schema_version to {version}: {e}"))?;
            tx.execute(
                "INSERT INTO app_meta(key, value) VALUES('last_migrated_at', datetime('now')) \
                 ON CONFLICT(key) DO UPDATE SET value = datetime('now')",
                [],
            )
            .map_err(|e| format!("stamp last_migrated_at: {e}"))?;
            tx.commit()
                .map_err(|e| format!("commit migration v{version}: {e}"))?;
        }
    }

    Ok(conn)
}

/// `app_meta` is a tiny key/value table holding the schema version and timestamps.
/// It is created outside the migration chain so the chain itself can record into it.
fn ensure_meta(conn: &Connection) -> Result<(), String> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS app_meta (
            key   TEXT PRIMARY KEY,
            value TEXT NOT NULL
        );",
    )
    .map_err(|e| format!("create app_meta: {e}"))?;
    conn.execute(
        "INSERT OR IGNORE INTO app_meta(key, value) VALUES('schema_version', '0')",
        [],
    )
    .map_err(|e| format!("seed schema_version: {e}"))?;
    conn.execute(
        "INSERT OR IGNORE INTO app_meta(key, value) VALUES('created_at', datetime('now'))",
        [],
    )
    .map_err(|e| format!("seed created_at: {e}"))?;
    Ok(())
}

fn current_version(conn: &Connection) -> i64 {
    conn.query_row(
        "SELECT value FROM app_meta WHERE key = 'schema_version'",
        [],
        |row| row.get::<_, String>(0),
    )
    .ok()
    .and_then(|s| s.parse::<i64>().ok())
    .unwrap_or(0)
}

/// Copy the existing DB aside before an upgrade so a botched migration is always
/// recoverable. WAL is checkpointed first so the copied file is self-contained.
fn backup_before_migrate(path: &Path, from_version: i64) -> Result<(), String> {
    // Best-effort checkpoint; ignore errors (a fresh DB may have no WAL yet).
    if let Ok(conn) = Connection::open(path) {
        let _ = conn.pragma_update(None, "wal_checkpoint", "TRUNCATE");
    }
    let backup = path.with_extension(format!("db.bak-v{from_version}"));
    std::fs::copy(path, &backup)
        .map_err(|e| format!("backup db to {}: {e}", backup.display()))?;
    Ok(())
}

/// Apply the schema for a single version. Add a new arm here and bump
/// `SCHEMA_VERSION` for every future change — never edit a shipped arm.
fn apply_migration(tx: &Transaction, version: i64) -> Result<(), String> {
    let sql = match version {
        1 => MIGRATION_V1,
        2 => MIGRATION_V2,
        3 => MIGRATION_V3,
        4 => MIGRATION_V4,
        other => return Err(format!("no migration defined for schema v{other}")),
    };
    tx.execute_batch(sql)
        .map_err(|e| format!("apply migration v{version}: {e}"))
}

/// V1 — user settings as a typed-by-convention key/value table. Runtime/state and
/// fact/aggregate tables arrive in later versions (V2+).
const MIGRATION_V1: &str = "
    CREATE TABLE settings (
        key        TEXT PRIMARY KEY,
        value      TEXT NOT NULL,
        updated_at TEXT NOT NULL DEFAULT (datetime('now'))
    );
";

/// V2 — fact tables. These are the source-of-truth event log; aggregate tables
/// (V3) are derived from them. Privacy: `activity_sessions` records the foreground
/// *process* for context but never the window *title*.
const MIGRATION_V2: &str = "
    CREATE TABLE activity_sessions (
        id             INTEGER PRIMARY KEY AUTOINCREMENT,
        started_at     TEXT NOT NULL,
        ended_at       TEXT,
        active_seconds REAL NOT NULL DEFAULT 0,
        reading        INTEGER NOT NULL DEFAULT 0,
        process        TEXT,
        created_at     TEXT NOT NULL DEFAULT (datetime('now'))
    );
    CREATE INDEX idx_activity_started ON activity_sessions(started_at);

    CREATE TABLE reminder_events (
        id             INTEGER PRIMARY KEY AUTOINCREMENT,
        at             TEXT NOT NULL,
        kind           TEXT NOT NULL,
        result         TEXT NOT NULL,
        active_seconds REAL NOT NULL DEFAULT 0,
        gaze_seconds   REAL NOT NULL DEFAULT 0,
        note           TEXT,
        created_at     TEXT NOT NULL DEFAULT (datetime('now'))
    );
    CREATE INDEX idx_reminder_at ON reminder_events(at);

    CREATE TABLE symptom_records (
        id             INTEGER PRIMARY KEY AUTOINCREMENT,
        at             TEXT NOT NULL,
        dry            INTEGER NOT NULL DEFAULT 0,
        blur           INTEGER NOT NULL DEFAULT 0,
        headache       INTEGER NOT NULL DEFAULT 0,
        neck           INTEGER NOT NULL DEFAULT 0,
        note           TEXT,
        screen_seconds REAL NOT NULL DEFAULT 0,
        created_at     TEXT NOT NULL DEFAULT (datetime('now'))
    );
    CREATE INDEX idx_symptom_at ON symptom_records(at);
";

/// V3 — aggregate tables the dashboard reads directly. `daily_stats` is one row
/// per day (today is upserted live; finished days are finalized at rollover).
/// `hourly_stats` buckets eye-use seconds per local hour for the heatmap.
const MIGRATION_V3: &str = "
    CREATE TABLE daily_stats (
        date                 TEXT PRIMARY KEY,
        screen_seconds       REAL    NOT NULL DEFAULT 0,
        distant_gaze_seconds REAL    NOT NULL DEFAULT 0,
        micro_due            INTEGER NOT NULL DEFAULT 0,
        micro_done           INTEGER NOT NULL DEFAULT 0,
        deep_due             INTEGER NOT NULL DEFAULT 0,
        deep_done            INTEGER NOT NULL DEFAULT 0,
        skipped              INTEGER NOT NULL DEFAULT 0,
        postponed            INTEGER NOT NULL DEFAULT 0,
        deferred             INTEGER NOT NULL DEFAULT 0,
        risk_score           INTEGER NOT NULL DEFAULT 0,
        risk_peak            INTEGER NOT NULL DEFAULT 0,
        updated_at           TEXT    NOT NULL DEFAULT (datetime('now','localtime'))
    );

    CREATE TABLE hourly_stats (
        date           TEXT    NOT NULL,
        hour           INTEGER NOT NULL,
        screen_seconds REAL    NOT NULL DEFAULT 0,
        PRIMARY KEY (date, hour)
    );
";

/// V4 — timezone-safe event timestamps. Adds `at_ms` (UTC epoch milliseconds) to the
/// fact tables alongside the legacy local-time text `at`, and back-fills it from the
/// existing rows (best-effort: the old local strings are interpreted in the machine's
/// current timezone). New writes set `at_ms` directly from a UTC clock; the UI reads
/// `at_ms` so timestamps stay correct across timezone changes / DST.
const MIGRATION_V4: &str = "
    ALTER TABLE reminder_events ADD COLUMN at_ms INTEGER;
    ALTER TABLE symptom_records ADD COLUMN at_ms INTEGER;
    UPDATE reminder_events
        SET at_ms = CAST(strftime('%s', at, 'utc') AS INTEGER) * 1000
        WHERE at_ms IS NULL AND at IS NOT NULL;
    UPDATE symptom_records
        SET at_ms = CAST(strftime('%s', at, 'utc') AS INTEGER) * 1000
        WHERE at_ms IS NULL AND at IS NOT NULL;
    CREATE INDEX idx_reminder_at_ms ON reminder_events(at_ms);
    CREATE INDEX idx_symptom_at_ms ON symptom_records(at_ms);
";

// ---- Settings access -------------------------------------------------------

pub fn get_all_settings(conn: &Connection) -> Result<BTreeMap<String, String>, String> {
    let mut stmt = conn
        .prepare("SELECT key, value FROM settings")
        .map_err(|e| format!("prepare get settings: {e}"))?;
    let rows = stmt
        .query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)))
        .map_err(|e| format!("query settings: {e}"))?;
    let mut map = BTreeMap::new();
    for row in rows {
        let (k, v) = row.map_err(|e| format!("read setting row: {e}"))?;
        map.insert(k, v);
    }
    Ok(map)
}

pub fn set_setting(conn: &Connection, key: &str, value: &str) -> Result<(), String> {
    conn.execute(
        "INSERT INTO settings(key, value, updated_at) VALUES(?1, ?2, datetime('now')) \
         ON CONFLICT(key) DO UPDATE SET value = excluded.value, updated_at = datetime('now')",
        params![key, value],
    )
    .map_err(|e| format!("set setting {key}: {e}"))?;
    Ok(())
}

pub fn get_setting(conn: &Connection, key: &str) -> Option<String> {
    conn.query_row(
        "SELECT value FROM settings WHERE key = ?1",
        params![key],
        |row| row.get::<_, String>(0),
    )
    .ok()
}

/// Today's date in the user's local timezone, as `YYYY-MM-DD` (via SQLite, so no
/// chrono dependency). Used for day-rollover.
pub fn today_local(conn: &Connection) -> String {
    conn.query_row("SELECT date('now','localtime')", [], |row| {
        row.get::<_, String>(0)
    })
    .unwrap_or_default()
}

pub fn schema_version(conn: &Connection) -> i64 {
    current_version(conn)
}

// ---- Fact tables: reminder events -----------------------------------------

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReminderEventRow {
    pub id: i64,
    pub at: String,
    pub kind: String,
    pub result: String,
    pub active_seconds: f64,
    pub gaze_seconds: f64,
    pub note: Option<String>,
    /// UTC epoch milliseconds (timezone-safe); `None` only for un-back-fillable rows.
    pub at_ms: Option<i64>,
}

/// Record one reminder lifecycle event. `at` defaults to now when `None` (live
/// recording); an explicit timestamp is used by the legacy-JSON import (Phase 4).
#[allow(clippy::too_many_arguments)]
pub fn insert_reminder_event(
    conn: &Connection,
    at: Option<&str>,
    at_ms: Option<i64>,
    kind: &str,
    result: &str,
    active_seconds: f64,
    gaze_seconds: f64,
    note: Option<&str>,
) -> Result<i64, String> {
    // Live callers pass `at_ms` (a UTC clock); the legacy import passes `None` and the
    // local `at` string is converted to a UTC epoch here (best-effort by machine tz).
    conn.execute(
        "INSERT INTO reminder_events(at, at_ms, kind, result, active_seconds, gaze_seconds, note) \
         VALUES(COALESCE(?1, datetime('now','localtime')), \
                COALESCE(?2, CAST(strftime('%s', ?1, 'utc') AS INTEGER) * 1000), \
                ?3, ?4, ?5, ?6, ?7)",
        params![at, at_ms, kind, result, active_seconds, gaze_seconds, note],
    )
    .map_err(|e| format!("insert reminder_event: {e}"))?;
    Ok(conn.last_insert_rowid())
}

pub fn recent_reminder_events(
    conn: &Connection,
    limit: i64,
) -> Result<Vec<ReminderEventRow>, String> {
    let mut stmt = conn
        .prepare(
            "SELECT id, at, kind, result, active_seconds, gaze_seconds, note, at_ms \
             FROM reminder_events ORDER BY at_ms DESC, id DESC LIMIT ?1",
        )
        .map_err(|e| format!("prepare recent reminders: {e}"))?;
    let rows = stmt
        .query_map([limit], |row| {
            Ok(ReminderEventRow {
                id: row.get(0)?,
                at: row.get(1)?,
                kind: row.get(2)?,
                result: row.get(3)?,
                active_seconds: row.get(4)?,
                gaze_seconds: row.get(5)?,
                note: row.get(6)?,
                at_ms: row.get(7)?,
            })
        })
        .map_err(|e| format!("query recent reminders: {e}"))?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("read reminder rows: {e}"))
}

// ---- Fact tables: symptom records -----------------------------------------

#[derive(serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SymptomRow {
    pub id: i64,
    pub at: String,
    pub dry: i64,
    pub blur: i64,
    pub headache: i64,
    pub neck: i64,
    pub note: Option<String>,
    pub screen_seconds: f64,
    /// UTC epoch milliseconds (timezone-safe); `None` only for un-back-fillable rows.
    pub at_ms: Option<i64>,
}

#[allow(clippy::too_many_arguments)]
pub fn insert_symptom(
    conn: &Connection,
    at: Option<&str>,
    at_ms: Option<i64>,
    dry: i64,
    blur: i64,
    headache: i64,
    neck: i64,
    note: Option<&str>,
    screen_seconds: f64,
) -> Result<i64, String> {
    conn.execute(
        "INSERT INTO symptom_records(at, at_ms, dry, blur, headache, neck, note, screen_seconds) \
         VALUES(COALESCE(?1, datetime('now','localtime')), \
                COALESCE(?2, CAST(strftime('%s', ?1, 'utc') AS INTEGER) * 1000), \
                ?3, ?4, ?5, ?6, ?7, ?8)",
        params![at, at_ms, dry, blur, headache, neck, note, screen_seconds],
    )
    .map_err(|e| format!("insert symptom: {e}"))?;
    Ok(conn.last_insert_rowid())
}

pub fn recent_symptoms(conn: &Connection, limit: i64) -> Result<Vec<SymptomRow>, String> {
    let mut stmt = conn
        .prepare(
            "SELECT id, at, dry, blur, headache, neck, note, screen_seconds, at_ms \
             FROM symptom_records ORDER BY at_ms DESC, id DESC LIMIT ?1",
        )
        .map_err(|e| format!("prepare recent symptoms: {e}"))?;
    let rows = stmt
        .query_map([limit], |row| {
            Ok(SymptomRow {
                id: row.get(0)?,
                at: row.get(1)?,
                dry: row.get(2)?,
                blur: row.get(3)?,
                headache: row.get(4)?,
                neck: row.get(5)?,
                note: row.get(6)?,
                screen_seconds: row.get(7)?,
                at_ms: row.get(8)?,
            })
        })
        .map_err(|e| format!("query recent symptoms: {e}"))?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("read symptom rows: {e}"))
}

// ---- Fact tables: activity sessions ---------------------------------------

/// Open a new activity interval and return its id. The engine bumps and closes it
/// as real eye-use time accrues. Wired for the hourly heatmap in Phase 3.
#[allow(dead_code)]
pub fn open_activity_session(
    conn: &Connection,
    started_at: Option<&str>,
    process: Option<&str>,
    reading: bool,
) -> Result<i64, String> {
    conn.execute(
        "INSERT INTO activity_sessions(started_at, process, reading, active_seconds) \
         VALUES(COALESCE(?1, datetime('now','localtime')), ?2, ?3, 0)",
        params![started_at, process, reading as i64],
    )
    .map_err(|e| format!("open activity_session: {e}"))?;
    Ok(conn.last_insert_rowid())
}

/// Update an open session's accrued seconds and (optionally) mark it ended.
#[allow(dead_code)]
pub fn update_activity_session(
    conn: &Connection,
    id: i64,
    active_seconds: f64,
    ended: bool,
) -> Result<(), String> {
    if ended {
        conn.execute(
            "UPDATE activity_sessions SET active_seconds = ?2, ended_at = datetime('now','localtime') WHERE id = ?1",
            params![id, active_seconds],
        )
    } else {
        conn.execute(
            "UPDATE activity_sessions SET active_seconds = ?2 WHERE id = ?1",
            params![id, active_seconds],
        )
    }
    .map_err(|e| format!("update activity_session {id}: {e}"))?;
    Ok(())
}

// ---- Aggregate tables (V3): daily_stats + hourly_stats ---------------------

/// Today's local hour (0-23) via SQLite, so no chrono dependency.
pub fn local_hour(conn: &Connection) -> i64 {
    conn.query_row(
        "SELECT CAST(strftime('%H','now','localtime') AS INTEGER)",
        [],
        |row| row.get::<_, i64>(0),
    )
    .unwrap_or(0)
}

/// One day's aggregate snapshot, taken from the engine's counters.
pub struct DailyAgg<'a> {
    pub date: &'a str,
    pub screen_seconds: f64,
    pub distant_gaze_seconds: f64,
    pub micro_due: i64,
    pub micro_done: i64,
    pub deep_due: i64,
    pub deep_done: i64,
    pub skipped: i64,
    pub postponed: i64,
    pub deferred: i64,
    pub risk_score: i64,
}

/// Upsert a day's row. `risk_peak` is the running max of `risk_score` seen.
pub fn upsert_daily_stats(conn: &Connection, a: &DailyAgg) -> Result<(), String> {
    conn.execute(
        "INSERT INTO daily_stats(date, screen_seconds, distant_gaze_seconds, micro_due, \
             micro_done, deep_due, deep_done, skipped, postponed, deferred, risk_score, \
             risk_peak, updated_at) \
         VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?11, datetime('now','localtime')) \
         ON CONFLICT(date) DO UPDATE SET \
             screen_seconds = excluded.screen_seconds, \
             distant_gaze_seconds = excluded.distant_gaze_seconds, \
             micro_due = excluded.micro_due, micro_done = excluded.micro_done, \
             deep_due = excluded.deep_due, deep_done = excluded.deep_done, \
             skipped = excluded.skipped, postponed = excluded.postponed, \
             deferred = excluded.deferred, risk_score = excluded.risk_score, \
             risk_peak = MAX(daily_stats.risk_peak, excluded.risk_score), \
             updated_at = excluded.updated_at",
        params![
            a.date,
            a.screen_seconds,
            a.distant_gaze_seconds,
            a.micro_due,
            a.micro_done,
            a.deep_due,
            a.deep_done,
            a.skipped,
            a.postponed,
            a.deferred,
            a.risk_score
        ],
    )
    .map_err(|e| format!("upsert daily_stats: {e}"))?;
    Ok(())
}

/// Add eye-use seconds to one (date, hour) heatmap bucket.
pub fn add_hourly_seconds(conn: &Connection, date: &str, hour: i64, delta: f64) -> Result<(), String> {
    conn.execute(
        "INSERT INTO hourly_stats(date, hour, screen_seconds) VALUES(?1, ?2, ?3) \
         ON CONFLICT(date, hour) DO UPDATE SET screen_seconds = screen_seconds + excluded.screen_seconds",
        params![date, hour, delta],
    )
    .map_err(|e| format!("add hourly_stats: {e}"))?;
    Ok(())
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DailyStatsRow {
    pub date: String,
    pub screen_seconds: f64,
    pub distant_gaze_seconds: f64,
    pub micro_due: i64,
    pub micro_done: i64,
    pub deep_due: i64,
    pub deep_done: i64,
    pub skipped: i64,
    pub postponed: i64,
    pub deferred: i64,
    pub risk_score: i64,
    pub risk_peak: i64,
}

/// Most-recent `limit` days, newest first.
pub fn query_daily_stats(conn: &Connection, limit: i64) -> Result<Vec<DailyStatsRow>, String> {
    let mut stmt = conn
        .prepare(
            "SELECT date, screen_seconds, distant_gaze_seconds, micro_due, micro_done, \
                    deep_due, deep_done, skipped, postponed, deferred, risk_score, risk_peak \
             FROM daily_stats ORDER BY date DESC LIMIT ?1",
        )
        .map_err(|e| format!("prepare daily_stats: {e}"))?;
    let rows = stmt
        .query_map([limit], |row| {
            Ok(DailyStatsRow {
                date: row.get(0)?,
                screen_seconds: row.get(1)?,
                distant_gaze_seconds: row.get(2)?,
                micro_due: row.get(3)?,
                micro_done: row.get(4)?,
                deep_due: row.get(5)?,
                deep_done: row.get(6)?,
                skipped: row.get(7)?,
                postponed: row.get(8)?,
                deferred: row.get(9)?,
                risk_score: row.get(10)?,
                risk_peak: row.get(11)?,
            })
        })
        .map_err(|e| format!("query daily_stats: {e}"))?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("read daily_stats rows: {e}"))
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HourlyStatsRow {
    pub date: String,
    pub hour: i64,
    pub screen_seconds: f64,
}

/// All hourly buckets on or after `since_date` (YYYY-MM-DD), ordered.
pub fn query_hourly_stats(conn: &Connection, since_date: &str) -> Result<Vec<HourlyStatsRow>, String> {
    let mut stmt = conn
        .prepare(
            "SELECT date, hour, screen_seconds FROM hourly_stats \
             WHERE date >= ?1 ORDER BY date, hour",
        )
        .map_err(|e| format!("prepare hourly_stats: {e}"))?;
    let rows = stmt
        .query_map([since_date], |row| {
            Ok(HourlyStatsRow {
                date: row.get(0)?,
                hour: row.get(1)?,
                screen_seconds: row.get(2)?,
            })
        })
        .map_err(|e| format!("query hourly_stats: {e}"))?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("read hourly_stats rows: {e}"))
}

// ---- Reliability (V6): backup, recovery, retention -------------------------

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Refresh the rolling daily backup (`<db>.backup.db`) if it is missing or older
/// than ~24h. WAL is checkpointed first so the copy is self-contained.
pub fn daily_backup(path: &Path) {
    let backup = path.with_extension("backup.db");
    let stale = std::fs::metadata(&backup)
        .and_then(|m| m.modified())
        .map(|t| t.elapsed().map(|d| d.as_secs() > 86_400).unwrap_or(true))
        .unwrap_or(true);
    if stale {
        if let Ok(conn) = Connection::open(path) {
            let _ = conn.pragma_update(None, "wal_checkpoint", "TRUNCATE");
        }
        let _ = std::fs::copy(path, &backup);
    }
}

/// Recover from an unreadable main DB: restore the rolling backup if present and
/// usable; otherwise move the corrupt file aside (timestamped) and recreate fresh.
/// Always returns a usable, migrated connection.
pub fn recover_and_open(path: &Path) -> Result<Connection, String> {
    let backup = path.with_extension("backup.db");
    if backup.exists() && std::fs::copy(&backup, path).is_ok() {
        if let Ok(conn) = open(path) {
            return Ok(conn);
        }
    }
    let corrupt = path.with_extension(format!("corrupt-{}.db", now_secs()));
    let _ = std::fs::rename(path, &corrupt);
    let _ = std::fs::remove_file(path.with_extension("db-wal"));
    let _ = std::fs::remove_file(path.with_extension("db-shm"));
    open(path)
}

/// Retention: drop fine-grained events older than `days` (the long-term
/// aggregate `daily_stats` and user-entered `symptom_records` are kept).
pub fn prune_old(conn: &Connection, days: i64) -> Result<(), String> {
    let cutoff: String = conn
        .query_row(
            "SELECT date('now','localtime', ?1)",
            [format!("-{} days", days.max(1))],
            |row| row.get(0),
        )
        .unwrap_or_default();
    if cutoff.is_empty() {
        return Ok(());
    }
    let _ = conn.execute("DELETE FROM reminder_events WHERE at < ?1", params![cutoff]);
    let _ = conn.execute("DELETE FROM hourly_stats WHERE date < ?1", params![cutoff]);
    Ok(())
}

/// Build a complete, privacy-safe data export (aggregates + facts, no window
/// titles, no internal engine blob) as pretty JSON.
pub fn export_json(conn: &Connection) -> Result<String, String> {
    let daily = query_daily_stats(conn, 3650)?;
    let reminders = recent_reminder_events(conn, 1_000_000)?;
    let symptoms = recent_symptoms(conn, 1_000_000)?;
    let payload = serde_json::json!({
        "exportedAt": today_local(conn),
        "schemaVersion": current_version(conn),
        "dailyStats": daily,
        "reminderEvents": reminders,
        "symptoms": symptoms,
    });
    serde_json::to_string_pretty(&payload).map_err(|e| format!("serialize export: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    fn temp_path() -> std::path::PathBuf {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        std::env::temp_dir().join(format!("gaze20-test-{}-{}.db", std::process::id(), n))
    }

    fn cleanup(path: &Path) {
        let _ = std::fs::remove_file(path);
        let _ = std::fs::remove_file(path.with_extension("db-wal"));
        let _ = std::fs::remove_file(path.with_extension("db-shm"));
        let _ = std::fs::remove_file(path.with_extension("backup.db"));
    }

    fn agg(date: &str, risk: i64) -> DailyAgg<'_> {
        DailyAgg {
            date,
            screen_seconds: 100.0,
            distant_gaze_seconds: 0.0,
            micro_due: 1,
            micro_done: 1,
            deep_due: 0,
            deep_done: 0,
            skipped: 0,
            postponed: 0,
            deferred: 0,
            risk_score: risk,
        }
    }

    #[test]
    fn fresh_open_migrates_to_latest() {
        let path = temp_path();
        let conn = open(&path).unwrap();
        assert_eq!(schema_version(&conn), SCHEMA_VERSION);
        for t in [
            "app_meta",
            "settings",
            "activity_sessions",
            "reminder_events",
            "symptom_records",
            "daily_stats",
            "hourly_stats",
        ] {
            let n: i64 = conn
                .query_row(
                    "SELECT count(*) FROM sqlite_master WHERE type='table' AND name=?1",
                    [t],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(n, 1, "table {t} should exist");
        }
        drop(conn);
        cleanup(&path);
    }

    #[test]
    fn settings_roundtrip_upsert() {
        let path = temp_path();
        let conn = open(&path).unwrap();
        set_setting(&conn, "mode", "intense").unwrap();
        set_setting(&conn, "mode", "balanced").unwrap();
        assert_eq!(get_setting(&conn, "mode").as_deref(), Some("balanced"));
        assert_eq!(
            get_all_settings(&conn).unwrap().get("mode").map(String::as_str),
            Some("balanced")
        );
        drop(conn);
        cleanup(&path);
    }

    #[test]
    fn fact_inserts_and_queries() {
        let path = temp_path();
        let conn = open(&path).unwrap();
        insert_reminder_event(&conn, Some("2026-06-16 10:00:00"), None, "micro", "completed", 1200.0, 20.0, Some("ok")).unwrap();
        insert_symptom(&conn, Some("2026-06-16 10:00:00"), None, 3, 2, 1, 0, None, 1200.0).unwrap();
        assert_eq!(recent_reminder_events(&conn, 10).unwrap().len(), 1);
        let s = recent_symptoms(&conn, 10).unwrap();
        assert_eq!(s.len(), 1);
        assert_eq!(s[0].dry, 3);
        drop(conn);
        cleanup(&path);
    }

    #[test]
    fn daily_upsert_tracks_peak() {
        let path = temp_path();
        let conn = open(&path).unwrap();
        upsert_daily_stats(&conn, &agg("2026-06-16", 30)).unwrap();
        upsert_daily_stats(&conn, &agg("2026-06-16", 50)).unwrap();
        upsert_daily_stats(&conn, &agg("2026-06-16", 40)).unwrap();
        let rows = query_daily_stats(&conn, 10).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].risk_score, 40);
        assert_eq!(rows[0].risk_peak, 50, "risk_peak should track the running max");
        drop(conn);
        cleanup(&path);
    }

    #[test]
    fn hourly_accumulates_in_bucket() {
        let path = temp_path();
        let conn = open(&path).unwrap();
        add_hourly_seconds(&conn, "2026-06-16", 14, 100.0).unwrap();
        add_hourly_seconds(&conn, "2026-06-16", 14, 50.0).unwrap();
        let rows = query_hourly_stats(&conn, "2026-06-16").unwrap();
        assert_eq!(rows.len(), 1);
        assert!((rows[0].screen_seconds - 150.0).abs() < 1e-9);
        drop(conn);
        cleanup(&path);
    }

    #[test]
    fn prune_drops_old_keeps_recent() {
        let path = temp_path();
        let conn = open(&path).unwrap();
        insert_reminder_event(&conn, Some("2000-01-01 00:00:00"), None, "micro", "completed", 0.0, 0.0, None).unwrap();
        insert_reminder_event(&conn, None, None, "micro", "completed", 0.0, 0.0, None).unwrap();
        add_hourly_seconds(&conn, "2000-01-01", 9, 100.0).unwrap();
        prune_old(&conn, 90).unwrap();
        assert_eq!(recent_reminder_events(&conn, 10).unwrap().len(), 1, "ancient event pruned");
        assert!(query_hourly_stats(&conn, "2000-01-01").unwrap().is_empty(), "ancient hourly pruned");
        drop(conn);
        cleanup(&path);
    }

    #[test]
    fn at_ms_set_on_live_and_backfilled_on_legacy() {
        let path = temp_path();
        let conn = open(&path).unwrap();
        // Live path: an explicit UTC epoch is stored verbatim.
        insert_reminder_event(&conn, None, Some(1_700_000_000_000), "micro", "completed", 0.0, 0.0, None).unwrap();
        // Legacy path: a local `at` string is converted to a UTC epoch (best-effort).
        insert_reminder_event(&conn, Some("2026-06-16 10:00:00"), None, "deep", "completed", 0.0, 0.0, None).unwrap();
        insert_symptom(&conn, Some("2026-06-16 10:00:00"), None, 1, 0, 0, 0, None, 0.0).unwrap();

        let rows = recent_reminder_events(&conn, 10).unwrap();
        assert_eq!(rows.len(), 2);
        assert!(rows.iter().all(|r| r.at_ms.is_some()), "every reminder row has at_ms");
        assert!(rows.iter().any(|r| r.at_ms == Some(1_700_000_000_000)), "live epoch stored verbatim");

        let syms = recent_symptoms(&conn, 10).unwrap();
        assert_eq!(syms.len(), 1);
        assert!(syms[0].at_ms.is_some(), "symptom at_ms back-filled from legacy string");
        drop(conn);
        cleanup(&path);
    }

    #[test]
    fn recover_from_corrupt_db() {
        let path = temp_path();
        std::fs::write(&path, b"this is definitely not a sqlite database").unwrap();
        assert!(open(&path).is_err(), "garbage file should not open as a DB");
        let conn = recover_and_open(&path).unwrap();
        assert_eq!(schema_version(&conn), SCHEMA_VERSION, "recovery yields a fresh, migrated DB");
        drop(conn);
        cleanup(&path);
        if let Ok(dir) = std::fs::read_dir(std::env::temp_dir()) {
            for entry in dir.flatten() {
                let name = entry.file_name().to_string_lossy().into_owned();
                if name.starts_with("gaze20-test-") && name.contains("corrupt-") {
                    let _ = std::fs::remove_file(entry.path());
                }
            }
        }
    }
}
