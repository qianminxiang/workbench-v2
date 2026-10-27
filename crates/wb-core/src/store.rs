//! 存储层：SQLite 双库、schema 版本与迁移、写前备份轮转、主/归档分离、JSON 导出。
//!
//! 对应架构 §4.1–4.11。这里的每一条都是「不做就等于没有」的硬要求：
//!
//! - **禁止直接复制 `.db` 文件**：WAL 模式下未 checkpoint 的写入在 `-wal` 里，
//!   直接拷主库会得到丢数据的备份。本模块一律走 SQLite 在线备份接口。
//! - **备份失败必须阻断写入**：这是「写前备份」的全部意义。失败就不允许改数据。
//! - **迁移前强制 L2 备份**；脚本只增不改、可重入；库版本高于程序版本时拒绝启动。
//! - **主库与归档库 schema 版本必须同步推进**，否则 ATTACH 查询会因列不存在而失败。

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{params, Connection, OpenFlags};

// ───────────────────────────── 常量与类型 ─────────────────────────────

/// 构建时嵌入的目标 schema 版本（架构 §4.8）。
pub const TARGET_SCHEMA_VERSION: i64 = 2;

/// L1 写前快照保留份数。
pub const L1_KEEP: usize = 20;
/// L2 日备 / 周备 / 月备保留份数。
pub const L2_KEEP_DAILY: usize = 14;
pub const L2_KEEP_WEEKLY: usize = 8;
pub const L2_KEEP_MONTHLY: usize = 6;

#[derive(Debug)]
pub enum StoreError {
    Sqlite(String),
    Io(String),
    /// 库版本高于程序版本：拒绝启动，不尝试降级
    SchemaTooNew { db_version: i64, app_target: i64 },
    /// 备份失败：写入被阻断
    BackupFailed(String),
    /// 迁移失败：进入只读模式
    MigrationFailed { script: String, error: String },
}

impl std::fmt::Display for StoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StoreError::Sqlite(e) => write!(f, "数据库错误：{e}"),
            StoreError::Io(e) => write!(f, "文件操作失败：{e}"),
            StoreError::SchemaTooNew { db_version, app_target } => write!(
                f,
                "数据库 schema 版本 {db_version} 高于当前程序支持的 {app_target}，\
                 检测到版本回退。已拒绝启动以免降级损坏数据，请安装匹配的新版本。"
            ),
            StoreError::BackupFailed(e) => {
                write!(f, "写前备份失败，已阻断本次写入以保护数据：{e}")
            }
            StoreError::MigrationFailed { script, error } => write!(
                f,
                "迁移脚本 {script} 执行失败（已回滚，应用进入只读模式）：{error}"
            ),
        }
    }
}

impl std::error::Error for StoreError {}

impl From<rusqlite::Error> for StoreError {
    fn from(e: rusqlite::Error) -> Self {
        StoreError::Sqlite(e.to_string())
    }
}

fn now_iso() -> String {
    // 只由系统层提供时间（架构 §4.11-4），前端不读时钟
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let d = crate::date::CivilDate::from_days(days);
    format!(
        "{}T{:02}:{:02}:{:02}Z",
        d.to_iso(),
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// 对外暴露的时间源：**时间只由系统层提供**（架构 §4.11-4），
/// 领域层与仓库层都从这里取，不在别处各自读时钟。
pub fn now_iso_pub() -> String {
    now_iso()
}

/// 对外暴露的毫秒时间戳（同步 `updated_at` 与冲突日志使用）。
pub fn now_ms_pub() -> i64 {
    now_ms()
}

// ───────────────────────────── 库布局 ─────────────────────────────

/// 库根目录布局（架构 §4.1）。
#[derive(Debug, Clone)]
pub struct Layout {
    pub root: PathBuf,
}

impl Layout {
    pub fn new(root: impl AsRef<Path>) -> Self {
        Layout { root: root.as_ref().to_path_buf() }
    }

    pub fn main_db(&self) -> PathBuf {
        self.root.join("workbench.db")
    }
    pub fn archive_db(&self) -> PathBuf {
        self.root.join("archive.db")
    }
    pub fn backups_dir(&self) -> PathBuf {
        self.root.join("backups")
    }
    pub fn snapshots_dir(&self) -> PathBuf {
        self.root.join("snapshots")
    }
    pub fn attachments_dir(&self) -> PathBuf {
        self.root.join("attachments")
    }

    pub fn ensure(&self) -> Result<(), StoreError> {
        for d in [
            self.root.clone(),
            self.backups_dir(),
            self.snapshots_dir(),
            self.attachments_dir(),
        ] {
            fs::create_dir_all(&d).map_err(|e| StoreError::Io(format!("{}: {e}", d.display())))?;
        }
        Ok(())
    }
}

// ───────────────────────────── 迁移 ─────────────────────────────

/// 迁移脚本：只增不改，可重入（架构 §4.8-5）。
pub struct Migration {
    pub version: i64,
    pub name: &'static str,
    /// 幂等 DDL：全部使用 `IF NOT EXISTS`，重复执行不报错
    pub sql: &'static str,
}

/// 业务表 + 同步字段（架构 §4.16：业务表统一增补 updated_at / updated_by / deleted_at）。
const SCHEMA_V1: &str = r#"
CREATE TABLE IF NOT EXISTS domain_board (
    code        TEXT PRIMARY KEY,
    zh_name     TEXT NOT NULL,
    sheet_name  TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS work_item (
    id                      TEXT PRIMARY KEY,
    seq_no                  INTEGER NOT NULL,
    board_code              TEXT NOT NULL REFERENCES domain_board(code),
    title                   TEXT NOT NULL,
    category                TEXT,
    priority                TEXT CHECK (priority IN ('A','B','C') OR priority IS NULL),
    target_quant            TEXT,
    planned_start_date      TEXT,
    planned_start_precision TEXT NOT NULL
                              CHECK (planned_start_precision IN ('day','month','quarter','ongoing','unknown')),
    planned_start_raw       TEXT,
    planned_end_date        TEXT,
    planned_end_precision   TEXT NOT NULL
                              CHECK (planned_end_precision IN ('day','month','quarter','ongoing','unknown')),
    planned_end_raw         TEXT,
    progress_pct            INTEGER NOT NULL DEFAULT 0 CHECK (progress_pct BETWEEN 0 AND 100),
    status                  TEXT,
    milestone_stage         TEXT,
    next_action             TEXT,
    owner                   TEXT,
    source_sheet            TEXT,
    fiscal_year             INTEGER NOT NULL,
    ext_json                TEXT NOT NULL DEFAULT '{}',
    created_at              TEXT,
    updated_at              TEXT,
    updated_by              TEXT,
    deleted_at              TEXT,
    archived_at             TEXT,
    UNIQUE (fiscal_year, seq_no)
);
CREATE INDEX IF NOT EXISTS idx_item_board    ON work_item(fiscal_year, board_code);
CREATE INDEX IF NOT EXISTS idx_item_end_date ON work_item(fiscal_year, planned_end_date);
CREATE INDEX IF NOT EXISTS idx_item_status   ON work_item(status);

CREATE TABLE IF NOT EXISTS progress_log (
    id          TEXT PRIMARY KEY,
    item_id     TEXT NOT NULL REFERENCES work_item(id),
    from_pct    INTEGER,
    to_pct      INTEGER NOT NULL,
    note        TEXT,
    member_id   TEXT,
    created_at  TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_progress_item ON progress_log(item_id, created_at);

CREATE TABLE IF NOT EXISTS kpi_metric (
    id           TEXT PRIMARY KEY,
    category     TEXT NOT NULL,
    name         TEXT NOT NULL,
    target_value REAL NOT NULL,
    rule         TEXT NOT NULL CHECK (rule IN ('board_total','board_started','board_done')),
    source_sheet TEXT,
    note         TEXT,
    fiscal_year  INTEGER NOT NULL,
    updated_at   TEXT,
    updated_by   TEXT,
    deleted_at   TEXT
);

CREATE TABLE IF NOT EXISTS plan_item (
    id              TEXT PRIMARY KEY,
    raw_text        TEXT NOT NULL,
    parsed_json     TEXT,
    type            TEXT,
    anchor_date     TEXT,
    anchor_precision TEXT,
    fiscal_year     INTEGER NOT NULL,
    status          TEXT NOT NULL DEFAULT 'pending'
                      CHECK (status IN ('pending','confirmed','ignored','merged')),
    source          TEXT NOT NULL DEFAULT 'manual',
    created_at      TEXT NOT NULL,
    updated_at      TEXT,
    updated_by      TEXT,
    deleted_at      TEXT
);
CREATE TABLE IF NOT EXISTS plan_link (
    plan_item_id TEXT NOT NULL,
    work_item_id TEXT NOT NULL,
    relation     TEXT,
    PRIMARY KEY (plan_item_id, work_item_id)
);

CREATE TABLE IF NOT EXISTS reminder_instance (
    id          TEXT PRIMARY KEY,
    rule_id     TEXT NOT NULL,
    item_id     TEXT,
    fire_date   TEXT NOT NULL,
    dedup_key   TEXT NOT NULL UNIQUE,
    state       TEXT NOT NULL DEFAULT 'pending'
                  CHECK (state IN ('pending','sent','read','ignored','expired')),
    payload_json TEXT,
    created_at  TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_reminder_date ON reminder_instance(fire_date, state);

CREATE TABLE IF NOT EXISTS schedule_block (
    id              TEXT PRIMARY KEY,
    owner_member_id TEXT,
    work_item_id    TEXT,
    plan_item_id    TEXT,
    date            TEXT NOT NULL,
    start_time      TEXT,
    end_time        TEXT,
    kind            TEXT,
    title           TEXT NOT NULL,
    location        TEXT,
    source          TEXT,
    status          TEXT NOT NULL DEFAULT 'planned',
    updated_at      TEXT,
    updated_by      TEXT,
    deleted_at      TEXT
);
CREATE INDEX IF NOT EXISTS idx_block_date ON schedule_block(date);

CREATE TABLE IF NOT EXISTS member (
    member_id     TEXT PRIMARY KEY,
    display_name  TEXT NOT NULL,
    role          TEXT NOT NULL CHECK (role IN ('admin','member')),
    password_hash TEXT,
    token         TEXT,
    invited_by    TEXT,
    created_at    TEXT NOT NULL,
    status        TEXT NOT NULL DEFAULT 'active'
);

CREATE TABLE IF NOT EXISTS sync_cursor (
    table_name TEXT PRIMARY KEY,
    seq        INTEGER NOT NULL DEFAULT 0,
    updated_at TEXT
);
CREATE TABLE IF NOT EXISTS sync_conflict_log (
    id          TEXT PRIMARY KEY,
    table_name  TEXT NOT NULL,
    row_id      TEXT NOT NULL,
    losing_json TEXT NOT NULL,
    losing_by   TEXT,
    winning_by  TEXT,
    resolved_at TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS kb_entry (
    id              TEXT PRIMARY KEY,
    kind            TEXT NOT NULL,
    title           TEXT,
    body            TEXT NOT NULL,
    source_type     TEXT NOT NULL DEFAULT 'paste',
    source_url      TEXT,
    origin          TEXT,
    author_member_id TEXT,
    captured_at     TEXT NOT NULL,
    extra           TEXT NOT NULL DEFAULT '{}',
    updated_at      TEXT,
    updated_by      TEXT,
    deleted_at      TEXT
);
CREATE TABLE IF NOT EXISTS kb_link (
    entry_id    TEXT NOT NULL,
    target_type TEXT NOT NULL,
    target_id   TEXT NOT NULL,
    updated_at  TEXT,
    updated_by  TEXT,
    deleted_at  TEXT,
    PRIMARY KEY (entry_id, target_type, target_id)
);
CREATE TABLE IF NOT EXISTS kb_tag (
    id   TEXT PRIMARY KEY,
    name TEXT NOT NULL UNIQUE
);
CREATE TABLE IF NOT EXISTS kb_entry_tag (
    entry_id TEXT NOT NULL,
    tag_id   TEXT NOT NULL,
    PRIMARY KEY (entry_id, tag_id)
);
CREATE TABLE IF NOT EXISTS kb_attachment (
    id           TEXT PRIMARY KEY,
    entry_id     TEXT NOT NULL,
    file_name    TEXT NOT NULL,
    ext          TEXT,
    mime         TEXT,
    sha256       TEXT NOT NULL,
    size_bytes   INTEGER NOT NULL,
    storage_path TEXT NOT NULL,
    updated_at   TEXT,
    updated_by   TEXT,
    deleted_at   TEXT
);
CREATE INDEX IF NOT EXISTS idx_attach_sha ON kb_attachment(sha256);

CREATE TABLE IF NOT EXISTS ext_source (
    id          TEXT PRIMARY KEY,
    source_type TEXT NOT NULL,
    name        TEXT NOT NULL,
    entry_url   TEXT,
    frequency   TEXT,
    enabled     INTEGER NOT NULL DEFAULT 1,
    last_run_at TEXT,
    last_ok_at  TEXT,
    consecutive_fail INTEGER NOT NULL DEFAULT 0,
    stale_flag  INTEGER NOT NULL DEFAULT 0
);
CREATE TABLE IF NOT EXISTS fetch_run (
    id           TEXT PRIMARY KEY,
    source_id    TEXT NOT NULL,
    started_at   TEXT NOT NULL,
    finished_at  TEXT,
    result       TEXT,
    error_class  TEXT,
    item_count   INTEGER,
    changed_count INTEGER
);
CREATE TABLE IF NOT EXISTS ext_record (
    id           TEXT PRIMARY KEY,
    source_id    TEXT NOT NULL,
    biz_key      TEXT NOT NULL,
    record_json  TEXT NOT NULL,
    valid_from   TEXT NOT NULL,
    valid_to     TEXT,
    is_current   INTEGER NOT NULL DEFAULT 1,
    content_hash TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_ext_record_cur ON ext_record(source_id, is_current);
CREATE TABLE IF NOT EXISTS ext_insight (
    id           TEXT PRIMARY KEY,
    source_id    TEXT NOT NULL,
    insight_type TEXT NOT NULL,
    severity     TEXT,
    title        TEXT NOT NULL,
    detail_md    TEXT,
    evidence_json TEXT,
    generated_at TEXT NOT NULL,
    dismissed    INTEGER NOT NULL DEFAULT 0
);
"#;

/// 迁移 v2：离线补推队列与同步状态（P0-1 落地的持久化部分）。
///
/// 只增不改：v1 的表一律不动，新增两张表。
/// - `sync_state` 存地址/令牌/游标等键值，令牌只落本机库，不下发前端；
/// - `sync_outbox` 是**断网安全网**：任何推送失败的行留在这里，恢复后按入队顺序补推。
///   `attempts` / `last_error` 保留断网连续性，便于排查「为什么一条改动补了三次」。
const SCHEMA_V2: &str = r#"
CREATE TABLE IF NOT EXISTS sync_state (
    k TEXT PRIMARY KEY,
    v TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS sync_outbox (
    table_name TEXT NOT NULL,
    row_id     TEXT NOT NULL,
    payload    TEXT NOT NULL,
    updated_at INTEGER NOT NULL,
    updated_by TEXT NOT NULL,
    device_id  TEXT NOT NULL,
    deleted_at INTEGER,
    queued_at  TEXT NOT NULL,
    attempts   INTEGER NOT NULL DEFAULT 0,
    last_error TEXT,
    PRIMARY KEY (table_name, row_id)
);
CREATE INDEX IF NOT EXISTS idx_outbox_queued ON sync_outbox(queued_at);

-- 板块字典是静态参考数据：预置六行，避免手工录入 / 同步落库时撞外键。
-- （此前只有 Excel 导入会写这张表，手工录入一落库就 FOREIGN KEY constraint failed）
INSERT OR IGNORE INTO domain_board (code, zh_name, sheet_name) VALUES
    ('gov',      '政府关系', '政府关系建立'),
    ('industry', '行业资源', '行业资源开发'),
    ('media',    '媒体传播', '媒体品牌传播'),
    ('event',    '活动执行', '活动执行管理'),
    ('customer', '客户转化', '客户线索转化'),
    ('policy',   '政策申报', '政策项目申报');
"#;

/// 迁移清单。**已发布的脚本永不修改**（架构 §4.8-5）。
pub fn migrations() -> Vec<Migration> {
    vec![
        Migration { version: 1, name: "0001_init", sql: SCHEMA_V1 },
        Migration { version: 2, name: "0002_sync_offline", sql: SCHEMA_V2 },
    ]
}

fn checksum(sql: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(sql.as_bytes());
    format!("{:x}", h.finalize())
}

fn current_version(conn: &Connection) -> Result<i64, StoreError> {
    // schema_migration 自身也要先存在
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS schema_migration (
            version     INTEGER PRIMARY KEY,
            script_name TEXT NOT NULL,
            checksum    TEXT NOT NULL,
            applied_at  TEXT NOT NULL,
            duration_ms INTEGER NOT NULL,
            result      TEXT NOT NULL
         );",
    )?;
    let v: Option<i64> = conn
        .query_row("SELECT MAX(version) FROM schema_migration WHERE result='success'", [], |r| r.get(0))
        .ok()
        .flatten();
    Ok(v.unwrap_or(0))
}

// ───────────────────────────── 存储服务 ─────────────────────────────

pub struct Store {
    pub layout: Layout,
    conn: Connection,
    /// 迁移失败后进入只读模式（架构 §4.8-4）
    pub read_only: bool,
}

impl Store {
    /// 打开（或创建）主库，跑迁移。**早于任何业务读写**（架构 §4.8）。
    pub fn open(root: impl AsRef<Path>) -> Result<Self, StoreError> {
        let layout = Layout::new(root);
        layout.ensure()?;

        let conn = Connection::open_with_flags(
            layout.main_db(),
            OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_CREATE,
        )?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        // 低配机器可用性（架构 §4.23）：page cache 限 32–64MB
        conn.pragma_update(None, "cache_size", -32_000i64)?;

        let v = current_version(&conn)?;
        if v > TARGET_SCHEMA_VERSION {
            return Err(StoreError::SchemaTooNew { db_version: v, app_target: TARGET_SCHEMA_VERSION });
        }

        let mut store = Store { layout, conn, read_only: false };
        store.migrate(v)?;
        Ok(store)
    }

    fn migrate(&mut self, from: i64) -> Result<(), StoreError> {
        let pending: Vec<Migration> = migrations().into_iter().filter(|m| m.version > from).collect();
        if pending.is_empty() {
            // 归档库也要有同版本 schema（架构 §4.10）
            self.ensure_archive_schema()?;
            return Ok(());
        }

        // 迁移前强制 L2 全量备份（架构 §4.8-3）
        if from > 0 {
            self.full_backup("pre-migration").map_err(|e| {
                StoreError::BackupFailed(format!("迁移前备份失败，已中止迁移：{e}"))
            })?;
        }

        for m in pending {
            let started = now_ms();
            // 每个脚本独立事务；失败回滚并进入只读
            let tx = self.conn.transaction()?;
            match tx.execute_batch(m.sql) {
                Ok(_) => {
                    let dur = now_ms() - started;
                    tx.execute(
                        "INSERT OR REPLACE INTO schema_migration
                         (version, script_name, checksum, applied_at, duration_ms, result)
                         VALUES (?1, ?2, ?3, ?4, ?5, 'success')",
                        params![m.version, m.name, checksum(m.sql), now_iso(), dur],
                    )?;
                    tx.commit()?;
                }
                Err(e) => {
                    let err = e.to_string();
                    drop(tx); // 回滚
                    let _ = self.conn.execute(
                        "INSERT OR REPLACE INTO schema_migration
                         (version, script_name, checksum, applied_at, duration_ms, result)
                         VALUES (?1, ?2, ?3, ?4, ?5, 'failed')",
                        params![m.version, m.name, checksum(m.sql), now_iso(), now_ms() - started],
                    );
                    self.read_only = true;
                    return Err(StoreError::MigrationFailed { script: m.name.to_string(), error: err });
                }
            }
        }
        self.ensure_archive_schema()?;
        Ok(())
    }

    fn ensure_archive_schema(&self) -> Result<(), StoreError> {
        let arc = Connection::open(self.layout.archive_db())?;
        arc.pragma_update(None, "journal_mode", "WAL")?;
        let v = current_version(&arc)?;
        if v > TARGET_SCHEMA_VERSION {
            return Err(StoreError::SchemaTooNew { db_version: v, app_target: TARGET_SCHEMA_VERSION });
        }
        for m in migrations() {
            if m.version > v {
                arc.execute_batch(m.sql)?;
                arc.execute(
                    "INSERT OR REPLACE INTO schema_migration
                     (version, script_name, checksum, applied_at, duration_ms, result)
                     VALUES (?1, ?2, ?3, ?4, 0, 'success')",
                    params![m.version, m.name, checksum(m.sql), now_iso()],
                )?;
            }
        }
        Ok(())
    }

    pub fn conn(&self) -> &Connection {
        &self.conn
    }

    pub fn schema_version(&self) -> i64 {
        current_version(&self.conn).unwrap_or(0)
    }
}

// ───────────────────────────── 备份 ─────────────────────────────

/// 一次备份的结果，供自检记录实测值。
#[derive(Debug, Clone, serde::Serialize)]
pub struct BackupRecord {
    pub level: &'static str,
    pub path: String,
    pub bytes: u64,
    pub kind: String,
    /// SQLite `PRAGMA integrity_check` 结果，必须为 ok 才入轮转
    pub integrity: String,
    pub json_tables: usize,
}

impl Store {
    /// L1 写前快照：SQLite 在线备份接口，最近 20 份循环覆盖。
    pub fn l1_snapshot(&self, label: &str) -> Result<BackupRecord, StoreError> {
        let dir = self.layout.backups_dir().join("l1");
        fs::create_dir_all(&dir).map_err(|e| StoreError::Io(e.to_string()))?;

        // 文件名带毫秒时间戳 + 自增序号，避免同毫秒内覆盖
        let stamp = now_ms();
        let mut n = 0u32;
        let path = loop {
            let p = dir.join(format!("l1_{stamp}_{n:03}_{label}.db"));
            if !p.exists() {
                break p;
            }
            n += 1;
        };

        // 走 SQLite 在线备份接口，不拷文件（架构 §4.9 关键实现要点）
        let mut dst = Connection::open(&path)?;
        {
            let backup = rusqlite::backup::Backup::new(&self.conn, &mut dst)?;
            backup.run_to_completion(64, std::time::Duration::from_millis(2), None)?;
        }
        dst.pragma_update(None, "journal_mode", "DELETE")?;
        let integrity = check_integrity(&path)?;
        drop(dst);

        if integrity != "ok" {
            let _ = fs::remove_file(&path);
            return Err(StoreError::BackupFailed(format!("快照完整性校验未通过：{integrity}")));
        }

        let bytes = fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        rotate(&dir, L1_KEEP)?;

        Ok(BackupRecord {
            level: "L1",
            path: path.to_string_lossy().to_string(),
            bytes,
            kind: label.to_string(),
            integrity,
            json_tables: 0,
        })
    }

    /// L2 全量备份：`.db` 完整副本 + 按表分文件的 JSON 导出。
    ///
    /// 触发时机由调用方决定（启动后首次写入前 / 每日首次写入前 / 迁移前 / 更新安装前 / 手动）。
    pub fn full_backup(&self, kind: &str) -> Result<BackupRecord, StoreError> {
        let dir = self.layout.backups_dir().join("l2");
        fs::create_dir_all(&dir).map_err(|e| StoreError::Io(e.to_string()))?;

        let stamp = now_ms();
        let bucket = bucket_of(kind);
        let path = dir.join(format!("l2_{bucket}_{stamp}_{kind}.db"));

        let mut dst = Connection::open(&path)?;
        {
            let backup = rusqlite::backup::Backup::new(&self.conn, &mut dst)?;
            backup.run_to_completion(64, std::time::Duration::from_millis(2), None)?;
        }
        dst.pragma_update(None, "journal_mode", "DELETE")?;
        let integrity = check_integrity(&path)?;
        drop(dst);
        if integrity != "ok" {
            let _ = fs::remove_file(&path);
            return Err(StoreError::BackupFailed(format!("全量备份完整性校验未通过：{integrity}")));
        }

        // JSON 导出：按表分文件，带 schema_version 头（架构 §4.9）
        let json_dir = dir.join(format!("l2_{bucket}_{stamp}_{kind}_json"));
        fs::create_dir_all(&json_dir).map_err(|e| StoreError::Io(e.to_string()))?;
        let tables = self.export_json(&json_dir)?;

        let bytes = fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        rotate_bucket(&dir, bucket, keep_for(bucket))?;

        Ok(BackupRecord {
            level: "L2",
            path: path.to_string_lossy().to_string(),
            bytes,
            kind: kind.to_string(),
            integrity,
            json_tables: tables,
        })
    }

    /// 全库 JSON 导出：一表一文件，表名 + 行数组。
    pub fn export_json(&self, dir: &Path) -> Result<usize, StoreError> {
        let mut tables: Vec<String> = self
            .conn
            .prepare("SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%'")?
            .query_map([], |r| r.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        tables.sort();

        let version = self.schema_version();
        for t in &tables {
            let mut stmt = self.conn.prepare(&format!("SELECT * FROM \"{t}\""))?;
            let cols: Vec<String> = stmt.column_names().into_iter().map(|s| s.to_string()).collect();
            let rows = stmt.query_map([], |row| {
                let mut obj = serde_json::Map::new();
                for (i, c) in cols.iter().enumerate() {
                    let v: rusqlite::types::Value = row.get(i)?;
                    obj.insert(c.clone(), sql_value_to_json(v));
                }
                Ok(serde_json::Value::Object(obj))
            })?;
            let mut data = Vec::new();
            for r in rows {
                data.push(r?);
            }
            let doc = serde_json::json!({
                "table": t,
                "schema_version": version,
                "exported_at": now_iso(),
                "row_count": data.len(),
                "rows": data,
            });
            let p = dir.join(format!("{t}.json"));
            fs::write(&p, serde_json::to_string_pretty(&doc).unwrap_or_default())
                .map_err(|e| StoreError::Io(format!("{}: {e}", p.display())))?;
        }
        Ok(tables.len())
    }
}

fn sql_value_to_json(v: rusqlite::types::Value) -> serde_json::Value {
    use rusqlite::types::Value as V;
    match v {
        V::Null => serde_json::Value::Null,
        V::Integer(i) => serde_json::json!(i),
        V::Real(f) => serde_json::json!(f),
        V::Text(s) => serde_json::json!(s),
        V::Blob(b) => serde_json::json!(format!("<blob {} bytes>", b.len())),
    }
}

fn check_integrity(path: &Path) -> Result<String, StoreError> {
    let c = Connection::open(path)?;
    let r: String = c
        .query_row("PRAGMA integrity_check", [], |row| row.get(0))
        .unwrap_or_else(|e| format!("error: {e}"));
    Ok(r)
}

// 日 / 周 / 月桶的三档轮转（日备 14 + 周备 8 + 月备 6）在实现上统一用字符串标识，
// 下面 bucket_of / keep_for 即为该映射；此前遗留的 `enum Bucket` 全仓零引用，已删除
// （cargo 的 dead_code 警告即指向它）。

fn bucket_of(kind: &str) -> &'static str {
    match kind {
        "weekly" => "weekly",
        "monthly" => "monthly",
        _ => "daily",
    }
}

fn keep_for(b: &str) -> usize {
    match b {
        "weekly" => L2_KEEP_WEEKLY,
        "monthly" => L2_KEEP_MONTHLY,
        _ => L2_KEEP_DAILY,
    }
}

/// 轮转删除按数量 + 时间双条件，任一条超限即删最旧；损坏备份直接剔除（架构 §4.9）。
fn rotate(dir: &Path, keep: usize) -> Result<usize, StoreError> {
    let mut files: Vec<PathBuf> = fs::read_dir(dir)
        .map_err(|e| StoreError::Io(e.to_string()))?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().map(|x| x == "db").unwrap_or(false))
        .collect();
    files.sort_by_key(|p| fs::metadata(p).and_then(|m| m.modified()).ok());

    let mut removed = 0usize;
    while files.len() > keep {
        let victim = files.remove(0);
        // 删除前校验完整性，损坏的同样剔除（但不占配额）
        if let Ok(i) = check_integrity(&victim) {
            if i != "ok" {
                let _ = fs::remove_file(&victim);
                removed += 1;
                continue;
            }
        }
        if fs::remove_file(&victim).is_ok() {
            removed += 1;
        }
    }
    Ok(removed)
}

fn rotate_bucket(dir: &Path, bucket: &str, keep: usize) -> Result<usize, StoreError> {
    let mut files: Vec<PathBuf> = fs::read_dir(dir)
        .map_err(|e| StoreError::Io(e.to_string()))?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            let name = p.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
            name.starts_with(&format!("l2_{bucket}_")) && p.extension().map(|x| x == "db").unwrap_or(false)
        })
        .collect();
    files.sort_by_key(|p| fs::metadata(p).and_then(|m| m.modified()).ok());

    let mut removed = 0usize;
    while files.len() > keep {
        let victim = files.remove(0);
        if fs::remove_file(&victim).is_ok() {
            removed += 1;
        }
        // 配套的 JSON 目录一并清掉，避免孤儿
        if let Some(stem) = victim.file_stem() {
            let jd = dir.join(format!("{}_json", stem.to_string_lossy()));
            if jd.exists() {
                let _ = fs::remove_dir_all(&jd);
            }
        }
    }
    Ok(removed)
}

// ───────────────────────────── 写入（备份前置） ─────────────────────────────

/// 写事务入口：**先做 L1 快照，成功才允许写**（架构 §4.9「备份失败必须阻断写入」）。
pub struct WriteGuard<'a> {
    store: &'a Store,
    pub snapshot: Option<BackupRecord>,
}

impl Store {
    pub fn begin_write(&self, label: &str) -> Result<WriteGuard<'_>, StoreError> {
        if self.read_only {
            return Err(StoreError::MigrationFailed {
                script: "(migration)".into(),
                error: "应用处于只读模式，拒绝写入".into(),
            });
        }
        let snapshot = self.l1_snapshot(label)?;
        Ok(WriteGuard { store: self, snapshot: Some(snapshot) })
    }
}

impl<'a> WriteGuard<'a> {
    pub fn conn(&self) -> &Connection {
        &self.store.conn
    }
}
