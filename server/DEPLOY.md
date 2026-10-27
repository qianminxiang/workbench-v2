# 同步服务端部署说明（交付物 ② 的一部分）

**定位（架构 §4.12–4.15）**：薄服务，只做三件事——存团队镜像、收 push 按行合并、按游标发增量。不做业务计算、不做调度、不做抓取。Node 22 内置模块实现（`node:http` / `node:crypto` / `node:sqlite`），**零 npm 依赖**，拷目录即部署。

## 1. 环境要求

- Node.js ≥ 22（需要内置 `node:sqlite`；部署机 `node -v` 确认）
- 一台 3–10 人团队可访问的服务器（内网穿透或云主机均可；公网必须 TLS）

## 1.5 铁律：**部署包内不得含数据库文件**（2026-10-27 P0 事故后新增）

**事故经过**：`server/data/team.db`（连同 `-wal` / `-shm`）曾被一起放进交付包并上传到云盘，任何人下载后都能直接读到成员表里的 Bearer token 明文与 admin 口令的 scrypt 哈希。泄露发生在**部署流程**，不是打包环节的疏忽。

**规则（强制）**：

1. **交付包内不得出现任何 `*.db` / `*.db-wal` / `*.db-shm` / `*.sqlite*`**。`server/data/` 整体由 `.gitignore` 排除，打包前必须复核：
   ```bash
   unzip -l <交付包>.zip | grep -E '\.db|\.sqlite'   # 必须无输出
   ```
2. **首次部署由服务自行建空库**：`server/data/` 留空目录即可，进程启动时 `new DatabaseSync(...)` 自动创建，`meta.schema_version` 与 `seq` 由代码初始化。**不要把任何已初始化的库拷到部署机。**
3. **首个 admin 用新口令重新注册**：空库首启后调 `POST /setup`，用户名与口令**与任何历史值无关**，不要复用旧口令。
4. **已外泄凭据的处理**：不要用 `POST /members/remove` 禁用唯一 admin——那会让团队彻底无法登录、也无人能再发邀请，比泄露更糟。正确做法就是上面第 2、3 条：**让那个库根本不存在**，token 与哈希随库一起消失。

## 2. 部署步骤

1. 拷贝 `server/` 目录到部署机（例如 `/opt/wb-sync`）。
2. 建专用低权限用户与数据目录：
   ```bash
   sudo useradd -r -s /usr/sbin/nologin workbench
   sudo mkdir -p /opt/wb-sync/data && sudo chown -R workbench:workbench /opt/wb-sync/data
   ```
3. 安装 systemd 单元：`sudo cp deploy/wb-sync.service /etc/systemd/system/ && sudo systemctl daemon-reload && sudo systemctl enable --now wb-sync`
4. 公网部署时配置 nginx + TLS（见 `deploy/nginx.conf.example`）；内网部署可直接暴露 8443，但 8444 管理端口永远只允许回环访问。
5. 验证：`curl http://127.0.0.1:8443/health` 返回 `{"ok":true,"schema_version":2,...}`（v2 为多 token / 离线补推版本）。

## 3. 初始化与成员管理

1. 首成员初始化（只允许一次，成为 admin）：`POST /setup`，body `{"username":"名字","password":"≥8位"}`，返回成员 ID 与登录 token。
2. 邀请成员：admin 调 `POST /invite`（48 小时有效邀请码）。
3. 成员加入：`POST /join`（凭邀请码 + 用户名/口令）。
4. 日常登录：`POST /login`（scrypt 口令散列 + 计时安全比较；同 IP 连续失败 10 次锁定）。
5. 手动备份：admin 在部署机本机 `POST http://127.0.0.1:8444/admin/backup`（`VACUUM INTO` 全量备份，保留 14 份；另有每日自动备份）。

## 4. 同步语义（与客户端 Rust 核心逐条对齐，两侧已各有实测）

- 行级 LWW：`updated_at` → `updated_by` → `device_id` 三键决胜；
- 定向例外：`progress_pct` 取 max，任一方向胜出都生效（只有真改写胜出方进度才记为例外）；
- 任一方向的覆盖都写 `conflict_log`（被覆盖值永久可查，不定期清理）；
- 删除一律软删参与同步；客户端版本与服务端 `schema_version` 不一致时 push 返回 409，不做半吊子合并。

## 5. 实测记录（2026-10-27，本仓库交付前在部署形态下跑通）

- `/setup` → `/login` → `/invite`（48h TTL）→ `/members` 全链路 200；
- push 新行 → `inserted`（seq=1）；push 旧时间戳 + 高进度行 → `local_wins_progress_max`（标题保留、进度抬升到 80），且被覆盖值进冲突日志；
- `/sync/pull?cursor=0` 增量正确、`cursor=2` 二次拉取 0 行（游标无重复投递）；
- `schema_version: 9` push → 409 版本协商拒绝；
- `/admin/backup` 成功（`VACUUM INTO`，产物 53,248 字节）。

## 6. 运维注意

- 数据目录含 SQLite 主库与每日备份，建议放独立分区并做系统级快照；
- 备份保留 14 份自动轮转，磁盘占用在 3–10 人规模下长期 <100MB；
- 升级服务端前先 `POST /admin/backup` 手动打一份全量备份。
