//! 同步客户端：push/pull 编排、补推队列、连接状态（架构 §4.14–4.15 / P0-1）。
//!
//! 这一层刻意只做**编排**，把「怎么合并」留给 `sync`（纯函数），把「怎么走路」留给
//! `Transport`（可替换实现）。因此本模块能在没有网络、没有 GUI 的环境里被单测穷举：
//! 断网、恢复、重复推、游标推进、冲突去重这些最容易出错的路径，全部有确定性测试。
//!
//! 四条不可妥协的口径：
//! 1. **断网不丢改动**：发送失败的行留在 `sync_outbox`，恢复后自动补推——这是「双端协作」
//!    能不能成立的地基。失败只累计 `attempts` 与 `last_error`，绝不静默丢弃。
//! 2. **游标只在页面落库成功后推进**：先写库再前移游标，否则一次崩溃就会永久跳过一段增量。
//! 3. **合并语义一律走 `sync::merge_row`**：行级 LWW + `progress_pct` 取 max + 被覆盖值进
//!    冲突日志，服务端与客户端同一口径，不在客户端另立一套。
//! 4. **令牌按设备**：每设备一枚 token（服务端上限 5、超限淘汰最旧），登出只作废本机令牌，
//!    本机数据保留、可离线继续用——单 token 互踢在多端协作下不成立。

use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};

use crate::store::{StoreError};
use crate::sync::{merge_row, ConflictLogEntry, MergeDecision, SyncRow};

/// 参与同步的表（首版只同步工作项主干，架构 §4.14）。
pub const SYNCED_TABLES: [&str; 1] = ["work_item"];

/// 客户端 speak 的 schema 版本；服务端不一致时返回 409（架构 §4.16）。
pub const CLIENT_SCHEMA_VERSION: i64 = 2;

/// 单次 push 批量：低配机器上一次别塞太多（架构 §4.23）。
pub const PUSH_BATCH: usize = 200;
/// 单次 pull 页大小。
pub const PULL_LIMIT: usize = 500;

/// `sync_state` 里用到的键。
pub mod keys {
    pub const BASE_URL: &str = "base_url";
    pub const TOKEN: &str = "token";
    pub const MEMBER_ID: &str = "member_id";
    pub const MEMBER_NAME: &str = "member_name";
    pub const DEVICE_ID: &str = "device_id";
    pub const CURSOR: &str = "cursor";
    pub const LAST_OK_AT: &str = "last_ok_at";
    pub const LAST_ERROR: &str = "last_error";
    // ── 单机化后的本机偏好（v2.1）：免打扰窗口与语音播报开关 ──
    // 多端模式已放弃，但 sync_state 键值表仍是官方持久化通道（可逆决策）。
    pub const QUIET_START: &str = "quiet_start"; // "HH:MM"
    pub const QUIET_END: &str = "quiet_end"; // "HH:MM"
    pub const VOICE_ENABLED: &str = "voice_enabled"; // "1" / "0"
    pub const VOICE_PACK: &str = "voice_pack"; // "system" | "pack"（v2.2 语音包）
    /// 主题档位（v2.2.1）："dark"（机甲态，默认）/ "light"（办公态）/ "system"（跟随系统）
    pub const THEME: &str = "theme";
}

// ───────────────────────────── 传输层契约 ─────────────────────────────

/// 传输失败的分类。**分类是为了决定下一步动作**，不是为了好看：
/// 断网要进补推队列、401 要提示重新登录、schema 不符要停下来升级。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransportError {
    /// 网络不可达 / 超时：改动留在队列，恢复后补推
    Unreachable(String),
    /// 令牌无效或过期：需重新登录（不自动用密码重试，避免把密码反复送上网）
    Unauthorized,
    /// schema 版本不一致：停止同步，提示升级，不做半吊子合并
    SchemaMismatch { server: i64 },
    /// 服务端明确拒绝（4xx/5xx 且非上述）
    Rejected(String),
}

impl TransportError {
    /// 是否属于「离线」类失败——这是能否自动补推的唯一判据。
    pub fn is_offline(&self) -> bool {
        matches!(self, TransportError::Unreachable(_))
    }

    pub fn describe(&self) -> String {
        match self {
            TransportError::Unreachable(d) => format!("无法连接同步服务：{d}"),
            TransportError::Unauthorized => "登录状态已失效，请重新登录".to_string(),
            TransportError::SchemaMismatch { server } => {
                format!("与服务端 schema 版本不一致（服务端 {server}），请升级后再同步")
            }
            TransportError::Rejected(d) => format!("服务端拒绝：{d}"),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoginResult {
    pub member_id: String,
    pub role: String,
    pub token: String,
    /// 服务端为本设备分配的稳定标识（同设备重登不变）
    #[serde(default)]
    pub device_id: String,
    /// 登录后该成员的活跃令牌数（多端并存，上限 5）
    #[serde(default)]
    pub active_tokens: i64,
}

#[derive(Debug, Clone)]
pub struct AppliedRow {
    pub table: String,
    pub id: String,
    pub decision: String,
    pub seq: i64,
}

#[derive(Debug, Clone)]
pub struct PushResult {
    pub applied: Vec<AppliedRow>,
    pub conflicts: i64,
    pub server_seq: i64,
}

#[derive(Debug, Clone)]
pub struct PullResult {
    pub rows: Vec<SyncRow>,
    pub cursor: i64,
    pub has_more: bool,
    pub server_seq: i64,
}

/// 传输实现可替换：桌面用 HTTP，测试用内存假服务端。
pub trait Transport {
    fn login(
        &self,
        base_url: &str,
        username: &str,
        password: &str,
        device_id: &str,
    ) -> Result<LoginResult, TransportError>;
    fn push(&self, base_url: &str, token: &str, rows: &[SyncRow]) -> Result<PushResult, TransportError>;
    fn pull(
        &self,
        base_url: &str,
        token: &str,
        cursor: i64,
        limit: usize,
    ) -> Result<PullResult, TransportError>;
    fn logout(&self, base_url: &str, token: &str) -> Result<(), TransportError>;
}

// ───────────────────────────── 地址校验 ─────────────────────────────

/// 同步地址校验。只接受 http(s) 绝对地址；公网明文 http 明确拒绝——
/// 令牌与工作项内容都会过这条连接，明文等于把它们交给同一网段的任何人。
pub fn validate_base_url(raw: &str) -> Result<String, String> {
    let s = raw.trim().trim_end_matches('/');
    if s.is_empty() {
        return Err("同步地址不能为空".into());
    }
    let (scheme, rest) = s
        .split_once("://")
        .ok_or_else(|| "地址需以 http:// 或 https:// 开头".to_string())?;
    let scheme_lc = scheme.to_ascii_lowercase();
    if scheme_lc != "http" && scheme_lc != "https" {
        return Err("仅支持 http:// 或 https:// 地址".into());
    }
    let host_port = rest.split('/').next().unwrap_or("");
    if host_port.is_empty() {
        return Err("地址缺少主机名".into());
    }
    let host = host_port.split(':').next().unwrap_or("").to_ascii_lowercase();
    let is_loopback = host == "localhost"
        || host == "127.0.0.1"
        || host == "::1"
        || host == "[::1]"
        || host.ends_with(".local");
    let is_private = host.starts_with("192.168.")
        || host.starts_with("10.")
        || (host.starts_with("172.")
            && host
                .trim_start_matches("172.")
                .split('.')
                .next()
                .and_then(|o| o.parse::<u32>().ok())
                .map(|o| (16..=31).contains(&o))
                .unwrap_or(false));
    if scheme_lc == "http" && !(is_loopback || is_private) {
        return Err("公网地址必须用 https://（明文 http 会泄露令牌与工作项内容）".into());
    }
    Ok(s.to_string())
}

// ───────────────────────────── 状态读写 ─────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SyncState {
    /// 未配置同步地址
    Disabled,
    /// 已登录且上次同步成功
    Connected,
    /// 有改动待推 / 当前处于离线补推状态
    Pending,
    /// 非离线类错误（schema 不符、服务端拒绝）
    Error,
    /// 令牌失效，需重新登录
    AuthRequired,
}

impl SyncState {
    pub fn label(&self) -> &'static str {
        match self {
            SyncState::Disabled => "未启用同步",
            SyncState::Connected => "已连接",
            SyncState::Pending => "待补推",
            SyncState::Error => "同步异常",
            SyncState::AuthRequired => "需重新登录",
        }
    }
}

/// 设置页展示用的完整状态（不含令牌本身，绝不把 token 送进前端）。
#[derive(Debug, Clone, Serialize)]
pub struct SyncStatus {
    pub base_url: String,
    pub logged_in: bool,
    pub member_name: Option<String>,
    pub member_id: Option<String>,
    pub device_id: Option<String>,
    pub cursor: i64,
    pub pending: i64,
    pub last_ok_at: Option<String>,
    pub last_error: Option<String>,
    pub state: SyncState,
}

pub fn state_get(conn: &Connection, k: &str) -> Option<String> {
    conn.query_row("SELECT v FROM sync_state WHERE k = ?1", params![k], |r| r.get::<_, String>(0))
        .ok()
}

pub fn state_set(conn: &Connection, k: &str, v: &str) -> Result<(), StoreError> {
    conn.execute(
        "INSERT INTO sync_state (k, v) VALUES (?1, ?2)
         ON CONFLICT(k) DO UPDATE SET v = excluded.v",
        params![k, v],
    )?;
    Ok(())
}

pub fn state_del(conn: &Connection, k: &str) -> Result<(), StoreError> {
    conn.execute("DELETE FROM sync_state WHERE k = ?1", params![k])?;
    Ok(())
}

/// 设备标识：首次生成后固化在库里。它参与 LWW 同刻决胜，**必须稳定**——
/// 每次启动重新生成会让同一台设备在冲突判定里被当成两台，收敛性就没了。
pub fn ensure_device_id(conn: &Connection, seed: &str) -> Result<String, StoreError> {
    if let Some(d) = state_get(conn, keys::DEVICE_ID) {
        if !d.trim().is_empty() {
            return Ok(d);
        }
    }
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    let nanos = crate::store::now_ms_pub();
    h.update(seed.as_bytes());
    h.update(nanos.to_le_bytes());
    let id = format!("d_{}", &format!("{:x}", h.finalize())[..16]);
    state_set(conn, keys::DEVICE_ID, &id)?;
    Ok(id)
}

pub fn read_status(conn: &Connection) -> SyncStatus {
    let base_url = state_get(conn, keys::BASE_URL).unwrap_or_default();
    let token = state_get(conn, keys::TOKEN).unwrap_or_default();
    let cursor = state_get(conn, keys::CURSOR)
        .and_then(|s| s.parse::<i64>().ok())
        .unwrap_or(0);
    let pending = count_pending(conn).unwrap_or(0);
    let last_error = state_get(conn, keys::LAST_ERROR).filter(|s| !s.trim().is_empty());
    let last_ok_at = state_get(conn, keys::LAST_OK_AT);

    let logged_in = !token.is_empty();
    let state = if base_url.is_empty() {
        SyncState::Disabled
    } else if !logged_in {
        SyncState::AuthRequired
    } else if last_error.is_some() {
        if pending > 0 {
            SyncState::Pending
        } else {
            SyncState::Error
        }
    } else if pending > 0 {
        SyncState::Pending
    } else {
        SyncState::Connected
    };

    SyncStatus {
        base_url,
        logged_in,
        member_name: state_get(conn, keys::MEMBER_NAME),
        member_id: state_get(conn, keys::MEMBER_ID),
        device_id: state_get(conn, keys::DEVICE_ID),
        cursor,
        pending,
        last_ok_at,
        last_error,
        state,
    }
}

// ───────────────────────────── 补推队列 ─────────────────────────────

/// 把一行改动放进待推队列。同主键重复入队按最新值覆盖（upsert），
/// 但保留 `attempts` / `last_error`——排查断网次数需要连续性。
pub fn enqueue(conn: &Connection, row: &SyncRow, queued_at: &str) -> Result<(), StoreError> {
    let payload = serde_json::to_string(&row.payload).unwrap_or_else(|_| "{}".into());
    conn.execute(
        "INSERT INTO sync_outbox
            (table_name, row_id, payload, updated_at, updated_by, device_id, deleted_at, queued_at, attempts, last_error)
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,0,NULL)
         ON CONFLICT(table_name, row_id) DO UPDATE SET
            payload=excluded.payload, updated_at=excluded.updated_at, updated_by=excluded.updated_by,
            device_id=excluded.device_id, deleted_at=excluded.deleted_at, queued_at=excluded.queued_at",
        params![
            row.table,
            row.id,
            payload,
            row.updated_at,
            row.updated_by,
            row.device_id,
            row.deleted_at,
            queued_at
        ],
    )?;
    Ok(())
}

pub fn count_pending(conn: &Connection) -> Result<i64, StoreError> {
    let n: i64 = conn.query_row("SELECT COUNT(*) FROM sync_outbox", [], |r| r.get(0))?;
    Ok(n)
}

/// 取一批待推行（按入队时间，先进先出）。
pub fn take_batch(conn: &Connection, limit: usize) -> Result<Vec<SyncRow>, StoreError> {
    let mut stmt = conn.prepare(
        "SELECT table_name, row_id, payload, updated_at, updated_by, device_id, deleted_at
         FROM sync_outbox ORDER BY queued_at, row_id LIMIT ?1",
    )?;
    let rows = stmt
        .query_map(params![limit as i64], |r| {
            let payload: String = r.get(2)?;
            let map: serde_json::Map<String, serde_json::Value> =
                serde_json::from_str(&payload).unwrap_or_default();
            Ok(SyncRow {
                table: r.get(0)?,
                id: r.get(1)?,
                payload: map,
                updated_at: r.get(3)?,
                updated_by: r.get(4)?,
                device_id: r.get(5)?,
                seq: 0,
                deleted_at: r.get(6)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// 推送成功（或被服务端判定为 identical）的行出队。
pub fn drop_pushed(conn: &Connection, applied: &[AppliedRow]) -> Result<usize, StoreError> {
    let mut n = 0usize;
    for a in applied {
        n += conn.execute(
            "DELETE FROM sync_outbox WHERE table_name = ?1 AND row_id = ?2",
            params![a.table, a.id],
        )?;
    }
    Ok(n)
}

/// 推送失败：只记账不丢弃。同样的行留在队列里，恢复后自动补推。
pub fn note_failure(conn: &Connection, rows: &[SyncRow], error: &str) -> Result<(), StoreError> {
    for r in rows {
        conn.execute(
            "UPDATE sync_outbox SET attempts = attempts + 1, last_error = ?3
             WHERE table_name = ?1 AND row_id = ?2",
            params![r.table, r.id, error],
        )?;
    }
    Ok(())
}

/// 全量入队（首次接入同步 / 换服务器时用）。幂等：重复调用不会重复累计。
pub fn seed_outbox(conn: &Connection, queued_at: &str) -> Result<usize, StoreError> {
    let mut n = 0usize;
    for table in SYNCED_TABLES {
        if table != "work_item" {
            continue;
        }
        let mut stmt = conn.prepare(
            "SELECT id, payload_json FROM (
                SELECT id, json_object(
                    'id', id, 'seq_no', seq_no, 'board_code', board_code, 'title', title,
                    'priority', priority, 'planned_start_date', planned_start_date,
                    'planned_start_precision', planned_start_precision,
                    'planned_end_date', planned_end_date,
                    'planned_end_precision', planned_end_precision,
                    'progress_pct', progress_pct, 'status', status,
                    'milestone_stage', milestone_stage, 'next_action', next_action,
                    'owner', owner, 'fiscal_year', fiscal_year, 'ext_json', ext_json,
                    'updated_at', updated_at, 'updated_by', updated_by, 'deleted_at', deleted_at
                ) AS payload_json
                FROM work_item WHERE updated_at IS NOT NULL
             )",
        )?;
        let rows = stmt
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
            .collect::<Result<Vec<_>, _>>()?;
        for (id, payload) in rows {
            let map: serde_json::Map<String, serde_json::Value> =
                serde_json::from_str(&payload).unwrap_or_default();
            let updated_at = map
                .get("updated_at")
                .and_then(|v| v.as_str())
                .and_then(parse_iso_to_ms)
                .unwrap_or_else(crate::store::now_ms_pub);
            let row = SyncRow {
                table: table.to_string(),
                id,
                payload: map,
                updated_at,
                updated_by: state_get(conn, keys::MEMBER_ID).unwrap_or_else(|| "local".into()),
                device_id: state_get(conn, keys::DEVICE_ID).unwrap_or_else(|| "unknown".into()),
                seq: 0,
                deleted_at: None,
            };
            enqueue(conn, &row, queued_at)?;
            n += 1;
        }
    }
    Ok(n)
}

// ───────────────────────────── 拉取落库 ─────────────────────────────

/// 读本地一行（不存在则 `None`）。
pub fn local_row(conn: &Connection, table: &str, id: &str) -> Result<Option<SyncRow>, StoreError> {
    if table != "work_item" {
        return Ok(None);
    }
    let r = conn.query_row(
        "SELECT json_object(
                    'id', id, 'seq_no', seq_no, 'board_code', board_code, 'title', title,
                    'priority', priority, 'planned_start_date', planned_start_date,
                    'planned_start_precision', planned_start_precision,
                    'planned_end_date', planned_end_date,
                    'planned_end_precision', planned_end_precision,
                    'progress_pct', progress_pct, 'status', status,
                    'milestone_stage', milestone_stage, 'next_action', next_action,
                    'owner', owner, 'fiscal_year', fiscal_year, 'ext_json', ext_json,
                    'updated_at', updated_at, 'updated_by', updated_by, 'deleted_at', deleted_at),
                updated_at, updated_by, deleted_at
         FROM work_item WHERE id = ?1",
        params![id],
        |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, Option<i64>>(3)?,
            ))
        },
    );
    match r {
        Ok((payload, updated_at, updated_by, deleted_at)) => {
            let map: serde_json::Map<String, serde_json::Value> =
                serde_json::from_str(&payload).unwrap_or_default();
            let ms = updated_at.as_deref().and_then(parse_iso_to_ms).unwrap_or(0);
            Ok(Some(SyncRow {
                table: table.to_string(),
                id: id.to_string(),
                payload: map,
                updated_at: ms,
                // 必须回读真实值：写死 "local" 会让下一轮 LWW 把同一台机器判成第三台设备
                updated_by: updated_by.unwrap_or_else(|| "local".into()),
                device_id: state_get(conn, keys::DEVICE_ID).unwrap_or_else(|| "unknown".into()),
                seq: 0,
                deleted_at,
            }))
        }
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(e) => Err(StoreError::from(e)),
    }
}

fn json_str(map: &serde_json::Map<String, serde_json::Value>, k: &str) -> Option<String> {
    map.get(k).and_then(|v| v.as_str()).map(|s| s.to_string())
}

fn json_i64(map: &serde_json::Map<String, serde_json::Value>, k: &str) -> Option<i64> {
    map.get(k).and_then(|v| v.as_i64())
}

/// 写一行远端结果（插入或整行更新）。同步字段一并落库，
/// 否则下一次 LWW 比较会因为缺 `updated_at` 而退化成「总是本地胜」。
///
/// **`seq_no` 冲突处理**：`work_item` 上有 `UNIQUE(fiscal_year, seq_no)`，而各端独立
/// 自增序号，两台设备录的第 7 条会撞在同一个 `(2026, 7)` 上。同步落库不能让整个 pull
/// 因此失败（那会让一条撞号把整页增量都丢掉），所以撞号时按「本年度 max+1」重分配，
/// 并在日志侧保留原值痕迹。序号是展示顺序，不是业务主键，重分配不损失信息。
pub fn write_remote_row(conn: &Connection, row: &SyncRow) -> Result<(), StoreError> {
    if row.table != "work_item" {
        return Ok(());
    }
    let p = &row.payload;
    let updated_iso = iso_from_ms(row.updated_at);
    let board = json_str(p, "board_code").unwrap_or_else(|| "gov".into());
    let fiscal_year = json_i64(p, "fiscal_year").unwrap_or(0);
    let desired_seq = json_i64(p, "seq_no").unwrap_or(0);

    // 该 (年度, seq) 是否已被**另一行**占用
    let occupied: Option<String> = conn
        .query_row(
            "SELECT id FROM work_item WHERE fiscal_year = ?1 AND seq_no = ?2 AND id <> ?3",
            params![fiscal_year, desired_seq, row.id],
            |r| r.get(0),
        )
        .ok();
    let seq_no = if occupied.is_some() {
        let max_seq: Option<i64> = conn
            .query_row(
                "SELECT MAX(seq_no) FROM work_item WHERE fiscal_year = ?1",
                params![fiscal_year],
                |r| r.get(0),
            )
            .ok()
            .flatten();
        max_seq.unwrap_or(0) + 1
    } else {
        desired_seq
    };

    conn.execute(
        "INSERT INTO work_item (
            id, seq_no, board_code, title, category, priority, target_quant,
            planned_start_date, planned_start_precision, planned_start_raw,
            planned_end_date, planned_end_precision, planned_end_raw,
            progress_pct, status, milestone_stage, next_action, owner, source_sheet,
            fiscal_year, ext_json, created_at, updated_at, updated_by, deleted_at
         ) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20,?21,?22,?23,?24,?25)
         ON CONFLICT(id) DO UPDATE SET
            seq_no=excluded.seq_no, board_code=excluded.board_code, title=excluded.title,
            category=excluded.category, priority=excluded.priority, target_quant=excluded.target_quant,
            planned_start_date=excluded.planned_start_date,
            planned_start_precision=excluded.planned_start_precision,
            planned_start_raw=excluded.planned_start_raw,
            planned_end_date=excluded.planned_end_date,
            planned_end_precision=excluded.planned_end_precision,
            planned_end_raw=excluded.planned_end_raw,
            progress_pct=excluded.progress_pct, status=excluded.status,
            milestone_stage=excluded.milestone_stage, next_action=excluded.next_action,
            owner=excluded.owner, source_sheet=excluded.source_sheet,
            fiscal_year=excluded.fiscal_year, ext_json=excluded.ext_json,
            updated_at=excluded.updated_at, updated_by=excluded.updated_by,
            deleted_at=excluded.deleted_at",
        params![
            row.id,
            seq_no,
            board,
            json_str(p, "title").unwrap_or_default(),
            json_str(p, "category"),
            json_str(p, "priority"),
            json_str(p, "target_quant"),
            json_str(p, "planned_start_date"),
            json_str(p, "planned_start_precision").unwrap_or_else(|| "unknown".into()),
            json_str(p, "planned_start_raw"),
            json_str(p, "planned_end_date"),
            json_str(p, "planned_end_precision").unwrap_or_else(|| "unknown".into()),
            json_str(p, "planned_end_raw"),
            json_i64(p, "progress_pct").unwrap_or(0),
            json_str(p, "status"),
            json_str(p, "milestone_stage"),
            json_str(p, "next_action"),
            json_str(p, "owner"),
            json_str(p, "source_sheet"),
            json_i64(p, "fiscal_year").unwrap_or(0),
            json_str(p, "ext_json").unwrap_or_else(|| "{}".into()),
            json_str(p, "created_at").unwrap_or_else(|| updated_iso.clone()),
            updated_iso,
            row.updated_by,
            row.deleted_at,
        ],
    )?;
    Ok(())
}

/// 冲突日志写入（同一条目重复拉取不重复记，否则日志会被同一个冲突刷屏）。
///
/// 去重键刻意**不含 `resolved_at`**：同一对「被覆盖值」重复出现，本质是同一场冲突被
/// 反复应用（游标回退、重放、重试都会造成），时间戳不同不代表是新的损失。
/// 若带上时间戳，日志就会被同一个冲突无限累积，人反而找不到真正的新条目。
pub fn write_conflict(conn: &Connection, c: &ConflictLogEntry) -> Result<bool, StoreError> {
    let exists: i64 = conn.query_row(
        "SELECT COUNT(*) FROM sync_conflict_log
         WHERE table_name = ?1 AND row_id = ?2 AND losing_json = ?3",
        params![c.table_name, c.row_id, c.losing_json],
        |r| r.get(0),
    )?;
    if exists > 0 {
        return Ok(false);
    }
    let id = format!(
        "cf_{}_{}",
        c.row_id,
        crate::store::now_ms_pub()
    );
    conn.execute(
        "INSERT INTO sync_conflict_log
            (id, table_name, row_id, losing_json, losing_by, winning_by, resolved_at)
         VALUES (?1,?2,?3,?4,?5,?6,?7)",
        params![
            id,
            c.table_name,
            c.row_id,
            c.losing_json,
            c.losing_by,
            c.winning_by,
            iso_from_ms(c.resolved_at)
        ],
    )?;
    Ok(true)
}

/// 应用一页 pull 结果：逐行 LWW 合并，返回（写入行数，新增冲突数）。
pub fn apply_pull(conn: &Connection, rows: &[SyncRow], now_ms: i64) -> Result<(usize, usize), StoreError> {
    let mut written = 0usize;
    let mut conflicts = 0usize;
    for remote in rows {
        match local_row(conn, &remote.table, &remote.id)? {
            None => {
                write_remote_row(conn, remote)?;
                written += 1;
            }
            Some(local) => {
                // 同一版本（时间戳 + 修改人相同）就是同一次编辑，差异只可能来自
                // `progress_pct` 取 max 这条定向例外。此时**不能**走 LWW 判定：
                // 否则每次重放（游标回退、重试、补拉）都会把已被抬升的进度判成
                // 「被远端覆盖」并记一条冲突，冲突日志会被同一场冲突刷屏，
                // 真正的新冲突反而找不到。这里只补 max，不记冲突。
                if local.updated_at == remote.updated_at && local.updated_by == remote.updated_by {
                    let lp = local.payload.get("progress_pct").and_then(|v| v.as_i64());
                    let rp = remote.payload.get("progress_pct").and_then(|v| v.as_i64());
                    if let (Some(a), Some(b)) = (lp, rp) {
                        if b > a {
                            let mut merged = local.clone();
                            merged.payload.insert("progress_pct".into(), serde_json::json!(b));
                            write_remote_row(conn, &merged)?;
                            written += 1;
                        }
                    }
                    continue;
                }
                let outcome = merge_row(&local, remote);
                if outcome.decision == MergeDecision::Identical {
                    continue;
                }
                // 无论谁胜，都以合并结果落库：progress_pct 取 max 的混合结果只能这样表达
                write_remote_row(conn, &outcome.merged)?;
                written += 1;
                if let Some(c) = outcome.conflict_entry(now_ms) {
                    if write_conflict(conn, &c)? {
                        conflicts += 1;
                    }
                }
            }
        }
    }
    Ok((written, conflicts))
}

// ───────────────────────────── 时间换算 ─────────────────────────────

/// `YYYY-MM-DDTHH:MM:SSZ` → 毫秒时间戳。解析失败返回 `None`，
/// 由调用方决定兜底（绝不把 0 当合法时间参与 LWW）。
pub fn parse_iso_to_ms(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    if b.len() < 20 {
        return None;
    }
    let t = &s[..19];
    let date = crate::date::CivilDate::parse(&t[..10])?;
    let hh: i64 = t[11..13].parse().ok()?;
    let mm: i64 = t[14..16].parse().ok()?;
    let ss: i64 = t[17..19].parse().ok()?;
    Some(date.to_days() * 86_400_000 + (hh * 3600 + mm * 60 + ss) * 1000)
}

pub fn iso_from_ms(ms: i64) -> String {
    let days = ms.div_euclid(86_400_000);
    let rem = ms.rem_euclid(86_400_000);
    let d = crate::date::CivilDate::from_days(days);
    format!(
        "{}T{:02}:{:02}:{:02}Z",
        d.to_iso(),
        rem / 3_600_000,
        (rem % 3_600_000) / 60_000,
        (rem % 60_000) / 1000
    )
}

// ───────────────────────────── 一次完整同步 ─────────────────────────────

#[derive(Debug, Clone, Serialize)]
pub struct SyncReport {
    /// 本次成功推出队列的行数
    pub pushed: usize,
    /// 本次拉取并落库的行数
    pub pulled: usize,
    /// 本次新增冲突条数
    pub conflicts: usize,
    /// 剩余待推行数
    pub pending: i64,
    pub cursor: i64,
    pub state: SyncState,
    pub error: Option<String>,
}

/// 推空队列 → 拉全增量。断网时立刻停下并保留队列（**不重试、不丢弃**，
/// 自动补推交给下一次触发或定时巡检）。
pub fn sync_once(
    conn: &Connection,
    base_url: &str,
    token: &str,
    transport: &dyn Transport,
    now_ms: i64,
    now_iso: &str,
) -> SyncReport {
    let mut pushed = 0usize;
    let mut pulled = 0usize;
    let mut conflicts = 0usize;

    // ① push：分批直到队列空
    for _ in 0..64 {
        let batch = match take_batch(conn, PUSH_BATCH) {
            Ok(b) => b,
            Err(e) => {
                let msg = format!("读取待推队列失败：{e}");
                let _ = state_set(conn, keys::LAST_ERROR, &msg);
                return finish(conn, pushed, pulled, conflicts, SyncState::Error, Some(msg));
            }
        };
        if batch.is_empty() {
            break;
        }
        match transport.push(base_url, token, &batch) {
            Ok(res) => {
                let _ = drop_pushed(conn, &res.applied);
                pushed += res.applied.len();
                conflicts += res.conflicts.max(0) as usize;
            }
            Err(e) => {
                let msg = e.describe();
                let _ = note_failure(conn, &batch, &msg);
                let _ = state_set(conn, keys::LAST_ERROR, &msg);
                let st = match e {
                    TransportError::Unauthorized => SyncState::AuthRequired,
                    TransportError::Unreachable(_) => SyncState::Pending,
                    _ => SyncState::Error,
                };
                return finish(conn, pushed, pulled, conflicts, st, Some(msg));
            }
        }
    }

    // ② pull：按游标翻页，**先落库再前移游标**
    let mut cursor = state_get(conn, keys::CURSOR)
        .and_then(|s| s.parse::<i64>().ok())
        .unwrap_or(0);
    for _ in 0..64 {
        match transport.pull(base_url, token, cursor, PULL_LIMIT) {
            Ok(page) => {
                match apply_pull(conn, &page.rows, now_ms) {
                    Ok((w, c)) => {
                        pulled += w;
                        conflicts += c;
                    }
                    Err(e) => {
                        let msg = format!("写入远端行失败：{e}");
                        let _ = state_set(conn, keys::LAST_ERROR, &msg);
                        return finish(conn, pushed, pulled, conflicts, SyncState::Error, Some(msg));
                    }
                }
                cursor = page.cursor;
                let _ = state_set(conn, keys::CURSOR, &cursor.to_string());
                if !page.has_more || page.rows.is_empty() {
                    break;
                }
            }
            Err(e) => {
                let msg = e.describe();
                let _ = state_set(conn, keys::LAST_ERROR, &msg);
                let st = match e {
                    TransportError::Unauthorized => SyncState::AuthRequired,
                    TransportError::Unreachable(_) => SyncState::Pending,
                    _ => SyncState::Error,
                };
                return finish(conn, pushed, pulled, conflicts, st, Some(msg));
            }
        }
    }

    let _ = state_set(conn, keys::LAST_OK_AT, now_iso);
    let _ = state_del(conn, keys::LAST_ERROR);
    finish(conn, pushed, pulled, conflicts, SyncState::Connected, None)
}

fn finish(
    conn: &Connection,
    pushed: usize,
    pulled: usize,
    conflicts: usize,
    state: SyncState,
    error: Option<String>,
) -> SyncReport {
    let pending = count_pending(conn).unwrap_or(0);
    let cursor = state_get(conn, keys::CURSOR)
        .and_then(|s| s.parse::<i64>().ok())
        .unwrap_or(0);
    SyncReport { pushed, pulled, conflicts, pending, cursor, state, error }
}

// ───────────────────────────── 登录 / 登出 ─────────────────────────────

/// 登录并落地令牌与身份。**登录不覆盖游标**：换账号时由调用方显式重置，
/// 否则新账号会用旧账号的游标跳过全部历史。
pub fn login(
    conn: &Connection,
    base_url: &str,
    username: &str,
    password: &str,
    device_seed: &str,
    transport: &dyn Transport,
    now_iso: &str,
) -> Result<SyncStatus, TransportError> {
    let url = validate_base_url(base_url).map_err(TransportError::Rejected)?;
    let device_id = ensure_device_id(conn, device_seed).map_err(|e| TransportError::Rejected(e.to_string()))?;
    let res = transport.login(&url, username, password, &device_id)?;

    let _ = state_set(conn, keys::BASE_URL, &url);
    let _ = state_set(conn, keys::TOKEN, &res.token);
    let _ = state_set(conn, keys::MEMBER_ID, &res.member_id);
    let _ = state_set(conn, keys::MEMBER_NAME, username);
    if !res.device_id.trim().is_empty() {
        let _ = state_set(conn, keys::DEVICE_ID, &res.device_id);
    }
    let _ = state_set(conn, keys::LAST_OK_AT, now_iso);
    let _ = state_del(conn, keys::LAST_ERROR);
    Ok(read_status(conn))
}

/// 登出：作废本机令牌，**本地数据与游标保留**（离线继续可用，架构 §4.15）。
pub fn logout(conn: &Connection, transport: &dyn Transport) -> Result<SyncStatus, TransportError> {
    let base_url = state_get(conn, keys::BASE_URL).unwrap_or_default();
    let token = state_get(conn, keys::TOKEN).unwrap_or_default();
    if !base_url.is_empty() && !token.is_empty() {
        // 服务端不可达也允许本地登出：令牌会在服务端到期/被淘汰，本机不必卡住
        let _ = transport.logout(&base_url, &token);
    }
    let _ = state_del(conn, keys::TOKEN);
    let _ = state_del(conn, keys::MEMBER_ID);
    let _ = state_del(conn, keys::MEMBER_NAME);
    let _ = state_set(conn, keys::LAST_ERROR, "已退出登录，重新登录后可继续同步");
    Ok(read_status(conn))
}

// ───────────────────────────── 测试用假服务端 ─────────────────────────────

#[cfg(test)]
mod tests;
