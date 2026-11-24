// W6 监控版块组装 · 渲染级断言 + 抖音「可摘除」验证
//
// 判据来源：《设计基线落地规范》§3.4（单内容区 + 双页签 / 各自完整一屏）
// 与 §3.4.1（抖音舆情为有条件模块，可独立摘除；摘除后布局自洽）。
//
// 关键：本测试不只验「插件在的时候能跑」，还要验「把抖音注册项摘掉之后
// 监控版块依然自洽」——这是 §3.4.1 明写的验收方式（质检单独跑一次摘除构建）。
// 为了能真验这一条，测试会在内存里重新载入一份「摘除后」的 registry 并驱动渲染。
const test = require('node:test');
const assert = require('node:assert');
const fs = require('node:fs');
const path = require('node:path');
const { JSDOM } = require('/home/gem/.aily/.cli/npm/node_modules/jsdom');

const SRC = path.join(__dirname, 'src');
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const closeAll = (w) => { try { w.close(); } catch (_) {} };

/** 起一个可跑 app.js 的窗口。dealer 数据可传 null 模拟加载失败态。 */
function boot({ dealer } = {}) {
  const rawHtml = fs.readFileSync(path.join(SRC, 'index.html'), 'utf8');
  const dom = new JSDOM(rawHtml.replace(/<script[^>]*><\/script>/g, ''), {
    runScripts: 'outside-only', pretendToBeVisual: true,
  });
  const win = dom.window;
  win.__TAURI__ = {
    core: {
      invoke: async (cmd) => {
        if (cmd === 'dealer_get') return dealer === undefined ? { tree: [], snapshot: [], last_run_date: '2026-11-23' } : dealer;
        if (cmd === 'get_prefs') return { quiet_start: '22:00', quiet_end: '08:00' };
        if (cmd === 'sync_status') return { logged_in: false, state: '未登录' };
        if (cmd === 'member_list') return { members: [], me: null };
        if (cmd === 'fiscal_year') return 2027;
        if (cmd === 'list_items') return [];
        if (cmd === 'alerts') return { brief: null };
        if (cmd === 'kpi_overview') return [];
        if (cmd === 'units') return [];
        return {};
      },
    },
    dialog: { open: async () => null },
    event: { listen: async () => () => {} },
  };
  win.__TAURI__.window = { getCurrent: () => ({ label: 'main' }) };
  win.eval(fs.readFileSync(path.join(SRC, 'app.js'), 'utf8'));
  return win;
}

async function openMonitor(opts) {
  const win = boot(opts);
  await sleep(140);
  const btn = win.document.querySelector('#nav button[data-view="monitor"]');
  assert.ok(btn, '导航必须有「监控」入口（规范 §2.1 五入口之一）');
  btn.dispatchEvent(new win.MouseEvent('click', { bubbles: true }));
  await sleep(80);
  return { win, main: win.document.querySelector('#main') };
}

test('W6 · 监控页签：网点 + 抖音两个页签，抖音标记"待接入"', async () => {
  const { win, main } = await openMonitor();
  try {
    const tabs = Array.from(main.querySelectorAll('[data-mon-tab]'));
    assert.strictEqual(tabs.length, 2, '应有 2 个页签（网点 / 抖音舆情）');
    const ids = tabs.map((t) => t.dataset.monTab);
    assert.deepStrictEqual(ids, ['dealer', 'douyin'], '页签顺序：网点在前、抖音在后');
    // 抖音页签文案带「待接入」标注
    const dy = main.querySelector('[data-mon-tab="douyin"]');
    assert.ok(dy.textContent.includes('待接入'), '抖音页签须标注「待接入」');
    // 默认激活第一个页签
    assert.strictEqual(main.querySelector('[data-mon-tab="dealer"]').getAttribute('aria-selected'), 'true');
    // 网点页签**正文**已渲染（不能只靠页签标题「渠道网点」判——那个标题本身就在页签按钮里，
    // 用它当判据会假绿：正文加载失败时页签标题仍在，测试照样通过。本轮实测踩过。）
    assert.ok(main.textContent.includes('当前快照'), '默认页签应渲染网点正文（当前快照区块）');
    assert.ok(!main.textContent.includes('网点数据加载失败'), '默认页签不应停在加载失败态');
  } finally { closeAll(win); }
});

test('W6 · 抖音页签只出占位说明，不预置任何演示条目（§3.4.1 状态口径）', async () => {
  const { win, main } = await openMonitor();
  try {
    // 切到抖音页签
    const dy = main.querySelector('[data-mon-tab="douyin"]');
    dy.dispatchEvent(new win.MouseEvent('click', { bubbles: true }));
    await sleep(60);
    const main2 = win.document.querySelector('#main');
    assert.ok(main2.querySelector('.mon-pane-placeholder'), '抖音页签须渲染占位块');
    // 占位文案要点：接口额度待接入 + 不预置演示数据
    assert.ok(main2.textContent.includes('接口额度待接入'), '占位须说明接口额度待接入');
    assert.ok(!main2.textContent.includes('转待办'), '抖音占位态不得出现「转待办」按钮（无接口、无数据）');
    assert.ok(!main2.querySelector('.mon-tab[aria-selected="true"]').textContent.match(/^渠道网点/), '切换后抖音为激活态');
    // 抖音激活时不应渲染网点内容（「当前快照」等）
    assert.ok(!main2.textContent.includes('当前快照'), '抖音激活时不应同时渲染网点正文');
  } finally { closeAll(win); }
});

test('W6 · 抖音可摘除：删掉注册项后监控自洽（不渲染单页签分段控件）', async () => {
  // 这一步复刻 §3.4.1 的验收方式：单独跑一次「摘除页签」构建。
  // 做法是拦截 app.js 源码，在 eval 前把 douyin 那条注册项整块删掉，
  // 直接检验「只剩一个页签」的真实渲染结果，而不是靠我口头推断。
  const rawHtml = fs.readFileSync(path.join(SRC, 'index.html'), 'utf8');
  const dom = new JSDOM(rawHtml.replace(/<script[^>]*><\/script>/g, ''), {
    runScripts: 'outside-only', pretendToBeVisual: true,
  });
  const win = dom.window;
  win.__TAURI__ = {
    core: { invoke: async (cmd) => {
      if (cmd === 'dealer_get') return { tree: [], snapshot: [], last_run_date: '2026-11-23' };
      if (cmd === 'member_list') return { members: [], me: null };
      if (cmd === 'sync_status') return { logged_in: false };
      if (cmd === 'get_prefs') return { quiet_start: '22:00', quiet_end: '08:00' };
      return {};
    } },
    dialog: { open: async () => null }, event: { listen: async () => () => {} },
  };
  win.__TAURI__.window = { getCurrent: () => ({ label: 'main' }) };

  let src = fs.readFileSync(path.join(SRC, 'app.js'), 'utf8');
  // 精确摘除 douyin 注册块（从 `id: "douyin",` 到该对象的收尾 `},`）
  const marker = `  {\n    id: "douyin",`;
  const idx = src.indexOf(marker);
  assert.ok(idx > 0, '未在 MON_TABS 里找到 douyin 注册块（摘除实验无法进行）');
  const end = src.indexOf('\n  },\n];', idx);
  assert.ok(end > idx, '未定位到 douyin 注册块结尾');
  src = src.slice(0, idx) + src.slice(end + '\n  },\n'.length);

  win.eval(src);
  await sleep(140);
  const btn = win.document.querySelector('#nav button[data-view="monitor"]');
  btn.dispatchEvent(new win.MouseEvent('click', { bubbles: true }));
  await sleep(80);
  const main = win.document.querySelector('#main');
  try {
    // 断言 1：不再有页签条（只剩一个页签时不得渲染分段控件）
    assert.strictEqual(main.querySelectorAll('.mon-tabs').length, 0, '摘除后不应渲染页签条');
    assert.strictEqual(main.querySelectorAll('[data-mon-tab]').length, 0, '摘除后不应残留页签按钮');
    // 断言 2：网点内容照常渲染、占满整宽（无报错）
    assert.ok(main.textContent.includes('渠道网点'), '摘除后网点页签必须正常显示');
    assert.ok(main.querySelector('#dealer-refresh'), '摘除后网点刷新按钮仍应在');
    // 断言 3：无空页签、无占位残影
    assert.ok(!main.querySelector('.mon-pane-placeholder'), '摘除后不应残留占位块');
    assert.ok(!main.textContent.includes('抖音舆情'), '摘除后不应残留抖音相关文案');
  } finally { closeAll(win); }
});

test('W6 · 网点页签内容与旧 viewDealer 一致（迁移不重写内容）', async () => {
  const { win, main } = await openMonitor();
  try {
    // 网点页签应含 v2.2 既有要素：四件套 / 当前快照 / 上次比对
    assert.ok(main.textContent.includes('当前快照'), '网点页签须保留「当前快照」区块');
    assert.ok(main.textContent.includes('上次比对'), '网点页签须保留「上次比对」时间');
    assert.ok(main.querySelector('#dealer-refresh'), '网点刷新按钮须保留');
  } finally { closeAll(win); }
});
