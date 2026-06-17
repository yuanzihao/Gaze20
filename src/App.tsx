import {
  Activity,
  AlarmClock,
  BarChart3,
  Bell,
  BellOff,
  Check,
  ChevronRight,
  Clock3,
  Coffee,
  Download,
  Eye,
  FileText,
  Flame,
  Gauge,
  Keyboard,
  MonitorPlay,
  Moon,
  Pause,
  Play,
  RotateCcw,
  Settings2,
  ShieldCheck,
  SkipForward,
  Sparkles,
  TimerReset,
  X
} from "lucide-react";
import React, {
  useCallback,
  useEffect,
  useMemo,
  useRef,
  useState,
  type ReactNode
} from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";

type ViewId = "overview" | "reminders" | "trends" | "symptoms" | "settings";
type ModeId = "conservative" | "balanced" | "intense";
type ReminderKind = "micro" | "deep";
type ReminderResult = "completed" | "postponed" | "skipped" | "deferred";
type SymptomKind = "dry" | "blur" | "headache" | "neck";

type ModePreset = {
  id: ModeId;
  label: string;
  sub: string;
  microMinutes: number;
  deepMinutes: number;
  breakSeconds: number;
  deepBreakMinutes: number;
};

type Settings = {
  mode: ModeId;
  notifications: boolean;
  doNotDisturb: boolean;
  autoStart: boolean;
  readingGraceMinutes: number;
  awayMinutes: number;
  debugFastMode: boolean;
};

type ActivitySnapshot = {
  idleSeconds: number;
  foregroundTitle: string;
  foregroundProcess: string;
  isFullscreen: boolean;
  inputActive: boolean;
  readingActive: boolean;
  eyeActivityWeight: number;
  shouldDefer: boolean;
  reason: string;
  capturedAtMs: number;
};

type ReminderLog = {
  id: string;
  at: string;
  kind: ReminderKind;
  result: ReminderResult;
  activeSeconds: number;
  note: string;
};

type SymptomRecord = {
  id: string;
  at: string;
  scores: Record<SymptomKind, number>;
  note: string;
  screenSeconds: number;
};

type DayStats = {
  date: string;
  screenSeconds: number;
  distantGazeSeconds: number;
  microDue: number;
  microDone: number;
  deepDue: number;
  deepDone: number;
  skipped: number;
  postponed: number;
  deferred: number;
  riskScore: number;
};

type PersistedData = {
  version: number;
  settings: Settings;
  today: DayStats;
  logs: ReminderLog[];
  symptoms: SymptomRecord[];
  streakDays: number;
};

const presets: ModePreset[] = [
  {
    id: "conservative",
    label: "保守",
    sub: "会议多，尽量少打扰",
    microMinutes: 30,
    deepMinutes: 90,
    breakSeconds: 20,
    deepBreakMinutes: 3
  },
  {
    id: "balanced",
    label: "平衡",
    sub: "默认推荐，提醒适中",
    microMinutes: 20,
    deepMinutes: 60,
    breakSeconds: 20,
    deepBreakMinutes: 3
  },
  {
    id: "intense",
    label: "激进",
    sub: "干涩明显，强化休息",
    microMinutes: 15,
    deepMinutes: 45,
    breakSeconds: 30,
    deepBreakMinutes: 5
  }
];

const defaultSettings: Settings = {
  mode: "balanced",
  notifications: true,
  doNotDisturb: false,
  autoStart: false,
  readingGraceMinutes: 8,
  awayMinutes: 12,
  debugFastMode: false
};

const todayKey = () => new Date().toISOString().slice(0, 10);

function emptyStats(): DayStats {
  return {
    date: todayKey(),
    screenSeconds: 0,
    distantGazeSeconds: 0,
    microDue: 0,
    microDone: 0,
    deepDue: 0,
    deepDone: 0,
    skipped: 0,
    postponed: 0,
    deferred: 0,
    riskScore: 18
  };
}

function defaultData(): PersistedData {
  return {
    version: 3,
    settings: defaultSettings,
    today: emptyStats(),
    logs: [],
    symptoms: [],
    streakDays: 1
  };
}

function migrateData(parsed: Partial<PersistedData>): PersistedData {
  const today = parsed.today?.date === todayKey()
    ? { ...emptyStats(), ...parsed.today, date: todayKey() }
    : emptyStats();

  return {
    ...defaultData(),
    ...parsed,
    version: 3,
    settings: { ...defaultSettings, ...(parsed.settings ?? {}) },
    today,
    logs: parsed.logs ?? [],
    symptoms: parsed.symptoms ?? []
  };
}

function clamp(value: number, min: number, max: number) {
  return Math.max(min, Math.min(max, value));
}

function formatDuration(seconds: number, compact = false) {
  const s = Math.max(0, Math.floor(seconds));
  const h = Math.floor(s / 3600);
  const m = Math.floor((s % 3600) / 60);
  const sec = s % 60;
  if (h > 0) return compact ? `${h}h ${m}m` : `${h} 小时 ${m} 分`;
  if (compact) return `${m}m ${sec}s`;
  return `${String(m).padStart(2, "0")}:${String(sec).padStart(2, "0")}`;
}

function riskLabel(score: number) {
  if (score >= 70) return "高";
  if (score >= 40) return "中";
  return "低";
}

function riskTone(score: number) {
  if (score >= 70) return "danger";
  if (score >= 40) return "warn";
  return "ok";
}

function modeToPreset(mode: ModeId) {
  return presets.find((item) => item.id === mode) ?? presets[1];
}

function makeFallbackSnapshot(): ActivitySnapshot {
  return {
    idleSeconds: 0,
    foregroundTitle: "浏览器预览模式",
    foregroundProcess: "browser-preview",
    isFullscreen: false,
    inputActive: true,
    readingActive: false,
    eyeActivityWeight: 1,
    shouldDefer: false,
    reason: "预览模式：模拟键鼠活跃",
    capturedAtMs: Date.now()
  };
}

function computeRisk(
  stats: DayStats,
  continuousSeconds: number,
  microActiveSeconds: number,
  preset: ModePreset
) {
  const continuousRisk = clamp(continuousSeconds / (90 * 60), 0, 1) * 30;
  const totalRisk = clamp(stats.screenSeconds / (8 * 3600), 0, 1) * 20;
  const microDue = Math.max(1, Math.floor(stats.screenSeconds / (preset.microMinutes * 60)));
  const doneRate = stats.microDone / microDue;
  const completionRisk = (1 - clamp(doneRate, 0, 1)) * 20;
  const gazeTarget = Math.max(1, stats.microDone * preset.breakSeconds);
  const gazeRisk = (1 - clamp(stats.distantGazeSeconds / gazeTarget, 0, 1)) * 10;
  const pressureRisk = clamp(microActiveSeconds / (preset.microMinutes * 60), 0, 1) * 20;
  return Math.round(
    clamp(continuousRisk + totalRisk + completionRisk + gazeRisk + pressureRisk, 0, 100)
  );
}

function scoreFromRisk(risk: number) {
  return clamp(Math.round(100 - risk * 0.72), 18, 100);
}

function eyeImageIndex(risk: number) {
  return clamp(Math.round((risk / 100) * 9), 0, 9);
}

async function safeInvoke<T>(cmd: string, args?: Record<string, unknown>) {
  try {
    return await invoke<T>(cmd, args);
  } catch {
    return null;
  }
}

export default function App() {
  // Make this feel like a native app, not a web page: no right-click menu, and no
  // browser reload via F5 / Ctrl+R (would wipe live in-memory UI state).
  useEffect(() => {
    const blockMenu = (event: MouseEvent) => event.preventDefault();
    const blockReload = (event: KeyboardEvent) => {
      const reload = event.key === "F5" || ((event.ctrlKey || event.metaKey) && event.key.toLowerCase() === "r");
      if (reload) event.preventDefault();
    };
    window.addEventListener("contextmenu", blockMenu);
    window.addEventListener("keydown", blockReload);
    return () => {
      window.removeEventListener("contextmenu", blockMenu);
      window.removeEventListener("keydown", blockReload);
    };
  }, []);

  return <MainApp />;
}

// Live state pushed by the Rust engine (camelCase mirrors lib.rs `LiveState`).
type LiveState = {
  mode: ModeId;
  running: boolean;
  doNotDisturb: boolean;
  debugFastMode: boolean;
  readingGraceMinutes: number;
  awayMinutes: number;
  date: string;
  screenSeconds: number;
  microActive: number;
  deepActive: number;
  continuous: number;
  microDone: number;
  deepDone: number;
  microDue: number;
  deepDue: number;
  distantGaze: number;
  postponed: number;
  skipped: number;
  deferred: number;
  risk: number;
  eyeScore: number;
  imageIndex: number;
  streakDays: number;
  effectiveMicroSeconds: number;
  effectiveDeepSeconds: number;
  nextMicroSeconds: number;
  nextDeepSeconds: number;
  reminding: string | null;
  snoozeActive: boolean;
  reason: string;
  foregroundProcess: string;
  shouldDefer: boolean;
  isFullscreen: boolean;
};

type DbReminderRow = {
  id: number;
  at: string;
  kind: string;
  result: string;
  activeSeconds: number;
  gazeSeconds: number;
  note: string | null;
};

type DbSymptomRow = {
  id: number;
  at: string;
  dry: number;
  blur: number;
  headache: number;
  neck: number;
  note: string | null;
  screenSeconds: number;
};

type DbDailyRow = {
  date: string;
  screenSeconds: number;
  distantGazeSeconds: number;
  microDue: number;
  microDone: number;
  deepDue: number;
  deepDone: number;
  skipped: number;
  postponed: number;
  deferred: number;
  riskScore: number;
  riskPeak: number;
};

type DbHourlyRow = { date: string; hour: number; screenSeconds: number };

function localIsoDate(d: Date): string {
  const p = (n: number) => String(n).padStart(2, "0");
  return `${d.getFullYear()}-${p(d.getMonth() + 1)}-${p(d.getDate())}`;
}

function dbToLog(row: DbReminderRow): ReminderLog {
  return {
    id: String(row.id),
    at: row.at,
    kind: row.kind === "deep" ? "deep" : "micro",
    result: row.result as ReminderResult,
    activeSeconds: row.activeSeconds,
    note: row.note ?? ""
  };
}

function dbToSymptom(row: DbSymptomRow): SymptomRecord {
  return {
    id: String(row.id),
    at: row.at,
    scores: { dry: row.dry, blur: row.blur, headache: row.headache, neck: row.neck },
    note: row.note ?? "",
    screenSeconds: row.screenSeconds
  };
}

function liveToSnapshot(live: LiveState): ActivitySnapshot {
  return {
    idleSeconds: 0,
    foregroundTitle: "",
    foregroundProcess: live.foregroundProcess,
    isFullscreen: live.isFullscreen,
    inputActive: true,
    readingActive: false,
    eyeActivityWeight: 1,
    shouldDefer: live.shouldDefer,
    reason: live.reason,
    capturedAtMs: Date.now()
  };
}

function MainApp() {
  const [view, setView] = useState<ViewId>("overview");
  // The Rust engine owns the state machine; `data` is a view-model fed from it.
  const [data, setData] = useState<PersistedData>(() => defaultData());
  const [snapshot, setSnapshot] = useState<ActivitySnapshot>(() => makeFallbackSnapshot());
  const [running, setRunning] = useState(true);
  const [microActiveSeconds, setMicroActiveSeconds] = useState(0);
  const [deepActiveSeconds, setDeepActiveSeconds] = useState(0);
  const [continuousSeconds, setContinuousSeconds] = useState(0);
  const [engineeringOpen, setEngineeringOpen] = useState(false);
  const [displayCount, setDisplayCount] = useState<number | null>(null);

  const preset = modeToPreset(data.settings.mode);
  const effectiveMicroSeconds = data.settings.debugFastMode ? 45 : preset.microMinutes * 60;
  const effectiveDeepSeconds = data.settings.debugFastMode ? 120 : preset.deepMinutes * 60;
  const eyeScore = scoreFromRisk(data.today.riskScore);
  const imageIndex = eyeImageIndex(data.today.riskScore);

  // Fold an engine LiveState payload into the local view-model. Engine-owned
  // fields (today counters + mode/dnd/detection/debug) come from here; local-only
  // fields (notifications, autoStart, logs, symptoms) are left untouched.
  const applyLive = useCallback((live: LiveState | null) => {
    if (!live) return;
    setRunning(live.running);
    setMicroActiveSeconds(live.microActive);
    setDeepActiveSeconds(live.deepActive);
    setContinuousSeconds(live.continuous);
    setSnapshot(liveToSnapshot(live));
    setData((current) => ({
      ...current,
      streakDays: live.streakDays,
      settings: {
        ...current.settings,
        mode: live.mode,
        doNotDisturb: live.doNotDisturb,
        debugFastMode: live.debugFastMode,
        readingGraceMinutes: live.readingGraceMinutes,
        awayMinutes: live.awayMinutes
      },
      today: {
        date: live.date,
        screenSeconds: live.screenSeconds,
        distantGazeSeconds: live.distantGaze,
        microDue: live.microDue,
        microDone: live.microDone,
        deepDue: live.deepDue,
        deepDone: live.deepDone,
        skipped: live.skipped,
        postponed: live.postponed,
        deferred: live.deferred,
        riskScore: live.risk
      }
    }));
  }, []);

  // The view components keep their `onChange(updater)` contract; engine-owned
  // settings changes and new symptom records are routed to Rust/DB here.
  const updateData = useCallback((updater: (current: PersistedData) => PersistedData) => {
    setData((current) => {
      const next = updater(current);
      const a = current.settings;
      const b = next.settings;
      if (a.mode !== b.mode) safeInvoke("engine_set_mode", { mode: b.mode });
      if (a.doNotDisturb !== b.doNotDisturb)
        safeInvoke("engine_set_dnd", { doNotDisturb: b.doNotDisturb });
      if (a.debugFastMode !== b.debugFastMode)
        safeInvoke("engine_set_debug_fast", { debugFastMode: b.debugFastMode });
      if (a.readingGraceMinutes !== b.readingGraceMinutes || a.awayMinutes !== b.awayMinutes)
        safeInvoke("engine_set_detection", {
          readingGraceMinutes: b.readingGraceMinutes,
          awayMinutes: b.awayMinutes
        });
      if (next.symptoms.length > current.symptoms.length) {
        const s = next.symptoms[0];
        safeInvoke("db_add_symptom", {
          dry: s.scores.dry,
          blur: s.scores.blur,
          headache: s.scores.headache,
          neck: s.scores.neck,
          note: s.note,
          screenSeconds: s.screenSeconds
        });
      }
      return next;
    });
  }, []);

  useEffect(() => {
    const toggleEngineering = (event: KeyboardEvent) => {
      if (event.ctrlKey && event.shiftKey && event.code === "KeyE") {
        event.preventDefault();
        setEngineeringOpen((open) => !open);
      }
    };
    window.addEventListener("keydown", toggleEngineering);
    return () => window.removeEventListener("keydown", toggleEngineering);
  }, []);

  useEffect(() => {
    if (!engineeringOpen) return;
    safeInvoke<number>("get_display_count").then((count) => {
      setDisplayCount(count);
    });
  }, [engineeringOpen]);

  // Subscribe to the engine's live state (≈1 Hz) and seed the initial state.
  useEffect(() => {
    safeInvoke<LiveState>("engine_get_state").then(applyLive);
    const unlisten = listen<LiveState>("engine-state", (event) => applyLive(event.payload));
    return () => {
      unlisten.then((dispose) => dispose()).catch(() => undefined);
    };
  }, [applyLive]);

  // Recent reminder events + symptom records come from the database.
  const refreshFacts = useCallback(async () => {
    const [logs, symptoms] = await Promise.all([
      safeInvoke<DbReminderRow[]>("db_recent_reminders", { limit: 80 }),
      safeInvoke<DbSymptomRow[]>("db_recent_symptoms", { limit: 240 })
    ]);
    setData((current) => ({
      ...current,
      logs: (logs ?? []).map(dbToLog),
      symptoms: (symptoms ?? []).map(dbToSymptom)
    }));
  }, []);

  useEffect(() => {
    refreshFacts();
    const handle = window.setInterval(refreshFacts, 8000);
    return () => window.clearInterval(handle);
  }, [refreshFacts]);

  useEffect(() => {
    safeInvoke<boolean>("get_autostart").then((enabled) => {
      if (enabled === null) return;
      setData((current) => ({
        ...current,
        settings: { ...current.settings, autoStart: enabled }
      }));
    });
  }, []);

  // Tray menu actions route to the engine (Rust owns the state machine).
  useEffect(() => {
    const unlisten = listen<string>("tray-action", (event) => {
      if (event.payload === "pause")
        safeInvoke<LiveState>("engine_set_running", { running: false }).then(applyLive);
      if (event.payload === "resume")
        safeInvoke<LiveState>("engine_set_running", { running: true }).then(applyLive);
      if (event.payload === "remind")
        safeInvoke<LiveState>("engine_rest_now").then(applyLive);
    });
    return () => {
      unlisten.then((dispose) => dispose()).catch(() => undefined);
    };
  }, [applyLive]);

  function startBreakNow() {
    safeInvoke<LiveState>("engine_rest_now").then(applyLive);
  }

  function snoozeReminders() {
    safeInvoke<LiveState>("engine_snooze", { minutes: 10 }).then(applyLive);
  }

  function toggleRunning() {
    safeInvoke<LiveState>("engine_set_running", { running: !running }).then(applyLive);
  }

  // Untracked preview: fire the native overlay without arming an engine reminder.
  function previewReminder(kind: ReminderKind) {
    const seconds = kind === "micro" ? preset.breakSeconds : preset.deepBreakMinutes * 60;
    safeInvoke("show_reminder_overlay", { kind, seconds, imageIndex, score: eyeScore });
  }

  function resetToday() {
    safeInvoke<LiveState>("engine_reset_today").then(applyLive);
    refreshFacts();
  }

  function setEngineeringEyeScore(score: number) {
    const nextRisk = clamp(Math.round((100 - score) / 0.72), 0, 100);
    setData((current) => ({ ...current, today: { ...current.today, riskScore: nextRisk } }));
  }

  function closeEngineeringOverlays() {
    safeInvoke("overlay_action", { action: "skip" });
  }

  async function setAutoStart(enabled: boolean) {
    await safeInvoke("set_autostart", { enabled });
    setData((current) => ({
      ...current,
      settings: { ...current.settings, autoStart: enabled }
    }));
  }

  async function exportData() {
    // Full, privacy-safe export from the DB (daily history + reminder/symptom
    // facts, no window titles). Falls back to the in-memory view-model.
    const json =
      (await safeInvoke<string>("db_export")) ??
      JSON.stringify(
        { exportedAt: new Date().toISOString(), today: data.today, logs: data.logs, symptoms: data.symptoms },
        null,
        2
      );
    const blob = new Blob([json], { type: "application/json" });
    const link = document.createElement("a");
    link.href = URL.createObjectURL(blob);
    link.download = `gaze20-${todayKey()}.json`;
    link.click();
    URL.revokeObjectURL(link.href);
  }

  const navItems = [
    { id: "overview" as const, label: "今日概览", icon: Eye },
    { id: "reminders" as const, label: "提醒中心", icon: AlarmClock },
    { id: "trends" as const, label: "趋势统计", icon: BarChart3 },
    { id: "symptoms" as const, label: "症状记录", icon: FileText },
    { id: "settings" as const, label: "设置", icon: Settings2 }
  ];

  return (
    <div className="app">
      <aside className="sidebar">
        <div className="brand-block">
          <div className="brand-icon">
            <Eye size={34} />
          </div>
          <strong>远眺</strong>
          <span>Gaze20</span>
          <p>每 20 分钟，远眺 20 秒</p>
        </div>

        <nav className="side-nav">
          {navItems.map((item) => {
            const Icon = item.icon;
            return (
              <button
                className={view === item.id ? "active" : ""}
                key={item.id}
                onClick={() => setView(item.id)}
              >
                <Icon size={20} />
                <span>{item.label}</span>
              </button>
            );
          })}
        </nav>

        <div className="sidebar-art">
          <div className="mountain" />
          <p>&lt;/&gt; 为你的眼睛 · 代码不止</p>
        </div>
      </aside>

      <main className="main">
        <TopBar
          view={view}
          running={running}
          localOnly
          onToggleRunning={toggleRunning}
          doNotDisturb={data.settings.doNotDisturb}
          onToggleDnd={() =>
            updateData((current) => ({
              ...current,
              settings: {
                ...current.settings,
                doNotDisturb: !current.settings.doNotDisturb
              }
            }))
          }
        />

        <div className="content">
          {view === "overview" && (
            <Overview
              data={data}
              snapshot={snapshot}
              preset={preset}
              running={running}
              eyeScore={eyeScore}
              imageIndex={imageIndex}
              microActiveSeconds={microActiveSeconds}
              deepActiveSeconds={deepActiveSeconds}
              effectiveMicroSeconds={effectiveMicroSeconds}
              effectiveDeepSeconds={effectiveDeepSeconds}
              continuousSeconds={continuousSeconds}
              onStartBreak={startBreakNow}
              onSnooze={snoozeReminders}
              onSetMode={(mode) =>
                updateData((current) => ({
                  ...current,
                  settings: { ...current.settings, mode }
                }))
              }
            />
          )}

          {view === "reminders" && (
            <Reminders
              data={data}
              preset={preset}
              microActiveSeconds={microActiveSeconds}
              effectiveMicroSeconds={effectiveMicroSeconds}
              snapshot={snapshot}
              onPreview={() => previewReminder("micro")}
            />
          )}

          {view === "trends" && (
            <TrendStatsPage data={data} onExport={exportData} />
          )}

          {view === "symptoms" && (
            <Symptoms
              data={data}
              eyeScore={eyeScore}
              imageIndex={imageIndex}
              onChange={updateData}
            />
          )}

          {view === "settings" && (
            <Settings
              data={data}
              snapshot={snapshot}
              presets={presets}
              onChange={updateData}
              onReset={resetToday}
              onAutoStart={setAutoStart}
              onPreviewMicro={() => previewReminder("micro")}
              onPreviewDeep={() => previewReminder("deep")}
            />
          )}
        </div>
      </main>

      {engineeringOpen && (
        <EngineeringPanel
          data={data}
          displayCount={displayCount}
          eyeScore={eyeScore}
          imageIndex={imageIndex}
          snapshot={snapshot}
          onClose={() => setEngineeringOpen(false)}
          onSetEyeScore={setEngineeringEyeScore}
          onPreviewMicro={() => previewReminder("micro")}
          onPreviewDeep={() => previewReminder("deep")}
          onCloseOverlays={closeEngineeringOverlays}
          onReset={resetToday}
          onToggleFastMode={(debugFastMode) =>
            updateData((current) => ({
              ...current,
              settings: { ...current.settings, debugFastMode }
            }))
          }
        />
      )}
    </div>
  );
}

// Shared visual tokens ported from the Claude Design export (Gaze20-export.dc.html).
const dzCard: React.CSSProperties = {
  background: "#fff",
  border: "1px solid #eef2f0",
  borderRadius: 18,
  padding: 22,
  boxShadow: "0 6px 16px -12px rgba(24,52,44,.2)"
};
const dzCardSm: React.CSSProperties = { ...dzCard, borderRadius: 16, padding: 18 };
const dzSoftCard: React.CSSProperties = {
  background: "linear-gradient(155deg,#e7f3ee,#f1f8f5)",
  border: "1px solid #e0eee8",
  borderRadius: 18,
  padding: 22
};
const dzH: React.CSSProperties = { fontSize: 15, fontWeight: 700, color: "#1d3a32" };
const dzPrimaryBtn: React.CSSProperties = {
  background: "linear-gradient(135deg,#2f9e80,#1f7a64)",
  color: "#fff",
  border: "none",
  borderRadius: 13,
  fontSize: 14.5,
  fontWeight: 700,
  cursor: "pointer"
};
const dzGhostBtn: React.CSSProperties = {
  background: "#eef6f2",
  color: "#1f7a64",
  border: "none",
  borderRadius: 12,
  fontSize: 13.5,
  fontWeight: 700,
  cursor: "pointer"
};
function dzRiskLevel(risk: number) {
  return risk < 40 ? "低" : risk < 70 ? "中" : "高";
}
function dzRiskColor(risk: number) {
  return risk < 40 ? "#2f9e6f" : risk < 70 ? "#e8920c" : "#e5544b";
}

function TopBar(props: {
  view: ViewId;
  running: boolean;
  localOnly: boolean;
  doNotDisturb: boolean;
  onToggleRunning: () => void;
  onToggleDnd: () => void;
}) {
  const meta = {
    overview: ["今日用眼概览", "根据键盘、鼠标、前台窗口和阅读状态累计真实用眼时间"],
    reminders: ["提醒中心", "远眺、深休息、延后和勿扰规则都按电脑活动时间触发"],
    trends: ["趋势统计", "最近 7 天游眼节奏、完成率和风险变化"],
    symptoms: ["症状记录", "把眼干、模糊、头痛、颈肩酸痛和电脑活动放在一起看"],
    settings: ["设置", "Windows 活动检测、本地保存、开机自启和提醒策略"]
  }[props.view];

  return (
    <header className="topbar">
      <div>
        <h1>{meta[0]}</h1>
        <p>{meta[1]}</p>
      </div>
      <div className="top-actions">
        <span className="local-pill">
          <i /> 本地优先
        </span>
        <button className={props.doNotDisturb ? "soft active" : "soft"} onClick={props.onToggleDnd}>
          {props.doNotDisturb ? <BellOff size={17} /> : <Bell size={17} />}
          {props.doNotDisturb ? "勿扰中" : "提醒中"}
        </button>
        <button className="window-button" onClick={props.onToggleRunning}>
          {props.running ? <Pause size={17} /> : <Play size={17} />}
        </button>
      </div>
    </header>
  );
}

function timelineBarColor(frac: number): string {
  // Light teal → deep green: a high-contrast ramp so low hours stay readable.
  const lo = [143, 213, 184];
  const hi = [31, 122, 100];
  const t = clamp(frac, 0, 1);
  const c = lo.map((l, i) => Math.round(l + (hi[i] - l) * t));
  return `rgb(${c[0]}, ${c[1]}, ${c[2]})`;
}

function Overview(props: {
  data: PersistedData;
  snapshot: ActivitySnapshot;
  preset: ModePreset;
  running: boolean;
  eyeScore: number;
  imageIndex: number;
  microActiveSeconds: number;
  deepActiveSeconds: number;
  effectiveMicroSeconds: number;
  effectiveDeepSeconds: number;
  continuousSeconds: number;
  onStartBreak: () => void;
  onSnooze: () => void;
  onSetMode: (mode: ModeId) => void;
}) {
  const nextMicro = Math.max(0, props.effectiveMicroSeconds - props.microActiveSeconds);
  const nextDeep = Math.max(0, props.effectiveDeepSeconds - props.deepActiveSeconds);
  const risk = props.data.today.riskScore;
  const scoreDash = `${((props.eyeScore / 100) * 2 * Math.PI * 56).toFixed(1)} ${(2 * Math.PI * 56).toFixed(1)}`;
  const now = new Date();
  const nowLabel = `${String(now.getHours()).padStart(2, "0")}:${String(now.getMinutes()).padStart(2, "0")}`;

  const statCards = [
    { label: "总看屏时长", value: formatDuration(props.data.today.screenSeconds, true), delta: "基于电脑活动累计", deltaColor: "#8499913", bg: "#e8f0ff", color: "#5b8def", icon: <><path d="M12 7v5l3 2" /><path d="M21 12a9 9 0 1 1-18 0 9 9 0 0 1 18 0z" /></> },
    { label: "远眺时长", value: formatDuration(props.data.today.distantGazeSeconds, true), delta: "只记录完成倒计时", deltaColor: "#2f9e6f", bg: "#e6f6ee", color: "#2caa7e", icon: <><path d="M2 12s4-7 10-7 10 7 10 7-4 7-10 7S2 12 2 12z" /><path d="M12 15a3 3 0 1 0 0-6 3 3 0 0 0 0 6z" /></> },
    { label: "微休息完成", value: `${props.data.today.microDone}/${Math.max(1, props.data.today.microDue)}`, delta: props.snapshot.shouldDefer ? "当前自动延后" : "正常检测中", deltaColor: "#2f9e6f", bg: "#efeaff", color: "#8b7ff0", icon: <><path d="M12 7v5l3 2" /><path d="M21 12a9 9 0 1 1-18 0 9 9 0 0 1 18 0z" /></> },
    { label: "风险值", value: `${risk} / ${dzRiskLevel(risk)}`, delta: `延后/跳过 ${props.data.today.postponed}/${props.data.today.skipped}`, deltaColor: dzRiskColor(risk), bg: "#fff1de", color: "#eaa13a", icon: <path d="M12 22s8-4 8-10V5l-8-3-8 3v7c0 6 8 10 8 10z" /> }
  ];

  // Real 24-hour timeline from hourly_stats (today's per-hour eye-use seconds).
  const [hourly, setHourly] = useState<number[]>(() => Array(24).fill(0));
  useEffect(() => {
    let active = true;
    const load = async () => {
      const rows = await safeInvoke<DbHourlyRow[]>("db_hourly_stats", { days: 1 });
      if (!active) return;
      const today = localIsoDate(new Date());
      const arr = Array(24).fill(0);
      for (const r of rows ?? []) {
        if (r.date === today && r.hour >= 0 && r.hour < 24) arr[r.hour] = r.screenSeconds;
      }
      setHourly(arr);
    };
    load();
    const handle = window.setInterval(load, 10000);
    return () => {
      active = false;
      window.clearInterval(handle);
    };
  }, []);
  // Timeline geometry: each hour is a fixed-height slot; a full fill = the whole hour.
  const SLOT_H = 56;

  // This week's guard status from real daily_stats (refreshed periodically).
  const [dailyDone, setDailyDone] = useState<Record<string, boolean>>({});
  useEffect(() => {
    let active = true;
    const load = async () => {
      const rows = await safeInvoke<DbDailyRow[]>("db_daily_stats", { limit: 21 });
      if (!active) return;
      const map: Record<string, boolean> = {};
      for (const r of rows ?? []) map[r.date] = r.microDone >= 1 && r.screenSeconds >= 1200;
      setDailyDone(map);
    };
    load();
    const handle = window.setInterval(load, 60000);
    return () => {
      active = false;
      window.clearInterval(handle);
    };
  }, []);

  // Completed micro/deep breaks today, grouped by hour with their minute-of-hour so
  // the timeline can place them in time order. The legend totals are summed from the
  // same source, so the counts always match the bars on screen.
  const todayIso = localIsoDate(new Date());
  type RestMark = { minute: number; kind: "micro" | "deep" };
  const restsByHour: RestMark[][] = Array.from({ length: 24 }, () => []);
  let microCount = 0;
  let deepCount = 0;
  for (const log of props.data.logs) {
    if (log.result !== "completed" || !log.at || log.at.slice(0, 10) !== todayIso) continue;
    const h = parseInt(log.at.slice(11, 13), 10);
    const m = parseInt(log.at.slice(14, 16), 10);
    if (h < 0 || h >= 24) continue;
    const kind: "micro" | "deep" = log.kind === "deep" ? "deep" : "micro";
    restsByHour[h].push({ minute: Number.isFinite(m) ? clamp(m, 0, 59) : 0, kind });
    if (kind === "deep") deepCount += 1;
    else microCount += 1;
  }
  for (const arr of restsByHour) arr.sort((a, b) => a.minute - b.minute);

  // Mon–Sun cells: done / miss (past + live today) / future (not yet reached).
  const todayQualified = props.data.today.microDone >= 1 && props.data.today.screenSeconds >= 1200;
  const weekMonday = new Date();
  weekMonday.setDate(weekMonday.getDate() - ((weekMonday.getDay() + 6) % 7));
  const weekCells = ["一", "二", "三", "四", "五", "六", "日"].map((label, i) => {
    const d = new Date(weekMonday);
    d.setDate(weekMonday.getDate() + i);
    const iso = localIsoDate(d);
    const state: "done" | "miss" | "future" =
      iso > todayIso ? "future" : iso === todayIso ? (todayQualified ? "done" : "miss") : dailyDone[iso] ? "done" : "miss";
    return { label, state };
  });

  const suggestions = [
    { title: "看向 6 米外 20 秒", sub: "放松睫状肌，缓解视觉疲劳", bg: "#e6f6ee", color: "#2caa7e", icon: <><path d="M2 12s4-7 10-7 10 7 10 7-4 7-10 7S2 12 2 12z" /><path d="M12 15a3 3 0 1 0 0-6 3 3 0 0 0 0 6z" /></> },
    { title: "做 10 次完整眨眼", sub: "保持眼表湿润，预防干涩", bg: "#e8f0ff", color: "#5b8def", icon: <path d="M2 12s4-6 10-6 10 6 10 6M5 14l-1 2M19 14l1 2M9 15l-.5 2M15 15l.5 2" /> },
    { title: `连续高负荷 ${Math.round(props.continuousSeconds / 60)} 分钟`, sub: "建议 3 分钟深休息", bg: "#fff1de", color: "#eaa13a", icon: <path d="M18 8h1a3 3 0 0 1 0 6h-1M3 8h15v5a5 5 0 0 1-5 5H8a5 5 0 0 1-5-5zM6 1v2M10 1v2M14 1v2" /> }
  ];

  const modeList: Array<{ id: ModeId; label: string }> = [
    { id: "conservative", label: "保守" },
    { id: "balanced", label: "平衡" },
    { id: "intense", label: "激进" }
  ];

  return (
    <div className="overview-shell">
      <div style={{ display: "flex", flexDirection: "column", gap: 20 }}>
        <div style={{ position: "relative", borderRadius: 22, overflow: "hidden", background: "linear-gradient(155deg,#e7f3ee 0%,#f1f8f5 52%,#eaf4f0 100%)", border: "1px solid #e0eee8", padding: "26px 28px" }}>
          <svg style={{ position: "absolute", right: 0, top: 0, height: "100%", width: "54%", opacity: 0.5 }} viewBox="0 0 420 320" preserveAspectRatio="xMaxYMid slice" fill="none">
            <circle cx="330" cy="78" r="40" fill="#d3e9df" />
            <path d="M0 230 L70 150 L140 210 L220 130 L300 200 L360 158 L420 196 L420 320 L0 320 Z" fill="#cae3d8" opacity="0.8" />
            <path d="M0 262 L90 196 L170 246 L250 188 L330 238 L420 206 L420 320 L0 320 Z" fill="#b4d8c9" />
          </svg>
          <div className="overview-hero-layout" style={{ position: "relative", zIndex: 2 }}>
            <div className="overview-hero-main" style={{ flex: 1, minWidth: 0 }}>
              <div style={{ display: "flex", alignItems: "center", gap: 8, fontSize: 13.5, fontWeight: 700, color: "#3f8d79" }}>
                <span style={{ width: 8, height: 8, borderRadius: "50%", background: "#2c8e76" }} />眼睛状态
              </div>
              <div className="overview-eye-strip">
                <img src={`/eyes/eyes-redness-score-${props.imageIndex}.webp`} alt="当前眼睛状态" draggable={false} />
              </div>
              <div style={{ fontSize: 26, fontWeight: 800, color: "#1d3a32", marginTop: 10 }}>
                {props.running ? "正在专注工作" : "已暂停记录"} <span style={{ fontFamily: "'Manrope'", color: "#2c8e76" }}>{formatDuration(props.data.today.screenSeconds, true)}</span>
              </div>
              <div style={{ fontSize: 14.5, color: "#6f857c", marginTop: 4, fontWeight: 500 }}>
                下次远眺提醒 <span style={{ fontFamily: "'Manrope'", fontWeight: 700, color: "#1d3a32" }}>{props.snapshot.shouldDefer ? "已延后" : formatDuration(nextMicro)}</span>，深休息还有 <span style={{ fontFamily: "'Manrope'", fontWeight: 700, color: "#1d3a32" }}>{formatDuration(nextDeep)}</span>
              </div>
              <div style={{ display: "flex", gap: 12, marginTop: 18 }}>
                <button onClick={props.onStartBreak} style={{ display: "flex", alignItems: "center", gap: 8, background: "linear-gradient(135deg,#2f9e80,#1f7a64)", color: "#fff", border: "none", padding: "12px 20px", borderRadius: 13, fontSize: 14.5, fontWeight: 700, cursor: "pointer", boxShadow: "0 8px 18px -6px rgba(31,122,100,.5)" }}>
                  <svg width="17" height="17" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round"><path d="M18 8h1a3 3 0 0 1 0 6h-1M3 8h15v5a5 5 0 0 1-5 5H8a5 5 0 0 1-5-5zM6 1v2M10 1v2M14 1v2" /></svg>
                  立即休息
                </button>
                <button onClick={props.onSnooze} style={{ display: "flex", alignItems: "center", gap: 8, background: "#fff", color: "#1f7a64", border: "1.5px solid #cfe7dd", padding: "12px 18px", borderRadius: 13, fontSize: 14.5, fontWeight: 700, cursor: "pointer" }}>
                  <svg width="17" height="17" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round"><circle cx="12" cy="12" r="9" /><path d="M12 7v5l3 2" /></svg>
                  稍后提醒
                </button>
              </div>
            </div>

            <div className="overview-score-panel" style={{ width: 200, flex: "none", borderLeft: "1px solid #d7e8e0", paddingLeft: 22, display: "flex", flexDirection: "column" }}>
              <div style={{ display: "flex", alignItems: "center", gap: 6, fontSize: 13.5, fontWeight: 700, color: "#3f8d79" }}>实时用眼分 <span style={{ color: "#a8bcb3" }}>ⓘ</span></div>
              <div style={{ margin: "14px auto 6px", position: "relative", width: 130, height: 130 }}>
                <svg width="130" height="130" viewBox="0 0 130 130">
                  <circle cx="65" cy="65" r="56" fill="none" stroke="#e4efea" strokeWidth="11" />
                  <circle cx="65" cy="65" r="56" fill="none" stroke="url(#scoreG)" strokeWidth="11" strokeLinecap="round" strokeDasharray={scoreDash} transform="rotate(-90 65 65)" />
                  <defs><linearGradient id="scoreG" x1="0" y1="0" x2="1" y2="1"><stop offset="0%" stopColor="#5fcaa6" /><stop offset="100%" stopColor="#2c8e76" /></linearGradient></defs>
                </svg>
                <div style={{ position: "absolute", inset: 0, display: "flex", flexDirection: "column", alignItems: "center", justifyContent: "center" }}>
                  <div style={{ fontFamily: "'Manrope'", fontSize: 38, fontWeight: 800, color: "#1d3a32", lineHeight: 1 }}>{props.eyeScore}</div>
                  <div style={{ fontSize: 12, color: "#90a59c", fontWeight: 600 }}>/ 100</div>
                </div>
              </div>
              <div style={{ fontSize: 13.5, color: "#6f857c", fontWeight: 600, marginTop: 2 }}>
                当前风险值 <span style={{ fontFamily: "'Manrope'", fontSize: 18, fontWeight: 800, color: dzRiskColor(risk) }}>{risk}</span> <span style={{ color: dzRiskColor(risk), fontWeight: 700 }}>/ {dzRiskLevel(risk)}</span>
              </div>
              <p style={{ fontSize: 11.5, color: "#9aada5", marginTop: 8, lineHeight: 1.5 }}>{props.snapshot.reason}</p>
            </div>
          </div>
        </div>

        <div style={{ display: "grid", gridTemplateColumns: "repeat(4,1fr)", gap: 14 }}>
          {statCards.map((card) => (
            <div key={card.label} style={{ ...dzCardSm, padding: "16px 16px 15px" }}>
              <div style={{ display: "flex", alignItems: "center", gap: 9 }}>
                <span style={{ width: 30, height: 30, borderRadius: 9, display: "flex", alignItems: "center", justifyContent: "center", background: card.bg }}>
                  <svg width="17" height="17" viewBox="0 0 24 24" fill="none" stroke={card.color} strokeWidth="2" strokeLinecap="round" strokeLinejoin="round">{card.icon}</svg>
                </span>
                <span style={{ fontSize: 13, color: "#6f857c", fontWeight: 600 }}>{card.label}</span>
              </div>
              <div style={{ fontFamily: "'Manrope'", fontSize: 27, fontWeight: 800, color: "#1d3a32", marginTop: 11, letterSpacing: "-.5px" }}>{card.value}</div>
              <div style={{ fontSize: 12, fontWeight: 600, marginTop: 5, color: card.deltaColor }}>{card.delta}</div>
            </div>
          ))}
        </div>

        <div style={{ ...dzCard, padding: "20px 22px" }}>
          <div style={{ display: "flex", alignItems: "center", justifyContent: "space-between" }}>
            <div style={{ display: "flex", alignItems: "center", gap: 7, fontSize: 15, fontWeight: 700, color: "#1d3a32" }}>今日时间轴</div>
            <div style={{ fontSize: 11.5, color: "#9aada5", fontWeight: 600 }}>柱高 = 该小时用眼时长 · 横杆 = 当时的微/深休息</div>
          </div>
          <div style={{ position: "relative", height: SLOT_H, display: "flex", gap: 3, padding: "0 8px", marginTop: 14 }}>
            <div style={{ position: "absolute", left: 8, right: 8, top: SLOT_H / 2, borderTop: "1px dashed #c2d6cd", pointerEvents: "none", zIndex: 2 }} />
            <span style={{ position: "absolute", right: 9, top: SLOT_H / 2 - 13, fontSize: 9.5, color: "#a7bbb2", fontFamily: "'Manrope'", fontWeight: 600 }}>30 分</span>
            {hourly.map((sec, h) => {
              const frac = clamp(sec / 3600, 0, 1);
              const active = sec > 1;
              const fillH = active ? Math.max(3, Math.round(frac * SLOT_H)) : 0;
              const minutes = Math.round(sec / 60);
              const marks = restsByHour[h];
              const microN = marks.reduce((n, mk) => n + (mk.kind === "micro" ? 1 : 0), 0);
              const deepN = marks.length - microN;
              const tip = `${String(h).padStart(2, "0")}:00 · 用眼 ${minutes} 分钟` +
                (microN ? ` · 微休息 ${microN} 次` : "") +
                (deepN ? ` · 深休息 ${deepN} 次` : "");
              // Position each break by its minute (top = :00, bottom = :59), nudging
              // adjacent bars apart so they never overlap while keeping time order.
              let lastTop = -10;
              const placed = marks.map((mk) => {
                let top = Math.round(3 + (mk.minute / 60) * (SLOT_H - 9));
                if (top < lastTop + 4) top = lastTop + 4;
                if (top > SLOT_H - 6) top = SLOT_H - 6;
                lastTop = top;
                return { top, kind: mk.kind };
              });
              return (
                <div key={h} title={tip} style={{ position: "relative", flex: 1, height: "100%", background: "#e6efeb", borderRadius: 4, overflow: "hidden" }}>
                  <div style={{ position: "absolute", left: 0, right: 0, bottom: 0, height: fillH, background: active ? timelineBarColor(frac) : "transparent", zIndex: 1 }} />
                  {placed.map((p, k) => (
                    <div key={k} style={{ position: "absolute", left: 2, right: 2, top: p.top, height: 3, borderRadius: 1.5, background: p.kind === "deep" ? "#7b8ff0" : "#34cda6", boxShadow: "0 0 0 1px rgba(255,255,255,.8)", zIndex: 3 }} />
                  ))}
                </div>
              );
            })}
          </div>
          <div style={{ display: "flex", gap: 3, padding: "0 8px", marginTop: 6, fontFamily: "'Manrope'", fontSize: 10, color: "#9aada5", fontWeight: 600 }}>
            {Array.from({ length: 24 }).map((_, h) => (
              <span key={h} style={{ flex: 1, textAlign: "center" }}>{h % 3 === 0 ? String(h).padStart(2, "0") : ""}</span>
            ))}
          </div>
          <div style={{ display: "flex", alignItems: "center", gap: 16, marginTop: 13, fontSize: 12.5, color: "#6f857c", fontWeight: 600, flexWrap: "wrap" }}>
            <span style={{ display: "flex", alignItems: "center", gap: 6 }}><span style={{ width: 10, height: 10, borderRadius: 3, background: "linear-gradient(180deg,#8fd5b8,#1f7a64)" }} />用眼（满格 60 分）</span>
            <span style={{ display: "flex", alignItems: "center", gap: 6 }}><span style={{ width: 12, height: 3, borderRadius: 1.5, background: "#34cda6" }} />微休息 {microCount} 次</span>
            <span style={{ display: "flex", alignItems: "center", gap: 6 }}><span style={{ width: 12, height: 3, borderRadius: 1.5, background: "#7b8ff0" }} />深休息 {deepCount} 次</span>
            <span style={{ marginLeft: "auto", display: "flex", alignItems: "center", gap: 6, background: "#1d3a32", color: "#fff", fontFamily: "'Manrope'", fontWeight: 700, fontSize: 12, padding: "5px 11px", borderRadius: 8 }}>现在 {nowLabel}</span>
          </div>
        </div>
      </div>

      <div style={{ display: "flex", flexDirection: "column", gap: 18 }}>
        <div style={{ ...dzCard, padding: "18px 18px 12px" }}>
          <div style={{ ...dzH, marginBottom: 13 }}>今日建议 <span style={{ color: "#5fcaa6" }}>✦</span></div>
          <div style={{ display: "flex", flexDirection: "column", gap: 9 }}>
            {suggestions.map((g) => (
              <div key={g.title} style={{ display: "flex", alignItems: "center", gap: 12, background: "#fafcfb", border: "1px solid #eef2f0", borderRadius: 13, padding: "11px 12px" }}>
                <span style={{ width: 38, height: 38, flex: "none", borderRadius: 11, display: "flex", alignItems: "center", justifyContent: "center", background: g.bg }}>
                  <svg width="19" height="19" viewBox="0 0 24 24" fill="none" stroke={g.color} strokeWidth="2" strokeLinecap="round" strokeLinejoin="round">{g.icon}</svg>
                </span>
                <span style={{ flex: 1, minWidth: 0 }}>
                  <span style={{ display: "block", fontSize: 13.5, fontWeight: 700, color: "#28403a" }}>{g.title}</span>
                  <span style={{ display: "block", fontSize: 12, color: "#849991", marginTop: 2 }}>{g.sub}</span>
                </span>
              </div>
            ))}
          </div>
        </div>

        <div style={dzCard}>
          <div style={{ ...dzH, marginBottom: 13 }}>模式选择 <span style={{ color: "#b3c4bc", fontSize: 13 }}>ⓘ</span></div>
          <div style={{ display: "flex", gap: 8, background: "#f1f6f4", borderRadius: 13, padding: 5 }}>
            {modeList.map((m) => {
              const on = props.preset.id === m.id;
              return (
                <button
                  key={m.id}
                  onClick={() => props.onSetMode(m.id)}
                  style={{ flex: 1, border: "none", padding: "9px 0", borderRadius: 10, fontSize: 13.5, fontWeight: 700, cursor: "pointer", background: on ? "#fff" : "transparent", color: on ? "#1f7a64" : "#798f87", boxShadow: on ? "0 3px 8px -3px rgba(24,52,44,.22)" : "none" }}
                >
                  {m.label}
                </button>
              );
            })}
          </div>
        </div>

        <div style={dzCard}>
          <div style={{ display: "flex", alignItems: "center", justifyContent: "space-between", marginBottom: 14 }}>
            <div style={dzH}>🔥 本周连续守护</div>
            <div style={{ fontFamily: "'Manrope'", fontSize: 15, fontWeight: 800, color: "#2c8e76" }}>{props.data.streakDays} 天</div>
          </div>
          <div style={{ display: "flex", justifyContent: "space-between" }}>
            {weekCells.map((cell, i) => (
              <div key={i} style={{ display: "flex", flexDirection: "column", alignItems: "center", gap: 8 }}>
                <span style={{ fontSize: 12, color: "#9aada5", fontWeight: 600 }}>{cell.label}</span>
                <span
                  style={{
                    width: 30,
                    height: 30,
                    borderRadius: "50%",
                    display: "flex",
                    alignItems: "center",
                    justifyContent: "center",
                    background: cell.state === "done" ? "linear-gradient(135deg,#3fa98e,#2c8e76)" : cell.state === "miss" ? "#f1f5f3" : "transparent",
                    border: cell.state === "done" ? "none" : cell.state === "miss" ? "1.5px solid #e0e9e5" : "1.5px dashed #dbe6e1",
                    opacity: cell.state === "future" ? 0.6 : 1
                  }}
                >
                  {cell.state === "done" && <svg width="13" height="13" viewBox="0 0 24 24" fill="none" stroke="#fff" strokeWidth="3.4" strokeLinecap="round" strokeLinejoin="round"><path d="M5 13l4 4L19 7" /></svg>}
                  {cell.state === "miss" && <span style={{ width: 5, height: 5, borderRadius: "50%", background: "#cdd9d4" }} />}
                </span>
              </div>
            ))}
          </div>
        </div>

        <div style={{ background: "linear-gradient(135deg,#eaf5f0,#f1f8f5)", border: "1px solid #e0eee8", borderRadius: 18, padding: "18px 20px", display: "flex", alignItems: "center", gap: 14 }}>
          <div style={{ flex: 1 }}>
            <div style={{ fontSize: 24, color: "#b8d4c9", lineHeight: 1, fontFamily: "Georgia,serif" }}>“</div>
            <div style={{ fontSize: 14.5, fontWeight: 700, color: "#2f5a4e", lineHeight: 1.6, marginTop: 2 }}>远眺不是中断专注，<br />而是让专注更持久。</div>
          </div>
          <svg width="26" height="26" viewBox="0 0 24 24" fill="none" stroke="#9cc4b6" strokeWidth="2" strokeLinecap="round"><path d="M2 12s4-7 10-7 10 7 10 7-4 7-10 7S2 12 2 12z" /><circle cx="12" cy="12" r="3" /></svg>
        </div>
      </div>
    </div>
  );
}

function Reminders(props: {
  data: PersistedData;
  preset: ModePreset;
  microActiveSeconds: number;
  effectiveMicroSeconds: number;
  snapshot: ActivitySnapshot;
  onPreview: () => void;
}) {
  const completion = Math.round(
    (props.data.today.microDone / Math.max(1, props.data.today.microDue)) * 100
  );
  return (
    <div className="reminder-grid">
      <section className="plan-card">
        <div>
          <span>已完成</span>
          <strong>{props.data.today.microDone} / {Math.max(1, props.data.today.microDue)} 次</strong>
          <p>下次提醒还有 {formatDuration(Math.max(0, props.effectiveMicroSeconds - props.microActiveSeconds))}</p>
        </div>
        <button className="primary" onClick={props.onPreview}>预览提醒</button>
      </section>

      <section className="wide-card">
        <SectionTitle title="远眺动作组合" sub="每次提醒引导你完成的标准动作" />
        <div className="action-steps">
          <Step num="1" title="看向 6 米外 20 秒" sub="放松调节系统" />
          <Step num="2" title="做 10 次完整眨眼" sub="保持眼表湿润" />
          <Step num="3" title="站起走动 30-60 秒" sub="缓解颈肩，效果更好" />
        </div>
      </section>

      <section className="logs-card">
        <SectionTitle title="最近完成记录" sub="查看全部记录" />
        <div className="log-list">
          {props.data.logs.slice(0, 8).map((log) => (
            <div className="log-row" key={log.id}>
              <span className={log.result}>{log.result}</span>
              <div>
                <strong>{log.kind === "micro" ? "远眺提醒" : "深休息"}</strong>
                <p>{new Date(log.at).toLocaleTimeString()} · {log.note}</p>
              </div>
            </div>
          ))}
          {props.data.logs.length === 0 && <p className="empty">还没有提醒记录。</p>}
        </div>
      </section>

      <section className="rule-card">
        <SectionTitle title="提醒规则概览" sub="不是固定墙钟，而是活动用眼时间" />
        <Rule label="微休息阈值" val={`${props.preset.microMinutes} 分钟活动用眼`} />
        <Rule label="深休息阈值" val={`${props.preset.deepMinutes} 分钟连续高负荷`} />
        <Rule label="当前活动状态" val={props.snapshot.reason} />
        <Rule label="完成率" val={`${completion}%`} />
      </section>

      <section className="dnd-card">
        <SectionTitle title="勿扰状态" sub="会议、全屏、演示会自动延后" />
        <div className={props.snapshot.shouldDefer ? "defer-state on" : "defer-state"}>
          {props.snapshot.shouldDefer ? <BellOff size={22} /> : <Bell size={22} />}
          <strong>{props.snapshot.shouldDefer ? "当前自动延后" : "可正常提醒"}</strong>
          <p>{props.snapshot.foregroundProcess || "未知窗口"} · {props.snapshot.foregroundTitle || "无标题"}</p>
        </div>
      </section>
    </div>
  );
}

function Trends(props: { data: PersistedData; onExport: () => void }) {
  const days = useMemo(() => {
    const base = props.data.today.screenSeconds || 1;
    return Array.from({ length: 7 }).map((_, i) => ({
      d: ["一", "二", "三", "四", "五", "六", "日"][i],
      screen: Math.max(1800, base * (0.62 + i * 0.06)),
      risk: clamp(props.data.today.riskScore - 10 + i * 3, 12, 88)
    }));
  }, [props.data.today.riskScore, props.data.today.screenSeconds]);
  const max = Math.max(...days.map((day) => day.screen), 1);
  const completion = Math.round(
    (props.data.today.microDone / Math.max(1, props.data.today.microDue)) * 100
  );

  return (
    <div className="trends-grid">
      <div className="trend-tabs">
        <button className="active">近 7 天</button>
        <button>近 30 天</button>
        <button>自定义</button>
        <button className="export" onClick={props.onExport}><Download size={15} /> 导出数据</button>
      </div>

      <TrendStat icon={<Clock3 size={16} />} label="平均看屏时长" value={formatDuration(props.data.today.screenSeconds, true)} delta="基于今日活动换算" />
      <TrendStat icon={<Eye size={16} />} label="微休息完成率" value={`${completion}%`} delta="完成率越高风险越低" />
      <TrendStat icon={<Activity size={16} />} label="远眺总时长" value={formatDuration(props.data.today.distantGazeSeconds, true)} delta="完成倒计时后计入" />
      <TrendStat icon={<Gauge size={16} />} label="平均风险值" value={`${props.data.today.riskScore} / ${riskLabel(props.data.today.riskScore)}`} delta="只做行为提示" />

      <section className="chart-card span-2">
        <SectionTitle title="近 7 天趋势" sub="看屏时长与风险值" />
        <div className="bars">
          {days.map((day) => (
            <div className="bar-item" key={day.d}>
              <div className="bar-track">
                <i style={{ height: `${(day.risk / 100) * 100}%` }} />
                <b style={{ height: `${(day.screen / max) * 100}%` }} />
              </div>
              <span>{day.d}</span>
            </div>
          ))}
        </div>
      </section>

      <section className="heat-card">
        <SectionTitle title="每日用眼热力" sub="按小时粗略分布" />
        <div className="heat-grid">
          {Array.from({ length: 48 }).map((_, i) => (
            <span
              key={i}
              style={{ opacity: i % 7 === 0 ? 0.25 : 0.35 + ((i * 17) % 60) / 100 }}
            />
          ))}
        </div>
      </section>
    </div>
  );
}

type TrendRange = "7" | "30" | "custom";

function TrendStatsPage(props: { data: PersistedData; onExport: () => void }) {
  const [range, setRange] = useState("7天");
  const count = range === "30天" ? 30 : range === "自定义" ? 14 : 7;
  const [daily, setDaily] = useState<DbDailyRow[]>([]);
  const [hourly, setHourly] = useState<DbHourlyRow[]>([]);
  const symptoms = props.data.symptoms ?? [];

  useEffect(() => {
    safeInvoke<DbDailyRow[]>("db_daily_stats", { limit: count }).then((rows) => setDaily(rows ?? []));
    safeInvoke<DbHourlyRow[]>("db_hourly_stats", { days: 7 }).then((rows) => setHourly(rows ?? []));
  }, [count]);

  // Continuous day axis (oldest -> newest), zero-filling days without data.
  const series = useMemo(() => {
    const byDate = new Map(daily.map((d) => [d.date, d]));
    const now = new Date();
    return Array.from({ length: count }).map((_, idx) => {
      const i = count - 1 - idx;
      const d = new Date(now.getFullYear(), now.getMonth(), now.getDate() - i);
      const row = byDate.get(localIsoDate(d));
      return {
        date: localIsoDate(d),
        label: `${String(d.getMonth() + 1).padStart(2, "0")}/${String(d.getDate()).padStart(2, "0")}`,
        screenHours: row ? row.screenSeconds / 3600 : 0,
        risk: row ? row.riskScore : 0,
        gaze: row ? row.distantGazeSeconds : 0,
        completion: row ? Math.round((row.microDone / Math.max(1, row.microDue)) * 100) : 0
      };
    });
  }, [daily, count]);

  const activeDays = series.filter((s) => s.screenHours > 0.001);
  const mean = (vals: number[]) => (vals.length ? vals.reduce((a, b) => a + b, 0) / vals.length : 0);
  const avgScreenH = mean(activeDays.map((s) => s.screenHours));
  const avgRisk = Math.round(mean(activeDays.map((s) => s.risk)));
  const avgCompletion = Math.round(mean(activeDays.map((s) => s.completion)));
  const gazeTotal = series.reduce((a, s) => a + s.gaze, 0);
  const risk = avgRisk;
  const completion = avgCompletion;

  const cW = 540;
  const cH = 160;
  const maxH = Math.max(8, ...series.map((s) => s.screenHours));
  const xAt = (i: number) => (series.length <= 1 ? 0 : (i * cW) / (series.length - 1));
  const labelStep = Math.max(1, Math.ceil(series.length / 7));
  const tDates = series.map((s) => s.label);
  const tScreen = series.map((s) => Number(s.screenHours.toFixed(1)));
  const tRisk = series.map((s) => s.risk);
  const screenPts = series.map((s, i) => `${xAt(i).toFixed(1)},${(cH - (s.screenHours / maxH) * cH).toFixed(1)}`).join(" ");
  const riskPts = series.map((s, i) => `${xAt(i).toFixed(1)},${(cH - (s.risk / 100) * cH).toFixed(1)}`).join(" ");

  // Heatmap: 7 weekdays (Mon..Sun) x 8 three-hour buckets, from hourly_stats.
  const heatColors = ["#eaf4ef", "#c8e8d8", "#8fd0b5", "#5bbf96", "#2c9e78", "#1a7557"];
  const heatRows = ["周一", "周二", "周三", "周四", "周五", "周六", "周日"];
  const heatSeed = useMemo(() => {
    const seconds = Array.from({ length: 7 }, () => Array(8).fill(0) as number[]);
    let max = 0;
    for (const h of hourly) {
      const d = new Date(`${h.date}T00:00:00`);
      let wd = d.getDay() - 1;
      if (wd < 0) wd = 6;
      const bucket = Math.min(7, Math.floor(h.hour / 3));
      seconds[wd][bucket] += h.screenSeconds;
      if (seconds[wd][bucket] > max) max = seconds[wd][bucket];
    }
    return seconds.map((row) => row.map((v) => (max > 0 ? Math.min(5, Math.round((v / max) * 5)) : 0)));
  }, [hourly]);

  // Risk composition: the three components derivable from daily aggregates, plus
  // a "持续高负荷" slice for the rest (continuous + pressure aren't stored daily).
  const donutItems = useMemo(() => {
    const m = (f: (r: DbDailyRow) => number) => (daily.length ? daily.reduce((a, r) => a + f(r), 0) / daily.length : 0);
    const screen = m((r) => r.screenSeconds);
    const microDone = m((r) => r.microDone);
    const microDue = m((r) => r.microDue);
    const gaze = m((r) => r.distantGazeSeconds);
    const total = clamp(screen / (8 * 3600), 0, 1) * 20;
    const comp = (1 - clamp(microDone / Math.max(1, microDue), 0, 1)) * 20;
    const gazeRisk = (1 - clamp(gaze / Math.max(1, microDone * 20), 0, 1)) * 10;
    const load = Math.max(0, avgRisk - total - comp - gazeRisk);
    return [
      { label: "持续高负荷", val: Math.round(load), color: "#5b8def" },
      { label: "总看屏时长", val: Math.round(total), color: "#2caa7e" },
      { label: "微休息完成率", val: Math.round(comp), color: "#8b7ff0" },
      { label: "远眺达标", val: Math.round(gazeRisk), color: "#eaa13a" }
    ];
  }, [daily, avgRisk]);
  const donutTotal = Math.max(1, donutItems.reduce((a, b) => a + b.val, 0));
  const dR = 54;
  const dCirc = 2 * Math.PI * dR;
  let dOff = 0;
  const donutArcs = donutItems.map((it) => {
    const dash = (it.val / donutTotal) * dCirc;
    const arc = { color: it.color, dash: `${dash.toFixed(1)} ${(dCirc - dash).toFixed(1)}`, offset: -dCirc * 0.25 + dOff };
    dOff += dash;
    return arc;
  });

  // Symptom trend: per-day averages of each dimension over the day axis.
  const syColors = ["#2caa7e", "#5b8def", "#eaa13a", "#c07ef0"];
  const syLabels = ["干涩", "模糊", "头痛", "颈肩"];
  const syData = useMemo(() => {
    const byDay = new Map<string, { v: [number, number, number, number]; n: number }>();
    for (const r of symptoms) {
      const key = (r.at || "").slice(0, 10);
      const e = byDay.get(key) ?? { v: [0, 0, 0, 0] as [number, number, number, number], n: 0 };
      e.v[0] += r.scores.dry;
      e.v[1] += r.scores.blur;
      e.v[2] += r.scores.headache;
      e.v[3] += r.scores.neck;
      e.n += 1;
      byDay.set(key, e);
    }
    return [0, 1, 2, 3].map((dim) =>
      series.map((s) => {
        const e = byDay.get(s.date);
        return e && e.n > 0 ? e.v[dim] / e.n : 0;
      })
    );
  }, [symptoms, series]);
  const syH = 140;

  const peakDay = activeDays.reduce(
    (best, s) => (s.risk > best.risk ? s : best),
    activeDays[0] ?? { label: "—", risk: 0 }
  );

  const tabs = [
    { t: "7 天", k: "7天" },
    { t: "30 天", k: "30天" },
    { t: "自定义", k: "自定义" }
  ];
  const chipLabel: React.CSSProperties = { display: "flex", alignItems: "center", gap: 8, fontSize: 13, color: "#6f857c", fontWeight: 600, marginBottom: 10 };
  const chipIcon = (bg: string): React.CSSProperties => ({ width: 28, height: 28, background: bg, borderRadius: 8, display: "flex", alignItems: "center", justifyContent: "center" });
  const bigVal: React.CSSProperties = { fontFamily: "'Manrope'", fontSize: 28, fontWeight: 800, color: "#1d3a32" };
  const deltaUp: React.CSSProperties = { fontSize: 12, color: "#2f9e6f", fontWeight: 600, marginTop: 4 };

  const coverage = `近 ${count} 天 · ${activeDays.length} 天有记录`;
  const statCards = [
    { label: "平均看屏时长", value: formatDuration(avgScreenH * 3600, true), delta: coverage, bg: "#e8f0ff", color: "#5b8def", icon: <><circle cx="12" cy="12" r="9" /><path d="M12 7v5l3 2" /></> },
    { label: "微休息完成率", value: `${completion}%`, delta: coverage, bg: "#e6f6ee", color: "#2caa7e", icon: <><path d="M2 12s4-6 10-6 10 6 10 6" /><path d="M12 15a3 3 0 1 0 0-6 3 3 0 0 0 0 6z" /></> },
    { label: "远眺总时长", value: formatDuration(gazeTotal, true), delta: coverage, bg: "#efeaff", color: "#8b7ff0", icon: <path d="M3 18 L9 9 L13 14 L21 5" /> },
    { label: "平均风险值", value: `${risk} / ${dzRiskLevel(risk)}`, delta: coverage, bg: "#fff1de", color: "#eaa13a", icon: <path d="M12 22s8-4 8-10V5l-8-3-8 3v7c0 6 8 10 8 10z" /> }
  ];

  return (
    <div style={{ display: "flex", flexDirection: "column", gap: 20 }}>
      <div style={{ display: "flex", alignItems: "center", justifyContent: "space-between" }}>
        <div style={{ display: "flex", gap: 8 }}>
          {tabs.map((tab) => {
            const on = range === tab.k;
            return (
              <button
                key={tab.k}
                onClick={() => setRange(tab.k)}
                style={{
                  padding: "9px 18px",
                  borderRadius: 10,
                  background: on ? "linear-gradient(135deg,#3fa98e,#2c8e76)" : "#fff",
                  color: on ? "#fff" : "#41584f",
                  border: on ? "none" : "1.5px solid #dde7e3",
                  fontSize: 14,
                  fontWeight: on ? 700 : 600,
                  cursor: "pointer"
                }}
              >
                {tab.t}
              </button>
            );
          })}
        </div>
        <button
          onClick={props.onExport}
          style={{ display: "flex", alignItems: "center", gap: 7, background: "#fff", color: "#41584f", border: "1.5px solid #dde7e3", padding: "9px 15px", borderRadius: 11, fontSize: 13.5, fontWeight: 700, cursor: "pointer" }}
        >
          <svg width="15" height="15" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round"><path d="M4 16v1a3 3 0 0 0 3 3h10a3 3 0 0 0 3-3v-1M7 10l5 5 5-5M12 15V3" /></svg>
          导出数据
        </button>
      </div>

      <div style={{ display: "grid", gridTemplateColumns: "repeat(4,1fr)", gap: 14 }}>
        {statCards.map((card) => (
          <div key={card.label} style={dzCardSm}>
            <div style={chipLabel}>
              <span style={chipIcon(card.bg)}>
                <svg width="15" height="15" viewBox="0 0 24 24" fill="none" stroke={card.color} strokeWidth="2" strokeLinecap="round">{card.icon}</svg>
              </span>
              {card.label}
            </div>
            <div style={bigVal}>{card.value}</div>
            <div style={deltaUp}>{card.delta}</div>
          </div>
        ))}
      </div>

      <div style={{ display: "grid", gridTemplateColumns: "1fr 360px", gap: 20 }}>
        <div style={dzCard}>
          <div style={{ display: "flex", alignItems: "center", justifyContent: "space-between", marginBottom: 16 }}>
            <div style={dzH}>近 7 天趋势</div>
            <div style={{ display: "flex", gap: 14, fontSize: 12, fontWeight: 600 }}>
              <span style={{ display: "flex", alignItems: "center", gap: 5, color: "#2caa7e" }}><span style={{ width: 20, height: 2, background: "#2caa7e", borderRadius: 2 }} />看屏时长（小时）</span>
              <span style={{ display: "flex", alignItems: "center", gap: 5, color: "#eaa13a" }}><span style={{ width: 20, height: 2, background: "#eaa13a", borderRadius: 2 }} />风险值</span>
            </div>
          </div>
          <svg width="100%" viewBox="0 0 560 200" style={{ overflow: "visible" }}>
            <line x1="0" y1="0" x2="0" y2="160" stroke="#f0f4f2" strokeWidth="1" />
            {[0, 40, 80, 120].map((y) => <line key={y} x1="0" y1={y} x2="540" y2={y} stroke="#f0f4f2" strokeWidth="1" />)}
            <line x1="0" y1="160" x2="540" y2="160" stroke="#eef2f0" strokeWidth="1.5" />
            {[["9", 44], ["6", 84], ["3", 124]].map(([t, y]) => <text key={`l${t}`} x="-6" y={y as number} fill="#b3c4bc" fontSize="10" textAnchor="end" fontFamily="Manrope">{t}</text>)}
            {[["75", 44], ["50", 84], ["25", 124]].map(([t, y]) => <text key={`r${t}`} x="560" y={y as number} fill="#eaa13a" fontSize="10" textAnchor="start" fontFamily="Manrope">{t}</text>)}
            <polyline points={screenPts} fill="none" stroke="#2caa7e" strokeWidth="2.5" strokeLinejoin="round" strokeLinecap="round" />
            <polyline points={riskPts} fill="none" stroke="#eaa13a" strokeWidth="2" strokeLinejoin="round" strokeLinecap="round" strokeDasharray="5 3" />
            {tScreen.map((v, i) => {
              if (series.length > 10) return null;
              const x = xAt(i);
              const y = cH - (v / maxH) * cH;
              return (
                <g key={`d${i}`}>
                  <circle cx={x} cy={y} r="5" fill="#2caa7e" stroke="#fff" strokeWidth="2" />
                  <text x={x} y={y} dy="-10" fill="#2caa7e" fontSize="11" textAnchor="middle" fontWeight="700" fontFamily="Manrope">{v}</text>
                </g>
              );
            })}
            {tDates.map((d, i) =>
              i % labelStep === 0 ? (
                <text key={`${d}-${i}`} x={xAt(i)} y="178" fill="#9aada5" fontSize="10" textAnchor="middle" fontFamily="Manrope">{d}</text>
              ) : null
            )}
          </svg>
        </div>

        <div style={dzCard}>
          <div style={{ display: "flex", alignItems: "center", justifyContent: "space-between", marginBottom: 16 }}>
            <div style={dzH}>每日用眼热力</div>
            <span style={{ background: "#eef6f2", color: "#2c8e76", fontSize: 12, fontWeight: 700, padding: "5px 10px", borderRadius: 8 }}>看屏时长</span>
          </div>
          <div style={{ display: "flex", flexDirection: "column", gap: 6 }}>
            {heatRows.map((row, ri) => (
              <div key={row} style={{ display: "flex", alignItems: "center", gap: 6 }}>
                <span style={{ width: 26, fontSize: 11, color: "#9aada5", fontWeight: 600, flex: "none" }}>{row}</span>
                <div style={{ display: "flex", gap: 3, flex: 1 }}>
                  {heatSeed[ri].map((v, ci) => <div key={ci} style={{ flex: 1, height: 22, borderRadius: 4, background: heatColors[v] }} />)}
                </div>
              </div>
            ))}
          </div>
          <div style={{ display: "flex", alignItems: "center", gap: 8, marginTop: 14, fontSize: 12, color: "#9aada5", fontWeight: 600 }}>
            <span>低</span>
            <div style={{ flex: 1, height: 8, borderRadius: 4, background: "linear-gradient(to right,#eaf4ef,#2c9e78)" }} />
            <span>高</span>
          </div>
        </div>
      </div>

      <div style={{ display: "grid", gridTemplateColumns: "360px 1fr 280px", gap: 20 }}>
        <div style={dzCard}>
          <div style={{ ...dzH, marginBottom: 16 }}>风险构成 <span style={{ fontSize: 12, color: "#9aada5", fontWeight: 500 }}>近 7 天平均</span></div>
          <div style={{ display: "flex", alignItems: "center", gap: 18 }}>
            <div style={{ position: "relative", flex: "none", width: 130, height: 130 }}>
              <svg width="130" height="130" viewBox="0 0 130 130">
                <circle cx="65" cy="65" r={dR} fill="none" stroke="#f2f6f4" strokeWidth="16" />
                {donutArcs.map((arc, i) => (
                  <circle key={i} cx="65" cy="65" r={dR} fill="none" stroke={arc.color} strokeWidth="16" strokeDasharray={arc.dash} strokeDashoffset={arc.offset} />
                ))}
              </svg>
              <div style={{ position: "absolute", inset: 0, display: "flex", flexDirection: "column", alignItems: "center", justifyContent: "center" }}>
                <div style={{ fontSize: 10, color: "#9aada5", fontWeight: 600 }}>平均风险值</div>
                <div style={{ fontFamily: "'Manrope'", fontSize: 26, fontWeight: 800, color: "#1d3a32", lineHeight: 1.1 }}>{risk}</div>
              </div>
            </div>
            <div style={{ flex: 1, display: "flex", flexDirection: "column", gap: 8 }}>
              {donutItems.map((di) => (
                <div key={di.label} style={{ display: "flex", alignItems: "center", gap: 7 }}>
                  <span style={{ width: 8, height: 8, borderRadius: 2, background: di.color, flex: "none" }} />
                  <span style={{ fontSize: 12, color: "#41584f", fontWeight: 600, flex: 1 }}>{di.label}</span>
                  <span style={{ fontFamily: "'Manrope'", fontSize: 13, fontWeight: 700, color: "#1d3a32" }}>{di.val}/100</span>
                </div>
              ))}
            </div>
          </div>
        </div>

        <div style={dzCard}>
          <div style={{ display: "flex", alignItems: "center", justifyContent: "space-between", marginBottom: 16 }}>
            <div style={dzH}>症状趋势 <span style={{ fontSize: 12, color: "#9aada5", fontWeight: 500 }}>近 7 天平均分</span></div>
            <div style={{ display: "flex", gap: 10 }}>
              {syLabels.map((label, i) => (
                <span key={label} style={{ display: "flex", alignItems: "center", gap: 4, fontSize: 11.5, fontWeight: 600, color: "#6f857c" }}>
                  <span style={{ width: 16, height: 2, background: syColors[i], borderRadius: 1 }} />{label}
                </span>
              ))}
            </div>
          </div>
          <svg width="100%" viewBox="0 0 540 160" style={{ overflow: "visible" }}>
            {[35, 70, 105].map((y) => <line key={y} x1="0" y1={y} x2="540" y2={y} stroke="#f0f4f2" strokeWidth="1" />)}
            <line x1="0" y1="140" x2="540" y2="140" stroke="#eef2f0" strokeWidth="1.5" />
            {syData.map((rowVals, i) => (
              <polyline key={i} points={rowVals.map((v, j) => `${xAt(j).toFixed(1)},${(syH - (v / 5) * syH).toFixed(1)}`).join(" ")} fill="none" stroke={syColors[i]} strokeWidth="2.2" strokeLinejoin="round" strokeLinecap="round" />
            ))}
            {tDates.map((d, i) =>
              i % labelStep === 0 ? (
                <text key={`${d}-${i}`} x={xAt(i)} y="158" fill="#9aada5" fontSize="10" textAnchor="middle" fontFamily="Manrope">{d}</text>
              ) : null
            )}
          </svg>
        </div>

        <div style={dzCard}>
          <div style={{ ...dzH, marginBottom: 16 }}>趋势洞察</div>
          <div style={{ display: "flex", flexDirection: "column", gap: 13 }}>
            <div style={{ padding: 12, background: "#eaf6f0", borderRadius: 13, borderLeft: "3px solid #2caa7e" }}>
              <div style={{ fontSize: 13.5, fontWeight: 700, color: "#1d3a32" }}>近 {count} 天概览</div>
              <div style={{ fontSize: 12, color: "#5f8074", marginTop: 5, lineHeight: 1.5 }}>平均看屏 {avgScreenH.toFixed(1)}h、平均风险 {avgRisk}（{dzRiskLevel(avgRisk)}）、微休息完成率 {avgCompletion}%。</div>
            </div>
            <div style={{ padding: 12, background: "#fff8eb", borderRadius: 13, borderLeft: "3px solid #eaa13a" }}>
              <div style={{ fontSize: 13.5, fontWeight: 700, color: "#1d3a32" }}>{peakDay.label} 风险最高</div>
              <div style={{ fontSize: 12, color: "#7a6040", marginTop: 5, lineHeight: 1.5 }}>风险值 {peakDay.risk}，主要来自连续高负荷与看屏时长较长。</div>
            </div>
            <div style={{ padding: 12, background: "#eff1ff", borderRadius: 13, borderLeft: "3px solid #8b7ff0" }}>
              <div style={{ fontSize: 13.5, fontWeight: 700, color: "#1d3a32" }}>建议增加深休息</div>
              <div style={{ fontSize: 12, color: "#5a547a", marginTop: 5, lineHeight: 1.5 }}>可在午后 14–16 时每 90 分钟进行 5 分钟深休息。</div>
            </div>
          </div>
        </div>
      </div>
    </div>
  );
}

const symptomMeta: Array<{ key: SymptomKind; label: string; hint: string }> = [
  { key: "dry", label: "眼干 / 异物感", hint: "眼睛发干、刺痛、异物感" },
  { key: "blur", label: "视物模糊", hint: "看字发虚、聚焦变慢" },
  { key: "headache", label: "头痛 / 眼胀", hint: "额头、眼眶或太阳穴不适" },
  { key: "neck", label: "颈肩酸痛", hint: "脖子、肩背紧张酸痛" }
];

const emptySymptomScores = (): Record<SymptomKind, number> => ({
  dry: 0,
  blur: 0,
  headache: 0,
  neck: 0
});

function Symptoms(props: {
  data: PersistedData;
  eyeScore: number;
  imageIndex: number;
  onChange: (updater: (current: PersistedData) => PersistedData) => void;
}) {
  const [scores, setScores] = useState<Record<SymptomKind, number>>(() => emptySymptomScores());
  const records = props.data.symptoms ?? [];
  const canSave = Object.values(scores).some((value) => value > 0);

  function setScore(key: SymptomKind, value: number) {
    setScores((current) => ({ ...current, [key]: clamp(value, 0, 5) }));
  }

  function saveRecord() {
    if (!canSave) return;
    const nextRecord: SymptomRecord = {
      id: crypto.randomUUID(),
      at: new Date().toISOString(),
      scores,
      note: "",
      screenSeconds: props.data.today.screenSeconds
    };
    props.onChange((current) => ({
      ...current,
      symptoms: [nextRecord, ...(current.symptoms ?? [])].slice(0, 240)
    }));
    setScores(emptySymptomScores());
  }

  function exportSymptoms() {
    const blob = new Blob([JSON.stringify(records, null, 2)], { type: "application/json" });
    const link = document.createElement("a");
    link.href = URL.createObjectURL(blob);
    link.download = `gaze20-symptoms-${todayKey()}.json`;
    link.click();
    URL.revokeObjectURL(link.href);
  }

  const symList: Array<{ key: SymptomKind; label: string; emoji: string }> = [
    { key: "dry", label: "眼睛干涩", emoji: "👁" },
    { key: "blur", label: "视物模糊", emoji: "🌫" },
    { key: "headache", label: "头痛不适", emoji: "🤕" },
    { key: "neck", label: "颈肩酸痛", emoji: "💆" }
  ];
  const symColors: Record<SymptomKind, string> = { dry: "#2caa7e", blur: "#5b8def", headache: "#eaa13a", neck: "#c07ef0" };
  const symTrack: Record<SymptomKind, string> = { dry: "#e6f6ee", blur: "#e8f0ff", headache: "#fff1de", neck: "#f3e8ff" };
  const recent = records.slice(0, 6);
  const dateLabel = (iso: string) => {
    const d = iso.slice(0, 10);
    if (d === todayKey()) return "今日";
    if (d === new Date(Date.now() - 86400000).toISOString().slice(0, 10)) return "昨日";
    return `${iso.slice(5, 7)}/${iso.slice(8, 10)}`;
  };

  return (
    <div style={{ display: "grid", gridTemplateColumns: "1fr 340px", gap: 22, alignItems: "start" }}>
      <div style={{ display: "flex", flexDirection: "column", gap: 20 }}>
        <div style={{ ...dzSoftCard, borderRadius: 20, padding: 24 }}>
          <div style={{ ...dzH, marginBottom: 18 }}>
            今日症状自评 <span style={{ fontSize: 12.5, color: "#90a59c", fontWeight: 500 }}>— 每天 1 分钟，长期追踪更有参考价值</span>
          </div>
          <div style={{ display: "grid", gridTemplateColumns: "1fr 1fr", gap: 14 }}>
            {symList.map((item) => (
              <div key={item.key} style={{ background: "#fff", borderRadius: 16, padding: 18, border: "1px solid #e6efeb" }}>
                <div style={{ fontSize: 13.5, fontWeight: 700, color: "#28403a", marginBottom: 12 }}>
                  {item.emoji} {item.label}
                </div>
                <div style={{ display: "flex", gap: 8 }}>
                  {[1, 2, 3, 4, 5].map((n) => {
                    const on = scores[item.key] === n;
                    return (
                      <button
                        key={n}
                        onClick={() => setScore(item.key, n)}
                        style={{
                          flex: 1,
                          height: 36,
                          borderRadius: 9,
                          border: `1.5px solid ${on ? "#2c8e76" : "#dde7e3"}`,
                          background: on ? "#eaf4ef" : "#fff",
                          fontSize: 13,
                          fontWeight: 700,
                          color: on ? "#1f7a64" : "#6f857c",
                          cursor: "pointer"
                        }}
                      >
                        {n}
                      </button>
                    );
                  })}
                </div>
                <div style={{ display: "flex", justifyContent: "space-between", fontSize: 11, color: "#c0cdc8", marginTop: 6, fontWeight: 600 }}>
                  <span>无</span>
                  <span>严重</span>
                </div>
              </div>
            ))}
          </div>
          <button
            onClick={saveRecord}
            disabled={!canSave}
            style={{ ...dzPrimaryBtn, marginTop: 18, padding: "13px 32px", opacity: canSave ? 1 : 0.5, cursor: canSave ? "pointer" : "default" }}
          >
            保存今日评分
          </button>
        </div>

        <div style={dzCard}>
          <div style={{ display: "flex", alignItems: "center", justifyContent: "space-between", marginBottom: 16 }}>
            <div style={dzH}>近期记录</div>
            <div style={{ display: "flex", gap: 14, fontSize: 12, fontWeight: 600 }}>
              {symList.map((item) => (
                <span key={item.key} style={{ display: "flex", alignItems: "center", gap: 4, color: symColors[item.key] }}>
                  <span style={{ width: 10, height: 10, borderRadius: 2, background: symColors[item.key] }} />
                  {item.label.slice(-2)}
                </span>
              ))}
            </div>
          </div>
          {recent.length === 0 && (
            <div style={{ fontSize: 13, color: "#90a59c", padding: "18px 4px" }}>还没有记录。先给当前不适程度打分，再保存。</div>
          )}
          {recent.map((record) => (
            <div key={record.id} style={{ display: "flex", alignItems: "center", gap: 14, padding: "13px 4px", borderBottom: "1px solid #f1f5f3" }}>
              <span style={{ fontFamily: "'Manrope'", fontWeight: 700, color: "#1d3a32", fontSize: 14, minWidth: 38 }}>{dateLabel(record.at)}</span>
              <div style={{ display: "flex", flex: 1, gap: 6 }}>
                {symList.map((item) => (
                  <div key={item.key} style={{ flex: 1, height: 28, background: symTrack[item.key], borderRadius: 7, position: "relative", overflow: "hidden" }}>
                    <div style={{ position: "absolute", left: 0, top: 0, bottom: 0, background: symColors[item.key], borderRadius: 7, width: `${(record.scores[item.key] / 5) * 100}%` }} />
                  </div>
                ))}
              </div>
            </div>
          ))}
        </div>
      </div>

      <div style={{ display: "flex", flexDirection: "column", gap: 18 }}>
        <div style={dzCard}>
          <div style={{ ...dzH, marginBottom: 14 }}>评分说明</div>
          <div style={{ display: "flex", flexDirection: "column", gap: 10, fontSize: 13, color: "#5f7d72", lineHeight: 1.6 }}>
            <div style={{ background: "#f6faf8", borderRadius: 11, padding: "11px 13px" }}><strong>1–2 分：</strong>无明显不适或轻微，正常范围内。</div>
            <div style={{ background: "#fff8eb", borderRadius: 11, padding: "11px 13px" }}><strong>3 分：</strong>中等不适，建议增加休息频率。</div>
            <div style={{ background: "#fff1f0", borderRadius: 11, padding: "11px 13px" }}><strong>4–5 分：</strong>明显不适，建议停工休息。如持续 1–2 周，请考虑就医。</div>
          </div>
        </div>

        <div style={{ background: "linear-gradient(135deg,#eaf5f0,#f1f8f5)", border: "1px solid #e0eee8", borderRadius: 18, padding: 18 }}>
          <div style={{ fontSize: 14, fontWeight: 700, color: "#1d3a32", marginBottom: 8 }}>⚠️ 重要提示</div>
          <div style={{ fontSize: 13, color: "#5f7d72", lineHeight: 1.6 }}>
            症状记录仅供个人习惯追踪参考，<strong>不构成医疗诊断</strong>。若症状持续或加重，请及时就医。
          </div>
        </div>

        <div style={dzCard}>
          <div style={{ ...dzH, marginBottom: 14 }}>导出数据</div>
          <div style={{ fontSize: 13, color: "#6f857c", marginBottom: 14, lineHeight: 1.5 }}>可导出历史评分，供个人复盘或就医时参考。</div>
          <button onClick={exportSymptoms} style={{ ...dzGhostBtn, width: "100%", padding: 11 }}>导出 CSV / JSON</button>
        </div>
      </div>
    </div>
  );
}

function EngineeringPanel(props: {
  data: PersistedData;
  displayCount: number | null;
  eyeScore: number;
  imageIndex: number;
  snapshot: ActivitySnapshot;
  onClose: () => void;
  onSetEyeScore: (score: number) => void;
  onPreviewMicro: () => void;
  onPreviewDeep: () => void;
  onCloseOverlays: () => void;
  onReset: () => void;
  onToggleFastMode: (enabled: boolean) => void;
}) {
  const [draftScore, setDraftScore] = useState(props.eyeScore);

  useEffect(() => {
    setDraftScore(props.eyeScore);
  }, [props.eyeScore]);

  function changeScore(value: number) {
    const next = clamp(value, 18, 100);
    setDraftScore(next);
    props.onSetEyeScore(next);
  }

  return (
    <aside className="engineering-panel" role="dialog" aria-label="工程模式">
      <div className="engineering-head">
        <div>
          <span>Ctrl + Shift + E</span>
          <h2>工程模式</h2>
        </div>
        <button onClick={props.onClose} aria-label="关闭工程模式"><X size={20} /></button>
      </div>

      <div className="engineering-eye">
        <img src={`/eyes/eyes-redness-score-${props.imageIndex}.webp`} alt="工程模式眼睛分数预览" />
        <label>
          <span>眼睛分数</span>
          <input
            type="range"
            min={18}
            max={100}
            value={draftScore}
            onChange={(event) => changeScore(Number(event.target.value))}
          />
          <strong>{draftScore}</strong>
        </label>
      </div>

      <div className="engineering-grid">
        <Rule label="显示器数量" val={props.displayCount === null ? "检测中" : `${props.displayCount} 个`} />
        <Rule label="风险值" val={`${props.data.today.riskScore} / ${riskLabel(props.data.today.riskScore)}`} />
        <Rule label="前台进程" val={props.snapshot.foregroundProcess || "未知"} />
        <Rule label="检测原因" val={props.snapshot.reason} />
      </div>

      <Toggle
        label="快速演示模式"
        checked={props.data.settings.debugFastMode}
        onChange={props.onToggleFastMode}
      />

      <div className="engineering-actions">
        <button className="primary" onClick={props.onPreviewMicro}>
          <MonitorPlay size={16} /> 弹出远眺提醒
        </button>
        <button onClick={props.onPreviewDeep}>
          <Coffee size={16} /> 弹出深休息
        </button>
        <button onClick={props.onCloseOverlays}>
          <X size={16} /> 关闭提醒
        </button>
        <button className="danger-button" onClick={props.onReset}>
          <RotateCcw size={16} /> 清空今日数据
        </button>
      </div>
    </aside>
  );
}

function Settings(props: {
  data: PersistedData;
  snapshot: ActivitySnapshot;
  presets: ModePreset[];
  onChange: (updater: (current: PersistedData) => PersistedData) => void;
  onReset: () => void;
  onAutoStart: (enabled: boolean) => void;
  onPreviewMicro: () => void;
  onPreviewDeep: () => void;
}) {
  const [updateMsg, setUpdateMsg] = useState<string | null>(null);
  const [checking, setChecking] = useState(false);
  async function checkUpdate() {
    setChecking(true);
    setUpdateMsg(null);
    try {
      const version = await invoke<string | null>("check_for_update");
      setUpdateMsg(version ? `发现新版本 v${version}，请前往下载更新` : "已是最新版本");
    } catch {
      setUpdateMsg("当前未配置更新源（见 doc/RELEASE.md）");
    } finally {
      setChecking(false);
    }
  }
  return (
    <div className="settings-grid">
      <section className="wide-card">
        <SectionTitle title="模式选择" sub="允许用户自定义强度，避免软件被卸载" />
        <div className="mode-grid">
          {props.presets.map((mode) => (
            <button
              className={props.data.settings.mode === mode.id ? "mode active" : "mode"}
              key={mode.id}
              onClick={() =>
                props.onChange((current) => ({
                  ...current,
                  settings: { ...current.settings, mode: mode.id }
                }))
              }
            >
              <strong>{mode.label}</strong>
              <span>{mode.sub}</span>
              <small>{mode.microMinutes} 分钟 / {mode.deepMinutes} 分钟</small>
            </button>
          ))}
        </div>
      </section>

      <section className="settings-card">
        <SectionTitle title="活动检测" sub="兼顾键鼠输入和阅读文档" />
        <Slider
          label="阅读宽限"
          value={props.data.settings.readingGraceMinutes}
          min={2}
          max={20}
          suffix="分钟"
          onChange={(readingGraceMinutes) =>
            props.onChange((current) => ({
              ...current,
              settings: { ...current.settings, readingGraceMinutes }
            }))
          }
        />
        <Slider
          label="离开判定"
          value={props.data.settings.awayMinutes}
          min={5}
          max={30}
          suffix="分钟"
          onChange={(awayMinutes) =>
            props.onChange((current) => ({
              ...current,
              settings: { ...current.settings, awayMinutes }
            }))
          }
        />
      </section>

      <section className="settings-card">
        <SectionTitle title="桌面能力" sub="Windows 常驻能力" />
        <Toggle
          label="桌面通知"
          checked={props.data.settings.notifications}
          onChange={(notifications) =>
            props.onChange((current) => ({
              ...current,
              settings: { ...current.settings, notifications }
            }))
          }
        />
        <Toggle
          label="开机自启"
          checked={props.data.settings.autoStart}
          onChange={props.onAutoStart}
        />
        <div className="slider-row" style={{ gridTemplateColumns: "1fr auto", marginTop: 18 }}>
          <span style={{ fontWeight: 800 }}>
            软件更新
            {updateMsg && (
              <span style={{ display: "block", fontSize: 12, fontWeight: 600, color: "#6f857c", marginTop: 4 }}>{updateMsg}</span>
            )}
          </span>
          <button
            onClick={checkUpdate}
            disabled={checking}
            style={{ background: "#eef6f2", color: "#1f7a64", border: "none", borderRadius: 12, padding: "9px 16px", fontSize: 13.5, fontWeight: 700, cursor: checking ? "default" : "pointer" }}
          >
            {checking ? "检查中…" : "检查更新"}
          </button>
        </div>
      </section>

      <section className="settings-card">
        <SectionTitle title="勿扰与自动延后" sub="全屏、会议和长文档阅读时减少打断" />
        <Toggle
          label="勿扰模式"
          checked={props.data.settings.doNotDisturb}
          onChange={(doNotDisturb) =>
            props.onChange((current) => ({
              ...current,
              settings: { ...current.settings, doNotDisturb }
            }))
          }
        />
        <Rule label="全屏窗口" val={props.snapshot.isFullscreen ? "当前自动延后" : "检测后自动延后"} />
        <Rule label="阅读状态" val={props.snapshot.readingActive ? "正在计入用眼时间" : "无输入时保留阅读宽限"} />
        <Rule label="当前判断" val={props.snapshot.reason} />
      </section>

      <section className="settings-card privacy-card">
        <SectionTitle title="隐私与数据" sub="和你给的网页文案一致，只做本地提醒" />
        <Rule label="摄像头" val="不调用、不采集、不上传画面" />
        <Rule label="数据位置" val="本机应用数据目录与 localStorage 双备份" />
        <Rule label="导出方式" val="趋势统计页可导出 JSON，自行留存" />
      </section>

      <section className="activity-card">
        <SectionTitle title="实时检测状态" sub="来自 Rust / Windows API" />
        <Rule label="前台进程" val={props.snapshot.foregroundProcess || "未知"} />
        <Rule label="窗口标题" val={props.snapshot.foregroundTitle || "无标题"} />
        <Rule label="空闲时间" val={`${Math.round(props.snapshot.idleSeconds)} 秒`} />
        <Rule label="活动权重" val={`${Math.round(props.snapshot.eyeActivityWeight * 100)}%`} />
        <Rule label="判断原因" val={props.snapshot.reason} />
        <button className="danger-button" onClick={props.onReset}><RotateCcw size={15} /> 清空今日演示数据</button>
      </section>
    </div>
  );
}

function SectionTitle(props: { title: string; sub: string }) {
  return (
    <div className="section-title">
      <div>
        <h3>{props.title}</h3>
        <p>{props.sub}</p>
      </div>
    </div>
  );
}

function Suggestion(props: { icon: ReactNode; title: string; sub: string }) {
  return (
    <div className="suggestion">
      <span>{props.icon}</span>
      <div>
        <strong>{props.title}</strong>
        <p>{props.sub}</p>
      </div>
    </div>
  );
}

function Step(props: { num: string; title: string; sub: string }) {
  return (
    <div className="step">
      <b>{props.num}</b>
      <strong>{props.title}</strong>
      <span>{props.sub}</span>
    </div>
  );
}

function Rule(props: { label: string; val: string }) {
  return (
    <div className="rule">
      <span>{props.label}</span>
      <strong>{props.val}</strong>
    </div>
  );
}

function TrendStat(props: { icon: ReactNode; label: string; value: string; delta: string }) {
  return (
    <section className="trend-stat">
      <span>{props.icon}</span>
      <p>{props.label}</p>
      <strong>{props.value}</strong>
      <small>{props.delta}</small>
    </section>
  );
}

function Timeline(props: { stats: DayStats }) {
  const done = Math.max(1, props.stats.microDone + props.stats.deepDone);
  return (
    <div className="timeline">
      <div className="timeline-bar">
        {Array.from({ length: 24 }).map((_, i) => (
          <span
            key={i}
            className={i % 6 === 0 ? "rest" : i < done + 8 ? "work" : ""}
          />
        ))}
      </div>
      <div className="timeline-labels">
        <span>0 时</span>
        <span>6 时</span>
        <span>12 时</span>
        <span>18 时</span>
        <span>24 时</span>
      </div>
      <div className="timeline-summary">
        <b>专注 {formatDuration(props.stats.screenSeconds, true)}</b>
        <b>微休息 {props.stats.microDone} 次</b>
        <b>深休息 {props.stats.deepDone} 次</b>
      </div>
    </div>
  );
}

function Slider(props: {
  label: string;
  value: number;
  min: number;
  max: number;
  suffix: string;
  onChange: (value: number) => void;
}) {
  return (
    <label className="slider-row">
      <span>{props.label}</span>
      <input
        type="range"
        min={props.min}
        max={props.max}
        value={props.value}
        onChange={(event) => props.onChange(Number(event.target.value))}
      />
      <b>{props.value}{props.suffix}</b>
    </label>
  );
}

function Toggle(props: { label: string; checked: boolean; onChange: (checked: boolean) => void }) {
  return (
    <label className="toggle-row">
      <span>{props.label}</span>
      <input
        type="checkbox"
        checked={props.checked}
        onChange={(event) => props.onChange(event.target.checked)}
      />
      <i />
    </label>
  );
}
