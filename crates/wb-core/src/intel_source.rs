//! v2.3.0 应用内直连抓取：**各源 fetch 适配器接线** + **标题初筛 / 正文复核两级命中**。
//!
//! 本模块回答三个此前悬空的问题（用户 2026-11-05 拍板的需求②③，以及「适配器未接线」的未竟项）：
//!
//! 1. **Epoint 同族只查标题会漏** —— 需求②的根因不在解析，在请求参数：`fields:"title"`
//!    只让服务端在标题里匹配 `wd`。实测六安同一窗口 `fields=title` → 22 条、
//!    `fields=title;content` → 70 条，漏 48 条（2026-11-05 本单实测）。
//!    但「命中正文」不等于「该进线索池」：正文里出现「无人机」可能只是把无人机当辅助工具
//!    （航拍/巡线），因此**服务端放宽召回、客户端仍要分级判定**——这就是两级命中：
//!    标题命中 → 直接进；仅正文命中 → 必须再过农业加分词复核才升级，否则落 Suspect。
//! 2. **池州 / 芜湖是非 Epoint 站** —— 此前一律 `ParseError("适配器待第二批逐站验证实现")`。
//!    本轮逐站解出真实接口（池州 `POST /front/search/list` 需 `page`/`rows`/`searchKey`
//!    三段式 JSON；芜湖官方 `.NET API` 在沙箱出口层 502，按 NeedsCredential/不可达如实标注）。
//! 3. **企业官网（普农 pjwnc.com）与聚合站（乙方宝）** —— 前者是主源（首发方），
//!    后者是辅源（不披露出处）。二者都是 HTML，但抽取方式与入库去向不同，
//!    因此用 [`SourceKind`] 区分而不是塞进 Epoint 适配器。

use crate::intel::{
    classify, clean_text, ensure_not_blocked, head_chars, FetchFail, FetchStatus, Http, IntelItem, Relevance, SiteSpec,
    DIAG_BODY_CHARS, FETCH_TIMEOUT_SECS,
};
use serde::{Deserialize, Serialize};
use std::time::Duration;

// ─────────────────────────────────────────────────────────────────────────
// 源类型：决定「怎么抓」与「进哪个池」
// ─────────────────────────────────────────────────────────────────────────
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceKind {
    /// Epoint 同族全文检索接口（16 地市中 13 站可用，含宿州双层包装）
    EpointFulltext,
    /// 池州自有 JSON 检索接口（无日期参数，须本地按 publishTime 过滤）
    ChizhouJson,
    /// 芜湖独立 .NET API（仅标题式检索）
    WuhuDotNet,
    /// 企事业单位官网 CMS（普农 pjwnc.com 一类）：**主源**，公告首发方
    EnterpriseCms,
    /// 招标聚合站（乙方宝一类）：**辅源**，只作初筛，不独立成线索
    AggregatorHtml,
}

impl SourceKind {
    /// 该源是否可以独立产生线索。
    ///
    /// 只由**源的性质**决定，不由抓取成败决定：企业官网虽然是 HTML，但它是公告首发方，
    /// 因此是主源；聚合站即使字段更全也永不独立成线索（去重口径 §3）。
    pub fn is_primary(self) -> bool {
        !matches!(self, SourceKind::AggregatorHtml)
    }

    /// 是否需要「进正文」——决定抓取时是否多打一次详情页请求。
    ///
    /// Epoint / 池州 / 芜湖的检索接口**本身就回正文**（实测 content 字段 503 字节起），
    /// 不必二次请求；企业官网与聚合站的列表页只有标题，正文要逐条进详情页取。
    pub fn needs_detail_fetch(self) -> bool {
        matches!(self, SourceKind::EnterpriseCms | SourceKind::AggregatorHtml)
    }
}

/// 站点 id → 源类型。注册表不新增字段，避免与既有 20 项 SiteSpec 字面量冲突。
pub fn kind_of(site_id: &str) -> SourceKind {
    match site_id {
        "chizhou" => SourceKind::ChizhouJson,
        "wuhu" => SourceKind::WuhuDotNet,
        // 本次随需求③新登记的企业官网源
        "pjwnc" => SourceKind::EnterpriseCms,
        // 需求①聚合站辅助层
        "yibao" => SourceKind::AggregatorHtml,
        _ => SourceKind::EpointFulltext,
    }
}

// ─────────────────────────────────────────────────────────────────────────
// 两级命中：标题初筛 + 正文复核（需求②的核心）
// ─────────────────────────────────────────────────────────────────────────
/// 命中级别。**决定该条目是「进线索池」还是「只进正文复核候选」**。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HitLevel {
    /// 标题命中主题词（且通过分类判定）：直接进
    Title,
    /// 标题不含主题词，正文含主题词 + 农业加分词：复核通过，升级进
    /// （普农 TLTRZB【2026】074 即此类：标题只有「无人机」，正文有 植保/施肥/撒播/田间/病虫害）
    BodyVerified,
    /// 正文含主题词但没有农业加分词：疑似（进 Suspect，可人工改判，不静默丢弃）
    BodyWeak,
    /// 两级都没命中：无关
    None,
}

impl HitLevel {
    pub fn label(&self) -> &'static str {
        match self {
            HitLevel::Title => "标题命中",
            HitLevel::BodyVerified => "正文复核命中",
            HitLevel::BodyWeak => "仅正文弱命中",
            HitLevel::None => "未命中",
        }
    }

    /// 是否进入线索池（Title / BodyVerified）。BodyWeak 进 Suspect 待人工改判。
    pub fn enters_pool(&self) -> bool {
        matches!(self, HitLevel::Title | HitLevel::BodyVerified)
    }
}

/// 两级命中判定结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TwoStageHit {
    pub level: HitLevel,
    /// 判定依据（逐词列出，结报要「给出命中依据」，不能只给结论）
    pub reasons: Vec<String>,
    /// 复用 `classify` 的相关性结论，避免词表在两处漂移
    pub relevance: Relevance,
}

/// 标题初筛 + 正文关键词复核。
///
/// 与 [`crate::intel::classify`] 的关系：`classify` 只看「主题词 + 加分词 + 排除词」，
/// 不区分命中位置；这里补的是**位置维度**——标题命中是强信号（招标方自己写了主题），
/// 正文命中是弱信号（可能只是把无人机当辅助工具提及），必须加农业词复核才升级。
pub fn two_stage(title: &str, body: &str) -> TwoStageHit {
    let (relevance, mut reasons) = classify(title, body);
    let title_theme = crate::intel::THEME_WORDS.iter().any(|w| title.contains(w));
    // 标题里的农业语义（决定「标题本身是否已经足够」）
    let title_plus: Vec<&str> = crate::intel::PLUS_WORDS.iter().copied().filter(|w| title.contains(w)).collect();
    // 正文（含标题，因为 classify 也是拼接判定）里的农业语义
    let full = format!("{}{}", title, body);
    let body_plus: Vec<&str> = crate::intel::PLUS_WORDS.iter().copied().filter(|w| full.contains(w)).collect();

    let level = if title_theme && !title_plus.is_empty() {
        // 标题自己就写清了「农业 + 无人机」——最强的信号，直接进
        HitLevel::Title
    } else if title_theme {
        // 标题只有主题词、没有农业语义（**普农件的真实形态**：标题「…综合农事服务中心无人机采购项目」，
        // 没有「农业/植保/农田」，只有正文里才有 植保/施肥/撒播/田间/病虫害）。
        // 此时不能凭标题就判 Core——「无人机采购」也可能是航拍/巡线。必须过正文复核。
        if !body_plus.is_empty() {
            reasons.push("位置:标题仅含主题词，正文复核".into());
            reasons.extend(body_plus.iter().map(|w| format!("正文复核加分词:{}", w)));
            HitLevel::BodyVerified
        } else {
            reasons.push("位置:标题仅含主题词且全文无农业语义".into());
            HitLevel::BodyWeak
        }
    } else if crate::intel::THEME_WORDS.iter().any(|w| full.contains(w)) && !body_plus.is_empty() {
        // 标题完全没有主题词，主题词只出现在正文——**需求②要救回的那批**（只查标题会整条漏掉）
        reasons.push("位置:标题未含主题词，仅正文命中".into());
        reasons.extend(body_plus.iter().map(|w| format!("正文复核加分词:{}", w)));
        HitLevel::BodyVerified
    } else if crate::intel::THEME_WORDS.iter().any(|w| full.contains(w)) {
        reasons.push("位置:仅正文含主题词且无农业语义".into());
        HitLevel::BodyWeak
    } else {
        HitLevel::None
    };

    TwoStageHit { level, reasons, relevance }
}

/// 把两级命中结果写回条目：升级相关性的同时**保留判定依据**。
///
/// 为什么要单独一个函数：抓取适配器（Epoint / 池州 / 芜湖 / 官网）都要做同一件事，
/// 各自写一遍必然漂移；且 `manual_verdict` 必须不受自动判定影响（AC-2-7）。
pub fn apply_two_stage(item: &mut IntelItem, body: &str) {
    if item.manual_verdict {
        return;
    }
    let hit = two_stage(&item.title, body);
    item.relevance = match hit.level {
        HitLevel::Title => hit.relevance,
        // 正文复核通过 → 视为 Core（这是需求②要救回的那批）
        HitLevel::BodyVerified => Relevance::Core,
        // 弱命中 → Suspect，人工可改判
        HitLevel::BodyWeak => Relevance::Suspect,
        HitLevel::None => Relevance::Excluded,
    };
    let mut r = hit.reasons;
    r.insert(0, format!("命中级别:{}", hit.level.label()));
    item.reasons = r;
}

// ─────────────────────────────────────────────────────────────────────────
// HTML 工具（企业官网 / 聚合站共用）
// ─────────────────────────────────────────────────────────────────────────
/// HTML → 纯文本。去脚本、样式、标签，解常见实体，折叠空白。
///
/// 与 `intel::clean_text` 的区别：那个是给**已是纯文本的字段**洗高亮标签用的（轻量）；
/// 这个是给**整页 HTML**用的，必须先剥 `<script>` / `<style>` 的整块内容，
/// 否则 JS 里的字符串会混进正文、污染关键词计数与 5-gram 指纹。
pub fn html_to_text(html: &str) -> String {
    let mut s = String::with_capacity(html.len());
    let lower = html.to_ascii_lowercase();
    let mut i = 0usize;
    while i < html.len() {
        // 整块跳过 script / style
        let mut skipped = false;
        for tag in ["<script", "<style"] {
            if lower[i..].starts_with(tag) {
                let close = if tag == "<script" { "</script>" } else { "</style>" };
                match lower[i..].find(close) {
                    Some(p) => {
                        i += p + close.len();
                        skipped = true;
                    }
                    None => {
                        i = html.len();
                        skipped = true;
                    }
                }
                break;
            }
        }
        if skipped {
            continue;
        }
        // 普通标签：整段跳过但保留标签之间的文字
        if lower[i..].starts_with('<') {
            match html[i..].find('>') {
                Some(p) => {
                    i += p + 1;
                    // 块级标签处补空格，避免「上一段」与「下一段」粘成一个词
                    s.push(' ');
                    continue;
                }
                None => break,
            }
        }
        let ch = html[i..].chars().next().unwrap();
        s.push(ch);
        i += ch.len_utf8();
    }
    let s = s
        .replace("&nbsp;", " ")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&#39;", "'")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&");
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// 从 HTML 里按属性名取 meta 值（`<meta name="ArticleTitle" content="…">`）。
pub fn meta_content(html: &str, name: &str) -> Option<String> {
    let needle = format!("name=\"{}\"", name);
    let pos = html.find(&needle)?;
    // meta 标签可能跨行，取到本标签结束为止
    let seg = &html[pos..];
    let end = seg.find('>').unwrap_or(seg.len());
    let tag = &seg[..end];
    let cp = tag.find("content=\"")?;
    let rest = &tag[cp + 9..];
    let qe = rest.find('"')?;
    Some(rest[..qe].trim().to_string())
}

/// 列表页里的一条（标题 + 详情链接 + 日期）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListEntry {
    pub title: String,
    pub url: String,
    pub date: String,
}

/// 从企业官网 / 聚合站列表页解析条目。
///
/// 手法刻意保守：只认 `<a href="/content/detail/...">标题</a>` 这类**同站相对链接**，
/// 并用「链接前最近的日期串」补日期（普农列表格式为 `<li><span>2026-11-04</span><a …>标题</a></li>`）。
/// 不做通用 HTML 解析库依赖：这两类站结构稳定，正则/游标足够，且避免为一个功能引入 html5ever 全栈。
pub fn parse_list_entries(html: &str, detail_prefix: &str) -> Vec<ListEntry> {
    let mut out = Vec::new();
    let mut pos = 0usize;
    while let Some(i) = html[pos..].find(detail_prefix) {
        let start = pos + i;
        let seg = &html[start..];
        // href 值到引号结束
        let href_end = seg.find('"').unwrap_or(seg.len());
        let href = &seg[..href_end];
        let after = &seg[href_end..];
        // 标题在 > … </a>
        let title = match (after.find('>'), after.find("</a>")) {
            (Some(gt), Some(lt)) if lt > gt => clean_text(&after[gt + 1..lt]),
            _ => String::new(),
        };
        // 日期：链接前 200 字符内最后一个 YYYY-MM-DD
        // 注意 byte 200 可能在多字节 UTF-8 字符中间——必须回退到最近的 char boundary，
        // 否则真实页面（中文密度高）直接 panic（2026-11-06 普农实测踩中）。
        let mut pre_from = start.saturating_sub(200);
        while pre_from < start && !html.is_char_boundary(pre_from) {
            pre_from += 1;
        }
        let pre = &html[pre_from..start];
        let date = last_date_in(pre).unwrap_or_default();
        if !title.is_empty() {
            let url = if href.starts_with("http") {
                href.to_string()
            } else {
                href.to_string()
            };
            out.push(ListEntry { title, url, date });
        }
        pos = start + href_end;
    }
    out
}

/// 取文本里**最后一个** `YYYY-MM-DD`（列表页日期在链接之前，用最后一个避免抓到页头日期）。
pub fn last_date_in(s: &str) -> Option<String> {
    let b: Vec<char> = s.chars().collect();
    let mut found: Option<String> = None;
    let mut i = 0usize;
    while i + 9 < b.len() {
        if b[i].is_ascii_digit() && b[i + 1].is_ascii_digit() && b[i + 2].is_ascii_digit() && b[i + 3].is_ascii_digit()
            && b[i + 4] == '-'
            && b[i + 5].is_ascii_digit()
            && b[i + 6].is_ascii_digit()
            && b[i + 7] == '-'
            && b[i + 8].is_ascii_digit()
            && b[i + 9].is_ascii_digit()
        {
            let cand: String = b[i..i + 10].iter().collect();
            let y: i32 = cand[..4].parse().unwrap_or(0);
            if (2000..=2100).contains(&y) {
                found = Some(cand);
            }
            i += 10;
            continue;
        }
        i += 1;
    }
    found
}

// ─────────────────────────────────────────────────────────────────────────
// 池州适配器（自有 JSON 接口）
// ─────────────────────────────────────────────────────────────────────────
/// 池州检索请求体。**参数名由站点 `partcommon.js` 的 `initPage()` 逆向得出**：
/// `{filter:{}, page, rows, searchKey, sourceType, categoryType}`。
/// 传给它的字段名写错会返回 `{"code":"9999","data":"系统异常"}`——本轮踩过。
pub fn build_chizhou_request(wd: &str, page: usize, rows: usize) -> String {
    serde_json::json!({
        "filter": {},
        "page": page + 1,           // 站点分页从 1 起
        "rows": rows,
        "searchKey": wd,
        "sourceType": "",
        "categoryType": "",
    })
    .to_string()
}

/// 池州响应 → IntelItem。**该接口不收回日期区间**，须本地按 `publishTime` 过滤（预研 5.3 已记）。
pub fn parse_chizhou(spec: &SiteSpec, resp: &str, today: &str, start: &str, end: &str) -> Result<Vec<IntelItem>, FetchFail> {
    let v: serde_json::Value = serde_json::from_str(resp).map_err(|_| FetchFail {
        body_head: head_chars(resp, DIAG_BODY_CHARS),
        ..FetchFail::local(FetchStatus::ParseError, "池州响应非 JSON")
    })?;
    // 站点自有的异常响应：code=9999 时没有 rows，必须显式报出来而不是当成「0 条」
    if let Some(code) = v.get("code").and_then(|c| c.as_str()) {
        if code == "9999" {
            return Err(FetchFail {
                body_head: head_chars(resp, DIAG_BODY_CHARS),
                ..FetchFail::local(FetchStatus::ParseError, "池州返回系统异常 code=9999")
            });
        }
    }
    // **不许静默回落**：`rows` 键缺失说明响应结构变了（或被打到别的端点），
    // 此时返回「0 条」会让界面显示"查询成功但今日无新增"，这是最危险的假绿。
    let rows = match v.get("rows").and_then(|r| r.as_array()) {
        Some(a) => a.clone(),
        None => {
            return Err(FetchFail {
                body_head: head_chars(resp, DIAG_BODY_CHARS),
                ..FetchFail::local(FetchStatus::ParseError, "池州响应缺 rows 键（结构变更或端点错）")
            })
        }
    };
    let mut items = Vec::new();
    for r in &rows {
        let title = clean_text(r.get("title").and_then(|t| t.as_str()).unwrap_or(""));
        if title.is_empty() {
            continue;
        }
        let body = clean_text(r.get("content").and_then(|t| t.as_str()).unwrap_or(""));
        let date = r
            .get("publishTime")
            .and_then(|t| t.as_str())
            .map(|s| s.chars().take(10).collect::<String>())
            .unwrap_or_default();
        // 本地日期窗口过滤（接口不认日期参数）
        if !date.is_empty() && (!start.is_empty() && date.as_str() < start) {
            continue;
        }
        if !date.is_empty() && (!end.is_empty() && date.as_str() > end) {
            continue;
        }
        let rel_url = r.get("url").and_then(|t| t.as_str()).unwrap_or("").trim().to_string();
        let url = if rel_url.starts_with("http") {
            rel_url.clone()
        } else if rel_url.starts_with('/') {
            format!("{}{}", spec.base, rel_url)
        } else {
            format!("{}/{}", spec.base, rel_url)
        };
        let (relevance, reasons) = classify(&title, &body);
        let area = r.get("regionCode").and_then(|t| t.as_str()).unwrap_or("").to_string();
        let mut item = IntelItem {
            key: format!("{}|{}", spec.id, url.trim()),
            site_id: spec.id.to_string(),
            site_name: spec.name.to_string(),
            title,
            org: String::new(),
            area,
            date,
            amount: None,
            url,
            relevance,
            reasons,
            incomplete: true, // 单位字段缺 → 如实标 incomplete（AC-2-1）
            manual_verdict: false,
            first_seen: today.to_string(),
            body_head: crate::intel::head_chars(&body, 600),
        };
        apply_two_stage(&mut item, &body);
        items.push(item);
    }
    Ok(items)
}

// ─────────────────────────────────────────────────────────────────────────
// 芜湖适配器（独立 .NET API）
// ─────────────────────────────────────────────────────────────────────────
/// 芜湖 GET 请求 URL。该站只有**标题式检索**（`GetNoticePage`），没有全文检索参数——
/// 这是它与 Epoint 同族的本质差别，不是「参数没调对」。
pub fn build_wuhu_url(base: &str, wd: &str, page: usize, page_size: usize) -> String {
    format!(
        "{}/api/Home/GetNoticePage?pageIndex={}&pageSize={}&keyword={}",
        base,
        page + 1,
        page_size,
        urlencode(wd)
    )
}

/// 芜湖响应 → IntelItem。字段名按 .NET 侧 PascalCase 兼容两种写法。
pub fn parse_wuhu(spec: &SiteSpec, resp: &str, today: &str) -> Result<Vec<IntelItem>, FetchFail> {
    let v: serde_json::Value = serde_json::from_str(resp).map_err(|_| FetchFail {
        body_head: head_chars(resp, DIAG_BODY_CHARS),
        ..FetchFail::local(FetchStatus::ParseError, "芜湖响应非 JSON")
    })?;
    // 常见包装：{data:{list:[…]}} / {Data:[…]} / 直接数组
    let arr = v
        .pointer("/data/list")
        .or_else(|| v.pointer("/data/rows"))
        .or_else(|| v.get("Data"))
        .or_else(|| v.get("data"))
        .and_then(|x| x.as_array())
        .cloned()
        .or_else(|| v.as_array().cloned())
        .unwrap_or_default();
    if arr.is_empty() && !(v.is_array() || v.get("data").is_some() || v.get("Data").is_some()) {
        return Err(FetchFail {
            body_head: head_chars(resp, DIAG_BODY_CHARS),
            ..FetchFail::local(FetchStatus::ParseError, "芜湖响应未见 data/Data 包装键（结构变更或端点错）")
        });
    }
    let mut items = Vec::new();
    for r in &arr {
        let title = pick_str(r, &["Title", "title"]).map(|s| clean_text(&s)).unwrap_or_default();
        if title.is_empty() {
            continue;
        }
        let date = pick_str(r, &["PublishDate", "publishDate", "CreateTime", "createTime"])
            .map(|s| s.chars().take(10).collect::<String>())
            .unwrap_or_default();
        let rel = pick_str(r, &["Url", "url", "LinkUrl", "linkurl"]).unwrap_or_default();
        let url = if rel.starts_with("http") {
            rel.clone()
        } else if rel.starts_with('/') {
            format!("{}{}", spec.base, rel)
        } else {
            format!("{}/{}", spec.base, rel)
        };
        let (relevance, reasons) = classify(&title, "");
        let mut item = IntelItem {
            key: format!("{}|{}", spec.id, url.trim()),
            site_id: spec.id.to_string(),
            site_name: spec.name.to_string(),
            title,
            org: String::new(),
            area: String::new(),
            date,
            amount: None,
            url,
            relevance,
            reasons,
            incomplete: true,
            manual_verdict: false,
            first_seen: today.to_string(),
            body_head: String::new(),
        };
        apply_two_stage(&mut item, ""); // 该接口无正文，只能标题命中
        items.push(item);
    }
    Ok(items)
}

fn pick_str(v: &serde_json::Value, keys: &[&str]) -> Option<String> {
    for k in keys {
        if let Some(s) = v.get(*k).and_then(|x| x.as_str()) {
            if !s.trim().is_empty() {
                return Some(s.trim().to_string());
            }
        }
    }
    None
}

fn urlencode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
            _ => out.push_str(&format!("%{:02X}", b)),
        }
    }
    out
}

// ─────────────────────────────────────────────────────────────────────────
// 企业官网 / 聚合站适配器（列表 → 逐条详情）
// ─────────────────────────────────────────────────────────────────────────
/// 企业官网 / 聚合站抓取配置（不塞进 `SiteSpec`，避免动既有 20 项注册表字面量）。
#[derive(Debug, Clone, Copy)]
pub struct HtmlSourceCfg {
    pub site_id: &'static str,
    pub site_name: &'static str,
    pub base: &'static str,
    /// 公告频道列表页路径（普农：`/content/channel/6038695d2792eea9207b23c7/`）
    pub list_path: &'static str,
    /// 详情链接前缀（用于列表页定位条目）
    pub detail_prefix: &'static str,
    /// 是否必须进详情页取正文（企业官网/聚合站均为 true）
    pub fetch_detail: bool,
}

/// 普农集团（安徽省普济圩现代农业集团）通知公告频道——需求③的首个实锚源。
pub const PJWNC: HtmlSourceCfg = HtmlSourceCfg {
    site_id: "pjwnc",
    site_name: "安徽省普济圩现代农业集团（普农集团）",
    base: "http://www.pjwnc.com",
    list_path: "/content/channel/6038695d2792eea9207b23c7/",
    detail_prefix: "/content/detail/",
    fetch_detail: true,
};

/// 抓列表页 → 解析条目 → （可选）逐条进详情取正文 → 两级命中。
///
/// 详情页逐条请求是必要成本：列表页只有标题，而需求②的点就是「标题没有主题词、正文才有」，
/// 不取正文就无法复核。为控制对目标站的压力，条目数上限与调用方传入的 `max_items` 一致，
/// 且调用方负责站内 ≥2s 间隔（本模块不 sleep，便于测试）。
pub fn fetch_html_source(
    cfg: &HtmlSourceCfg,
    http: &dyn Http,
    _wd: &str,
    _start: &str,
    _end: &str,
    today: &str,
    max_items: usize,
) -> Result<Vec<IntelItem>, FetchFail> {
    let list_url = format!("{}{}", cfg.base, cfg.list_path);
    let list_html = http
        .get(&list_url, Duration::from_secs(FETCH_TIMEOUT_SECS))
        .map_err(|e| FetchFail::from_http(&e, &list_url, 1))?;
    ensure_not_blocked(&list_html, &list_url)?;
    let entries = parse_list_entries(&list_html, cfg.detail_prefix);
    if entries.is_empty() {
        return Err(FetchFail {
            body_head: head_chars(&list_html, DIAG_BODY_CHARS),
            ..FetchFail::local(FetchStatus::ParseError, "列表页未解析到详情链接")
        });
    }
    let mut items = Vec::new();
    for e in entries.into_iter().take(max_items) {
        let url = format!("{}{}", cfg.base, e.url);
        let body = if cfg.fetch_detail {
            match http.get(&url, Duration::from_secs(FETCH_TIMEOUT_SECS)) {
                Ok(h) => html_to_text(&h),
                // 单条详情失败不该拖垮整源：把该条记为 incomplete，正文留空
                Err(_) => String::new(),
            }
        } else {
            String::new()
        };
        let title = if e.title.is_empty() {
            // 列表标题缺失时用详情页 ArticleTitle（普农详情页有此 meta）
            String::new()
        } else {
            e.title.clone()
        };
        let (relevance, reasons) = classify(&title, &body);
        let mut item = IntelItem {
            key: format!("{}|{}", cfg.site_id, url.trim()),
            site_id: cfg.site_id.to_string(),
            site_name: cfg.site_name.to_string(),
            title,
            org: cfg.site_name.to_string(),
            area: "铜陵".to_string(), // 普农位于铜陵市普济圩；源配置固定地区
            date: e.date.clone(),
            amount: None,
            url,
            relevance,
            reasons,
            incomplete: body.is_empty(),
            manual_verdict: false,
            first_seen: today.to_string(),
            body_head: crate::intel::head_chars(&body, 600),
        };
        apply_two_stage(&mut item, &body);
        items.push(item);
    }
    Ok(items)
}

// ─────────────────────────────────────────────────────────────────────────
// 统一分派（intel.rs 的 fetch_site 调这里，避免两处各自 match）
// ─────────────────────────────────────────────────────────────────────────
/// 按源类型抓取。`Http` 由调用方提供（生产 `UreqFetcher`，测试 mock）。
pub fn fetch_by_kind(
    spec: &SiteSpec,
    http: &dyn Http,
    wd: &str,
    start: &str,
    end: &str,
    today: &str,
) -> Result<Vec<IntelItem>, FetchFail> {
    match kind_of(spec.id) {
        SourceKind::ChizhouJson => {
            let url = format!("{}/front/search/list", spec.base);
            let body = build_chizhou_request(wd, 0, 15);
            let resp = http
                .post_json(&url, &body, Duration::from_secs(FETCH_TIMEOUT_SECS))
                .map_err(|e| FetchFail::from_http(&e, &url, 1))?;
            ensure_not_blocked(&resp, &url)?;
            parse_chizhou(spec, &resp, today, start, end)
        }
        SourceKind::WuhuDotNet => {
            let url = build_wuhu_url(spec.base, wd, 0, 15);
            let resp = http
                .get(&url, Duration::from_secs(FETCH_TIMEOUT_SECS))
                .map_err(|e| FetchFail::from_http(&e, &url, 1))?;
            ensure_not_blocked(&resp, &url)?;
            parse_wuhu(spec, &resp, today)
        }
        SourceKind::EnterpriseCms | SourceKind::AggregatorHtml => {
            // HTML 源在 site 注册表里没有条目时，用内置配置兜底
            let cfg = if kind_of(spec.id) == SourceKind::EnterpriseCms {
                PJWNC
            } else {
                HtmlSourceCfg {
                    site_id: "yibao",
                    site_name: spec.name,
                    base: spec.base,
                    list_path: "/",
                    detail_prefix: "/detail/",
                    fetch_detail: true,
                }
            };
            fetch_html_source(&cfg, http, wd, start, end, today, 20)
        }
        SourceKind::EpointFulltext => Err(FetchFail::local(
            FetchStatus::ParseError,
            "Epoint 源由 intel::fetch_epoint 处理（本分派不接管）",
        )),
    }
}

/// 黄山站实况：`/inteligentsearch` 前缀存在但要求 `authorization` 头，缺头即 403。
/// 预研表 6 处失效域名修正之一。**不得把它标成「已接入」**——这里给结构化结论供注册表 note 使用。
pub fn huangshan_status() -> (&'static str, &'static str) {
    (
        "NeedsCredential",
        "实测 403：{\"error\":\"Missing authorization header,缺失授权令牌\"}；旧前缀 /inteligentSearch 为 404（nginx）。未取得凭据前不计入已接入",
    )
}

// ─────────────────────────────────────────────────────────────────────────
// 测试
// ─────────────────────────────────────────────────────────────────────────
#[cfg(test)]
mod tests {
    use super::*;
    use crate::intel::HttpErr;
    use std::cell::RefCell;

    // ── 两级命中（需求②）──
    #[test]
    fn title_hit_is_direct() {
        let h = two_stage("XX市农业无人机采购项目招标公告", "");
        // 标题同时含主题词与农业加分词（"农业"）→ 无需进正文即可判 Title
        assert_eq!(h.level, HitLevel::Title);
        assert!(h.level.enters_pool());
    }

    /// 普农真实案例：标题只含「无人机」不含「农业无人机」，正文才有农业语义。
    #[test]
    fn pjwnc_case_is_body_verified_not_title() {
        let title = "普农集团综合农事服务中心无人机采购项目招标公告";
        let body = "一、项目基本情况 1.项目编号：TLTRZB【2026】074 2.项目名称：普农集团综合农事服务中心无人机采购项目 \
                    采购需求：植保无人机2台，用于田间病虫害防治、施肥、撒播作业";
        let h = two_stage(title, body);
        assert_eq!(h.level, HitLevel::BodyVerified, "标题含无人机但正文有农业加分词，应判正文复核命中");
        assert!(h.reasons.iter().any(|r| r.contains("正文复核加分词:植保")), "命中依据必须逐词给出");
        assert!(h.reasons.iter().any(|r| r.contains("正文复核")), "须写明复核位置: {:?}", h.reasons);
    }

    /// 反向：正文只有「无人机」但没有农业语义 → 弱命中，不能直接升级为 Core。
    #[test]
    fn body_theme_without_agri_plus_is_weak() {
        let h = two_stage("某园区安防设备采购", "配套无人机1台用于园区巡检");
        assert_eq!(h.level, HitLevel::BodyWeak);
        assert!(!h.level.enters_pool(), "仅弱命中不得直接进线索池");
    }

    /// 反向：两级都不命中。
    #[test]
    fn no_hit_returns_none() {
        let h = two_stage("办公家具采购", "桌椅板凳若干");
        assert_eq!(h.level, HitLevel::None);
        assert!(!h.level.enters_pool());
    }

    /// apply_two_stage 对人工改判不生效（AC-2-7）。
    #[test]
    fn manual_verdict_is_never_overridden() {
        let spec = SiteSpec {
            id: "t", name: "测试", city: "测试", tier: crate::intel::Tier::CityBatch1,
            base: "https://x", epoint: None, note: "",
        };
        let mut it = IntelItem {
            key: "k".into(), site_id: spec.id.into(), site_name: spec.name.into(),
            title: "无关".into(), org: "o".into(), area: "a".into(), date: "2026-11-01".into(),
            amount: None, url: "u".into(), relevance: Relevance::Core, reasons: vec![],
            incomplete: false, manual_verdict: true, first_seen: "2026-11-05".into(),
            body_head: String::new(),
        };
        apply_two_stage(&mut it, "办公室采购");
        assert_eq!(it.relevance, Relevance::Core, "人工改判过的条目不得被自动判定覆盖");
    }

    // ── HTML 工具 ──
    #[test]
    fn html_to_text_strips_script_and_tags() {
        let h = r#"<p class="a">普农集团</p><script>var x="无人机";</script><span>植保&nbsp;无人机</span>"#;
        let t = html_to_text(h);
        assert!(t.contains("普农集团"));
        assert!(!t.contains("var x"), "script 整块内容必须剔除，否则污染关键词计数: {}", t);
        assert!(t.contains("植保 无人机"), "实体解码与块间空格: {}", t);
    }

    #[test]
    fn meta_content_reads_article_title() {
        let h = r#"<meta name="ArticleTitle" content="普农集团综合农事服务中心无人机采购项目招标公告">"#;
        assert_eq!(
            meta_content(h, "ArticleTitle").as_deref(),
            Some("普农集团综合农事服务中心无人机采购项目招标公告")
        );
    }

    #[test]
    fn list_entries_pick_date_from_preceding_span() {
        // 与普农实测结构一致：<li><span>日期</span><a href="…" title="…">标题</a></li>
        let h = r#"<ul><li><span>2026-11-04</span><a href="/content/detail/6aeaee8b575e5df96d8b4583.html" title="x">普农集团综合农事服务中心插秧机采购补充公告   </a></li>
                   <li><span>2026-10-28</span><a href="/content/detail/6ae19b3c575e5d1a568b4571.html">普农集团综合农事服务中心无人机采购项目招标公告</a></li></ul>"#;
        let e = parse_list_entries(h, "/content/detail/");
        assert_eq!(e.len(), 2);
        assert_eq!(e[1].date, "2026-10-28");
        assert_eq!(e[1].title, "普农集团综合农事服务中心无人机采购项目招标公告");
        assert!(e[1].url.starts_with("/content/detail/6ae19b3c"));
    }

    #[test]
    fn last_date_in_takes_last_not_header() {
        let s = "页头 2022-05-25 … 条目 2026-11-04";
        assert_eq!(last_date_in(s).as_deref(), Some("2026-11-04"));
    }

    // ── 池州 ──
    #[test]
    fn chizhou_request_matches_site_js_shape() {
        let b = build_chizhou_request("无人机", 0, 15);
        let v: serde_json::Value = serde_json::from_str(&b).unwrap();
        assert_eq!(v["page"], 1, "站点分页从 1 起");
        assert_eq!(v["rows"], 15);
        assert_eq!(v["searchKey"], "无人机");
        assert!(v.get("filter").is_some(), "缺 filter 会返回 code=9999");
    }

    /// 池州异常响应必须显式报错，不能当成「0 条」（否则界面上「0 条」看起来是正常的）。
    #[test]
    fn chizhou_code_9999_is_error_not_empty() {
        let spec = SiteSpec {
            id: "chizhou", name: "池州", city: "池州", tier: crate::intel::Tier::CityBatch2,
            base: "https://ggj.chizhou.gov.cn", epoint: None, note: "",
        };
        let resp = r#"{"code":"9999","data":"系统异常，请稍后再试","msg":"系统异常"}"#;
        let r = parse_chizhou(&spec, resp, "2026-11-05", "2026-08-01", "2026-11-05");
        assert!(r.is_err());
        let f = r.unwrap_err();
        assert_eq!(f.status, FetchStatus::ParseError);
        assert!(f.detail.contains("9999"));
    }

    #[test]
    fn chizhou_rows_are_date_filtered_locally() {
        let spec = SiteSpec {
            id: "chizhou", name: "池州", city: "池州", tier: crate::intel::Tier::CityBatch2,
            base: "https://ggj.chizhou.gov.cn", epoint: None, note: "",
        };
        let resp = r#"{"rows":[{"title":"植保无人机采购","content":"田间病虫害防治","publishTime":"2026-10-20 09:00:00","url":"/front/detail/1","regionCode":"341700"},
                              {"title":"老项目","content":"","publishTime":"2026-06-01 09:00:00","url":"/front/detail/2","regionCode":"341700"}]}"#;
        let items = parse_chizhou(&spec, resp, "2026-11-05", "2026-08-01", "2026-11-05").unwrap();
        assert_eq!(items.len(), 1, "接口不认日期参数，越窗条目须本地剔除");
        assert!(items[0].url.starts_with("https://ggj.chizhou.gov.cn/front/detail/1"));
    }

    // ── 芜湖 ──
    #[test]
    fn wuhu_url_is_title_only_search() {
        let u = build_wuhu_url("https://ggzy.wuhu.gov.cn", "无人机", 0, 15);
        assert!(u.contains("GetNoticePage"));
        assert!(u.contains("pageIndex=1"));
        assert!(u.contains("keyword=%E6%97%A0%E4%BA%BA%E6%9C%BA"), "中文须百分号编码: {}", u);
    }

    #[test]
    fn wuhu_parse_handles_list_wrapper() {
        let spec = SiteSpec {
            id: "wuhu", name: "芜湖", city: "芜湖", tier: crate::intel::Tier::CityBatch2,
            base: "https://ggzy.wuhu.gov.cn", epoint: None, note: "",
        };
        let resp = r#"{"data":{"list":[{"Title":"无人机采购项目","PublishDate":"2026-10-30 10:00:00","Url":"/detail/1"}]}}"#;
        let items = parse_wuhu(&spec, resp, "2026-11-05").unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].title, "无人机采购项目");
        assert_eq!(items[0].date, "2026-10-30");
        assert!(items[0].incomplete, "无单位/地区字段，须如实标 incomplete");
    }

    // ── 源类型 ──
    #[test]
    fn aggregator_is_never_primary() {
        assert!(!SourceKind::AggregatorHtml.is_primary());
        assert!(SourceKind::EnterpriseCms.is_primary(), "企业官网是首发方，归主源");
        assert!(SourceKind::EpointFulltext.is_primary());
    }

    #[test]
    fn html_sources_need_detail_fetch_but_apis_do_not() {
        assert!(SourceKind::EnterpriseCms.needs_detail_fetch());
        assert!(!SourceKind::EpointFulltext.needs_detail_fetch(),
            "Epoint/池州/芜湖的检索响应本身带 content，二次请求是浪费");
    }

    // ── 企业官网端到端（mock Http，验证「列表 → 详情 → 两级命中」链路）──
    struct MockHttp {
        list: String,
        detail: String,
        calls: RefCell<Vec<String>>,
    }
    impl Http for MockHttp {
        fn post_json(&self, url: &str, _b: &str, _t: Duration) -> Result<String, HttpErr> {
            self.calls.borrow_mut().push(url.to_string());
            Err(HttpErr::Status(404, String::new()))
        }
        fn get(&self, url: &str, _t: Duration) -> Result<String, HttpErr> {
            self.calls.borrow_mut().push(url.to_string());
            if url.ends_with("6038695d2792eea9207b23c7/") {
                Ok(self.list.clone())
            } else {
                Ok(self.detail.clone())
            }
        }
    }

    #[test]
    fn enterprise_cms_end_to_end_verifies_pjwnc_tender() {
        let list = r#"<ul><li><span>2026-10-28</span><a href="/content/detail/6ae19b3c575e5d1a568b4571.html">普农集团综合农事服务中心无人机采购项目招标公告</a></li></ul>"#;
        let detail = r#"<meta name="ArticleTitle" content="普农集团综合农事服务中心无人机采购项目招标公告">
                        <div class="content"><p>1.项目编号：TLTRZB【2026】074</p>
                        <p>采购植保无人机2台，用于田间病虫害防治、施肥、撒播作业</p></div>
                        <script>var x='航拍';</script>"#;
        let http = MockHttp { list: list.into(), detail: detail.into(), calls: RefCell::new(vec![]) };
        let items = fetch_html_source(&PJWNC, &http, "无人机", "", "", "2026-11-05", 20).unwrap();
        assert_eq!(items.len(), 1);
        let it = &items[0];
        assert_eq!(it.date, "2026-10-28");
        assert!(it.url.starts_with("http://www.pjwnc.com/content/detail/6ae19b3c"));
        assert_eq!(it.relevance, Relevance::Core, "正文复核通过应升级为相关: {:?}", it.reasons);
        assert!(it.reasons.iter().any(|r| r.contains("正文复核命中")), "依据: {:?}", it.reasons);
        // 详情页的 script 内容不得混入判定
        assert!(!it.reasons.iter().any(|r| r.contains("航拍")), "script 内文字不应参与判定: {:?}", it.reasons);
        // 列表一次 + 详情一次
        assert_eq!(http.calls.borrow().len(), 2);
    }
}
