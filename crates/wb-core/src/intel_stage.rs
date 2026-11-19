//! v3.2.0 情报「项目阶段」判据（排期 §四 T3 轨「阶段规则」，PRD §5.4 / §5.5）。
//!
//! ## 两条路线与选择
//!
//! - **A · 文本抽取**：从公告标题（+正文头部）按公告类型词判阶段。逐条可解释、与 PRD §5.5
//!   「判定线索（标题/公告类型常见写法）」一列一一对应，判错时能定位到命中的是哪个词。
//! - **B · 启发式分期**：不新增字段，用「项目编号 + 首次出现时间 + 公告序号」推阶段。
//!   零解析成本，但**公告序号依赖抓取分页顺序**（翻页顺序变即结论变），且无法区分
//!   「意向」与「评审」——两者都可能出现在任何一天。
//!
//! **落地选择：A 为主、B 只做兜底（PRD §5.4 明确允许叠加）**。理由三条：
//! 1. A 的每一条判定都能回溯到具体词，B 的判定无法自证，出错时用户改判也无从下手；
//! 2. B 的核心输入「公告序号」在现有抓取链路里不稳定（`intel::fetch_epoint` 按页取、
//!    站点按 `webdate` 排序，补抓/重抓会改变批次内相对顺序），拿它当主判据会把
//!    「阶段」变成依赖抓取顺序的变量，与「进程合一」要求的可复核性相悖；
//! 3. A 判不出的部分（企业官网、聚合站标题不规范）正是 PRD 说 B 适用的场景——
//!    所以 B 保留为 A 未知时的兜底，而不是替代。
//!
//! 判据强度分三级，落库时写进 `stage_source`，界面按此决定是否挂「待确认」标记：
//! `rule`（A 命中）> `heuristic`（B 兜底）> `unknown`（两者都判不出，阶段未知）。

use serde::{Deserialize, Serialize};

/// 阶段。序号即进程顺序（`Intent` 最早、`Operation` 最晚）。
///
/// 枚举来自 PRD §5.5 与任务书：意向 / 签约 / 评审 / 验收 / 运维，另加「阶段未知」。
/// **签名顺序与序号顺序不同**：政府采购链路是 意向 → 评审（招标）→ 签约（中标/合同）
/// → 验收 → 运维，因此 `order()` 显式给出，不依赖 enum 声明次序。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Stage {
    Intent,
    Review,
    Contract,
    Acceptance,
    Operation,
    Unknown,
}

impl Stage {
    /// 中文标签（界面与报告直接使用，避免各处再拼一次）。
    pub fn label(self) -> &'static str {
        match self {
            Stage::Intent => "意向",
            Stage::Review => "评审",
            Stage::Contract => "签约",
            Stage::Acceptance => "验收",
            Stage::Operation => "运维",
            Stage::Unknown => "阶段未知",
        }
    }

    /// 进程序号。`Unknown` 返回 `None`——它不是一个「段」，而是一个待确认标记
    /// （PRD §5.5：阶段未知「是（挂『待确认』标记）」）。
    pub fn order(self) -> Option<u8> {
        match self {
            Stage::Intent => Some(0),
            Stage::Review => Some(1),
            Stage::Contract => Some(2),
            Stage::Acceptance => Some(3),
            Stage::Operation => Some(4),
            Stage::Unknown => None,
        }
    }

    /// 稳定短码，落进 `ext_record.stage` 列（列窄，不存中文）。
    pub fn code(self) -> &'static str {
        match self {
            Stage::Intent => "intent",
            Stage::Review => "review",
            Stage::Contract => "contract",
            Stage::Acceptance => "acceptance",
            Stage::Operation => "operation",
            Stage::Unknown => "unknown",
        }
    }

    pub fn from_code(s: &str) -> Stage {
        match s {
            "intent" => Stage::Intent,
            "review" => Stage::Review,
            "contract" => Stage::Contract,
            "acceptance" => Stage::Acceptance,
            "operation" => Stage::Operation,
            _ => Stage::Unknown,
        }
    }
}

/// 判据来源。落 `ext_record.stage_source`，供聚合视图决定是否挂「待确认」。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StageSource {
    /// A 路线：标题/正文命中公告类型词
    Rule,
    /// B 路线：按项目内首末公告位置启发式分期
    Heuristic,
    /// 人工改判（PRD §5.6「人工改判」）
    Manual,
    /// 判不出
    Unknown,
}

impl StageSource {
    pub fn code(self) -> &'static str {
        match self {
            StageSource::Rule => "rule",
            StageSource::Heuristic => "heuristic",
            StageSource::Manual => "manual",
            StageSource::Unknown => "unknown",
        }
    }
    pub fn from_code(s: &str) -> StageSource {
        match s {
            "rule" => StageSource::Rule,
            "heuristic" => StageSource::Heuristic,
            "manual" => StageSource::Manual,
            _ => StageSource::Unknown,
        }
    }
}

/// A 路线词表：按「优先级从高到低」排列，**先匹配更具体的词**。
///
/// 顺序不能随意：`中标（成交）结果公告` 同时含「公告」与「中标」，若把宽泛词放前面
/// 会被判成评审；`采购意向公告` 同时含「采购」与「意向」，同理必须先判意向。
/// 每一行都对应 PRD §5.5「判定线索」列的常见写法。
const RULES: &[(Stage, &[&str])] = &[
    (
        Stage::Operation,
        &["运维服务", "运维公告", "质保", "维保", "售后服务", "运营服务"],
    ),
    (
        Stage::Acceptance,
        &["履约验收", "验收公告", "验收结果", "项目验收", "竣工验收", "交付验收"],
    ),
    (
        Stage::Contract,
        &[
            "中标（成交）结果", "中标(成交)结果", "中标结果", "成交结果", "中标公告",
            "成交公告", "中选结果", "合同公告", "合同公示", "签约", "中选公告",
            // 「中标通知书」是签约环节的正式文书，只含「中标」二字，必须单列
            "中标通知书",
        ],
    ),
    (
        Stage::Intent,
        &["采购意向", "意向公开", "意向公告", "需求公示", "采购需求"],
    ),
    (
        Stage::Review,
        &[
            "招标公告", "采购公告", "竞争性磋商", "竞争性谈判", "询价公告", "单一来源",
            "资格预审", "招标文件", "澄清", "更正公告", "变更公告", "答疑", "磋商公告",
            "谈判公告",
        ],
    ),
];

/// A 路线：从标题（+正文头部）抽阶段。**命中即返回第一个（最具体）词**。
///
/// 只看标题优先级高于正文：标题是发布方给的公告类型，正文里「中标」常出现在
/// 「本项目将依法确定中标人」这类说明句中，拿正文判会把招标公告误判成签约。
/// 正文只在标题完全判不出时才作为补充（企业官网标题常只有项目名没有类型词）。
pub fn stage_from_text(title: &str, body_head: &str) -> Option<(Stage, &'static str)> {
    for (stage, words) in RULES.iter() {
        for w in words.iter() {
            if title.contains(*w) {
                return Some((*stage, *w));
            }
        }
    }
    // 标题判不出时退到正文头部（限 300 字：正文越长越容易撞上说明句）
    let head: String = body_head.chars().take(300).collect();
    for (stage, words) in RULES.iter() {
        for w in words.iter() {
            if head.contains(*w) {
                return Some((*stage, *w));
            }
        }
    }
    None
}

/// 判定结果：阶段 + 来源 + 依据（依据留空串表示无）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StageVerdict {
    pub stage: Stage,
    pub source: StageSource,
    /// 命中词（`rule` / `heuristic` 时给出，供界面「为什么是这个阶段」）
    pub reason: String,
}

/// B 路线兜底：给定「该项目内所有公告按期排序后的位置」，推阶段。
///
/// 规则（PRD §5.4 B「项目编号 + 首次出现时间 + 公告序号」）：
/// - 位置 0（项目最早的一条）→ 评审（招标/采购启动这一步在交易平台最早出现）
/// - 位置末位且总条数 ≥2 → 签约（链路末端在交易平台通常是结果公告）
/// - 其余 → 未知
///
/// **这条规则只在 A 判不出时使用**；它自己也会判错（例如项目最早一条其实是意向公开），
/// 所以落库标 `heuristic`，界面必须挂「待确认」，允许人工改判。
pub fn stage_heuristic(index: usize, total: usize) -> StageVerdict {
    if total >= 2 && index + 1 == total {
        StageVerdict {
            stage: Stage::Contract,
            source: StageSource::Heuristic,
            reason: "项目内末位公告，按链路末端推定为签约".into(),
        }
    } else if index == 0 {
        StageVerdict {
            stage: Stage::Review,
            source: StageSource::Heuristic,
            reason: "项目内首条公告，按链路起点推定为评审".into(),
        }
    } else {
        StageVerdict {
            stage: Stage::Unknown,
            source: StageSource::Unknown,
            reason: String::new(),
        }
    }
}

/// 单条记录的最终判据：A 优先，判不出再交给 B（由调用方给出项目内位置）。
pub fn judge(title: &str, body_head: &str, index: usize, total: usize) -> StageVerdict {
    match stage_from_text(title, body_head) {
        Some((stage, w)) => StageVerdict {
            stage,
            source: StageSource::Rule,
            reason: format!("标题/正文命中「{}」", w),
        },
        None => stage_heuristic(index, total),
    }
}

/// 公告签名：用于区分「同一项目的**不同**公告」与「同一条公告的跨源转载」。
///
/// 组成 = 阶段码 + 发布日期。同一公告的转载副本同阶段同日期 → 签名相同 → 应合并；
/// 招标公告与中标公告阶段不同 → 签名不同 → 必须各成一条（否则时间轴只剩一个节点，
/// 这正是本轮实测发现的既有缺陷：`probe_l3` 里两条公告落库后 `current` 只有 1 行）。
///
/// 日期参与签名是必要的：同一项目可能发多次「更正公告」，它们同阶段但不同日期，
/// 是真正独立的时间轴节点，不该被合并掉。
pub fn announcement_signature(stage: Stage, date: &str) -> String {
    format!("{}|{}", stage.code(), date.trim())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn intent_is_detected_before_review() {
        // 「采购意向公告」同时含「采购」，若宽泛词先匹配会误判成评审
        let v = judge("XX单位无人机采购意向公告", "", 0, 2);
        assert_eq!(v.stage, Stage::Intent);
        assert_eq!(v.source, StageSource::Rule);
        assert!(v.reason.contains("采购意向"), "依据要落到具体词: {}", v.reason);
    }

    #[test]
    fn contract_result_is_not_misread_as_review() {
        // 「中标（成交）结果公告」同时含「公告」，必须先命中更具体的结果词
        let v = judge("普农集团无人机采购项目中标（成交）结果公告", "", 1, 2);
        assert_eq!(v.stage, Stage::Contract, "结果公告应判签约，不能落评审");
        assert!(v.reason.contains("中标"));
    }

    #[test]
    fn body_is_only_used_when_title_has_no_type_word() {
        // 企业官网标题常只有项目名；正文有公告类型
        let t = "综合农事服务中心无人机采购项目";
        let b = "一、项目基本情况 …… 现发布招标公告，欢迎符合条件的供应商参加";
        let v = judge(t, b, 0, 2);
        assert_eq!(v.stage, Stage::Review);
        assert_eq!(v.source, StageSource::Rule);
    }

    #[test]
    fn body_sentence_with_zhongbiao_does_not_flip_review_to_contract() {
        // 标题已判出评审；正文里「依法确定中标人」不得把阶段翻成签约
        let v = judge("XX项目招标公告", "本项目将依法确定中标人，现进行公开招标", 0, 2);
        assert_eq!(v.stage, Stage::Review, "标题优先于正文");
    }

    #[test]
    fn unknown_falls_back_to_heuristic_and_is_marked() {
        let v = judge("XX项目情况说明", "", 0, 3);
        assert_eq!(v.stage, Stage::Review, "首条按 B 兜底为评审");
        assert_eq!(v.source, StageSource::Heuristic, "兜底必须标 heuristic 供界面挂待确认");
        let last = judge("XX项目情况说明", "", 2, 3);
        assert_eq!(last.stage, Stage::Contract, "末位按 B 兜底为签约");
    }

    #[test]
    fn single_record_project_does_not_invent_last_stage() {
        // 只有一条时不能同时判「首条=评审」和「末位=签约」
        let v = judge("XX项目情况说明", "", 0, 1);
        assert_eq!(v.stage, Stage::Review);
        assert_eq!(v.source, StageSource::Heuristic);
    }

    #[test]
    fn signature_separates_stages_and_keeps_reprints_together() {
        let a = announcement_signature(Stage::Review, "2026-10-20");
        let b = announcement_signature(Stage::Contract, "2026-11-10");
        assert_ne!(a, b, "招标/中标必须不同签名（否则被合并成一个节点）");
        // 同阶段同日期 = 跨源转载副本 → 签名相同
        assert_eq!(
            announcement_signature(Stage::Review, "2026-10-20"),
            announcement_signature(Stage::Review, " 2026-10-20 ")
        );
        // 同阶段的多次更正公告（不同日期）是独立节点
        assert_ne!(
            announcement_signature(Stage::Review, "2026-10-20"),
            announcement_signature(Stage::Review, "2026-10-25")
        );
    }

    #[test]
    fn order_is_chain_order_not_enum_order() {
        assert_eq!(Stage::Intent.order(), Some(0));
        assert_eq!(Stage::Review.order(), Some(1));
        assert_eq!(Stage::Contract.order(), Some(2));
        assert_eq!(Stage::Acceptance.order(), Some(3));
        assert_eq!(Stage::Operation.order(), Some(4));
        assert_eq!(Stage::Unknown.order(), None, "未知不是进程段");
    }
}
