# 综合工作台 v2.0（安徽农业无人机 PR 工作台）

面向安徽省区域经理的本地优先桌面应用：政府 / 政策 / 行业 / 媒体 / 活动 / 客户六板块看板、KPI、提醒规则、晨报、最小客户画像，配自建薄同步服务端。设计基线为已评审的 PRD、架构设计与「变形金刚主题档位 B」UI 提案（8 条设计原则 + Design Tokens）。

## 仓库布局

- `crates/wb-core` —— 纯函数核心：导入（8 sheet / 64 项存量对账、日期规范化）、落库（SQLite，写前 L1 快照轮转 + L2 全量，主数据 / 归档分库，schema 版本迁移）、规则引擎（临期 / 逾期 / 落后 / 滞留 + 日配额）、晨报、同步合并（LWW + progress 取 max + 冲突日志 + 增量游标）、更新清单与 Ed25519 验签。40 个单元测试。
- `crates/wbd` —— CLI 驱动器，子命令见下文「自检与演练」。
- `apps/desktop` —— Tauri 2 桌面壳（Rust 只做窗口 / 托盘 / 通知 / 存储，业务全部走 wb-core）+ 零依赖静态前端（`src/index.html` / `tokens.css` / `app.js`，变形金刚档位 B 明暗双主题）。
- `server` —— 自建同步服务端，Node 22 内置模块实现（`node:http` / `node:crypto` / `node:sqlite`），**零 npm 依赖**，拷目录即部署。详见 `server/DEPLOY.md`。
- `scripts/make-manifest.cjs` —— 生成四平台 `latest.json` 更新清单；签名内容来自 `WB_SIGNATURE` 环境变量，密钥材料永不入仓库。
- `.github/workflows/windows-release.yml` —— Windows CI：构建签名安装包（**超 500MB 即失败**）并产出更新清单。
- `docs/` —— 实现自检记录（`SELF-CHECK.md`）与原始实测 JSON（`selfcheck-2026-10-27.json`）。

## 构建与运行

前置：Rust ≥ 1.80（stable）、Node.js ≥ 22、Windows 构建需 MSVC 工具链（桌面端）。

```bash
# 核心与 CLI（Linux/macOS/Windows 均可）
cargo build --release            # 产出 target/release/wbd

# 桌面端（Windows）
cd apps/desktop
npm install                       # 仅 tauri-cli 等构建工具
npm run tauri build               # 产出 NSIS/MSI 安装包 + latest.json
```

同步服务端（部署机，零 npm 依赖）：

```bash
cd server
node src/server.js                # 默认 8443（成员端口）/ 8444（管理端口，仅内网）
# systemd / nginx TLS 配置见 server/deploy/
```

## 自检与演练（wbd）

```bash
wbd import    <xlsx> <fiscal_year> [--json]   # 导入对账报告
wbd dates     <xlsx> <fiscal_year>           # 日期规范化逐条核对（TSV）
wbd backup    <data_dir> <n>                  # 备份轮转演练
wbd rules     <data_dir> <fiscal_year> <yyyy-mm-dd>   # 规则求值 + 晨报
wbd sync      <data_dir>                      # 同步合并演练（LWW / progress-max / 冲突日志）
wbd update    <data_dir>                     # 更新清单与验签演练（含反例）
wbd selfcheck <xlsx> <fiscal_year> <data_dir> # 端到端自检，输出实测 JSON
```

## 关键设计约束（红线）

1. 本地优先：断网 / 服务端故障时本地功能完整可用，写入转待补推队列。
2. SQLite 写前自动备份轮转（L1 快照 20 份 + L2 全量 14 份上限），主数据 `workbench.db` 与归档 `archive.db` 分离。
3. schema 版本号 + 只增可重入迁移脚本；库版本高于目标版本拒绝启动。
4. 更新通道：静态 `latest.json`（四平台必填，`signature` 必须是签名内容本身）+ 下载 URL 零鉴权；私钥只走 `TAURI_SIGNING_PRIVATE_KEY` / `WB_SIGNATURE` 环境变量。
5. 时间线画月粒度整月区间带（斜纹），不编日精度。
6. F10 语音播报首版仅留接口：主界面总开关显眼且默认关闭（disabled），P2 二期再实现，无任何声纹克隆代码。
7. 安装包 ≤500MB 由 CI 硬门保证（超限即构建失败）；口径为用户 2026-10-27 确认，Tauri 轻量（5–15MB）仅作优化方向。

红线逐条实测数据见 `docs/SELF-CHECK.md`。
