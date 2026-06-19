use serde::Serialize;
use std::fs;
use tauri::menu::{Menu, MenuItem};
use tauri::tray::{TrayIconBuilder, TrayIconEvent};
use tauri::{
    AppHandle, Emitter, Listener, Manager, Runtime, State, WebviewUrl, WebviewWindowBuilder,
};
use tauri_plugin_notification::NotificationExt;

mod db;
mod engine;

// Kept as a potential fallback renderer; the active overlay is the WebView card.
#[cfg(windows)]
#[allow(dead_code)]
mod native_reminder;

#[cfg(not(windows))]
#[allow(dead_code)]
mod native_reminder {
    use tauri::{AppHandle, Runtime};

    pub fn show<R: Runtime + 'static>(
        _app: AppHandle<R>,
        _kind: String,
        _seconds: u32,
        _image_index: u8,
        _score: u32,
    ) -> Result<u32, String> {
        Ok(0)
    }

    pub fn start() -> bool {
        false
    }

    pub fn close() {}
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct ActivitySnapshot {
    idle_seconds: f64,
    foreground_title: String,
    foreground_process: String,
    is_fullscreen: bool,
    input_active: bool,
    reading_active: bool,
    eye_activity_weight: f64,
    should_defer: bool,
    reason: String,
    captured_at_ms: u128,
}

#[tauri::command]
fn get_activity_snapshot(reading_grace_seconds: u64, away_seconds: u64) -> ActivitySnapshot {
    platform_activity_snapshot(reading_grace_seconds, away_seconds)
}

#[tauri::command]
fn notify<R: Runtime>(app: AppHandle<R>, title: String, body: String) -> Result<(), String> {
    app.notification()
        .builder()
        .title(title)
        .body(body)
        .show()
        .map_err(|error| error.to_string())
}

#[tauri::command]
fn load_app_data<R: Runtime>(app: AppHandle<R>) -> Result<Option<String>, String> {
    let path = app_data_path(&app)?;
    if !path.exists() {
        return Ok(None);
    }
    fs::read_to_string(path).map(Some).map_err(|error| error.to_string())
}

#[tauri::command]
fn save_app_data<R: Runtime>(app: AppHandle<R>, data: String) -> Result<(), String> {
    let path = app_data_path(&app)?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    }
    fs::write(path, data).map_err(|error| error.to_string())
}

#[tauri::command]
fn set_autostart<R: Runtime>(app: AppHandle<R>, enabled: bool) -> Result<(), String> {
    set_platform_autostart(&app, enabled)
}

#[tauri::command]
fn get_autostart() -> Result<bool, String> {
    get_platform_autostart()
}

#[tauri::command]
fn show_reminder_overlay<R: Runtime + 'static>(
    app: AppHandle<R>,
    kind: String,
    seconds: u32,
    image_index: u8,
    score: u32,
) -> Result<u32, String> {
    // Optimistic count for the caller (preview path); the real windows are built
    // on the main thread because WebView windows must be created there.
    let expected = app
        .available_monitors()
        .map(|monitors| monitors.len().max(1))
        .unwrap_or(1) as u32;
    schedule_fire_reminder(app, kind == "deep", seconds, image_index, score, 0);
    Ok(expected)
}

#[tauri::command]
fn overlay_action<R: Runtime>(app: AppHandle<R>, action: String) -> Result<(), String> {
    close_overlay_windows(&app);
    app.emit("overlay-action", action)
        .map_err(|error| error.to_string())
}

#[tauri::command]
fn overlay_start<R: Runtime>(app: AppHandle<R>) -> Result<(), String> {
    // Broadcast so every monitor's card starts its countdown in sync.
    app.emit("overlay-start", ()).map_err(|error| error.to_string())
}

#[tauri::command]
fn get_display_count<R: Runtime>(app: AppHandle<R>) -> Result<usize, String> {
    app.available_monitors()
        .map(|monitors| monitors.len())
        .map_err(|error| error.to_string())
}

fn close_overlay_windows<R: Runtime>(app: &AppHandle<R>) {
    for (label, window) in app.webview_windows() {
        if label.starts_with("reminder-overlay-") {
            let _ = window.close();
        }
    }
}

/// Where a single overlay card should sit (physical pixels) and how big it is
/// (logical units, for the WebView's inner size).
struct OverlayPlacement {
    logical_w: f64,
    logical_h: f64,
    phys_x: i32,
    phys_y: i32,
    phys_w: i32,
    phys_h: i32,
}

/// Build one small, opaque, centered reminder card per monitor and return how many
/// were actually created. Must run on the main thread. The card is deliberately
/// small (~1/10 of the screen) so the desktop stays visible around it — a reminder,
/// not a block.
fn build_overlay_windows<R: Runtime + 'static>(
    app: &AppHandle<R>,
    kind: &str,
    seconds: u32,
    image_index: u8,
    score: u32,
    focus_minutes: i64,
) -> u32 {
    let kind_value = if kind == "deep" { "deep" } else { "micro" };
    let init_script = format!(
        "window.__GAZE20_OVERLAY__ = {{ kind: \"{kind_value}\", seconds: {seconds}, \
         imageIndex: {image_index}, score: {score}, focusMinutes: {focus_minutes} }};"
    );
    let session = now_ms();
    let monitors = app.available_monitors().unwrap_or_default();
    let mut created = 0u32;

    if monitors.is_empty() {
        // Headless fallback: let Tauri center one card on the primary monitor.
        let label = format!("reminder-overlay-{session}-0");
        if build_one_overlay(app, &label, &init_script, None).is_ok() {
            created += 1;
        }
        return created;
    }

    for (i, m) in monitors.iter().enumerate() {
        let scale = m.scale_factor().max(1.0);
        let mw_logical = m.size().width as f64 / scale;
        let mh_logical = m.size().height as f64 / scale;
        // Compact card; clamp so it never overflows tiny displays.
        let card_w = 420.0_f64.min(mw_logical - 32.0).max(300.0);
        let card_h = 560.0_f64.min(mh_logical - 40.0).max(380.0);
        let phys_w = (card_w * scale).round() as i32;
        let phys_h = (card_h * scale).round() as i32;
        let center_x = m.position().x + (m.size().width as i32) / 2;
        let center_y = m.position().y + (m.size().height as i32) / 2;
        let placement = OverlayPlacement {
            logical_w: card_w,
            logical_h: card_h,
            phys_x: center_x - phys_w / 2,
            phys_y: center_y - phys_h / 2,
            phys_w,
            phys_h,
        };
        let label = format!("reminder-overlay-{session}-{i}");
        if build_one_overlay(app, &label, &init_script, Some(placement)).is_ok() {
            created += 1;
        }
    }
    created
}

fn build_one_overlay<R: Runtime + 'static>(
    app: &AppHandle<R>,
    label: &str,
    init_script: &str,
    placement: Option<OverlayPlacement>,
) -> Result<(), String> {
    let (logical_w, logical_h) = match &placement {
        Some(p) => (p.logical_w, p.logical_h),
        None => (420.0, 560.0),
    };
    let mut builder = WebviewWindowBuilder::new(app, label, WebviewUrl::App("overlay.html".into()))
        .title("远眺提醒")
        .inner_size(logical_w, logical_h)
        .decorations(false)
        .transparent(false)
        .shadow(true)
        .always_on_top(true)
        .skip_taskbar(true)
        .resizable(false)
        .maximizable(false)
        .minimizable(false)
        .focused(true)
        .initialization_script(init_script);
    if placement.is_none() {
        builder = builder.center();
    }
    let window = builder.build().map_err(|error| error.to_string())?;
    if let Some(p) = placement {
        // Pin exact physical geometry per monitor; repeat after show() because some
        // WebView2 builds re-center on first paint.
        let size = tauri::PhysicalSize::new(p.phys_w.max(1) as u32, p.phys_h.max(1) as u32);
        let pos = tauri::PhysicalPosition::new(p.phys_x, p.phys_y);
        let _ = window.set_size(size);
        let _ = window.set_position(pos);
        let _ = window.show();
        let _ = window.set_always_on_top(true);
        let _ = window.set_focus();
        let _ = window.set_size(size);
        let _ = window.set_position(pos);
    }
    Ok(())
}

fn app_data_path<R: Runtime>(app: &AppHandle<R>) -> Result<std::path::PathBuf, String> {
    let dir = app.path().app_data_dir().map_err(|error| error.to_string())?;
    Ok(dir.join("gaze20-data.json"))
}

// ---- Local database commands (Phase 1) ------------------------------------

#[tauri::command]
fn db_get_settings(
    db: State<db::Database>,
) -> Result<std::collections::BTreeMap<String, String>, String> {
    let conn = db.conn.lock().map_err(|_| "db lock poisoned".to_string())?;
    db::get_all_settings(&conn)
}

#[tauri::command]
fn db_set_setting(db: State<db::Database>, key: String, value: String) -> Result<(), String> {
    let conn = db.conn.lock().map_err(|_| "db lock poisoned".to_string())?;
    db::set_setting(&conn, &key, &value)
}

#[tauri::command]
fn db_schema_version(db: State<db::Database>) -> Result<i64, String> {
    let conn = db.conn.lock().map_err(|_| "db lock poisoned".to_string())?;
    Ok(db::schema_version(&conn))
}

#[tauri::command]
#[allow(clippy::too_many_arguments)]
fn db_add_symptom(
    db: State<db::Database>,
    dry: i64,
    blur: i64,
    headache: i64,
    neck: i64,
    note: Option<String>,
    screen_seconds: f64,
) -> Result<i64, String> {
    let conn = db.conn.lock().map_err(|_| "db lock poisoned".to_string())?;
    db::insert_symptom(
        &conn,
        None,
        Some(now_ms() as i64),
        dry,
        blur,
        headache,
        neck,
        note.as_deref(),
        screen_seconds,
    )
}

#[tauri::command]
fn db_recent_symptoms(
    db: State<db::Database>,
    limit: Option<i64>,
) -> Result<Vec<db::SymptomRow>, String> {
    let conn = db.conn.lock().map_err(|_| "db lock poisoned".to_string())?;
    db::recent_symptoms(&conn, limit.unwrap_or(240))
}

#[tauri::command]
fn db_recent_reminders(
    db: State<db::Database>,
    limit: Option<i64>,
) -> Result<Vec<db::ReminderEventRow>, String> {
    let conn = db.conn.lock().map_err(|_| "db lock poisoned".to_string())?;
    db::recent_reminder_events(&conn, limit.unwrap_or(80))
}

#[tauri::command]
fn db_daily_stats(
    db: State<db::Database>,
    limit: Option<i64>,
) -> Result<Vec<db::DailyStatsRow>, String> {
    let conn = db.conn.lock().map_err(|_| "db lock poisoned".to_string())?;
    db::query_daily_stats(&conn, limit.unwrap_or(30).clamp(1, 365))
}

#[tauri::command]
fn db_hourly_stats(
    db: State<db::Database>,
    days: Option<i64>,
) -> Result<Vec<db::HourlyStatsRow>, String> {
    let conn = db.conn.lock().map_err(|_| "db lock poisoned".to_string())?;
    let n = days.unwrap_or(7).clamp(1, 365);
    let since: String = conn
        .query_row(
            "SELECT date('now','localtime', ?1)",
            [format!("-{} days", n - 1)],
            |row| row.get(0),
        )
        .unwrap_or_default();
    db::query_hourly_stats(&conn, &since)
}

#[tauri::command]
fn db_export(db: State<db::Database>) -> Result<String, String> {
    let conn = db.conn.lock().map_err(|_| "db lock poisoned".to_string())?;
    db::export_json(&conn)
}

/// Check the configured update endpoint. Returns the new version string if one is
/// available, `None` if up to date. Errors (e.g. "updater not configured") are
/// surfaced so the UI can say so — see doc/RELEASE.md to activate updates.
#[tauri::command]
async fn check_for_update<R: Runtime>(app: AppHandle<R>) -> Result<Option<String>, String> {
    use tauri_plugin_updater::UpdaterExt;
    let updater = app.updater().map_err(|e| e.to_string())?;
    let update = updater.check().await.map_err(|e| e.to_string())?;
    Ok(update.map(|u| u.version))
}

// ---- Engine wiring (Phase 2c): Rust owns the state machine -----------------

/// Managed state: the live engine plus the most recent activity snapshot (so a
/// command can answer `engine_get_state` without re-polling the OS).
struct EngineHandle {
    engine: std::sync::Mutex<engine::Engine>,
    snapshot: std::sync::Mutex<ActivitySnapshot>,
}

fn fallback_snapshot() -> ActivitySnapshot {
    ActivitySnapshot {
        idle_seconds: 0.0,
        foreground_title: String::new(),
        foreground_process: String::new(),
        is_fullscreen: false,
        input_active: false,
        reading_active: false,
        eye_activity_weight: 1.0,
        should_defer: false,
        reason: "正在初始化".into(),
        captured_at_ms: now_ms(),
    }
}

fn score_from_risk(risk: i64) -> u32 {
    ((100.0 - risk as f64 * 0.72).round() as i64).clamp(18, 100) as u32
}

fn eye_image_index(risk: i64) -> u8 {
    ((risk as f64 / 100.0 * 9.0).round() as i64).clamp(0, 9) as u8
}

/// Everything the UI renders, in one camelCase payload. Emitted every second on
/// `engine-state` and returned by the engine commands.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct LiveState {
    mode: String,
    running: bool,
    do_not_disturb: bool,
    debug_fast_mode: bool,
    reading_grace_minutes: f64,
    away_minutes: f64,
    date: String,
    screen_seconds: f64,
    micro_active: f64,
    deep_active: f64,
    continuous: f64,
    micro_done: i64,
    deep_done: i64,
    micro_due: i64,
    deep_due: i64,
    distant_gaze: f64,
    postponed: i64,
    skipped: i64,
    deferred: i64,
    risk: i64,
    eye_score: u32,
    image_index: u8,
    streak_days: i64,
    effective_micro_seconds: f64,
    effective_deep_seconds: f64,
    next_micro_seconds: f64,
    next_deep_seconds: f64,
    reminding: Option<String>,
    snooze_active: bool,
    reason: String,
    foreground_process: String,
    should_defer: bool,
    is_fullscreen: bool,
}

fn build_live_state(e: &engine::Engine, snap: &ActivitySnapshot, now: u128) -> LiveState {
    let eff_micro = e.effective_micro_seconds();
    let eff_deep = e.effective_deep_seconds();
    LiveState {
        mode: e.mode.as_str().to_string(),
        running: e.running,
        do_not_disturb: e.do_not_disturb,
        debug_fast_mode: e.debug_fast_mode,
        reading_grace_minutes: e.reading_grace_minutes,
        away_minutes: e.away_minutes,
        date: e.date.clone(),
        screen_seconds: e.screen_seconds,
        micro_active: e.micro_active,
        deep_active: e.deep_active,
        continuous: e.continuous,
        micro_done: e.micro_done,
        deep_done: e.deep_done,
        micro_due: e.micro_due,
        deep_due: e.deep_due,
        distant_gaze: e.distant_gaze,
        postponed: e.postponed,
        skipped: e.skipped,
        deferred: e.deferred,
        risk: e.risk,
        eye_score: score_from_risk(e.risk),
        image_index: eye_image_index(e.risk),
        streak_days: e.streak_days,
        effective_micro_seconds: eff_micro,
        effective_deep_seconds: eff_deep,
        next_micro_seconds: (eff_micro - e.micro_active).max(0.0),
        next_deep_seconds: (eff_deep - e.deep_active).max(0.0),
        reminding: e.reminding.map(|k| k.as_str().to_string()),
        snooze_active: now < e.snooze_until_ms,
        reason: snap.reason.clone(),
        foreground_process: snap.foreground_process.clone(),
        should_defer: snap.should_defer,
        is_fullscreen: snap.is_fullscreen,
    }
}

fn current_live(eh: &EngineHandle) -> LiveState {
    let e = eh.engine.lock().unwrap_or_else(|p| p.into_inner());
    let s = eh.snapshot.lock().unwrap_or_else(|p| p.into_inner());
    build_live_state(&e, &s, now_ms())
}

fn record_reminder_event<R: Runtime>(
    app: &AppHandle<R>,
    kind: &str,
    result: &str,
    active: f64,
    gaze: f64,
    note: Option<&str>,
) {
    if let Some(db) = app.try_state::<db::Database>() {
        if let Ok(conn) = db.conn.lock() {
            let _ = db::insert_reminder_event(&conn, None, Some(now_ms() as i64), kind, result, active, gaze, note);
        }
    }
}

fn persist_engine<R: Runtime>(app: &AppHandle<R>) {
    let json = match app.try_state::<EngineHandle>() {
        Some(eh) => eh
            .engine
            .lock()
            .ok()
            .and_then(|e| serde_json::to_string(&*e).ok()),
        None => None,
    };
    if let (Some(json), Some(db)) = (json, app.try_state::<db::Database>()) {
        if let Ok(conn) = db.conn.lock() {
            let _ = db::set_setting(&conn, "engine_state", &json);
        }
    }
}

/// Upsert one day's aggregate row from the engine's counters (today live, or a
/// finished day at rollover).
fn upsert_daily<R: Runtime>(app: &AppHandle<R>, e: &engine::Engine) {
    if e.date.is_empty() {
        return;
    }
    if let Some(db) = app.try_state::<db::Database>() {
        if let Ok(conn) = db.conn.lock() {
            let _ = db::upsert_daily_stats(
                &conn,
                &db::DailyAgg {
                    date: &e.date,
                    screen_seconds: e.screen_seconds,
                    distant_gaze_seconds: e.distant_gaze,
                    micro_due: e.micro_due,
                    micro_done: e.micro_done,
                    deep_due: e.deep_due,
                    deep_done: e.deep_done,
                    skipped: e.skipped,
                    postponed: e.postponed,
                    deferred: e.deferred,
                    risk_score: e.risk,
                },
            );
        }
    }
}

/// Best-effort flush of live state to disk: the engine snapshot plus today's
/// aggregate. Called on a clean quit so the last few seconds aren't lost.
fn flush_state<R: Runtime>(app: &AppHandle<R>) {
    persist_engine(app);
    if let Some(eh) = app.try_state::<EngineHandle>() {
        let snapshot = eh.engine.lock().unwrap_or_else(|p| p.into_inner()).clone();
        upsert_daily(app, &snapshot);
    }
}

/// Whole-day difference `to - from` (both `YYYY-MM-DD`), via SQLite date math.
/// Returns 1 (the "consecutive" default) if the database is unavailable.
fn days_between<R: Runtime>(app: &AppHandle<R>, from: &str, to: &str) -> i64 {
    if let Some(db) = app.try_state::<db::Database>() {
        if let Ok(conn) = db.conn.lock() {
            if let Ok(diff) = conn.query_row(
                "SELECT CAST(round(julianday(?2) - julianday(?1)) AS INTEGER)",
                rusqlite::params![from, to],
                |row| row.get::<_, i64>(0),
            ) {
                return diff;
            }
        }
    }
    1
}

fn load_engine<R: Runtime>(app: &AppHandle<R>) -> engine::Engine {
    let mut e = engine::Engine::default();
    if let Some(db) = app.try_state::<db::Database>() {
        if let Ok(conn) = db.conn.lock() {
            if let Some(json) = db::get_setting(&conn, "engine_state") {
                if let Ok(loaded) = serde_json::from_str::<engine::Engine>(&json) {
                    e = loaded;
                }
            }
            if e.date.is_empty() {
                e.date = db::today_local(&conn);
            }
        }
    }
    e
}

/// Resolve a finished reminder (button or countdown timeout) into the engine and
/// record the fact. Idempotent: a duplicate `overlay-action` resolves to `None`.
fn handle_overlay_action<R: Runtime>(app: &AppHandle<R>, payload: &str) {
    let action = serde_json::from_str::<String>(payload)
        .unwrap_or_else(|_| payload.trim_matches('"').to_string());
    let result = match action.as_str() {
        "complete" => engine::Reminder::Completed,
        "postpone" => engine::Reminder::Postponed,
        "skip" => engine::Reminder::Skipped,
        _ => return,
    };
    let now = now_ms();
    let (outcome, active) = match app.try_state::<EngineHandle>() {
        Some(eh) => {
            let mut e = eh.engine.lock().unwrap_or_else(|p| p.into_inner());
            let active = e.screen_seconds;
            (e.resolve(result, now), active)
        }
        None => return,
    };
    if let Some((kind, gaze)) = outcome {
        let result_str = match result {
            engine::Reminder::Completed => "completed",
            engine::Reminder::Postponed => "postponed",
            engine::Reminder::Skipped => "skipped",
        };
        record_reminder_event(app, kind.as_str(), result_str, active, gaze, None);
        persist_engine(app);
    }
}

/// Fire the WebView reminder card on every monitor, falling back to a system
/// notification if no window could be created (so the reminder is never lost).
/// Window creation is marshalled onto the main thread.
fn fire_reminder<R: Runtime + 'static>(
    app: &AppHandle<R>,
    deep: bool,
    seconds: u32,
    image_index: u8,
    eye_score: u32,
    focus_minutes: i64,
) {
    let app2 = app.clone();
    let kind = (if deep { "deep" } else { "micro" }).to_string();
    let _ = app.run_on_main_thread(move || {
        close_overlay_windows(&app2);
        let shown = build_overlay_windows(&app2, &kind, seconds, image_index, eye_score, focus_minutes);
        if shown == 0 {
            log::warn!("overlay created 0 windows; falling back to a notification");
            let (title, body) = if deep {
                ("该起身深休息了", "离开屏幕几分钟，走动、补水、放松肩颈和眼睛。")
            } else {
                ("该远眺一下了", "看向 6 米外，保持 20 秒，并做 5–10 次完整眨眼。")
            };
            let _ = app2.notification().builder().title(title).body(body).show();
        }
    });
}

/// Frontend-triggered commands can arrive while the main WebView is still inside
/// its IPC handling path. Creating another WebView synchronously in that moment is
/// exactly the case that produced a visible but blank white card. Schedule the
/// real reminder a hair later from a background thread; it still creates windows
/// on Tauri's main thread via `fire_reminder`, but after the command has returned.
fn schedule_fire_reminder<R: Runtime + 'static>(
    app: AppHandle<R>,
    deep: bool,
    seconds: u32,
    image_index: u8,
    eye_score: u32,
    focus_minutes: i64,
) {
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(120));
        fire_reminder(&app, deep, seconds, image_index, eye_score, focus_minutes);
    });
}

/// The 1-second authority loop. Polls activity, advances the engine, performs the
/// single decision (fire native overlay / record a deferral), persists, and emits
/// live state. Runs regardless of whether the main window is visible.
fn spawn_engine_loop<R: Runtime + 'static>(app: AppHandle<R>) {
    std::thread::spawn(move || {
        let mut last = std::time::Instant::now();
        let mut ticks: u64 = 0;
        loop {
            std::thread::sleep(std::time::Duration::from_millis(1000));
            let now = std::time::Instant::now();
            let dt = (now - last).as_secs_f64().min(3.0);
            last = now;
            ticks += 1;

            let (grace_s, away_s) = match app.try_state::<EngineHandle>() {
                Some(eh) => {
                    let e = eh.engine.lock().unwrap_or_else(|p| p.into_inner());
                    (
                        (e.reading_grace_minutes * 60.0) as u64,
                        (e.away_minutes * 60.0) as u64,
                    )
                }
                None => (8 * 60, 12 * 60),
            };
            let snap = platform_activity_snapshot(grace_s, away_s);
            let now_ms_val = now_ms();
            let today = app
                .try_state::<db::Database>()
                .and_then(|db| db.conn.lock().ok().map(|c| db::today_local(&c)))
                .unwrap_or_default();

            let (decision, live, finished, accumulated) = match app.try_state::<EngineHandle>() {
                Some(eh) => {
                    *eh.snapshot.lock().unwrap_or_else(|p| p.into_inner()) = snap.clone();
                    let mut e = eh.engine.lock().unwrap_or_else(|p| p.into_inner());
                    let finished = if today.is_empty() { None } else { e.roll_over(&today) };
                    let decision = e.tick(dt, snap.eye_activity_weight, snap.should_defer, now_ms_val);
                    let accumulated = e.last_tick_accumulated;
                    (
                        decision,
                        build_live_state(&e, &snap, now_ms_val),
                        finished,
                        accumulated,
                    )
                }
                None => continue,
            };

            // Finalize the finished day's aggregate at the boundary, then advance the
            // consecutive-guard streak based on how that day went.
            if let Some(finished_day) = finished {
                upsert_daily(&app, &finished_day);
                let consecutive = days_between(&app, &finished_day.date, &today) == 1;
                let qualified =
                    engine::day_qualifies(finished_day.micro_done, finished_day.screen_seconds);
                let new_streak =
                    engine::next_streak(finished_day.streak_days, qualified, consecutive);
                if let Some(eh) = app.try_state::<EngineHandle>() {
                    eh.engine.lock().unwrap_or_else(|p| p.into_inner()).streak_days = new_streak;
                }
                persist_engine(&app);
            }

            // Bump the per-hour heatmap bucket for the seconds actually accrued.
            if accumulated > 0.0 && !today.is_empty() {
                if let Some(db) = app.try_state::<db::Database>() {
                    if let Ok(conn) = db.conn.lock() {
                        let hour = db::local_hour(&conn);
                        let _ = db::add_hourly_seconds(&conn, &today, hour, accumulated);
                    }
                }
            }

            match decision {
                engine::Decision::Fire { deep, seconds } => {
                    let focus_minutes = (live.continuous / 60.0).round() as i64;
                    fire_reminder(&app, deep, seconds, live.image_index, live.eye_score, focus_minutes);
                }
                engine::Decision::Deferred => {
                    record_reminder_event(&app, "micro", "deferred", live.screen_seconds, 0.0, Some(&snap.reason));
                    persist_engine(&app);
                }
                engine::Decision::None => {}
            }

            let _ = app.emit("engine-state", &live);
            if ticks.is_multiple_of(5) {
                persist_engine(&app);
                if let Some(eh) = app.try_state::<EngineHandle>() {
                    let snapshot_engine = eh.engine.lock().unwrap_or_else(|p| p.into_inner()).clone();
                    upsert_daily(&app, &snapshot_engine);
                }
            }
        }
    });
}

#[tauri::command]
fn engine_get_state(eh: State<EngineHandle>) -> LiveState {
    current_live(&eh)
}

#[tauri::command]
fn engine_set_running<R: Runtime>(
    app: AppHandle<R>,
    eh: State<EngineHandle>,
    running: bool,
) -> LiveState {
    eh.engine.lock().unwrap_or_else(|p| p.into_inner()).running = running;
    persist_engine(&app);
    current_live(&eh)
}

#[tauri::command]
fn engine_set_mode<R: Runtime>(
    app: AppHandle<R>,
    eh: State<EngineHandle>,
    mode: String,
) -> LiveState {
    eh.engine.lock().unwrap_or_else(|p| p.into_inner()).mode = engine::Mode::from_str(&mode);
    persist_engine(&app);
    current_live(&eh)
}

#[tauri::command]
fn engine_set_dnd<R: Runtime>(
    app: AppHandle<R>,
    eh: State<EngineHandle>,
    do_not_disturb: bool,
) -> LiveState {
    eh.engine.lock().unwrap_or_else(|p| p.into_inner()).do_not_disturb = do_not_disturb;
    persist_engine(&app);
    current_live(&eh)
}

#[tauri::command]
fn engine_set_debug_fast<R: Runtime>(
    app: AppHandle<R>,
    eh: State<EngineHandle>,
    debug_fast_mode: bool,
) -> LiveState {
    eh.engine.lock().unwrap_or_else(|p| p.into_inner()).debug_fast_mode = debug_fast_mode;
    persist_engine(&app);
    current_live(&eh)
}

#[tauri::command]
fn engine_set_detection<R: Runtime>(
    app: AppHandle<R>,
    eh: State<EngineHandle>,
    reading_grace_minutes: f64,
    away_minutes: f64,
) -> LiveState {
    {
        let mut e = eh.engine.lock().unwrap_or_else(|p| p.into_inner());
        e.reading_grace_minutes = reading_grace_minutes;
        e.away_minutes = away_minutes;
    }
    persist_engine(&app);
    current_live(&eh)
}

#[tauri::command]
fn engine_snooze<R: Runtime>(
    app: AppHandle<R>,
    eh: State<EngineHandle>,
    minutes: f64,
) -> LiveState {
    eh.engine.lock().unwrap_or_else(|p| p.into_inner()).snooze_until_ms =
        now_ms() + (minutes * 60_000.0) as u128;
    persist_engine(&app);
    current_live(&eh)
}

#[tauri::command]
fn engine_reset_today<R: Runtime>(app: AppHandle<R>, eh: State<EngineHandle>) -> LiveState {
    {
        let mut e = eh.engine.lock().unwrap_or_else(|p| p.into_inner());
        let keep = e.clone();
        *e = engine::Engine::default();
        e.mode = keep.mode;
        e.running = keep.running;
        e.do_not_disturb = keep.do_not_disturb;
        e.debug_fast_mode = keep.debug_fast_mode;
        e.reading_grace_minutes = keep.reading_grace_minutes;
        e.away_minutes = keep.away_minutes;
        e.date = keep.date;
        e.streak_days = keep.streak_days;
    }
    persist_engine(&app);
    current_live(&eh)
}

/// Manual "rest now": fire a micro break immediately if one isn't already showing.
#[tauri::command]
fn engine_rest_now<R: Runtime + 'static>(app: AppHandle<R>, eh: State<EngineHandle>) -> LiveState {
    let fire = {
        let mut e = eh.engine.lock().unwrap_or_else(|p| p.into_inner());
        if e.reminding.is_some() {
            None
        } else {
            e.reminding = Some(engine::Kind::Micro);
            Some((
                e.mode.preset().break_seconds as u32,
                score_from_risk(e.risk),
                eye_image_index(e.risk),
                (e.continuous / 60.0).round() as i64,
            ))
        }
    };
    if let Some((seconds, score, image, focus_minutes)) = fire {
        schedule_fire_reminder(app.clone(), false, seconds, image, score, focus_minutes);
    }
    current_live(&eh)
}

// ---- Legacy JSON import (Phase 4) -----------------------------------------

#[derive(serde::Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
struct LegacySettings {
    mode: String,
    do_not_disturb: bool,
    reading_grace_minutes: f64,
    away_minutes: f64,
    debug_fast_mode: bool,
}

#[derive(serde::Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
struct LegacyToday {
    date: String,
    screen_seconds: f64,
    distant_gaze_seconds: f64,
    micro_due: i64,
    micro_done: i64,
    deep_due: i64,
    deep_done: i64,
    skipped: i64,
    postponed: i64,
    deferred: i64,
    risk_score: i64,
}

#[derive(serde::Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
struct LegacyLog {
    at: String,
    kind: String,
    result: String,
    active_seconds: f64,
    note: String,
}

#[derive(serde::Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
struct LegacyScores {
    dry: i64,
    blur: i64,
    headache: i64,
    neck: i64,
}

#[derive(serde::Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
struct LegacySymptom {
    at: String,
    scores: LegacyScores,
    note: String,
    screen_seconds: f64,
}

#[derive(serde::Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
struct LegacyData {
    settings: LegacySettings,
    today: LegacyToday,
    logs: Vec<LegacyLog>,
    symptoms: Vec<LegacySymptom>,
    streak_days: i64,
}

fn non_empty(s: &str) -> Option<&str> {
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

/// One-time import of the legacy `gaze20-data.json` into the database. Backs up
/// the JSON first, imports logs/symptoms as facts and the legacy "today" as a
/// daily_stats row, and (only on a fresh install) seeds the engine settings.
/// Guarded by `app_meta`-style `legacy_imported` so it never runs twice.
#[allow(clippy::field_reassign_with_default)]
fn import_legacy_json<R: Runtime>(app: &AppHandle<R>) {
    let db = match app.try_state::<db::Database>() {
        Some(db) => db,
        None => return,
    };
    let conn = match db.conn.lock() {
        Ok(conn) => conn,
        Err(_) => return,
    };
    if db::get_setting(&conn, "legacy_imported").is_some() {
        return;
    }
    let path = match app_data_path(app) {
        Ok(path) => path,
        Err(_) => return,
    };
    if !path.exists() {
        let _ = db::set_setting(&conn, "legacy_imported", "no-file");
        return;
    }
    let raw = match fs::read_to_string(&path) {
        Ok(raw) => raw,
        Err(_) => return,
    };
    let legacy: LegacyData = match serde_json::from_str(&raw) {
        Ok(value) => value,
        Err(error) => {
            log::error!("legacy import parse failed: {error}");
            let _ = db::set_setting(&conn, "legacy_imported", "parse-error");
            return;
        }
    };

    // Preserve the original before treating it as imported.
    let _ = fs::copy(&path, path.with_extension("json.bak"));

    for log in &legacy.logs {
        let kind = if log.kind == "deep" { "deep" } else { "micro" };
        let _ = db::insert_reminder_event(
            &conn,
            non_empty(&log.at),
            None,
            kind,
            &log.result,
            log.active_seconds,
            0.0,
            non_empty(&log.note),
        );
    }
    for symptom in &legacy.symptoms {
        let _ = db::insert_symptom(
            &conn,
            non_empty(&symptom.at),
            None,
            symptom.scores.dry,
            symptom.scores.blur,
            symptom.scores.headache,
            symptom.scores.neck,
            non_empty(&symptom.note),
            symptom.screen_seconds,
        );
    }
    if !legacy.today.date.is_empty() && legacy.today.screen_seconds > 0.0 {
        let _ = db::upsert_daily_stats(
            &conn,
            &db::DailyAgg {
                date: &legacy.today.date,
                screen_seconds: legacy.today.screen_seconds,
                distant_gaze_seconds: legacy.today.distant_gaze_seconds,
                micro_due: legacy.today.micro_due,
                micro_done: legacy.today.micro_done,
                deep_due: legacy.today.deep_due,
                deep_done: legacy.today.deep_done,
                skipped: legacy.today.skipped,
                postponed: legacy.today.postponed,
                deferred: legacy.today.deferred,
                risk_score: legacy.today.risk_score,
            },
        );
    }

    // Only seed engine settings on a clean install — never clobber live state.
    if db::get_setting(&conn, "engine_state").is_none() {
        let mut e = engine::Engine::default();
        e.mode = engine::Mode::from_str(&legacy.settings.mode);
        e.do_not_disturb = legacy.settings.do_not_disturb;
        if legacy.settings.reading_grace_minutes > 0.0 {
            e.reading_grace_minutes = legacy.settings.reading_grace_minutes;
        }
        if legacy.settings.away_minutes > 0.0 {
            e.away_minutes = legacy.settings.away_minutes;
        }
        e.debug_fast_mode = legacy.settings.debug_fast_mode;
        if legacy.streak_days > 0 {
            e.streak_days = legacy.streak_days;
        }
        e.date = db::today_local(&conn);
        // If the legacy "today" really is today, resume those counters.
        if legacy.today.date == e.date {
            e.screen_seconds = legacy.today.screen_seconds;
            e.distant_gaze = legacy.today.distant_gaze_seconds;
            e.micro_due = legacy.today.micro_due;
            e.micro_done = legacy.today.micro_done;
            e.deep_due = legacy.today.deep_due;
            e.deep_done = legacy.today.deep_done;
            e.skipped = legacy.today.skipped;
            e.postponed = legacy.today.postponed;
            e.deferred = legacy.today.deferred;
            e.risk = legacy.today.risk_score;
        }
        if let Ok(json) = serde_json::to_string(&e) {
            let _ = db::set_setting(&conn, "engine_state", &json);
        }
    }

    let stamp = db::today_local(&conn);
    let _ = db::set_setting(&conn, "legacy_imported", &stamp);
    log::info!(
        "legacy import: {} logs, {} symptoms, day {}",
        legacy.logs.len(),
        legacy.symptoms.len(),
        legacy.today.date
    );
}

pub fn run() {
    let mut builder = tauri::Builder::default();
    // Single-instance must be the first plugin: a second launch hands its args to
    // the running instance and exits, so we surface the existing window instead of
    // piling up tray-only zombie processes. Skipped under the self-test flag so a
    // diagnostic launch is never swallowed by an already-running instance.
    let self_test = std::env::var_os("GAZE20_SELF_TEST_REMINDER").is_some()
        || std::env::var_os("GAZE20_SELF_TEST_TRAY").is_some();
    if !self_test {
        builder = builder.plugin(tauri_plugin_single_instance::init(|app, _argv, _cwd| {
            if let Some(window) = app.get_webview_window("main") {
                let _ = window.unminimize();
                let _ = window.show();
                let _ = window.set_focus();
            }
        }));
    }
    builder
        .plugin(
            tauri_plugin_log::Builder::new()
                .level(log::LevelFilter::Info)
                .targets([
                    tauri_plugin_log::Target::new(tauri_plugin_log::TargetKind::Stderr),
                    tauri_plugin_log::Target::new(tauri_plugin_log::TargetKind::LogDir {
                        file_name: Some("gaze20".into()),
                    }),
                ])
                .build(),
        )
        .plugin(tauri_plugin_updater::Builder::new().build())
        .plugin(tauri_plugin_notification::init())
        .setup(|app| {
            log::info!("Gaze20 v{} starting", env!("CARGO_PKG_VERSION"));

            // Open the local SQLite database (creating + migrating as needed) and
            // hand it to Tauri's managed state so commands can reach it.
            match app.path().app_data_dir() {
                Ok(dir) => {
                    let _ = std::fs::create_dir_all(&dir);
                    let db_path = dir.join("gaze20.db");
                    let conn = match db::open(&db_path) {
                        Ok(conn) => Some(conn),
                        Err(error) => {
                            log::error!("database open failed: {error}; recovering");
                            db::recover_and_open(&db_path).ok()
                        }
                    };
                    if let Some(conn) = conn {
                        db::daily_backup(&db_path); // rolling 24h backup
                        let _ = db::prune_old(&conn, 90); // keep 90d of fine-grained events
                        app.manage(db::Database {
                            conn: std::sync::Mutex::new(conn),
                        });
                    }
                }
                Err(error) => log::error!("app_data_dir failed: {error}"),
            }

            // One-time import of the legacy gaze20-data.json (before the engine
            // loads, so a fresh install can seed engine settings from it).
            import_legacy_json(app.handle());

            // Rust now owns the state machine: load the persisted engine (resumes
            // mid-day), start the 1s authority loop, and resolve native-overlay
            // button/timeout actions into the engine + the reminder_events log.
            let engine_state = load_engine(app.handle());
            app.manage(EngineHandle {
                engine: std::sync::Mutex::new(engine_state),
                snapshot: std::sync::Mutex::new(fallback_snapshot()),
            });
            spawn_engine_loop(app.handle().clone());
            {
                let action_app = app.handle().clone();
                app.listen("overlay-action", move |event| {
                    handle_overlay_action(&action_app, event.payload());
                });
            }

            let show = MenuItem::with_id(app, "show", "打开远眺", true, None::<&str>)?;
            let remind = MenuItem::with_id(app, "remind", "立即远眺", true, None::<&str>)?;
            let pause = MenuItem::with_id(app, "pause", "暂停记录", true, None::<&str>)?;
            let resume = MenuItem::with_id(app, "resume", "继续记录", true, None::<&str>)?;
            let quit = MenuItem::with_id(app, "quit", "退出", true, None::<&str>)?;
            let menu = Menu::with_items(app, &[&show, &remind, &pause, &resume, &quit])?;

            let mut builder = TrayIconBuilder::with_id("gaze20")
                .tooltip("远眺 Gaze20")
                .menu(&menu)
                .show_menu_on_left_click(true)
                .on_menu_event(|app, event| match event.id().as_ref() {
                    "show" => show_main_window(app),
                    "remind" => {
                        let _ = app.emit("tray-action", "remind");
                    }
                    "pause" => {
                        let _ = app.emit("tray-action", "pause");
                    }
                    "resume" => {
                        let _ = app.emit("tray-action", "resume");
                    }
                    "quit" => {
                        flush_state(app);
                        app.exit(0);
                    }
                    _ => {}
                })
                .on_tray_icon_event(|tray, event| {
                    if let TrayIconEvent::DoubleClick { .. } = event {
                        show_main_window(tray.app_handle());
                    }
                });

            if let Some(icon) = app.default_window_icon() {
                builder = builder.icon(icon.clone());
            }

            builder.build(app)?;

            if std::env::var_os("GAZE20_SELF_TEST_REMINDER").is_some() {
                let handle = app.handle().clone();
                std::thread::spawn(move || {
                    std::thread::sleep(std::time::Duration::from_millis(1500));
                    fire_reminder(&handle, false, 20, 2, 88, 24);
                });
            }

            // Diagnostic: drive the same engine + async overlay path as
            // `engine_rest_now`, without relying on frontend listener timing.
            if std::env::var_os("GAZE20_SELF_TEST_TRAY").is_some() {
                let handle = app.handle().clone();
                std::thread::spawn(move || {
                    std::thread::sleep(std::time::Duration::from_millis(3500));
                    let fire = handle.try_state::<EngineHandle>().and_then(|eh| {
                        let mut e = eh.engine.lock().unwrap_or_else(|p| p.into_inner());
                        if e.reminding.is_some() {
                            None
                        } else {
                            e.reminding = Some(engine::Kind::Micro);
                            Some((
                                e.mode.preset().break_seconds as u32,
                                score_from_risk(e.risk),
                                eye_image_index(e.risk),
                                (e.continuous / 60.0).round() as i64,
                            ))
                        }
                    });
                    if let Some((seconds, score, image, focus_minutes)) = fire {
                        schedule_fire_reminder(handle, false, seconds, image, score, focus_minutes);
                    }
                });
            }

            Ok(())
        })
        .on_window_event(|window, event| {
            if window.label() == "main" {
                if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                    api.prevent_close();
                    let _ = window.hide();
                }
            }
        })
        .invoke_handler(tauri::generate_handler![
            get_activity_snapshot,
            notify,
            load_app_data,
            save_app_data,
            set_autostart,
            get_autostart,
            show_reminder_overlay,
            overlay_action,
            overlay_start,
            get_display_count,
            db_get_settings,
            db_set_setting,
            db_schema_version,
            db_add_symptom,
            db_recent_symptoms,
            db_recent_reminders,
            db_daily_stats,
            db_hourly_stats,
            db_export,
            check_for_update,
            engine_get_state,
            engine_set_running,
            engine_set_mode,
            engine_set_dnd,
            engine_set_debug_fast,
            engine_set_detection,
            engine_snooze,
            engine_reset_today,
            engine_rest_now
        ])
        .run(tauri::generate_context!())
        .expect("error while running Gaze20");
}

fn show_main_window<R: Runtime>(app: &AppHandle<R>) {
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.show();
        let _ = window.set_focus();
    }
}

#[cfg(windows)]
fn platform_activity_snapshot(reading_grace_seconds: u64, away_seconds: u64) -> ActivitySnapshot {
    windows_activity::snapshot(reading_grace_seconds, away_seconds)
}

#[cfg(not(windows))]
fn platform_activity_snapshot(_reading_grace_seconds: u64, _away_seconds: u64) -> ActivitySnapshot {
    ActivitySnapshot {
        idle_seconds: 0.0,
        foreground_title: "Unsupported platform preview".into(),
        foreground_process: "unknown".into(),
        is_fullscreen: false,
        input_active: true,
        reading_active: false,
        eye_activity_weight: 1.0,
        should_defer: false,
        reason: "非 Windows 平台预览：模拟活跃".into(),
        captured_at_ms: now_ms(),
    }
}

#[cfg(windows)]
fn set_platform_autostart<R: Runtime>(_app: &AppHandle<R>, enabled: bool) -> Result<(), String> {
    use winreg::enums::{HKEY_CURRENT_USER, KEY_WRITE};
    use winreg::RegKey;

    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let (run_key, _) = hkcu
        .create_subkey_with_flags("Software\\Microsoft\\Windows\\CurrentVersion\\Run", KEY_WRITE)
        .map_err(|error| error.to_string())?;
    if enabled {
        let exe = std::env::current_exe().map_err(|error| error.to_string())?;
        run_key
            .set_value("Gaze20", &format!("\"{}\"", exe.display()))
            .map_err(|error| error.to_string())
    } else {
        match run_key.delete_value("Gaze20") {
            Ok(_) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error.to_string()),
        }
    }
}

#[cfg(not(windows))]
fn set_platform_autostart<R: Runtime>(_app: &AppHandle<R>, _enabled: bool) -> Result<(), String> {
    Ok(())
}

#[cfg(windows)]
fn get_platform_autostart() -> Result<bool, String> {
    use winreg::enums::{HKEY_CURRENT_USER, KEY_READ};
    use winreg::RegKey;

    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let run_key = hkcu
        .open_subkey_with_flags("Software\\Microsoft\\Windows\\CurrentVersion\\Run", KEY_READ)
        .map_err(|error| error.to_string())?;
    Ok(run_key.get_value::<String, _>("Gaze20").is_ok())
}

#[cfg(not(windows))]
fn get_platform_autostart() -> Result<bool, String> {
    Ok(false)
}

fn now_ms() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or_default()
}

#[cfg(windows)]
mod windows_activity {
    use super::{now_ms, ActivitySnapshot};
    use std::path::Path;
    use windows::core::PWSTR;
    use windows::Win32::Foundation::{CloseHandle, HWND, RECT};
    use windows::Win32::System::SystemInformation::GetTickCount64;
    use windows::Win32::System::Threading::{
        OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_FORMAT,
        PROCESS_QUERY_LIMITED_INFORMATION,
    };
    use windows::Win32::UI::Input::KeyboardAndMouse::{GetLastInputInfo, LASTINPUTINFO};
    use windows::Win32::UI::WindowsAndMessaging::{
        GetForegroundWindow, GetSystemMetrics, GetWindowRect, GetWindowTextLengthW,
        GetWindowTextW, GetWindowThreadProcessId, SM_CXSCREEN, SM_CYSCREEN,
    };

    pub fn snapshot(reading_grace_seconds: u64, away_seconds: u64) -> ActivitySnapshot {
        let idle_seconds = last_input_idle_seconds();
        let hwnd = unsafe { GetForegroundWindow() };
        let title = window_title(hwnd);
        let process = process_name(hwnd);
        let fullscreen = is_fullscreen(hwnd);
        let title_lower = title.to_lowercase();
        let process_lower = process.to_lowercase();

        let defer_by_process = contains_any(
            &process_lower,
            &[
                "zoom",
                "teams",
                "powerpnt",
                "obs",
                "vlc",
                "potplayer",
                "steam",
                "game",
            ],
        ) || contains_any(&title_lower, &["会议", "演示", "共享", "放映", "全屏"]);
        let reading_like = contains_any(
            &process_lower,
            &[
                "chrome",
                "msedge",
                "firefox",
                "code",
                "cursor",
                "devenv",
                "idea",
                "webstorm",
                "pycharm",
                "notepad",
                "winword",
                "wps",
                "acrord",
                "sumatra",
                "pdf",
                "obsidian",
            ],
        ) || !title.trim().is_empty();

        let input_active = idle_seconds <= 60.0;
        let reading_active =
            !input_active && idle_seconds <= reading_grace_seconds as f64 && reading_like;
        let away = idle_seconds >= away_seconds as f64;
        let should_defer = fullscreen || defer_by_process;
        let (weight, reason) = if away {
            (0.0, "已离开电脑，暂停累计用眼时间".to_string())
        } else if input_active {
            (
                1.0,
                format!(
                    "键盘/鼠标活跃，前台 {}",
                    visible_process_label(&process, &title)
                ),
            )
        } else if reading_active && should_defer {
            (
                0.45,
                "正在全屏/会议/演示，继续轻量计时并延后提醒".to_string(),
            )
        } else if reading_active {
            (
                0.7,
                format!(
                    "无输入但疑似阅读/看文档，前台 {}",
                    visible_process_label(&process, &title)
                ),
            )
        } else {
            (
                0.0,
                format!("空闲 {:.0} 秒，暂不累计用眼时间", idle_seconds),
            )
        };

        ActivitySnapshot {
            idle_seconds,
            foreground_title: title,
            foreground_process: process,
            is_fullscreen: fullscreen,
            input_active,
            reading_active,
            eye_activity_weight: weight,
            should_defer,
            reason,
            captured_at_ms: now_ms(),
        }
    }

    fn last_input_idle_seconds() -> f64 {
        let mut info = LASTINPUTINFO {
            cbSize: std::mem::size_of::<LASTINPUTINFO>() as u32,
            dwTime: 0,
        };
        let ok = unsafe { GetLastInputInfo(&mut info).as_bool() };
        if !ok {
            return 0.0;
        }
        let tick = unsafe { GetTickCount64() };
        let idle_ms = tick.saturating_sub(info.dwTime as u64);
        idle_ms as f64 / 1000.0
    }

    fn window_title(hwnd: HWND) -> String {
        if hwnd.0.is_null() {
            return String::new();
        }
        let len = unsafe { GetWindowTextLengthW(hwnd) };
        if len <= 0 {
            return String::new();
        }
        let mut buffer = vec![0u16; len as usize + 1];
        let read = unsafe { GetWindowTextW(hwnd, &mut buffer) };
        String::from_utf16_lossy(&buffer[..read as usize])
    }

    fn process_name(hwnd: HWND) -> String {
        if hwnd.0.is_null() {
            return String::new();
        }
        let mut pid = 0u32;
        unsafe {
            GetWindowThreadProcessId(hwnd, Some(&mut pid));
        }
        if pid == 0 {
            return String::new();
        }
        let handle = match unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) } {
            Ok(handle) => handle,
            Err(_) => return String::new(),
        };
        let mut buffer = vec![0u16; 32768];
        let mut size = buffer.len() as u32;
        let ok = unsafe {
            QueryFullProcessImageNameW(
                handle,
                PROCESS_NAME_FORMAT(0),
                PWSTR(buffer.as_mut_ptr()),
                &mut size,
            )
            .is_ok()
        };
        let _ = unsafe { CloseHandle(handle) };
        if !ok || size == 0 {
            return String::new();
        }
        let full = String::from_utf16_lossy(&buffer[..size as usize]);
        Path::new(&full)
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or(&full)
            .to_string()
    }

    fn is_fullscreen(hwnd: HWND) -> bool {
        if hwnd.0.is_null() {
            return false;
        }
        let mut rect = RECT::default();
        let ok = unsafe { GetWindowRect(hwnd, &mut rect).is_ok() };
        if !ok {
            return false;
        }
        let screen_w = unsafe { GetSystemMetrics(SM_CXSCREEN) };
        let screen_h = unsafe { GetSystemMetrics(SM_CYSCREEN) };
        rect.left <= 0 && rect.top <= 0 && rect.right >= screen_w && rect.bottom >= screen_h
    }

    fn contains_any(value: &str, needles: &[&str]) -> bool {
        needles.iter().any(|needle| value.contains(needle))
    }

    /// Privacy: the displayed/persisted reason references the foreground *process*
    /// only — never the window *title*. The title is used solely in-memory for
    /// meeting/fullscreen keyword detection and is never shown or written to disk.
    fn visible_process_label(process: &str, _title: &str) -> String {
        if process.is_empty() {
            "未知窗口".to_string()
        } else {
            process.to_string()
        }
    }
}
