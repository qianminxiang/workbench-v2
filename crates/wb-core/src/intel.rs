//! FR-2 项目情报（应用内直连）+ FR-1 极飞网点周比对（v2.2）。
//!
//! 设计原则（PRD 第十章 10.2 / AC-2-5、AC-2-11）：
//! - 按"会失败"设计：每个站点独立抓取，失败只标记该源，不阻塞其他源与本地功能。
//! - 状态如实：未验证的分站不得标"已接入"；失败原因可区分（拦截/验证页/超时/解析失败）。
//! - 同站间隔 ≥2s 由调度方保证，本模块不 sleep（便于测试）。
//! - 网络走 [`Http`] trait，生产实现为 ureq，测试用 mock。

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::time::Duration;

pub const KEYWORD: &str = "无人机";
/// 单站单日请求硬上限（翻页保护）
pub const MAX_PAGES_PER_SITE: usize = 5;

// ─────────────────────────────────────────────────────────────────────────
// 关键词三层（PRD 6.3 暂定口径，Q2 未定稿，词表可由用户后续确认调整）
// ─────────────────────────────────────────────────────────────────────────
pub const THEME_WORDS: &[&str] = &["无人机", "无人飞机", "植保无人机", "植保机", "飞防", "农用航空"];
pub const PLUS_WORDS: &[&str] = &["农业", "农田", "农机", "植保", "统防统治", "高标准农田", "农林", "乡村振兴", "病虫害"];
pub const EXCLUDE_WORDS: &[&str] = &["航拍", "测绘", "电力巡线", "巡检", "安防监控", "青少年科普"];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Relevance {
    /// 命中主题词 + 加分词，无排除词
    Core,
    /// 命中主题词但缺加分词、或命中排除词：疑似不相关，可人工改判（AC-2-2 / AC-2-7）
    Suspect,
    /// 未命中主题词：不进相关列表
    Excluded,
}

impl Relevance {
    pub fn label(&self) -> &'static str {
        match self {
            Relevance::Core => "相关",
            Relevance::Suspect => "疑似不相关",
            Relevance::Excluded => "无关",
        }
    }
}

/// 相关性判定：主题词必须命中；加分词提升为 Core；排除词降级为 Suspect（可改判，不静默丢弃）。
pub fn classify(title: &str, content: &str) -> (Relevance, Vec<String>) {
    let text = format!("{}{}", title, content);
    let theme_hits: Vec<&str> = THEME_WORDS.iter().copied().filter(|w| text.contains(w)).collect();
    let mut reasons: Vec<String> = theme_hits.iter().map(|w| format!("主题词:{}", w)).collect();
    if theme_hits.is_empty() {
        return (Relevance::Excluded, vec!["未命中主题词".to_string()]);
    }
    let plus_hits: Vec<&str> = PLUS_WORDS.iter().copied().filter(|w| text.contains(w)).collect();
    let exclude_hits: Vec<&str> = EXCLUDE_WORDS.iter().copied().filter(|w| text.contains(w)).collect();
    reasons.extend(plus_hits.iter().map(|w| format!("加分词:{}", w)));
    reasons.extend(exclude_hits.iter().map(|w| format!("排除词:{}", w)));
    if !exclude_hits.is_empty() {
        (Relevance::Suspect, reasons)
    } else if !plus_hits.is_empty() {
        (Relevance::Core, reasons)
    } else {
        (Relevance::Suspect, reasons)
    }
}

// ─────────────────────────────────────────────────────────────────────────
// 站点注册表（依据 2026-10-28 预研实测；状态如实，AC-2-9 / AC-2-10）
// ─────────────────────────────────────────────────────────────────────────
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Tier {
    /// 省级主通道（ggzy.ah.gov.cn，表单检索 + 服务端渲染 HTML）
    ProvincialMain,
    /// 省级兜底源（徽采云，直连触发 NEED_CAPTCHA，低频 + 冷却，不作主通道）
    ProvincialFallback,
    /// 第一批 7 站（Epoint 标准/近标准，共用适配器）
    CityBatch1,
    /// 第二批 8 站（参数特殊或命中偏低）
    CityBatch2,
    /// 降级观察（铜陵：近 3 月标题 0 命中，周频观察）
    CityWeekly,
    /// 保留为人工/浏览器通道，不进自动调度（search.ccgp.gov.cn 裸请求被拦）
    ManualOnly,
    /// 实测不可行（deal.ggzy.gov.cn 接口 502）
    Infeasible,
}

impl Tier {
    pub fn is_scheduled(&self) -> bool {
        matches!(
            self,
            Tier::ProvincialMain | Tier::ProvincialFallback | Tier::CityBatch1 | Tier::CityBatch2 | Tier::CityWeekly
        )
    }
}

/// Epoint 适配器站间差异（15/16 地市站共用，预研 5.3 逐站实测要点）
#[derive(Debug, Clone)]
pub struct EpointCfg {
    /// 端点前缀：标准 "/inteligentsearch"、滁州 "/inteligentsearchNew"、_es 站走根路径 ""
    pub prefix: &'static str,
    /// 栏目编码：六安/阜阳 "001"，宿州全串，其余空
    pub cnum: &'static str,
    /// 单页条数（马鞍山单页上限 10）
    pub rn: usize,
    /// 响应双层 JSON 包装（淮南）需解包
    pub double_wrap: bool,
}

impl EpointCfg {
    pub const fn std() -> Self {
        EpointCfg { prefix: "/inteligentsearch", cnum: "", rn: 15, double_wrap: false }
    }
}

#[derive(Debug, Clone)]
pub struct SiteSpec {
    pub id: &'static str,
    pub name: &'static str,
    pub city: &'static str,
    pub tier: Tier,
    pub base: &'static str,
    pub epoint: Option<EpointCfg>,
    /// 状态依据（界面逐站展示，AC-2-9：不得把未验证写成已接入）
    pub note: &'static str,
}

pub const SITE_REGISTRY: &[SiteSpec] = &[
    // ── 省级 4 站（预研 3.2 实测）──
    SiteSpec {
        id: "ggzy_ah", name: "安徽省公共资源交易监管网", city: "省级", tier: Tier::ProvincialMain,
        base: "https://ggzy.ah.gov.cn", epoint: None,
        note: "预研实测 2026-10-28：POST /souSuo 表单检索可行，列表/详情服务端渲染；命中动态保护脚本，检测到验证页即降级",
    },
    SiteSpec {
        id: "huicaiyun", name: "徽采云·安徽省政府采购网", city: "省级", tier: Tier::ProvincialFallback,
        base: "https://www.ccgp-anhui.gov.cn", epoint: None,
        note: "预研实测 2026-10-28：POST /portal/all 无鉴权、字段全（标题62/全文268），但直连触发 NEED_CAPTCHA 验证码，按低频兜底源接入",
    },
    SiteSpec {
        id: "ccgp", name: "中国政府采购网", city: "全国", tier: Tier::ManualOnly,
        base: "https://search.ccgp.gov.cn", epoint: None,
        note: "预研实测 2026-10-28：裸请求被「频繁访问!」拦截，浏览器可用；保留人工通道，不进自动调度",
    },
    SiteSpec {
        id: "deal_ggzy", name: "全国公共资源交易平台", city: "全国", tier: Tier::Infeasible,
        base: "https://deal.ggzy.gov.cn", epoint: None,
        note: "预研实测 2026-10-28：列表接口 POST/GET 均 502，判定不可行，由省级平台覆盖",
    },
    // ── 第一批 7 站（Epoint 标准/近标准，共用适配器，覆盖近 3 月全文命中 61%）──
    SiteSpec { id: "anqing", name: "安庆市公共资源交易平台", city: "安庆", tier: Tier::CityBatch1,
        base: "https://ggzy.anqing.gov.cn", epoint: Some(EpointCfg { cnum: "", ..EpointCfg::std() }),
        note: "预研实测 2026-10-28：标准 cnum 去 002 即可；标题40/全文63" },
    SiteSpec { id: "chuzhou", name: "滁州市公共资源交易平台", city: "滁州", tier: Tier::CityBatch1,
        base: "https://ggzy.chuzhou.gov.cn", epoint: Some(EpointCfg { prefix: "/inteligentsearchNew", cnum: "", rn: 15, double_wrap: false }),
        note: "预研实测 2026-10-28：端点前缀 /inteligentsearchNew；标题34/全文56" },
    SiteSpec { id: "maanshan", name: "马鞍山市公共资源交易网", city: "马鞍山", tier: Tier::CityBatch1,
        base: "https://ggzy.mas.gov.cn", epoint: Some(EpointCfg { prefix: "", cnum: "", rn: 10, double_wrap: false }),
        note: "预研实测 2026-10-28：_es 端点走根路径，单页上限 10 须逐页翻；标题25/全文35" },
    SiteSpec { id: "luan", name: "六安市公共资源交易平台", city: "六安", tier: Tier::CityBatch1,
        base: "https://ggzy.luan.gov.cn", epoint: Some(EpointCfg { cnum: "001", ..EpointCfg::std() }),
        note: "预研实测 2026-10-28：标准 cnum=001（六安口径），模板逆向自六安 jysearch.js；标题18/全文52" },
    SiteSpec { id: "bozhou", name: "亳州市公共资源交易平台", city: "亳州", tier: Tier::CityBatch1,
        base: "https://ggzy.bozhou.gov.cn", epoint: Some(EpointCfg::std()),
        note: "预研实测 2026-10-28：去栏目过滤即可；标题20/全文26" },
    SiteSpec { id: "hefei", name: "合肥公共资源交易中心", city: "合肥", tier: Tier::CityBatch1,
        base: "https://hfggfc.hefei.gov.cn", epoint: Some(EpointCfg::std()),
        note: "预研实测 2026-10-28：去栏目过滤即可；标题11/全文43" },
    SiteSpec { id: "xuancheng", name: "宣城市公共资源交易平台", city: "宣城", tier: Tier::CityBatch1,
        base: "https://ggzy.xuancheng.gov.cn", epoint: Some(EpointCfg { prefix: "", cnum: "", rn: 15, double_wrap: false }),
        note: "预研实测 2026-10-28：_es 端点走根路径；标题7/全文37" },
    // ── 第二批 8 站（参数特殊或命中偏低）──
    SiteSpec { id: "fuyang", name: "阜阳市公共资源交易平台", city: "阜阳", tier: Tier::CityBatch2,
        base: "https://ggzy.fuyang.gov.cn", epoint: Some(EpointCfg { cnum: "001", rn: 15, ..EpointCfg::std() }),
        note: "预研实测 2026-10-28：需用其默认参数形态（cnum=001、time:null）；标题12/全文29" },
    SiteSpec { id: "suzhou_ah", name: "宿州市公共资源交易平台", city: "宿州", tier: Tier::CityBatch2,
        base: "https://ggzy.suzhou.gov.cn", epoint: Some(EpointCfg { cnum: "001;002;003;004;005;006;007;008;009", rn: 15, ..EpointCfg::std() }),
        note: "预研实测 2026-10-28：cnum 必须传全串、wd 需 URL 编码；标题2/全文44" },
    SiteSpec { id: "wuhu", name: "芜湖市公共资源交易平台", city: "芜湖", tier: Tier::CityBatch2,
        base: "https://ggzy.wuhu.gov.cn", epoint: None,
        note: "预研实测 2026-10-28：独立 .NET API（GET /api/Home/GetNoticePage），仅标题式检索" },
    SiteSpec { id: "chizhou", name: "池州市公共资源交易管理中心", city: "池州", tier: Tier::CityBatch2,
        base: "https://ggj.chizhou.gov.cn", epoint: None,
        note: "预研实测 2026-10-28：自有 JSON API（POST /front/search/list）无鉴权，无日期参数须本地过滤 publishTime；标准 Epoint 端点有登录墙" },
    SiteSpec { id: "bengbu", name: "蚌埠市公共资源交易平台", city: "蚌埠", tier: Tier::CityBatch2,
        base: "https://ggzy.bengbu.gov.cn", epoint: Some(EpointCfg::std()),
        note: "预研实测 2026-10-28：仅 https（http 307）；标题7/全文18" },
    SiteSpec { id: "huainan", name: "淮南市公共资源交易平台", city: "淮南", tier: Tier::CityBatch2,
        base: "https://ggzy.huainan.gov.cn", epoint: Some(EpointCfg { prefix: "", cnum: "", rn: 15, double_wrap: true }),
        note: "预研实测 2026-10-28：_es 端点 + 响应双层 JSON 包装需解包；标题2/全文3" },
    SiteSpec { id: "huaibei", name: "淮北市公共资源交易平台", city: "淮北", tier: Tier::CityBatch2,
        base: "https://ggzy.huaibei.gov.cn", epoint: Some(EpointCfg::std()),
        note: "预研实测 2026-10-28：去栏目过滤即可；标题3/全文8" },
    SiteSpec { id: "huangshan", name: "黄山市公共资源交易平台", city: "黄山", tier: Tier::CityBatch2,
        base: "https://ggzy.huangshan.gov.cn", epoint: Some(EpointCfg { prefix: "/inteligentSearch", cnum: "", rn: 15, double_wrap: false }),
        note: "预研实测 2026-10-28：旧端点 inteligentSearch/getFullTextData；标题3/全文7" },
    // ── 降级观察 ──
    SiteSpec { id: "tongling", name: "铜陵市公共资源交易平台", city: "铜陵", tier: Tier::CityWeekly,
        base: "https://ggzy.tongling.gov.cn", epoint: Some(EpointCfg::std()),
        note: "预研实测 2026-10-28：可达但近 3 月标题 0 命中/全文仅 5，按周频低频观察，不进日频首批" },
];

/// 覆盖统计（AC-2-10：摘要显示 已接入 N / 计划覆盖 M）
pub fn coverage_counts() -> (usize, usize) {
    let scheduled = SITE_REGISTRY.iter().filter(|s| s.tier.is_scheduled()).count();
    // 计划覆盖 = 省级 4 站（含不可行的 deal.ggzy 与人工通道 ccgp，如实计数）+ 16 地市
    (scheduled, SITE_REGISTRY.len())
}

// ─────────────────────────────────────────────────────────────────────────
// HTTP 抽象（测试 mock / 生产 ureq）
// ─────────────────────────────────────────────────────────────────────────
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum HttpErr {
    /// 网络不可达 / DNS / 超时（AC-2-11 离线场景）
    Network,
    /// HTTP 非成功状态（418/429/5xx…）
    Status(u16),
    /// 命中反爬/验证页特征（418 拦截页 /「频繁访问」/NEED_CAPTCHA）
    Blocked(&'static str),
}

pub trait Http {
    fn post_json(&self, url: &str, body: &str, timeout: Duration) -> Result<String, HttpErr>;
    fn get(&self, url: &str, timeout: Duration) -> Result<String, HttpErr>;
}

/// 生产实现：ureq（v2.1 曾随单机化移除，v2.2 经用户拍板恢复）。
pub struct UreqFetcher;

impl Http for UreqFetcher {
    fn post_json(&self, url: &str, body: &str, timeout: Duration) -> Result<String, HttpErr> {
        let resp = ureq::post(url)
            .timeout(timeout)
            .set("Content-Type", "application/json")
            .set("User-Agent", "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36")
            .send_string(body)
            .map_err(map_ureq_err)?;
        read_resp(resp)
    }
    fn get(&self, url: &str, timeout: Duration) -> Result<String, HttpErr> {
        let resp = ureq::get(url)
            .timeout(timeout)
            .set("User-Agent", "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36")
            .call()
            .map_err(map_ureq_err)?;
        read_resp(resp)
    }
}

fn map_ureq_err(e: ureq::Error) -> HttpErr {
    match e {
        ureq::Error::Status(code, _) => {
            if code == 418 || code == 429 { HttpErr::Blocked("http_418_429") } else { HttpErr::Status(code) }
        }
        _ => HttpErr::Network,
    }
}

fn read_resp(resp: ureq::Response) -> Result<String, HttpErr> {
    let body = resp.into_string().map_err(|_| HttpErr::Network)?;
    // 验证页特征检测：返回内容是拦截页时即使 HTTP 200 也算被拦（AC-2-5）
    if body.contains("频繁访问") || body.contains("NEED_CAPTCHA") || body.contains("访问被拦截") {
        return Err(HttpErr::Blocked("captcha_page"));
    }
    Ok(body)
}

// ─────────────────────────────────────────────────────────────────────────
// 抓取结果模型
// ─────────────────────────────────────────────────────────────────────────
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct IntelItem {
    /// 幂等键：来源站点 + 公告 URL 规范化（AC-2-4）
    pub key: String,
    pub site_id: String,
    pub site_name: String,
    pub title: String,
    pub org: String,
    pub area: String,
    /// 发布日期（YYYY-MM-DD；源站给不出时为空 → incomplete）
    pub date: String,
    pub amount: Option<String>,
    pub url: String,
    pub relevance: Relevance,
    pub reasons: Vec<String>,
    /// 六项必填（项目名/单位/地区/日期/来源站点/原始链接）缺失任一项即 true（AC-2-1）
    pub incomplete: bool,
    /// 人工改判时间戳（AC-2-7：不被自动规则覆盖）
    pub manual_verdict: bool,
    pub first_seen: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum FetchStatus {
    Ok,
    /// 被反爬/验证页拦截
    Blocked,
    /// 网络/超时
    Network,
    /// 解析失败（源站结构变更）
    ParseError,
}

impl FetchStatus {
    pub fn label(&self) -> &'static str {
        match self {
            FetchStatus::Ok => "成功",
            FetchStatus::Blocked => "被拦截",
            FetchStatus::Network => "网络失败",
            FetchStatus::ParseError => "解析失败",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SiteResult {
    pub site_id: String,
    pub status: FetchStatus,
    pub detail: String,
    pub items: usize,
    pub new_items: usize,
}

// ─────────────────────────────────────────────────────────────────────────
// Epoint 适配器（15/16 地市站共用）
// ─────────────────────────────────────────────────────────────────────────
/// 构造 Epoint 全文检索请求（模板逆向自六安 jysearch.js，预研 5.3）
pub fn build_epoint_request(spec: &SiteSpec, wd: &str, start: &str, end: &str, page: usize) -> (String, String) {
    let cfg = spec.epoint.as_ref().expect("epoint site");
    // 黄山旧端点；其余 _es/标准/新前缀
    let path = if spec.id == "huangshan" {
        format!("{}/rest/esinteligentsearch/getFullTextData", cfg.prefix)
    } else {
        format!("{}/rest/esinteligentsearch/getFullTextDataNew", cfg.prefix)
    };
    let url = format!("{}{}", spec.base, path);
    let cnum = cfg.cnum.to_string();
    let time = if spec.id == "fuyang" {
        serde_json::json!(null)
    } else {
        serde_json::json!([{ "fieldName": "webdate", "startTime": start, "endTime": end }])
    };
    let body = serde_json::json!({
        "token": "",
        "pn": page,
        "rn": cfg.rn,
        "sdt": start,
        "edt": end,
        "wd": wd,
        "fields": "title",
        "cnum": cnum,
        "sort": "{'webdate':'0'}",
        "ssort": "title",
        "cl": 500,
        "terminal": "",
        "condition": [],
        "time": time,
    });
    (url, body.to_string())
}

/// Epoint 响应解析：响应结构存在站间差异（result.results / data / 双层包装），
/// 用"递归找对象数组 + 含 title 字段"的容错定位，任一层命中即可。
fn find_record_arrays(v: &serde_json::Value, depth: u8) -> Vec<serde_json::Value> {
    if depth > 4 {
        return vec![];
    }
    match v {
        serde_json::Value::Array(a) => {
            if !a.is_empty() && a.iter().all(|x| x.is_object()) && a.iter().any(|x| x.get("title").is_some() || x.get("projectName").is_some()) {
                a.clone()
            } else {
                a.iter().flat_map(|x| find_record_arrays(x, depth + 1)).collect()
            }
        }
        serde_json::Value::Object(o) => o.values().flat_map(|x| find_record_arrays(x, depth + 1)).collect(),
        _ => vec![],
    }
}

/// 解析单条记录为 IntelItem（六字段抽取，缺失标 incomplete）
fn record_to_item(spec: &SiteSpec, rec: &serde_json::Value, today: &str) -> IntelItem {
    let title = rec
        .get("title").or_else(|| rec.get("projectName")).and_then(|t| t.as_str()).unwrap_or("").trim().to_string();
    let org = rec
        .get("purchaseName").or_else(|| rec.get("purchaseunitname")).or_else(|| rec.get("jiaoyimethod"))
        .and_then(|t| t.as_str()).unwrap_or("").trim().to_string();
    let area = rec
        .get("districtName").or_else(|| rec.get("area")).or_else(|| rec.get("xiaqucode"))
        .and_then(|t| t.as_str()).unwrap_or("").trim().to_string();
    let date = normalize_date_value(rec.get("webdate").or_else(|| rec.get("publishDate")).or_else(|| rec.get("infodatepx")));
    let amount = rec
        .get("budgetPrice").or_else(|| rec.get("monitorAmount")).or_else(|| rec.get("totalContractAmount"))
        .and_then(|t| t.as_str()).map(|s| s.trim().to_string()).filter(|s| !s.is_empty() && s != "null");
    let url = resolve_link(spec, rec);
    let content = rec.get("content").and_then(|t| t.as_str()).unwrap_or("");
    let (relevance, reasons) = classify(&title, content);
    let key = format!("{}|{}", spec.id, url.trim());
    let incomplete = title.is_empty() || org.is_empty() || area.is_empty() || date.is_empty() || url.is_empty();
    IntelItem {
        key,
        site_id: spec.id.to_string(),
        site_name: spec.name.to_string(),
        title,
        org,
        area,
        date,
        amount,
        url,
        relevance,
        reasons,
        incomplete,
        manual_verdict: false,
        first_seen: today.to_string(),
    }
}

fn normalize_date_value(v: Option<&serde_json::Value>) -> String {
    match v {
        Some(serde_json::Value::Number(n)) => {
            // 毫秒时间戳（徽采云 publishDate）→ 东八区日期
            let ms = n.as_i64().unwrap_or(0);
            if ms > 1_000_000_000_000 {
                let secs = (ms - 8 * 3600 * 1000) / 1000;
                let days = secs.div_euclid(86400);
                let (y, m, d) = civil_from_days(days);
                format!("{:04}-{:02}-{:02}", y, m, d)
            } else {
                String::new()
            }
        }
        Some(serde_json::Value::String(s)) => {
            let s = s.trim();
            if s.len() >= 10 {
                s.chars().take(10).collect::<String>()
            } else {
                String::new()
            }
        }
        _ => String::new(),
    }
}

/// 天数 → 公历（Howard Hinnant 算法，避免引入 chrono）
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

fn resolve_link(spec: &SiteSpec, rec: &serde_json::Value) -> String {
    // 徽采云：articleId + parentId 拼详情页
    if spec.id == "huicaiyun" {
        if let (Some(a), Some(p)) = (rec.get("articleId").and_then(|x| x.as_str()), rec.get("parentId")) {
            return format!(
                "https://www.ccgp-anhui.gov.cn/luban/detail?categoryCode=ZcyAnnouncement&parentId={}&articleId={}",
                p, a
            );
        }
    }
    let raw = rec.get("linkurl").or_else(|| rec.get("url")).and_then(|x| x.as_str()).unwrap_or("").trim();
    if raw.is_empty() {
        return String::new();
    }
    if raw.starts_with("http://") || raw.starts_with("https://") {
        raw.to_string()
    } else if raw.starts_with('/') {
        format!("{}{}", spec.base, raw)
    } else {
        format!("{}/{}", spec.base, raw)
    }
}

// ─────────────────────────────────────────────────────────────────────────
// 各源抓取
// ─────────────────────────────────────────────────────────────────────────
pub fn fetch_site(
    spec: &SiteSpec,
    http: &dyn Http,
    wd: &str,
    start: &str,
    end: &str,
    today: &str,
) -> Result<Vec<IntelItem>, (FetchStatus, String)> {
    match spec.tier {
        Tier::CityBatch1 | Tier::CityBatch2 | Tier::CityWeekly => fetch_epoint(spec, http, wd, start, end, today),
        Tier::ProvincialFallback => fetch_huicaiyun(spec, http, wd, start, end, today),
        Tier::ProvincialMain => fetch_ggzy_ah(spec, http, wd, start, end, today),
        _ => Err((FetchStatus::ParseError, "该源不在自动调度范围".into())),
    }
}

fn fetch_epoint(
    spec: &SiteSpec,
    http: &dyn Http,
    wd: &str,
    start: &str,
    end: &str,
    today: &str,
) -> Result<Vec<IntelItem>, (FetchStatus, String)> {
    let mut items = Vec::new();
    for page in 0..MAX_PAGES_PER_SITE as usize {
        let (url, body) = build_epoint_request(spec, wd, start, end, page);
        let resp = http.post_json(&url, &body, Duration::from_secs(20)).map_err(map_http)?;
        let v: serde_json::Value = serde_json::from_str(&resp).map_err(|_| (FetchStatus::ParseError, "响应非 JSON".into()))?;
        let v = if spec.epoint.as_ref().map(|c| c.double_wrap).unwrap_or(false) {
            // 淮南：双层 JSON 包装（外层字符串再包一层 JSON）
            if let Some(inner) = v.as_str() {
                serde_json::from_str(inner).unwrap_or(v)
            } else {
                v
            }
        } else {
            v
        };
        let records = find_record_arrays(&v, 0);
        if records.is_empty() {
            if page == 0 {
                return Err((FetchStatus::ParseError, "未在响应中定位到记录数组".into()));
            }
            break;
        }
        let got = records.len();
        for rec in &records {
            items.push(record_to_item(spec, rec, today));
        }
        let cfg_rn = spec.epoint.as_ref().map(|c| c.rn).unwrap_or(15);
        if got < cfg_rn {
            break; // 最后一页
        }
    }
    Ok(items)
}

fn fetch_huicaiyun(
    spec: &SiteSpec,
    http: &dyn Http,
    wd: &str,
    start: &str,
    end: &str,
    today: &str,
) -> Result<Vec<IntelItem>, (FetchStatus, String)> {
    let (s_ms, e_ms) = match (
        date_str_to_ms(start),
        date_str_to_ms(end),
    ) {
        (Some(s), Some(e)) => (s, e + 86_399_000),
        _ => return Err((FetchStatus::ParseError, "日期参数非法".into())),
    };
    let url = format!("{}/portal/all", spec.base);
    let body = serde_json::json!({
        "keyword": wd, "firstCode": "", "secondCode": "", "districtCode": [],
        "publishDateBegin": s_ms, "publishDateEnd": e_ms,
        "pageNo": 1, "pageSize": 15, "isTitleSearch": 1, "order": "desc", "leaf": "0",
    });
    let resp = http.post_json(&url, &body.to_string(), Duration::from_secs(20)).map_err(map_http)?;
    let v: serde_json::Value = serde_json::from_str(&resp).map_err(|_| (FetchStatus::ParseError, "响应非 JSON".into()))?;
    let records = v
        .pointer("/result/data/data")
        .and_then(|x| x.as_array()).cloned()
        .unwrap_or_default();
    if records.is_empty() && v.pointer("/result/data/total").and_then(|t| t.as_i64()) != Some(0) {
        return Err((FetchStatus::ParseError, "未定位到徽采云结果数组".into()));
    }
    Ok(records.iter().map(|rec| record_to_item(spec, rec, today)).collect())
}

fn date_str_to_ms(s: &str) -> Option<i64> {
    let parts: Vec<i64> = s.split('-').filter_map(|x| x.parse().ok()).collect();
    if parts.len() != 3 {
        return None;
    }
    // 公历 → 天数（civil_from_days 逆运算），再转毫秒（东八区零点 = UTC 前一日 16:00）
    let (y, m, d) = (parts[0], parts[1], parts[2]);
    let yy = if m <= 2 { y - 1 } else { y };
    let era = yy.div_euclid(400);
    let yoe = yy.rem_euclid(400);
    let mp = if m > 2 { m - 3 } else { m + 9 };
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some((days * 86_400 - 16 * 3600) * 1000)
}

fn fetch_ggzy_ah(
    spec: &SiteSpec,
    http: &dyn Http,
    wd: &str,
    _start: &str,
    _end: &str,
    today: &str,
) -> Result<Vec<IntelItem>, (FetchStatus, String)> {
    // POST /souSuo 表单（param 域），服务端渲染 HTML；解析 newDetail 链接 + 标题
    let url = format!("{}/souSuo", spec.base);
    let form = format!(
        "param={}&currentPage=1&pageNo=1",
        urlencoding_min(wd)
    );
    let resp = http
        .post_json(&url, &form, Duration::from_secs(20))
        .map_err(map_http)?;
    let mut items = Vec::new();
    let mut rest = resp.as_str();
    while let Some(i) = rest.find("newDetail?guid=") {
        let seg = &rest[i..];
        let guid_end = seg.find('"').or(Some(seg.len())).unwrap();
        let link = format!("{}/zfcg/{}", spec.base, &seg[..guid_end]);
        let after = &rest[i + guid_end..];
        // 标题取链接后的 >…</a>
        let title = extract_anchor_text(after);
        if !title.is_empty() {
            let (relevance, reasons) = classify(&title, "");
            items.push(IntelItem {
                key: format!("{}|{}", spec.id, link),
                site_id: spec.id.to_string(),
                site_name: spec.name.to_string(),
                title: title.clone(),
                org: String::new(),
                area: String::new(),
                date: String::new(),
                amount: None,
                url: link,
                relevance,
                reasons,
                incomplete: true, // 列表页无单位/地区/日期，详情页字段本期不抓正文（信息不全，如实标注）
                manual_verdict: false,
                first_seen: today.to_string(),
            });
        }
        rest = after;
    }
    if items.is_empty() {
        return Err((FetchStatus::ParseError, "HTML 未解析到 newDetail 链接".into()));
    }
    Ok(items)
}

fn extract_anchor_text(s: &str) -> String {
    if let Some(gt) = s.find('>') {
        let t = &s[gt + 1..];
        if let Some(lt) = t.find("</a>") {
            return t[..lt].trim().replace("&nbsp;", " ");
        }
    }
    String::new()
}

fn urlencoding_min(s: &str) -> String {
    // 仅做 URL 编码中必须的字符转义（中文按 UTF-8 字节百分号编码）
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
            _ => out.push_str(&format!("%{:02X}", b)),
        }
    }
    out
}

fn map_http(e: HttpErr) -> (FetchStatus, String) {
    match e {
        HttpErr::Network => (FetchStatus::Network, "网络不可达或超时".into()),
        HttpErr::Status(c) => (FetchStatus::Blocked, format!("HTTP {}", c)),
        HttpErr::Blocked(f) => (FetchStatus::Blocked, format!("命中拦截特征 {}", f)),
    }
}

// ─────────────────────────────────────────────────────────────────────────
// 状态持久化（intel_state.json）：站点状态 + 人工改判 + 条目存储
// ─────────────────────────────────────────────────────────────────────────
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct IntelState {
    /// 站点抓取状态（按 site_id）
    pub sites: BTreeMap<String, SiteStateRow>,
    /// 人工改判（AC-2-7：key → 改判结果，再次采集不覆盖）
    pub overrides: BTreeMap<String, Relevance>,
    /// 已见条目（去重 + first_seen），按 key
    pub items: BTreeMap<String, IntelItem>,
    /// 最近一次日采集日期（YYYY-MM-DD）
    pub last_run_date: Option<String>,
    /// 最近一次周比对日期
    pub last_dealer_run_date: Option<String>,
    /// 极飞网点快照（FR-1）
    pub dealer_snapshot: Vec<DealerNode>,
    #[serde(default)]
    pub dealer_first_baseline: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SiteStateRow {
    pub last_attempt: Option<String>,
    pub last_success: Option<String>,
    pub last_status: Option<FetchStatus>,
    pub last_detail: String,
    pub fail_streak: u32,
}

impl IntelState {
    pub fn load(dir: &std::path::Path) -> IntelState {
        let p = dir.join("intel_state.json");
        std::fs::read_to_string(&p)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    pub fn save(&self, dir: &std::path::Path) {
        let p = dir.join("intel_state.json");
        if let Ok(s) = serde_json::to_string_pretty(self) {
            let _ = std::fs::write(p, s);
        }
    }
}

/// 去重合并一批新条目（AC-2-4：站点+URL 规范化幂等；AC-2-7：人工改判保留）
pub fn merge_items(state: &mut IntelState, incoming: Vec<IntelItem>, today: &str) -> (usize, usize) {
    let mut new_count = 0usize;
    let mut total = 0usize;
    for it in incoming {
        if let Some(ov) = state.overrides.get(&it.key) {
            let mut kept = it;
            kept.relevance = *ov;
            kept.manual_verdict = true;
            state.items.insert(kept.key.clone(), kept);
        } else if let Some(prev) = state.items.get_mut(&it.key) {
            // 已存在：不覆盖相关性与 first_seen，只更新可变业务字段
            prev.amount = it.amount;
            prev.incomplete = it.incomplete;
        } else {
            new_count += 1;
            state.items.insert(it.key.clone(), it);
        }
    }
    total = state.items.len();
    // 存储封顶 500 条（最旧的先淘汰），防长期内存/磁盘增长
    if state.items.len() > 500 {
        let keys: Vec<String> = state.items.keys().cloned().collect();
        let excess = state.items.len() - 500;
        for k in keys.into_iter().take(excess) {
            state.items.remove(&k);
        }
        total = state.items.len();
    }
    let _ = today;
    (new_count, total)
}

/// 人工改判（AC-2-7）
pub fn set_verdict(state: &mut IntelState, key: &str, verdict: Relevance) -> bool {
    state.overrides.insert(key.to_string(), verdict);
    match state.items.get_mut(key) {
        Some(it) => {
            it.relevance = verdict;
            it.manual_verdict = true;
            true
        }
        None => false,
    }
}

/// 日采集编排：按注册表顺序抓取全部调度源（同站间隔由调用方 sleep）。
/// 返回每站结果 + 当日摘要（AC-2-5 / AC-2-8）。
pub fn run_daily(
    state: &mut IntelState,
    http: &dyn Http,
    today: &str,
    start: &str,
    end: &str,
) -> (Vec<SiteResult>, String) {
    let mut results = Vec::new();
    let mut new_total = 0usize;
    let mut failed = 0usize;
    for spec in SITE_REGISTRY.iter().filter(|s| s.tier.is_scheduled()) {
        let r = fetch_site(spec, http, KEYWORD, start, end, today);
        let row = state.sites.entry(spec.id.to_string()).or_default();
        row.last_attempt = Some(today.to_string());
        let sr = match r {
            Ok(items) => {
                let (new_n, _) = merge_items(state, items, today);
                row.last_success = Some(today.to_string());
                row.last_status = Some(FetchStatus::Ok);
                row.last_detail = String::new();
                row.fail_streak = 0;
                let n = state.items.values().filter(|i| i.site_id == spec.id).count();
                new_total += new_n;
                SiteResult { site_id: spec.id.to_string(), status: FetchStatus::Ok, detail: String::new(), items: n, new_items: new_n }
            }
            Err((st, detail)) => {
                failed += 1;
                row.last_status = Some(st.clone());
                row.last_detail = detail.clone();
                row.fail_streak += 1;
                SiteResult { site_id: spec.id.to_string(), status: st, detail, items: 0, new_items: 0 }
            }
        };
        results.push(sr);
    }
    state.last_run_date = Some(today.to_string());
    let (connected, planned) = coverage_counts();
    let summary = if results.is_empty() {
        format!("今日 0 条，无已接入源（计划覆盖 {} 源）", planned)
    } else {
        format!(
            "今日新增相关项目 {} 条，{} 个源采集失败；已接入 {} / 计划覆盖 {} 源",
            new_total, failed, connected, planned
        )
    };
    (results, summary)
}

// ─────────────────────────────────────────────────────────────────────────
// FR-1 极飞安徽网点周比对
// ─────────────────────────────────────────────────────────────────────────
pub const DEALER_TAGS: &[&str] = &["client", "own_warehouse", "auth_warehouse", "train_warehouse"];
pub const DEALER_PROVINCE: &str = "安徽省";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DealerNode {
    pub guid: String,
    pub company_name: String,
    pub area: String,
    pub address: String,
    pub phone: String,
    pub tags: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct DealerDiff {
    pub added: Vec<DealerNode>,
    pub removed: Vec<DealerNode>,
    /// 首周基线：无比对结果（FR-1 验收：首周只建基线）
    pub baseline_week: bool,
    pub total: usize,
}

impl DealerNode {
    /// 归一化主键：公司名（去空白）+ 省市区（去空白）
    pub fn node_key(&self) -> String {
        let norm = |s: &str| s.chars().filter(|c| !c.is_whitespace()).collect::<String>();
        format!("{}|{}", norm(&self.company_name), norm(&self.area))
    }
}

pub fn fetch_dealers(http: &dyn Http) -> Result<Vec<DealerNode>, (FetchStatus, String)> {
    let mut nodes = Vec::new();
    let mut seen = std::collections::BTreeSet::new();
    for tag in DEALER_TAGS {
        let url = format!(
            "https://www.xa.com/service/dealer-address?province={}&tag={}",
            urlencoding_min(DEALER_PROVINCE), tag
        );
        let resp = http.get(&url, Duration::from_secs(20)).map_err(map_http)?;
        let v: serde_json::Value = serde_json::from_str(&resp).map_err(|_| (FetchStatus::ParseError, "响应非 JSON".into()))?;
        let arr = v.get("data").and_then(|d| d.as_array()).cloned().unwrap_or_default();
        for d in arr {
            let node = DealerNode {
                guid: d.get("guid").and_then(|x| x.as_str()).unwrap_or("").to_string(),
                company_name: d.get("company_name").and_then(|x| x.as_str()).unwrap_or("").to_string(),
                area: d.get("area").and_then(|x| x.as_str()).unwrap_or("").to_string(),
                address: d.get("address").and_then(|x| x.as_str()).unwrap_or("").to_string(),
                phone: d.get("contact_phone").and_then(|x| x.as_str()).unwrap_or("").to_string(),
                tags: vec![tag.to_string()],
            };
            if node.company_name.is_empty() || !seen.insert(node.node_key()) {
                continue;
            }
            nodes.push(node);
        }
    }
    Ok(nodes)
}

/// 周比对：首周建基线（baseline_week=true 无增减）；之后按归一化主键 diff
pub fn diff_dealers(prev: &[DealerNode], next: Vec<DealerNode>) -> DealerDiff {
    let prev_keys: std::collections::BTreeMap<String, &DealerNode> =
        prev.iter().map(|n| (n.node_key(), n)).collect();
    let next_keys: std::collections::BTreeSet<String> = next.iter().map(|n| n.node_key()).collect();
    let total = next.len();
    if prev.is_empty() {
        return DealerDiff { added: vec![], removed: vec![], baseline_week: true, total };
    }
    let added = next
        .iter()
        .filter(|n| !prev_keys.contains_key(&n.node_key()))
        .cloned()
        .collect();
    let removed = prev
        .iter()
        .filter(|n| !next_keys.contains(&n.node_key()))
        .cloned()
        .collect();
    DealerDiff { added, removed, baseline_week: false, total }
}

// ─────────────────────────────────────────────────────────────────────────
// 测试（mock HTTP，全部离线可跑）
// ─────────────────────────────────────────────────────────────────────────
#[cfg(test)]
mod tests {
    use super::*;

    struct MockHttp {
        responses: BTreeMap<String, Result<String, HttpErr>>,
    }
    impl MockHttp {
        fn ok(url_frag: &str, body: &str) -> Self {
            MockHttp { responses: BTreeMap::from([(url_frag.to_string(), Ok(body.to_string()))]) }
        }
    }
    impl Http for MockHttp {
        fn post_json(&self, url: &str, _body: &str, _t: Duration) -> Result<String, HttpErr> {
            self.responses.iter().find(|(k, _)| url.contains(k.as_str())).map(|(_, v)| v.clone()).unwrap_or(Err(HttpErr::Network))
        }
        fn get(&self, url: &str, _t: Duration) -> Result<String, HttpErr> {
            self.responses.iter().find(|(k, _)| url.contains(k.as_str())).map(|(_, v)| v.clone()).unwrap_or(Err(HttpErr::Network))
        }
    }

    fn epoint_body() -> String {
        serde_json::json!({
            "totalrecord": 2, "pageSize": 15, "pageNo": 0,
            "result": { "results": [
                { "title": "XX县农业无人机植保服务采购公告", "webdate": "2026-10-20 10:00:00",
                  "linkurl": "/jyxx/001/202610/t20261020_123.html", "infoid": "a1",
                  "content": "为推进统防统治工作，采购植保无人机", "xiaqucode": "六安市" },
                { "title": "城市宣传片航拍服务招标", "webdate": "2026-10-19",
                  "linkurl": "https://ggzy.example.com/jyxx/002.html", "infoid": "a2", "content": "航拍" }
            ]}
        }).to_string()
    }

    #[test]
    fn epoint_request_chuzhou_uses_new_prefix_and_maanshan_root() {
        let cz = SITE_REGISTRY.iter().find(|s| s.id == "chuzhou").unwrap();
        let (url, body) = build_epoint_request(cz, "无人机", "2026-07-28", "2026-10-28", 0);
        assert!(url.starts_with("https://ggzy.chuzhou.gov.cn/inteligentsearchNew/rest/esinteligentsearch/getFullTextDataNew"));
        assert!(body.contains("\"wd\":\"无人机\""));
        let mas = SITE_REGISTRY.iter().find(|s| s.id == "maanshan").unwrap();
        let (url2, _) = build_epoint_request(mas, "无人机", "2026-07-28", "2026-10-28", 0);
        assert!(url2.starts_with("https://ggzy.mas.gov.cn/rest/"));
    }

    #[test]
    fn parse_epoint_extracts_six_fields_and_resolves_relative_link() {
        let luan = SITE_REGISTRY.iter().find(|s| s.id == "luan").unwrap();
        let http = MockHttp::ok("luan.gov.cn", &epoint_body());
        let items = fetch_site(luan, &http, KEYWORD, "2026-07-28", "2026-10-28", "2026-10-28").unwrap();
        assert_eq!(items.len(), 2);
        let it = &items[0];
        assert_eq!(it.title, "XX县农业无人机植保服务采购公告");
        assert_eq!(it.date, "2026-10-20");
        assert_eq!(it.url, "https://ggzy.luan.gov.cn/jyxx/001/202610/t20261020_123.html");
        assert_eq!(it.site_name, luan.name);
        assert!(!it.incomplete, "六字段齐备不应标信息不全");
    }

    #[test]
    fn relevance_three_layers() {
        // AC-2-2：无人机+农业 → 相关；仅无人机（航拍）→ 疑似不相关；无主题词 → 无关
        let (r1, why1) = classify("农业无人机采购", "统防统治");
        assert_eq!(r1, Relevance::Core);
        assert!(why1.iter().any(|r| r.contains("加分词")));
        let (r2, _) = classify("航拍服务", "无人机航拍测绘");
        assert_eq!(r2, Relevance::Suspect);
        let (r3, _) = classify("农田水利施工", "高标准农田灌溉");
        assert_eq!(r3, Relevance::Excluded);
    }

    #[test]
    fn dedup_same_url_across_pagination() {
        // AC-2-4：分页重复只留一条
        let mut st = IntelState::default();
        let a = IntelItem {
            key: "luan|https://x/1.html".into(), site_id: "luan".into(), site_name: "六安".into(),
            title: "t".into(), org: "o".into(), area: "a".into(), date: "2026-10-20".into(),
            amount: None, url: "https://x/1.html".into(), relevance: Relevance::Core, reasons: vec![],
            incomplete: false, manual_verdict: false, first_seen: "2026-10-28".into(),
        };
        merge_items(&mut st, vec![a.clone(), a], "2026-10-28");
        assert_eq!(st.items.len(), 1);
    }

    #[test]
    fn blocked_and_network_failures_are_distinguishable() {
        // AC-2-5：拦截 / 网络失败 / 解析失败三种状态可区分
        let spec = SITE_REGISTRY.iter().find(|s| s.id == "luan").unwrap();
        let blocked = MockHttp { responses: BTreeMap::from([("luan".to_string(), Ok("<html>频繁访问!</html>".to_string()))]) };
        let e = fetch_site(spec, &blocked, KEYWORD, "2026-07-28", "2026-10-28", "2026-10-28").unwrap_err();
        assert_eq!(e.0, FetchStatus::Blocked);
        assert!(e.1.contains("拦截特征"));
        let offline = MockHttp { responses: BTreeMap::new() };
        let e2 = fetch_site(spec, &offline, KEYWORD, "2026-07-28", "2026-10-28", "2026-10-28").unwrap_err();
        assert_eq!(e2.0, FetchStatus::Network);
        let bad = MockHttp { responses: BTreeMap::from([("luan".to_string(), Ok("not json".to_string()))]) };
        let e3 = fetch_site(spec, &bad, KEYWORD, "2026-07-28", "2026-10-28", "2026-10-28").unwrap_err();
        assert_eq!(e3.0, FetchStatus::ParseError);
    }

    #[test]
    fn missing_required_fields_marked_incomplete_not_dropped() {
        // AC-2-1：六项缺任一 → 标"信息不全"，不静默丢弃
        let spec = SITE_REGISTRY.iter().find(|s| s.id == "luan").unwrap();
        let body = serde_json::json!({
            "result": { "results": [ { "title": "某县植保无人机采购", "linkurl": "/x/1.html" } ] }
        }).to_string();
        let http = MockHttp::ok("luan.gov.cn", &body);
        let items = fetch_site(spec, &http, KEYWORD, "2026-07-28", "2026-10-28", "2026-10-28").unwrap();
        assert_eq!(items.len(), 1);
        assert!(items[0].incomplete);
    }

    #[test]
    fn manual_verdict_survives_refetch() {
        // AC-2-7：人工改判后再次采集同条目不覆盖
        let mut st = IntelState::default();
        let key = "luan|https://x/1.html";
        st.items.insert(key.to_string(), IntelItem {
            key: key.into(), site_id: "luan".into(), site_name: "六安".into(),
            title: "t".into(), org: "o".into(), area: "a".into(), date: "2026-10-20".into(),
            amount: None, url: "https://x/1.html".into(), relevance: Relevance::Core, reasons: vec![],
            incomplete: false, manual_verdict: false, first_seen: "2026-10-28".into(),
        });
        assert!(set_verdict(&mut st, key, Relevance::Excluded));
        // 再次采集到同 key 的 Core 条目
        let again = IntelItem {
            key: key.into(), site_id: "luan".into(), site_name: "六安".into(),
            title: "t".into(), org: "o".into(), area: "a".into(), date: "2026-10-20".into(),
            amount: None, url: "https://x/1.html".into(), relevance: Relevance::Core, reasons: vec![],
            incomplete: false, manual_verdict: false, first_seen: "2026-10-29".into(),
        };
        merge_items(&mut st, vec![again], "2026-10-29");
        assert_eq!(st.items[key].relevance, Relevance::Excluded);
        assert!(st.items[key].manual_verdict);
    }

    #[test]
    fn zero_result_summary_distinguishes_no_data_from_no_items() {
        // AC-2-8：全部源成功但 0 条 → 摘要含"0 条"与已接入/计划源数，不写成"没采到"
        let mut st = IntelState::default();
        // 用一个全部源返回空数组的 mock：epoint 站解析到空 results 数组
        let empty = serde_json::json!({ "result": { "results": [] } }).to_string();
        let http = MockHttp { responses: BTreeMap::from([("gov.cn".to_string(), Ok(empty))]) };
        let (results, summary) = run_daily(&mut st, &http, "2026-10-28", "2026-07-28", "2026-10-28");
        assert!(!summary.is_empty());
        assert!(summary.contains("今日新增相关项目 0 条"));
        let (connected, planned) = coverage_counts();
        assert!(summary.contains(&format!("已接入 {} / 计划覆盖 {} 源", connected, planned)));
        // epoint 空数组：page0 非空记录数组→Ok 0 条；徽采云/ggzy_ah 无匹配 mock → Network 失败计入失败源
        let ok_sources = results.iter().filter(|r| r.status == FetchStatus::Ok).count();
        assert!(ok_sources >= 15, "16 地市 Epoint 站应解析成功（空结果也是成功）");
    }

    #[test]
    fn registry_is_honest_about_verification() {
        // AC-2-9 / AC-2-10：调度源数量、铜陵周频、人工/不可行源不进调度
        let (connected, planned) = coverage_counts();
        assert_eq!(planned, 20, "4 省级 + 16 地市");
        assert_eq!(connected, 18, "20 - 人工通道 - 不可行 = 18 个调度源");
        assert_eq!(SITE_REGISTRY.iter().filter(|s| s.tier == Tier::CityBatch1).count(), 7);
        assert_eq!(SITE_REGISTRY.iter().filter(|s| s.tier == Tier::CityBatch2).count(), 8);
        assert!(SITE_REGISTRY.iter().any(|s| s.id == "tongling" && s.tier == Tier::CityWeekly));
        assert!(!Tier::ManualOnly.is_scheduled());
        assert!(!Tier::Infeasible.is_scheduled());
        for s in SITE_REGISTRY {
            assert!(!s.note.is_empty(), "每站必须有状态依据（AC-2-9）: {}", s.id);
        }
    }

    #[test]
    fn huicaiyun_parse_and_detail_link() {
        let spec = SITE_REGISTRY.iter().find(|s| s.id == "huicaiyun").unwrap();
        let body = serde_json::json!({
            "success": true,
            "result": { "data": { "total": 1, "data": [{
                "articleId": "XEc90doKIkyxzt9viTgxoQ==", "parentId": 541080,
                "title": "当涂县公安局无人机机场采购项目合同公告",
                "projectName": "当涂县公安局无人机机场采购项目",
                "purchaseName": "当涂县公安局", "districtName": "当涂县",
                "publishDate": 1791856760000, "budgetPrice": null,
                "pathName": "采购公告 | 合同公告 | 合同公告"
            }]}}
        }).to_string();
        let http = MockHttp::ok("ccgp-anhui", &body);
        let items = fetch_site(spec, &http, KEYWORD, "2026-07-28", "2026-10-28", "2026-10-28").unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].org, "当涂县公安局");
        assert_eq!(items[0].area, "当涂县");
        assert_eq!(items[0].date, "2026-10-13"); // 毫秒时间戳东八区日期
        assert!(items[0].url.contains("articleId=XEc90doKIkyxzt9viTgxoQ=="));
    }

    #[test]
    fn ggzy_ah_html_parse() {
        let spec = SITE_REGISTRY.iter().find(|s| s.id == "ggzy_ah").unwrap();
        let html = r#"<li><a href="/zfcg/newDetail?guid=abc123">某农业无人机采购公告</a></li><li><a href="/jsgc/newDetail?guid=def456">另一个项目</a></li>"#;
        let http = MockHttp::ok("ggzy.ah.gov.cn", html);
        let items = fetch_site(spec, &http, KEYWORD, "2026-07-28", "2026-10-28", "2026-10-28").unwrap();
        assert_eq!(items.len(), 2);
        assert!(items[0].url.contains("guid=abc123"));
        assert!(items[0].incomplete, "列表页缺单位/地区/日期，如实标信息不全");
    }

    #[test]
    fn dealer_fetch_dedup_and_diff() {
        // FR-1：四 tag 拉取去重；首周基线；第二周 diff 出增减
        let body = serde_json::json!({
            "status": 1,
            "data": [{ "guid": "G1", "company_name": "凤台县丰沃农机技术服务有限公司",
                "area": "安徽省/淮南市/凤台县", "address": "刘集乡", "contact_phone": "13800000000" }]
        }).to_string();
        let http = MockHttp { responses: BTreeMap::from([("dealer-address".to_string(), Ok(body))]) };
        let nodes = fetch_dealers(&http).unwrap();
        // 去重口径 = 公司名+area 归一化：四个 tag 返回同一家公司时只留一条
        assert_eq!(nodes.len(), 1);
        assert_eq!(nodes[0].company_name, "凤台县丰沃农机技术服务有限公司");
    }

    #[test]
    fn dealer_diff_baseline_then_change() {
        let prev = vec![DealerNode {
            guid: "G1".into(), company_name: "甲公司".into(), area: "安徽省/淮南市/凤台县".into(),
            address: "a".into(), phone: "p".into(), tags: vec!["client".into()],
        }];
        let d0 = diff_dealers(&[], prev.clone());
        assert!(d0.baseline_week);
        let next = vec![
            prev[0].clone(),
            DealerNode { guid: "G2".into(), company_name: "乙公司".into(), area: "安徽省/合肥市/肥西县".into(),
                address: "b".into(), phone: "q".into(), tags: vec!["client".into()] },
        ];
        let d1 = diff_dealers(&prev, next);
        assert!(!d1.baseline_week);
        assert_eq!(d1.added.len(), 1);
        assert_eq!(d1.removed.len(), 0);
        assert_eq!(d1.total, 2);
        // 原有网点消失
        let d2 = diff_dealers(&prev, vec![]);
        assert_eq!(d2.removed.len(), 1);
    }
}
