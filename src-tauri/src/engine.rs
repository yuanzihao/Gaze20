//! The eye-usage state machine, ported from the frontend into Rust so timing and
//! reminder decisions survive restarts and are recorded as durable facts.
//!
//! This module is **pure logic** — no Tauri, no DB, no clock of its own. `lib.rs`
//! drives it: each tick it feeds a real activity snapshot + elapsed time, gets back
//! a [`Decision`], and performs the side effects (fire a native reminder, write a
//! `reminder_events` row, emit live state). That keeps the rules unit-testable.

// Wired into the live tick loop in Phase 2c; until then the public API is only
// exercised by the unit tests.
#![allow(dead_code)]

use serde::{Deserialize, Serialize};

const SECS_PER_MIN: f64 = 60.0;

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    Conservative,
    Balanced,
    Intense,
}

impl Mode {
    pub fn from_str(s: &str) -> Mode {
        match s {
            "conservative" => Mode::Conservative,
            "intense" => Mode::Intense,
            _ => Mode::Balanced,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Mode::Conservative => "conservative",
            Mode::Balanced => "balanced",
            Mode::Intense => "intense",
        }
    }

    pub fn preset(self) -> Preset {
        match self {
            Mode::Conservative => Preset { micro_minutes: 30.0, deep_minutes: 90.0, break_seconds: 20.0, deep_break_minutes: 3.0 },
            Mode::Balanced => Preset { micro_minutes: 20.0, deep_minutes: 60.0, break_seconds: 20.0, deep_break_minutes: 3.0 },
            Mode::Intense => Preset { micro_minutes: 15.0, deep_minutes: 45.0, break_seconds: 30.0, deep_break_minutes: 5.0 },
        }
    }
}

#[derive(Clone, Copy)]
pub struct Preset {
    pub micro_minutes: f64,
    pub deep_minutes: f64,
    pub break_seconds: f64,
    pub deep_break_minutes: f64,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Micro,
    Deep,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Micro => "micro",
            Kind::Deep => "deep",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Reminder {
    /// Result of a finished reminder, applied via [`Engine::resolve`].
    Completed,
    Postponed,
    Skipped,
}

/// What a tick wants the host (lib.rs) to do. Exactly one thing per tick.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Decision {
    None,
    /// Show a reminder of this kind for `seconds` (host fires the native overlay).
    Fire { deep: bool, seconds: u32 },
    /// A due micro-break was auto-deferred (meeting/fullscreen/etc.); host records
    /// a `deferred` reminder_event.
    Deferred,
}

/// The full runtime state. Persistent "today" counters round-trip through JSON
/// (a settings row) so a restart resumes mid-day; transient fields reset on load.
#[derive(Clone, Serialize, Deserialize)]
pub struct Engine {
    pub mode: Mode,
    pub running: bool,
    pub do_not_disturb: bool,
    pub debug_fast_mode: bool,
    pub reading_grace_minutes: f64,
    pub away_minutes: f64,

    pub date: String, // YYYY-MM-DD this row's counters belong to
    pub screen_seconds: f64,
    pub micro_active: f64,
    pub deep_active: f64,
    pub continuous: f64,
    pub micro_done: i64,
    pub deep_done: i64,
    pub micro_due: i64,
    pub deep_due: i64,
    pub distant_gaze: f64,
    pub postponed: i64,
    pub skipped: i64,
    pub deferred: i64,
    pub risk: i64,
    pub streak_days: i64,

    // Transient: never persisted, reset to defaults on load.
    #[serde(skip)]
    pub snooze_until_ms: u128,
    #[serde(skip)]
    pub reminding: Option<Kind>,
    /// Weighted active seconds accrued on the most recent tick (0 if none). The
    /// host reads this to bump the per-hour heatmap bucket.
    #[serde(skip)]
    pub last_tick_accumulated: f64,
}

impl Default for Engine {
    fn default() -> Self {
        Engine {
            mode: Mode::Balanced,
            running: true,
            do_not_disturb: false,
            debug_fast_mode: false,
            reading_grace_minutes: 8.0,
            away_minutes: 12.0,
            date: String::new(),
            screen_seconds: 0.0,
            micro_active: 0.0,
            deep_active: 0.0,
            continuous: 0.0,
            micro_done: 0,
            deep_done: 0,
            micro_due: 0,
            deep_due: 0,
            distant_gaze: 0.0,
            postponed: 0,
            skipped: 0,
            deferred: 0,
            risk: 18,
            streak_days: 1,
            snooze_until_ms: 0,
            reminding: None,
            last_tick_accumulated: 0.0,
        }
    }
}

fn clamp(v: f64, lo: f64, hi: f64) -> f64 {
    v.max(lo).min(hi)
}

impl Engine {
    pub fn effective_micro_seconds(&self) -> f64 {
        if self.debug_fast_mode {
            45.0
        } else {
            self.mode.preset().micro_minutes * SECS_PER_MIN
        }
    }

    pub fn effective_deep_seconds(&self) -> f64 {
        if self.debug_fast_mode {
            120.0
        } else {
            self.mode.preset().deep_minutes * SECS_PER_MIN
        }
    }

    /// Risk score 0-100. Mirrors the frontend `computeRisk` exactly, including that
    /// it always uses the configured minutes (not the debug-fast thresholds).
    pub fn compute_risk(&self, continuous: f64, micro_active: f64) -> i64 {
        let preset = self.mode.preset();
        let continuous_risk = clamp(continuous / (90.0 * 60.0), 0.0, 1.0) * 30.0;
        let total_risk = clamp(self.screen_seconds / (8.0 * 3600.0), 0.0, 1.0) * 20.0;
        let micro_due = (self.screen_seconds / (preset.micro_minutes * SECS_PER_MIN))
            .floor()
            .max(1.0);
        let done_rate = self.micro_done as f64 / micro_due;
        let completion_risk = (1.0 - clamp(done_rate, 0.0, 1.0)) * 20.0;
        let gaze_target = (self.micro_done as f64 * preset.break_seconds).max(1.0);
        let gaze_risk = (1.0 - clamp(self.distant_gaze / gaze_target, 0.0, 1.0)) * 10.0;
        let pressure_risk = clamp(micro_active / (preset.micro_minutes * SECS_PER_MIN), 0.0, 1.0) * 20.0;
        clamp(
            continuous_risk + total_risk + completion_risk + gaze_risk + pressure_risk,
            0.0,
            100.0,
        )
        .round() as i64
    }

    /// Advance one tick. `dt` is elapsed seconds (the host clamps it), `eye_weight`
    /// is the snapshot's eye-activity weight, `should_defer` is true during
    /// meetings/fullscreen/away. Returns the single side effect to perform.
    pub fn tick(&mut self, dt: f64, eye_weight: f64, should_defer: bool, now_ms: u128) -> Decision {
        self.last_tick_accumulated = 0.0;
        // No accumulation or triggering while a reminder is on screen or paused.
        if !self.running || self.reminding.is_some() {
            return Decision::None;
        }

        let weighted = dt * clamp(eye_weight, 0.0, 1.0);
        if weighted > 0.02 {
            self.last_tick_accumulated = weighted;
            self.micro_active += weighted;
            self.deep_active += weighted;
            self.continuous += weighted;
            self.screen_seconds += weighted;
            let eff_micro = self.effective_micro_seconds();
            let eff_deep = self.effective_deep_seconds();
            self.micro_due = self.micro_due.max((self.screen_seconds / eff_micro).floor() as i64);
            self.deep_due = self.deep_due.max((self.screen_seconds / eff_deep).floor() as i64);
            self.risk = self.compute_risk(self.continuous, self.micro_active);
        }

        let eff_micro = self.effective_micro_seconds();
        let eff_deep = self.effective_deep_seconds();
        let can_remind = self.running && !self.do_not_disturb && !should_defer && now_ms > self.snooze_until_ms;

        // Meeting/fullscreen/away: a due micro-break is auto-deferred, not forced.
        if should_defer && self.micro_active >= eff_micro {
            self.deferred += 1;
            self.micro_active = eff_micro - 120.0;
            return Decision::Deferred;
        }
        if can_remind && self.deep_active >= eff_deep {
            self.reminding = Some(Kind::Deep);
            let preset = self.mode.preset();
            return Decision::Fire {
                deep: true,
                seconds: (preset.deep_break_minutes * SECS_PER_MIN) as u32,
            };
        }
        if can_remind && self.micro_active >= eff_micro {
            self.reminding = Some(Kind::Micro);
            let preset = self.mode.preset();
            return Decision::Fire {
                deep: false,
                seconds: preset.break_seconds as u32,
            };
        }
        Decision::None
    }

    /// Apply the outcome of the on-screen reminder. Returns the resolved kind and
    /// the gaze-seconds credited (so the host can write the `reminder_events` row),
    /// or `None` if no reminder was active (idempotent against duplicate actions).
    pub fn resolve(&mut self, result: Reminder, now_ms: u128) -> Option<(Kind, f64)> {
        let kind = match self.reminding {
            Some(k) => k,
            None => return None,
        };
        let preset = self.mode.preset();
        let mut gaze_credit = 0.0;
        match result {
            Reminder::Completed => {
                match kind {
                    Kind::Micro => {
                        self.micro_done += 1;
                        gaze_credit = preset.break_seconds;
                        self.distant_gaze += gaze_credit;
                        self.risk = self.compute_risk(self.continuous, 0.0);
                    }
                    Kind::Deep => {
                        self.deep_done += 1;
                        self.deep_active = 0.0;
                        self.continuous = 0.0;
                        self.risk = self.compute_risk(0.0, 0.0);
                    }
                }
                self.micro_active = 0.0;
            }
            Reminder::Postponed => {
                self.postponed += 1;
                self.snooze_until_ms = now_ms + 5 * 60 * 1000;
            }
            Reminder::Skipped => {
                self.skipped += 1;
                match kind {
                    Kind::Micro => self.micro_active = 0.0,
                    Kind::Deep => self.deep_active = 0.0,
                }
            }
        }
        self.reminding = None;
        Some((kind, gaze_credit))
    }

    /// If the wall-clock day changed, hand back the finished day's counters (for the
    /// host to archive into daily_stats — Phase 3) and reset to a fresh day.
    pub fn roll_over(&mut self, today: &str) -> Option<Engine> {
        if self.date == today {
            return None;
        }
        let had_real_day = !self.date.is_empty() && self.screen_seconds > 0.0;
        let finished = if had_real_day { Some(self.clone()) } else { None };
        let keep_mode = self.mode;
        let keep_running = self.running;
        let keep_dnd = self.do_not_disturb;
        let keep_debug = self.debug_fast_mode;
        let keep_streak = self.streak_days;
        *self = Engine::default();
        self.mode = keep_mode;
        self.running = keep_running;
        self.do_not_disturb = keep_dnd;
        self.debug_fast_mode = keep_debug;
        self.streak_days = keep_streak;
        self.date = today.to_string();
        finished
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn balanced() -> Engine {
        Engine {
            date: "2026-06-16".into(),
            ..Engine::default()
        }
    }

    #[test]
    fn accumulates_weighted_active_time() {
        let mut e = balanced();
        e.tick(1.0, 1.0, false, 1000);
        e.tick(1.0, 0.5, false, 2000);
        // 1.0*1.0 + 1.0*0.5 = 1.5
        assert!((e.screen_seconds - 1.5).abs() < 1e-9);
        assert!((e.micro_active - 1.5).abs() < 1e-9);
    }

    #[test]
    fn near_zero_weight_does_not_accumulate() {
        let mut e = balanced();
        e.tick(1.0, 0.0, false, 1000);
        assert_eq!(e.screen_seconds, 0.0);
    }

    #[test]
    fn fires_micro_at_threshold() {
        let mut e = balanced();
        e.debug_fast_mode = true; // micro threshold = 45s
        let mut fired = None;
        for i in 0..60 {
            let d = e.tick(1.0, 1.0, false, (i as u128 + 1) * 1000);
            if let Decision::Fire { deep, seconds } = d {
                fired = Some((deep, seconds));
                break;
            }
        }
        assert_eq!(fired, Some((false, 20))); // micro, 20s break
        assert_eq!(e.reminding, Some(Kind::Micro));
    }

    #[test]
    fn defers_instead_of_firing_when_should_defer() {
        let mut e = balanced();
        e.debug_fast_mode = true;
        let mut saw_defer = false;
        for i in 0..60 {
            let d = e.tick(1.0, 1.0, true, (i as u128 + 1) * 1000);
            match d {
                Decision::Deferred => {
                    saw_defer = true;
                    break;
                }
                Decision::Fire { .. } => panic!("should not fire while deferring"),
                Decision::None => {}
            }
        }
        assert!(saw_defer);
        assert_eq!(e.deferred, 1);
        assert!(e.reminding.is_none());
    }

    #[test]
    fn complete_micro_credits_gaze_and_resets_micro() {
        let mut e = balanced();
        e.debug_fast_mode = true;
        for i in 0..60 {
            if let Decision::Fire { .. } = e.tick(1.0, 1.0, false, (i as u128 + 1) * 1000) {
                break;
            }
        }
        let out = e.resolve(Reminder::Completed, 100_000);
        assert_eq!(out, Some((Kind::Micro, 20.0))); // micro, break_seconds gaze
        assert_eq!(e.micro_done, 1);
        assert_eq!(e.micro_active, 0.0);
        assert!(e.reminding.is_none());
        assert!((e.distant_gaze - 20.0).abs() < 1e-9);
    }

    #[test]
    fn postpone_sets_snooze_window() {
        let mut e = balanced();
        e.reminding = Some(Kind::Micro);
        e.resolve(Reminder::Postponed, 1_000_000);
        assert_eq!(e.postponed, 1);
        assert_eq!(e.snooze_until_ms, 1_000_000 + 5 * 60 * 1000);
    }

    #[test]
    fn risk_rises_with_continuous_load() {
        let mut e = balanced();
        e.screen_seconds = 3.0 * 3600.0;
        let low = e.compute_risk(0.0, 0.0);
        let high = e.compute_risk(90.0 * 60.0, 0.0);
        assert!(high > low);
        assert!((0..=100).contains(&high));
    }

    #[test]
    fn roll_over_resets_counters_but_keeps_settings() {
        let mut e = balanced();
        e.screen_seconds = 5000.0;
        e.mode = Mode::Intense;
        e.streak_days = 7;
        let finished = e.roll_over("2026-06-17");
        assert!(finished.is_some());
        assert_eq!(finished.unwrap().screen_seconds, 5000.0);
        assert_eq!(e.screen_seconds, 0.0);
        assert_eq!(e.date, "2026-06-17");
        assert_eq!(e.mode, Mode::Intense); // settings preserved
        assert_eq!(e.streak_days, 7);
    }
}
