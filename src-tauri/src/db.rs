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
pub const SCHEMA_VERSION: i64 = 6;

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
        5 => MIGRATION_V5,
        6 => MIGRATION_V6,
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

/// V5 — the raw activity fact layer is finally wired up. The V2 `activity_sessions`
/// table was designed but never written, so it is safe to drop and recreate with a
/// proper, timezone-safe, analysis-ready shape: one row per contiguous span of real
/// eye-use with the same foreground process + activity state. `started_ms`/`ended_ms`
/// are UTC epoch milliseconds; `date`/`hour` are the local-day/local-hour the session
/// started in (cheap grouping keys). `state` is `active` / `reading` / `meeting`, and
/// `process` enables future per-app analysis. Privacy unchanged: process name only.
const MIGRATION_V5: &str = "
    DROP TABLE IF EXISTS activity_sessions;
    CREATE TABLE activity_sessions (
        id             INTEGER PRIMARY KEY AUTOINCREMENT,
        started_ms     INTEGER NOT NULL,
        ended_ms       INTEGER,
        active_seconds REAL    NOT NULL DEFAULT 0,
        state          TEXT    NOT NULL DEFAULT 'active',
        process        TEXT,
        date           TEXT    NOT NULL,
        hour           INTEGER NOT NULL
    );
    CREATE INDEX idx_activity_started_ms ON activity_sessions(started_ms);
    CREATE INDEX idx_activity_date ON activity_sessions(date);
";

/// V6 — product-grade data context. Keep the existing fact/aggregate tables, but
/// add the metadata future versions need to interpret old data: risk-model
/// snapshots, local timezone offsets, richer activity explanations, and a stable
/// reminder lifecycle table linked by `session_uid`.
const MIGRATION_V6: &str = "
    ALTER TABLE daily_stats ADD COLUMN metric_version INTEGER NOT NULL DEFAULT 1;
    ALTER TABLE daily_stats ADD COLUMN mode TEXT;
    ALTER TABLE daily_stats ADD COLUMN micro_minutes REAL;
    ALTER TABLE daily_stats ADD COLUMN deep_minutes REAL;
    ALTER TABLE daily_stats ADD COLUMN break_seconds REAL;
    ALTER TABLE daily_stats ADD COLUMN deep_break_minutes REAL;
    ALTER TABLE daily_stats ADD COLUMN risk_components_json TEXT;
    ALTER TABLE daily_stats ADD COLUMN timezone_offset_minutes INTEGER;

    ALTER TABLE hourly_stats ADD COLUMN timezone_offset_minutes INTEGER;

    ALTER TABLE reminder_events ADD COLUMN session_uid TEXT;
    ALTER TABLE reminder_events ADD COLUMN due_ms INTEGER;
    ALTER TABLE reminder_events ADD COLUMN shown_ms INTEGER;
    ALTER TABLE reminder_events ADD COLUMN resolved_ms INTEGER;
    ALTER TABLE reminder_events ADD COLUMN source TEXT NOT NULL DEFAULT 'legacy';
    ALTER TABLE reminder_events ADD COLUMN focus_minutes INTEGER;
    ALTER TABLE reminder_events ADD COLUMN target_seconds INTEGER;
    ALTER TABLE reminder_events ADD COLUMN timezone_offset_minutes INTEGER;
    CREATE INDEX idx_reminder_session_uid ON reminder_events(session_uid);

    CREATE TABLE reminder_sessions (
        uid                     TEXT PRIMARY KEY,
        kind                    TEXT NOT NULL,
        source                  TEXT NOT NULL,
        due_ms                  INTEGER,
        shown_ms                INTEGER NOT NULL,
        resolved_ms             INTEGER,
        result                  TEXT,
        target_seconds          INTEGER NOT NULL,
        focus_minutes           INTEGER,
        screen_seconds_at_show  REAL NOT NULL DEFAULT 0,
        screen_seconds_at_end   REAL,
        gaze_seconds            REAL NOT NULL DEFAULT 0,
        timezone_offset_minutes INTEGER,
        created_at              TEXT NOT NULL DEFAULT (datetime('now'))
    );
    CREATE INDEX idx_reminder_sessions_shown ON reminder_sessions(shown_ms);

    ALTER TABLE symptom_records ADD COLUMN timezone_offset_minutes INTEGER;

    ALTER TABLE activity_sessions ADD COLUMN eye_activity_weight REAL NOT NULL DEFAULT 1.0;
    ALTER TABLE activity_sessions ADD COLUMN should_defer INTEGER NOT NULL DEFAULT 0;
    ALTER TABLE activity_sessions ADD COLUMN is_fullscreen INTEGER NOT NULL DEFAULT 0;
    ALTER TABLE activity_sessions ADD COLUMN reason TEXT;
    ALTER TABLE activity_sessions ADD COLUMN timezone_offset_minutes INTEGER;
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

/// Current local timezone offset from UTC, in minutes. Stored with local-day/hour
/// buckets so history remains interpretable if the user later changes timezone.
pub fn timezone_offset_minutes(conn: &Connection) -> i64 {
    conn.query_row(
        "SELECT CAST(round((julianday('now','localtime') - julianday('now')) * 1440) AS INTEGER)",
        [],
        |row| row.get::<_, i64>(0),
    )
    .unwrap_or(0)
}

pub fn schema_version(conn: &Connection) -> i64 {
    current_version(conn)
}

// ---- Fact tables: reminder events -----------------------------------------

#[derive(serde::Serialize, serde::Deserialize)]
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
    pub session_uid: Option<String>,
    pub due_ms: Option<i64>,
    pub shown_ms: Option<i64>,
    pub resolved_ms: Option<i64>,
    pub source: Option<String>,
    pub focus_minutes: Option<i64>,
    pub target_seconds: Option<i64>,
    pub timezone_offset_minutes: Option<i64>,
}

#[derive(Default)]
pub struct ReminderEventExtra<'a> {
    pub session_uid: Option<&'a str>,
    pub due_ms: Option<i64>,
    pub shown_ms: Option<i64>,
    pub resolved_ms: Option<i64>,
    pub source: Option<&'a str>,
    pub focus_minutes: Option<i64>,
    pub target_seconds: Option<i64>,
    pub timezone_offset_minutes: Option<i64>,
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
    insert_reminder_event_detailed(
        conn,
        at,
        at_ms,
        kind,
        result,
        active_seconds,
        gaze_seconds,
        note,
        ReminderEventExtra::default(),
    )
}

/// Detailed reminder event insert used by the live engine. The extra lifecycle
/// fields are optional so legacy imports can still use the compact wrapper above.
#[allow(clippy::too_many_arguments)]
pub fn insert_reminder_event_detailed(
    conn: &Connection,
    at: Option<&str>,
    at_ms: Option<i64>,
    kind: &str,
    result: &str,
    active_seconds: f64,
    gaze_seconds: f64,
    note: Option<&str>,
    extra: ReminderEventExtra<'_>,
) -> Result<i64, String> {
    // Live callers pass `at_ms` (a UTC clock); the legacy import passes `None` and the
    // local `at` string is converted to a UTC epoch here (best-effort by machine tz).
    let tz = extra
        .timezone_offset_minutes
        .unwrap_or_else(|| timezone_offset_minutes(conn));
    conn.execute(
        "INSERT INTO reminder_events(at, at_ms, kind, result, active_seconds, gaze_seconds, note, \
            session_uid, due_ms, shown_ms, resolved_ms, source, focus_minutes, target_seconds, \
            timezone_offset_minutes) \
         VALUES(COALESCE(?1, datetime('now','localtime')), \
                COALESCE(?2, CAST(strftime('%s', ?1, 'utc') AS INTEGER) * 1000), \
                ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, COALESCE(?12, 'live'), ?13, ?14, ?15)",
        params![
            at,
            at_ms,
            kind,
            result,
            active_seconds,
            gaze_seconds,
            note,
            extra.session_uid,
            extra.due_ms,
            extra.shown_ms,
            extra.resolved_ms,
            extra.source,
            extra.focus_minutes,
            extra.target_seconds,
            tz
        ],
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
            "SELECT id, at, kind, result, active_seconds, gaze_seconds, note, at_ms, \
                    session_uid, due_ms, shown_ms, resolved_ms, source, focus_minutes, \
                    target_seconds, timezone_offset_minutes \
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
                session_uid: row.get(8)?,
                due_ms: row.get(9)?,
                shown_ms: row.get(10)?,
                resolved_ms: row.get(11)?,
                source: row.get(12)?,
                focus_minutes: row.get(13)?,
                target_seconds: row.get(14)?,
                timezone_offset_minutes: row.get(15)?,
            })
        })
        .map_err(|e| format!("query recent reminders: {e}"))?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("read reminder rows: {e}"))
}

#[derive(serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReminderSessionRow {
    pub uid: String,
    pub kind: String,
    pub source: String,
    pub due_ms: Option<i64>,
    pub shown_ms: i64,
    pub resolved_ms: Option<i64>,
    pub result: Option<String>,
    pub target_seconds: i64,
    pub focus_minutes: Option<i64>,
    pub screen_seconds_at_show: f64,
    pub screen_seconds_at_end: Option<f64>,
    pub gaze_seconds: f64,
    pub timezone_offset_minutes: Option<i64>,
}

#[allow(clippy::too_many_arguments)]
pub fn open_reminder_session(
    conn: &Connection,
    kind: &str,
    source: &str,
    due_ms: Option<i64>,
    shown_ms: i64,
    target_seconds: i64,
    focus_minutes: Option<i64>,
    screen_seconds_at_show: f64,
) -> Result<String, String> {
    let uid = format!("reminder:{shown_ms}:{source}:{kind}");
    conn.execute(
        "INSERT OR IGNORE INTO reminder_sessions(uid, kind, source, due_ms, shown_ms, \
             target_seconds, focus_minutes, screen_seconds_at_show, timezone_offset_minutes) \
         VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        params![
            uid,
            kind,
            source,
            due_ms,
            shown_ms,
            target_seconds,
            focus_minutes,
            screen_seconds_at_show,
            timezone_offset_minutes(conn)
        ],
    )
    .map_err(|e| format!("open reminder_session: {e}"))?;
    Ok(uid)
}

pub fn resolve_reminder_session(
    conn: &Connection,
    uid: &str,
    result: &str,
    resolved_ms: i64,
    screen_seconds_at_end: f64,
    gaze_seconds: f64,
) -> Result<(), String> {
    conn.execute(
        "UPDATE reminder_sessions SET result = ?2, resolved_ms = ?3, \
             screen_seconds_at_end = ?4, gaze_seconds = ?5 WHERE uid = ?1",
        params![uid, result, resolved_ms, screen_seconds_at_end, gaze_seconds],
    )
    .map_err(|e| format!("resolve reminder_session {uid}: {e}"))?;
    Ok(())
}

pub fn recent_reminder_sessions(
    conn: &Connection,
    limit: i64,
) -> Result<Vec<ReminderSessionRow>, String> {
    let mut stmt = conn
        .prepare(
            "SELECT uid, kind, source, due_ms, shown_ms, resolved_ms, result, target_seconds, \
                    focus_minutes, screen_seconds_at_show, screen_seconds_at_end, gaze_seconds, \
                    timezone_offset_minutes \
             FROM reminder_sessions ORDER BY shown_ms DESC LIMIT ?1",
        )
        .map_err(|e| format!("prepare reminder_sessions: {e}"))?;
    let rows = stmt
        .query_map([limit], |row| {
            Ok(ReminderSessionRow {
                uid: row.get(0)?,
                kind: row.get(1)?,
                source: row.get(2)?,
                due_ms: row.get(3)?,
                shown_ms: row.get(4)?,
                resolved_ms: row.get(5)?,
                result: row.get(6)?,
                target_seconds: row.get(7)?,
                focus_minutes: row.get(8)?,
                screen_seconds_at_show: row.get(9)?,
                screen_seconds_at_end: row.get(10)?,
                gaze_seconds: row.get(11)?,
                timezone_offset_minutes: row.get(12)?,
            })
        })
        .map_err(|e| format!("query reminder_sessions: {e}"))?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("read reminder_session rows: {e}"))
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
    pub timezone_offset_minutes: Option<i64>,
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
        "INSERT INTO symptom_records(at, at_ms, dry, blur, headache, neck, note, screen_seconds, \
            timezone_offset_minutes) \
         VALUES(COALESCE(?1, datetime('now','localtime')), \
                COALESCE(?2, CAST(strftime('%s', ?1, 'utc') AS INTEGER) * 1000), \
                ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        params![
            at,
            at_ms,
            dry,
            blur,
            headache,
            neck,
            note,
            screen_seconds,
            timezone_offset_minutes(conn)
        ],
    )
    .map_err(|e| format!("insert symptom: {e}"))?;
    Ok(conn.last_insert_rowid())
}

pub fn recent_symptoms(conn: &Connection, limit: i64) -> Result<Vec<SymptomRow>, String> {
    let mut stmt = conn
        .prepare(
            "SELECT id, at, dry, blur, headache, neck, note, screen_seconds, at_ms, \
                    timezone_offset_minutes \
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
                timezone_offset_minutes: row.get(9)?,
            })
        })
        .map_err(|e| format!("query recent symptoms: {e}"))?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("read symptom rows: {e}"))
}

// ---- Fact tables: activity sessions ---------------------------------------

/// Open an activity session and return its id. `started_ms` is UTC epoch ms; `date`
/// (local `YYYY-MM-DD`) and `hour` (local 0-23) are stored for cheap day/hour grouping.
#[allow(clippy::too_many_arguments)]
pub fn open_activity_session(
    conn: &Connection,
    started_ms: i64,
    state: &str,
    process: Option<&str>,
    date: &str,
    hour: i64,
    eye_activity_weight: f64,
    should_defer: bool,
    is_fullscreen: bool,
    reason: Option<&str>,
) -> Result<i64, String> {
    conn.execute(
        "INSERT INTO activity_sessions(started_ms, state, process, date, hour, active_seconds, \
             eye_activity_weight, should_defer, is_fullscreen, reason, timezone_offset_minutes) \
         VALUES(?1, ?2, ?3, ?4, ?5, 0, ?6, ?7, ?8, ?9, ?10)",
        params![
            started_ms,
            state,
            process,
            date,
            hour,
            eye_activity_weight,
            if should_defer { 1 } else { 0 },
            if is_fullscreen { 1 } else { 0 },
            reason,
            timezone_offset_minutes(conn)
        ],
    )
    .map_err(|e| format!("open activity_session: {e}"))?;
    Ok(conn.last_insert_rowid())
}

/// Update an open session's accrued seconds; pass `ended_ms` (UTC epoch ms) to close it.
pub fn update_activity_session(
    conn: &Connection,
    id: i64,
    active_seconds: f64,
    ended_ms: Option<i64>,
) -> Result<(), String> {
    conn.execute(
        "UPDATE activity_sessions \
         SET active_seconds = ?2, ended_ms = COALESCE(?3, ended_ms) WHERE id = ?1",
        params![id, active_seconds, ended_ms],
    )
    .map_err(|e| format!("update activity_session {id}: {e}"))?;
    Ok(())
}

#[derive(serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ActivitySessionRow {
    pub started_ms: i64,
    pub ended_ms: Option<i64>,
    pub active_seconds: f64,
    pub state: String,
    pub process: Option<String>,
    pub date: String,
    pub hour: i64,
    pub eye_activity_weight: Option<f64>,
    pub should_defer: Option<bool>,
    pub is_fullscreen: Option<bool>,
    pub reason: Option<String>,
    pub timezone_offset_minutes: Option<i64>,
}

/// Most-recent `limit` activity sessions, newest first (used by the data export).
pub fn recent_activity_sessions(
    conn: &Connection,
    limit: i64,
) -> Result<Vec<ActivitySessionRow>, String> {
    let mut stmt = conn
        .prepare(
            "SELECT started_ms, ended_ms, active_seconds, state, process, date, hour, \
                    eye_activity_weight, should_defer, is_fullscreen, reason, timezone_offset_minutes \
             FROM activity_sessions ORDER BY started_ms DESC LIMIT ?1",
        )
        .map_err(|e| format!("prepare activity_sessions: {e}"))?;
    let rows = stmt
        .query_map([limit], |row| {
            let should_defer: Option<i64> = row.get(8)?;
            let is_fullscreen: Option<i64> = row.get(9)?;
            Ok(ActivitySessionRow {
                started_ms: row.get(0)?,
                ended_ms: row.get(1)?,
                active_seconds: row.get(2)?,
                state: row.get(3)?,
                process: row.get(4)?,
                date: row.get(5)?,
                hour: row.get(6)?,
                eye_activity_weight: row.get(7)?,
                should_defer: should_defer.map(|v| v != 0),
                is_fullscreen: is_fullscreen.map(|v| v != 0),
                reason: row.get(10)?,
                timezone_offset_minutes: row.get(11)?,
            })
        })
        .map_err(|e| format!("query activity_sessions: {e}"))?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("read activity_sessions rows: {e}"))
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
    pub metric_version: i64,
    pub mode: &'a str,
    pub micro_minutes: f64,
    pub deep_minutes: f64,
    pub break_seconds: f64,
    pub deep_break_minutes: f64,
    pub risk_components_json: Option<&'a str>,
}

/// Upsert a day's row. `risk_peak` is the running max of `risk_score` seen.
pub fn upsert_daily_stats(conn: &Connection, a: &DailyAgg) -> Result<(), String> {
    conn.execute(
        "INSERT INTO daily_stats(date, screen_seconds, distant_gaze_seconds, micro_due, \
             micro_done, deep_due, deep_done, skipped, postponed, deferred, risk_score, \
             risk_peak, updated_at, metric_version, mode, micro_minutes, deep_minutes, \
             break_seconds, deep_break_minutes, risk_components_json, timezone_offset_minutes) \
         VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?11, datetime('now','localtime'), \
             ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19) \
         ON CONFLICT(date) DO UPDATE SET \
             screen_seconds = excluded.screen_seconds, \
             distant_gaze_seconds = excluded.distant_gaze_seconds, \
             micro_due = excluded.micro_due, micro_done = excluded.micro_done, \
             deep_due = excluded.deep_due, deep_done = excluded.deep_done, \
             skipped = excluded.skipped, postponed = excluded.postponed, \
             deferred = excluded.deferred, risk_score = excluded.risk_score, \
             risk_peak = MAX(daily_stats.risk_peak, excluded.risk_score), \
             metric_version = excluded.metric_version, mode = excluded.mode, \
             micro_minutes = excluded.micro_minutes, deep_minutes = excluded.deep_minutes, \
             break_seconds = excluded.break_seconds, deep_break_minutes = excluded.deep_break_minutes, \
             risk_components_json = excluded.risk_components_json, \
             timezone_offset_minutes = excluded.timezone_offset_minutes, \
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
            a.risk_score,
            a.metric_version,
            a.mode,
            a.micro_minutes,
            a.deep_minutes,
            a.break_seconds,
            a.deep_break_minutes,
            a.risk_components_json,
            timezone_offset_minutes(conn)
        ],
    )
    .map_err(|e| format!("upsert daily_stats: {e}"))?;
    Ok(())
}

/// Add eye-use seconds to one (date, hour) heatmap bucket.
pub fn add_hourly_seconds(conn: &Connection, date: &str, hour: i64, delta: f64) -> Result<(), String> {
    conn.execute(
        "INSERT INTO hourly_stats(date, hour, screen_seconds, timezone_offset_minutes) VALUES(?1, ?2, ?3, ?4) \
         ON CONFLICT(date, hour) DO UPDATE SET screen_seconds = screen_seconds + excluded.screen_seconds, \
             timezone_offset_minutes = excluded.timezone_offset_minutes",
        params![date, hour, delta, timezone_offset_minutes(conn)],
    )
    .map_err(|e| format!("add hourly_stats: {e}"))?;
    Ok(())
}

// ---- Derived analytics from the activity_sessions fact layer (V5) ----------

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AppUsageRow {
    /// Foreground process name (e.g. `chrome.exe`); `None` for unknown windows.
    pub process: Option<String>,
    pub active_seconds: f64,
    pub sessions: i64,
}

/// Eye-use time grouped by foreground process since `since_date` (local `YYYY-MM-DD`),
/// busiest first. Powers the "which apps strain your eyes most" view.
pub fn query_app_usage(
    conn: &Connection,
    since_date: &str,
    limit: i64,
) -> Result<Vec<AppUsageRow>, String> {
    let mut stmt = conn
        .prepare(
            "SELECT process, SUM(active_seconds) AS secs, COUNT(*) AS sessions \
             FROM activity_sessions WHERE date >= ?1 \
             GROUP BY process ORDER BY secs DESC LIMIT ?2",
        )
        .map_err(|e| format!("prepare app_usage: {e}"))?;
    let rows = stmt
        .query_map(params![since_date, limit], |row| {
            Ok(AppUsageRow {
                process: row.get(0)?,
                active_seconds: row.get::<_, Option<f64>>(1)?.unwrap_or(0.0),
                sessions: row.get(2)?,
            })
        })
        .map_err(|e| format!("query app_usage: {e}"))?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("read app_usage rows: {e}"))
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StateUsageRow {
    /// `active` (typing) / `reading` (no input but on-screen) / `meeting` (fullscreen/call).
    pub state: String,
    pub active_seconds: f64,
}

/// Eye-use time grouped by activity state since `since_date` — the "use construction"
/// breakdown (active vs passive reading vs meeting).
pub fn query_state_breakdown(
    conn: &Connection,
    since_date: &str,
) -> Result<Vec<StateUsageRow>, String> {
    let mut stmt = conn
        .prepare(
            "SELECT state, SUM(active_seconds) AS secs FROM activity_sessions \
             WHERE date >= ?1 GROUP BY state ORDER BY secs DESC",
        )
        .map_err(|e| format!("prepare state_breakdown: {e}"))?;
    let rows = stmt
        .query_map(params![since_date], |row| {
            Ok(StateUsageRow {
                state: row.get(0)?,
                active_seconds: row.get::<_, Option<f64>>(1)?.unwrap_or(0.0),
            })
        })
        .map_err(|e| format!("query state_breakdown: {e}"))?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("read state_breakdown rows: {e}"))
}

#[derive(serde::Serialize, serde::Deserialize)]
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
    pub metric_version: Option<i64>,
    pub mode: Option<String>,
    pub micro_minutes: Option<f64>,
    pub deep_minutes: Option<f64>,
    pub break_seconds: Option<f64>,
    pub deep_break_minutes: Option<f64>,
    pub risk_components_json: Option<String>,
    pub timezone_offset_minutes: Option<i64>,
}

/// Most-recent `limit` days, newest first.
pub fn query_daily_stats(conn: &Connection, limit: i64) -> Result<Vec<DailyStatsRow>, String> {
    let mut stmt = conn
        .prepare(
            "SELECT date, screen_seconds, distant_gaze_seconds, micro_due, micro_done, \
                    deep_due, deep_done, skipped, postponed, deferred, risk_score, risk_peak, \
                    metric_version, mode, micro_minutes, deep_minutes, break_seconds, \
                    deep_break_minutes, risk_components_json, timezone_offset_minutes \
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
                metric_version: row.get(12)?,
                mode: row.get(13)?,
                micro_minutes: row.get(14)?,
                deep_minutes: row.get(15)?,
                break_seconds: row.get(16)?,
                deep_break_minutes: row.get(17)?,
                risk_components_json: row.get(18)?,
                timezone_offset_minutes: row.get(19)?,
            })
        })
        .map_err(|e| format!("query daily_stats: {e}"))?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("read daily_stats rows: {e}"))
}

#[derive(serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HourlyStatsRow {
    pub date: String,
    pub hour: i64,
    pub screen_seconds: f64,
    pub timezone_offset_minutes: Option<i64>,
}

/// All hourly buckets on or after `since_date` (YYYY-MM-DD), ordered.
pub fn query_hourly_stats(conn: &Connection, since_date: &str) -> Result<Vec<HourlyStatsRow>, String> {
    let mut stmt = conn
        .prepare(
            "SELECT date, hour, screen_seconds, timezone_offset_minutes FROM hourly_stats \
             WHERE date >= ?1 ORDER BY date, hour",
        )
        .map_err(|e| format!("prepare hourly_stats: {e}"))?;
    let rows = stmt
        .query_map([since_date], |row| {
            Ok(HourlyStatsRow {
                date: row.get(0)?,
                hour: row.get(1)?,
                screen_seconds: row.get(2)?,
                timezone_offset_minutes: row.get(3)?,
            })
        })
        .map_err(|e| format!("query hourly_stats: {e}"))?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("read hourly_stats rows: {e}"))
}

/// Close sessions that were left open by a crash or forced quit. We use the
/// accrued active_seconds to avoid turning an overnight app shutdown into a
/// multi-hour fake eye-use span.
pub fn close_stale_activity_sessions(conn: &Connection) -> Result<(), String> {
    conn.execute(
        "UPDATE activity_sessions \
         SET ended_ms = started_ms + CAST(active_seconds * 1000 AS INTEGER) \
         WHERE ended_ms IS NULL",
        [],
    )
    .map_err(|e| format!("close stale activity_sessions: {e}"))?;
    Ok(())
}

// ---- Reliability: backup, recovery, retention ------------------------------

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
    let _ = conn.execute("DELETE FROM activity_sessions WHERE date < ?1", params![cutoff]);
    Ok(())
}

/// Build a complete, privacy-safe data export (aggregates + facts, no window titles,
/// no internal engine blob) as pretty JSON. Round-trips with [`import_json`].
pub fn export_json(conn: &Connection) -> Result<String, String> {
    let daily = query_daily_stats(conn, 3650)?;
    let hourly = query_hourly_stats(conn, "1970-01-01")?;
    let reminder_sessions = recent_reminder_sessions(conn, 1_000_000)?;
    let reminders = recent_reminder_events(conn, 1_000_000)?;
    let symptoms = recent_symptoms(conn, 1_000_000)?;
    let activity = recent_activity_sessions(conn, 1_000_000)?;
    let payload = serde_json::json!({
        "exportedAt": today_local(conn),
        "schemaVersion": current_version(conn),
        "exportFormat": 2,
        "dailyStats": daily,
        "hourlyStats": hourly,
        "reminderSessions": reminder_sessions,
        "reminderEvents": reminders,
        "symptoms": symptoms,
        "activitySessions": activity,
    });
    serde_json::to_string_pretty(&payload).map_err(|e| format!("serialize export: {e}"))
}

/// How many new rows an import inserted per table.
#[derive(Default, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ImportSummary {
    pub daily: i64,
    pub hourly: i64,
    pub reminder_sessions: i64,
    pub reminders: i64,
    pub symptoms: i64,
    pub activity: i64,
}

/// Merge a previously-exported JSON into this database in a single transaction.
/// Idempotent: rows that already exist (by natural key — date, (date,hour), or
/// timestamp+content) are skipped, so re-importing or merging two machines is safe and
/// never overwrites the local data. Returns the count of newly inserted rows per table.
pub fn import_json(conn: &mut Connection, json: &str) -> Result<ImportSummary, String> {
    let v: serde_json::Value =
        serde_json::from_str(json).map_err(|e| format!("parse import json: {e}"))?;
    let tx = conn.transaction().map_err(|e| format!("begin import tx: {e}"))?;
    let mut s = ImportSummary::default();

    if let Some(arr) = v.get("dailyStats") {
        let daily: Vec<DailyStatsRow> =
            serde_json::from_value(arr.clone()).map_err(|e| format!("parse dailyStats: {e}"))?;
        for d in daily {
            s.daily += tx
                .execute(
                    "INSERT OR IGNORE INTO daily_stats(date, screen_seconds, distant_gaze_seconds, \
                         micro_due, micro_done, deep_due, deep_done, skipped, postponed, deferred, \
                         risk_score, risk_peak, metric_version, mode, micro_minutes, deep_minutes, \
                         break_seconds, deep_break_minutes, risk_components_json, timezone_offset_minutes) \
                     VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20)",
                    params![
                        d.date,
                        d.screen_seconds,
                        d.distant_gaze_seconds,
                        d.micro_due,
                        d.micro_done,
                        d.deep_due,
                        d.deep_done,
                        d.skipped,
                        d.postponed,
                        d.deferred,
                        d.risk_score,
                        d.risk_peak,
                        d.metric_version,
                        d.mode,
                        d.micro_minutes,
                        d.deep_minutes,
                        d.break_seconds,
                        d.deep_break_minutes,
                        d.risk_components_json,
                        d.timezone_offset_minutes
                    ],
                )
                .map_err(|e| format!("import daily: {e}"))? as i64;
        }
    }
    if let Some(arr) = v.get("hourlyStats") {
        let hourly: Vec<HourlyStatsRow> =
            serde_json::from_value(arr.clone()).map_err(|e| format!("parse hourlyStats: {e}"))?;
        for h in hourly {
            s.hourly += tx
                .execute(
                    "INSERT OR IGNORE INTO hourly_stats(date, hour, screen_seconds, timezone_offset_minutes) VALUES(?1,?2,?3,?4)",
                    params![h.date, h.hour, h.screen_seconds, h.timezone_offset_minutes],
                )
                .map_err(|e| format!("import hourly: {e}"))? as i64;
        }
    }
    if let Some(arr) = v.get("reminderSessions") {
        let rows: Vec<ReminderSessionRow> =
            serde_json::from_value(arr.clone()).map_err(|e| format!("parse reminderSessions: {e}"))?;
        for r in rows {
            s.reminder_sessions += tx
                .execute(
                    "INSERT OR IGNORE INTO reminder_sessions(uid, kind, source, due_ms, shown_ms, resolved_ms, \
                         result, target_seconds, focus_minutes, screen_seconds_at_show, screen_seconds_at_end, \
                         gaze_seconds, timezone_offset_minutes) \
                     VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13)",
                    params![r.uid, r.kind, r.source, r.due_ms, r.shown_ms, r.resolved_ms, r.result, r.target_seconds, r.focus_minutes, r.screen_seconds_at_show, r.screen_seconds_at_end, r.gaze_seconds, r.timezone_offset_minutes],
                )
                .map_err(|e| format!("import reminder session: {e}"))? as i64;
        }
    }
    if let Some(arr) = v.get("reminderEvents") {
        let rows: Vec<ReminderEventRow> =
            serde_json::from_value(arr.clone()).map_err(|e| format!("parse reminderEvents: {e}"))?;
        for r in rows {
            s.reminders += tx
                .execute(
                    "INSERT INTO reminder_events(at, at_ms, kind, result, active_seconds, gaze_seconds, note, \
                         session_uid, due_ms, shown_ms, resolved_ms, source, focus_minutes, target_seconds, \
                         timezone_offset_minutes) \
                     SELECT ?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,COALESCE(?12,'import'),?13,?14,?15 \
                     WHERE NOT EXISTS(SELECT 1 FROM reminder_events WHERE \
                         ((session_uid IS NOT NULL AND session_uid IS ?8 AND result=?4) OR \
                          (session_uid IS NULL AND at_ms IS ?2 AND kind=?3 AND result=?4 AND active_seconds=?5)))",
                    params![r.at, r.at_ms, r.kind, r.result, r.active_seconds, r.gaze_seconds, r.note, r.session_uid, r.due_ms, r.shown_ms, r.resolved_ms, r.source, r.focus_minutes, r.target_seconds, r.timezone_offset_minutes],
                )
                .map_err(|e| format!("import reminder: {e}"))? as i64;
        }
    }
    if let Some(arr) = v.get("symptoms") {
        let rows: Vec<SymptomRow> =
            serde_json::from_value(arr.clone()).map_err(|e| format!("parse symptoms: {e}"))?;
        for r in rows {
            s.symptoms += tx
                .execute(
                    "INSERT INTO symptom_records(at, at_ms, dry, blur, headache, neck, note, screen_seconds, timezone_offset_minutes) \
                     SELECT ?1,?2,?3,?4,?5,?6,?7,?8,?9 \
                     WHERE NOT EXISTS(SELECT 1 FROM symptom_records WHERE at_ms IS ?2 AND dry=?3 AND blur=?4 AND headache=?5 AND neck=?6)",
                    params![r.at, r.at_ms, r.dry, r.blur, r.headache, r.neck, r.note, r.screen_seconds, r.timezone_offset_minutes],
                )
                .map_err(|e| format!("import symptom: {e}"))? as i64;
        }
    }
    if let Some(arr) = v.get("activitySessions") {
        let rows: Vec<ActivitySessionRow> =
            serde_json::from_value(arr.clone()).map_err(|e| format!("parse activitySessions: {e}"))?;
        for a in rows {
            s.activity += tx
                .execute(
                    "INSERT INTO activity_sessions(started_ms, ended_ms, active_seconds, state, process, date, hour, \
                         eye_activity_weight, should_defer, is_fullscreen, reason, timezone_offset_minutes) \
                     SELECT ?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12 \
                     WHERE NOT EXISTS(SELECT 1 FROM activity_sessions WHERE started_ms=?1 AND process IS ?5)",
                    params![a.started_ms, a.ended_ms, a.active_seconds, a.state, a.process, a.date, a.hour, a.eye_activity_weight.unwrap_or(1.0), a.should_defer.unwrap_or(false) as i64, a.is_fullscreen.unwrap_or(false) as i64, a.reason, a.timezone_offset_minutes],
                )
                .map_err(|e| format!("import activity: {e}"))? as i64;
        }
    }

    tx.commit().map_err(|e| format!("commit import: {e}"))?;
    Ok(s)
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
            metric_version: 1,
            mode: "balanced",
            micro_minutes: 20.0,
            deep_minutes: 60.0,
            break_seconds: 20.0,
            deep_break_minutes: 3.0,
            risk_components_json: Some(r#"{"modelVersion":1,"score":40}"#),
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
            "reminder_sessions",
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
    fn activity_session_open_accrue_close() {
        let path = temp_path();
        let conn = open(&path).unwrap();
        let id = open_activity_session(
            &conn,
            1_700_000_000_000,
            "active",
            Some("chrome.exe"),
            "2026-06-19",
            14,
            0.8,
            false,
            false,
            Some("keyboard/mouse active"),
        )
        .unwrap();
        update_activity_session(&conn, id, 30.0, None).unwrap(); // accrue, still open
        update_activity_session(&conn, id, 45.0, Some(1_700_000_045_000)).unwrap(); // close
        let (secs, ended, state, process, weight, reason): (
            f64,
            Option<i64>,
            String,
            Option<String>,
            f64,
            Option<String>,
        ) = conn
            .query_row(
                "SELECT active_seconds, ended_ms, state, process, eye_activity_weight, reason \
                 FROM activity_sessions WHERE id = ?1",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)),
            )
            .unwrap();
        assert!((secs - 45.0).abs() < 1e-9);
        assert_eq!(ended, Some(1_700_000_045_000));
        assert_eq!(state, "active");
        assert_eq!(process.as_deref(), Some("chrome.exe"));
        assert!((weight - 0.8).abs() < 1e-9);
        assert_eq!(reason.as_deref(), Some("keyboard/mouse active"));
        drop(conn);
        cleanup(&path);
    }

    #[test]
    fn app_and_state_aggregation() {
        let path = temp_path();
        let conn = open(&path).unwrap();
        let id1 = open_activity_session(&conn, 1, "active", Some("chrome.exe"), "2026-06-19", 9, 1.0, false, false, None).unwrap();
        update_activity_session(&conn, id1, 100.0, Some(2)).unwrap();
        let id2 = open_activity_session(&conn, 3, "reading", Some("chrome.exe"), "2026-06-19", 10, 0.7, false, false, Some("reading")).unwrap();
        update_activity_session(&conn, id2, 50.0, Some(4)).unwrap();
        let id3 = open_activity_session(&conn, 5, "active", Some("code.exe"), "2026-06-19", 11, 1.0, false, false, None).unwrap();
        update_activity_session(&conn, id3, 200.0, Some(6)).unwrap();

        let apps = query_app_usage(&conn, "2026-06-19", 10).unwrap();
        assert_eq!(apps.len(), 2);
        assert_eq!(apps[0].process.as_deref(), Some("code.exe")); // busiest first
        assert!((apps[0].active_seconds - 200.0).abs() < 1e-9);
        assert_eq!(apps[1].process.as_deref(), Some("chrome.exe"));
        assert!((apps[1].active_seconds - 150.0).abs() < 1e-9); // 100 + 50
        assert_eq!(apps[1].sessions, 2);

        let states = query_state_breakdown(&conn, "2026-06-19").unwrap();
        let active = states.iter().find(|s| s.state == "active").unwrap().active_seconds;
        let reading = states.iter().find(|s| s.state == "reading").unwrap().active_seconds;
        assert!((active - 300.0).abs() < 1e-9); // 100 + 200
        assert!((reading - 50.0).abs() < 1e-9);

        assert!(query_app_usage(&conn, "2026-06-20", 10).unwrap().is_empty(), "since filter excludes earlier days");
        drop(conn);
        cleanup(&path);
    }

    #[test]
    fn export_import_roundtrip_is_idempotent() {
        let src = temp_path();
        let conn = open(&src).unwrap();
        insert_reminder_event(&conn, None, Some(1000), "micro", "completed", 100.0, 20.0, Some("x")).unwrap();
        insert_symptom(&conn, None, Some(2000), 3, 2, 1, 0, None, 1200.0).unwrap();
        upsert_daily_stats(&conn, &agg("2026-06-19", 40)).unwrap();
        add_hourly_seconds(&conn, "2026-06-19", 14, 100.0).unwrap();
        let uid = open_reminder_session(&conn, "micro", "auto", Some(900), 1000, 20, Some(24), 100.0).unwrap();
        resolve_reminder_session(&conn, &uid, "completed", 1200, 120.0, 20.0).unwrap();
        let aid = open_activity_session(&conn, 5000, "active", Some("chrome.exe"), "2026-06-19", 14, 1.0, false, false, None).unwrap();
        update_activity_session(&conn, aid, 60.0, Some(6000)).unwrap();
        let json = export_json(&conn).unwrap();
        drop(conn);

        let dst = temp_path();
        let mut conn2 = open(&dst).unwrap();
        let s1 = import_json(&mut conn2, &json).unwrap();
        assert_eq!((s1.daily, s1.hourly, s1.reminder_sessions, s1.reminders, s1.symptoms, s1.activity), (1, 1, 1, 1, 1, 1), "fresh import inserts all");
        let s2 = import_json(&mut conn2, &json).unwrap();
        assert_eq!((s2.daily, s2.hourly, s2.reminder_sessions, s2.reminders, s2.symptoms, s2.activity), (0, 0, 0, 0, 0, 0), "re-import is fully deduped");
        assert_eq!(recent_reminder_events(&conn2, 10).unwrap().len(), 1);
        assert_eq!(query_daily_stats(&conn2, 10).unwrap().len(), 1);
        assert_eq!(recent_symptoms(&conn2, 10).unwrap()[0].dry, 3);
        drop(conn2);
        cleanup(&src);
        cleanup(&dst);
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
