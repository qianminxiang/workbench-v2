# 实现自检记录（交付物 ③）

**自检日期**：2026-10-27 · **自检工具**：`wbd selfcheck`（仓库自带 CLI 驱动器，7 段 JSON 输出，原始文件 `docs/selfcheck-2026-10-27.json`）
**规则求值基准日**：2026-10-26（固定基准日，保证结果可复现；晨报/提醒在目标机上以当日为基准）
**存量样本**：真实的 `work_progress.xlsx`（8 sheet / 64 项 + KPI 表），非模拟数据。

## 一、红线逐条对照

| 红线 | 结果 | 实测数据 |
|-|-|-|
| 存量 Excel 完整导入且日期规范化生效 | ✅ 通过 | 64/64 项对账一致（`reconciled: true`）；「2月」→ 当年 2 月 1 日，`all_feb_first_of_month: true`（20 处月粒度起点全部归到 1 号）；占位行 1 条按规则跳过、14 条标待复核，**不静默造数** |
| 本地优先，断网/服务端故障本地功能完整可用 | ✅ 通过 | 模拟同步不可达时本地写入成功（`local_write_succeeded: true`），64 行转入待补推队列；读取不受影响 |
| SQLite 写前自动备份轮转 | ✅ 通过 | 24 次写入 → L1 快照保留 20 份（上限 20，`l1_failed: 0`，全部 integrity ok）；L2 全量备份 `l2_integrity: ok`、21 张表 JSON、保留 14 份上限 |
| 主数据 / 归档分离 | ✅ 通过 | 布局层 `workbench.db` / `archive.db` 分文件，备份目录独立可见 |
| schema 版本号 + 迁移脚本 | ✅ 通过 | `schema_migration` 表 + 只增可重入迁移；库版本高于目标版本时拒绝启动（测试覆盖） |
| 更新通道：静态 JSON + 下载 URL，零鉴权 | ✅ 通过 | `latest.json` 四平台必填、`signature` 必须是内容本身（填路径直接拒绝，实测 ⑥）、缺任一平台键判整份清单无效（实测 ⑦）、Ed25519 验签（通过/篡改拒绝，实测 ④⑤） |
| 更新包签名、密钥不进仓库 | ✅ 通过 | 私钥只走 `TAURI_SIGNING_PRIVATE_KEY` 环境变量（CI secrets）；仓库与本文档均无任何密钥材料 |
| 时间线画区间带、不编日精度 | ✅ 通过 | 64 项中月末粒度 54 / 日 10，前端时间线月粒度画整月区间带（斜纹），不显示虚构日 |
| F10 语音播报首版只留接口、总开关显眼且默认关闭 | ✅ 通过 | 主界面顶栏常驻「语音播报」总开关，`disabled` 且不勾选；设置页标注「P2 二期」 |
| 安装包 ≤500MB | ⚠️ **未实测** | 红线口径为 500MB（用户 2026-10-27 确认，原 15MB 已作废）。本沙箱无 Windows 工具链与 GUI 依赖（详见第四节），无法产出 .exe；CI（`.github/workflows/windows-release.yml`）已内置「超 500MB 即失败」的硬门，首个真实安装包出来后此项才算闭环 |
| 空闲内存 ≤60MB | ⚠️ **未实测** | 同上，需 Windows 目标机进程实测；无估算值 |
| 不做真人声纹克隆 | ✅ 通过 | F10 全部实现物中无任何声纹/克隆代码，仅一个 disabled 开关 |

## 二、功能与规则实测

- **导入对账**：64 项 = 板块分布 政府 12 / 行业 11 / 媒体 14 / 活动 10 / 客户 9 / 政策 8；KPI 10 项；跳过 1（占位行 `客户名称=1`，架构 §2.3 预判命中）。
- **落库回读**：写入 64 项 + 10 KPI，回读一致（`items_readback: 64`）。
- **KPI 派生**：口径落不到板块时返回空并显示「—」，**不退化成全库计数**（该缺陷已在自检中发现并修复：曾把「政策项目总数」算成 64，实际应为 8）。
- **规则引擎**（基准日 2026-10-26）：候选提醒 56 条 = 逾期 49 + 临期 7；日配额 5 条约束主动推送；「初始状态不算进度落后」验证通过（`behind_when_enabled: 0`）。
- **晨报（F2/D）**：四段结构产出正常；周一附加周聚合。
- **同步合并（Rust 核心单测 40 个全过）**：行级 LWW 三键决胜、`progress_pct` 取 max 例外、同刻内容不同判「待人工确认」、被覆盖值永久进冲突日志。
- **同步服务端端到端**（Node 22，实测）：setup/login/invite(48h)/members 全通；旧时间戳高进度 push → `local_wins_progress_max`（标题保留、进度抬升、冲突留痕）；游标增量无重复投递；schema 不匹配 409；`VACUUM INTO` 备份成功。

## 三、本次交付修复的缺陷（全部有实测佐证）

1. KPI 板块口径退化（64 vs 8）——别名映射 + `Option` 口径，宁缺勿错。
2. `progress_pct` max 等于胜出方原值时被误标为例外——改为「真改写才标记」。
3. 同步服务端 `/sync/pull`、`/sync/conflicts` 因 `url` 参数未透传一律 400——修复并回归。
4. 同步服务端 `local_wins` 分支漏掉「取 max」例外与冲突留痕——与 Rust 核心对齐后回归。
5. 更新清单 `signature` 填路径不拒绝——按 Tauri 校验顺序补齐（实测 ⑥）。

## 四、环境限制与未测项声明（如实）

自检机为 1 核 Linux 沙箱：无 `x86_64-pc-windows-msvc` / mingw / zig 工具链，无 `webkit2gtk-4.1` 与 `gtk+-3.0`，无 sudo。

**工具链可装性（2026-10-27 复核后更正）**：曾有「工具链损坏后无法重装」的表述，此说法不准确——`static.rust-lang.org` 与 GitHub Releases 在本沙箱确实不可达，但**改用 rsproxy.cn 镜像可以正常安装 stable 工具链**，且已用它实跑通 `cargo test -p wb-core --locked`（75 passed / 0 failed）。下一个复核者请按下面命令装，不要被旧结论误导：

```bash
export RUSTUP_DIST_SERVER=https://rsproxy.cn
export RUSTUP_UPDATE_ROOT=https://rsproxy.cn/rustup
curl -sSf https://rsproxy.cn/rustup/dist/x86_64-unknown-linux-gnu/rustup-init -o /tmp/rustup-init
chmod +x /tmp/rustup-init && /tmp/rustup-init -y --profile minimal --default-toolchain stable
```

（注意路径是 `/rustup/dist/<target>/rustup-init`；`https://rsproxy.cn/rustup-init` 会返回 404——这是本沙箱实测过的。）

因此：

- **Windows 安装包未产出、体积未实测**——不提供估算值；CI 已内置 500MB 硬门禁。
- **Tauri 壳与前端未在本机编译/运行过**——`apps/desktop` 代码是按 Tauri 2 稳定 API 与 wb-core 已实测签名编写，但未经编译验证；`wbd` 自检二进制为上一轮构建（源码在其后仅有一处已核对的两参 `fail()` 一致性小改，全部调用点已逐一核对一致）。
- 上述两项须在 Windows 目标机/CI 上补测后，红线表才视为全数闭环。

## 五、原始数据

完整 JSON 见同目录 `selfcheck-2026-10-27.json`（`wbd selfcheck ../../data/source.xlsx 2026 <data_dir>` 产出）。
