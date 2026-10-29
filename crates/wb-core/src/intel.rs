//! FR-2 项目情报（应用内直连）+ FR-1 极飞网点周比对（v2.2）。
//!
//! 设计原则（PRD 第十章 10.2 / AC-2-5、AC-2-11）：
//! - 按"会失败"设计：每个站点独立抓取，失败只标记该源，不阻塞其他源与本地功能。
//! - 状态如实：未验证的分站不得标"已接入"；失败原因可区分（拦截/验证页/超时/解析失败）。
//! - 同站间隔 ≥2s 由调度方保证，本模块不 sleep（便于测试）。
//! - 网络走 [`Http`] trait，生产实现为 ureq，测试用 mock。

use serde::{Deserialize, Serialize};
use std::cell::{Cell, RefCell};
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
/// v2.2.4 抓取改造：单请求超时 20s → 40s。
/// 政府站普遍慢，20s 在弱网或长响应下会被截断，表现为「网络失败」而实际是读超时。
pub const FETCH_TIMEOUT_SECS: u64 = 40;
/// 网络类失败的重试间隔（秒）：5s、15s 递增，最多重试 2 次。
pub const RETRY_DELAYS_SECS: &[u64] = &[5, 15];
/// 诊断里保留的响应体前 N 字符（自带上报用，不落全量）。
pub const DIAG_BODY_CHARS: usize = 300;
/// 每源滚动保留的诊断条数。
pub const DIAG_KEEP: usize = 3;

pub const UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
     (KHTML, like Gecko) Chrome/126.0.0.0 Safari/537.36";

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum HttpErr {
    /// 网络不可达 / DNS / 连接被 reset（政府站 WAF 常见「连不上」多为此类）
    Network(String),
    /// 超时（连接超时 / 读超时 / 整体超时）
    Timeout(String),
    /// TLS 握手失败（证书、协议版本、指纹被拒）
    Tls(String),
    /// HTTP 非成功状态（状态码 + 响应体前 300 字符）
    Status(u16, String),
    /// 命中反爬/验证页特征（418 拦截页 /「频繁访问」/NEED_CAPTCHA）
    Blocked(&'static str),
}

impl HttpErr {
    /// 是否值得重试。Blocked 明确不重试——验证码/拦截越重试封得越久。
    pub fn is_retryable(&self) -> bool {
        matches!(self, HttpErr::Network(_) | HttpErr::Timeout(_) | HttpErr::Tls(_))
    }

    /// 诊断分类标签（界面直接展示，用户可整段贴回）
    pub fn kind_label(&self) -> &'static str {
        match self {
            HttpErr::Network(_) => "网络",
            HttpErr::Timeout(_) => "超时",
            HttpErr::Tls(_) => "TLS",
            HttpErr::Status(..) => "状态码",
            HttpErr::Blocked(_) => "验证码/拦截",
        }
    }

    pub fn http_status(&self) -> Option<u16> {
        match self {
            HttpErr::Status(c, _) => Some(*c),
            _ => None,
        }
    }

    pub fn body_head(&self) -> &str {
        match self {
            HttpErr::Status(_, b) => b.as_str(),
            _ => "",
        }
    }

    pub fn message(&self) -> String {
        match self {
            HttpErr::Network(m) | HttpErr::Timeout(m) | HttpErr::Tls(m) => m.clone(),
            HttpErr::Status(c, _) => format!("HTTP {}", c),
            HttpErr::Blocked(f) => format!("命中拦截特征 {}", f),
        }
    }
}

pub trait Http {
    fn post_json(&self, url: &str, body: &str, timeout: Duration) -> Result<String, HttpErr>;
    fn get(&self, url: &str, timeout: Duration) -> Result<String, HttpErr>;

    /// 表单 POST。与 post_json 的区别只在 Content-Type——
    /// 省级主站 ggzy.ah.gov.cn 的 /souSuo 收的是 `param=…&currentPage=1` 表单，
    /// 此前复用 post_json 会把 Content-Type 写成 application/json，
    /// 服务端按 JSON 解析失败、返回错误页而非记录列表（表现为「解析失败」）。
    fn post_form(&self, url: &str, body: &str, timeout: Duration) -> Result<String, HttpErr> {
        self.post_json(url, body, timeout)
    }
}

/// 生产实现：ureq（v2.1 曾随单机化移除，v2.2 经用户拍板恢复）。
///
/// v2.2.4 改造要点（都只为「提高成功率」这一件事）：
/// - **native-tls**：ureq 的 native-tls 是"适配器"，`ureq::get/post` 这类便捷函数永远不会
///   采用它，必须在 Agent 上 `tls_connector` 显式配置——只加 feature 不改代码等于没改。
///   Windows 走 SChannel，与浏览器同一套栈；政府站 WAF 对 rustls 的 ClientHello 指纹
///   常见做法是直接 reset，这正是「浏览器/curl 通、程序全挂」的主因。
/// - **共享 Agent**：连接池复用 + 共享 cookie store（省级站要先 GET 首页拿会话再 POST）。
/// - **请求头补全**：Accept / Accept-Language / Referer / Origin。Epoint 的 POST 常校验 Referer。
pub struct UreqFetcher;

impl UreqFetcher {
    /// 实际生效的 TLS 后端（界面上可核对，避免「以为切了其实没切」）。
    pub fn tls_backend(&self) -> &'static str {
        let _ = shared_agent(); // 触发一次初始化，保证读到真实结果
        tls_backend_label()
    }
}

fn origin_of(url: &str) -> String {
    match url.find("://") {
        Some(p) => {
            let rest = &url[p + 3..];
            let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
            format!("{}{}", &url[..p + 3], &rest[..end])
        }
        None => url.to_string(),
    }
}

/// 统一补齐浏览器常规请求头。缺 Referer/Origin 时 Epoint 系接口常返回空或错误页。
fn apply_common(req: ureq::Request, url: &str) -> ureq::Request {
    let origin = origin_of(url);
    req.set("User-Agent", UA)
        .set("Accept", "text/html,application/json,application/xhtml+xml,*/*;q=0.8")
        .set("Accept-Language", "zh-CN,zh;q=0.9")
        .set("Referer", &format!("{}/", origin))
        .set("Origin", &origin)
}

fn shared_agent() -> &'static ureq::Agent {
    static AGENT: std::sync::OnceLock<ureq::Agent> = std::sync::OnceLock::new();
    AGENT.get_or_init(build_agent)
}

fn tls_backend_label() -> &'static str {
    if *NATIVE_TLS_OK.get().unwrap_or(&false) {
        if cfg!(target_os = "windows") {
            "native-tls/schannel"
        } else if cfg!(target_os = "macos") {
            "native-tls/security-framework"
        } else {
            "native-tls/openssl"
        }
    } else {
        "rustls（native-tls 初始化失败，已回落）"
    }
}

static NATIVE_TLS_OK: std::sync::OnceLock<bool> = std::sync::OnceLock::new();

fn build_agent() -> ureq::Agent {
    let base = ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(FETCH_TIMEOUT_SECS))
        .redirects(3)
        .user_agent(UA);
    // 不设 min_protocol_version：部分政府站只到 TLS1.0/1.1，收紧协议反而会砍掉可用站点。
    // 系统栈的默认协议集合与浏览器一致，这正是要的效果。
    match ureq::native_tls::TlsConnector::new() {
        Ok(connector) => {
            NATIVE_TLS_OK.set(true).ok();
            base.tls_connector(std::sync::Arc::new(connector)).build()
        }
        Err(_) => {
            NATIVE_TLS_OK.set(false).ok();
            base.build()
        }
    }
}

impl Http for UreqFetcher {
    fn post_json(&self, url: &str, body: &str, timeout: Duration) -> Result<String, HttpErr> {
        let resp = apply_common(shared_agent().post(url), url)
            .timeout(timeout)
            .set("Content-Type", "application/json; charset=UTF-8")
            .send_string(body)
            .map_err(map_ureq_err)?;
        read_resp(resp)
    }

    fn post_form(&self, url: &str, body: &str, timeout: Duration) -> Result<String, HttpErr> {
        let resp = apply_common(shared_agent().post(url), url)
            .timeout(timeout)
            .set("Content-Type", "application/x-www-form-urlencoded; charset=UTF-8")
            .send_string(body)
            .map_err(map_ureq_err)?;
        read_resp(resp)
    }

    fn get(&self, url: &str, timeout: Duration) -> Result<String, HttpErr> {
        let resp = apply_common(shared_agent().get(url), url)
            .timeout(timeout)
            .call()
            .map_err(map_ureq_err)?;
        read_resp(resp)
    }
}

/// 响应体前 N 字符（按字符切，避免把 UTF-8 断在半个汉字上）。
fn head_chars(body: &str, n: usize) -> String {
    let s: String = body.chars().take(n).collect();
    if body.chars().count() > n {
        format!("{}…（截断）", s)
    } else {
        s
    }
}

fn map_ureq_err(e: ureq::Error) -> HttpErr {
    match e {
        ureq::Error::Status(code, resp) => {
            let body = resp.into_string().unwrap_or_default();
            if code == 418 || code == 429 {
                HttpErr::Blocked("http_418_429")
            } else {
                HttpErr::Status(code, head_chars(&body, DIAG_BODY_CHARS))
            }
        }
        ureq::Error::Transport(t) => {
            let msg = t.to_string();
            let lower = msg.to_lowercase();
            // 超时在 ureq 里落在 Io 里，只能按消息文本区分——所以保留原文便于远程核对
            if lower.contains("timed out") || lower.contains("timeout") || lower.contains("超时") {
                HttpErr::Timeout(msg)
            } else if lower.contains("tls")
                || lower.contains("certificate")
                || lower.contains("handshake")
                || lower.contains("ssl")
            {
                HttpErr::Tls(msg)
            } else {
                HttpErr::Network(msg)
            }
        }
    }
}

fn read_resp(resp: ureq::Response) -> Result<String, HttpErr> {
    let status = resp.status();
    let body = resp.into_string().map_err(|e| HttpErr::Network(e.to_string()))?;
    // 验证页特征检测：返回内容是拦截页时即使 HTTP 200 也算被拦（AC-2-5）
    if body.contains("频繁访问") || body.contains("NEED_CAPTCHA") || body.contains("访问被拦截") {
        return Err(HttpErr::Blocked("captcha_page"));
    }
    // 5xx 但 ureq 未当错误抛出的边缘情况，统一按状态码归类
    if status >= 500 {
        return Err(HttpErr::Status(status, head_chars(&body, DIAG_BODY_CHARS)));
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

/// 清洗入库文本：去 HTML 标签 + 解码常见实体 + 折叠空白。
///
/// 为什么必须在**入库前**做：部分源站（Epoint 系）把搜索高亮原样塞进 title 字段，
/// 形如 `<em>无</em><em>人</em><em>机</em>`——逐字高亮，标签夹在字与字之间。
/// 前端 `esc()` 只转义尖括号、不会剥离标签，于是界面字面显示 `<em>无</em>…`，
/// 用户看到的是调试残渣而不是项目名。清洗放在领域层（而不是前端渲染时），
/// 保证 JSON 库存本身干净、导出/搜索/去重都基于同一份文本。
pub fn clean_text(s: &str) -> String {
    // ① 去标签（含未闭合的残片），标签内内容保留
    let mut buf = String::with_capacity(s.len());
    let mut in_tag = false;
    for ch in s.chars() {
        match ch {
            '<' => in_tag = true,
            '>' => in_tag = false,
            c if !in_tag => buf.push(c),
            _ => {}
        }
    }
    // ② 实体解码（先做 &amp; 之外的具名实体，再统一处理数字实体）
    let mut out = buf
        .replace("&nbsp;", " ")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&#39;", "'")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&");
    // ③ 数字实体（&#xxx; / &#xhh;），只处理可见字符，越界值原样保留
    let chars: Vec<char> = out.chars().collect();
    let mut decoded = String::with_capacity(chars.len());
    let mut i = 0usize;
    while i < chars.len() {
        if chars[i] == '&' && i + 1 < chars.len() && chars[i + 1] == '#' {
            let rest: String = chars[i..].iter().take(12).collect();
            if let Some(semi) = rest.find(';') {
                let body = &rest[2..semi];
                let v = if let Some(hex) = body.strip_prefix('x').or_else(|| body.strip_prefix('X')) {
                    u32::from_str_radix(hex, 16).ok()
                } else {
                    body.parse::<u32>().ok()
                };
                if let Some(cp) = v.and_then(char::from_u32) {
                    decoded.push(cp);
                    i += semi + 1;
                    continue;
                }
            }
        }
        decoded.push(chars[i]);
        i += 1;
    }
    out = decoded;
    // ④ 折叠空白（换行/制表/连续空格 → 单空格）
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// 解析单条记录为 IntelItem（六字段抽取，缺失标 incomplete）
fn record_to_item(spec: &SiteSpec, rec: &serde_json::Value, today: &str) -> IntelItem {
    let title = clean_text(
        rec.get("title").or_else(|| rec.get("projectName")).and_then(|t| t.as_str()).unwrap_or("").trim(),
    );
    let org = clean_text(
        rec.get("purchaseName")
            .or_else(|| rec.get("purchaseunitname"))
            .or_else(|| rec.get("jiaoyimethod"))
            .and_then(|t| t.as_str())
            .unwrap_or("")
            .trim(),
    );
    let area = clean_text(
        rec.get("districtName")
            .or_else(|| rec.get("area"))
            .or_else(|| rec.get("xiaqucode"))
            .and_then(|t| t.as_str())
            .unwrap_or("")
            .trim(),
    );
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
                let secs = ms / 1000;
                let days = (secs + 8 * 3600).div_euclid(86400);
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

// ─────────────────────────────────────────────────────────────────────────
// v2.2.4 抓取改造：失败诊断 + 可注入重试
// ─────────────────────────────────────────────────────────────────────────
/// 单站采集失败的结构化载荷。
/// 相比原来的 `(FetchStatus, String)` 元组，多带三项诊断必需信息：
/// **实际请求的 URL**、**HTTP 状态码**、**响应体前 300 字符**。
/// 用户真实机器是唯一的验证环境（沙箱出口 502、CI 在海外都到不了这些政务站），
/// 所以诊断必须自报，让用户能把整段贴回来定位。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FetchFail {
    pub status: FetchStatus,
    /// 人读摘要（列表页直接展示）
    pub detail: String,
    /// 失败时正在请求的 URL（含分页序号，能判断卡在第几页）
    pub url: String,
    pub http_status: Option<u16>,
    pub body_head: String,
    /// 实际尝试次数（1 = 没重试；3 = 重试两次仍失败）
    pub attempts: u32,
}

impl FetchFail {
    /// 本地产生的失败（解析不到记录、适配器未实现等），没有 HTTP 层信息。
    pub fn local(status: FetchStatus, detail: impl Into<String>) -> Self {
        FetchFail {
            status,
            detail: detail.into(),
            url: String::new(),
            http_status: None,
            body_head: String::new(),
            attempts: 1,
        }
    }

    /// 从 HTTP 层错误转出来：状态码/响应体原文都留着。
    pub fn from_http(e: &HttpErr, url: &str, attempts: u32) -> Self {
        let status = match e {
            HttpErr::Network(_) | HttpErr::Timeout(_) | HttpErr::Tls(_) => FetchStatus::Network,
            HttpErr::Status(..) | HttpErr::Blocked(_) => FetchStatus::Blocked,
        };
        FetchFail {
            status,
            detail: e.message(),
            url: url.to_string(),
            http_status: e.http_status(),
            body_head: e.body_head().to_string(),
            attempts,
        }
    }
}

/// 重试等待的注入点。领域层不直接 sleep：生产用 [`RealSleeper`]，
/// 测试用 [`NoSleep`]，否则单测会为每次模拟网络失败白等 20 秒。
pub trait RetrySleeper {
    fn sleep_secs(&self, secs: u64);
}

/// 生产实现：真等。重试间隔按需求固定为 5s / 15s 递增。
pub struct RealSleeper;
impl RetrySleeper for RealSleeper {
    fn sleep_secs(&self, secs: u64) {
        std::thread::sleep(Duration::from_secs(secs));
    }
}

/// 测试实现：不等待。
pub struct NoSleep;
impl RetrySleeper for NoSleep {
    fn sleep_secs(&self, _secs: u64) {}
}

/// 包一层重试：网络类失败最多再试 [`RETRY_DELAYS_SECS`] 次（5s、15s），
/// **Blocked（验证码/拦截）一律不重试**——越重试封得越久，直接降级。
///
/// 做成 `Http` 的装饰器，是为了让上面 15 个地市站的适配器代码一行都不用改，
/// 重试语义集中在一处，也就不会出现「这个站重试了那个站没重试」。
pub struct RetryingHttp<'a> {
    inner: &'a dyn Http,
    sleeper: &'a dyn RetrySleeper,
    /// 最近一次请求的 URL，失败时用来落诊断
    last_url: RefCell<String>,
    /// 最近一次请求的尝试次数
    attempts: Cell<u32>,
}

impl<'a> RetryingHttp<'a> {
    pub fn new(inner: &'a dyn Http, sleeper: &'a dyn RetrySleeper) -> Self {
        RetryingHttp { inner, sleeper, last_url: RefCell::new(String::new()), attempts: Cell::new(0) }
    }

    pub fn attempts(&self) -> u32 {
        self.attempts.get().max(1)
    }

    pub fn last_url(&self) -> String {
        self.last_url.borrow().clone()
    }

    fn run(&self, url: &str, mut call: impl FnMut() -> Result<String, HttpErr>) -> Result<String, HttpErr> {
        *self.last_url.borrow_mut() = url.to_string();
        let mut tried = 0usize;
        loop {
            match call() {
                Ok(v) => {
                    self.attempts.set(tried as u32 + 1);
                    return Ok(v);
                }
                Err(e) => {
                    if e.is_retryable() && tried < RETRY_DELAYS_SECS.len() {
                        let d = RETRY_DELAYS_SECS[tried];
                        tried += 1;
                        self.sleeper.sleep_secs(d);
                        continue;
                    }
                    self.attempts.set(tried as u32 + 1);
                    return Err(e);
                }
            }
        }
    }
}

impl<'a> Http for RetryingHttp<'a> {
    fn post_json(&self, url: &str, body: &str, timeout: Duration) -> Result<String, HttpErr> {
        self.run(url, || self.inner.post_json(url, body, timeout))
    }
    fn post_form(&self, url: &str, body: &str, timeout: Duration) -> Result<String, HttpErr> {
        self.run(url, || self.inner.post_form(url, body, timeout))
    }
    fn get(&self, url: &str, timeout: Duration) -> Result<String, HttpErr> {
        self.run(url, || self.inner.get(url, timeout))
    }
}

pub fn fetch_site(
    spec: &SiteSpec,
    http: &dyn Http,
    wd: &str,
    start: &str,
    end: &str,
    today: &str,
) -> Result<Vec<IntelItem>, FetchFail> {
    match spec.tier {
        Tier::CityBatch1 | Tier::CityBatch2 | Tier::CityWeekly => fetch_epoint(spec, http, wd, start, end, today),
        Tier::ProvincialFallback => fetch_huicaiyun(spec, http, wd, start, end, today),
        Tier::ProvincialMain => fetch_ggzy_ah(spec, http, wd, start, end, today),
        _ => Err(FetchFail::local(FetchStatus::ParseError, "该源不在自动调度范围")),
    }
}

/// 带重试的单站抓取（生产路径入口）。诊断所需的 URL / 状态码 / 响应体
/// 全从 [`RetryingHttp`] 与 [`FetchFail::from_http`] 带出，不用各适配器自己拼。
pub fn fetch_site_with_retry(
    spec: &SiteSpec,
    http: &dyn Http,
    sleeper: &dyn RetrySleeper,
    wd: &str,
    start: &str,
    end: &str,
    today: &str,
) -> Result<Vec<IntelItem>, FetchFail> {
    let r = RetryingHttp::new(http, sleeper);
    let out = fetch_site(spec, &r, wd, start, end, today);
    match out {
        Ok(v) => Ok(v),
        Err(mut f) => {
            // 适配器已带出 HTTP 层信息就不覆盖（那是更精确的一手信息）
            if f.url.is_empty() {
                f.url = r.last_url();
            }
            f.attempts = r.attempts();
            Err(f)
        }
    }
}

fn fetch_epoint(
    spec: &SiteSpec,
    http: &dyn Http,
    wd: &str,
    start: &str,
    end: &str,
    today: &str,
) -> Result<Vec<IntelItem>, FetchFail> {
    // 芜湖（.NET API）/ 池州（自有 JSON API）为非 Epoint 站，接口参数未逐站验证，
    // 按「未验证不冒充已实现」如实返回，待第二批逐站接入（AC-2-9）
    spec.epoint
        .as_ref()
        .ok_or_else(|| FetchFail::local(FetchStatus::ParseError, "非 Epoint 站，适配器待第二批逐站验证实现"))?;
    let mut items = Vec::new();
    for page in 0..MAX_PAGES_PER_SITE as usize {
        let (url, body) = build_epoint_request(spec, wd, start, end, page);
        let resp = http
            .post_json(&url, &body, Duration::from_secs(FETCH_TIMEOUT_SECS))
            .map_err(|e| FetchFail::from_http(&e, &url, 1))?;
        ensure_not_blocked(&resp, &url)?;
        let v: serde_json::Value = serde_json::from_str(&resp).map_err(|_| {
            FetchFail { body_head: head_chars(&resp, DIAG_BODY_CHARS), ..FetchFail::local(FetchStatus::ParseError, "响应非 JSON") }
                .with_url(&url)
        })?;
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
        // 标准路径 /result/results 优先（空数组=合法空结果，AC-2-8）；
        // 站间差异（双层包装/异构键名）回落到递归发现
        let records = match v.pointer("/result/results") {
            Some(serde_json::Value::Array(a)) => a.clone(),
            _ => find_record_arrays(&v, 0),
        };
        if records.is_empty() {
            if page == 0 && v.pointer("/result/results").is_none() && find_record_arrays(&v, 0).is_empty() {
                return Err(FetchFail {
                    body_head: head_chars(&resp, DIAG_BODY_CHARS),
                    ..FetchFail::local(FetchStatus::ParseError, "未在响应中定位到记录数组").with_url(&url)
                });
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

impl FetchFail {
    fn with_url(mut self, url: &str) -> Self {
        self.url = url.to_string();
        self
    }
}

fn fetch_huicaiyun(
    spec: &SiteSpec,
    http: &dyn Http,
    wd: &str,
    start: &str,
    end: &str,
    today: &str,
) -> Result<Vec<IntelItem>, FetchFail> {
    let (s_ms, e_ms) = match (date_str_to_ms(start), date_str_to_ms(end)) {
        (Some(s), Some(e)) => (s, e + 86_399_000),
        _ => return Err(FetchFail::local(FetchStatus::ParseError, "日期参数非法")),
    };
    let url = format!("{}/portal/all", spec.base);
    let body = serde_json::json!({
        "keyword": wd, "firstCode": "", "secondCode": "", "districtCode": [],
        "publishDateBegin": s_ms, "publishDateEnd": e_ms,
        "pageNo": 1, "pageSize": 15, "isTitleSearch": 1, "order": "desc", "leaf": "0",
    });
    let resp = http
        .post_json(&url, &body.to_string(), Duration::from_secs(FETCH_TIMEOUT_SECS))
        .map_err(|e| FetchFail::from_http(&e, &url, 1))?;
    ensure_not_blocked(&resp, &url)?;
    let v: serde_json::Value = serde_json::from_str(&resp).map_err(|_| FetchFail {
        body_head: head_chars(&resp, DIAG_BODY_CHARS),
        ..FetchFail::local(FetchStatus::ParseError, "响应非 JSON").with_url(&url)
    })?;
    let records = v
        .pointer("/result/data/data")
        .and_then(|x| x.as_array())
        .cloned()
        .unwrap_or_default();
    if records.is_empty() && v.pointer("/result/data/total").and_then(|t| t.as_i64()) != Some(0) {
        return Err(FetchFail {
            body_head: head_chars(&resp, DIAG_BODY_CHARS),
            ..FetchFail::local(FetchStatus::ParseError, "未定位到徽采云结果数组").with_url(&url)
        });
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
    Some((days * 86_400 - 8 * 3600) * 1000)
}

fn fetch_ggzy_ah(
    spec: &SiteSpec,
    http: &dyn Http,
    wd: &str,
    _start: &str,
    _end: &str,
    today: &str,
) -> Result<Vec<IntelItem>, FetchFail> {
    // 省级主站要求「先有会话再检索」：直接 POST /souSuo 会被当成无会话裸请求拦掉。
    // 改造⑥：先 GET 首页把会话 cookie 拿到共享 cookie store（UreqFetcher 的 Agent 带 cookies
    // feature），再 POST /souSuo 表单。首页预热失败不算致命——继续试检索，让服务端自己说话，
    // 失败的真相会落进诊断里，比在这里猜更可靠。
    let home = format!("{}/", spec.base);
    let prime = http.get(&home, Duration::from_secs(FETCH_TIMEOUT_SECS));
    let url = format!("{}/souSuo", spec.base);
    let form = format!("param={}&currentPage=1&pageNo=1", urlencoding_min(wd));
    let resp = match http.post_form(&url, &form, Duration::from_secs(FETCH_TIMEOUT_SECS)) {
        Ok(r) => r,
        Err(e) => {
            // 检索本身失败：优先报检索（那才是拿不到数据的原因），首页失败只作补充说明
            let mut f = FetchFail::from_http(&e, &url, 1);
            if prime.is_err() {
                f.detail = format!("{}（首页会话预热也失败，可能是整站不可达）", f.detail);
            }
            return Err(f);
        }
    };
    ensure_not_blocked(&resp, &url)?;
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
        return Err(FetchFail {
            body_head: head_chars(&resp, DIAG_BODY_CHARS),
            ..FetchFail::local(FetchStatus::ParseError, "HTML 未解析到 newDetail 链接").with_url(&url)
        });
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

/// 拦截页特征检测（AC-2-5）：HTTP 200 但内容是验证/拦截页也算 Blocked。
/// 放在 fetch 层而非仅生产 HTTP 实现里，保证 mock 测试与生产行为一致。
fn ensure_not_blocked(body: &str, url: &str) -> Result<(), FetchFail> {
    if body.contains("频繁访问") || body.contains("NEED_CAPTCHA") || body.contains("访问被拦截") {
        Err(FetchFail {
            body_head: head_chars(body, DIAG_BODY_CHARS),
            ..FetchFail::local(FetchStatus::Blocked, "命中拦截特征 captcha_page").with_url(url)
        })
    } else {
        Ok(())
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
    /// 最近一次周比对结果（界面展示用）
    #[serde(default)]
    pub dealer_last_diff: Option<DealerDiff>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SiteStateRow {
    pub last_attempt: Option<String>,
    pub last_success: Option<String>,
    pub last_status: Option<FetchStatus>,
    pub last_detail: String,
    pub fail_streak: u32,
    /// v2.2.4 诊断日志：每源滚动保留最近 [`DIAG_KEEP`] 次采集记录，不无限增长。
    /// 用户机器是唯一能连通政务站的环境，这份记录是唯一的事后证据。
    #[serde(default)]
    pub diags: Vec<DiagEntry>,
}

/// 单次采集的诊断记录（一次一条，成功也留——成功记录能证明「刚才还通」，
/// 一旦转为失败，与上一条成功记录对比就能分辨是站点变了还是本地网络变了）。
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct DiagEntry {
    /// 采集时刻（本地时区 `YYYY-MM-DDTHH:MM:SSZ` 口径随 store::now_iso_pub）
    pub ts: String,
    /// 本次实际请求的 URL（空表示本地失败，未发出请求）
    pub url: String,
    pub http_status: Option<u16>,
    /// 分类：成功 / 网络 / 超时 / TLS / 状态码 / 验证码/拦截 / 解析
    pub kind: String,
    /// 一句话摘要
    pub detail: String,
    /// 响应体前 300 字符（解析失败时是判断源站结构变更的唯一线索）
    pub body_head: String,
    /// 尝试次数（>1 说明重试过）
    pub attempts: u32,
}

impl DiagEntry {
    /// 给人看的整段诊断文本——用户点「复制诊断」直接贴回来就是这个。
    pub fn to_block(&self) -> String {
        let mut out = String::new();
        out.push_str(&format!("时间：{}\n", if self.ts.is_empty() { "（未取到）" } else { &self.ts }));
        out.push_str(&format!("请求：{}\n", if self.url.is_empty() { "（未发出请求）" } else { &self.url }));
        out.push_str(&format!(
            "HTTP 状态：{}\n",
            self.http_status.map(|c| c.to_string()).unwrap_or_else(|| "（无）".into())
        ));
        out.push_str(&format!("分类：{}\n", self.kind));
        out.push_str(&format!("尝试次数：{}\n", self.attempts));
        out.push_str(&format!("说明：{}\n", self.detail));
        if !self.body_head.is_empty() {
            out.push_str(&format!("响应体前 300 字符：\n{}\n", self.body_head));
        }
        out
    }
}

impl SiteStateRow {
    /// 追加一条诊断，只留最近 [`DIAG_KEEP`] 条。
    pub fn push_diag(&mut self, entry: DiagEntry) {
        self.diags.push(entry);
        let n = self.diags.len();
        if n > DIAG_KEEP {
            self.diags.drain(0..n - DIAG_KEEP);
        }
    }
}

impl IntelState {
    pub fn load(dir: &std::path::Path) -> IntelState {
        let p = dir.join("intel_state.json");
        let mut st: IntelState = std::fs::read_to_string(&p)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default();
        // 存量清洗迁移：v2.2.1 之前入库的条目可能带着源站高亮标签
        // （形如 `<em>无</em><em>人</em><em>机</em>`），加载时就地洗一遍并回写。
        if clean_stored(&mut st) {
            let _ = st.save(dir);
        }
        st
    }

    pub fn save(&self, dir: &std::path::Path) {
        let p = dir.join("intel_state.json");
        if let Ok(s) = serde_json::to_string_pretty(self) {
            let _ = std::fs::write(p, s);
        }
    }
}

/// 存量条目清洗：把已入库条目的 title/org/area 过一遍 `clean_text`。
/// 返回是否发生了改写（false = 存量已干净，不触发回写）。
pub fn clean_stored(state: &mut IntelState) -> bool {
    let mut changed = false;
    for it in state.items.values_mut() {
        let t = clean_text(&it.title);
        let o = clean_text(&it.org);
        let a = clean_text(&it.area);
        if t != it.title {
            it.title = t;
            changed = true;
        }
        if o != it.org {
            it.org = o;
            changed = true;
        }
        if a != it.area {
            it.area = a;
            changed = true;
        }
    }
    changed
}

/// 去重合并一批新条目（AC-2-4：站点+URL 规范化幂等；AC-2-7：人工改判保留）
pub fn merge_items(state: &mut IntelState, incoming: Vec<IntelItem>, today: &str) -> (usize, usize) {
    let mut new_count = 0usize;
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
    // 存储封顶 500 条（最旧的先淘汰），防长期内存/磁盘增长
    if state.items.len() > 500 {
        let keys: Vec<String> = state.items.keys().cloned().collect();
        let excess = state.items.len() - 500;
        for k in keys.into_iter().take(excess) {
            state.items.remove(&k);
        }
        let _ = today;
        return (new_count, state.items.len());
    }
    let _ = today;
    (new_count, state.items.len())
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
    run_daily_at(state, http, &RealSleeper, today, start, end, &crate::store::now_iso_pub())
}

/// 与 [`run_daily`] 同语义，但可注入等待实现与采集时刻——
/// 单测要断言「重试了 2 次」「诊断滚到第 4 条时最早那条被挤掉」，
/// 既不能真等 20 秒，也不能依赖真实时钟。
pub fn run_daily_at(
    state: &mut IntelState,
    http: &dyn Http,
    sleeper: &dyn RetrySleeper,
    today: &str,
    start: &str,
    end: &str,
    ts: &str,
) -> (Vec<SiteResult>, String) {
    let mut results = Vec::new();
    let mut new_total = 0usize;
    let mut failed = 0usize;
    for spec in SITE_REGISTRY.iter().filter(|s| s.tier.is_scheduled()) {
        let r = fetch_site_with_retry(spec, http, sleeper, KEYWORD, start, end, today);
        let mut fail: Option<FetchFail> = None;
        let sr = match r {
            Ok(items) => {
                let (new_n, _) = merge_items(state, items, today);
                let n = state.items.values().filter(|i| i.site_id == spec.id).count();
                new_total += new_n;
                SiteResult { site_id: spec.id.to_string(), status: FetchStatus::Ok, detail: String::new(), items: n, new_items: new_n }
            }
            Err(f) => {
                failed += 1;
                let sr = SiteResult {
                    site_id: spec.id.to_string(),
                    status: f.status.clone(),
                    detail: f.detail.clone(),
                    items: 0,
                    new_items: 0,
                };
                fail = Some(f);
                sr
            }
        };
        // 诊断记录先组装好，再与状态行一起落库。关键：**诊断写入失败绝不能中断采集**——
        // 这里全是不可能失败的纯内存操作，落盘那一步在 IntelState::save 里已被 `if let Ok` 包住；
        // 若某天真失败，也只会在该源 diags 里少一条，不会把整轮采集带崩。
        let diag = match fail.as_ref() {
            None => DiagEntry {
                ts: ts.to_string(),
                url: String::new(),
                http_status: Some(200),
                kind: "成功".into(),
                detail: if sr.items == 0 {
                    "请求成功，结果 0 条".into()
                } else {
                    format!("请求成功，本站在库 {} 条", sr.items)
                },
                body_head: String::new(),
                attempts: 1,
            },
            Some(f) => DiagEntry {
                ts: ts.to_string(),
                url: f.url.clone(),
                http_status: f.http_status,
                kind: sr.status.label().to_string(),
                detail: f.detail.clone(),
                body_head: f.body_head.clone(),
                attempts: f.attempts,
            },
        };
        let row = state.sites.entry(spec.id.to_string()).or_default();
        row.last_attempt = Some(today.to_string());
        match sr.status {
            FetchStatus::Ok => {
                row.last_success = Some(today.to_string());
                row.last_status = Some(FetchStatus::Ok);
                row.last_detail = String::new();
                row.fail_streak = 0;
            }
            ref st => {
                row.last_status = Some(st.clone());
                row.last_detail = sr.detail.clone();
                row.fail_streak += 1;
            }
        }
        row.push_diag(diag);
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

pub fn fetch_dealers(http: &dyn Http) -> Result<Vec<DealerNode>, FetchFail> {
    let mut nodes = Vec::new();
    let mut seen = std::collections::BTreeSet::new();
    for tag in DEALER_TAGS {
        let url = format!(
            "https://www.xa.com/service/dealer-address?province={}&tag={}",
            urlencoding_min(DEALER_PROVINCE), tag
        );
        let resp = http.get(&url, Duration::from_secs(FETCH_TIMEOUT_SECS)).map_err(|e| FetchFail::from_http(&e, &url, 1))?;
        let v: serde_json::Value = serde_json::from_str(&resp).map_err(|_| FetchFail {
            body_head: head_chars(&resp, DIAG_BODY_CHARS),
            ..FetchFail::local(FetchStatus::ParseError, "响应非 JSON").with_url(&url)
        })?;
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
pub fn diff_dealers(prev: &[DealerNode], next: &[DealerNode]) -> DealerDiff {
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

    #[test]
    fn clean_text_strips_highlight_tags_and_decodes_entities() {
        // 源站搜索高亮：标签夹在字与字之间，去标签后应还原成项目名
        assert_eq!(clean_text("<em>无</em><em>人</em><em>机</em>"), "无人机");
        assert_eq!(clean_text("A &amp; B &lt;x&gt;"), "A & B <x>");
        assert_eq!(clean_text("&#22909;&#x4EBA;"), "好人");
        assert_eq!(clean_text("  多  空格\n换行 "), "多 空格 换行");
        // 纯文本不应被改动（幂等）
        assert_eq!(clean_text("普通项目名"), "普通项目名");
        assert_eq!(clean_text("普通项目名"), clean_text("普通项目名"));
    }

    #[test]
    fn clean_stored_rewrites_persisted_highlight_titles_once() {
        let mut st = IntelState::default();
        st.items.insert(
            "k".into(),
            IntelItem {
                key: "k".into(),
                site_id: "s".into(),
                site_name: "n".into(),
                title: "<em>无</em><em>人</em><em>机</em>采购公告".into(),
                org: "<b>某县农业农村局</b>".into(),
                area: "六安市".into(),
                date: "2026-10-20".into(),
                amount: None,
                url: "https://x.example/a.html".into(),
                relevance: Relevance::Core,
                reasons: vec![],
                incomplete: false,
                manual_verdict: false,
                first_seen: "2026-10-20".into(),
            },
        );
        assert!(clean_stored(&mut st));
        let it = &st.items["k"];
        assert_eq!(it.title, "无人机采购公告");
        assert_eq!(it.org, "某县农业农村局");
        assert!(!clean_stored(&mut st)); // 幂等：已干净就不再改写、不触发回写
    }

    struct MockHttp {
        responses: BTreeMap<String, Result<String, HttpErr>>,
        /// 记录被调用次数，用于断言重试确实发生
        calls: std::cell::Cell<usize>,
    }
    impl MockHttp {
        fn ok(url_frag: &str, body: &str) -> Self {
            MockHttp { responses: BTreeMap::from([(url_frag.to_string(), Ok(body.to_string()))]), calls: Default::default() }
        }
    }
    impl MockHttp {
        /// 最长匹配优先：`host/` 不能把 `host/souSuo` 抢走（BTreeMap 顺序恰好是先短后长）
        fn pick(&self, url: &str) -> Result<String, HttpErr> {
            self.calls.set(self.calls.get() + 1);
            let best = self.responses.iter()
                .filter(|(k, _)| url.contains(k.as_str()))
                .max_by_key(|(k, _)| k.len());
            match best {
                Some((_, v)) => v.clone(),
                None => Err(HttpErr::Network("mock: no route".into())),
            }
        }
    }
    impl Http for MockHttp {
        fn post_json(&self, url: &str, _body: &str, _t: Duration) -> Result<String, HttpErr> {
            self.pick(url)
        }
        fn get(&self, url: &str, _t: Duration) -> Result<String, HttpErr> {
            self.pick(url)
        }
    }

    fn epoint_body() -> String {
        serde_json::json!({
            "totalrecord": 2, "pageSize": 15, "pageNo": 0,
            "result": { "results": [
                { "title": "XX县农业无人机植保服务采购公告", "webdate": "2026-10-20 10:00:00",
                  "linkurl": "/jyxx/001/202610/t20261020_123.html", "infoid": "a1",
                  "purchaseName": "XX县农业农村局", "xiaqucode": "六安市",
                  "content": "为推进统防统治工作，采购植保无人机" },
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

    /// 记录重试间隔的假等待器：断言"5s/15s 递增"而不是"随便等一下"。
    struct SpySleeper { waits: std::cell::RefCell<Vec<u64>> }
    impl SpySleeper { fn new() -> Self { SpySleeper { waits: Default::default() } } }
    impl RetrySleeper for SpySleeper {
        fn sleep_secs(&self, secs: u64) { self.waits.borrow_mut().push(secs); }
    }

    /// 前 N 次失败、之后成功的假 Http：用来验证"重试真的发生了"。
    struct FlakyHttp {
        fail_first: usize,
        seen: std::cell::Cell<usize>,
        err: HttpErr,
        body: String,
    }
    impl FlakyHttp {
        fn new(fail_first: usize, err: HttpErr, body: &str) -> Self {
            FlakyHttp { fail_first, seen: Default::default(), err, body: body.to_string() }
        }
    }
    impl Http for FlakyHttp {
        fn post_json(&self, _u: &str, _b: &str, _t: Duration) -> Result<String, HttpErr> {
            let n = self.seen.get();
            self.seen.set(n + 1);
            if n < self.fail_first { Err(self.err.clone()) } else { Ok(self.body.clone()) }
        }
        fn get(&self, _u: &str, _t: Duration) -> Result<String, HttpErr> {
            self.post_json(_u, "", _t)
        }
    }

    #[test]
    fn network_failure_retried_twice_with_5s_then_15s() {
        // 需求②：网络类失败重试 2 次，间隔 5s / 15s 递增
        let spec = SITE_REGISTRY.iter().find(|s| s.id == "luan").unwrap();
        let http = FlakyHttp::new(2, HttpErr::Network("reset by peer".into()), &epoint_body());
        let sleeper = SpySleeper::new();
        let out = fetch_site_with_retry(spec, &http, &sleeper, KEYWORD, "2026-07-28", "2026-10-28", "2026-10-28");
        assert!(out.is_ok(), "两次失败后第三次应成功");
        assert_eq!(http.seen.get(), 3, "共 3 次尝试（首试 + 2 次重试）");
        assert_eq!(*sleeper.waits.borrow(), vec![5u64, 15u64], "重试间隔必须是 5s、15s");
    }

    #[test]
    fn blocked_is_never_retried() {
        // 需求②后半：Blocked（验证码/拦截）不重试，立即降级——越重试封得越久
        let spec = SITE_REGISTRY.iter().find(|s| s.id == "luan").unwrap();
        let http = FlakyHttp::new(99, HttpErr::Blocked("captcha_page"), "");
        let sleeper = SpySleeper::new();
        let e = fetch_site_with_retry(spec, &http, &sleeper, KEYWORD, "2026-07-28", "2026-10-28", "2026-10-28").unwrap_err();
        assert_eq!(http.seen.get(), 1, "被拦截只能试 1 次");
        assert!(sleeper.waits.borrow().is_empty(), "被拦截不得等待重试");
        assert_eq!(e.status, FetchStatus::Blocked);
        assert_eq!(e.attempts, 1);
    }

    #[test]
    fn retry_exhausted_reports_three_attempts_and_status_code() {
        // 三次全失败：诊断要能说清「试了几次」「服务端给了什么码」「回了什么」
        let spec = SITE_REGISTRY.iter().find(|s| s.id == "luan").unwrap();
        // 网络类错误连试 3 次仍不行 → attempts=3，URL 落到诊断里
        let http = FlakyHttp::new(99, HttpErr::Network("connection reset".into()), "");
        let sleeper = SpySleeper::new();
        let e = fetch_site_with_retry(spec, &http, &sleeper, KEYWORD, "2026-07-28", "2026-10-28", "2026-10-28").unwrap_err();
        assert_eq!(http.seen.get(), 3);
        assert_eq!(e.attempts, 3);
        assert!(e.url.contains("luan.gov.cn"), "诊断要带实际请求 URL，实际：{}", e.url);

        // 状态码类（非网络类）一次即止，但状态码与响应体原文必须保住
        let http2 = FlakyHttp::new(99, HttpErr::Status(503, "服务暂不可用".into()), "");
        let e2 = fetch_site_with_retry(spec, &http2, &sleeper, KEYWORD, "2026-07-28", "2026-10-28", "2026-10-28").unwrap_err();
        assert_eq!(http2.seen.get(), 1, "HTTP 状态码不属网络类，按需求不重试");
        assert_eq!(e2.http_status, Some(503));
        assert_eq!(e2.body_head, "服务暂不可用");
    }

    #[test]
    fn timeout_class_is_retryable_and_tls_too() {
        // 分类边界：超时/TLS 属网络类→重试；HTTP 状态码不属网络类→不重试
        assert!(HttpErr::Timeout("timed out".into()).is_retryable());
        assert!(HttpErr::Tls("handshake".into()).is_retryable());
        assert!(HttpErr::Network("reset".into()).is_retryable());
        assert!(!HttpErr::Status(404, String::new()).is_retryable());
        assert!(!HttpErr::Blocked("captcha_page").is_retryable());
    }

    #[test]
    fn diag_keeps_only_last_three() {
        // 需求：每源滚动保留最近 3 次诊断，不无限增长
        let mut row = SiteStateRow::default();
        for i in 1..=5 {
            row.push_diag(DiagEntry { ts: format!("2026-10-2{}T09:00:00Z", i), kind: format!("第{}次", i), ..Default::default() });
        }
        assert_eq!(row.diags.len(), 3);
        assert_eq!(row.diags[0].kind, "第3次", "最早的两条应被挤掉");
        assert_eq!(row.diags[2].kind, "第5次");
    }

    #[test]
    fn diag_block_text_is_paste_back_ready() {
        // 「复制诊断」贴回来的那段文本：六项要素齐全，缺项要写清（不能留空白让人猜）
        let d = DiagEntry {
            ts: "2026-10-29T10:30:00Z".into(),
            url: "https://ggzy.luan.gov.cn/rest/esinteligentsearch/getFullTextDataNew".into(),
            http_status: Some(403),
            kind: "被拦截".into(),
            detail: "命中拦截特征 captcha_page".into(),
            body_head: "<html>频繁访问，请稍后再试".into(),
            attempts: 1,
        };
        let t = d.to_block();
        assert!(t.contains("2026-10-29T10:30:00Z"));
        assert!(t.contains("getFullTextDataNew"));
        assert!(t.contains("HTTP 状态：403"));
        assert!(t.contains("尝试次数：1"));
        assert!(t.contains("频繁访问"));

        let empty = DiagEntry::default().to_block();
        assert!(empty.contains("（未取到）"), "时间缺失要显式标出");
        assert!(empty.contains("（未发出请求）"), "未发请求要显式标出");
        assert!(empty.contains("（无）"), "无状态码要显式标出");
    }

    #[test]
    fn run_daily_writes_diag_for_success_and_failure() {
        // 诊断落库：成功也留一条（成功记录能证明"刚才还通"）
        let mut st = IntelState::default();
        let empty = serde_json::json!({ "result": { "results": [] } }).to_string();
        let http = MockHttp { responses: BTreeMap::from([("gov.cn".to_string(), Ok(empty))]), calls: Default::default() };
        let sleeper = NoSleep;
        let (results, _) = run_daily_at(&mut st, &http, &sleeper, "2026-10-28", "2026-07-28", "2026-10-28", "2026-10-29T02:00:00Z");
        let ok = results.iter().find(|r| r.site_id == "luan").unwrap();
        assert_eq!(ok.status, FetchStatus::Ok);
        let row = &st.sites["luan"];
        assert_eq!(row.diags.len(), 1);
        assert_eq!(row.diags[0].kind, "成功");
        assert_eq!(row.diags[0].ts, "2026-10-29T02:00:00Z");
        // 未匹配 mock 的源（徽采云 / 省级主站）记为失败，且诊断带上分类
        let bad = results.iter().find(|r| r.status != FetchStatus::Ok).unwrap();
        let bad_row = &st.sites[&bad.site_id];
        assert_eq!(bad_row.diags[0].kind, bad.status.label());
        assert!(!bad_row.diags[0].detail.is_empty(), "失败诊断必须有说明");
    }

    #[test]
    fn run_daily_keeps_diags_bounded_across_runs() {
        // 连跑 5 轮 → 每源仍只有 3 条，且是最近 3 条
        let mut st = IntelState::default();
        let empty = serde_json::json!({ "result": { "results": [] } }).to_string();
        let http = MockHttp { responses: BTreeMap::from([("gov.cn".to_string(), Ok(empty))]), calls: Default::default() };
        let sleeper = NoSleep;
        for i in 0..5 {
            run_daily_at(&mut st, &http, &sleeper, "2026-10-28", "2026-07-28", "2026-10-28", &format!("2026-10-29T0{}:00:00Z", i));
        }
        let row = &st.sites["luan"];
        assert_eq!(row.diags.len(), DIAG_KEEP);
        assert_eq!(row.diags[2].ts, "2026-10-29T04:00:00Z", "最后一条应是最近那轮");
    }

    #[test]
    fn ggzy_ah_primes_session_then_posts_form() {
        // 需求⑥：先 GET 首页拿会话 cookie，再 POST /souSuo 表单
        let spec = SITE_REGISTRY.iter().find(|s| s.tier == Tier::ProvincialMain).unwrap();
        let html = "<html>…<a href=\"newDetail?guid=abc123\">XX县植保无人机采购项目</a>…</html>";
        let http = MockHttp { responses: BTreeMap::from([
            ("ggzy.ah.gov.cn/souSuo".to_string(), Ok(html.to_string())),
            ("ggzy.ah.gov.cn/".to_string(), Ok("<html>首页</html>".to_string())),
        ]), calls: Default::default() };
        let items = fetch_site(spec, &http, KEYWORD, "2026-07-28", "2026-10-28", "2026-10-28").unwrap();
        assert_eq!(items.len(), 1);
        assert!(items[0].url.contains("newDetail?guid=abc123"));
        // 首页 + 检索各一次
        assert_eq!(http.calls.get(), 2, "必须先是首页、再是检索");
    }

    #[test]
    fn ggzy_ah_homepage_failure_does_not_mask_search_failure() {
        // 首页预热失败不该掩盖检索失败的真相：报检索，附一句"可能整站不可达"
        let spec = SITE_REGISTRY.iter().find(|s| s.tier == Tier::ProvincialMain).unwrap();
        let http = MockHttp { responses: BTreeMap::new(), calls: Default::default() };
        let e = fetch_site(spec, &http, KEYWORD, "2026-07-28", "2026-10-28", "2026-10-28").unwrap_err();
        assert_eq!(e.status, FetchStatus::Network);
        assert!(e.url.ends_with("/souSuo"), "诊断 URL 应是检索地址（不是首页），实际：{}", e.url);
        assert!(e.detail.contains("整站不可达"), "首页也失败时应点明整站不可达的可能");
    }

    #[test]
    fn tls_backend_reports_what_actually_took_effect() {
        // 防"以为切了其实没切"：feature 只加不改代码时后端仍会是 rustls，
        // 所以这里断言的是一个**可读的具体后端名**，而不是"配置了一个 feature"。
        let label = UreqFetcher.tls_backend();
        assert!(!label.is_empty());
        // 本机（Linux 且 native-tls 可用）应为 openssl；若初始化失败会明确写"回落"
        #[cfg(target_os = "linux")]
        {
            assert!(
                label.contains("openssl") || label.contains("回落"),
                "Linux 上 native-tls 后端应为 openssl 或明确回落，实际：{}",
                label
            );
        }
    }

    #[test]
    fn form_post_and_json_post_use_different_content_types() {
        // 需求⑥的根因验证：省级主站收表单，之前复用 post_json 发 application/json。
        // 用 127.0.0.1 上一次性 TCP 回声服务端抓请求头，直接看发出去的到底是什么。
        use std::io::{Read, Write};
        use std::net::TcpListener;

        fn capture_once(trigger: impl FnOnce(u16)) -> String {
            let l = TcpListener::bind("127.0.0.1:0").unwrap();
            let port = l.local_addr().unwrap().port();
            let h = std::thread::spawn(move || {
                let (mut s, _) = l.accept().unwrap();
                let mut buf = [0u8; 2048];
                let n = s.read(&mut buf).unwrap_or(0);
                let _ = s.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok");
                String::from_utf8_lossy(&buf[..n]).to_string()
            });
            trigger(port);
            h.join().unwrap()
        }

        let req1 = capture_once(|port| {
            let url = format!("http://127.0.0.1:{}/souSuo", port);
            let r = UreqFetcher.post_form(&url, "param=%E6%97%A0%E4%BA%BA%E6%9C%BA&currentPage=1", Duration::from_secs(5));
            assert!(r.is_ok(), "本地表单 POST 应成功：{:?}", r.err());
        });
        assert!(req1.contains("application/x-www-form-urlencoded"), "表单请求的 Content-Type 不对：\n{}", req1);
        assert!(req1.to_lowercase().contains("accept-language: zh-cn"), "缺 Accept-Language：\n{}", req1);
        assert!(req1.contains("Referer: http://127.0.0.1:"), "缺 Referer（Epoint 常校验）：\n{}", req1);

        let req2 = capture_once(|port| {
            let url = format!("http://127.0.0.1:{}/rest", port);
            let r = UreqFetcher.post_json(&url, "{}", Duration::from_secs(5));
            assert!(r.is_ok(), "本地 JSON POST 应成功：{:?}", r.err());
        });
        assert!(req2.contains("application/json"), "JSON 请求的 Content-Type 不对：\n{}", req2);
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
        let blocked = MockHttp { responses: BTreeMap::from([("luan".to_string(), Ok("<html>频繁访问!</html>".to_string()))]), calls: Default::default() };
        let e = fetch_site(spec, &blocked, KEYWORD, "2026-07-28", "2026-10-28", "2026-10-28").unwrap_err();
        assert_eq!(e.status, FetchStatus::Blocked);
        assert!(e.detail.contains("拦截特征"));
        let offline = MockHttp { responses: BTreeMap::new(), calls: Default::default() };
        let e2 = fetch_site(spec, &offline, KEYWORD, "2026-07-28", "2026-10-28", "2026-10-28").unwrap_err();
        assert_eq!(e2.status, FetchStatus::Network);
        let bad = MockHttp { responses: BTreeMap::from([("luan".to_string(), Ok("not json".to_string()))]), calls: Default::default() };
        let e3 = fetch_site(spec, &bad, KEYWORD, "2026-07-28", "2026-10-28", "2026-10-28").unwrap_err();
        assert_eq!(e3.status, FetchStatus::ParseError);
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
        let http = MockHttp { responses: BTreeMap::from([("gov.cn".to_string(), Ok(empty))]), calls: Default::default() };
        let (results, summary) = run_daily(&mut st, &http, "2026-10-28", "2026-07-28", "2026-10-28");
        assert!(!summary.is_empty());
        assert!(summary.contains("今日新增相关项目 0 条"));
        let (connected, planned) = coverage_counts();
        assert!(summary.contains(&format!("已接入 {} / 计划覆盖 {} 源", connected, planned)));
        // epoint 空数组：page0 非空记录数组→Ok 0 条；徽采云/ggzy_ah 无匹配 mock → Network 失败计入失败源
        let ok_sources = results.iter().filter(|r| r.status == FetchStatus::Ok).count();
        // 该 mock 对全部站返回 Epoint 空结构：14 个 Epoint 地市站解析成功（空结果=成功，AC-2-8）；
        // 芜湖/池州（适配器待实现）、省级主通道与徽采云（格式不匹配）计 4 个失败源，摘要如实呈现
        assert_eq!(ok_sources, 14);
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
                "publishDate": 1791856760000i64, "budgetPrice": null,
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
        let http = MockHttp { responses: BTreeMap::from([("dealer-address".to_string(), Ok(body))]), calls: Default::default() };
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
        let d0 = diff_dealers(&[], &prev);
        assert!(d0.baseline_week);
        let next = vec![
            prev[0].clone(),
            DealerNode { guid: "G2".into(), company_name: "乙公司".into(), area: "安徽省/合肥市/肥西县".into(),
                address: "b".into(), phone: "q".into(), tags: vec!["client".into()] },
        ];
        let d1 = diff_dealers(&prev, &next);
        assert!(!d1.baseline_week);
        assert_eq!(d1.added.len(), 1);
        assert_eq!(d1.removed.len(), 0);
        assert_eq!(d1.total, 2);
        // 原有网点消失
        let d2 = diff_dealers(&prev, &[]);
        assert_eq!(d2.removed.len(), 1);
    }
}
