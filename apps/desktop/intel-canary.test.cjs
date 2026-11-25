// W9 情报采集金丝雀回归 · 契约零偏差断言 + 请求体逐字段重建 + 活站实测
//
// 判据来源：《桌面端更新准备清单》§三「搜索条件（情报采集，全部保留）」——
// 该节明确「任何一条改了都会导致漏采或误采」，是 v3.4.0 改版中最需要原样保留的技术条件。
//
// 三层结构（本环境无 Rust 工具链，用「源码静态抽取 + JS 重建对拍 + 活站实测」替代编译级验证）：
//   ① 静态契约：从 crates/wb-core/src/intel.rs 逐行抽取常量与站点注册表，
//      与盘点文档 §三 的口径逐字段断言（词表 / 90 天窗 / 双字段 / 5 页 / rn / 特例站）。
//   ② 请求体重建：用抽取到的注册表值，在 JS 里按 build_epoint_request 同一语义重建
//      各站请求体，与文档口径的**期望 JSON** 逐字段 diff。
//   ③ 活站实测：把重建出的请求体原样 POST 到仍可达的 Epoint 站（六安 / 宿州 / 滁州），
//      断言 200 + JSON + records 命中。这一层证明「契约 → 请求体 → 真站点」整链路没漂。
//
// 产出：测试通过后，如设置了 CANARY_OUT 环境变量，活站证据（URL / 请求体 / 命中数）
// 会写成 JSON 报告，供金丝雀报告文档引用（上云由 CI/交付脚本完成，测试只落盘）。
const test = require('node:test');
const assert = require('node:assert');
const fs = require('node:fs');
const path = require('node:path');

const INTEL_RS = path.join(__dirname, '../../crates/wb-core/src/intel.rs');
const LIB_RS = path.join(__dirname, 'src-tauri/src/lib.rs');
const rs = fs.readFileSync(INTEL_RS, 'utf8');
const libRs = fs.readFileSync(LIB_RS, 'utf8');

// ─────────────────────────────────────────────────────────────────────────
// §1 源码静态抽取（唯一事实源：intel.rs / lib.rs，不是本文档记忆）
// ─────────────────────────────────────────────────────────────────────────
function constStr(name) {
  const m = rs.match(new RegExp(`pub const ${name}: &str = "([^"]+)";`));
  return m ? m[1] : null;
}
function constUsize(name) {
  const m = rs.match(new RegExp(`pub const ${name}: usize = (\\d+);`));
  return m ? Number(m[1]) : null;
}
function wordList(name) {
  const m = rs.match(new RegExp(`pub const ${name}: &\\[&str\\] = &\\[([^\\]]+)\\];`));
  if (!m) return null;
  return [...m[1].matchAll(/"([^"]+)"/g)].map((x) => x[1]);
}

// 站点注册表：截取 SITE_REGISTRY 块，按 `SiteSpec {` 切片逐站解析。
// 结构化解析而不是字面量锚点——注册项写法有 `EpointCfg::std()`、`{ cnum: "001", ..std() }`、
// 全字段展开三种形态，字面量匹配会漏（参见 MEMORY：锚点必须按行形状）。
function parseRegistry() {
  const start = rs.indexOf('pub const SITE_REGISTRY');
  const end = rs.indexOf('];', start);
  const block = rs.slice(start, end);
  const sites = [];
  const chunks = block.split(/SiteSpec \{/).slice(1);
  for (const c of chunks) {
    const id = c.match(/id: "([^"]+)"/)?.[1];
    const name = c.match(/name: "([^"]+)"/)?.[1];
    const base = c.match(/base: "([^"]+)"/)?.[1];
    const tier = c.match(/tier: Tier::(\w+)/)?.[1];
    const site = { id, name, base, tier, epoint: null };
    const eStart = c.indexOf('epoint: Some(EpointCfg');
    if (eStart >= 0) {
      const eBlock = c.slice(eStart, eStart + 220);
      const prefix = eBlock.match(/prefix: "([^"]*)"/);
      const cnum = eBlock.match(/cnum: "([^"]*)"/);
      const rn = eBlock.match(/rn: (\d+)/);
      const dw = eBlock.match(/double_wrap: (true|false)/);
      const std = /EpointCfg::std\(\)/.test(eBlock);
      // std() 默认：prefix "/inteligentsearch" / cnum "" / rn 15 / double_wrap false
      site.epoint = {
        prefix: prefix ? prefix[1] : std ? '/inteligentsearch' : null,
        cnum: cnum ? cnum[1] : std ? '' : null,
        rn: rn ? Number(rn[1]) : std ? 15 : null,
        double_wrap: dw ? dw[1] === 'true' : std ? false : null,
      };
    }
    sites.push(site);
  }
  return sites;
}
function releaseEnabled() {
  const m = rs.match(/pub const RELEASE_ENABLED_SITES: &\[&str\] =\s*&\[([^\]]+)\];/s);
  return [...m[1].matchAll(/"([^"]+)"/g)].map((x) => x[1]);
}

const KEYWORD = constStr('KEYWORD');
const MAX_PAGES = constUsize('MAX_PAGES_PER_SITE');
const THEME = wordList('THEME_WORDS');
const PLUS = wordList('PLUS_WORDS');
const EXCLUDE = wordList('EXCLUDE_WORDS');
const REGISTRY = parseRegistry();
const ENABLED = releaseEnabled();

// ─────────────────────────────────────────────────────────────────────────
// §2 build_epoint_request 的 JS 同语义重建（与 intel.rs:637 逐字段对齐）
//   URL：base + prefix + （huangshan 旧端点 getFullTextData，其余 getFullTextDataNew）
//   body：token "" / pn=page（0 起）/ rn=cfg.rn / sdt / edt / wd / fields "title;content"
//         / cnum / sort "{'webdate':'0'}" / ssort "title" / cl 500 / terminal "" / condition []
//         / time null（v2.3.0 修正①：time 必须空，二次过滤会漏）
// ─────────────────────────────────────────────────────────────────────────
function buildEpointRequest(spec, wd, start, end, page) {
  const cfg = spec.epoint;
  assert.ok(cfg, 'epoint site');
  const path = spec.id === 'huangshan'
    ? `${cfg.prefix}/rest/esinteligentsearch/getFullTextData`
    : `${cfg.prefix}/rest/esinteligentsearch/getFullTextDataNew`;
  return {
    url: spec.base + path,
    body: {
      token: '', pn: page, rn: cfg.rn, sdt: start, edt: end, wd,
      fields: 'title;content', cnum: cfg.cnum, sort: "{'webdate':'0'}",
      ssort: 'title', cl: 500, terminal: '', condition: [], time: null,
    },
  };
}
const byId = (id) => REGISTRY.find((s) => s.id === id);

// 90 天窗口：与 lib.rs spawn_intel_run 同一口径（today.add_days(-90).to_iso() → today）
//
// v3.4.0 质检 B2·阻断 修复：窗口一律按「日期字符串」算，全程不碰时刻。
// 旧实现 const START_90 = toIso(new Date(TODAY.getTime() - 90 * 86400_000)) 让 START_90
// 被 toIso 截成纯日期（UTC 00:00），而 TODAY 仍带执行时刻；两者相减在 UTC 时数 ≥ 12 时
// 得到 90.5+ 天，被 Math.round 进位成 91 —— 断言随执行时刻漂移，是质检点名的时刻耦合缺陷。
const TODAY = new Date();
const toIso = (d) => d.toISOString().slice(0, 10);
// 把 Date 或 'YYYY-MM-DD' 归一到该日的 UTC 零点毫秒数；日期差用它算，与时区/时刻无关
const dayUtc = (v) => {
  const iso = typeof v === 'string' ? v.slice(0, 10) : toIso(v);
  return Date.UTC(+iso.slice(0, 4), +iso.slice(5, 7) - 1, +iso.slice(8, 10));
};
const START_90 = toIso(new Date(dayUtc(TODAY) - 90 * 86400_000));

// ─────────────────────────────────────────────────────────────────────────
// §1 断言：静态契约与盘点文档 §三逐字段对齐
// ─────────────────────────────────────────────────────────────────────────
test('W9 · 检索主词：无人机（KEYWORD）', () => {
  assert.strictEqual(KEYWORD, '无人机');
});

test('W9 · 分页上限：每站最多 5 页（MAX_PAGES_PER_SITE）', () => {
  assert.strictEqual(MAX_PAGES, 5);
  // 翻页循环语义：for page in 0..MAX_PAGES_PER_SITE（0 起含头）——pn 从 0 开始
  assert.match(rs, /for page in 0\.\.MAX_PAGES_PER_SITE/);
});

test('W9 · 三层词表逐字对齐（主题 6 / 相关 9 / 排除 6）', () => {
  assert.deepStrictEqual(THEME, ['无人机', '无人飞机', '植保无人机', '植保机', '飞防', '农用航空']);
  assert.deepStrictEqual(PLUS, ['农业', '农田', '农机', '植保', '统防统治', '高标准农田', '农林', '乡村振兴', '病虫害']);
  assert.deepStrictEqual(EXCLUDE, ['航拍', '测绘', '电力巡线', '巡检', '安防监控', '青少年科普']);
});

test('W9 · 检索字段：title;content 双字段（漏 48 条教训）', () => {
  // 请求体里的字面量（intel.rs:662）
  assert.match(rs, /"fields": "title;content"/);
});

test('W9 · 时间窗：lib.rs 用 add_days(-90) 起、today 止', () => {
  // spawn_intel_run 的窗口四元组：end = today.to_iso()，start = today.add_days(-90).to_iso()
  // （注意不能按 `};` 截块匹配——函数体里 `else { return };` 会让非贪婪正则提前收口）
  assert.match(libRs, /t\.add_days\(-90\)\.to_iso\(\)/);
  const i = libRs.indexOf('fn spawn_intel_run');
  const seg = libRs.slice(i, i + 2000);
  assert.ok(seg.includes('add_days(-90)'), '90 天窗必须在 spawn_intel_run 内（采集入口）');
  assert.ok(seg.includes('to_iso()'), '窗口日期必须走 to_iso()（yyyy-MM-dd）');
});

test('W9 · 源站注册表：21 站齐全', () => {
  assert.strictEqual(REGISTRY.length, 21);
  const ids = REGISTRY.map((s) => s.id);
  for (const id of ['ggzy_ah', 'huicaiyun', 'ccgp', 'deal_ggzy',
    'anqing', 'chuzhou', 'maanshan', 'luan', 'bozhou', 'hefei', 'xuancheng',
    'fuyang', 'suzhou_ah', 'wuhu', 'chizhou', 'bengbu', 'huainan', 'huaibei', 'huangshan',
    'tongling', 'pjwnc']) {
    assert.ok(ids.includes(id), `注册表缺 ${id}`);
  }
});

test('W9 · Epoint 特例站逐站参数（§三「各源特殊参数」全表）', () => {
  // 宿州：/XZinteligentsearch + cnum 全串（v3.3.1 修正，旧值是江苏苏州站）
  const sz = byId('suzhou_ah');
  assert.strictEqual(sz.base, 'https://ggzyjy.ahsz.gov.cn');
  assert.strictEqual(sz.epoint.prefix, '/XZinteligentsearch');
  assert.strictEqual(sz.epoint.cnum, '001;002;003;004;005;006;007');
  // 淮南：double_wrap = true（响应双层包装）
  assert.strictEqual(byId('huainan').epoint.double_wrap, true);
  assert.strictEqual(byId('huainan').epoint.prefix, '');
  // 黄山：/inteligentSearch 前缀（v2.3.0 实测 403 Missing authorization header）
  assert.strictEqual(byId('huangshan').epoint.prefix, '/inteligentSearch');
  // 六安 / 阜阳：cnum = 001
  assert.strictEqual(byId('luan').epoint.cnum, '001');
  assert.strictEqual(byId('fuyang').epoint.cnum, '001');
  // 滁州：/inteligentsearchNew
  assert.strictEqual(byId('chuzhou').epoint.prefix, '/inteligentsearchNew');
  // 马鞍山：_es 根路径 + 单页上限 10
  assert.strictEqual(byId('maanshan').epoint.prefix, '');
  assert.strictEqual(byId('maanshan').epoint.rn, 10);
  // 宣城：_es 根路径
  assert.strictEqual(byId('xuancheng').epoint.prefix, '');
  // 标准站（安庆/亳州/合肥/蚌埠/淮北/铜陵）：EpointCfg::std()
  for (const id of ['anqing', 'bozhou', 'hefei', 'bengbu', 'huaibei', 'tongling']) {
    const e = byId(id).epoint;
    assert.deepStrictEqual(e, { prefix: '/inteligentsearch', cnum: '', rn: 15, double_wrap: false }, `${id} 应为 std()`);
  }
});

test('W9 · 首发闸门白名单 8 站不变', () => {
  assert.deepStrictEqual(ENABLED, ['pjwnc', 'chuzhou', 'luan', 'bozhou', 'suzhou_ah', 'bengbu', 'huaibei', 'chizhou']);
});

// ─────────────────────────────────────────────────────────────────────────
// §2 断言：请求体逐字段重建 diff
// ─────────────────────────────────────────────────────────────────────────
test('W9 · 标准站请求体逐字段 diff（以亳州为例，90 天窗 + 全部 14 个键）', () => {
  const r = buildEpointRequest(byId('bozhou'), KEYWORD, START_90, toIso(TODAY), 0);
  assert.strictEqual(r.url, 'https://ggzy.bozhou.gov.cn/inteligentsearch/rest/esinteligentsearch/getFullTextDataNew');
  assert.deepStrictEqual(r.body, {
    token: '', pn: 0, rn: 15, sdt: START_90, edt: toIso(TODAY), wd: '无人机',
    fields: 'title;content', cnum: '', sort: "{'webdate':'0'}",
    ssort: 'title', cl: 500, terminal: '', condition: [], time: null,
  });
  // 90 天窗自检：start 到 end 恰好 90 天（按日期字符串算，与时区/执行时刻无关）
  const days = (dayUtc(toIso(TODAY)) - dayUtc(START_90)) / 86400_000;
  assert.strictEqual(days, 90);
});

test('W9 · 特例站请求体逐字段 diff（宿州 / 六安 / 马鞍山 / 黄山 / 滁州）', () => {
  const sz = buildEpointRequest(byId('suzhou_ah'), KEYWORD, START_90, toIso(TODAY), 0);
  assert.strictEqual(sz.url, 'https://ggzyjy.ahsz.gov.cn/XZinteligentsearch/rest/esinteligentsearch/getFullTextDataNew');
  assert.strictEqual(sz.body.cnum, '001;002;003;004;005;006;007');

  const la = buildEpointRequest(byId('luan'), KEYWORD, START_90, toIso(TODAY), 0);
  assert.strictEqual(la.url, 'https://ggzy.luan.gov.cn/inteligentsearch/rest/esinteligentsearch/getFullTextDataNew');
  assert.strictEqual(la.body.cnum, '001');

  const mas = buildEpointRequest(byId('maanshan'), KEYWORD, START_90, toIso(TODAY), 3);
  assert.strictEqual(mas.url, 'https://ggzy.mas.gov.cn/rest/esinteligentsearch/getFullTextDataNew');
  assert.strictEqual(mas.body.rn, 10);
  assert.strictEqual(mas.body.pn, 3); // pn 随页码（0 起），不是 1 起

  // 黄山走旧端点 getFullTextData（无 New）
  const hs = buildEpointRequest(byId('huangshan'), KEYWORD, START_90, toIso(TODAY), 0);
  assert.strictEqual(hs.url, 'https://ggzy.huangshan.gov.cn/inteligentSearch/rest/esinteligentsearch/getFullTextData');

  const cz = buildEpointRequest(byId('chuzhou'), KEYWORD, START_90, toIso(TODAY), 0);
  assert.strictEqual(cz.url, 'https://ggzy.chuzhou.gov.cn/inteligentsearchNew/rest/esinteligentsearch/getFullTextDataNew');
});

// ─────────────────────────────────────────────────────────────────────────
// §3 断言：活站实测（契约 → 重建请求体 → 真站点响应命中）
//   覆盖三个形态：标准+特例 cnum（六安）、独立前缀+全串 cnum（宿州）、独立前缀（滁州）。
//   这是金丝雀：站点若改了接口/风控，这里先红，先修再合。
// ─────────────────────────────────────────────────────────────────────────
const LIVE_SITES = ['luan', 'suzhou_ah', 'chuzhou'];
const liveEvidence = [];

// SKIP_LIVE 门控（2026-11-25 追加授权，归 W9 PR #9 边界；与 guard-gate.cjs 判据联合对齐）：
// 默认关闭——本地/境内环境活站三例照跑（金丝雀语义不变）。
// 仅显式 SKIP_LIVE=1 时跳过：GitHub 境外 runner 物理不可达境内站（ETIMEDOUT 实证），
// 门禁口径为「境外 runner 不承载活站断言，活站三例由境内环境验证」——已知取舍，非验证通过。
// guard-gate.cjs 只容忍「SKIP_LIVE=1 且跳过恰好 3 条、全部是本三例」，其余任何跳过仍红。
const SKIP_LIVE = process.env.SKIP_LIVE === '1';

for (const id of LIVE_SITES) {
  test(`W9 · 活站实测：${id} 按契约请求体真实 POST 命中`, { timeout: 60_000, skip: SKIP_LIVE && 'SKIP_LIVE=1（境外 runner 不可达境内站，由境内环境验证）' }, async () => {
    const spec = byId(id);
    const { url, body } = buildEpointRequest(spec, KEYWORD, START_90, toIso(TODAY), 0);
    const resp = await fetch(url, {
      method: 'POST',
      headers: { 'Content-Type': 'application/json', 'User-Agent': 'Mozilla/5.0' },
      body: JSON.stringify(body),
      signal: AbortSignal.timeout(40_000), // 单请求 40s 超时（与 FETCH_TIMEOUT_SECS 同口径）
    });
    assert.strictEqual(resp.status, 200, `${id} HTTP ${resp.status}`);
    const text = await resp.text();
    let v = JSON.parse(text);
    // 宿州/淮南双层包装：unwrap_double_json 同语义（按内容判据，不按站点开关）
    if (typeof v.content === 'string' && v.content.trimStart().startsWith('{')) {
      const inner = JSON.parse(v.content);
      if (inner.result) v = inner;
    }
    const total = v?.result?.totalcount;
    const records = v?.result?.records;
    assert.ok(typeof total === 'number' || typeof total === 'string', `${id} 响应缺 totalcount`);
    assert.ok(Number(total) > 0, `${id} 90 天窗内 0 命中（金丝雀口径：>0 才算链路活）`);
    assert.ok(Array.isArray(records) && records.length > 0, `${id} records 为空`);
    assert.ok(records[0].title, `${id} record 缺 title 字段`);
    liveEvidence.push({
      site: id, name: spec.name, url, http: resp.status,
      totalcount: Number(total), records: records.length,
      request_body: body, window: [START_90, toIso(TODAY)], wd: KEYWORD,
    });
  });
}

test('W9 · 金丝雀报告落盘（CANARY_OUT 设置时）', () => {
  const out = process.env.CANARY_OUT;
  if (!out) return; // 常规跑测试不写盘
  if (SKIP_LIVE) return; // 活站被跳过时无证据可收，不生成报告（活站断言归境内环境验证）
  assert.strictEqual(liveEvidence.length, LIVE_SITES.length, '活站证据未收齐，不生成报告');
  fs.writeFileSync(out, JSON.stringify({
    run_at: new Date().toISOString(),
    branch: 'feat/v3.4.0',
    scope: 'W9 情报采集金丝雀回归',
    contract: {
      keyword: KEYWORD, max_pages_per_site: MAX_PAGES,
      fields: 'title;content', window_days: 90,
      theme_words: THEME, plus_words: PLUS, exclude_words: EXCLUDE,
      release_enabled: ENABLED, registry_sites: REGISTRY.length,
    },
    live: liveEvidence,
  }, null, 2));
});
