# Windows 安装说明（个人工作台 v2.0）

## 重要说明（先读）

本安装说明对应的安装包（`.exe` / `.msi`）由 GitHub Actions 在 Windows 构建机上产出：仓库 CI（`.github/workflows/windows-release.yml`）签出本仓库后执行 `tauri build`，签名私钥经 CI secrets 注入（仓库内无任何密钥材料），并对安装包执行 **500MB 硬门（超限即失败；口径为用户 2026-10-27 确认，原 15MB 已作废）**。当前开发沙箱为 1 核 Linux、无 Windows 工具链与 GUI 依赖，**无法在本机产出 .exe**；因此请从 CI 构建产物（Artifacts / Releases）下载安装包，或自行在 Windows 机器上按「自行构建」一节构建。

## 一、安装

1. 从 CI 构建产物下载 `Workbench_2.0.0_x64-setup.exe`（NSIS，推荐）或 `.msi`。
2. 双击运行，按向导完成安装（默认安装到 `%LOCALAPPDATA%\Programs\Workbench`，无需管理员权限）。
3. 首次启动：桌面/开始菜单「个人工作台」图标，或直接运行安装目录下的 `Workbench.exe`。

## 二、首次使用

1. **导入存量 Excel**：顶栏「设置」视图 → 「导入存量工作簿」→ 选择 `work_progress.xlsx`。导入完成后显示对账报告：8 个 sheet / 64 项全部入库、KPI 10 项；1 条占位行按规则跳过、14 条标记待复核（界面上会列出，需要人工确认，不会静默造数）。
2. **日期口径**：所有「2月」这类月粒度起点统一落到当年 2 月 1 日生效；时间线上月粒度项目画整月区间带（斜纹），不会显示虚构的日。
3. **数据位置**：本地数据在 `%APPDATA%\com.anhui.pr.workbench\data\`——`workbench.db`（主数据）与 `archive.db`（归档）分文件存放；每次写库前自动做 L1 快照（保留 20 份），每日 L2 全量备份（保留 14 份），可在设置页一键手动备份。
4. **主题**：默认浅色「办公模式」，设置页可切换深色「机甲模式」；系统减少动态偏好自动降级为 80ms 交叉淡入。

## 三、托盘与关闭行为（B7 最小形态）

- 点窗口右上角 **×**：**不退出**，最小化到系统托盘（任务栏右下角），进程常驻继续做规则提醒与通知。
- 托盘菜单：双击图标恢复窗口；右键菜单「显示主界面」「立即备份」「退出」。
- 关闭/重启电脑后再启动：若上次有未送达的晨报/提醒，会在主界面顶部显示「补发横幅」，点开即读，**不静默丢弃**。

## 四、同步（可选）

本应用本地优先：**不配置同步服务端时，以上功能全部完整可用**。团队多人协作才需要：

1. 管理员按 `server/DEPLOY.md` 部署自建同步服务端（Node 22，拷目录即可，零 npm 依赖）。
2. 管理员完成 `/setup` 初始化 → 生成成员邀请链接（48 小时有效）→ 分发给成员。
3. 成员在本应用「设置」→ 「同步服务地址」填入服务器地址（HTTPS，nginx TLS 配置见 `server/deploy/nginx.conf.example`），登录后自动双向同步。
4. 同步语义：行级 LWW（`updated_at` → `updated_by` → `device_id`），进度字段取最大值；每次被覆盖的值都会写入冲突日志（服务端可查，永不定期清理）；断网期间本地写入自动入队，恢复后补推。
5. 服务端版本不匹配时返回 409，客户端提示升级，不做有损合并。

## 五、更新机制

- 应用内置更新检查：从静态 `latest.json` 清单读取版本与下载 URL，**下载 URL 零鉴权**（任意静态托管均可）。
- 更新包经 **Ed25519 验签**（minisign 兼容），验签失败拒绝安装；私钥仅存在于 CI secrets，不随仓库分发。
- 设置页可手动检查更新；`latest.json` 缺任一平台键判整份清单无效（宁可不更新）。

## 六、语音播报（F10，P2 二期）

主界面顶栏常驻「语音播报」总开关，当前为**灰色不可用、默认关闭**——首版按设计只预留接口，二期实现；无任何声纹/克隆相关代码。

## 七、自行构建（备选）

Windows 10/11 x64，安装 Rust（MSVC）、Node.js ≥ 22、WebView2 Runtime（Win11 自带）：

```bat
git clone <repo> && cd workbench\apps\desktop
npm install
npm run tauri build
:: 产物在 src-tauri\target\release\bundle\nsis\ ，超 500MB 会构建失败（硬门）
```

签名（可选，仅发布时）：设置 `TAURI_SIGNING_PRIVATE_KEY` 与 `TAURI_SIGNING_PRIVATE_KEY_PASSWORD` 环境变量后构建，再用 `node scripts\make-manifest.cjs` 生成 `latest.json`。

## 八、常见问题

- **杀毒软件拦截安装**：安装包未付费购买代码签名证书（成本考虑，见架构 §8 讨论），Windows SmartScreen 可能提示「未知发布者」——点「仍要运行」；验签由应用内 Ed25519 完成，不依赖系统证书。
- **数据迁移**：换机时拷贝 `%APPDATA%\com.anhui.pr.workbench\data\` 整个目录即可；schema 版本不匹配时应用会自动跑迁移脚本，目标版本低于库版本则拒绝启动（防降级损坏）。
- **找回数据**：设置页「立即备份」会做一次 L2 全量；备份文件在 `data\backup\` 下，含 21 张表的 JSON 快照可用于人工恢复。
