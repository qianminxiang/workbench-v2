//! 情报「手动建档」数据层（设计规格 v1.2 §2 末段 + A-7 建档面板/合并流程）。
//!
//! 两类项目来源并存：① 引擎聚合（公告驱动）；② 手动建档——个别项目网上无痕迹、
//! 尚无任何公告，由用户手工录入。本模块只做**数据层**（存储、查重、疑似关联、
//! 合并审计），不做面板与对照视图的视觉（那属界面层）。
//!
//! 规格 A-7 的五条硬要求，逐条对应到函数：
//! 1. 必填仅两项（项目名 + 采购人），可选地区/预算/备注/线索来源/预计采购方式
//!    → [`ManualDraft`] 的字段划分 + [`create_manual`] 的入参校验。
//! 2. **建档即查重**：输入的聚合键实时比对已有项目，命中给两个出口
//!    → [`check_duplicate`] 返回 [`DupCheck`]；「仍单独建档」由 [`create_manual`]
//!    的 `force` 参数承接（用户明示后才允许建出重复卡）。
//! 3. **疑似关联不自动合并**：引擎后续采到公告、聚合键命中某手动项目时只写提示
//!    → [`detect_suspected_links`] 只产 `pending` 行，从不推进合并。
//! 4. **用户确认合并**：采购人/项目名以公告为准，地区/预算等手动字段保留并标注
//!    「手动补充」，写审计 → [`merge_into_announcement`] / [`merge_manual_into_project`]。
//! 5. **忽略后不再重复提示**（留审计、可手动重开）→ [`ignore_link`] / [`reopen_link`]。
//!
//! ## 「强命中」的边界（规格 A-6 第 5 条：弱命中阈值留给引擎侧）
//!
//! 只对**强命中**出提示。本模块把强命中定义为三类，全部可解释、可逐条落到证据：
//! - `exact_key`：聚合键完全相同（采购人 + 项目名归一后全等）；
//! - `same_buyer_name_overlap`：同一采购人、项目名归一后一方包含另一方
//!   （真实公告标题常比用户手写的名称多一层限定语）；
//! - `same_buyer_code`：同一采购人，且手动档案里提到的项目编号命中公告编号。
//! 三类都要**同一采购人**为前提——规格 §2「不同采购人的同名项目不合并」是硬约束，
//! 疑似关联再宽也不能越过它。名称相似度（编辑距离/向量）属弱命中，本层不做。

use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};

use crate::intel_key::{aggregate_key, AggKey};
use crate::store::{now_iso_pub, Store, StoreError};

// ───────────────────────── 档案与草稿 ─────────────────────────

/// 必填项缺失时的拒绝原因（面板据此高亮哪个字段）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DraftReject {
    MissingProjectName,
    MissingBuyer,
}

impl DraftReject {
    pub fn reason(&self) -> &'static str {
        match self {
            DraftReject::MissingProjectName => "项目名必填",
            DraftReject::MissingBuyer => "采购人必填",
        }
    }
}

/// 建档草稿（必填 2 项 + 可选 5 项，规格 A-7）。
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ManualDraft {
    pub project_name: String,
    pub buyer: String,
    pub area: Option<String>,
    pub budget: Option<String>,
    pub note: Option<String>,
    pub lead_source: Option<String>,
    pub expect_method: Option<String>,
}

impl ManualDraft {
    /// 必填校验：只查规格点名的两项，不做「看起来像项目名」之类的猜测。
    pub fn validate(&self) -> Result<(), DraftReject> {
        if self.project_name.trim().is_empty() {
            return Err(DraftReject::MissingProjectName);
        }
        if self.buyer.trim().is_empty() {
            return Err(DraftReject::MissingBuyer);
        }
        Ok(())
    }
}

/// 一条手动档案（落库形态）。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ManualProject {
    pub manual_id: String,
    pub project_name: String,
    pub buyer: String,
    pub agg_key: String,
    /// 归一化剔除项（页面「合并依据」原文）
    pub agg_basis: String,
    pub area: Option<String>,
    pub budget: Option<String>,
    pub note: Option<String>,
    pub lead_source: Option<String>,
    pub expect_method: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    /// 已合并进的公告聚合项目键（未合并为 None）
    pub merged_into: Option<String>,
}

impl ManualProject {
    /// 该档案是否仍「无公告」——合并后才回到公告驱动的阶段判定（规格 §2）。
    pub fn is_unmerged(&self) -> bool {
        self.merged_into.is_none()
    }

    /// 已被合并后，哪些字段是「手动补充」（规格 A-7：地区/预算等保留并标注）。
    /// 只标注手动独有的可选字段；项目名与采购人合并后以公告为准，不在此列。
    pub fn manual_supplement_fields(&self) -> Vec<&'static str> {
        let mut out = Vec::new();
        if self.area.as_deref().map(|s| !s.trim().is_empty()).unwrap_or(false) {
            out.push("地区");
        }
        if self.budget.as_deref().map(|s| !s.trim().is_empty()).unwrap_or(false) {
            out.push("预算");
        }
        if self.note.as_deref().map(|s| !s.trim().is_empty()).unwrap_or(false) {
            out.push("备注");
        }
        if self.lead_source.as_deref().map(|s| !s.trim().is_empty()).unwrap_or(false) {
            out.push("线索来源");
        }
        if self.expect_method.as_deref().map(|s| !s.trim().is_empty()).unwrap_or(false) {
            out.push("预计采购方式");
        }
        out
    }
}

/// 合并结果：合并后项目名/采购人以公告为准，手动补充字段保留并标注。
///
/// 只做 `Serialize`：`manual_supplement` 是 `&'static str` 列表（字段名常量），
/// 反序列化无法给出 'static 生命周期，而这个结构只往界面/CLI 单向输出。
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct MergeOutcome {
    pub manual: ManualProject,
    pub target_project: String,
    /// 合并后的展示名与采购人（**以公告为准**，规格 A-7）
    pub effective_name: String,
    pub effective_buyer: String,
    /// 保留下来并需标注「手动补充」的字段名
    pub manual_supplement: Vec<&'static str>,
    /// 与公告冲突的手动字段说明（进对照历史）
    pub conflicts: Vec<String>,
}

fn write_audit(
    tx: &Connection,
    manual_id: &str,
    action: &str,
    target: &str,
    actor: &str,
    at: &str,
    detail: &str,
) -> Result<(), StoreError> {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(format!("{manual_id}|{action}|{target}|{at}").as_bytes());
    let audit_id = format!("au:{:x}", &h.finalize())[..19].to_string();
    tx.execute(
        "INSERT OR REPLACE INTO manual_merge_audit
           (audit_id, manual_id, action, target_project, actor, at, detail)
         VALUES (?1,?2,?3,?4,?5,?6,?7)",
        params![audit_id, manual_id, action, target, actor, at, detail],
    )?;
    Ok(())
}

fn set_decision(
    tx: &Connection,
    link_id: &str,
    decision: &str,
    at: &str,
) -> Result<(), StoreError> {
    tx.execute(
        "UPDATE manual_link SET decision = ?1, decided_at = ?2 WHERE link_id = ?3",
        params![decision, at, link_id],
    )?;
    Ok(())
}

// ───────────────────────── 查重（建档即查重） ─────────────────────────

/// 查重结果（规格 A-7「建档即查重」）。
#[derive(Debug, Clone, PartialEq)]
pub struct DupCheck {
    /// 新输入的聚合键与依据
    pub key: AggKey,
    /// 命中的已有手动档案（键完全相同）
    pub manual_hit: Option<ManualProject>,
    /// 命中的已有公告聚合项目（键完全相同；由调用方传入，见 [`check_duplicate`]）
    pub announcement_hit: Option<String>,
}

impl DupCheck {
    /// 是否命中已有项目。命中即面板要给出两个出口（打开该卡 / 仍单独建档）。
    pub fn is_dup(&self) -> bool {
        self.manual_hit.is_some() || self.announcement_hit.is_some()
    }

    /// 面板提示文案（规格 A-7 原文语气）。无命中返回 None。
    pub fn prompt(&self) -> Option<String> {
        if let Some(m) = &self.manual_hit {
            return Some(format!("已存在疑似同一项目：{}（手动建档）", m.project_name));
        }
        if let Some(k) = &self.announcement_hit {
            return Some(format!("已存在疑似同一项目：{k}（已有公告聚合）"));
        }
        None
    }
}

/// 查重：把输入算成聚合键，比对①已有手动档案、②已有公告聚合项目。
///
/// `announcement_keys` 由调用方从聚合查询结果里传入（形如 `(agg_key, 展示名)`）——
/// 本模块不自己去查公告库，避免把「聚合查询」和「手动建档」两层的依赖搅在一起。
pub fn check_duplicate(
    conn: &Connection,
    draft: &ManualDraft,
    announcement_keys: &[(String, String)],
) -> Result<DupCheck, StoreError> {
    let key = aggregate_key(&draft.buyer, &draft.project_name);
    let manual_hit = find_by_key(conn, &key.key)?;
    let announcement_hit = announcement_keys
        .iter()
        .find(|(k, _)| *k == key.key)
        .map(|(_, name)| name.clone())
        .or_else(|| {
            // 公告侧可能还没算出 `k:` 键（引擎聚合按编号分组），此时按
            // 「同采购人 + 同名归一」在展示名上兜一次，覆盖「面板刚打开、
            // 聚合键尚未回填」的过渡态。
            announcement_keys
                .iter()
                .find(|(_, name)| {
                    let (n, _) = crate::intel_key::normalize_name(name);
                    n == key.name_norm
                })
                .map(|(k, _)| k.clone())
        });
    Ok(DupCheck { key, manual_hit, announcement_hit })
}

fn find_by_key(conn: &Connection, agg_key: &str) -> Result<Option<ManualProject>, StoreError> {
    let mut stmt = conn.prepare(
        "SELECT manual_id, project_name, buyer, agg_key, agg_basis, area, budget, note,
                lead_source, expect_method, created_at, updated_at, merged_into
           FROM manual_project WHERE agg_key = ?1",
    )?;
    let mut rows = stmt.query(params![agg_key])?;
    match rows.next()? {
        Some(r) => Ok(Some(row_of(r)?)),
        None => Ok(None),
    }
}

fn row_of(r: &rusqlite::Row<'_>) -> rusqlite::Result<ManualProject> {
    Ok(ManualProject {
        manual_id: r.get(0)?,
        project_name: r.get(1)?,
        buyer: r.get(2)?,
        agg_key: r.get(3)?,
        agg_basis: r.get(4)?,
        area: r.get(5)?,
        budget: r.get(6)?,
        note: r.get(7)?,
        lead_source: r.get(8)?,
        expect_method: r.get(9)?,
        created_at: r.get(10)?,
        updated_at: r.get(11)?,
        merged_into: r.get(12)?,
    })
}

/// 建档结果：可能是新建，也可能是被查重拦下（用户未明示强行建档）。
#[derive(Debug, Clone, PartialEq)]
pub enum CreateOutcome {
    Created(ManualProject),
    /// 已有同键项目，`force = false` 时拒绝新建，并把命中的项目带回去供面板跳转
    Duplicate(DupCheck),
}

fn manual_id_of(key: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(key.as_bytes());
    format!("mp:{:x}", &h.finalize())[..19].to_string()
}

/// 建档。**建档即查重**：同键已存在且 `force = false` 时不新建（返回 [`CreateOutcome::Duplicate`]）。
///
/// `force = true` 只在用户明示「仍单独建档」后传入（规格 A-7 两个出口之一）。
pub fn create_manual(
    store: &Store,
    draft: &ManualDraft,
    force: bool,
    announcement_keys: &[(String, String)],
    ts: Option<&str>,
) -> Result<CreateOutcome, StoreError> {
    draft.validate().map_err(|r| StoreError::Sqlite(format!("建档被拒：{}", r.reason())))?;
    let ts = ts.map(|s| s.to_string()).unwrap_or_else(now_iso_pub);
    let dup = check_duplicate(store.conn(), draft, announcement_keys)?;
    if dup.is_dup() && !force {
        return Ok(CreateOutcome::Duplicate(dup));
    }

    let key = dup.key.clone();
    let basis = key.merge_basis();
    let guard = store.begin_write("manual-create")?;
    let conn = guard.conn();
    let tx = conn.unchecked_transaction()?;
    tx.execute(
        "INSERT INTO manual_project
           (manual_id, project_name, buyer, agg_key, agg_basis, area, budget, note,
            lead_source, expect_method, created_at, updated_at, merged_into)
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,NULL)",
        params![
            manual_id_of(&key.key),
            draft.project_name.trim(),
            draft.buyer.trim(),
            key.key,
            basis,
            draft.area.as_deref().map(str::trim),
            draft.budget.as_deref().map(str::trim),
            draft.note.as_deref().map(str::trim),
            draft.lead_source.as_deref().map(str::trim),
            draft.expect_method.as_deref().map(str::trim),
            ts,
            ts,
        ],
    )?;
    tx.commit()?;
    let created = find_by_key(store.conn(), &key.key)?
        .ok_or_else(|| StoreError::Sqlite("建档后回读不到记录".to_string()))?;
    Ok(CreateOutcome::Created(created))
}

/// 列出全部手动档案（`merged_only = true` 时只列已合并的）。
pub fn list_manual(store: &Store, merged_only: bool) -> Result<Vec<ManualProject>, StoreError> {
    let conn = store.conn();
    let sql = if merged_only {
        "SELECT manual_id, project_name, buyer, agg_key, agg_basis, area, budget, note,
                lead_source, expect_method, created_at, updated_at, merged_into
           FROM manual_project WHERE merged_into IS NOT NULL ORDER BY created_at DESC"
    } else {
        "SELECT manual_id, project_name, buyer, agg_key, agg_basis, area, budget, note,
                lead_source, expect_method, created_at, updated_at, merged_into
           FROM manual_project ORDER BY created_at DESC"
    };
    let mut stmt = conn.prepare(sql)?;
    let rows = stmt.query_map([], row_of)?;
    Ok(rows.collect::<Result<Vec<_>, _>>()?)
}

/// 按 id 取一条手动档案。
pub fn get_manual(store: &Store, manual_id: &str) -> Result<Option<ManualProject>, StoreError> {
    let conn = store.conn();
    let mut stmt = conn.prepare(
        "SELECT manual_id, project_name, buyer, agg_key, agg_basis, area, budget, note,
                lead_source, expect_method, created_at, updated_at, merged_into
           FROM manual_project WHERE manual_id = ?1",
    )?;
    let mut rows = stmt.query(params![manual_id])?;
    match rows.next()? {
        Some(r) => Ok(Some(row_of(r)?)),
        None => Ok(None),
    }
}

// ───────────────────────── 疑似关联（强命中） ─────────────────────────

/// 一条疑似关联（`pending` 即「待用户裁决」的提示条）。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SuspectedLink {
    pub link_id: String,
    pub manual_id: String,
    /// 公告聚合项目键（合并目标）
    pub target_project: String,
    /// 命中强度：exact_key / same_buyer_name_overlap / same_buyer_code
    pub hit_strength: String,
    /// 命中依据原文（对照视图逐项标差异用）
    pub hit_basis: String,
    /// pending / merged / ignored
    pub decision: String,
    pub decided_at: Option<String>,
    pub created_at: String,
}

/// 公告侧参与疑似关联检测的最小信息（由界面层从聚合查询结果里给出）。
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct AnnouncementCandidate {
    pub project_key: String,
    pub project_name: String,
    pub buyer: String,
    /// 该项目里的项目编号（可能为空）
    pub project_code: String,
    /// 该项目的公告条数（提示文案「N 条公告」用）
    pub announcement_count: usize,
}

fn link_id_of(manual_id: &str, target: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(manual_id.as_bytes());
    h.update([0x1f]);
    h.update(target.as_bytes());
    format!("lk:{:x}", &h.finalize())[..20].to_string()
}

/// 强命中判定：返回 `(强度, 依据)`；不命中返回 None。
///
/// 三种形态的前提都是**同一采购人**（规格 §2 不同采购人同名项目不合并）。
fn strong_hit(manual: &ManualProject, cand: &AnnouncementCandidate) -> Option<(&'static str, String)> {
    let m_buyer = crate::intel_key::normalize_buyer(&manual.buyer);
    let c_buyer = crate::intel_key::normalize_buyer(&cand.buyer);
    if m_buyer.is_empty() || m_buyer != c_buyer {
        // 采购人不同（或缺失无法确认）→ 强命中不成立，不提示
        return None;
    }
    let (c_name, _) = crate::intel_key::normalize_name(&cand.project_name);
    let (m_name, _) = crate::intel_key::normalize_name(&manual.project_name);

    // ① 精确键
    if !m_name.is_empty() && m_name == c_name {
        return Some((
            "exact_key",
            format!("聚合键相同：采购人「{}」+ 项目名归一一致", manual.buyer),
        ));
    }
    // ② 同采购人、项目名归一后一方包含另一方
    if !m_name.is_empty() && !c_name.is_empty() && (m_name.contains(&c_name) || c_name.contains(&m_name)) {
        return Some((
            "same_buyer_name_overlap",
            format!(
                "同一采购人「{}」；项目名归一后一方包含另一方（手动「{}」↔ 公告「{}」，{} 条公告）",
                manual.buyer, manual.project_name, cand.project_name, cand.announcement_count
            ),
        ));
    }
    // ③ 编号命中：手动档案的名称/备注里提到的编号出现在公告项目上
    let code = crate::lead_dedup::normalize_project_code(&cand.project_code);
    if !code.is_empty() {
        let hay = format!(
            "{} {} {}",
            manual.project_name,
            manual.note.as_deref().unwrap_or(""),
            manual.buyer
        );
        let hay_norm = crate::lead_dedup::normalize_project_code(&hay);
        if hay_norm.contains(&code) {
            return Some((
                "same_buyer_code",
                format!(
                    "同一采购人「{}」；项目编号「{}」在手动档案中被提及",
                    manual.buyer, cand.project_code
                ),
            ));
        }
    }
    None
}

/// 检测疑似关联并落 `pending` 提示（**从不自动合并**，规格 A-7）。
///
/// - 已 `ignored` 的同一对（manual_id, target_project）**不再重复提示**
///   （规格：忽略后同一批公告不再重复提示，留审计可手动重开）；
/// - 已 `merged` 的对也不再提示；
/// - 返回本轮**新增**的 pending 行（已存在的对不重复计数）。
pub fn detect_suspected_links(
    store: &Store,
    candidates: &[AnnouncementCandidate],
    ts: Option<&str>,
) -> Result<Vec<SuspectedLink>, StoreError> {
    let ts = ts.map(|s| s.to_string()).unwrap_or_else(now_iso_pub);
    let manuals = list_manual(store, false)?;
    let guard = store.begin_write("manual-link")?;
    let conn = guard.conn();
    let tx = conn.unchecked_transaction()?;
    let mut created: Vec<SuspectedLink> = Vec::new();

    for m in manuals.iter().filter(|m| m.is_unmerged()) {
        for c in candidates {
            let Some((strength, basis)) = strong_hit(m, c) else { continue };
            let link_id = link_id_of(&m.manual_id, &c.project_key);
            // 已裁决过的对：不再提示（ignored 的留审计、可手动重开）
            let prior: Option<String> = {
                let mut stmt = tx.prepare("SELECT decision FROM manual_link WHERE link_id = ?1")?;
                let mut rows = stmt.query(params![link_id])?;
                match rows.next()? {
                    Some(r) => Some(r.get(0)?),
                    None => None,
                }
            };
            if let Some(d) = prior {
                if d == "ignored" || d == "merged" {
                    continue;
                }
            }
            tx.execute(
                "INSERT INTO manual_link
                   (link_id, manual_id, target_project, hit_strength, hit_basis, decision,
                    decided_at, created_at)
                 VALUES (?1,?2,?3,?4,?5,'pending',NULL,
                         COALESCE((SELECT created_at FROM manual_link WHERE link_id = ?1), ?6))
                 ON CONFLICT(link_id) DO UPDATE SET
                   hit_strength = excluded.hit_strength,
                   hit_basis = excluded.hit_basis,
                   decision = 'pending',
                   decided_at = NULL",
                params![link_id, m.manual_id, c.project_key, strength, basis, ts],
            )?;
            created.push(SuspectedLink {
                link_id,
                manual_id: m.manual_id.clone(),
                target_project: c.project_key.clone(),
                hit_strength: strength.to_string(),
                hit_basis: basis,
                decision: "pending".to_string(),
                decided_at: None,
                created_at: ts.clone(),
            });
        }
    }
    tx.commit()?;
    Ok(created)
}

/// 列出某手动项目的疑似关联（默认只列 pending）。
pub fn links_of(store: &Store, manual_id: &str, pending_only: bool) -> Result<Vec<SuspectedLink>, StoreError> {
    let conn = store.conn();
    let sql = if pending_only {
        "SELECT link_id, manual_id, target_project, hit_strength, hit_basis, decision,
                decided_at, created_at FROM manual_link
          WHERE manual_id = ?1 AND decision = 'pending' ORDER BY created_at DESC"
    } else {
        "SELECT link_id, manual_id, target_project, hit_strength, hit_basis, decision,
                decided_at, created_at FROM manual_link
          WHERE manual_id = ?1 ORDER BY created_at DESC"
    };
    let mut stmt = conn.prepare(sql)?;
    let rows = stmt.query_map(params![manual_id], |r| {
        Ok(SuspectedLink {
            link_id: r.get(0)?,
            manual_id: r.get(1)?,
            target_project: r.get(2)?,
            hit_strength: r.get(3)?,
            hit_basis: r.get(4)?,
            decision: r.get(5)?,
            decided_at: r.get(6)?,
            created_at: r.get(7)?,
        })
    })?;
    Ok(rows.collect::<Result<Vec<_>, _>>()?)
}

// ───────────────────────── 合并 / 忽略 / 重开（含审计） ─────────────────────────

/// 用户确认合并：把手动档案并进公告聚合项目（规格 A-7「合并到该项目流程」）。
///
/// 三条规则逐条落地：
/// - **采购人、项目名以公告为准** → 返回值 [`MergeOutcome::effective_name/buyer`] 取公告侧；
/// - **地区/预算等手动字段保留并标注「手动补充」** → `manual_supplement` 列表，
///   档案行本身不清空（保留为对照历史）；
/// - **写一条「手动合并」审计** → `manual_merge_audit` action = `merge`。
pub fn merge_into_announcement(
    store: &Store,
    manual_id: &str,
    target_project: &str,
    announcement_name: &str,
    announcement_buyer: &str,
    actor: &str,
    ts: Option<&str>,
) -> Result<MergeOutcome, StoreError> {
    let ts = ts.map(|s| s.to_string()).unwrap_or_else(now_iso_pub);
    let manual = get_manual(store, manual_id)?
        .ok_or_else(|| StoreError::Sqlite(format!("手动档案不存在：{manual_id}")))?;

    let mut conflicts = Vec::new();
    if manual.project_name.trim() != announcement_name.trim() {
        conflicts.push(format!(
            "项目名：手动「{}」→ 公告「{}」（以公告为准）",
            manual.project_name, announcement_name
        ));
    }
    if manual.buyer.trim() != announcement_buyer.trim() {
        conflicts.push(format!(
            "采购人：手动「{}」→ 公告「{}」（以公告为准）",
            manual.buyer, announcement_buyer
        ));
    }
    let supplement = manual.manual_supplement_fields();

    let guard = store.begin_write("manual-merge")?;
    let conn = guard.conn();
    let tx = conn.unchecked_transaction()?;
    tx.execute(
        "UPDATE manual_project SET merged_into = ?1, updated_at = ?2 WHERE manual_id = ?3",
        params![target_project, ts, manual_id],
    )?;
    // 与本目标相关的 pending/历史行一并标 merged
    let link_id = link_id_of(manual_id, target_project);
    set_decision(&tx, &link_id, "merged", &ts)?;
    write_audit(
        &tx,
        manual_id,
        "merge",
        target_project,
        actor,
        &ts,
        &format!(
            "采购人/项目名以公告为准（{}）；手动补充保留：{}",
            conflicts.join("；"),
            if supplement.is_empty() { "无".to_string() } else { supplement.join("、") }
        ),
    )?;
    tx.commit()?;

    let mut after = get_manual(store, manual_id)?
        .ok_or_else(|| StoreError::Sqlite("合并后回读不到档案".to_string()))?;
    after.merged_into = Some(target_project.to_string());
    Ok(MergeOutcome {
        manual: after,
        target_project: target_project.to_string(),
        effective_name: announcement_name.trim().to_string(),
        effective_buyer: announcement_buyer.trim().to_string(),
        manual_supplement: supplement,
        conflicts,
    })
}

/// 反向合并（规格 A-7「合并方向也支持反向」）：查重命中已有聚合卡时，
/// 直接把手动档案并进去。落库动作与正向完全一致，只是入口语义不同——
/// 共用 [`merge_into_announcement`] 的实现，避免两条路写出两种审计格式。
pub fn merge_manual_into_project(
    store: &Store,
    manual_id: &str,
    target_project: &str,
    announcement_name: &str,
    announcement_buyer: &str,
    actor: &str,
    ts: Option<&str>,
) -> Result<MergeOutcome, StoreError> {
    merge_into_announcement(store, manual_id, target_project, announcement_name, announcement_buyer, actor, ts)
}

/// 用户忽略（「不是同一项目」）：写审计、置 `ignored`，之后同一对不再提示。
pub fn ignore_link(
    store: &Store,
    manual_id: &str,
    target_project: &str,
    actor: &str,
    ts: Option<&str>,
) -> Result<(), StoreError> {
    let ts = ts.map(|s| s.to_string()).unwrap_or_else(now_iso_pub);
    let guard = store.begin_write("manual-ignore")?;
    let conn = guard.conn();
    let tx = conn.unchecked_transaction()?;
    let link_id = link_id_of(manual_id, target_project);
    set_decision(&tx, &link_id, "ignored", &ts)?;
    write_audit(&tx, manual_id, "ignore", target_project, actor, &ts, "用户判定不是同一项目")?;
    tx.commit()?;
    Ok(())
}

/// 手动重开对照（规格 A-7「可手动重开对照」）：把 `ignored` 改回 `pending`，写审计。
pub fn reopen_link(
    store: &Store,
    manual_id: &str,
    target_project: &str,
    actor: &str,
    ts: Option<&str>,
) -> Result<(), StoreError> {
    let ts = ts.map(|s| s.to_string()).unwrap_or_else(now_iso_pub);
    let guard = store.begin_write("manual-reopen")?;
    let conn = guard.conn();
    let tx = conn.unchecked_transaction()?;
    let link_id = link_id_of(manual_id, target_project);
    set_decision(&tx, &link_id, "pending", &ts)?;
    tx.execute("UPDATE manual_link SET decided_at = NULL WHERE link_id = ?1", params![link_id])?;
    write_audit(&tx, manual_id, "reopen", target_project, actor, &ts, "用户手动重开对照")?;
    tx.commit()?;
    Ok(())
}

/// 一条审计记录（界面「合并历史」可查）。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MergeAudit {
    pub audit_id: String,
    pub manual_id: String,
    pub action: String,
    pub target_project: String,
    pub actor: String,
    pub at: String,
    pub detail: String,
}

/// 读某手动项目的审计历史。
pub fn audit_of(store: &Store, manual_id: &str) -> Result<Vec<MergeAudit>, StoreError> {
    let conn = store.conn();
    let mut stmt = conn.prepare(
        "SELECT audit_id, manual_id, action, target_project, actor, at, detail
           FROM manual_merge_audit WHERE manual_id = ?1 ORDER BY at ASC, audit_id ASC",
    )?;
    let rows = stmt.query_map(params![manual_id], |r| {
        Ok(MergeAudit {
            audit_id: r.get(0)?,
            manual_id: r.get(1)?,
            action: r.get(2)?,
            target_project: r.get(3)?,
            actor: r.get(4)?,
            at: r.get(5)?,
            detail: r.get(6)?,
        })
    })?;
    Ok(rows.collect::<Result<Vec<_>, _>>()?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::TARGET_SCHEMA_VERSION;

    fn tmp_store(tag: &str) -> Store {
        let dir = std::env::temp_dir().join(format!("wb-manual-{}-{}", tag, std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        Store::open(&dir).expect("open store")
    }

    fn draft(name: &str, buyer: &str) -> ManualDraft {
        ManualDraft {
            project_name: name.into(),
            buyer: buyer.into(),
            ..Default::default()
        }
    }

    /// 迁移 v6 真的跑起来了，且目标版本已递增（已发布脚本不动，只加新脚本）。
    #[test]
    fn migration_v6_applied_and_target_bumped() {
        let store = tmp_store("mig");
        assert_eq!(store.schema_version(), TARGET_SCHEMA_VERSION);
        // 不钉死具体数字：v3.4.0 起目标版本由 6 升到 7（新增 0007_project_track）。
        // 本测试要守的是「v6 已应用且版本不再停在 v5」，钉死 6 会把它绑成版本号回归器。
        assert!(TARGET_SCHEMA_VERSION >= 6, "基线 b645562 已到 v5，手动建档从 v6 起");
        // 三张表都在
        let conn = store.conn();
        for t in ["manual_project", "manual_link", "manual_merge_audit"] {
            let n: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name=?1",
                    params![t],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(n, 1, "v6 应建出 {t} 表");
        }
        // 迁移历史里 0006 记成功
        let ok: String = conn
            .query_row("SELECT result FROM schema_migration WHERE version=6", [], |r| r.get(0))
            .unwrap();
        assert_eq!(ok, "success");
    }

    /// 必填两项：缺项目名或缺采购人都拒绝，且理由可指到具体字段。
    #[test]
    fn required_fields_are_enforced() {
        let store = tmp_store("req");
        let r = create_manual(&store, &draft("", "某局"), false, &[], Some("2026-11-23T09:00:00Z"));
        assert!(r.is_err(), "缺项目名必须拒绝");
        assert!(r.unwrap_err().to_string().contains("项目名必填"));

        let r2 = create_manual(&store, &draft("无人机采购项目", "  "), false, &[], Some("2026-11-23T09:00:00Z"));
        assert!(r2.is_err(), "缺采购人必须拒绝");
        assert!(r2.unwrap_err().to_string().contains("采购人必填"));
    }

    /// 可选字段可空、也可填；建档后能原样回读（含全部 5 个可选项）。
    #[test]
    fn optional_fields_roundtrip() {
        let store = tmp_store("opt");
        let mut d = draft("综合农事服务中心无人机采购项目", "安徽省普济圩现代农业集团");
        d.area = Some("铜陵".into());
        d.budget = Some("22万元".into());
        d.note = Some("村委口头线索".into());
        d.lead_source = Some("乡镇走访".into());
        d.expect_method = Some("竞争性磋商".into());
        let out = create_manual(&store, &d, false, &[], Some("2026-11-23T09:00:00Z")).unwrap();
        let m = match out {
            CreateOutcome::Created(m) => m,
            _ => panic!("应新建"),
        };
        assert_eq!(m.area.as_deref(), Some("铜陵"));
        assert_eq!(m.budget.as_deref(), Some("22万元"));
        assert_eq!(m.lead_source.as_deref(), Some("乡镇走访"));
        assert_eq!(m.expect_method.as_deref(), Some("竞争性磋商"));
        assert!(m.is_unmerged(), "新建档案未合并");
        assert_eq!(m.manual_supplement_fields().len(), 5, "五个可选字段都填了");
        assert!(m.agg_basis.contains("采购人"), "建档时要把合并依据原文存下来: {}", m.agg_basis);
    }

    /// 建档即查重：同键第二次建档被拦，并带回命中项（面板据此给两个出口）。
    #[test]
    fn create_dedups_against_existing_manual() {
        let store = tmp_store("dedup");
        let d = draft("综合农事服务中心无人机采购项目（第2次）", "普农集团");
        let first = create_manual(&store, &d, false, &[], Some("2026-11-23T09:00:00Z")).unwrap();
        assert!(matches!(first, CreateOutcome::Created(_)));

        // 归一后同键（去掉「（第2次）」）
        let d2 = draft("综合农事服务中心无人机采购项目", "普农集团");
        let second = create_manual(&store, &d2, false, &[], Some("2026-11-23T10:00:00Z")).unwrap();
        match second {
            CreateOutcome::Duplicate(check) => {
                assert!(check.is_dup());
                assert!(check.manual_hit.is_some(), "应命中已有手动档案");
                let p = check.prompt().unwrap();
                assert!(p.contains("已存在疑似同一项目"), "提示语要符合规格: {p}");
                assert!(p.contains("综合农事服务中心无人机采购项目"));
            }
            _ => panic!("同键必须被查重拦下"),
        }
        assert_eq!(list_manual(&store, false).unwrap().len(), 1, "拦下后库里仍只有一条");
    }

    /// 不同采购人同名项目**不得**被判重（规格 §2 硬约束）。
    #[test]
    fn dedup_does_not_cross_buyers() {
        let store = tmp_store("crossbuyer");
        create_manual(&store, &draft("无人机采购项目", "普农集团"), false, &[], Some("2026-11-23T09:00:00Z")).unwrap();
        let out = create_manual(&store, &draft("无人机采购项目", "池州市农业农村局"), false, &[], Some("2026-11-23T10:00:00Z")).unwrap();
        assert!(matches!(out, CreateOutcome::Created(_)), "不同采购人应允许各自建档");
        assert_eq!(list_manual(&store, false).unwrap().len(), 2);
    }

    /// 查重也能命中已有公告聚合项目（规格 A-7「打开该聚合卡」那个出口）。
    #[test]
    fn dedup_hits_existing_announcement_project() {
        let store = tmp_store("dupann");
        let keys = vec![(
            aggregate_key("普农集团", "综合农事服务中心无人机采购项目").key,
            "综合农事服务中心无人机采购项目".to_string(),
        )];
        let out = create_manual(
            &store,
            &draft("综合农事服务中心无人机采购项目", "普农集团"),
            false,
            &keys,
            Some("2026-11-23T09:00:00Z"),
        )
        .unwrap();
        match out {
            CreateOutcome::Duplicate(c) => {
                assert!(c.announcement_hit.is_some(), "应命中公告聚合项目");
                assert!(c.prompt().unwrap().contains("已有公告聚合"));
            }
            _ => panic!("应被公告聚合项目拦下"),
        }
    }

    /// 强命中三类：精确键 / 同采购人名包含 / 同采购人编号命中。
    #[test]
    fn strong_hit_detects_three_shapes() {
        let store = tmp_store("strong");
        let m = match create_manual(
            &store,
            &draft("综合农事服务中心无人机采购项目", "普农集团"),
            false,
            &[],
            Some("2026-11-23T09:00:00Z"),
        )
        .unwrap()
        {
            CreateOutcome::Created(m) => m,
            _ => panic!(),
        };

        // ① 精确键
        let exact = AnnouncementCandidate {
            project_key: "code:x1".into(),
            project_name: "综合农事服务中心无人机采购项目".into(),
            buyer: "普农集团".into(),
            project_code: "TLTRZB【2026】074".into(),
            announcement_count: 4,
        };
        let (s, b) = strong_hit(&m, &exact).expect("精确键应强命中");
        assert_eq!(s, "exact_key");
        assert!(b.contains("聚合键相同"));

        // ② 同采购人、名称一方包含另一方
        let overlap = AnnouncementCandidate {
            project_key: "code:x2".into(),
            project_name: "综合农事服务中心无人机采购项目（二期）".into(),
            buyer: "普农集团".into(),
            project_code: String::new(),
            announcement_count: 3,
        };
        let (s2, b2) = strong_hit(&m, &overlap).expect("名称包含应强命中");
        assert_eq!(s2, "same_buyer_name_overlap");
        assert!(b2.contains("3 条公告"), "依据要带公告条数: {b2}");

        // ③ 不同采购人 → 三种形态全部不成立
        let other = AnnouncementCandidate {
            project_key: "code:x3".into(),
            project_name: "综合农事服务中心无人机采购项目".into(),
            buyer: "池州市农业农村局".into(),
            project_code: "TLTRZB【2026】074".into(),
            announcement_count: 4,
        };
        assert!(strong_hit(&m, &other).is_none(), "不同采购人绝不强命中");
    }

    /// 编号命中：手动档案备注里提到编号，公告项目编号一致 → 强命中。
    #[test]
    fn strong_hit_by_project_code_in_note() {
        let store = tmp_store("code");
        let mut d = draft("某村无人机植保服务项目", "普农集团");
        d.note = Some("线索来自 TLTRZB【2026】074 的开标记录".into());
        let m = match create_manual(&store, &d, false, &[], Some("2026-11-23T09:00:00Z")).unwrap() {
            CreateOutcome::Created(m) => m,
            _ => panic!(),
        };
        let cand = AnnouncementCandidate {
            project_key: "code:tltrzb2026074".into(),
            project_name: "综合农事服务中心无人机采购项目".into(),
            buyer: "普农集团".into(),
            project_code: "TLTRZB【2026】074".into(),
            announcement_count: 2,
        };
        let (s, b) = strong_hit(&m, &cand).expect("编号命中应强命中");
        assert_eq!(s, "same_buyer_code");
        assert!(b.contains("项目编号"));
    }

    /// 疑似关联只产 pending 提示，**绝不自动合并**（规格 A-7）。
    #[test]
    fn detection_never_auto_merges() {
        let store = tmp_store("nolinkauto");
        let m = match create_manual(
            &store,
            &draft("综合农事服务中心无人机采购项目", "普农集团"),
            false,
            &[],
            Some("2026-11-23T09:00:00Z"),
        )
        .unwrap()
        {
            CreateOutcome::Created(m) => m,
            _ => panic!(),
        };
        let cands = vec![AnnouncementCandidate {
            project_key: "code:tltrzb2026074".into(),
            project_name: "综合农事服务中心无人机采购项目".into(),
            buyer: "普农集团".into(),
            project_code: "TLTRZB【2026】074".into(),
            announcement_count: 4,
        }];
        let created = detect_suspected_links(&store, &cands, Some("2026-11-24T09:00:00Z")).unwrap();
        assert_eq!(created.len(), 1, "应出 1 条提示");
        assert_eq!(created[0].decision, "pending", "只提示，不合并");
        let after = get_manual(&store, &m.manual_id).unwrap().unwrap();
        assert!(after.is_unmerged(), "检测不得把档案标成已合并");
    }

    /// 忽略后同一对不再重复提示；审计留痕；可手动重开。
    #[test]
    fn ignore_then_no_repeat_then_reopen() {
        let store = tmp_store("ignore");
        let m = match create_manual(
            &store,
            &draft("综合农事服务中心无人机采购项目", "普农集团"),
            false,
            &[],
            Some("2026-11-23T09:00:00Z"),
        )
        .unwrap()
        {
            CreateOutcome::Created(m) => m,
            _ => panic!(),
        };
        let cands = vec![AnnouncementCandidate {
            project_key: "code:tltrzb2026074".into(),
            project_name: "综合农事服务中心无人机采购项目".into(),
            buyer: "普农集团".into(),
            project_code: String::new(),
            announcement_count: 2,
        }];
        detect_suspected_links(&store, &cands, Some("2026-11-24T09:00:00Z")).unwrap();
        ignore_link(&store, &m.manual_id, "code:tltrzb2026074", "大角子", Some("2026-11-24T10:00:00Z")).unwrap();

        // 同一批公告再来一轮：不再提示
        let again = detect_suspected_links(&store, &cands, Some("2026-11-25T09:00:00Z")).unwrap();
        assert!(again.is_empty(), "忽略后同一对不得重复提示，got {}", again.len());
        assert!(links_of(&store, &m.manual_id, true).unwrap().is_empty());

        // 审计留痕（ignore 一条）
        let audits = audit_of(&store, &m.manual_id).unwrap();
        assert!(audits.iter().any(|a| a.action == "ignore"), "忽略要留审计: {:?}", audits);

        // 手动重开：回到 pending，可再次提示
        reopen_link(&store, &m.manual_id, "code:tltrzb2026074", "大角子", Some("2026-11-26T09:00:00Z")).unwrap();
        let reopened = links_of(&store, &m.manual_id, true).unwrap();
        assert_eq!(reopened.len(), 1, "重开后应回到待裁决");
        let audits2 = audit_of(&store, &m.manual_id).unwrap();
        assert!(audits2.iter().any(|a| a.action == "reopen"), "重开要留审计");
    }

    /// 确认合并：采购人/项目名以公告为准；手动字段保留并标注；写合并审计。
    #[test]
    fn merge_uses_announcement_authority_and_keeps_manual_fields() {
        let store = tmp_store("merge");
        let mut d = draft("综合农事服务中心无人机采购项目（第2次）", "普农集团");
        d.area = Some("铜陵".into());
        d.budget = Some("22万元".into());
        d.note = Some("村委口头线索".into());
        let m = match create_manual(&store, &d, false, &[], Some("2026-11-23T09:00:00Z")).unwrap() {
            CreateOutcome::Created(m) => m,
            _ => panic!(),
        };
        let out = merge_into_announcement(
            &store,
            &m.manual_id,
            "code:tltrzb2026074",
            "综合农事服务中心无人机采购项目",
            "安徽省普济圩现代农业集团有限公司",
            "大角子",
            Some("2026-11-25T09:00:00Z"),
        )
        .unwrap();

        assert_eq!(out.effective_name, "综合农事服务中心无人机采购项目", "项目名以公告为准");
        assert_eq!(out.effective_buyer, "安徽省普济圩现代农业集团有限公司", "采购人以公告为准");
        assert!(out.manual_supplement.contains(&"地区"), "地区要保留并标注手动补充");
        assert!(out.manual_supplement.contains(&"预算"));
        assert!(!out.conflicts.is_empty(), "名称/采购人有差异要进对照历史: {:?}", out.conflicts);
        assert!(out.conflicts.iter().any(|c| c.contains("第2次")), "差异要写出原始写法");

        // 档案仍保留手动字段（不清空），但已标 merged
        let after = get_manual(&store, &m.manual_id).unwrap().unwrap();
        assert_eq!(after.merged_into.as_deref(), Some("code:tltrzb2026074"));
        assert_eq!(after.area.as_deref(), Some("铜陵"), "手动字段保留");
        assert!(!after.is_unmerged());

        // 审计写了 merge 一条，且写明「以公告为准」与保留项
        let audits = audit_of(&store, &m.manual_id).unwrap();
        let merge_a = audits.iter().find(|a| a.action == "merge").expect("应有合并审计");
        assert!(merge_a.detail.contains("以公告为准"), "审计要记口径: {}", merge_a.detail);
        assert!(merge_a.detail.contains("地区"), "审计要记保留的手动字段: {}", merge_a.detail);
        assert_eq!(merge_a.actor, "大角子");
    }

    /// 反向合并入口与正向共用实现：审计格式一致。
    #[test]
    fn reverse_merge_writes_same_audit_shape() {
        let store = tmp_store("rev");
        let m = match create_manual(
            &store,
            &draft("无人机采购项目", "普农集团"),
            false,
            &[],
            Some("2026-11-23T09:00:00Z"),
        )
        .unwrap()
        {
            CreateOutcome::Created(m) => m,
            _ => panic!(),
        };
        merge_manual_into_project(
            &store,
            &m.manual_id,
            "code:p1",
            "无人机采购项目",
            "普农集团",
            "大角子",
            Some("2026-11-25T09:00:00Z"),
        )
        .unwrap();
        let audits = audit_of(&store, &m.manual_id).unwrap();
        let a = audits.iter().find(|x| x.action == "merge").expect("反向合并也要写审计");
        assert_eq!(a.target_project, "code:p1");
    }

    /// 已合并的档案不再参与疑似关联检测（已回到公告驱动）。
    #[test]
    fn merged_manual_no_longer_detects() {
        let store = tmp_store("mergedskip");
        let m = match create_manual(
            &store,
            &draft("无人机采购项目", "普农集团"),
            false,
            &[],
            Some("2026-11-23T09:00:00Z"),
        )
        .unwrap()
        {
            CreateOutcome::Created(m) => m,
            _ => panic!(),
        };
        merge_into_announcement(&store, &m.manual_id, "code:p1", "无人机采购项目", "普农集团", "大角子", None).unwrap();
        let cands = vec![AnnouncementCandidate {
            project_key: "code:p2".into(),
            project_name: "无人机采购项目".into(),
            buyer: "普农集团".into(),
            project_code: String::new(),
            announcement_count: 1,
        }];
        let created = detect_suspected_links(&store, &cands, Some("2026-11-26T09:00:00Z")).unwrap();
        assert!(created.is_empty(), "已合并档案不再出提示");
    }
}
