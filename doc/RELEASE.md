# 发布与自动更新指南

本文件覆盖**代码签名**和**自动更新**两块——它们的代码/配置脚手架已经接好，但**证书、签名私钥、更新服务器都属于你自己的基础设施**，需要你来补齐。补完后用户才能无警告安装、并自动收到更新。

---

## 1. 代码签名（消除 SmartScreen「未知发布者」拦截）

未签名的安装包，Windows 会弹「未知发布者，可能有风险」，劝退大量用户。

1. 申请一张 **代码签名证书**：
   - **OV**（组织验证）：便宜，但新证书仍会被 SmartScreen 观察一段时间后才积累信誉。
   - **EV**（扩展验证）：贵、需硬件 token，但**立即**通过 SmartScreen。商业产品建议 EV。
2. 在 `src-tauri/tauri.conf.json` 的 `bundle` 下加签名配置（任选其一）：
   ```jsonc
   "bundle": {
     "windows": {
       // 方式 A：用证书指纹（证书已装进 Windows 证书库）
       "certificateThumbprint": "你的证书SHA1指纹",
       "digestAlgorithm": "sha256",
       "timestampUrl": "http://timestamp.digicert.com"
       // 方式 B：EV 硬件 token 用自定义签名命令
       // "signCommand": "签名工具 %1"
     }
   }
   ```
3. `npm run tauri build` 出来的 `.msi` / `setup.exe` 会自动被签名。

---

## 2. 自动更新（已接好 `tauri-plugin-updater`，缺配置 + 服务端）

代码侧已就绪：
- `src-tauri/src/lib.rs` 已注册 `tauri_plugin_updater` 插件，并暴露 `check_for_update` 命令；
- 设置页「桌面能力 → 软件更新 → 检查更新」按钮已接上；**未配置时会显示「当前未配置更新源」**，配置后即生效。

### 2.1 生成更新签名密钥对（一次性）
```bash
npx tauri signer generate -w gaze20-updater.key
```
- 它会输出**公钥**，并把**私钥**写到 `gaze20-updater.key`（再要你设一个密码）。
- **私钥 + 密码绝不能进 git**，妥善保管（密码管理器 / CI secret）。`gaze20-updater.key*` 建议加进 `.gitignore`。

### 2.2 把公钥写进配置
`src-tauri/tauri.conf.json` 里**已经有占位配置**，把两个占位值替换掉即可：
```jsonc
"plugins": {
  "updater": {
    "pubkey": "REPLACE_WITH_YOUR_UPDATER_PUBKEY",        // ← 换成 2.1 的公钥
    "endpoints": ["https://updates.example.com/gaze20/latest.json"]  // ← 换成你的真实地址
  }
}
```
> 当前占位地址是 `example.com`，所以点「检查更新」会失败并提示「未配置更新源」——这是预期的，换成真实值后即生效。
> ⚠️ `plugins.updater` 这一节**不能删**：`tauri-plugin-updater` 启动时若读不到该配置会直接 panic，应用起不来。

### 2.3 每次发版签名 + 发布
```bash
# 用私钥签名构建产物
export TAURI_SIGNING_PRIVATE_KEY="$(cat gaze20-updater.key)"
export TAURI_SIGNING_PRIVATE_KEY_PASSWORD="你的密码"
npm run tauri build
```
- 构建会额外产出每个安装包的 `.sig` 签名文件。
- 写一个 `latest.json`（版本号、各平台下载地址、对应 `.sig` 内容），连同安装包一起发到 GitHub Release。格式见 Tauri 官方文档「Updater / Static JSON」。
- 用户下次「检查更新」就会命中。

### 2.4（可选）开机/定时自动检查
现在是**手动点按钮**检查。要做成自动，可在前端启动时调一次 `check_for_update`，发现新版本弹提示；安装流程可在 Rust 侧加 `update.download_and_install()`（本文件未实现，留作下一步）。

---

## 3. GitHub Actions 自动发版

仓库已接入两个工作流：

- `.github/workflows/build.yml`：推送 `main` / `develop`、提交 PR 或手动触发时，执行 `tsc`、`cargo test`、`clippy -D warnings` 和 Tauri release 构建，并上传 exe / 安装包产物。
- `.github/workflows/release.yml`：推送形如 `v0.3.0` 的 tag，或手动输入 tag 后，自动创建 GitHub Release，上传 NSIS 安装包、MSI 安装包和免安装 portable zip。

发布新版时先确保 `package.json`、`src-tauri/Cargo.toml`、`src-tauri/tauri.conf.json` 版本一致，然后执行：

```bash
git tag -a v0.3.0 -m "远眺 Gaze20 v0.3.0"
git push origin main
git push origin v0.3.0
```

后续接入代码签名 / updater 签名时，把证书、私钥和密码放进 GitHub Secrets，再在 release workflow 中增加签名步骤即可。

---

**作者：yuanzihao**
