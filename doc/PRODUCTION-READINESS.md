# 产品化待办清单（Production Readiness Backlog）

把 Gaze20 从「能跑的原型」推向「能上线的专业工具」的进度与剩余项。按优先级排，每条标了**改哪**和**怎么改**。签名/更新的**服务端步骤**见 [`RELEASE.md`](RELEASE.md)。

---

## 数据层 · 已强化（地基）

数据层已按「专业工具」标准打牢，要点（均带单测，26 个数据库/引擎/弹窗调度单测）：

- ✅ **本地 SQLite 系统**：迁移链（现 `SCHEMA_VERSION = 7`）、事实表 + 聚合表分层、迁移前自动备份、损坏自动恢复、保留期裁剪、隐私不存窗口标题。
- ✅ **状态机在 Rust**：重启不丢当天进度。
- ✅ **P0-1 原始活动事实层（V5）**：`activity_sessions` 真正写入——每段「同进程 + 同活动档位」的连续有效用眼一行（`started_ms/ended_ms` UTC 毫秒、`state` active/reading/meeting、`process`、本地 `date/hour`）。解锁 per-app 分析、用眼构成、回溯重算。
- ✅ **P0-2 时间戳标准化（V4）**：事实表加 `at_ms`（UTC 毫秒）双写 + 回填；查询/前端按 epoch，跨时区/DST 安全。
- ✅ **P0-3 升级健壮性**：`Engine` 容器级 `#[serde(default)]`（加字段不再反序列化失败）；`streak_days` 冗余存 settings，blob 损坏时可恢复。
- ✅ **P1 per-app 用眼统计 + 用眼构成**：`db_app_usage` / `db_state_breakdown`，趋势页两张卡。
- ✅ **P1-2 导出补全 + 导入**：`export_json` 含 daily/hourly/reminder/symptom/activity + 格式版本；`import_json` 单事务、幂等去重合并（不覆盖本机数据）；设置页导出全部 / 导入。
- ✅ **V6 产品级数据上下文**：每日聚合保存风险模型版本与构成快照，提醒增加 `reminder_sessions` 生命周期表，小时/提醒/症状/活动补齐本地时区与触发上下文。
- ✅ **P2-6 保留天数可配**：设置页 30/90/180/365 天，重启生效。
- ✅ **streak 自增**（曾经的坏功能已修，跨天按达标 + 连续自增）。
- ✅ **退出 flush**：托盘退出前持久化引擎 + 落地当日聚合。

### 数据层扩展配方（按需再做，不预先塞投机列）

地基已就绪，下列扩展都是「一次小迁移」即可，无需重构：

1. **提醒上下文（触发时的模式）** — `ALTER TABLE reminder_events ADD COLUMN mode TEXT;`（V8），`insert_reminder_event` 增 `mode` 入参（`record_reminder_event` 由引擎当时 `mode.as_str()` 传入），`ReminderEventRow` + SELECT + 导出/导入补 `mode`。用于「分模式完成率」。
2. **症状可扩展** — `ALTER TABLE symptom_records ADD COLUMN extra TEXT;`（V8，存 JSON）。加新症状种类时写进 `extra`，无需再迁移；导出/导入随 `SymptomRow` 带上即可。比 EAV 简单且可查。
3. **多档案 / 多用户（profile）** — 真要做就一次做对：给 `daily_stats` / `hourly_stats` 改主键为 `(profile_id, date[, hour])`，其余表加 `profile_id`（默认 `'default'`），所有查询带 `WHERE profile_id = ?`。**不做半吊子预留**（单加一列不改 PK 反而埋坑）。

---

## P1 · 正确性 / 健壮性

### 命令失败对用户不可见
- **位置**：`src/App.tsx` 的 `safeInvoke`（出错一律吞成 `null`）。
- **改法**：加轻量 toast；关键写操作（保存症状、改设置、导入失败）提示用户而非静默。（导入已有内联状态文案，可推广为全局 toast。）

### Windows 通知的隐性依赖（AUMID）
- **现象**：直接双击 `gaze20.exe` 跑时 toast 可能不弹，走安装包才稳（需 Start 菜单快捷方式注册 AppUserModelID）。
- **改法**：补充「请用安装包安装」说明，或 Rust 侧显式设置 AUMID。弹窗降级已兜底。

---

## P2 · 工程化 / 仓库卫生

### 仓库历史仍背着大文件
- **现状**：`远眺 Gaze20.html`(26MB) 与 `eyes-redness-score-image/*.png`(~15MB) 已 untrack + `.gitignore`，但仍在历史提交里。
- **改法**：**首次推远端前**用 `git filter-repo` 重写历史抠掉，否则远端永久背着。

### CSP 已收紧
- **现状**：`src-tauri/tauri.conf.json` 已启用 CSP，限制默认来源、IPC 连接、图片/样式/脚本来源，并禁止 object/base/frame 嵌入。
- **后续**：如果将来把 `overlay.html` 的内联脚本迁到独立文件，可继续移除 `script-src 'unsafe-inline'`。

### lint / format
- **现状**：CI 已跑 `cargo test` + `clippy -D warnings` + `tsc`。前端无 eslint/prettier。
- **改法**：加 `eslint`/`prettier` + `npm run lint`。

---

## P3 · 产品打磨 / 体验

- **首次运行引导**：2–3 屏 onboarding（含隐私承诺：仅本地、不传云、不存窗口标题）。
- **提醒间隔自定义**：现仅保守/平衡/激进三档；允许自定义 micro/deep 分钟数。
- **风险模型校准**：症状自评已纳入 `engine.rs` 的 v2 风险模型；后续需要更多真实使用数据校准权重。
- **i18n / 无障碍**：全硬编码中文；要出海再抽 `react-i18next` + 补 `aria-*` / 焦点管理。
- **自动更新的「自动」**：现为手动「检查更新」；可做启动静默检查 + 一键安装（`download_and_install`），见 [`RELEASE.md`](RELEASE.md) §2.4。
- **代码签名 + 真实更新服务端**：脚手架就绪，缺证书 / 服务器，见 [`RELEASE.md`](RELEASE.md)。

---

## 已完成（其它）

- ✅ 文件日志（`tauri-plugin-log`）、锁中毒兜底、提醒弹窗失败降级系统通知、迁移/恢复测试。
- ✅ 自动更新接线 + 签名脚手架。
- ✅ CI（`cargo test` + `clippy -D warnings` + `tsc` + 构建产物）。
- ✅ README、提醒卡片重做（WebView 小卡片、多屏同步居中）、今日时间轴重做。

---

**作者：yuanzihao**　·　主分支 `main`（本地提交，未推远端）
