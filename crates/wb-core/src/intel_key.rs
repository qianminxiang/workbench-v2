//! 情报「同一项目」聚合键（设计规格 v1.2 §2 的产品口径）。
//!
//! 规格原文：聚合键 =「采购人 + 项目名称归一化」（剔除「（第 N 次）」「重新招标」
//! 「废标重招」等后缀与全半角差异）；不同采购人的同名项目不合并；页面上必须显示
//! 合并依据。本模块只负责**键与依据**，不碰存储、不碰展示。
//!
//! ## 与既有「项目编号键」的关系（两者不是一回事，不可互相替代）
//!
//! v3.2.0 已有的聚合是按 `lead_dedup` 的 `biz_key`（`code:<归一编号>` /
//! `fp:<标题>:<日期>` / `body:<正文指纹>`）分组。那个键回答的是
//! 「**同一条公告**的跨源转载要不要合并」（记录粒度）；
//! 本模块的键回答的是「**同一个标的**的历次公告要不要归到同一张项目卡」（项目粒度）。
//!
//! 编号键不能替代聚合键：企业官网与聚合站大量公告没有项目编号（实测缺失时退回
//! `fp:` 指纹键，同一项目的招标与中标会落成两个项目）。反过来聚合键也不能替代
//! 编号键：采购人与标的名称都相同、但确实是两个独立标段的场景存在（同名不同包），
//! 那要靠 `code:` 键区分。因此两层键**并存**，各管一段。
//!
//! ## 归一化的克制
//!
//! 只剔除**枚举过**的后缀与折叠全半角，不做「智能去重」。理由是可解释性与误合并
//! 代价不对称：多剔一个词，就可能把两个真实不同的项目并成一张卡，而页面上给出的
//! 合并依据（[`AggKey::merge_basis`]）必须能逐项解释「本条为什么归到这个项目」。
//! 具体克制点：
//! - 裸「二次」**不剔**（`二次供水改造`、`二次设备采购` 里它是标的的一部分，不是次数）；
//!   只有「（二次）」这种括号形式、「二次招标」「二次采购」这类带动作词的才剔。
//! - 采购人只做全半角/标点/空白归一，**不剥「有限公司」等主体类型词**——那会把
//!   同一集团下名称相近的不同主体并到一起。

use serde::{Deserialize, Serialize};

/// 全角 ASCII（U+FF01..U+FF5E）转半角，全角空格 U+3000 转普通空格。
///
/// 只做字符级映射，不按字节切：中文标点与全角括号混排时按字节处理会切坏。
pub fn to_halfwidth(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        if ch == '\u{3000}' {
            out.push(' ');
        } else if ('\u{FF01}'..='\u{FF5E}').contains(&ch) {
            out.push(char::from_u32(ch as u32 - 0xFEE0).unwrap_or(ch));
        } else {
            out.push(ch);
        }
    }
    out
}

/// 次数/版本后缀（**整词**剔除，带动作词才剔）。
///
/// 刻意不含裸「二次」「二次供水」类词：`二` 与 `次` 在真实标的名里常见。
const VERSION_WORDS: &[&str] = &[
    "重新招标",
    "重新采购",
    "重新发布",
    "重新组织",
    "废标重招",
    "流标重招",
    "二次招标",
    "二次采购",
    "再次招标",
    "再次采购",
    "第三次招标",
    "第二次招标",
];

/// 括号内可直接判定为「次数/版本」标记的词（不含数字的固定写法）。
const BRACKET_VERSION_WORDS: &[&str] = &["二次", "再次", "重新招标", "重新采购", "废标重招", "流标重招"];

/// 公告类型尾词：只在**名称尾部**剔除（用于把标题还回项目名）。
///
/// 顺序即最长优先，避免「结果公告」先被「公告」吃掉。
const TYPE_TAIL: &[&str] = &[
    "中标（成交）结果公告",
    "中标(成交)结果公告",
    "中标候选人公示",
    "中标结果公告",
    "成交结果公告",
    "中选结果公告",
    "竞争性磋商公告",
    "竞争性谈判公告",
    "资格预审公告",
    "单一来源公示",
    "采购意向公告",
    "采购需求公示",
    "履约验收公告",
    "验收结果公告",
    "项目验收公告",
    "合同公告",
    "合同公示",
    "中标通知书",
    "结果公告",
    "结果公示",
    "中标公告",
    "成交公告",
    "中选公告",
    "招标公告",
    "采购公告",
    "询价公告",
    "磋商公告",
    "谈判公告",
    "意向公开",
    "意向公告",
    "需求公示",
    "更正公告",
    "变更公告",
    "澄清公告",
    "答疑公告",
    "延期公告",
    "终止公告",
    "废标公告",
    "流标公告",
    "验收公告",
    "运维公告",
    "公告",
    "公示",
];

/// 归一化采购人：全半角折叠 + 去空白与标点 + 英文小写。
///
/// 保留全部汉字与主体类型词（`有限公司`/`局`/`院`），理由见模块文档「归一化的克制」。
pub fn normalize_buyer(raw: &str) -> String {
    let half = to_halfwidth(raw);
    let mut out = String::with_capacity(half.len());
    for ch in half.chars() {
        if ch.is_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
        }
    }
    out
}

fn is_count_token(s: &str) -> bool {
    if s.is_empty() {
        return false;
    }
    const CN: &str = "零〇一二三四五六七八九十";
    s.chars().all(|c| c.is_ascii_digit() || CN.contains(c))
}

/// 「第 N 次」形态（N 为阿拉伯数字或中文数字，允许夹空格）。
fn is_ordinal_marker(s: &str) -> bool {
    let t: String = s.chars().filter(|c| !c.is_whitespace()).collect();
    let chars: Vec<char> = t.chars().collect();
    if chars.len() < 3 || chars[0] != '第' || chars[chars.len() - 1] != '次' {
        return false;
    }
    // 按字符取中间段：此处若按字节切片，`第` 会落在非字符边界上直接 panic
    let inner: String = chars[1..chars.len() - 1].iter().collect();
    is_count_token(&inner)
}

fn close_of(open: char) -> Option<char> {
    match open {
        '(' => Some(')'),
        '【' => Some('】'),
        '〔' => Some('〕'),
        '[' => Some(']'),
        _ => None,
    }
}

/// 剔除括号形式的次数/版本标记：`（第2次）`、`(第二次)`、`【二次】`、`（重新招标）`。
fn strip_bracketed(s: &str, notes: &mut Vec<String>) -> String {
    let chars: Vec<char> = s.chars().collect();
    let mut out = String::with_capacity(s.len());
    let mut i = 0usize;
    while i < chars.len() {
        let c = chars[i];
        if let Some(close) = close_of(c) {
            if let Some(j) = (i + 1..chars.len()).find(|&k| chars[k] == close) {
                let inner: String = chars[i + 1..j].iter().collect();
                let hit = is_ordinal_marker(&inner)
                    || BRACKET_VERSION_WORDS.contains(&inner.trim());
                if hit {
                    notes.push(format!("剔除后缀「{}{}」", c, inner.trim()));
                    i = j + 1;
                    continue;
                }
            }
        }
        out.push(c);
        i += 1;
    }
    out
}

/// 剔除裸写的「第 N 次」（无括号）：只在「第」后 6 个字符内出现「次」、
/// 且中间全是数字/中文数字时动手，避免误吃 `第一人民医院` 这类真实名称。
fn strip_bare_ordinal(s: &str, notes: &mut Vec<String>) -> String {
    let chars: Vec<char> = s.chars().collect();
    let mut out = String::with_capacity(s.len());
    let mut i = 0usize;
    while i < chars.len() {
        if chars[i] == '第' {
            let mut matched: Option<usize> = None;
            let mut j = i + 1;
            while j < chars.len() && j <= i + 6 {
                if chars[j] == '次' {
                    let inner: String = chars[i + 1..j].iter().collect();
                    if is_count_token(&inner) {
                        matched = Some(j);
                    }
                    break;
                }
                j += 1;
            }
            if let Some(j) = matched {
                let inner: String = chars[i + 1..j].iter().collect();
                notes.push(format!("剔除后缀「第{inner}次」"));
                i = j + 1;
                continue;
            }
        }
        out.push(chars[i]);
        i += 1;
    }
    out
}

/// 剔除整词版本后缀（`重新招标` / `废标重招` / `二次招标` …）。
fn strip_version_words(s: &str, notes: &mut Vec<String>) -> String {
    let mut cur = s.to_string();
    let mut changed = true;
    while changed {
        changed = false;
        for w in VERSION_WORDS {
            if let Some(pos) = cur.find(*w) {
                notes.push(format!("剔除后缀「{w}」"));
                cur = format!("{}{}", &cur[..pos], &cur[pos + w.len()..]);
                changed = true;
                break;
            }
        }
    }
    cur
}

fn trim_tail_punct(s: &str) -> &str {
    s.trim_end_matches(|c: char| {
        c.is_whitespace() || "，。：:、·-—｜|/\\<>《》()（）[]【】{}".contains(c)
    })
}

/// 剔除名称尾部的公告类型词（把公告标题还回项目名）。
fn strip_type_tail(s: &str, notes: &mut Vec<String>) -> String {
    let mut cur = s.to_string();
    let mut changed = true;
    while changed {
        changed = false;
        let t = trim_tail_punct(&cur).to_string();
        for w in TYPE_TAIL {
            if t.ends_with(*w) && t.len() > w.len() {
                notes.push(format!("剔除公告类型词「{w}」"));
                cur = t[..t.len() - w.len()].to_string();
                changed = true;
                break;
            }
        }
    }
    cur
}

/// 项目名归一化：返回（比较用归一值，剔除项说明）。
///
/// 顺序：全半角折叠 → 括号次数标记 → 裸「第 N 次」→ 整词版本后缀 →
/// 尾部公告类型词 → 字符级压缩（只留字母数字、英文小写）。
/// 归一值绝不为空：实在剔空时退回「只做字符压缩的原文」，避免所有记录挤到同一个空键。
pub fn normalize_name(raw: &str) -> (String, Vec<String>) {
    let mut notes: Vec<String> = Vec::new();
    let half = to_halfwidth(raw);
    if half != raw {
        notes.push("全角转半角".to_string());
    }
    let mut s = strip_bracketed(&half, &mut notes);
    s = strip_bare_ordinal(&s, &mut notes);
    s = strip_version_words(&s, &mut notes);
    s = strip_type_tail(&s, &mut notes);

    let squeeze = |t: &str| -> String {
        let mut out = String::with_capacity(t.len());
        for ch in t.chars() {
            if ch.is_alphanumeric() {
                out.push(ch.to_ascii_lowercase());
            }
        }
        out
    };
    let mut norm = squeeze(&s);
    if norm.is_empty() {
        // 退路：不做后缀剔除，只压缩字符，保证键非空且不同名称仍可区分
        norm = squeeze(&half);
    }
    if norm.is_empty() {
        // 再退一步：整串都是标点/空白（如「。。。」「——」）。此时若一律给空串，
        // 所有这类记录会挤到同一个键上被错误地并成一个项目。
        // 用字符码位拼一个确定性标识，保证「不同标点串 → 不同键」且重算稳定。
        norm = format!(
            "u{}",
            half.chars()
                .map(|c| format!("{:x}", c as u32))
                .collect::<Vec<_>>()
                .join("_")
        );
        if norm == "u" {
            norm = "u_empty".to_string();
        }
    }
    (norm, notes)
}

/// 一个聚合键的完整信息（含页面要展示的合并依据）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AggKey {
    /// 采购人原文（首尾空白已去）
    pub buyer: String,
    /// 项目名原文（首尾空白已去）
    pub project_name: String,
    pub buyer_norm: String,
    pub name_norm: String,
    /// 稳定键：`k:<buyer_norm>|<name_norm>`（`k:` 前缀与引擎的 `code:`/`fp:`/`body:` 区分）
    pub key: String,
    /// 归一化过程中剔除/折叠掉的项（合并依据的原料）
    pub stripped: Vec<String>,
}

impl AggKey {
    /// 页面可见的合并依据（规格 §2「页面上必须显示合并依据」）。
    pub fn merge_basis(&self) -> String {
        let mut s = format!("采购人「{}」+ 项目名「{}」", self.buyer, self.project_name);
        if self.stripped.is_empty() {
            s.push_str("（归一后无差异）");
        } else {
            s.push_str(&format!("（归一：{}）", self.stripped.join("、")));
        }
        s
    }

    /// 是否同一聚合键（采购人与项目名归一值都相同）。
    pub fn same_as(&self, other: &AggKey) -> bool {
        self.key == other.key
    }
}

/// 按规格 §2 计算聚合键。采购人或项目名为空时该侧归一值为空，
/// 因而「采购人缺失」的记录只会与同样缺失的记录相遇——不跨采购人误合并。
pub fn aggregate_key(buyer: &str, project_name: &str) -> AggKey {
    let buyer = buyer.trim();
    let project_name = project_name.trim();
    let buyer_norm = normalize_buyer(buyer);
    let (name_norm, stripped) = normalize_name(project_name);
    AggKey {
        buyer: buyer.to_string(),
        project_name: project_name.to_string(),
        buyer_norm: buyer_norm.clone(),
        name_norm: name_norm.clone(),
        key: format!("k:{buyer_norm}|{name_norm}"),
        stripped,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn k(b: &str, n: &str) -> AggKey {
        aggregate_key(b, n)
    }

    /// 规格 §2 点名要剔除的三类后缀：`（第 N 次）`、`重新招标`、`废标重招`。
    #[test]
    fn spec_named_suffixes_are_stripped() {
        let base = k("普农集团", "综合农事服务中心无人机采购项目");
        for variant in [
            "综合农事服务中心无人机采购项目（第2次）",
            "综合农事服务中心无人机采购项目(第二次)",
            "综合农事服务中心无人机采购项目 第3次",
            "综合农事服务中心无人机采购项目重新招标",
            "综合农事服务中心无人机采购项目废标重招",
            "综合农事服务中心无人机采购项目二次招标",
        ] {
            let v = k("普农集团", variant);
            assert_eq!(v.key, base.key, "「{variant}」应与基准同键，实际 {}\n依据 {:?}", v.key, v.stripped);
            assert!(!v.stripped.is_empty(), "剔除项要留痕，供页面展示合并依据");
        }
    }

    /// 全半角差异必须折叠：同一项目名的全角/半角写法同键。
    #[test]
    fn fullwidth_and_halfwidth_collapse() {
        let a = k("普农集团", "无人机采购（Ａ包）");
        let b = k("普农集团", "无人机采购(A包)");
        assert_eq!(a.key, b.key, "全半角应折叠：{} vs {}", a.key, b.key);
        assert!(a.stripped.iter().any(|s| s.contains("全角")), "全角折叠要留痕: {:?}", a.stripped);

        // 全角空格（U+3000）同样归一
        let c = k("普农集团", "无人机采购项目\u{3000}招标公告");
        let d = k("普农集团", "无人机采购项目 招标公告");
        assert_eq!(c.key, d.key);
    }

    /// 不同采购人的同名项目**不得合并**（规格 §2 硬性要求）。
    #[test]
    fn same_name_different_buyer_never_merges() {
        let a = k("普农集团", "综合农事服务中心无人机采购项目");
        let b = k("池州市农业农村局", "综合农事服务中心无人机采购项目");
        assert_ne!(a.key, b.key, "不同采购人同名项目必须不同键");
        assert_eq!(a.name_norm, b.name_norm, "项目名归一值可以相同");
        assert_ne!(a.buyer_norm, b.buyer_norm);
        assert!(!a.same_as(&b));
    }

    /// 裸「二次」不剔：`二次供水改造` 里的「二次」是标的名的一部分，不是次数。
    #[test]
    fn bare_erci_is_not_a_version_suffix() {
        let a = k("某水司", "二次供水改造项目");
        let b = k("某水司", "供水改造项目");
        assert_ne!(a.key, b.key, "裸「二次」不得被当成次数剔掉，否则会误合并不同标的");
        assert!(a.stripped.iter().all(|s| !s.contains("二次")), "不该产生「二次」剔除记录: {:?}", a.stripped);

        // 但括号形式「（二次）」是次数标记，要剔
        let c = k("某水司", "二次供水改造项目（二次）");
        assert_eq!(c.key, a.key, "括号「（二次）」应剔: {:?}", c.stripped);
    }

    /// 公告类型尾词只在尾部剔，不能动名称中间的字。
    #[test]
    fn type_suffix_only_stripped_at_tail() {
        let a = k("某局", "无人机采购项目招标公告");
        let b = k("某局", "无人机采购项目");
        assert_eq!(a.key, b.key, "尾部「招标公告」应剔: {:?}", a.stripped);

        // 「公告」出现在名称中间时不得剔（`公告栏` 是标的的一部分）
        let c = k("某局", "公告栏LED屏采购");
        assert!(c.name_norm.contains("公告栏"), "名称中间的「公告」不得剔除: {}", c.name_norm);
    }

    /// 合并依据必须可读、且逐项解释剔除内容。
    #[test]
    fn merge_basis_is_explainable() {
        let a = k("普农集团", "综合农事服务中心无人机采购项目（第2次）");
        let basis = a.merge_basis();
        assert!(basis.contains("普农集团"), "依据要含采购人: {basis}");
        assert!(basis.contains("第2次"), "依据要含被剔除的后缀: {basis}");
        assert!(basis.contains("综合农事服务中心无人机采购项目"), "依据要含项目名: {basis}");

        let plain = k("普农集团", "综合农事服务中心无人机采购项目");
        assert!(plain.merge_basis().contains("无差异"), "无归一时要说明: {}", plain.merge_basis());
    }

    /// 采购人归一保留主体类型词（不剥「有限公司」），避免不同主体被并。
    #[test]
    fn buyer_normalization_keeps_entity_words() {
        let a = k("安徽省普济圩现代农业集团有限公司", "无人机采购项目");
        let b = k("安徽省普济圩现代农业集团", "无人机采购项目");
        assert_ne!(a.key, b.key, "主体类型词不同视为不同采购人（宁可少合，不可错合）");

        // 但标点/空白/全半角差异要归一
        let c = k(" 安徽省普济圩现代农业集团有限公司 ", "无人机采购项目");
        assert_eq!(c.key, a.key, "首尾空白不应影响键");
        let d = k("安徽省普济圩现代农业集团有限公司，", "无人机采购项目");
        assert_eq!(d.key, a.key, "尾部标点不应影响键");
    }

    /// 空采购人不与有采购人的记录合并（宁缺勿错）。
    #[test]
    fn empty_buyer_does_not_merge_with_named_buyer() {
        let a = k("", "无人机采购项目");
        let b = k("某局", "无人机采购项目");
        assert_ne!(a.key, b.key);
        assert_eq!(a.key, "k:|无人机采购项目");
    }

    /// 归一值绝不为空：极端输入（全标点）仍要产出可区分键。
    #[test]
    fn normalization_never_yields_empty_key() {
        let a = k("某局", "。。。");
        assert!(!a.name_norm.is_empty(), "归一值不得为空");
        let b = k("某局", "——");
        assert_ne!(a.key, b.key, "两个不同的全标点名不应挤到同一个空键");
    }
}
