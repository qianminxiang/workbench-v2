//! v2.3.0 招标情报 → SQLite 落库（验收1）与线索闭环（验收4）。
//!
//! 链路：`fetch_site` → [`persist_fetch`]（三层去重 → `ext_record` 拉链 + `intel_lead` 线索池）
//! → [`confirm_lead`] / [`reject_lead`]（用户裁决）→ [`generate_todo`]（确认后转待办）。
//!
//! 设计约束（对应任务书闸门条件）：
//! - **线索池零暗重复**：`intel_lead.dedup_key` 与 `ext_record.biz_key` 同口径，
//!   均来自 [`crate::lead_dedup::dedup_batch`] 的三层判定（L1 原生 ID → L3 项目编号 → L2 指纹）。
//!   落库前先读全量 current 键入池，同批次内重复也在 `dedup_batch` 内被拦住。
//! - **辅源不产待办**：`origin = auxiliary` 的线索 `needs_primary_confirm = 1`，
//!   [`generate_todo`] 直接拒绝（`TodoReject`），必须等主源落地或用户手动确认。
//! - **人工改判不被覆盖**（AC-2-7）：再次抓到同 `dedup_key` 时，
//!   只更新拉链（内容变化 → 封旧开新），`intel_lead` 的 state / todo 不动。
//! - **验收1 的证据形态**：落库后必须 [`read_current`] 从 SQLite 回读，
//!   样本级内存比对不算通过——本模块的读函数就是为此存在。

use std::collections::BTreeMap;

use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};

use crate::intel::IntelItem;
use crate::lead_dedup::{dedup_batch, DedupAction, DedupInput, DedupKey, DedupLayer};
use crate::intel_stage::{announcement_signature, stage_from_text, Stage, StageSource};
use crate::lead_todo::{self, Lead, LeadState, LeadTodo, Origin, TodoReject};
use crate::store::{now_iso_pub, Store, StoreError};

/// 单源一轮落库的报告（fetch_run 留档 + 界面展示）。
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct PersistReport {
    pub source_id: String,
    pub fetched: usize,
    /// ext_record 新链（新 dedup_key）
    pub inserted: usize,
    /// 判定并入已有记录（L1/L3/L2 命中，链接维度回填，不建新链）
    pub merged: usize,
    /// 同 biz_key 内容变化 → 拉链封旧开新
    pub superseded: usize,
    /// 同 biz_key 同 content_hash（幂等重抓）
    pub unchanged: usize,
    /// 新进线索池（两级命中 Title / BodyVerified 且主源）
    pub pool_new: usize,
    /// 命中但为辅源/疑似不相关，未进线索池
    pub pool_skipped: usize,
}

impl PersistReport {
    /// 暗重复守卫的口径：inserted 与 pool_new 必须一致地「每 key 至多一条」。
    pub fn new_links(&self) -> usize {
        self.inserted
    }
}

/// 计算 IntelItem 的内容哈希（拉链变更判定）。
/// 字段选取原则：**只选源站会真实变化的字段**，排除 first_seen（每次抓取都变）
/// 与 manual_verdict（人工态，不属于源内容）。
fn content_hash_of(it: &IntelItem) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    for part in [
        it.title.as_str(),
        it.org.as_str(),
        it.area.as_str(),
        it.date.as_str(),
        it.amount.as_deref().unwrap_or(""),
        it.url.as_str(),
        it.body_head.as_str(),
    ] {
        h.update(part.as_bytes());
        h.update([0x1f]);
    }
    format!("{:x}", h.finalize())
}

/// 从拉链里恢复 dedup 层（key 前缀还原；native 层不落 ext_record，见下方 L1 说明）。
fn layer_of_key(k: &str) -> DedupLayer {
    if k.starts_with("code:") {
        DedupLayer::ProjectCode
    } else {
        DedupLayer::Fingerprint
    }
}

/// 把一轮抓取结果落库（拉链 + 线索池），单事务提交。
///
/// `ts` 为采集时刻（ISO）；传 `None` 用系统时钟。
pub fn persist_fetch(
    store: &Store,
    items: &[IntelItem],
    ts: Option<&str>,
) -> Result<PersistReport, StoreError> {
    let ts = ts.map(|s| s.to_string()).unwrap_or_else(now_iso_pub);
    let mut rep = PersistReport {
        source_id: items.first().map(|i| i.site_id.clone()).unwrap_or_default(),
        fetched: items.len(),
        ..Default::default()
    };
    if items.is_empty() {
        return Ok(rep);
    }
    let guard = store.begin_write("intel-fetch")?;
    let conn = guard.conn();
    let tx = conn.unchecked_transaction()?;

    // 1) 现有池：全部 current biz_key（跨源，L3/L2 是跨源判定）
    //
    // v3.2.0：biz_key 的粒度从「项目」细化为「公告节点」（`code:<编号>#<阶段>|<日期>`），
    // 所以这里**同时登记项目键与节点键**：项目键让 L3 判据照旧能命中（同一项目跨源、
    // 跨阶段都先归到同一个项目），节点键让 L2/L1 的既有语义不变。
    let mut existing: BTreeMap<String, DedupKey> = BTreeMap::new();
    {
        let mut stmt = tx.prepare("SELECT biz_key FROM ext_record WHERE is_current = 1")?;
        let keys: Vec<String> = stmt
            .query_map([], |r| r.get::<_, String>(0))?
            .collect::<Result<_, _>>()?;
        for k in keys {
            let (proj, _sig) = split_project_key(&k);
            if !proj.is_empty() && !existing.contains_key(proj) {
                existing.insert(
                    proj.to_string(),
                    DedupKey { key: proj.to_string(), layer: DedupLayer::ProjectCode },
                );
            }
            let layer = layer_of_key(&k);
            existing.insert(k.clone(), DedupKey { key: k, layer });
        }
    }

    // 2) 三层去重（同批次内的重复也被拦：dedup_batch 内部维护 known）
    let inputs: Vec<DedupInput> = items
        .iter()
        .map(|it| DedupInput {
            site_id: it.site_id.clone(),
            native_id: it.key.clone(),
            title: it.title.clone(),
            // 分隔符理由同 lead_dedup::dedup_batch：避免正文编号吞掉标题开头 ASCII
            body: format!("{}\n{}", it.body_head, it.title),
            date: it.date.clone(),
            // 接口权威编号优先于正文抽取（见 DedupInput::native_code 实测说明）
            native_code: it.native_code.clone(),
        })
        .collect();
    let decisions = dedup_batch(&existing, inputs.iter());

    for (d, it) in decisions.iter().zip(items.iter()) {
        let hash = content_hash_of(it);
        // dedup 判据给出的「项目键」：Insert 时是新建键，Merge 时是被并入的项目键。
        let project_key = match &d.action {
            DedupAction::Insert { key } => key.key.clone(),
            DedupAction::Merge { with, .. } => with.clone(),
        };
        let proj = split_project_key(&project_key).0.to_string();

        // 阶段判据：A 路线（文本抽取）在落库时定；判不出就如实落 unknown，
        // B 路线（启发式）留到查询期做——它需要「项目内已有多条」这个上下文，
        // 且不能让阶段依赖抓取批次顺序（见 intel_stage 模块文档）。
        let (stage, source, reason) = match stage_from_text(&it.title, &it.body_head) {
            Some((st, w)) => (st, StageSource::Rule, format!("标题/正文命中「{}」", w)),
            None => (Stage::Unknown, StageSource::Unknown, String::new()),
        };
        let sig = announcement_signature(stage, &it.date);
        let node_key = node_key_of(&project_key, &sig);

        // 节点定位：**同一公告**（同项目同阶段同日期）才合并；同项目的其它公告各成一行。
        let prior: Option<(String, String, String)> = {
            let mut stmt = tx.prepare(
                "SELECT id, content_hash, record_json FROM ext_record
                  WHERE biz_key = ?1 AND is_current = 1 LIMIT 1",
            )?;
            let mut rows = stmt.query(params![node_key])?;
            match rows.next()? {
                Some(r) => Some((r.get(0)?, r.get(1)?, r.get(2)?)),
                None => None,
            }
        };

        match prior {
            Some((old_id, old_hash, old_json)) => {
                let prev: IntelItem = serde_json::from_str(&old_json)
                    .map_err(|e| StoreError::Sqlite(e.to_string()))?;
                if prev.site_id == it.site_id && prev.url == it.url {
                    // 同一条公告被重抓：内容没变即幂等，变了封旧开新
                    if old_hash == hash {
                        rep.unchanged += 1;
                    } else {
                        tx.execute(
                            "UPDATE ext_record SET valid_to = ?1, is_current = 0 WHERE id = ?2",
                            params![ts, old_id],
                        )?;
                        insert_chain(&tx, it, &node_key, &hash, &ts, stage, source, &reason, &sig)?;
                        rep.superseded += 1;
                    }
                } else {
                    // 跨源副本（同项目同阶段同日期、不同链接）：链接维度回填，不建新节点
                    rep.merged += 1;
                    append_alias_url(&tx, &old_id, &it.url, &it.site_id)?;
                }
            }
            None => {
                insert_chain(&tx, it, &node_key, &hash, &ts, stage, source, &reason, &sig)?;
                rep.inserted += 1;
                // 线索池仍按**项目**一条（dedup_key = 项目键）：
                // 同一项目的后续阶段公告不重复造线索，但其节点已落 ext_record 供时间轴使用。
                if it.relevance == crate::intel::Relevance::Core && enters_pool_by_reasons(it) {
                    let origin = origin_of_site(&it.site_id);
                    let n = upsert_lead(&tx, it, &proj, &d, origin, &ts)?;
                    if n > 0 {
                        rep.pool_new += 1;
                    } else {
                        rep.pool_skipped += 1;
                    }
                } else {
                    rep.pool_skipped += 1;
                }
            }
        }
    }

    tx.commit()?;
    Ok(rep)
}

/// 两级命中判据：reasons 里带两级命中的标记（Title / BodyVerified）。
/// intel_source::apply_two_stage 会写「位置:标题…」/「正文复核…」依据；
/// Epoint 站标题式命中走 classify，reasons 含「命中主题词」。
fn enters_pool_by_reasons(it: &IntelItem) -> bool {
    // BodyWeak / None 在 apply_two_stage 已降为 Suspect/Excluded，不会再是 Core；
    // 这里再核一遍依据链，防止未来改动破坏口径（宁缺勿滥：无依据不进池）。
    it.reasons.iter().any(|r| {
        r.contains("命中主题词") || r.contains("位置:标题") || r.contains("正文复核")
    })
}

/// 源性质：16 地市分站 / 省网 / 企业官网 = Primary；聚合站（乙方宝类）= Auxiliary。
/// 口径与 dedup_rule.md 的主源/辅源边界表一致。
pub fn origin_of_site(site_id: &str) -> Origin {
    match crate::intel_source::kind_of(site_id) {
        crate::intel_source::SourceKind::AggregatorHtml => Origin::Auxiliary,
        _ => Origin::Primary,
    }
}

#[allow(clippy::too_many_arguments)]
fn insert_chain(
    tx: &Connection,
    it: &IntelItem,
    biz_key: &str,
    hash: &str,
    ts: &str,
    stage: Stage,
    source: StageSource,
    reason: &str,
    sig: &str,
) -> Result<String, StoreError> {
    let row_id = format!("{}|{}|{}", it.site_id, biz_key, &hash[..12]);
    let json = serde_json::to_string(it).map_err(|e| StoreError::Sqlite(e.to_string()))?;
    tx.execute(
        "INSERT OR REPLACE INTO ext_record
           (id, source_id, biz_key, record_json, valid_from, valid_to, is_current, content_hash,
            stage, stage_source, stage_reason, ann_sig)
         VALUES (?1, ?2, ?3, ?4, ?5, NULL, 1, ?6, ?7, ?8, ?9, ?10)",
        params![
            row_id, it.site_id, biz_key, json, ts, hash,
            stage.code(), source.code(), reason, sig
        ],
    )?;
    Ok(row_id)
}

/// 项目键 ↔ 节点键的拆分。
///
/// 节点键格式：`<项目键>#<阶段码>|<日期>`，例如 `code:tltrzb2026074#review|2026-10-20`。
/// 没有 `#` 的是老库（v3.1.0 及以前）的项目级键——原样当作项目键，签名取空串，
/// 这样旧库数据读出来仍然可用（不要求存量回填即可上线）。
pub fn split_project_key(k: &str) -> (&str, &str) {
    match k.find('#') {
        Some(i) => (&k[..i], &k[i + 1..]),
        None => (k, ""),
    }
}

/// 由项目键与公告签名构造节点键。项目键为空时退化为纯签名节点键（无编号的公告）。
fn node_key_of(project_key: &str, sig: &str) -> String {
    let proj = split_project_key(project_key).0;
    if proj.is_empty() {
        return project_key.to_string();
    }
    format!("{}#{}", proj, sig)
}

/// 链接维度回填：把并入方的 URL 追加进目标链的 alias_urls（幂等）。
fn append_alias_url(tx: &Connection, row_id: &str, url: &str, site_id: &str) -> Result<(), StoreError> {
    let json: String = {
        let mut stmt = tx.prepare("SELECT record_json FROM ext_record WHERE id = ?1")?;
        let mut rows = stmt.query(params![row_id])?;
        match rows.next()? {
            Some(r) => r.get(0)?,
            None => return Ok(()), // 目标不存在（理论不可达）：跳过，不阻断
        }
    };
    let mut v: serde_json::Value = serde_json::from_str(&json).map_err(|e| StoreError::Sqlite(e.to_string()))?;
    let entry = format!("{}|{}", site_id, url);
    let aliases = v
        .as_object_mut()
        .and_then(|o| o.get_mut("alias_urls").and_then(|a| a.as_array_mut().cloned()));
    match aliases {
        Some(mut arr) => {
            let exists = arr
                .iter()
                .any(|x| x.as_str() == Some(entry.as_str()));
            if exists {
                return Ok(());
            }
            arr.push(serde_json::Value::String(entry));
            if let Some(o) = v.as_object_mut() {
                o.insert("alias_urls".into(), serde_json::Value::Array(arr));
            }
        }
        None => {
            if let Some(o) = v.as_object_mut() {
                o.insert(
                    "alias_urls".into(),
                    serde_json::Value::Array(vec![serde_json::Value::String(entry)]),
                );
            }
        }
    }
    let updated = serde_json::to_string(&v).map_err(|e| StoreError::Sqlite(e.to_string()))?;
    tx.execute("UPDATE ext_record SET record_json = ?1 WHERE id = ?2", params![updated, row_id])?;
    Ok(())
}

fn upsert_lead(
    tx: &Connection,
    it: &IntelItem,
    dedup_key: &str,
    d: &crate::lead_dedup::DedupDecision,
    origin: Origin,
    ts: &str,
) -> Result<usize, StoreError> {
    let layer = match &d.action {
        DedupAction::Insert { key } => key.layer.code(),
        DedupAction::Merge { layer, .. } => layer.code(),
    };
    let needs_primary = matches!(origin, Origin::Auxiliary);
    // INSERT OR IGNORE：人工已裁决（confirmed/rejected/todoed）的线索，
    // 再次抓到同 key 不覆盖生命周期字段（AC-2-7 在线索维度的对应约束）。
    // 返回值：影响行数（0 = 已存在被 IGNORE，调用方据此区分「新进池」与「已在池」）。
    let n = tx.execute(
        "INSERT OR IGNORE INTO intel_lead
           (dedup_key, dedup_layer, origin, title, org, area, date, url, amount,
            state, needs_primary_confirm, first_seen, state_changed_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, 'discovered', ?10, ?11, NULL)",
        params![
            dedup_key,
            layer,
            serde_json::to_string(&origin).unwrap_or_else(|_| "\"primary\"".into()).trim_matches('"'),
            it.title,
            it.org,
            it.area,
            it.date,
            it.url,
            it.amount,
            needs_primary as i64,
            ts,
        ],
    )?;
    Ok(n)
}

// ───────────────────────────── 回读（验收1 证据） ─────────────────────────────

/// 从 SQLite 回读某源的 current 记录（按 valid_from 倒序，取前 limit 条）。
/// 这是验收 1 的证据形态：**从库里读回来**，不是样本内存比对。
pub fn read_current(store: &Store, source_id: &str, limit: usize) -> Result<Vec<IntelItem>, StoreError> {
    let conn = store.conn();
    let mut stmt = conn.prepare(
        "SELECT record_json FROM ext_record
          WHERE source_id = ?1 AND is_current = 1
          ORDER BY valid_from DESC LIMIT ?2",
    )?;
    let rows: Vec<String> = stmt
        .query_map(params![source_id, limit as i64], |r| r.get(0))?
        .collect::<Result<_, _>>()?;
    rows.iter()
        .map(|j| serde_json::from_str(j).map_err(|e| StoreError::Sqlite(e.to_string())))
        .collect()
}

/// 回读拉链历史（封旧链也有证据：何时失效、为何失效）。
pub fn read_chain_history(
    store: &Store,
    source_id: &str,
    biz_key: &str,
) -> Result<Vec<(String, String, bool)>, StoreError> {
    let conn = store.conn();
    let mut stmt = conn.prepare(
        "SELECT valid_from, valid_to, is_current FROM ext_record
          WHERE source_id = ?1 AND biz_key = ?2 ORDER BY valid_from",
    )?;
    let rows = stmt
        .query_map(params![source_id, biz_key], |r| {
            Ok((
                r.get::<_, Option<String>>(0)?.unwrap_or_default(),
                r.get::<_, Option<String>>(1)?.unwrap_or_default(),
                r.get::<_, i64>(2)? == 1,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// 回读线索池（可按状态过滤；state 为 None 取全部）。
pub fn read_leads(store: &Store, state: Option<&LeadState>) -> Result<Vec<Lead>, StoreError> {
    let conn = store.conn();
    let sql = match state {
        Some(_) => "SELECT dedup_key, dedup_layer, origin, title, org, area, date, url, amount, state, needs_primary_confirm, first_seen FROM intel_lead WHERE state = ?1 ORDER BY first_seen",
        None => "SELECT dedup_key, dedup_layer, origin, title, org, area, date, url, amount, state, needs_primary_confirm, first_seen FROM intel_lead ORDER BY first_seen",
    };
    let state_str = state.map(|s| lead_state_str(s));
    let mut stmt = conn.prepare(sql)?;
    let mapper = |r: &rusqlite::Row| -> rusqlite::Result<Lead> {
        let origin_str: String = r.get(2)?;
        let state_s: String = r.get(9)?;
        Ok(Lead {
            dedup_key: r.get(0)?,
            dedup_layer: r.get(1)?,
            origin: serde_json::from_str(&format!("\"{}\"", origin_str)).unwrap_or(Origin::Primary),
            title: r.get(3)?,
            org: r.get(4)?,
            area: r.get(5)?,
            date: r.get(6)?,
            url: r.get(7)?,
            amount: r.get(8)?,
            state: parse_lead_state(&state_s),
            needs_primary_confirm: r.get::<_, i64>(10)? == 1,
            first_seen: r.get(11)?,
        })
    };
    let rows = match state_str.as_deref() {
        Some(s) => stmt.query_map(params![s], mapper)?.collect::<Result<Vec<_>, _>>()?,
        None => stmt.query_map([], mapper)?.collect::<Result<Vec<_>, _>>()?,
    };
    Ok(rows)
}

fn lead_state_str(s: &LeadState) -> &'static str {
    match s {
        LeadState::Discovered => "discovered",
        LeadState::Confirmed => "confirmed",
        LeadState::Rejected => "rejected",
        LeadState::Todoed => "todoed",
    }
}

fn parse_lead_state(s: &str) -> LeadState {
    match s {
        "confirmed" => LeadState::Confirmed,
        "rejected" => LeadState::Rejected,
        "todoed" => LeadState::Todoed,
        _ => LeadState::Discovered,
    }
}

// ───────────────────────────── 线索闭环（验收4） ─────────────────────────────

/// 用户确认线索（Discovered → Confirmed）。已裁决的线索不可再改（要改走 reopen，本期不做）。
pub fn confirm_lead(store: &Store, dedup_key: &str, ts: Option<&str>) -> Result<bool, StoreError> {
    transition_lead(store, dedup_key, LeadState::Confirmed, ts)
}

/// 用户否决线索（Discovered → Rejected）。未确认不进待办的另一半：确认的对立面。
pub fn reject_lead(store: &Store, dedup_key: &str, ts: Option<&str>) -> Result<bool, StoreError> {
    transition_lead(store, dedup_key, LeadState::Rejected, ts)
}

fn transition_lead(
    store: &Store,
    dedup_key: &str,
    to: LeadState,
    ts: Option<&str>,
) -> Result<bool, StoreError> {
    let ts = ts.map(|s| s.to_string()).unwrap_or_else(now_iso_pub);
    let guard = store.begin_write("lead-transition")?;
    let conn = guard.conn();
    // 只允许 discovered → confirmed/rejected（幂等：已是目标态返回 true）
    let n = conn.execute(
        "UPDATE intel_lead SET state = ?1, state_changed_at = ?2
          WHERE dedup_key = ?3 AND state = 'discovered'",
        params![lead_state_str(&to), ts, dedup_key],
    )?;
    if n > 0 {
        return Ok(true);
    }
    // 幂等检查：已是目标态？
    let cur: Option<String> = conn
        .query_row(
            "SELECT state FROM intel_lead WHERE dedup_key = ?1",
            params![dedup_key],
            |r| r.get(0),
        )
        .map(Some)
        .or_else(|e| match e {
            rusqlite::Error::QueryReturnedNoRows => Ok(None),
            other => Err(other),
        })?;
    match cur.as_deref() {
        Some(s) if s == lead_state_str(&to) => Ok(true),
        _ => Ok(false),
    }
}

/// 确认后转待办（验收4 的落地动作）。
/// - 未确认（discovered）→ 拒绝：`TodoReject::NotConfirmed`（「未确认不进待办」的闸门）
/// - 辅源未获主源落地 → 拒绝：`TodoReject::AuxiliaryOrigin`
/// - todo_id = `lead:{dedup_key}`，天然幂等（同一线索不会生成第二条待办）
pub fn generate_todo(
    store: &Store,
    dedup_key: &str,
    today: &str,
    owner: &str,
) -> Result<Result<LeadTodo, TodoReject>, StoreError> {
    let ts = now_iso_pub();
    let guard = store.begin_write("lead-todo")?;
    let conn = guard.conn();

    let row: Option<Lead> = {
        let mut stmt = conn.prepare(
            "SELECT dedup_key, dedup_layer, origin, title, org, area, date, url, amount, state, needs_primary_confirm, first_seen FROM intel_lead WHERE dedup_key = ?1",
        )?;
        let mut rows = stmt.query(params![dedup_key])?;
        match rows.next()? {
            Some(r) => {
                let origin_str: String = r.get(2)?;
                let state_s: String = r.get(9)?;
                Some(Lead {
                    dedup_key: r.get(0)?,
                    dedup_layer: r.get(1)?,
                    origin: serde_json::from_str(&format!("\"{}\"", origin_str)).unwrap_or(Origin::Primary),
                    title: r.get(3)?,
                    org: r.get(4)?,
                    area: r.get(5)?,
                    date: r.get(6)?,
                    url: r.get(7)?,
                    amount: r.get(8)?,
                    state: parse_lead_state(&state_s),
                    needs_primary_confirm: r.get::<_, i64>(10)? == 1,
                    first_seen: r.get(11)?,
                })
            }
            None => None,
        }
    };
    let mut lead = match row {
        Some(l) => l,
        None => return Ok(Err(TodoReject::NotConfirmed(LeadState::Discovered))), // 线索不存在视同未确认
    };

    // lead_todo::to_todo 内部校验 state（必须 Confirmed/Todoed）与 origin（辅源拒绝）
    let outcome = lead_todo::to_todo(&lead, today, owner);
    if let Ok(todo) = &outcome {
        conn.execute(
            "UPDATE intel_lead SET state = 'todoed', state_changed_at = ?1, todo_id = ?2, todo_json = ?3
              WHERE dedup_key = ?4",
            params![
                ts,
                todo.todo_id,
                serde_json::to_string(todo).map_err(|e| StoreError::Sqlite(e.to_string()))?,
                dedup_key
            ],
        )?;
        lead.state = LeadState::Todoed;
        // v3.4.0 W5（架构 §4.1 缺陷补齐）：转待办成功后同步向 work_item 落一行，
        // 否则「已转待办」的线索在任务/待办版块里看不到。
        // 幂等：id = todo_id（lead:<dedup_key>），INSERT OR IGNORE 天然复用幂等键，
        // 重复确认不会产生第二条；不影响 generate_todo 既有的幂等契约。
        insert_todo_work_item(conn, todo, &ts)?;
    }
    Ok(outcome)
}

/// 把一条 `LeadTodo` 写成 `work_item` 行（架构 §4.1 补齐方案的逐字段口径）。
/// - `id = todo.todo_id`（幂等键）；`board_code = 'prj'`（v3.4 占位板块，§3.2）；
/// - `fiscal_year` 取 `due_date` 年份；`planned_end_date = due_date`（精度 day）；
/// - `status = '待启动'`（`ItemStatus::from_zh` 认的中文全量值；架构 §4.1 写的「未启」
///   是前端 chip 显示缩写，入库必须是「待启动」，否则读回 status 解析为 None）；`next_action` / `owner` 传入值；
/// - `seq_no` 取同 `fiscal_year` 内 `MAX(seq_no)+1`（沿用既有 UNIQUE 约束）；
/// - 来源徽标口径（§3.2）：ext_json 记 `v34_source = "intel"`，前端据此渲染「情报导出」。
pub(crate) fn insert_todo_work_item(
    conn: &Connection,
    todo: &LeadTodo,
    ts: &str,
) -> Result<(), StoreError> {
    let fiscal_year: i64 = todo
        .due_date
        .get(0..4)
        .and_then(|y| y.parse::<i64>().ok())
        .or_else(|| ts.get(0..4).and_then(|y| y.parse::<i64>().ok()))
        .unwrap_or(0);
    let seq_no: i64 = conn.query_row(
        "SELECT COALESCE(MAX(seq_no), 0) + 1 FROM work_item WHERE fiscal_year = ?1",
        params![fiscal_year],
        |r| r.get(0),
    )?;
    conn.execute(
        "INSERT OR IGNORE INTO work_item
           (id, seq_no, board_code, title, priority,
            planned_start_date, planned_start_precision,
            planned_end_date, planned_end_precision,
            progress_pct, status, milestone_stage, next_action, owner,
            source_sheet, fiscal_year, ext_json, created_at, updated_at)
         VALUES (?1, ?2, 'prj', ?3, NULL,
                 NULL, 'unknown',
                 ?4, 'day',
                 0, '待启动', NULL, ?5, ?6,
                 NULL, ?7, ?8, ?9, ?9)",
        params![
            todo.todo_id,
            seq_no,
            todo.title,
            todo.due_date,
            todo.next_action,
            todo.owner,
            fiscal_year,
            serde_json::json!({"v34_source": "intel", "lead_url": todo.source_url, "lead_key": todo.lead_key}).to_string(),
            ts
        ],
    )?;
    Ok(())
}

/// 历史待办回填报告（W5 一次性幂等任务，失败可重跑）。
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct BackfillReport {
    /// 扫描到的已转待办线索数（state='todoed' 且 todo_json 非空）
    pub scanned: usize,
    /// 本次新插入 work_item 的行数
    pub inserted: usize,
    /// 已存在（INSERT OR IGNORE 命中 id）而跳过的行数
    pub skipped: usize,
    /// todo_json 解析失败（脏数据不搬运，如实计数）
    pub invalid_json: usize,
}

/// 历史待办回填（W5，架构 §4.1：0007 只建表不搬数，回填走一次性幂等任务）。
///
/// 按 `todo_json` 重放：对每条 `state='todoed'` 且已有 `todo_json` 的线索，
/// 以 `todo_id` 为幂等键 INSERT OR IGNORE 到 `work_item`。
/// 已存在的行不改写（§五-2 「不改写存量行」）；失败可整批重跑。
pub fn backfill_todos(store: &Store) -> Result<BackfillReport, StoreError> {
    let guard = store.begin_write("todo-backfill")?;
    let conn = guard.conn();
    let ts = now_iso_pub();
    let mut stmt =
        conn.prepare("SELECT todo_id, todo_json FROM intel_lead WHERE state = 'todoed' AND todo_id IS NOT NULL AND todo_json IS NOT NULL")?;
    let rows: Vec<(String, String)> = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<Result<_, _>>()?;
    let mut report = BackfillReport { scanned: rows.len(), ..Default::default() };
    for (todo_id, todo_json) in rows {
        let todo: LeadTodo = match serde_json::from_str(&todo_json) {
            Ok(t) => t,
            Err(_) => {
                report.invalid_json += 1;
                continue;
            }
        };
        let before = conn.prepare("SELECT COUNT(*) FROM work_item WHERE id = ?1")?;
        let existed: i64 = before.query_row(params![todo_id], |r| r.get(0))?;
        drop(before);
        insert_todo_work_item(conn, &todo, &ts)?;
        if existed > 0 {
            report.skipped += 1;
        } else {
            report.inserted += 1;
        }
    }
    Ok(report)
}

/// 拉链 + 线索池的暗重复审计（dedup_rule.md 的 dedup_audit 判定）。
/// 返回 (current 记录数, current biz_key 去重后数, 线索池数)。
/// 三者应满足：current biz_key 去重后数 == current 记录数（拉链层无暗重复），
/// 线索池数 == 线索池 dedup_key 去重后数（池层无暗重复，PK 已保证）。
pub fn dedup_audit(store: &Store) -> Result<(usize, usize, usize, usize), StoreError> {
    let conn = store.conn();
    let recs: i64 = conn.query_row(
        "SELECT COUNT(*) FROM ext_record WHERE is_current = 1",
        [],
        |r| r.get(0),
    )?;
    let keys: i64 = conn.query_row(
        "SELECT COUNT(DISTINCT biz_key) FROM ext_record WHERE is_current = 1",
        [],
        |r| r.get(0),
    )?;
    let leads: i64 = conn.query_row("SELECT COUNT(*) FROM intel_lead", [], |r| r.get(0))?;
    let lead_keys: i64 = conn.query_row(
        "SELECT COUNT(DISTINCT dedup_key) FROM intel_lead",
        [],
        |r| r.get(0),
    )?;
    Ok((recs as usize, keys as usize, leads as usize, lead_keys as usize))
}

// ───────────────────── 项目聚合查询（v3.2.0 情报「进程合一」） ─────────────────────

/// 时间轴上的一个节点（= 一条公告）。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ProjectNode {
    /// 节点行 id（ext_record.id）
    pub record_id: String,
    pub title: String,
    pub date: String,
    /// 阶段短码 + 中文标签（界面直接用 label，不用再查枚举）
    pub stage: String,
    pub stage_label: String,
    /// 进程序号（`unknown` 为 None，界面据此判断是否画空节点 / 挂待确认）
    pub stage_order: Option<u8>,
    /// 判据来源：rule / heuristic / manual / unknown
    pub stage_source: String,
    /// 判据依据（命中词 / 兜底理由 / 人工改判说明）
    pub stage_reason: String,
    pub source_id: String,
    pub source_name: String,
    pub url: String,
    /// 该节点合并的其他源链接（跨源转载副本）
    pub alias_urls: Vec<String>,
    pub org: String,
    pub area: String,
    pub amount: Option<String>,
    /// 正文头部（供界面「展开看原文摘要」，不落全文）
    pub body_head: String,
    pub first_seen: String,
}

/// 一个项目的聚合视图。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ProjectAggregate {
    /// 项目键（`code:<归一编号>`；无编号时为 `fp:`/`body:` 键）
    pub project_key: String,
    pub dedup_layer: String,
    /// 项目编号原文（从任一条记录标题/正文抽到的归一值；无编号为空）
    pub project_code: String,
    /// 项目名（取最早节点的标题，去掉公告类型后缀；判不出时用最早标题原文）
    pub project_name: String,
    pub org: String,
    pub area: String,
    /// 当前阶段 = 时间轴上序号最大的已知阶段（无已知阶段时为「阶段未知」）
    pub current_stage: String,
    pub current_stage_label: String,
    pub current_stage_order: Option<u8>,
    /// 时间轴节点，按 (阶段序, 日期) 排序；`unknown` 节点排在末尾但不丢弃
    pub nodes: Vec<ProjectNode>,
    /// 最早 / 最近公告日期
    pub first_date: String,
    pub last_date: String,
    /// 节点数（含阶段未知节点）
    pub node_count: usize,
    /// 已知阶段覆盖到的最大序号；跳段计算用
    pub max_stage_order: Option<u8>,
    /// 按阶段序跳过的段数（如 0→1→3 跳过 1 段）；0 表示无跳段
    pub skipped_stages: usize,
}

/// 把标题里的公告类型后缀剥掉，得到项目名。
/// 只剥已知类型词（与 `intel_stage::RULES` 同源），不做通用的「去括号」猜测。
/// 对外暴露的项目名抽取。`payload` 适配层与界面文案共用同一份规则，
/// 避免两处各写一份而慢慢漂移（节点名与卡片名必须一致）。
pub fn project_name_of_pub(title: &str) -> String {
    project_name_of(title)
}

fn project_name_of(title: &str) -> String {
    let mut t = title.to_string();
    for suffix in [
        "中标（成交）结果公告", "中标(成交)结果公告", "中标结果公告", "成交结果公告",
        "中标公告", "成交公告", "中选公告", "中选结果", "合同公告", "合同公示",
        "采购意向公告", "意向公开", "意向公告", "需求公示",
        "履约验收公告", "验收公告", "验收结果公告", "项目验收公告",
        "运维服务公告", "运维公告",
        "招标公告", "采购公告", "竞争性磋商公告", "竞争性谈判公告", "询价公告",
        "单一来源公告", "资格预审公告", "更正公告", "变更公告", "澄清公告", "答疑公告",
        "磋商公告", "谈判公告",
    ] {
        if let Some(pos) = t.find(suffix) {
            // 只在后缀位于尾部（允许尾部有空白/标点）时才剥，避免剥掉项目名中间的字
            let tail: String = t[pos + suffix.len()..].to_string();
            if tail.trim().chars().all(|c| !c.is_alphanumeric()) {
                t = t[..pos].to_string();
                break;
            }
        }
    }
    let cleaned = t.trim().trim_end_matches(['-', '—', '·', ':', '：', '（', '(']).trim();
    if cleaned.is_empty() {
        title.trim().to_string()
    } else {
        cleaned.to_string()
    }
}

/// **按项目聚合查询**：一次返回同一项目所有阶段公告组成的时间轴。
///
/// 数据来源全部是 `ext_record` 的 current 行（拉链的当前层），不重新抓取、不读内存。
/// - 项目划分用 `dedup_key` 的项目键（`code:` / `fp:` / `body:` 前缀，来自 `lead_dedup`）；
/// - 节点顺序：已知阶段按 `Stage::order()`，未知阶段排在其后按日期；
/// - `project_code` 从项目键还原（`code:` 前缀去掉即为归一编号）。
pub fn read_project_timeline(
    store: &Store,
    project_key: &str,
) -> Result<Option<ProjectAggregate>, StoreError> {
    let conn = store.conn();
    // 项目键可能是「项目级键」（无 #），也可能是某个节点键；统一按项目前缀范围匹配。
    let proj = split_project_key(project_key).0.to_string();
    let mut stmt = conn.prepare(
        "SELECT id, biz_key, record_json, stage, stage_source, stage_reason, ann_sig, valid_from
           FROM ext_record
          WHERE is_current = 1 AND (biz_key = ?1 OR biz_key LIKE ?1 || '#%')
          ORDER BY valid_from ASC, id ASC",
    )?;
    let rows: Vec<(String, String, String, String, String, String, String, String)> = stmt
        .query_map(params![proj], |r| {
            Ok((
                r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?, r.get(7)?,
            ))
        })?
        .collect::<Result<_, _>>()?;
    if rows.is_empty() {
        return Ok(None);
    }

    let mut nodes: Vec<ProjectNode> = Vec::with_capacity(rows.len());
    for (id, _biz, json, stage_c, src_c, reason, _sig, _vf) in rows {
        let it: IntelItem = serde_json::from_str(&json).map_err(|e| StoreError::Sqlite(e.to_string()))?;
        let stage = Stage::from_code(&stage_c);
        let alias_urls = serde_json::from_str::<serde_json::Value>(&json)
            .ok()
            .and_then(|v| v.get("alias_urls").and_then(|a| a.as_array()).cloned())
            .map(|a| a.iter().filter_map(|x| x.as_str().map(|s| s.to_string())).collect())
            .unwrap_or_default();
        nodes.push(ProjectNode {
            record_id: id,
            title: it.title.clone(),
            date: it.date.clone(),
            stage: stage.code().to_string(),
            stage_label: stage.label().to_string(),
            stage_order: stage.order(),
            stage_source: src_c,
            stage_reason: reason,
            source_id: it.site_id.clone(),
            source_name: it.site_name.clone(),
            url: it.url.clone(),
            alias_urls,
            org: it.org.clone(),
            area: it.area.clone(),
            amount: it.amount.clone(),
            body_head: it.body_head.clone(),
            first_seen: it.first_seen.clone(),
        });
    }

    // 排序：已知阶段按进程序，再按日期；未知阶段（order=None）整段排最后，按日期。
    nodes.sort_by(|a, b| {
        match (a.stage_order, b.stage_order) {
            (Some(x), Some(y)) => x.cmp(&y).then_with(|| a.date.cmp(&b.date)),
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (None, None) => a.date.cmp(&b.date),
        }
        .then_with(|| a.date.cmp(&b.date))
        .then_with(|| a.record_id.cmp(&b.record_id))
    });

    let first_date = nodes.iter().map(|n| n.date.clone()).min().unwrap_or_default();
    let last_date = nodes.iter().map(|n| n.date.clone()).max().unwrap_or_default();
    let max_stage_order = nodes.iter().filter_map(|n| n.stage_order).max();
    // 跳段：从 0 到 max_stage_order 之间，有哪几段没有任何节点
    let skipped_stages = match max_stage_order {
        Some(mx) => {
            let present: std::collections::BTreeSet<u8> =
                nodes.iter().filter_map(|n| n.stage_order).collect();
            (0..=mx).filter(|o| !present.contains(o)).count()
        }
        None => 0,
    };
    let (cur_stage, cur_label, cur_order) = match max_stage_order {
        Some(mx) => {
            let st = nodes.iter().find(|n| n.stage_order == Some(mx)).map(|n| n.stage.clone()).unwrap_or_else(|| "unknown".to_string());
            let s = Stage::from_code(&st);
            (st.to_string(), s.label().to_string(), Some(mx))
        }
        None => ("unknown".to_string(), Stage::Unknown.label().to_string(), None),
    };

    let first_node = nodes.first().cloned();
    let (project_name, org, area, layer) = match first_node {
        Some(n) => (
            project_name_of(&n.title),
            n.org.clone(),
            n.area.clone(),
            if proj.starts_with("code:") { "L3".to_string() } else { "L2".to_string() },
        ),
        None => (String::new(), String::new(), String::new(), "L2".to_string()),
    };
    let project_code = proj.strip_prefix("code:").unwrap_or("").to_string();
    let node_count = nodes.len();

    Ok(Some(ProjectAggregate {
        project_key: proj,
        dedup_layer: layer,
        project_code,
        project_name,
        org,
        area,
        current_stage: cur_stage,
        current_stage_label: cur_label,
        current_stage_order: cur_order,
        nodes,
        first_date,
        last_date,
        node_count,
        max_stage_order,
        skipped_stages,
    }))
}

/// 列出全部项目（每个项目一条聚合视图），按最近公告日期倒序。
///
/// `stage` 过滤：只返回「当前阶段 = 指定阶段」的项目（界面按阶段筛选用）。
/// `limit = 0` 表示不限。
pub fn list_projects(
    store: &Store,
    stage: Option<Stage>,
    limit: usize,
) -> Result<Vec<ProjectAggregate>, StoreError> {
    let conn = store.conn();
    let mut stmt = conn.prepare(
        "SELECT DISTINCT biz_key FROM ext_record WHERE is_current = 1 ORDER BY biz_key",
    )?;
    let keys: Vec<String> = stmt
        .query_map([], |r| r.get::<_, String>(0))?
        .collect::<Result<_, _>>()?;
    let mut out: Vec<ProjectAggregate> = Vec::new();
    for k in keys {
        let proj = split_project_key(&k).0.to_string();
        // 同一个项目会被多个节点键重复列出：按项目键去重
        if out.iter().any(|p| p.project_key == proj) {
            continue;
        }
        if let Some(p) = read_project_timeline(store, &proj)? {
            if let Some(want) = stage {
                if p.current_stage != want.code() {
                    continue;
                }
            }
            out.push(p);
        }
    }
    out.sort_by(|a, b| b.last_date.cmp(&a.last_date).then_with(|| a.project_key.cmp(&b.project_key)));
    if limit > 0 && out.len() > limit {
        out.truncate(limit);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::intel::Relevance;

    fn tmp_store(tag: &str) -> Store {
        let dir = std::env::temp_dir().join(format!("wb-intel-store-{}-{}", tag, std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        Store::open(&dir).expect("open store")
    }

    fn item(key: &str, title: &str, url: &str, body: &str) -> IntelItem {
        IntelItem {
            key: key.into(),
            site_id: "pjwnc".into(),
            site_name: "普农集团".into(),
            title: title.into(),
            org: "安徽省普济圩现代农业集团".into(),
            area: "铜陵".into(),
            date: "2026-10-28".into(),
            amount: Some("22万元".into()),
            url: url.into(),
            relevance: Relevance::Core,
            reasons: vec!["位置:标题仅含主题词，正文复核".into(), "正文复核加分词:植保".into()],
            incomplete: false,
            manual_verdict: false,
            first_seen: "2026-11-06".into(),
            body_head: body.into(),
            native_code: None,
        }
    }

    /// 验收1 主证：落库 → 从 SQLite 回读，字段逐项一致。
    #[test]
    fn roundtrip_read_back_from_sqlite() {
        let store = tmp_store("roundtrip");
        let pjw = item(
            "pjwnc|http://www.pjwnc.com/content/detail/x.html",
            "综合农事服务中心无人机采购项目招标公告",
            "http://www.pjwnc.com/content/detail/x.html",
            "TLTRZB【2026】074 预算22万元，采购植保无人机2台，用于田间施肥撒播与病虫害防治。",
        );
        let rep = persist_fetch(&store, &[pjw.clone()], Some("2026-11-06T08:00:00Z")).unwrap();
        assert_eq!(rep.inserted, 1);
        assert_eq!(rep.pool_new, 1);

        let back = read_current(&store, "pjwnc", 10).unwrap();
        assert_eq!(back.len(), 1, "从 SQLite 回读必须拿到刚落的记录");
        assert_eq!(back[0].title, pjw.title);
        assert_eq!(back[0].url, pjw.url);
        assert_eq!(back[0].amount, pjw.amount);
        assert!(back[0].body_head.contains("TLTRZB"), "正文头部应随链落库: {}", back[0].body_head);
    }

    /// 闸门条件：同批重抓（幂等）零新增、零暗重复。
    #[test]
    fn refetch_is_idempotent_no_dark_duplicates() {
        let store = tmp_store("idempotent");
        let a = item("pjwnc|http://x/1", "综合农事服务中心无人机采购项目招标公告", "http://x/1", "编号TLTRZB2026074 预算22万");
        let rep1 = persist_fetch(&store, &[a], Some("2026-11-06T08:00:00Z")).unwrap();
        assert_eq!((rep1.inserted, rep1.pool_new), (1, 1));

        // 完全相同的一轮重抓：unchanged，不产生新链、不产生新线索
        let b = item("pjwnc|http://x/1", "综合农事服务中心无人机采购项目招标公告", "http://x/1", "编号TLTRZB2026074 预算22万");
        let rep2 = persist_fetch(&store, &[b], Some("2026-11-06T12:00:00Z")).unwrap();
        assert_eq!(rep2.inserted, 0, "同内容重抓不得新增链");
        assert_eq!(rep2.unchanged, 1);

        // 同项目编号的跨源副本（乙方宝转载）：Merge，不建新链
        let mut c = item("yibao|http://y/9", "【转载】综合农事服务中心无人机采购项目招标公告", "http://y/9", "TLTRZB【2026】074");
        c.site_id = "yibao".into();
        c.key = "yibao|http://y/9".into();
        let rep3 = persist_fetch(&store, &[c], Some("2026-11-06T13:00:00Z")).unwrap();
        assert_eq!(rep3.merged, 1, "同编号跨源副本应并入已有链，got inserted={}", rep3.inserted);
        assert_eq!(rep3.inserted, 0);

        let (recs, keys, leads, lead_keys) = dedup_audit(&store).unwrap();
        assert_eq!(recs, keys, "拉链 current 层 biz_key 无暗重复");
        assert_eq!(leads, lead_keys);
        assert_eq!(keys, 1, "全库只有一条链（1 原发 + 1 转载合并）: {}", keys);
    }

    /// 拉链语义：内容真实变化（预算改了）→ 封旧开新，历史链可回读。
    #[test]
    fn content_change_supersedes_and_keeps_history() {
        let store = tmp_store("chain");
        let v1 = item("pjwnc|http://x/1", "无人机采购项目招标公告", "http://x/1", "预算20万元");
        persist_fetch(&store, &[v1], Some("2026-11-01T08:00:00Z")).unwrap();
        // 同标题同 URL、正文预算变化：同一条链的内容更新
        let v2 = item("pjwnc|http://x/1", "无人机采购项目招标公告", "http://x/1", "预算22万元");
        let rep = persist_fetch(&store, &[v2], Some("2026-11-06T08:00:00Z")).unwrap();
        assert_eq!(rep.superseded, 1, "内容变化应封旧开新");
        assert_eq!(rep.inserted, 0);

        let biz_key = {
            let conn = store.conn();
            conn.query_row(
                "SELECT biz_key FROM ext_record WHERE source_id='pjwnc' AND is_current=1",
                [],
                |r| r.get::<_, String>(0),
            )
            .unwrap()
        };
        let hist = read_chain_history(&store, "pjwnc", &biz_key).unwrap();
        assert_eq!(hist.len(), 2, "拉链应保留两段历史");
        assert!(!hist[0].2, "旧链 is_current=0");
        assert!(hist[1].2, "新链 is_current=1");

        let cur = read_current(&store, "pjwnc", 5).unwrap();
        assert_eq!(cur.len(), 1);
        assert!(cur[0].body_head.contains("预算22万元"), "current 应是新版内容: {}", cur[0].body_head);
    }

    /// AC-2-7 线索维度：用户已裁决的线索，再次抓到同 key 不覆盖生命周期。
    #[test]
    fn confirmed_lead_survives_refetch() {
        let store = tmp_store("survive");
        let a = item("pjwnc|http://x/1", "无人机采购项目招标公告", "http://x/1", "预算20万元");
        persist_fetch(&store, &[a], Some("2026-11-06T08:00:00Z")).unwrap();
        let leads = read_leads(&store, None).unwrap();
        assert_eq!(leads.len(), 1);
        let key = leads[0].dedup_key.clone();

        assert!(confirm_lead(&store, &key, Some("2026-11-06T09:00:00Z")).unwrap());
        // 再抓一轮（内容微变走 superseded）
        let b = item("pjwnc|http://x/1", "无人机采购项目招标公告", "http://x/1", "预算21万元");
        persist_fetch(&store, &[b], Some("2026-11-06T10:00:00Z")).unwrap();
        let leads2 = read_leads(&store, Some(&LeadState::Confirmed)).unwrap();
        assert_eq!(leads2.len(), 1, "确认态不被重抓覆盖");
    }

    /// 验收4 闸门：未确认不进待办；确认后转待办幂等。
    #[test]
    fn todo_requires_confirmation_and_is_idempotent() {
        let store = tmp_store("todo");
        let a = item("pjwnc|http://x/1", "无人机采购项目招标公告", "http://x/1", "开标时间2026-11-13");
        persist_fetch(&store, &[a], Some("2026-11-06T08:00:00Z")).unwrap();
        let key = read_leads(&store, None).unwrap()[0].dedup_key.clone();

        // 未确认 → 拒绝
        let r = generate_todo(&store, &key, "2026-11-06", "大角子").unwrap();
        assert!(r.is_err(), "未确认的线索不得生成待办");

        // 确认 → 待办
        assert!(confirm_lead(&store, &key, None).unwrap());
        let t1 = generate_todo(&store, &key, "2026-11-06", "大角子").unwrap().unwrap();
        assert!(t1.todo_id.starts_with("lead:"), "todo_id 应以 lead: 前缀锚定 dedup_key");

        // 幂等：再次转待办得到同一 todo_id（不会生成第二条）
        let t2 = generate_todo(&store, &key, "2026-11-06", "大角子").unwrap().unwrap();
        assert_eq!(t1.todo_id, t2.todo_id);

        // 状态落到 todoed
        let todoed = read_leads(&store, Some(&LeadState::Todoed)).unwrap();
        assert_eq!(todoed.len(), 1);
        assert_eq!(todoed[0].dedup_key, key);
    }

    /// 辅源（聚合站）线索：可进池观察，但永不独立产待办。
    #[test]
    fn aggregator_lead_cannot_produce_todo() {
        let store = tmp_store("agg");
        let mut a = item("yibao|http://y/1", "某县植保无人机采购公告", "http://y/1", "预算10万");
        a.site_id = "yibao".into();
        a.key = "yibao|http://y/1".into();
        persist_fetch(&store, &[a], Some("2026-11-06T08:00:00Z")).unwrap();
        let leads = read_leads(&store, None).unwrap();
        assert_eq!(leads.len(), 1);
        assert!(leads[0].needs_primary_confirm, "聚合站线索应标记需主源确认");

        let key = leads[0].dedup_key.clone();
        assert!(confirm_lead(&store, &key, None).unwrap());
        let r = generate_todo(&store, &key, "2026-11-06", "大角子").unwrap();
        assert!(r.is_err(), "辅源线索即使确认也不得直接产待办（需主源落地）");
    }

    /// 疑似不相关（Suspect）不进线索池。
    #[test]
    fn suspect_items_do_not_enter_pool() {
        let store = tmp_store("suspect");
        let mut a = item("pjwnc|http://x/1", "消防无人机演练采购", "http://x/1", "消防演练用");
        a.relevance = Relevance::Suspect;
        a.reasons = vec!["命中主题词但缺加分词".into()];
        let rep = persist_fetch(&store, &[a], Some("2026-11-06T08:00:00Z")).unwrap();
        assert_eq!(rep.pool_new, 0);
        assert_eq!(rep.pool_skipped, 1);
        assert!(read_leads(&store, None).unwrap().is_empty());
    }
}
