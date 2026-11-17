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
    let mut existing: BTreeMap<String, DedupKey> = BTreeMap::new();
    {
        let mut stmt = tx.prepare("SELECT biz_key FROM ext_record WHERE is_current = 1")?;
        let keys: Vec<String> = stmt
            .query_map([], |r| r.get::<_, String>(0))?
            .collect::<Result<_, _>>()?;
        for k in keys {
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
            body: format!("{}{}", it.body_head, it.title),
            date: it.date.clone(),
        })
        .collect();
    let decisions = dedup_batch(&existing, inputs.iter());

    // 本批次新插入的链：biz_key → row_id（后续 Merge 目标可能在本批次内）
    let mut fresh: BTreeMap<String, String> = BTreeMap::new();

    for (d, it) in decisions.iter().zip(items.iter()) {
        let hash = content_hash_of(it);
        match &d.action {
            DedupAction::Insert { key } => {
                // 同 biz_key 已有链（历史轮次）→ 按 content_hash 分流：不变 / 封旧开新
                let prior: Option<(String, String)> = {
                    let mut stmt = tx.prepare(
                        "SELECT id, content_hash FROM ext_record WHERE source_id = ?1 AND biz_key = ?2 AND is_current = 1",
                    )?;
                    let mut rows = stmt.query(params![it.site_id, key.key])?;
                    match rows.next()? {
                        Some(r) => Some((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
                        None => None,
                    }
                };
                match prior {
                    Some((old_id, old_hash)) => {
                        if old_hash == hash {
                            rep.unchanged += 1;
                        } else {
                            // 拉链封旧开新：内容变了才动（manual_verdict / first_seen 不进哈希）
                            tx.execute(
                                "UPDATE ext_record SET valid_to = ?1, is_current = 0 WHERE id = ?2",
                                params![ts, old_id],
                            )?;
                            let row_id = insert_chain(&tx, it, &key.key, &hash, &ts)?;
                            fresh.insert(key.key.clone(), row_id);
                            rep.superseded += 1;
                        }
                    }
                    None => {
                        let row_id = insert_chain(&tx, it, &key.key, &hash, &ts)?;
                        fresh.insert(key.key.clone(), row_id);
                        rep.inserted += 1;
                    }
                }
                // 线索池：两级命中（Title/BodyVerified → relevance Core；疑似 → Suspect 不进池）
                // + 主源/企业官网才独立成线索；聚合站是辅助层。
                if it.relevance == crate::intel::Relevance::Core && enters_pool_by_reasons(it) {
                    let origin = origin_of_site(&it.site_id);
                    upsert_lead(&tx, it, &key.key, &d, origin, &ts)?;
                    rep.pool_new += 1;
                } else {
                    rep.pool_skipped += 1;
                }
            }
            DedupAction::Merge { with, .. } => {
                // 目标行（本批新插 or 库里已有）：(row_id, content_hash, record_json)
                let target: Option<(String, String, String)> = match fresh.get(with) {
                    Some(id) => {
                        let mut stmt = tx.prepare("SELECT content_hash, record_json FROM ext_record WHERE id = ?1")?;
                        let mut rows = stmt.query(params![id])?;
                        match rows.next()? {
                            Some(r) => Some((id.clone(), r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
                            None => None,
                        }
                    }
                    None => {
                        let mut stmt = tx.prepare(
                            "SELECT id, content_hash, record_json FROM ext_record WHERE biz_key = ?1 AND is_current = 1 LIMIT 1",
                        )?;
                        let mut rows = stmt.query(params![with])?;
                        match rows.next()? {
                            Some(r) => Some((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?)),
                            None => None,
                        }
                    }
                };
                match target {
                    Some((tid, thash, tjson)) => {
                        let titem: IntelItem = serde_json::from_str(&tjson)
                            .map_err(|e| StoreError::Sqlite(e.to_string()))?;
                        if titem.site_id == it.site_id && titem.url == it.url {
                            // 自身链重抓（dedup_batch 对库里已有 key 判 Merge）：按内容分流
                            if thash == hash {
                                rep.unchanged += 1;
                            } else {
                                tx.execute(
                                    "UPDATE ext_record SET valid_to = ?1, is_current = 0 WHERE id = ?2",
                                    params![ts, tid],
                                )?;
                                insert_chain(&tx, it, with, &hash, &ts)?;
                                rep.superseded += 1;
                            }
                        } else {
                            // 跨源副本：链接维度回填（幂等：已存在不重复加）
                            rep.merged += 1;
                            append_alias_url(&tx, &tid, &it.url, &it.site_id)?;
                        }
                    }
                    None => {
                        // 目标链不存在（L2 标题窗命中的 key 未落库过）：保守建新链
                        let row_id = insert_chain(&tx, it, with, &hash, &ts)?;
                        fresh.insert(with.clone(), row_id);
                        rep.inserted += 1;
                    }
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

fn insert_chain(
    tx: &Connection,
    it: &IntelItem,
    biz_key: &str,
    hash: &str,
    ts: &str,
) -> Result<String, StoreError> {
    let row_id = format!("{}|{}|{}", it.site_id, biz_key, &hash[..12]);
    let json = serde_json::to_string(it).map_err(|e| StoreError::Sqlite(e.to_string()))?;
    tx.execute(
        "INSERT OR REPLACE INTO ext_record
           (id, source_id, biz_key, record_json, valid_from, valid_to, is_current, content_hash)
         VALUES (?1, ?2, ?3, ?4, ?5, NULL, 1, ?6)",
        params![row_id, it.site_id, biz_key, json, ts, hash],
    )?;
    Ok(row_id)
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
) -> Result<(), StoreError> {
    let layer = match &d.action {
        DedupAction::Insert { key } => key.layer.code(),
        DedupAction::Merge { layer, .. } => layer.code(),
    };
    let needs_primary = matches!(origin, Origin::Auxiliary);
    // INSERT OR IGNORE：人工已裁决（confirmed/rejected/todoed）的线索，
    // 再次抓到同 key 不覆盖生命周期字段（AC-2-7 在线索维度的对应约束）。
    tx.execute(
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
    Ok(())
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
    }
    Ok(outcome)
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
