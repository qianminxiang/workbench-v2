//! 六段阶段映射（设计规格 v1.2 §2 的产品侧分组）。
//!
//! 规格把公告类型归成六段：① 意向/需求 ② 采购公告 ③ 更正/澄清 ④ 中标成交
//! ⑤ 合同/验收 ⑥ 终止/废标。这是**产品侧分组，不是法规分类**——与既有
//! [`crate::intel_stage::Stage`] 的五段（意向/评审/签约/验收/运维）不是同一套口径：
//!
//! | 规格六段 | 既有五段 | 差异 |
//! |---|---|---|
//! | 意向/需求 | Intent | 一致 |
//! | 采购公告 + 更正/澄清 | Review（两者同属 Review） | 规格把更正/澄清单列一段 |
//! | 中标成交 | Contract | 一致 |
//! | 合同/验收 | Acceptance + Operation | 规格把「合同公告」与「验收」并成一段 |
//! | 终止/废标 | **无对应** | 规格新增，且是**侧状态不推进** |
//!
//! 为什么另开一套而不改 `intel_stage::Stage`：那套已经落库到 `ext_record.stage`
//! （v4 迁移）、被 8 个 `project_timeline.rs` 测试与 CLI/Tauri 两端锁住，改它会
//! 连带改数据迁移与既有消费方。本模块只在**查询期**做一次映射，不写库、不改旧枚举。
//! 两者是「存储口径 → 展示口径」的关系，不是互相替代。
//!
//! ## 终止/废标是侧状态（规格 §2 硬性要求）
//!
//! 「项目当前阶段 = 已出现公告中最靠后的那一段；『终止/废标』是侧状态，不参与进程推进
//! （出现时在卡上加一枚红色侧标，进程停在终止前的最后实段）」。
//! [`ProjectStageView::current_segment`] 因此**只看可推进的五段**，
//! 终止段单独用 `terminated` / `termination_segments` 表达。

use serde::{Deserialize, Serialize};

use crate::intel_stage::{Stage, StageSource};

/// 规格 §2 的六段。序号即产品侧展示顺序（不含侧状态语义）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Segment {
    /// ① 意向 / 需求
    Intent,
    /// ② 采购公告
    Tender,
    /// ③ 更正 / 澄清
    Correction,
    /// ④ 中标成交
    Award,
    /// ⑤ 合同 / 验收
    Contract,
    /// ⑥ 终止 / 废标（侧状态）
    Termination,
}

impl Segment {
    /// 六段固定顺序（阶段点条按这个画）。
    pub const ALL: [Segment; 6] = [
        Segment::Intent,
        Segment::Tender,
        Segment::Correction,
        Segment::Award,
        Segment::Contract,
        Segment::Termination,
    ];

    /// 稳定短码（payload 里用它，界面翻译成标签；改文案不必改数据）。
    pub fn code(self) -> &'static str {
        match self {
            Segment::Intent => "intent",
            Segment::Tender => "tender",
            Segment::Correction => "correction",
            Segment::Award => "award",
            Segment::Contract => "contract",
            Segment::Termination => "termination",
        }
    }

    /// 中文标签（规格 §2 表格原文）。
    pub fn label(self) -> &'static str {
        match self {
            Segment::Intent => "意向/需求",
            Segment::Tender => "采购公告",
            Segment::Correction => "更正/澄清",
            Segment::Award => "中标成交",
            Segment::Contract => "合同/验收",
            Segment::Termination => "终止/废标",
        }
    }

    /// 展示序号（1 起，阶段点条用）。
    pub fn display_index(self) -> u8 {
        match self {
            Segment::Intent => 1,
            Segment::Tender => 2,
            Segment::Correction => 3,
            Segment::Award => 4,
            Segment::Contract => 5,
            Segment::Termination => 6,
        }
    }

    /// 是否参与「当前阶段」推进。**只有终止段为 false**。
    pub fn advances(self) -> bool {
        !matches!(self, Segment::Termination)
    }

    /// 推进序号：仅对可推进的五段给出 0..4，终止段为 `None`
    /// （它不占进程位，只作侧标）。跳段计算与「停在终止前最后实段」都靠它。
    pub fn order(self) -> Option<u8> {
        match self {
            Segment::Intent => Some(0),
            Segment::Tender => Some(1),
            Segment::Correction => Some(2),
            Segment::Award => Some(3),
            Segment::Contract => Some(4),
            Segment::Termination => None,
        }
    }

    pub fn from_code(s: &str) -> Option<Segment> {
        match s {
            "intent" => Some(Segment::Intent),
            "tender" => Some(Segment::Tender),
            "correction" => Some(Segment::Correction),
            "award" => Some(Segment::Award),
            "contract" => Some(Segment::Contract),
            "termination" => Some(Segment::Termination),
            _ => None,
        }
    }
}

/// 既有五段 → 规格六段的映射。
///
/// 一处必须解释的分歧：旧枚举把「更正公告 / 变更公告 / 澄清 / 答疑」都判进
/// `Review`（与招标公告同段）。规格要求它们单列为第 3 段，但在**存量数据**里
/// 这条信息已经丢了（`ext_record.stage = 'review'` 分不出是招标还是更正）。
/// 因此这里的映射是**兜底口径**：`Review → 采购公告`，更正段由
/// [`segment_of_announcement`] 在查询期按 `stage_reason`（判据命中词）与标题
/// **重判**——旧枚举丢掉的区分，用判据原文补回来。
pub fn segment_of_stage(stage: Stage) -> Segment {
    match stage {
        Stage::Intent => Segment::Intent,
        Stage::Review => Segment::Tender,
        Stage::Contract => Segment::Award,
        Stage::Acceptance => Segment::Contract,
        Stage::Operation => Segment::Contract,
        Stage::Unknown => Segment::Tender,
    }
}

/// 更正/澄清段的判据词（规格 §2 第 3 段：更正公告、延期公告、澄清答疑）。
const CORRECTION_WORDS: &[&str] = &[
    "更正", "变更公告", "澄清", "答疑", "延期", "补充公告", "修正",
];

/// 终止/废标段的判据词（规格 §2 第 6 段：终止公告、废标公告、流标公告）。
const TERMINATION_WORDS: &[&str] = &["终止", "废标", "流标", "取消采购", "撤回"];

/// 判一段公告属于哪一段：**优先用落库的判据词**（`stage_reason` 里那个命中词），
/// 判据词不够时退回标题原文匹配，最后退回旧阶段枚举的兜底映射。
///
/// 为什么先看判据词而不是标题：`stage_reason` 是落库当时**实际命中的那个词**
/// （如「标题/正文命中「更正公告」」），它比事后重新扫标题更贴近原始判定；
/// 且人工改判（`stage_source = manual`）写下的说明也在同一字段，重判会与人工意图冲突。
pub fn segment_of_announcement(
    old_stage: Stage,
    stage_source: StageSource,
    stage_reason: &str,
    title: &str,
) -> Segment {
    // 人工改判优先：用户已明确改了阶段，不再由标题二次翻案
    if stage_source == StageSource::Manual {
        return segment_of_stage(old_stage);
    }
    let hit = |hay: &str, words: &[&str]| words.iter().any(|w| hay.contains(*w));
    if hit(stage_reason, TERMINATION_WORDS) || hit(title, TERMINATION_WORDS) {
        return Segment::Termination;
    }
    // 更正/澄清只可能从 Review 里拆出来（其它阶段没有更正公告的语义）
    if old_stage == Stage::Review
        && (hit(stage_reason, CORRECTION_WORDS) || hit(title, CORRECTION_WORDS))
    {
        return Segment::Correction;
    }
    segment_of_stage(old_stage)
}

/// 某一段在某项目里「出现了几条公告」的聚合结果。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SegmentBucket {
    pub code: String,
    pub label: String,
    pub display_index: u8,
    /// 该段公告条数（**只统计真实存在的公告**；0 表示该段没有公告，
    /// 界面据 `present = false` 直接跳过该点，不画成「0 条」）
    pub count: usize,
    /// 是否有公告。规格 §2：某段没有任何公告时该点直接跳过。
    pub present: bool,
    /// 该段最近一条公告的日期（无公告时为空）
    pub latest_date: String,
}

/// 一个项目的阶段视图（规格 §2 + §2 侧状态规则）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectStageView {
    /// 六段桶，顺序固定为 [`Segment::ALL`]，便于界面直接按序渲染。
    pub buckets: Vec<SegmentBucket>,
    /// 当前阶段短码：可推进五段里最靠后的那一段；无任何可推进公告时为 `none`。
    pub current_segment: String,
    pub current_label: String,
    /// 当前阶段展示序号（1..5）；无当前阶段为 `None`。
    pub current_display_index: Option<u8>,
    /// 是否出现终止/废标（侧状态，红色侧标用）
    pub terminated: bool,
    /// 终止/废标公告条数
    pub termination_count: usize,
    /// 终止公告的最近日期（无则空）
    pub termination_date: String,
    /// 进程停在「终止前的最后实段」——与 `current_segment` 同值，
    /// 单独给出是为了让界面不必自己推断「终止是否把它拦下来了」。
    pub stopped_at: String,
    /// 已出现公告里跳过的可推段数（0→3 之间缺的段数）
    pub skipped: usize,
    /// 无公告时的统一文案（规格 §2：手动项目无公告统一「暂无公告 · 手动建档」）
    pub empty_label: String,
}

/// 无公告项目的统一文案（规格 §2 原文）。
pub const EMPTY_NO_ANNOUNCEMENT: &str = "暂无公告 · 手动建档";

impl ProjectStageView {
    fn empty() -> ProjectStageView {
        ProjectStageView {
            buckets: Segment::ALL
                .iter()
                .map(|s| SegmentBucket {
                    code: s.code().to_string(),
                    label: s.label().to_string(),
                    display_index: s.display_index(),
                    count: 0,
                    present: false,
                    latest_date: String::new(),
                })
                .collect(),
            current_segment: "none".to_string(),
            current_label: EMPTY_NO_ANNOUNCEMENT.to_string(),
            current_display_index: None,
            terminated: false,
            termination_count: 0,
            termination_date: String::new(),
            stopped_at: "none".to_string(),
            skipped: 0,
            empty_label: EMPTY_NO_ANNOUNCEMENT.to_string(),
        }
    }
}

/// 由「每条公告的阶段结论」算出项目级阶段视图。
///
/// 输入是 `(段, 日期)` 序列（一条公告一项）；顺序无关，函数内部自己排序比较。
pub fn stage_view_of(announcements: &[(Segment, String)]) -> ProjectStageView {
    if announcements.is_empty() {
        return ProjectStageView::empty();
    }
    let mut view = ProjectStageView::empty();
    for (seg, date) in announcements {
        let idx = Segment::ALL.iter().position(|s| *s == *seg).unwrap_or(0);
        let b = &mut view.buckets[idx];
        b.count += 1;
        b.present = true;
        if b.latest_date.is_empty() || date.as_str() > b.latest_date.as_str() {
            b.latest_date = date.clone();
        }
    }
    view.termination_count = view.buckets[5].count;
    view.termination_date = view.buckets[5].latest_date.clone();
    view.terminated = view.termination_count > 0;

    // 当前阶段 = 可推进五段里最靠后的那一段（终止段永不参与）
    let best = view.buckets[..5]
        .iter()
        .enumerate()
        .filter(|(_, b)| b.present)
        .map(|(i, b)| (i, b))
        .max_by_key(|(i, _)| *i);
    match best {
        Some((i, _b)) => {
            let seg = Segment::ALL[i];
            view.current_segment = seg.code().to_string();
            view.current_label = seg.label().to_string();
            view.current_display_index = Some(seg.display_index());
            view.stopped_at = seg.code().to_string();
            let present: std::collections::BTreeSet<u8> =
                view.buckets[..5].iter().filter(|b| b.present).map(|b| b.display_index).collect();
            view.skipped = (1..=seg.display_index()).filter(|i| !present.contains(i)).count();
        }
        None => {
            // 只有终止公告、没有任何可推进公告：当前阶段为「无」，
            // 文案仍走统一空态——它没有「进程」可言，不能把终止段当当前阶段。
            view.current_segment = "none".to_string();
            view.current_label = EMPTY_NO_ANNOUNCEMENT.to_string();
            view.current_display_index = None;
            view.stopped_at = "none".to_string();
        }
    }
    view
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 六段顺序与标签必须与规格 §2 表格逐字对应。
    #[test]
    fn six_segments_match_spec_table() {
        let got: Vec<(&str, &str, u8)> = Segment::ALL
            .iter()
            .map(|s| (s.code(), s.label(), s.display_index()))
            .collect();
        assert_eq!(
            got,
            vec![
                ("intent", "意向/需求", 1),
                ("tender", "采购公告", 2),
                ("correction", "更正/澄清", 3),
                ("award", "中标成交", 4),
                ("contract", "合同/验收", 5),
                ("termination", "终止/废标", 6),
            ]
        );
        // 短码必须可逆（payload 里只传短码，界面据表翻标签）
        for s in Segment::ALL {
            assert_eq!(Segment::from_code(s.code()), Some(s));
        }
    }

    /// 终止/废标是侧状态：不参与推进。
    #[test]
    fn termination_does_not_advance() {
        assert_eq!(Segment::Termination.order(), None, "终止段不占进程位");
        assert!(!Segment::Termination.advances());
        for s in [Segment::Intent, Segment::Tender, Segment::Correction, Segment::Award, Segment::Contract] {
            assert!(s.advances());
            assert!(s.order().is_some());
        }
    }

    /// 规格 §2 核心：当前阶段 = 最靠后的一段；终止出现时进程停在终止前的最后实段。
    #[test]
    fn current_stage_is_furthest_and_termination_is_side() {
        let anns = vec![
            (Segment::Intent, "2026-09-01".to_string()),
            (Segment::Tender, "2026-10-20".to_string()),
            (Segment::Termination, "2026-11-05".to_string()),
        ];
        let v = stage_view_of(&anns);
        assert_eq!(v.current_segment, "tender", "当前阶段应为采购公告，不是终止");
        assert_eq!(v.stopped_at, "tender", "进程停在终止前最后实段");
        assert!(v.terminated, "终止侧标要亮");
        assert_eq!(v.termination_count, 1);
        assert_eq!(v.current_display_index, Some(2), "展示序号仍是第 2 段");
    }

    /// 全链路项目：六个点都有公告时当前阶段是合同/验收（第 5 段）。
    #[test]
    fn full_chain_current_stage_is_contract_segment() {
        let anns = vec![
            (Segment::Intent, "2026-06-01".to_string()),
            (Segment::Tender, "2026-07-01".to_string()),
            (Segment::Correction, "2026-07-15".to_string()),
            (Segment::Award, "2026-08-01".to_string()),
            (Segment::Contract, "2026-09-01".to_string()),
        ];
        let v = stage_view_of(&anns);
        assert_eq!(v.current_segment, "contract");
        assert_eq!(v.current_display_index, Some(5));
        assert!(!v.terminated);
        assert_eq!(v.skipped, 0, "六段里的 5 个可推段全覆盖，无跳段");
        // 每段公告数要对
        let counts: Vec<usize> = v.buckets.iter().map(|b| b.count).collect();
        assert_eq!(counts, vec![1, 1, 1, 1, 1, 0]);
    }

    /// 某段没有公告：present=false 且 count=0，界面据此跳过该点（不画成「0 条」）。
    #[test]
    fn absent_segment_is_flagged_not_drawn() {
        let anns = vec![
            (Segment::Tender, "2026-10-20".to_string()),
            (Segment::Award, "2026-11-10".to_string()),
        ];
        let v = stage_view_of(&anns);
        assert_eq!(v.buckets[0].present, false, "意向段无公告");
        assert_eq!(v.buckets[0].count, 0);
        assert_eq!(v.buckets[2].present, false, "更正段无公告");
        assert_eq!(v.buckets[3].present, true);
        assert_eq!(v.skipped, 2, "第 2 段到第 4 段之间缺 1、3 两段");
    }

    /// 同段多条公告：计数累加，latest_date 取最晚。
    #[test]
    fn same_segment_multiple_announcements_count_up() {
        let anns = vec![
            (Segment::Correction, "2026-07-01".to_string()),
            (Segment::Correction, "2026-07-20".to_string()),
            (Segment::Correction, "2026-07-10".to_string()),
        ];
        let v = stage_view_of(&anns);
        assert_eq!(v.buckets[2].count, 3, "更正段三条");
        assert_eq!(v.buckets[2].latest_date, "2026-07-20", "取最晚日期");
        assert_eq!(v.current_segment, "correction");
    }

    /// 无公告（手动建档）：统一「暂无公告 · 手动建档」，且没有任何当前阶段。
    #[test]
    fn no_announcement_gets_unified_label() {
        let v = stage_view_of(&[]);
        assert_eq!(v.current_segment, "none");
        assert_eq!(v.current_label, "暂无公告 · 手动建档");
        assert_eq!(v.empty_label, EMPTY_NO_ANNOUNCEMENT);
        assert_eq!(v.current_display_index, None);
        assert!(!v.terminated);
        for b in &v.buckets {
            assert_eq!(b.present, false);
            assert_eq!(b.count, 0);
        }
    }

    /// 只有终止公告、没有可推进公告：进程为「无」，不能把终止段当当前阶段。
    #[test]
    fn termination_only_project_has_no_current_stage() {
        let anns = vec![(Segment::Termination, "2026-11-05".to_string())];
        let v = stage_view_of(&anns);
        assert_eq!(v.current_segment, "none", "终止不推进，不能当当前阶段");
        assert!(v.terminated);
        assert_eq!(v.current_label, EMPTY_NO_ANNOUNCEMENT);
    }

    /// 旧五段 → 六段兜底映射。
    #[test]
    fn legacy_stage_maps_to_segment() {
        assert_eq!(segment_of_stage(Stage::Intent), Segment::Intent);
        assert_eq!(segment_of_stage(Stage::Review), Segment::Tender);
        assert_eq!(segment_of_stage(Stage::Contract), Segment::Award);
        assert_eq!(segment_of_stage(Stage::Acceptance), Segment::Contract);
        assert_eq!(segment_of_stage(Stage::Operation), Segment::Contract);
    }

    /// 更正/澄清要从旧 Review 里拆出来（靠判据词/标题补回旧枚举丢掉的区分）。
    #[test]
    fn correction_is_split_out_of_review() {
        let seg = segment_of_announcement(
            Stage::Review,
            StageSource::Rule,
            "标题/正文命中「更正公告」",
            "XX无人机采购项目更正公告",
        );
        assert_eq!(seg, Segment::Correction, "判据词命中更正，应归第 3 段");

        // 招标公告即便判据写 Review，也不得误判成更正
        let seg2 = segment_of_announcement(
            Stage::Review,
            StageSource::Rule,
            "标题/正文命中「招标公告」",
            "XX无人机采购项目招标公告",
        );
        assert_eq!(seg2, Segment::Tender);
    }

    /// 终止/废标要从任意旧阶段里识别出来，并归第 6 段（侧状态）。
    #[test]
    fn termination_detected_from_text() {
        for title in ["XX项目终止公告", "XX项目废标公告", "XX项目流标公告"] {
            let seg = segment_of_announcement(Stage::Review, StageSource::Rule, "", title);
            assert_eq!(seg, Segment::Termination, "「{title}」应归终止段");
        }
    }

    /// 人工改判不被标题翻案：用户已定阶段，重判必须服从。
    #[test]
    fn manual_override_is_respected() {
        let seg = segment_of_announcement(
            Stage::Contract,
            StageSource::Manual,
            "人工改判：按开标记录定为中标",
            "XX项目更正公告",
        );
        assert_eq!(seg, Segment::Award, "人工改判优先，标题里的「更正」不得翻案");
    }
}
