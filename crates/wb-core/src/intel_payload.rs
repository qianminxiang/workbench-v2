//! 情报「项目进程」界面的 **payload 适配层**（设计规格 v1.2 §2、A-6/A-7）。
//!
//! ## 这一层解决什么问题
//!
//! 已有的聚合查询（`intel_store::list_projects` / `read_project_timeline`）按
//! **项目编号键**分组、用**五段枚举**表达阶段，是「引擎视角」；界面层要的是
//! 「按采购人 + 项目名归一聚合、六段阶段点条、每段公告数、命中词、相关态、
//! 聚合依据可展开」的「产品视角」。两者口径不同，不能指望界面自己去换算——
//! 那会让同一套换算逻辑在 Rust 与前端各写一份，迟早漂移。
//!
//! 因此本模块**只做一次单向适配**：读已有聚合结果 → 按 [`crate::intel_key`] 的
//! 聚合键**重新分组** → 用 [`crate::intel_segment`] 映射成六段 → 产出两个 payload。
//! 不写库、不改既有查询、不动已有枚举，因而 `project_timeline.rs` 的 8 个
//! 既有测试与 CLI/Tauri 两端行为不受影响。
//!
//! ## 两个 payload（与界面层约定的字段契约，见契约文档）
//!
//! 1. [`ProjectListPayload`]——左栏聚合卡列表。每个项目含：项目名、采购人、
//!    六段阶段（每段公告数 + 最新日期 + 是否呈现）、当前阶段、最新公告时间、
//!    命中词、相关态、聚合依据（可展开看「本条为什么归到这个项目」）、
//!    项目来源（引擎聚合 / 手动建档）。
//! 2. [`ProjectDetailPayload`]——右栏进程轴。按段分组的公告条目，每条含
//!    日期、公告类型、标题、命中依据、原文链接（+转载副本链接）、阶段判据来源。
//!
//! ## 为什么「按段分组」要在这一层做
//!
//! `ProjectAggregate.nodes` 已经是按 (阶段序, 日期) 排好的时间轴，但它是
//! **五段口径**且**不分段成桶**。界面要「每段一个节点 + 该段下挂公告条目卡」，
//! 分组是展示结构，放在适配层一次算完，界面就不必再排序/再分组。

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::intel::Relevance;
use crate::intel_key::{aggregate_key, AggKey};
use crate::intel_segment::{
    segment_of_announcement, stage_view_of, ProjectStageView, Segment, SegmentBucket,
    EMPTY_NO_ANNOUNCEMENT,
};
use crate::intel_stage::{Stage, StageSource};
use crate::intel_store::read_project_timeline;
use crate::store::{Store, StoreError};

/// 项目来源（规格 §2「项目来源两类」）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProjectSourceKind {
    /// 引擎聚合（公告驱动）
    Engine,
    /// 手动建档（预留）
    Manual,
}

impl ProjectSourceKind {
    pub fn code(self) -> &'static str {
        match self {
            ProjectSourceKind::Engine => "engine",
            ProjectSourceKind::Manual => "manual",
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            ProjectSourceKind::Engine => "引擎聚合",
            ProjectSourceKind::Manual => "手动建档",
        }
    }
}

/// 聚合卡上的一个公告摘要（左栏用，只放不占空间的字段）。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AnnouncementBrief {
    pub title: String,
    pub date: String,
    pub segment: String,
    pub segment_label: String,
    pub url: String,
    pub relevance: String,
    pub relevance_label: String,
}

/// 聚合卡（左栏一项）。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ProjectCard {
    /// 聚合键 `k:<buyer_norm>|<name_norm>`；手动项目为 `mp:<manual_id>`
    pub agg_key: String,
    /// 引擎侧项目键（`code:` / `fp:` / `body:`；手动项目为空）
    pub engine_keys: Vec<String>,
    pub project_name: String,
    pub buyer: String,
    pub area: String,
    /// 预算（公告侧金额，取最近一条有金额的公告；无则空）
    pub budget: String,
    pub source_kind: String,
    pub source_kind_label: String,
    /// 六段阶段桶（顺序固定）
    pub stages: Vec<SegmentBucket>,
    /// 当前阶段短码（可推进五段里最靠后的一段；无则 `none`）
    pub current_stage: String,
    pub current_stage_label: String,
    pub current_stage_index: Option<u8>,
    /// 终止/废标侧标
    pub terminated: bool,
    pub termination_count: usize,
    /// 最新公告时间（该项目所有公告里最晚的日期；无公告为空）
    pub latest_announcement_at: String,
    /// 公告总条数
    pub announcement_count: usize,
    /// 命中词（去重后的主题词/加分词，界面折叠展示）
    pub hit_words: Vec<String>,
    /// 相关态（该项目公告里最「相关」的那一档：相关 > 疑似不相关 > 无关）
    pub relevance: String,
    pub relevance_label: String,
    /// 聚合依据（页面必须显示，可展开看「本条为什么归到这个项目」）
    pub merge_basis: String,
    /// 聚合依据的分项（逐项可读，界面对照视图直接用）
    pub merge_basis_items: Vec<String>,
    /// 参与合并的公告所属源站数
    pub source_count: usize,
    /// 空态文案（无公告时 = 「暂无公告 · 手动建档」）
    pub empty_label: String,
    /// 需挂「阶段待确认」标记的公告条数（heuristic/unknown 判据）
    pub pending_confirm_count: usize,
}

/// 左栏列表 payload。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ProjectListPayload {
    /// payload 契约版本（界面层按它判兼容）
    pub schema: String,
    pub total: usize,
    /// 阶段过滤短码（空串 = 不过滤）
    pub stage_filter: String,
    /// 六段定义（界面据此渲染阶段点条，不必内置枚举）
    pub segments: Vec<SegmentMeta>,
    pub projects: Vec<ProjectCard>,
}

/// 段的元信息（payload 自带，避免界面硬编码标签）。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SegmentMeta {
    pub code: String,
    pub label: String,
    pub display_index: u8,
    /// 是否参与进程推进（终止段为 false，界面画成侧标）
    pub advances: bool,
}

/// 右栏进程轴里的一条公告条目。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DetailEntry {
    pub record_id: String,
    pub title: String,
    pub date: String,
    /// 公告类型 chip（取判据命中词；无则「公告」）
    pub announcement_type: String,
    /// 命中依据（原文，如「正文复核加分词:植保」）
    pub hit_evidence: Vec<String>,
    /// 命中词（从依据里抽出的词，界面高亮用）
    pub hit_words: Vec<String>,
    pub relevance: String,
    pub relevance_label: String,
    pub stage: String,
    pub stage_label: String,
    /// 判据来源 rule / heuristic / manual / unknown
    pub stage_source: String,
    /// 判据说明（「为什么是这个阶段」）
    pub stage_reason: String,
    /// 原文链接（主源）
    pub url: String,
    pub source_name: String,
    /// 转载/副本链接（跨源合并进来的）
    pub alias_urls: Vec<String>,
    pub area: String,
    pub amount: Option<String>,
    pub body_head: String,
}

/// 进程轴上的一个段节点（该段下挂公告条目卡）。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DetailStageGroup {
    pub code: String,
    pub label: String,
    pub display_index: u8,
    pub advances: bool,
    pub count: usize,
    pub latest_date: String,
    pub entries: Vec<DetailEntry>,
}

/// 右栏明细 payload。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ProjectDetailPayload {
    pub schema: String,
    pub agg_key: String,
    pub engine_keys: Vec<String>,
    pub project_name: String,
    pub buyer: String,
    pub area: String,
    pub budget: String,
    pub source_kind: String,
    pub source_kind_label: String,
    pub current_stage: String,
    pub current_stage_label: String,
    pub current_stage_index: Option<u8>,
    pub terminated: bool,
    pub latest_announcement_at: String,
    pub announcement_count: usize,
    pub merge_basis: String,
    pub merge_basis_items: Vec<String>,
    /// **按段分组**的公告条目（只含有公告的段）
    pub stage_groups: Vec<DetailStageGroup>,
    /// 无公告时的统一文案
    pub empty_label: String,
    /// 界面「展开其余 N 段」用：实际有公告的段数
    pub present_stage_count: usize,
}

/// payload 契约版本。改动字段语义时递增，界面按此判兼容。
pub const PAYLOAD_SCHEMA: &str = "intel.project.v1";

fn segment_meta() -> Vec<SegmentMeta> {
    Segment::ALL
        .iter()
        .map(|s| SegmentMeta {
            code: s.code().to_string(),
            label: s.label().to_string(),
            display_index: s.display_index(),
            advances: s.advances(),
        })
        .collect()
}

/// 从 `reasons` 抽命中词：`主题词:X` / `加分词:X` / `正文复核加分词:X`。
///
/// 只抽这三类前缀——`位置:...` 是位置说明、`排除词:X` 是降级理由，都不是「命中词」。
fn hit_words_of(reasons: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for r in reasons {
        let mut s = r.as_str();
        for p in ["正文复核加分词:", "主题词:", "加分词:"] {
            if let Some(pos) = s.find(p) {
                s = &s[pos + p.len()..];
                break;
            }
        }
        if !s.contains(':') && !s.is_empty() && s.chars().count() <= 12 {
            let w = s.trim().to_string();
            if !w.is_empty() && !out.contains(&w) {
                out.push(w);
            }
        }
    }
    out
}

/// 公告类型 chip：优先取判据命中词（`stage_reason` 末尾那个词），其次标题尾词。
fn announcement_type_of(stage_reason: &str, title: &str) -> String {
    if let Some(v) = stage_reason.split('「').nth(1) {
        if let Some(w) = v.split('」').next() {
            if !w.is_empty() {
                return w.to_string();
            }
        }
    }
    // 标题里找最常见的类型尾词
    for w in ["中标（成交）结果公告", "中标结果公告", "招标公告", "采购公告", "更正公告", "终止公告", "废标公告", "合同公告", "验收公告"] {
        if title.contains(w) {
            return w.to_string();
        }
    }
    "公告".to_string()
}

/// 相关态：一条项目里多条公告取「最相关」一档（Core > Suspect > Excluded）。
fn relevance_rank(r: &str) -> u8 {
    match r {
        "core" => 0,
        "suspect" => 1,
        _ => 2,
    }
}

fn relevance_of(items: &[&crate::intel::IntelItem]) -> (String, String) {
    let mut best = "excluded";
    for it in items {
        let c = match it.relevance {
            Relevance::Core => "core",
            Relevance::Suspect => "suspect",
            Relevance::Excluded => "excluded",
        };
        if relevance_rank(c) < relevance_rank(best) {
            best = c;
        }
    }
    let label = match best {
        "core" => "相关",
        "suspect" => "疑似不相关",
        _ => "无关",
    };
    (best.to_string(), label.to_string())
}

/// 一条公告在适配层的中间态（从 `ProjectNode` 拆出来，便于跨项目重新分组）。
struct AnnRow {
    engine_key: String,
    item: crate::intel::IntelItem,
    stage_source: StageSource,
    stage_reason: String,
    segment: Segment,
    alias_urls: Vec<String>,
}

impl AnnRow {
    fn date(&self) -> &str {
        &self.item.date
    }
    fn buyer(&self) -> &str {
        &self.item.org
    }
}

/// 读全部 current 行，还原成适配层要的行。
///
/// **为什么不用 `ProjectNode`**：`ProjectNode` 只带阶段三列与展示字段，
/// **不带 `relevance` / `reasons`**——而界面要的「命中词」「相关态」正在那里。
/// 所以这里直接读 `ext_record.record_json`，用 `IntelItem` 反序列化拿到真值，
/// 不去改已落地的 `read_project_timeline`（那会动到 8 条锁定语义的既有测试）。
///
/// 项目分组仍用引擎的项目键（`biz_key` 去掉节点后缀）：聚合层的「同一项目」
/// 判定沿用已落地的 `lead_dedup` 口径，本层只在其上再做「按采购人 + 项目名」的
/// 重组（见 [`project_list_payload`]）。
fn load_announcements(store: &Store) -> Result<Vec<AnnRow>, StoreError> {
    let conn = store.conn();
    let mut stmt = conn.prepare(
        "SELECT id, biz_key, record_json, stage, stage_source, stage_reason
           FROM ext_record
          WHERE is_current = 1
          ORDER BY valid_from ASC, id ASC",
    )?;
    let rows: Vec<(String, String, String, String, String, String)> = stmt
        .query_map([], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?))
        })?
        .collect::<Result<_, _>>()?;
    let mut out = Vec::with_capacity(rows.len());
    for (_id, biz_key, json, stage_c, src_c, reason) in rows {
        let item: crate::intel::IntelItem = serde_json::from_str(&json)
            .map_err(|e| StoreError::Sqlite(format!("record_json 反序列化失败: {e}")))?;
        let alias_urls = serde_json::from_str::<serde_json::Value>(&json)
            .ok()
            .and_then(|v| v.get("alias_urls").and_then(|a| a.as_array()).cloned())
            .map(|a| a.iter().filter_map(|x| x.as_str().map(|s| s.to_string())).collect())
            .unwrap_or_default();
        let old_stage = Stage::from_code(&stage_c);
        let src = StageSource::from_code(&src_c);
        let segment = segment_of_announcement(old_stage, src, &reason, &item.title);
        out.push(AnnRow {
            engine_key: crate::intel_store::split_project_key(&biz_key).0.to_string(),
            item,
            stage_source: src,
            stage_reason: reason,
            segment,
            alias_urls,
        });
    }
    Ok(out)
}

/// 计算左栏列表 payload（按规格 §2 聚合键重新分组）。
///
/// - `stage` 过滤按**当前阶段短码**（`intent|tender|correction|award|contract|termination`），
///   与六段一一对应；非法值返回 Err（不静默当不过滤）。
/// - `limit = 0` 不限。
pub fn project_list_payload(
    store: &Store,
    stage: Option<&str>,
    limit: usize,
) -> Result<ProjectListPayload, StoreError> {
    let filter = match stage.map(|s| s.trim()).unwrap_or("") {
        "" => None,
        c => Some(Segment::from_code(c).ok_or_else(|| {
            StoreError::Sqlite(format!(
                "阶段短码非法：{c}（可选 intent|tender|correction|award|contract|termination；留空表示不过滤）"
            ))
        })?),
    };

    let rows = load_announcements(store)?;
    // 按聚合键重新分组：规格 §2 —— 采购人 + 项目名归一
    let mut groups: BTreeMap<String, Vec<AnnRow>> = BTreeMap::new();
    let mut group_key_of: BTreeMap<String, AggKey> = BTreeMap::new();
    for r in rows {
        let key = aggregate_key(r.buyer(), &crate::intel_store::project_name_of_pub(&r.item.title));
        group_key_of.entry(key.key.clone()).or_insert(key.clone());
        groups.entry(key.key.clone()).or_default().push(r);
    }

    let mut cards: Vec<ProjectCard> = Vec::new();
    for (agg_key, rows) in groups.iter() {
        let key = &group_key_of[agg_key];
        let anns: Vec<(Segment, String)> = rows.iter().map(|r| (r.segment, r.date().to_string())).collect();
        let view: ProjectStageView = stage_view_of(&anns);
        let latest = rows.iter().map(|r| r.date().to_string()).max().unwrap_or_default();
        let items: Vec<&crate::intel::IntelItem> = rows.iter().map(|r| &r.item).collect();
        let (rel, rel_label) = relevance_of(&items);
        let mut hit_words: Vec<String> = Vec::new();
        for r in rows.iter() {
            for w in hit_words_of(&r.item.reasons) {
                if !hit_words.contains(&w) {
                    hit_words.push(w);
                }
            }
        }
        let budget = rows
            .iter()
            .filter_map(|r| r.item.amount.clone())
            .last()
            .unwrap_or_default();
        let mut engine_keys: Vec<String> = rows.iter().map(|r| r.engine_key.clone()).collect();
        engine_keys.sort();
        engine_keys.dedup();
        let mut sources: Vec<String> = rows.iter().map(|r| r.item.site_id.clone()).collect();
        sources.sort();
        sources.dedup();
        let pending = rows
            .iter()
            .filter(|r| matches!(r.stage_source, StageSource::Heuristic | StageSource::Unknown))
            .count();
        let area = rows
            .iter()
            .map(|r| r.item.area.clone())
            .find(|a| !a.trim().is_empty())
            .unwrap_or_default();
        let project_name = rows
            .iter()
            .map(|r| crate::intel_store::project_name_of_pub(&r.item.title))
            .find(|n| !n.trim().is_empty())
            .unwrap_or_else(|| key.project_name.clone());
        let buyer = rows
            .iter()
            .map(|r| r.item.org.clone())
            .find(|b| !b.trim().is_empty())
            .unwrap_or_default();

        let card = ProjectCard {
            agg_key: agg_key.clone(),
            engine_keys,
            project_name,
            buyer,
            area,
            budget,
            source_kind: ProjectSourceKind::Engine.code().to_string(),
            source_kind_label: ProjectSourceKind::Engine.label().to_string(),
            stages: view.buckets.clone(),
            current_stage: view.current_segment.clone(),
            current_stage_label: view.current_label.clone(),
            current_stage_index: view.current_display_index,
            terminated: view.terminated,
            termination_count: view.termination_count,
            latest_announcement_at: latest,
            announcement_count: rows.len(),
            hit_words,
            relevance: rel,
            relevance_label: rel_label,
            merge_basis: key.merge_basis(),
            merge_basis_items: merge_basis_items(
                key,
                &{
                    // 汇总全组剔除记录（顺序按行出现顺序，去重）
                    let mut acc: Vec<String> = Vec::new();
                    for r in rows.iter() {
                        let (_, notes) = crate::intel_key::normalize_name(
                            &crate::intel_store::project_name_of_pub(&r.item.title),
                        );
                        for n in notes {
                            if !acc.contains(&n) {
                                acc.push(n);
                            }
                        }
                    }
                    acc
                },
                rows.len(),
                sources.len(),
            ),
            source_count: sources.len(),
            empty_label: EMPTY_NO_ANNOUNCEMENT.to_string(),
            pending_confirm_count: pending,
        };
        if let Some(want) = filter {
            let hit = if want.advances() {
                card.current_stage == want.code()
            } else {
                card.terminated
            };
            if !hit {
                continue;
            }
        }
        cards.push(card);
    }

    cards.sort_by(|a, b| {
        b.latest_announcement_at
            .cmp(&a.latest_announcement_at)
            .then_with(|| a.agg_key.cmp(&b.agg_key))
    });
    let total = cards.len();
    if limit > 0 && cards.len() > limit {
        cards.truncate(limit);
    }
    Ok(ProjectListPayload {
        schema: PAYLOAD_SCHEMA.to_string(),
        total,
        stage_filter: filter.map(|s| s.code().to_string()).unwrap_or_default(),
        segments: segment_meta(),
        projects: cards,
    })
}

/// 聚合依据（页面「本条为什么归到这个项目」的展开内容）。
///
/// `stripped` 必须**汇总组内每一条公告的剔除记录**，不能只取首次建组的那个键：
/// 「某村无人机植保服务项目（第2次）中标结果公告」这一条正是靠剥掉「（第2次）」
/// 才并进来的，若只留第一行的剔除项，这条最有说服力的依据就丢了。
/// 不同行的相同剔除项按首现去重（同一句「剔除公告类型词「招标公告」」不必重复列）。
fn merge_basis_items(key: &AggKey, stripped: &[String], ann_count: usize, source_count: usize) -> Vec<String> {
    let mut out = vec![format!("采购人归一为「{}」", if key.buyer_norm.is_empty() { "（空）" } else { &key.buyer_norm })];
    out.push(format!("项目名归一为「{}」", key.name_norm));
    for s in stripped.iter() {
        if !out.contains(s) {
            out.push(s.clone());
        }
    }
    out.push(format!("合并 {ann_count} 条公告、{source_count} 个源站"));
    if key.buyer_norm.is_empty() {
        out.push("采购人缺失：仅与同样缺失采购人的记录相遇（不跨采购人合并）".to_string());
    }
    out
}

/// 单项目进程明细 payload（右栏）。
///
/// `agg_key` 可以是聚合键（`k:...`），也可以是引擎项目键（`code:`/`fp:`/`body:`）——
/// 前者走规格口径的分组，后者退回按该引擎项目单读（保持 CLI/Tauri 两端的既有用法）。
pub fn project_detail_payload(store: &Store, agg_key: &str) -> Result<Option<ProjectDetailPayload>, StoreError> {
    let key_str = agg_key.trim();
    if key_str.is_empty() {
        return Ok(None);
    }
    let rows = load_announcements(store)?;

    // 目标分组：聚合键直接匹配；引擎键则取出它所属的那个聚合组
    let target: Option<(AggKey, Vec<AnnRow>)> = if key_str.starts_with("k:") {
        let hit: Option<(AggKey, Vec<AnnRow>)>;
        let mut buckets: BTreeMap<String, (AggKey, Vec<AnnRow>)> = BTreeMap::new();
        for r in rows {
            let k = aggregate_key(r.buyer(), &crate::intel_store::project_name_of_pub(&r.item.title));
            buckets.entry(k.key.clone()).or_insert((k, Vec::new())).1.push(r);
        }
        hit = buckets.remove(key_str);
        hit
    } else {
        // 引擎键：先定位到该键的项目，再用项目的聚合键分组
        let Some(agg) = read_project_timeline(store, key_str)? else { return Ok(None) };
        let first = agg.nodes.first();
        let (buyer, name) = match first {
            Some(n) => (n.org.clone(), crate::intel_store::project_name_of_pub(&n.title)),
            None => (String::new(), agg.project_name.clone()),
        };
        let k = aggregate_key(&buyer, &name);
        let mut buckets: BTreeMap<String, (AggKey, Vec<AnnRow>)> = BTreeMap::new();
        for r in rows {
            let kk = aggregate_key(r.buyer(), &crate::intel_store::project_name_of_pub(&r.item.title));
            buckets.entry(kk.key.clone()).or_insert((kk, Vec::new())).1.push(r);
        }
        buckets.remove(&k.key)
    };

    let Some((key, rows)) = target else { return Ok(None) };
    if rows.is_empty() {
        return Ok(None);
    }

    let anns: Vec<(Segment, String)> = rows.iter().map(|r| (r.segment, r.date().to_string())).collect();
    let view = stage_view_of(&anns);
    let latest = rows.iter().map(|r| r.date().to_string()).max().unwrap_or_default();
    let budget = rows.iter().filter_map(|r| r.item.amount.clone()).last().unwrap_or_default();
    let mut engine_keys: Vec<String> = rows.iter().map(|r| r.engine_key.clone()).collect();
    engine_keys.sort();
    engine_keys.dedup();
    let project_name = rows
        .iter()
        .map(|r| crate::intel_store::project_name_of_pub(&r.item.title))
        .find(|n| !n.trim().is_empty())
        .unwrap_or_else(|| key.project_name.clone());
    let buyer = rows.iter().map(|r| r.item.org.clone()).find(|b| !b.trim().is_empty()).unwrap_or_default();
    let area = rows.iter().map(|r| r.item.area.clone()).find(|a| !a.trim().is_empty()).unwrap_or_default();

    // 按段分组：顺序固定六段，只输出有公告的段
    let mut groups: Vec<DetailStageGroup> = Vec::new();
    for seg in Segment::ALL {
        let mut entries: Vec<DetailEntry> = Vec::new();
        for r in rows.iter().filter(|r| r.segment == seg) {
            let items_ref: [&crate::intel::IntelItem; 1] = [&r.item];
            let (rel, rel_label) = relevance_of(&items_ref);
            entries.push(DetailEntry {
                record_id: r.item.key.clone(),
                title: r.item.title.clone(),
                date: r.item.date.clone(),
                announcement_type: announcement_type_of(&r.stage_reason, &r.item.title),
                hit_evidence: r.item.reasons.clone(),
                hit_words: hit_words_of(&r.item.reasons),
                relevance: rel,
                relevance_label: rel_label,
                stage: seg.code().to_string(),
                stage_label: seg.label().to_string(),
                stage_source: r.stage_source.code().to_string(),
                stage_reason: r.stage_reason.clone(),
                url: r.item.url.clone(),
                source_name: r.item.site_name.clone(),
                alias_urls: r.alias_urls.clone(),
                area: r.item.area.clone(),
                amount: r.item.amount.clone(),
                body_head: r.item.body_head.clone(),
            });
        }
        if entries.is_empty() {
            continue;
        }
        entries.sort_by(|a, b| a.date.cmp(&b.date).then_with(|| a.record_id.cmp(&b.record_id)));
        let latest_d = entries.iter().map(|e| e.date.clone()).max().unwrap_or_default();
        let count = entries.len();
        let bucket: &SegmentBucket = &view.buckets[seg.display_index() as usize - 1];
        groups.push(DetailStageGroup {
            code: seg.code().to_string(),
            label: seg.label().to_string(),
            display_index: seg.display_index(),
            advances: seg.advances(),
            count,
            latest_date: if bucket.latest_date.is_empty() { latest_d } else { bucket.latest_date.clone() },
            entries,
        });
    }

    let present_stage_count = groups.len();
    Ok(Some(ProjectDetailPayload {
        schema: PAYLOAD_SCHEMA.to_string(),
        agg_key: key.key.clone(),
        engine_keys,
        project_name,
        buyer,
        area,
        budget,
        source_kind: ProjectSourceKind::Engine.code().to_string(),
        source_kind_label: ProjectSourceKind::Engine.label().to_string(),
        current_stage: view.current_segment.clone(),
        current_stage_label: view.current_label.clone(),
        current_stage_index: view.current_display_index,
        terminated: view.terminated,
        latest_announcement_at: latest,
        announcement_count: rows.len(),
        merge_basis: key.merge_basis(),
        merge_basis_items: merge_basis_items(
            &key,
            &rows.iter().fold(Vec::new(), |mut acc: Vec<String>, r| {
                let (_, notes) =
                    crate::intel_key::normalize_name(&crate::intel_store::project_name_of_pub(&r.item.title));
                for n in notes {
                    if !acc.contains(&n) {
                        acc.push(n);
                    }
                }
                acc
            }),
            rows.len(),
            1,
        ),
        stage_groups: groups,
        empty_label: EMPTY_NO_ANNOUNCEMENT.to_string(),
        present_stage_count,
    }))
}

/// 手动建档项目的聚合卡（无公告，统一「暂无公告 · 手动建档」）。
pub fn manual_card(m: &crate::intel_manual::ManualProject) -> ProjectCard {
    // 手动项目不适用「当前阶段」规则（无公告可判）
    let view = stage_view_of(&[]);
    ProjectCard {
        agg_key: m.agg_key.clone(),
        engine_keys: Vec::new(),
        project_name: m.project_name.clone(),
        buyer: m.buyer.clone(),
        area: m.area.clone().unwrap_or_default(),
        budget: m.budget.clone().unwrap_or_default(),
        source_kind: ProjectSourceKind::Manual.code().to_string(),
        source_kind_label: ProjectSourceKind::Manual.label().to_string(),
        stages: view.buckets,
        current_stage: view.current_segment,
        current_stage_label: view.current_label,
        current_stage_index: view.current_display_index,
        terminated: false,
        termination_count: 0,
        latest_announcement_at: String::new(),
        announcement_count: 0,
        hit_words: Vec::new(),
        relevance: "unknown".to_string(),
        relevance_label: "暂无公告".to_string(),
        merge_basis: if m.agg_basis.is_empty() {
            aggregate_key(&m.buyer, &m.project_name).merge_basis()
        } else {
            m.agg_basis.clone()
        },
        merge_basis_items: vec![format!("手动建档：采购人「{}」+ 项目名「{}」", m.buyer, m.project_name)],
        source_count: 0,
        empty_label: EMPTY_NO_ANNOUNCEMENT.to_string(),
        pending_confirm_count: 0,
    }
}

/// 手动项目的明细 payload（无公告：`stage_groups` 为空 + 统一空态文案）。
pub fn manual_detail(m: &crate::intel_manual::ManualProject) -> ProjectDetailPayload {
    let view = stage_view_of(&[]);
    ProjectDetailPayload {
        schema: PAYLOAD_SCHEMA.to_string(),
        agg_key: m.agg_key.clone(),
        engine_keys: Vec::new(),
        project_name: m.project_name.clone(),
        buyer: m.buyer.clone(),
        area: m.area.clone().unwrap_or_default(),
        budget: m.budget.clone().unwrap_or_default(),
        source_kind: ProjectSourceKind::Manual.code().to_string(),
        source_kind_label: ProjectSourceKind::Manual.label().to_string(),
        current_stage: view.current_segment,
        current_stage_label: view.current_label,
        current_stage_index: view.current_display_index,
        terminated: false,
        latest_announcement_at: String::new(),
        announcement_count: 0,
        merge_basis: if m.agg_basis.is_empty() {
            aggregate_key(&m.buyer, &m.project_name).merge_basis()
        } else {
            m.agg_basis.clone()
        },
        merge_basis_items: vec![format!("手动建档：采购人「{}」+ 项目名「{}」", m.buyer, m.project_name)],
        stage_groups: Vec::new(),
        empty_label: EMPTY_NO_ANNOUNCEMENT.to_string(),
        present_stage_count: 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::intel::IntelItem;
    use crate::intel_store::persist_fetch;

    fn tmp_store(tag: &str) -> Store {
        let dir = std::env::temp_dir().join(format!("wb-payload-{}-{}", tag, std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        Store::open(&dir).expect("open store")
    }

    fn item(site: &str, title: &str, url: &str, date: &str, body: &str, org: &str) -> IntelItem {
        IntelItem {
            key: format!("{site}|{url}"),
            site_id: site.into(),
            site_name: format!("{site}-name"),
            title: title.into(),
            org: org.into(),
            area: "铜陵".into(),
            date: date.into(),
            amount: Some("100万元".into()),
            url: url.into(),
            relevance: Relevance::Core,
            reasons: vec![
                "命中级别:正文复核命中".into(),
                "主题词:无人机".into(),
                "正文复核加分词:植保".into(),
            ],
            incomplete: false,
            manual_verdict: false,
            first_seen: date.into(),
            body_head: body.into(),
            native_code: None,
        }
    }

    /// 列表 payload：六段、每段公告数、最新时间、命中词、相关态、聚合依据齐备。
    #[test]
    fn list_payload_has_all_contract_fields() {
        let store = tmp_store("list");
        persist_fetch(
            &store,
            &[item(
                "pjwnc",
                "综合农事服务中心无人机采购项目招标公告",
                "http://x/1",
                "2026-10-20",
                "项目编号：TLTRZB【2026】074 采购植保无人机",
                "安徽省普济圩现代农业集团",
            )],
            Some("2026-10-20T00:00:00Z"),
        )
        .unwrap();
        persist_fetch(
            &store,
            &[item(
                "pjwnc",
                "综合农事服务中心无人机采购项目中标（成交）结果公告",
                "http://x/2",
                "2026-11-10",
                "项目编号：TLTRZB【2026】074 中标供应商",
                "安徽省普济圩现代农业集团",
            )],
            Some("2026-11-10T00:00:00Z"),
        )
        .unwrap();

        let p = project_list_payload(&store, None, 0).unwrap();
        assert_eq!(p.schema, "intel.project.v1");
        assert_eq!(p.segments.len(), 6, "payload 自带六段定义");
        assert_eq!(p.total, 1, "同一采购人同一项目应合成一张卡");
        let card = &p.projects[0];
        assert_eq!(card.announcement_count, 2);
        assert_eq!(card.current_stage, "award", "最靠后的是中标成交");
        assert_eq!(card.current_stage_index, Some(4));
        assert_eq!(card.latest_announcement_at, "2026-11-10", "最新公告时间");
        assert!(card.hit_words.contains(&"无人机".to_string()), "命中词: {:?}", card.hit_words);
        assert!(card.hit_words.contains(&"植保".to_string()));
        assert_eq!(card.relevance, "core");
        assert_eq!(card.relevance_label, "相关");
        assert!(card.merge_basis.contains("采购人"), "聚合依据: {}", card.merge_basis);
        assert!(card.merge_basis_items.len() >= 3);
        assert_eq!(card.stages.len(), 6);
        let counts: Vec<usize> = card.stages.iter().map(|s| s.count).collect();
        assert_eq!(counts, vec![0, 1, 0, 1, 0, 0], "采购公告 1 条 + 中标成交 1 条");
        assert_eq!(card.stages[0].present, false, "无公告的段 present=false");
        assert_eq!(card.stages[1].present, true);
        assert_eq!(card.source_kind, "engine");
    }

    /// 明细 payload：按段分组、每组下挂条目、含命中依据与原文链接。
    #[test]
    fn detail_payload_groups_by_stage_with_evidence_and_links() {
        let store = tmp_store("detail");
        persist_fetch(
            &store,
            &[item(
                "pjwnc",
                "综合农事服务中心无人机采购项目招标公告",
                "http://x/1",
                "2026-10-20",
                "项目编号：TLTRZB【2026】074",
                "安徽省普济圩现代农业集团",
            )],
            Some("2026-10-20T00:00:00Z"),
        )
        .unwrap();
        let p = project_list_payload(&store, None, 0).unwrap();
        let agg_key = p.projects[0].agg_key.clone();
        let d = project_detail_payload(&store, &agg_key).unwrap().expect("应查到明细");
        assert_eq!(d.agg_key, agg_key);
        assert_eq!(d.announcement_count, 1);
        assert_eq!(d.stage_groups.len(), 1, "只有采购公告一段有公告");
        let g = &d.stage_groups[0];
        assert_eq!(g.code, "tender");
        assert_eq!(g.label, "采购公告");
        assert_eq!(g.count, 1);
        assert_eq!(g.entries.len(), 1);
        let e = &g.entries[0];
        assert_eq!(e.url, "http://x/1", "原文链接必须在 payload 里");
        assert!(e.hit_evidence.iter().any(|h| h.contains("正文复核")), "命中依据: {:?}", e.hit_evidence);
        assert!(e.hit_words.contains(&"植保".to_string()));
        assert_eq!(e.announcement_type, "招标公告");
        assert_eq!(e.stage_source, "rule");
    }

    /// 不同采购人的同名项目在列表里必须是两张卡（规格 §2 硬约束）。
    #[test]
    fn list_keeps_different_buyers_apart() {
        let store = tmp_store("buyers");
        persist_fetch(
            &store,
            &[item("pjwnc", "无人机采购项目招标公告", "http://a/1", "2026-10-20", "编号 T1", "普农集团")],
            Some("2026-10-20T00:00:00Z"),
        )
        .unwrap();
        persist_fetch(
            &store,
            &[item("pjwnc", "无人机采购项目招标公告", "http://a/2", "2026-10-21", "编号 T2", "池州市农业农村局")],
            Some("2026-10-21T00:00:00Z"),
        )
        .unwrap();
        let p = project_list_payload(&store, None, 0).unwrap();
        assert_eq!(p.total, 2, "不同采购人同名项目必须是两张卡");
        let buyers: Vec<&str> = p.projects.iter().map(|c| c.buyer.as_str()).collect();
        assert!(buyers.contains(&"普农集团"));
        assert!(buyers.contains(&"池州市农业农村局"));
    }

    /// 阶段后缀差异要在同一张卡里（规格 §2 归一）。
    #[test]
    fn list_merges_via_normalized_name() {
        let store = tmp_store("norm");
        // 两条无编号公告，靠「采购人 + 项目名归一」才能合成一个项目
        persist_fetch(
            &store,
            &[item("pjwnc", "某村无人机植保服务项目招标公告", "http://n/1", "2026-10-20", "无编号", "普农集团")],
            Some("2026-10-20T00:00:00Z"),
        )
        .unwrap();
        persist_fetch(
            &store,
            &[item("pjwnc", "某村无人机植保服务项目（第2次）中标结果公告", "http://n/2", "2026-11-01", "无编号", "普农集团")],
            Some("2026-11-01T00:00:00Z"),
        )
        .unwrap();
        let p = project_list_payload(&store, None, 0).unwrap();
        assert_eq!(p.total, 1, "归一后应合成一张卡（引擎按指纹会拆成两个项目）");
        let card = &p.projects[0];
        assert_eq!(card.announcement_count, 2);
        assert!(card.merge_basis_items.iter().any(|i| i.contains("第2次")), "归一依据要留痕: {:?}", card.merge_basis_items);
    }

    /// 终止公告：侧标亮、当前阶段停在终止前最后实段。
    #[test]
    fn termination_shows_as_side_state() {
        let store = tmp_store("term");
        persist_fetch(
            &store,
            &[item("pjwnc", "XX无人机采购项目招标公告", "http://t/1", "2026-10-20", "编号 T9", "普农集团")],
            Some("2026-10-20T00:00:00Z"),
        )
        .unwrap();
        persist_fetch(
            &store,
            &[item("pjwnc", "XX无人机采购项目终止公告", "http://t/2", "2026-11-05", "编号 T9", "普农集团")],
            Some("2026-11-05T00:00:00Z"),
        )
        .unwrap();
        let p = project_list_payload(&store, None, 0).unwrap();
        let card = &p.projects[0];
        assert!(card.terminated, "终止侧标要亮");
        assert_eq!(card.current_stage, "tender", "当前阶段停在终止前（采购公告）");
        assert_eq!(card.termination_count, 1);
    }

    /// 非法阶段短码必须报错，不得静默当不过滤。
    #[test]
    fn illegal_stage_filter_errors_out() {
        let store = tmp_store("badfilter");
        assert!(project_list_payload(&store, Some("review"), 0).is_err(), "旧五段短码不是六段口径，应报错");
        assert!(project_list_payload(&store, Some("nonsense"), 0).is_err());
        assert!(project_list_payload(&store, Some(""), 0).is_ok());
    }

    /// 手动建档卡：无公告 → 统一「暂无公告 · 手动建档」，source_kind=manual。
    #[test]
    fn manual_card_has_empty_label() {
        let store = tmp_store("manualcard");
        let d = crate::intel_manual::ManualDraft {
            project_name: "某村无人机植保服务项目".into(),
            buyer: "普农集团".into(),
            area: Some("铜陵".into()),
            ..Default::default()
        };
        let m = match crate::intel_manual::create_manual(&store, &d, false, &[], Some("2026-11-23T09:00:00Z")).unwrap() {
            crate::intel_manual::CreateOutcome::Created(m) => m,
            _ => panic!(),
        };
        let card = manual_card(&m);
        assert_eq!(card.source_kind, "manual");
        assert_eq!(card.current_stage, "none");
        assert_eq!(card.current_stage_label, "暂无公告 · 手动建档");
        assert_eq!(card.announcement_count, 0);
        assert_eq!(card.area, "铜陵");

        let detail = manual_detail(&m);
        assert!(detail.stage_groups.is_empty(), "无公告则无段组");
        assert_eq!(detail.empty_label, "暂无公告 · 手动建档");
        assert_eq!(detail.present_stage_count, 0);
    }

    /// 引擎项目键仍可作为明细入参（保持 CLI/Tauri 既有用法）。
    #[test]
    fn detail_accepts_engine_project_key() {
        let store = tmp_store("enginekey");
        persist_fetch(
            &store,
            &[item(
                "pjwnc",
                "XX无人机采购项目招标公告",
                "http://e/1",
                "2026-10-20",
                "项目编号：TLTRZB【2026】074",
                "普农集团",
            )],
            Some("2026-10-20T00:00:00Z"),
        )
        .unwrap();
        let d = project_detail_payload(&store, "code:tltrzb2026074").unwrap().expect("引擎键应能查到");
        assert_eq!(d.announcement_count, 1);
        assert_eq!(d.buyer, "普农集团");
    }
}
