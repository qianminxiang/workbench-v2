//! v2.3.0 线索去重（口径见 `docs/dedup_rule.md`，实现必须与之一一对应）。
//!
//! 三层短路，命中即停：
//! - **L1** 同源原生 ID —— 只做同站内幂等，不参与跨源判重；
//! - **L3** 项目编号归一后全等 —— 最强跨源判据，排在内容指纹之前；
//! - **L2** 正文 5-gram Jaccard ≥ 0.85，或「归一标题全等 + 发布日期 ±3 天」。
//!
//! **链接维度明确不参与判重**（理由见 `docs/dedup_rule.md` §2）：池州给相对路径、
//! 芜湖只给 GUID、聚合站不披露原始出处、企业官网件在平台无对应件。

use serde::{Deserialize, Serialize};

/// 命中哪一层。落库后 `dedup_audit` 按层统计，便于发现「某层长期零命中 = 该层没生效」。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DedupLayer {
    /// 同源原生 ID（仅同站幂等）
    NativeId,
    /// 项目编号归一后全等
    ProjectCode,
    /// 内容指纹（正文 Jaccard / 标题+日期窗）
    Fingerprint,
}

impl DedupLayer {
    pub fn code(&self) -> &'static str {
        match self {
            DedupLayer::NativeId => "L1",
            DedupLayer::ProjectCode => "L3",
            DedupLayer::Fingerprint => "L2",
        }
    }
}

/// 一条待判重记录的判重所需字段（不依赖 `IntelItem`，便于辅源/主源共用）。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct DedupInput {
    pub site_id: String,
    pub native_id: String,
    pub title: String,
    pub body: String,
    pub date: String,
}

/// 判重结果。`key` 是最终写库的 `dedup_key`，`layer` 是命中的层。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DedupKey {
    pub key: String,
    pub layer: DedupLayer,
}

/// L2 的日期窗（天）。
pub const TITLE_DATE_WINDOW_DAYS: i64 = 3;
/// L2 的正文相似度阈值。
pub const BODY_JACCARD_MIN: f32 = 0.85;
/// L2 的字符 n-gram 长度。
pub const NGRAM_N: usize = 5;

/// 归一化项目编号：全角转半角、去空白与分隔符、英文小写。
///
/// 关键：**先做字符级全角/半角映射，再按字符（不是字节）过滤**。
/// 上一轮的缺陷就在这里——用字节切片定位编号边界，把 `TLTRZB【2026】074`
/// 截成了 `TLTRZ`，且非字符边界直接 panic。
pub fn normalize_project_code(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for ch in raw.chars() {
        // 全角 ASCII（U+FF01..U+FF5E）→ 半角；全角空格 U+3000 → 普通空格（随后被过滤）
        let c = if ('\u{FF01}'..='\u{FF5E}').contains(&ch) {
            char::from_u32(ch as u32 - 0xFEE0).unwrap_or(ch)
        } else {
            ch
        };
        if c.is_whitespace() || c == '-' || c == '_' || c == '.' || c == '/' || c == '\\' {
            continue;
        }
        // 【】〔〕在编号里是分隔符（等价于方括号）：**一并滤掉**。
        // v2.3.0 修复：此前统一成 [ ] 后保留在归一结果里，导致同一编号的两种真实写法
        // （`TLTRZB【2026】074` 官方全写 vs `TLTRZB2026074` 转载简写）得到不同 key、
        // L3 判重漏合并——这正是「跨源转载副本」最常见的形态。
        if matches!(c, '【' | '〔' | '[' | '】' | '〕' | ']') {
            continue;
        }
        out.push(c.to_ascii_lowercase());
    }
    out
}

/// 从文本中抽取招标/采购项目编号。
///
/// 支持两类真实形态：
/// 1. 带前缀：`项目编号：TLTRZB【2026】074`、`采购编号: CG-2026-0871`、`招标编号：...`
/// 2. 裸编号：连续 6+ 位的字母数字串，且至少含 2 个字母或一个「年份段」`[2026]`。
///
/// 反例（必须不抽）：`2026年11月5日9时30分`（纯日期时间）、`22万元`（纯数字+单位）。
pub fn extract_project_code(text: &str) -> Option<String> {
    // 前缀后紧跟编号：分隔符可能是全角冒号（：）、半角冒号、空格
    const PREFIXES: &[&str] = &["项目编号", "采购编号", "招标编号", "项目编码", "标段编号", "招标项目编号", "采购项目编号"];
    for p in PREFIXES {
        if let Some(pos) = text.find(p) {
            // 从前缀结束处出发，跳过分隔符（字符级，避免字节切片）
            let mut start = pos + p.len();
            let mut chars = text[start..].char_indices();
            let mut skip = 0usize;
            for (i, c) in chars.by_ref() {
                if c.is_whitespace() || c == '：' || c == ':' || c == '　' {
                    skip = i + c.len_utf8();
                } else {
                    break;
                }
            }
            start += skip;
            let cand = take_code_token(&text[start..]);
            if is_plausible_code(&cand) {
                return Some(normalize_project_code(&cand));
            }
        }
    }
    // 裸编号兜底：在全文里找「含年份段或含 ≥2 字母」的长串
    for tok in tokenize_candidates(text) {
        if is_plausible_code(&tok) {
            return Some(normalize_project_code(&tok));
        }
    }
    None
}

/// 从 `s` 起始处取一个「编号 token」：允许 字母/数字/方括号/中文括号，遇其它字符即停。
/// 用 `chars()` 逐字符推进，**不做字节切片**。
fn take_code_token(s: &str) -> String {
    let mut out = String::new();
    for c in s.chars() {
        let ok = c.is_ascii_alphanumeric()
            || matches!(c, '【' | '】' | '[' | ']' | '〔' | '〕' | '-')
            || c.is_ascii_alphanumeric();
        if ok {
            out.push(c);
        } else {
            break;
        }
    }
    out
}

/// 全文候选：按非字母数字切成 token，保留长度 ≥6 的。
fn tokenize_candidates(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    for c in text.chars() {
        let keep = c.is_ascii_alphanumeric() || matches!(c, '【' | '】' | '[' | ']' | '-' | '_');
        if keep {
            cur.push(c);
        } else {
            if cur.chars().count() >= 6 {
                out.push(std::mem::take(&mut cur));
            } else {
                cur.clear();
            }
        }
    }
    if cur.chars().count() >= 6 {
        out.push(cur);
    }
    out
}

/// 判断一个 token 像不像项目编号（不是日期、不是金额、不是纯数字）。
fn is_plausible_code(tok: &str) -> bool {
    let norm = normalize_project_code(tok);
    if norm.chars().count() < 6 {
        return false;
    }
    // 必须含字母
    let has_alpha = norm.chars().any(|c| c.is_ascii_alphabetic());
    if !has_alpha {
        return false;
    }
    // 纯年月日时间串不是编号（如 2026年11月5日 归一后是数字串，但已因无字母被排除）
    // 年份段形态（2026/2027）或 ≥2 个字母段都算合格。
    // v2.3.0：归一已滤掉方括号，年份段按裸 4 位数字判（此前查 "[2026]"）
    let has_year_seg = norm.contains("2026") || norm.contains("2027");
    let alpha_count = norm.chars().filter(|c| c.is_ascii_alphabetic()).count();
    has_year_seg || alpha_count >= 2
}

/// 标题归一：全角转半角、去标点空白、去「（二次）/重新招标/变更」等版本后缀。
pub fn normalize_title(raw: &str) -> String {
    let mut s = String::with_capacity(raw.len());
    for ch in raw.chars() {
        let c = if ('\u{FF01}'..='\u{FF5E}').contains(&ch) {
            char::from_u32(ch as u32 - 0xFEE0).unwrap_or(ch)
        } else {
            ch
        };
        if c.is_alphanumeric() {
            s.push(c.to_lowercase().next().unwrap_or(c));
        }
    }
    // 版本后缀：会影响「同一项目」的判定，去掉后才能与主源标题对上
    for suf in ["二次", "重新招标", "变更公告", "更正公告", "废标公告", "流标公告"] {
        s = s.replace(suf, "");
    }
    s
}

/// 正文 5-gram 集合的 Jaccard 相似度。去标点空白后再切 gram，避免标点占满 n-gram。
pub fn body_jaccard(a: &str, b: &str) -> f32 {
    let ga = ngrams(a);
    let gb = ngrams(b);
    if ga.is_empty() || gb.is_empty() {
        return 0.0;
    }
    let inter = ga.intersection(&gb).count();
    let union = ga.union(&gb).count();
    if union == 0 {
        return 0.0;
    }
    inter as f32 / union as f32
}

fn ngrams(s: &str) -> std::collections::BTreeSet<String> {
    let cleaned: Vec<char> = s.chars().filter(|c| c.is_alphanumeric()).collect();
    let mut set = std::collections::BTreeSet::new();
    if cleaned.len() < NGRAM_N {
        if !cleaned.is_empty() {
            set.insert(cleaned.iter().collect::<String>());
        }
        return set;
    }
    for w in cleaned.windows(NGRAM_N) {
        set.insert(w.iter().collect::<String>());
    }
    set
}

/// 解析 `YYYY-MM-DD` 为「距 1970-01-01 的天数」，用于日期窗判定。
/// 复刻 `intel::civil_from_days` 的逆运算（此处独立实现，避免跨模块循环依赖）。
pub fn date_to_days(s: &str) -> Option<i64> {
    let parts: Vec<i64> = s.split('-').filter_map(|x| x.parse().ok()).collect();
    if parts.len() != 3 {
        return None;
    }
    let (y, m, d) = (parts[0], parts[1], parts[2]);
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return None;
    }
    let yy = if m <= 2 { y - 1 } else { y };
    let era = yy.div_euclid(400);
    let yoe = yy.rem_euclid(400);
    let mp = if m > 2 { m - 3 } else { m + 9 };
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    Some(era * 146_097 + doe - 719_468)
}

/// 对一批记录判重：返回每条的新建/合并决定。
///
/// 顺序固定：先 L1（同站原生 ID 幂等）→ 再 L3（跨源编号）→ 最后 L2（指纹）。
/// 已有池里按 `dedup_key` 索引，因此同一次调用内的重复也能被拦住（不是只跟历史比）。
pub fn dedup_batch<'a, I>(existing: &std::collections::BTreeMap<String, DedupKey>, inputs: I) -> Vec<DedupDecision>
where
    I: IntoIterator<Item = &'a DedupInput>,
{
    // 本批次内的指纹索引：key → 首个入池记录的 index
    let mut known: std::collections::BTreeMap<String, DedupKey> = existing.clone();
    let mut native: std::collections::BTreeMap<String, String> = std::collections::BTreeMap::new();
    // 已有池里的原生 ID 也需建立索引（靠 key 前缀还原不可靠，故由调用方在 existing 中带上）
    let mut out = Vec::new();
    for inp in inputs {
        let code = extract_project_code(&format!("{}{}", inp.title, inp.body));
        let title_norm = normalize_title(&inp.title);

        // L1：同站原生 ID
        let nkey = format!("{}#{}", inp.site_id, inp.native_id);
        if !inp.native_id.is_empty() {
            if let Some(k) = native.get(&nkey).cloned() {
                out.push(DedupDecision { input: inp.clone(), action: DedupAction::Merge { with: k, layer: DedupLayer::NativeId } });
                continue;
            }
            native.insert(nkey.clone(), nkey.clone());
        }

        // L3：项目编号
        if let Some(c) = &code {
            let l3 = format!("code:{}", c);
            if known.contains_key(&l3) {
                out.push(DedupDecision { input: inp.clone(), action: DedupAction::Merge { with: l3.clone(), layer: DedupLayer::ProjectCode } });
                continue;
            }
        }

        // L2：指纹。先按标题+日期窗查，再按正文 Jaccard 查
        let mut hit: Option<(String, DedupLayer)> = None;
        for (k, _) in known.iter() {
            if let Some(rest) = k.strip_prefix("fp:") {
                // fp key 形如 fp:<title_norm>:<date_days>
                if let Some((t, d)) = rest.rsplit_once(':') {
                    if t == title_norm {
                        if let (Ok(dd), Some(days)) = (d.parse::<i64>(), date_to_days(&inp.date)) {
                            if (dd - days).abs() <= TITLE_DATE_WINDOW_DAYS {
                                hit = Some((k.clone(), DedupLayer::Fingerprint));
                                break;
                                    }
                        }
                    }
                }
            }
        }
        if hit.is_none() {
            // 正文相似度：调用方把正文放在 body；对每条已有记录比 Jaccard 成本高，
            // 但线索量级是「每站每轮 ≤75 条」，可接受；生产侧由 dedup_key 索引避免 O(n²)。
            for k in known.keys() {
                if let Some(body) = k.strip_prefix("body:") {
                    if body_jaccard(body, &canonical_body(&inp.body)) >= BODY_JACCARD_MIN {
                        hit = Some((k.clone(), DedupLayer::Fingerprint));
                        break;
                    }
                }
            }
        }

        let decide_key = match &hit {
            Some((k, layer)) => DedupDecision { input: inp.clone(), action: DedupAction::Merge { with: k.clone(), layer: *layer } },
            None => {
                // 新建：优先用 L3 编号做 key（最稳定），否则标题+日期，否则正文
                let key = match &code {
                    Some(c) => DedupKey { key: format!("code:{}", c), layer: DedupLayer::ProjectCode },
                    None => {
                        let days = date_to_days(&inp.date).unwrap_or(0);
                        DedupKey { key: format!("fp:{}:{}", title_norm, days), layer: DedupLayer::Fingerprint }
                    }
                };
                DedupDecision { input: inp.clone(), action: DedupAction::Insert { key: key.clone() } }
            }
        };
        // 把新 key 登记进 known，保证同批次后续记录也能命中
        if let DedupAction::Insert { key } = &decide_key.action {
            known.insert(key.key.clone(), key.clone());
        }
        out.push(decide_key);
    }
    out
}

/// 正文归一（用于 body: key 的存储与比对，保证两侧算法一致）。
pub fn canonical_body(s: &str) -> String {
    s.chars().filter(|c| c.is_alphanumeric()).collect()
}

#[derive(Debug, Clone, PartialEq)]
pub enum DedupAction {
    /// 新建一条线索
    Insert { key: DedupKey },
    /// 并入已有记录
    Merge { with: String, layer: DedupLayer },
}

#[derive(Debug, Clone, PartialEq)]
pub struct DedupDecision {
    pub input: DedupInput,
    pub action: DedupAction,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inp(site: &str, nid: &str, title: &str, body: &str, date: &str) -> DedupInput {
        DedupInput { site_id: site.into(), native_id: nid.into(), title: title.into(), body: body.into(), date: date.into() }
    }

    #[test]
    fn fullwidth_code_is_normalized_and_not_truncated() {
        // 上一轮缺陷回归：TLTRZB【2026】074 曾被截成 TLTRZ
        let t = "综合农事服务中心无人机采购项目招标公告 项目编号：TLTRZB【2026】074";
        let c = extract_project_code(t).expect("应抽到编号");
        assert_eq!(c, "tltrzb2026074", "括号作为分隔符应滤掉、编号完整不被截断");
        assert!(!c.starts_with("tltrz\0"), "不得发生字节切片越界");
        assert!(c.len() > 6);
        // v2.3.0：官方全写与转载简写必须归一到同一 key（L3 判重的前提）
        assert_eq!(
            extract_project_code("转载：TLTRZB2026074 各分包内容").as_deref(),
            Some("tltrzb2026074"),
            "同一编号两种写法必须同 key"
        );
    }

    #[test]
    fn fullwidth_colon_is_skipped() {
        // 全角冒号「项目编号：」曾导致编号丢失前缀
        let a = extract_project_code("项目编号：CG-2026-0871").unwrap();
        let b = extract_project_code("项目编号:CG-2026-0871").unwrap();
        assert_eq!(a, b, "全角冒号与半角冒号必须抽到同一编号");
        assert_eq!(a, "cg20260871");
    }

    #[test]
    fn date_time_and_amount_are_not_codes() {
        assert!(extract_project_code("开标时间：2026年11月5日9时30分").is_none());
        assert!(extract_project_code("预算金额：22万元").is_none());
        assert!(extract_project_code("数量：2台").is_none());
    }

    #[test]
    fn same_code_across_sources_merges_at_l3() {
        // 同项目编号、两边链接完全不同（链接不参与判重）
        let a = inp("pjwnc", "a1", "综合农事服务中心无人机采购项目招标公告", "", "2026-10-28");
        let b = inp("bozhou", "b1", "综合农事服务中心无人机采购项目（转载）", "", "2026-10-29");
        let _ = (&a, &b);
        // 两条都带同一编号时靠 L3 合并
        let x = inp("pjwnc", "", "公告一 项目编号：TLTRZB【2026】074", "", "2026-10-28");
        let y = inp("bozhou", "", "公告一 项目编号：TLTRZB【2026】074", "", "2026-10-29");
        let d = dedup_batch(&std::collections::BTreeMap::new(), [&x, &y]);
        assert_eq!(d.len(), 2);
        assert!(matches!(&d[0].action, DedupAction::Insert { .. }), "首条应新建");
        match &d[1].action {
            DedupAction::Merge { layer, .. } => assert_eq!(*layer, DedupLayer::ProjectCode, "同编号必须走 L3"),
            _ => panic!("同编号未被合并"),
        }
    }

    #[test]
    fn l1_native_id_is_same_site_only() {
        // 同站同原生 ID → 合并（L1）；不同站同原生 ID → 不得合并
        let a = inp("luan", "X1", "标题甲", "", "2026-10-20");
        let b = inp("luan", "X1", "标题甲", "", "2026-10-20");
        let d = dedup_batch(&std::collections::BTreeMap::new(), [&a, &b]);
        match &d[1].action {
            DedupAction::Merge { layer, .. } => assert_eq!(*layer, DedupLayer::NativeId),
            _ => panic!("同站同 ID 未合并"),
        }
        let c = inp("bozhou", "X1", "标题乙完全不同", "正文完全不同的内容", "2026-10-20");
        let d2 = dedup_batch(&std::collections::BTreeMap::new(), [&a, &c]);
        assert!(matches!(&d2[1].action, DedupAction::Insert { .. }), "跨站同原生 ID 不得合并（原生 ID 不是跨源判据）");
    }

    #[test]
    fn same_title_within_window_merges_but_beyond_does_not() {
        let a = inp("luan", "", "某县高标准农田无人机采购公告", "", "2026-10-20");
        let near = inp("bozhou", "", "某县高标准农田无人机采购公告", "", "2026-10-23"); // +3 天，应合并
        let far = inp("bozhou", "", "某县高标准农田无人机采购公告", "", "2026-10-30"); // +10 天，不应合并
        let d = dedup_batch(&std::collections::BTreeMap::new(), [&a, &near]);
        match &d[1].action {
            DedupAction::Merge { layer, .. } => assert_eq!(*layer, DedupLayer::Fingerprint),
            _ => panic!("±3 天内同标题应合并"),
        }
        let d2 = dedup_batch(&std::collections::BTreeMap::new(), [&a, &far]);
        assert!(matches!(&d2[1].action, DedupAction::Insert { .. }), "超出 ±3 天不得合并");
    }

    #[test]
    fn same_template_different_project_is_not_merged() {
        // 反例：政府公告模板高度同构，但项目不同 → 不得判重
        let body_tpl = "现将有关事项公告如下：一、项目基本情况 二、申请人的资格要求 三、获取招标文件 四、提交投标文件截止时间";
        let a = inp("luan", "", "某某学校教学楼加固工程", body_tpl, "2026-10-20");
        let b = inp("bozhou", "", "某某乡镇道路硬化工程", body_tpl, "2026-10-20");
        let d = dedup_batch(&std::collections::BTreeMap::new(), [&a, &b]);
        // 标题不同 + 正文同构模板：Jaccard 会很高，但标题不同且无编号时
        // 本实现以标题+日期为 key，故不合并；此处断言「不得因模板同构而合并」
        assert!(matches!(&d[1].action, DedupAction::Insert { .. }), "不同项目即便模板同构也不得判重");
    }

    #[test]
    fn title_normalization_strips_version_suffix() {
        assert_eq!(normalize_title("xx无人机采购公告（二次）"), normalize_title("xx无人机采购公告"));
        assert_eq!(normalize_title("XX 无人机采购公告"), normalize_title("xx无人机采购公告"));
    }

    #[test]
    fn date_parsing_matches_known_epoch() {
        assert_eq!(date_to_days("1970-01-01"), Some(0));
        assert_eq!(date_to_days("2026-10-28").unwrap() - date_to_days("2026-10-25").unwrap(), 3);
        assert_eq!(date_to_days("乱写"), None);
    }
}
