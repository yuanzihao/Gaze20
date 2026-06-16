# 产品化待办清单（Production Readiness Backlog）

这是把 Gaze20 从「能跑的原型」推向「能上线的产品」还欠缺的东西。**已完成的 4 个 P0**（代码签名脚手架+自动更新接线、文件日志、锁中毒兜底+弹窗降级、迁移/恢复测试）不在此列；签名/更新的**服务端步骤**见 [`RELEASE.md`](RELEASE.md)。

按优先级排，每条标了**改哪**和**怎么改**。

---

## P1 · 正确性 / 健壮性（影响功能与可信度）

### 1. `streak_days`（连续守护天数）永远不自增 —— 功能其实是坏的
- **位置**：`src-tauri/src/engine.rs` 的 `roll_over()`（只保留、不 +1）。
- **改法**：跨天定稿时，如果「昨天有有效用眼且完成了至少一次微休息（或达到某阈值）」，`streak_days += 1`；否则断签重置为 0/1。需要在 `roll_over` 拿到「昨天是否达标」（用 finished 的 `micro_done`/`screen_seconds` 判断）。补一个单测。

### 2. 命令失败对用户不可见
- **位置**：`src/App.tsx` 的 `safeInvoke`（出错一律吞成 `null`）。
- **改法**：加一个轻量 toast 机制；关键写操作（保存症状、改设置、导出）失败时提示用户，而不是静默。

### 3. 退出时最后几秒数据可能丢
- **位置**：引擎每 5s 才 `persist_engine`；托盘退出时循环线程直接死。
- **改法**：监听窗口/应用退出事件，退出前 `persist_engine` + `upsert_daily` 一次（best-effort flush）。

### 4. Windows 通知的隐性依赖（AUMID）
- **现象**：直接双击 `gaze20.exe` 跑时 toast 可能不弹，只有走安装包安装后才稳（toast 需要 Start 菜单快捷方式注册 AppUserModelID）。
- **改法**：安装包已建快捷方式；补充说明「请用安装包安装」，或在 Rust 侧显式设置 AUMID。弹窗降级（已做）能兜底,但通知本身值得修。

---

## P2 · 工程化 / 仓库卫生

### 5. 没有 CI
- **现状**：无 `.github/`。全靠手工 `tauri build`。
- **改法**：GitHub Actions：push/PR 跑 `cargo test` + `cargo clippy -D warnings` + `tsc --noEmit` + 构建产物；打 tag 时出签名 Release（见 RELEASE.md）。

### 6. 缺 README / CHANGELOG / lint 脚本
- **现状**：`package.json` 只有 `dev/build/preview/tauri`，**没有 test/lint/format**。无 README/CHANGELOG。
- **改法**：加 `README.md`（定位、安装、隐私声明、构建步骤）、`CHANGELOG.md`、`eslint`/`prettier` + `npm run lint`、`clippy`。

### 7. 仓库体积失控
- **现状**：根目录 **26MB 的 `远眺 Gaze20.html`**、`eyes-redness-score-image/` 的 **~15MB PNG 原图**都进了 git，`.git` 已 **~889MB**。
- **改法**：`git rm` 这些大文件 + 加进 `.gitignore`；用 BFG / `git filter-repo` 清理历史后再首次推远端（**推之前务必清，否则远端永久背着 889MB**）。

### 8. CSP 关着
- **位置**：`src-tauri/tauri.conf.json` `security.csp: null`。
- **改法**：设严格 CSP（本地资源 `'self'`、字体放行 google fonts 或本地化字体），做纵深防御。

---

## P3 · 产品打磨 / 体验

### 9. 没有首次运行引导
- 装完冷启动，不解释「为什么要这些权限 / 它怎么判断用眼 / 数据存在哪」。
- **改法**：首启一个 2-3 屏 onboarding（含隐私承诺：仅本地、不传云、不存窗口标题）。

### 10. 没有数据导入
- 只有导出（`db_export`）+ 旧 JSON 一次性迁移；用户换机/重装无法「导入之前导出的备份」。
- **改法**：加 `db_import` 命令，解析导出的 JSON 回灌 daily/reminder/symptom（带去重/冲突策略）。

### 11. 提醒间隔不可自定义
- 现只有 保守/平衡/激进 三档（研究文档里写的是 15/20/30 可选 + 自定义）。
- **改法**：设置里允许自定义 micro/deep 分钟数，存进引擎设置。

### 12. 症状没纳入风险模型
- 设计稿风险环里有「症状自评」，但 `engine.rs` 的 `compute_risk` 没有这一项（权重 30/20/20/10/20，无症状）。
- **改法**：把近 N 天症状均分作为一个风险贡献项纳入 `compute_risk`，并同步更新趋势页风险构成环。

### 13. i18n / 无障碍缺失
- 全硬编码中文；无键盘导航 / ARIA / 焦点管理。
- **改法**：要出海再抽 i18n（`react-i18next`）；无障碍补 `aria-*`、focus ring、Tab 顺序。

### 14. 自动更新的「自动」部分
- 现在是手动点「检查更新」。可做成启动时静默检查 + 一键安装（`download_and_install`）。详见 [`RELEASE.md`](RELEASE.md) §2.4。

---

## 已完成（参考）

- ✅ 本地 SQLite 数据系统（迁移链、事实/聚合表、旧数据迁移、隐私不存标题、备份/恢复/保留/导出）。
- ✅ 状态机在 Rust（重启不丢上下文），8 引擎单测 + 7 数据库单测。
- ✅ P0：文件日志（`tauri-plugin-log`）、锁中毒兜底、原生弹窗失败降级为系统通知、迁移/恢复测试、自动更新接线 + 签名脚手架。

---

**作者：yuanzihao**　·　分支 `feature/data-layer-v4`（未推远端）
