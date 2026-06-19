export type ViewId = "overview" | "reminders" | "trends" | "symptoms" | "settings";
export type ModeId = "conservative" | "balanced" | "intense";
export type ReminderKind = "micro" | "deep";
export type ReminderResult = "completed" | "partial" | "postponed" | "skipped" | "deferred";
export type SymptomKind = "dry" | "blur" | "headache" | "neck";

export type ModePreset = {
  id: ModeId;
  label: string;
  sub: string;
  microMinutes: number;
  deepMinutes: number;
  breakSeconds: number;
  deepBreakMinutes: number;
};

export type Settings = {
  mode: ModeId;
  notifications: boolean;
  doNotDisturb: boolean;
  autoStart: boolean;
  readingGraceMinutes: number;
  awayMinutes: number;
  debugFastMode: boolean;
};

export type ActivitySnapshot = {
  idleSeconds: number;
  foregroundProcess: string;
  isFullscreen: boolean;
  inputActive: boolean;
  readingActive: boolean;
  eyeActivityWeight: number;
  shouldDefer: boolean;
  reason: string;
  capturedAtMs: number;
};

export type ReminderLog = {
  id: string;
  at: string;
  atMs: number | null;
  kind: ReminderKind;
  result: ReminderResult;
  activeSeconds: number;
  note: string;
};

export type SymptomRecord = {
  id: string;
  at: string;
  atMs: number | null;
  scores: Record<SymptomKind, number>;
  note: string;
  screenSeconds: number;
};

export type DayStats = {
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

export type PersistedData = {
  version: number;
  settings: Settings;
  today: DayStats;
  logs: ReminderLog[];
  symptoms: SymptomRecord[];
  streakDays: number;
};

// Live state pushed by the Rust engine (camelCase mirrors lib.rs `LiveState`).
export type LiveState = {
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

export type DbReminderRow = {
  id: number;
  at: string;
  atMs: number | null;
  kind: string;
  result: string;
  activeSeconds: number;
  gazeSeconds: number;
  note: string | null;
};

export type DbSymptomRow = {
  id: number;
  at: string;
  atMs: number | null;
  dry: number;
  blur: number;
  headache: number;
  neck: number;
  note: string | null;
  screenSeconds: number;
};

export type DbDailyRow = {
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

export type DbHourlyRow = { date: string; hour: number; screenSeconds: number };
export type DbAppUsageRow = { process: string | null; activeSeconds: number; sessions: number };
export type DbStateRow = { state: string; activeSeconds: number };

export const presets: ModePreset[] = [
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

export const todayKey = () => new Date().toISOString().slice(0, 10);

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

export function defaultData(): PersistedData {
  return {
    version: 3,
    settings: defaultSettings,
    today: emptyStats(),
    logs: [],
    symptoms: [],
    streakDays: 1
  };
}

export function clamp(value: number, min: number, max: number) {
  return Math.max(min, Math.min(max, value));
}

export function formatDuration(seconds: number, compact = false) {
  const s = Math.max(0, Math.floor(seconds));
  const h = Math.floor(s / 3600);
  const m = Math.floor((s % 3600) / 60);
  const sec = s % 60;
  if (h > 0) return compact ? `${h}h ${m}m` : `${h} 小时 ${m} 分`;
  if (compact) return `${m}m ${sec}s`;
  return `${String(m).padStart(2, "0")}:${String(sec).padStart(2, "0")}`;
}

export function riskLabel(score: number) {
  if (score >= 70) return "高";
  if (score >= 40) return "中";
  return "低";
}

export function reminderResultLabel(result: ReminderResult) {
  switch (result) {
    case "completed":
      return "已完成";
    case "partial":
      return "提前结束";
    case "postponed":
      return "已延后";
    case "skipped":
      return "已跳过";
    case "deferred":
      return "自动延后";
    default:
      return result;
  }
}

export function modeToPreset(mode: ModeId) {
  return presets.find((item) => item.id === mode) ?? presets[1];
}

export function makeFallbackSnapshot(): ActivitySnapshot {
  return {
    idleSeconds: 0,
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

export function scoreFromRisk(risk: number) {
  return clamp(Math.round(100 - risk * 0.72), 18, 100);
}

export function eyeImageIndex(risk: number) {
  return clamp(Math.round((risk / 100) * 9), 0, 9);
}

export function localIsoDate(d: Date): string {
  const p = (n: number) => String(n).padStart(2, "0");
  return `${d.getFullYear()}-${p(d.getMonth() + 1)}-${p(d.getDate())}`;
}

export function dbToLog(row: DbReminderRow): ReminderLog {
  return {
    id: String(row.id),
    at: row.at,
    atMs: row.atMs,
    kind: row.kind === "deep" ? "deep" : "micro",
    result: row.result as ReminderResult,
    activeSeconds: row.activeSeconds,
    note: row.note ?? ""
  };
}

export function dbToSymptom(row: DbSymptomRow): SymptomRecord {
  return {
    id: String(row.id),
    at: row.at,
    atMs: row.atMs,
    scores: { dry: row.dry, blur: row.blur, headache: row.headache, neck: row.neck },
    note: row.note ?? "",
    screenSeconds: row.screenSeconds
  };
}

export function liveToSnapshot(live: LiveState): ActivitySnapshot {
  return {
    idleSeconds: 0,
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
