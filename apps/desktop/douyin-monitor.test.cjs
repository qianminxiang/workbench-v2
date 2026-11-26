// v3.4.0 · 抖音舆情免费通道低密度巡检小闭环 · 单元 + 渲染级断言
//
// 判据来源：任务书附加条款②（节流红线硬编码常量，配置只能红线内收紧）、
// ③（召回→人工筛三态可追溯）、④（默认关闭、设置页手动开启、升级后不突然轮询）、
// 验收①（未激活零残留）/③（召回条目进监控版块人工筛带留痕）/④（超红线配置被拦）。
//
// 三层：
//   A. 纯逻辑单元（只载 douyin-monitor.js，window.__DM）：红线常量 / clampSettings /
//      三态状态机 / 内容键去重 / 粘贴解析 / 轮次闸门 / 配置存取与损坏钳制；
//   B. 渲染级集成（douyin-monitor.js + intel-projects.js + app.js 真渲染）：
//      默认关闭零残留 → 设置页激活 → 超红线配置被拦 → 面板 → 导入 → 筛判 → 转待办。
//
// 注意：app.js 的 bindMarquee() 有 30s setInterval，每个用例结束必须 win.close()。
const test = require('node:test');
const assert = require('node:assert');
const fs = require('node:fs');
const path = require('node:path');
const { JSDOM } = require('jsdom');

const SRC = path.join(__dirname, 'src');
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const closeAll = (w) => { try { w.close(); } catch (_) {} };

/** 只载 douyin-monitor.js 的最小窗口（localStorage 可预置）。 */
function bootDm({ seed } = {}) {
  const dom = new JSDOM('<!DOCTYPE html><html><body></body></html>', {
    runScripts: 'outside-only', pretendToBeVisual: true, url: 'http://localhost/',
  });
  const win = dom.window;
  if (seed) {
    for (const [k, v] of Object.entries(seed)) win.localStorage.setItem(k, typeof v === 'string' ? v : JSON.stringify(v));
  }
  win.eval(fs.readFileSync(path.join(SRC, 'douyin-monitor.js'), 'utf8'));
  return win;
}

/** 完整应用窗口（真实 index.html 骨架 + 模块 + app.js），可预置 localStorage。 */
function bootApp({ seed } = {}) {
  const rawHtml = fs.readFileSync(path.join(SRC, 'index.html'), 'utf8');
  const html = rawHtml.replace(/<script[^>]*><\/script>/g, '');
  const dom = new JSDOM(html, { runScripts: 'outside-only', pretendToBeVisual: true, url: 'http://localhost/' });
  const win = dom.window;
  if (seed) {
    for (const [k, v] of Object.entries(seed)) win.localStorage.setItem(k, typeof v === 'string' ? v : JSON.stringify(v));
  }
  const invoked = [];
  win.__TAURI__ = {
    core: {
      invoke: async (cmd, payload) => {
        invoked.push({ cmd, payload });
        if (cmd === 'dealer_get') return { tree: [], snapshot: [], last_run_date: '2026-11-23' };
        if (cmd === 'get_prefs') return { quiet_start: '22:00', quiet_end: '08:00' };
        if (cmd === 'sync_status') return { logged_in: false, state: '未登录' };
        if (cmd === 'member_list') return { members: [], me: null };
        if (cmd === 'fiscal_year') return 2027;
        if (cmd === 'list_items') return [];
        if (cmd === 'alerts') return { brief: null };
        if (cmd === 'kpi_overview') return [];
        if (cmd === 'units') return [];
        if (cmd === 'todo_create') return { id: 'todo:1', ok: true };
        return {};
      },
    },
    dialog: { open: async () => null },
    event: { listen: async () => () => {} },
  };
  win.__TAURI__.window = { getCurrent: () => ({ label: 'main' }) };
  for (const f of ['douyin-monitor.js', 'intel-projects.js', 'app.js']) {
    win.eval(fs.readFileSync(path.join(SRC, f), 'utf8'));
  }
  return { win, invoked };
}

async function openView(win, view) {
  const btn = win.document.querySelector(`#nav button[data-view="${view}"]`);
  assert.ok(btn, `导航缺少「${view}」入口`);
  btn.dispatchEvent(new win.MouseEvent('click', { bubbles: true }));
  await sleep(80);
  return win.document.querySelector('#main');
}

async function openDouyinTab(win) {
  const main = await openView(win, 'monitor');
  const dy = main.querySelector('[data-mon-tab="douyin"]');
  assert.ok(dy, '监控页签里应有抖音舆情');
  dy.dispatchEvent(new win.MouseEvent('click', { bubbles: true }));
  await sleep(60);
  return win.document.querySelector('#main');
}

// ═══════════════ A. 纯逻辑单元 ═══════════════

test('A1 · 红线常量：数值与冻结（附加条款②）', () => {
  const win = bootDm();
  const H = win.__DM.HARD;
  assert.strictEqual(H.MIN_QUERY_INTERVAL_SEC, 12, '查询间隔下限 12 秒');
  assert.strictEqual(H.MAX_ROUNDS_PER_DAY, 6, '每日轮次上限 6');
  assert.strictEqual(H.MIN_ROUNDS_PER_DAY, 4, '每日轮次下限 4');
  assert.strictEqual(H.MIN_WORDS_PER_ROUND, 20, '单轮词数下限 20');
  assert.strictEqual(H.MAX_WORDS_PER_ROUND, 40, '单轮词数上限 40');
  assert.strictEqual(H.MIN_ROUND_GAP_MIN, 30, '轮间隔下限 30 分钟');
  assert.strictEqual(H.COOLDOWN_AFTER_CAPTCHA_MIN, 2, '验证码冷却 2 分钟');
  assert.ok(Object.isFrozen(H), '常量必须 Object.freeze，运行期不可改');
  assert.throws(() => { 'use strict'; H.MIN_QUERY_INTERVAL_SEC = 1; }, '严格模式下改冻结常量必须抛错');
  closeAll(win);
});

test('A2 · clampSettings：超红线一律钳回，红线内保留，类型陷阱按回退值（11 组对抗）', () => {
  const win = bootDm();
  const DM = win.__DM;
  const c = (raw) => DM.clampSettings(raw).value;
  // 间隔：<12 一律钳到 12（1/4/11/11.9/-3/0）；≥12 保留
  for (const v of [1, 4, 11, 11.9, -3, 0]) assert.strictEqual(c({ queryIntervalSec: v }).queryIntervalSec, 12, `间隔 ${v} 必须钳到 12`);
  assert.strictEqual(c({ queryIntervalSec: 60 }).queryIntervalSec, 60, '间隔 60 在红线内保留');
  // 轮次：>6 钳到 6；<4 钳到 4；4-6 保留
  for (const v of [7, 8, 20, 999]) assert.strictEqual(c({ roundsPerDay: v }).roundsPerDay, 6, `轮次 ${v} 必须钳到 6`);
  for (const v of [0, 1, 3]) assert.strictEqual(c({ roundsPerDay: v }).roundsPerDay, 4, `轮次 ${v} 必须钳到 4`);
  assert.strictEqual(c({ roundsPerDay: 5 }).roundsPerDay, 5, '轮次 5 在红线内保留');
  // 词数：8→20，200→40，30 保留
  assert.strictEqual(c({ wordsPerRound: 8 }).wordsPerRound, 20);
  assert.strictEqual(c({ wordsPerRound: 200 }).wordsPerRound, 40);
  assert.strictEqual(c({ wordsPerRound: 30 }).wordsPerRound, 30);
  // 轮间隔：1/5/29→30，45 保留
  for (const v of [1, 5, 29]) assert.strictEqual(c({ roundGapMin: v }).roundGapMin, 30, `轮间隔 ${v} 必须钳到 30`);
  assert.strictEqual(c({ roundGapMin: 45 }).roundGapMin, 45);
  // 类型陷阱：bool / NaN / Infinity / 非数字串 → 回退默认（12 等），不得被 Number 强转绕过
  for (const v of [true, false, NaN, Infinity, -Infinity, 'abc', null, undefined]) {
    assert.strictEqual(c({ queryIntervalSec: v }).queryIntervalSec, 12, `间隔 ${String(v)} 必须按回退值 12`);
  }
  assert.strictEqual(c({ queryIntervalSec: true }).queryIntervalSec, 12, 'true 不得被当 1（否则 1<12 也会钳，但语义上类型就该拒）');
  // 钳制事件必须如实记录 wanted→got，不许静默
  const r = DM.clampSettings({ roundsPerDay: 20, wordsPerRound: 200 });
  const evR = r.clamped.find((e) => e.key === 'roundsPerDay');
  const evW = r.clamped.find((e) => e.key === 'wordsPerRound');
  assert.ok(evR && evR.wanted === 20 && evR.got === 6, '轮次钳制事件要记 wanted/got');
  assert.ok(evW && evW.wanted === 200 && evW.got === 40, '词数钳制事件要记 wanted/got');
  assert.strictEqual(r.clamped.find((e) => e.key === 'queryIntervalSec'), undefined, '没改间隔就不该有该键的钳制事件');
  closeAll(win);
});

test('A3 · 三态状态机：流转 / 留痕 / 非法拒 / 撤销 / 重召回保判（附加条款③）', () => {
  const win = bootDm();
  const DM = win.__DM;
  const ST = DM.ST;
  const item = () => ({ id: 'k1', title: '合肥 大疆 炸机', state: ST.UNSORTED, screenedBy: null, screenedAt: null, reason: null });
  // 三态可达
  const s = DM.screen(item(), ST.SCREENED, { by: '大角子', at: '2026-11-25T10:00:00+08:00', reason: '属地命中合肥' });
  assert.strictEqual(s.state, ST.SCREENED);
  assert.strictEqual(s.screenedBy, '大角子');
  assert.match(s.screenedAt, /^2026-11-25/);
  const ig = DM.screen(item(), ST.IGNORED, { by: '大角子', at: '2026-11-25T10:01:00+08:00', reason: '无行业词' });
  assert.strictEqual(ig.state, ST.IGNORED);
  // 非法状态被拒；直接 screen 回未筛被拒（必须走 undo）
  assert.throws(() => DM.screen(item(), '已看', {}), /非法状态/);
  assert.throws(() => DM.screen(item(), ST.UNSORTED, {}), /undo/);
  // 撤销清痕迹
  const back = DM.undo(s);
  assert.strictEqual(back.state, ST.UNSORTED);
  assert.strictEqual(back.screenedBy, null);
  assert.strictEqual(back.reason, null);
  // 重召回保判（对齐仓库 AC-2-7：人工改判不被重新召回覆盖）
  const prior = [DM.screen(item(), ST.IGNORED, { by: '大角子', at: 't', reason: '用户判为噪声' })];
  const fresh = [{ ...item(), title: '旧-重新抓到', recalledAt: '2026-11-26T08:00:00+08:00' }, { ...item(), id: 'k2', title: '新条目' }];
  const merged = DM.reRecall(prior, fresh);
  const k1 = merged.find((r) => r.id === 'k1');
  assert.strictEqual(k1.state, ST.IGNORED, '已忽略不得被重抓重置为未筛');
  assert.strictEqual(k1.screenedBy, '大角子');
  assert.strictEqual(k1.reason, '用户判为噪声');
  assert.strictEqual(merged.find((r) => r.id === 'k2').state, ST.UNSORTED, '新条目应为未筛');
  closeAll(win);
});

test('A4 · 月度汇总：区分召回总量与被看过判过的量', () => {
  const win = bootDm();
  const DM = win.__DM;
  const ST = DM.ST;
  const base = { id: 'a', title: 'x', state: ST.UNSORTED, screenedBy: null, screenedAt: null, reason: null };
  const rows = [
    base,
    DM.screen({ ...base, id: 'b' }, ST.SCREENED, { by: 'u', at: 't', reason: 'r1' }),
    DM.screen({ ...base, id: 'c' }, ST.IGNORED, { by: 'u', at: 't', reason: 'r2' }),
    { ...base, id: 'd' },
  ];
  const sum = DM.monthlySummary(rows);
  assert.strictEqual(sum.recalled, 4);
  assert.strictEqual(sum.screened, 2, '被看过判过的只有 2 条');
  assert.strictEqual(sum.untouched, 2, '从未被看过的 2 条要能单独算出');
  assert.strictEqual(sum.byState[ST.UNSORTED], 2);
  assert.strictEqual(sum.byState[ST.SCREENED], 1);
  assert.strictEqual(sum.byState[ST.IGNORED], 1);
  assert.strictEqual(sum.screenedRate, 0.5);
  const empty = DM.monthlySummary([]);
  assert.strictEqual(empty.recalled, 0);
  assert.strictEqual(empty.screenedRate, null);
  closeAll(win);
});

test('A5 · 内容键：同内容同键、日期或来源变则键变（序号不可作边界）', () => {
  const win = bootDm();
  const DM = win.__DM;
  const k1 = DM.contentKey('黑猫', '2026-11-24', '大疆T50电池炸机 售后推诿');
  assert.strictEqual(k1, DM.contentKey('黑猫', '2026-11-24', '大疆T50电池炸机 售后推诿'), '同内容必须同键');
  assert.strictEqual(k1, DM.contentKey('黑猫', '2026-11-24', ' 大疆T50电池炸机  售后推诿 '), '空白差异不产生新键');
  assert.notStrictEqual(k1, DM.contentKey('黑猫', '2026-11-25', '大疆T50电池炸机 售后推诿'), '日期不同是不同事件');
  assert.notStrictEqual(k1, DM.contentKey('B站', '2026-11-24', '大疆T50电池炸机 售后推诿'), '来源不同是不同事件');
  closeAll(win);
});

test('A6 · 粘贴解析：一行一条、可选日期前缀、空行跳过、批内去重', () => {
  const win = bootDm();
  const DM = win.__DM;
  const text = [
    '2026-11-24 大疆T50电池炸机 售后推诿',
    '2026-11-23 农业植保无人机 云台故障',
    '',
    '微头条看到的换新纠纷',
    '2026-11-24 大疆T50电池炸机 售后推诿',
  ].join('\n');
  const parsed = DM.parseRecallPaste(text, { source: '黑猫', dateKey: '2026-11-25' });
  assert.strictEqual(parsed.length, 3, '批内重复与空行后应剩 3 条');
  const a = parsed[0];
  assert.strictEqual(a.dateKey, '2026-11-24', '行内日期优先');
  assert.strictEqual(a.title, '大疆T50电池炸机 售后推诿');
  assert.strictEqual(a.source, '黑猫');
  assert.strictEqual(a.state, DM.ST.UNSORTED);
  assert.strictEqual(parsed[2].dateKey, '2026-11-25', '无日期行用导入时指定日期');
  closeAll(win);
});

test('A7 · 导入合并：已筛判不被覆盖、新条目未筛、计数 added/updated 如实', () => {
  const win = bootDm();
  const DM = win.__DM;
  const old = [DM.screen(
    { id: DM.contentKey('黑猫', '2026-11-24', '电池炸机'), title: '电池炸机', source: '黑猫', dateKey: '2026-11-24', state: DM.ST.UNSORTED, screenedBy: null, screenedAt: null, reason: null },
    DM.ST.IGNORED, { by: '大角子', at: '2026-11-25T09:00:00+08:00', reason: '噪声' })];
  const fresh = DM.parseRecallPaste('2026-11-24 电池炸机\n2026-11-24 新纠纷', { source: '黑猫' });
  const { items, added, updated } = DM.importItems(old, fresh);
  assert.strictEqual(added, 1);
  assert.strictEqual(updated, 1);
  const hit = items.find((r) => r.title === '电池炸机');
  assert.strictEqual(hit.state, DM.ST.IGNORED, '已忽略不被重置');
  assert.strictEqual(hit.reason, '噪声');
  const neu = items.find((r) => r.title === '新纠纷');
  assert.strictEqual(neu.state, DM.ST.UNSORTED);
  closeAll(win);
});

test('A8 · 轮次闸门：超轮 / 间隔未到 / 放行，都给可读原因', () => {
  const win = bootDm();
  const DM = win.__DM;
  const cfg = { roundsPerDay: 4, roundGapMin: 30 };
  const mk = (no, iso) => ({ id: 'r' + no, title: 't' + no, state: DM.ST.UNSORTED, recalledAt: iso, roundNo: no });
  // 今日已 4 轮 → 超轮
  const items4 = [mk(1, '2026-11-25T08:00:00+08:00'), mk(2, '2026-11-25T09:00:00+08:00'), mk(3, '2026-11-25T10:00:00+08:00'), mk(4, '2026-11-25T11:00:00+08:00')];
  let g = DM.roundGate(cfg, items4, new Date('2026-11-25T12:00:00+08:00'));
  assert.strictEqual(g.ok, false);
  assert.match(g.reason, /轮/);
  assert.strictEqual(g.roundsToday, 4);
  // 未超轮但上轮 10 分钟前 → 间隔未到
  const items1 = [mk(1, '2026-11-25T11:50:00+08:00')];
  g = DM.roundGate(cfg, items1, new Date('2026-11-25T12:00:00+08:00'));
  assert.strictEqual(g.ok, false);
  assert.match(g.reason, /分钟|间隔/);
  // 上轮 40 分钟前 → 放行
  const items1b = [mk(1, '2026-11-25T11:15:00+08:00')];
  g = DM.roundGate(cfg, items1b, new Date('2026-11-25T12:00:00+08:00'));
  assert.strictEqual(g.ok, true);
  // 昨天的轮次不计入今日
  const itemsY = [mk(1, '2026-11-24T23:00:00+08:00')];
  g = DM.roundGate(cfg, itemsY, new Date('2026-11-25T08:00:00+08:00'));
  assert.strictEqual(g.roundsToday, 0);
  closeAll(win);
});

test('A9 · 配置存取：默认关闭、损坏配置钳回红线、enabled 只认真 true', () => {
  const win = bootDm({
    seed: { 'wb.douyin.config.v1': { enabled: true, roundsPerDay: 99, queryIntervalSec: 1, wordsPerRound: 500, roundGapMin: 0 } },
  });
  const DM = win.__DM;
  const { value, clamped } = DM.loadConfig();
  assert.strictEqual(value.enabled, true);
  assert.strictEqual(value.roundsPerDay, 6, '持久化的 99 必须钳回 6');
  assert.strictEqual(value.queryIntervalSec, 12, '持久化的 1 秒必须钳回 12');
  assert.strictEqual(value.wordsPerRound, 40, '持久化的 500 词必须钳回 40');
  assert.strictEqual(value.roundGapMin, 30, '持久化的 0 分钟必须钳回 30');
  assert.ok(clamped.length >= 4, '损坏配置的钳制事件要逐项记录');
  // enabled 伪造
  const w2 = bootDm({ seed: { 'wb.douyin.config.v1': { enabled: 'true' } } });
  assert.strictEqual(w2.__DM.loadConfig().value.enabled, false, '字符串 "true" 不算开启');
  closeAll(win); closeAll(w2);
});

test('A10 · 默认激活态：无配置时 activated() === false（附加条款④）', () => {
  const win = bootDm();
  assert.strictEqual(win.__DM.activated(), false);
  closeAll(win);
});

// ═══════════════ B. 渲染级集成 ═══════════════

test('B1 · 默认关闭：抖音页签只出占位（原文案），面板/控件/存储零残留（验收①）', async () => {
  const { win } = bootApp();
  try {
    const main = await openDouyinTab(win);
    assert.ok(main.querySelector('.mon-pane-placeholder'), '未激活时必须是占位块');
    assert.ok(main.textContent.includes('接口额度待接入'), '占位文案保留试跑口径');
    assert.ok(!main.querySelector('#dy-pane'), '不得渲染巡检面板');
    assert.strictEqual(main.querySelectorAll('[id^="dy-"]').length, 0, '不得出现任何 dy-* 控件');
    // 设置页也不因默认态写入任何抖音键
    await openView(win, 'settings');
    const keys = Object.keys(win.localStorage).filter((k) => k.startsWith('wb.douyin'));
    assert.deepStrictEqual(keys, [], '未激活时 localStorage 不得写入 wb.douyin.* 键');
  } finally { closeAll(win); }
});

test('B2 · 设置页激活闭环：② 情报采集卡内开关 → 开启即持久化（验收①/附加条款④）', async () => {
  const { win } = bootApp();
  try {
    let main = await openView(win, 'settings');
    const card = main.querySelector('.set-card[data-set="intel"]');
    assert.ok(card, '设置页 ② 情报采集卡必须在');
    const toggle = card.querySelector('#dy-toggle');
    assert.ok(toggle, '开关控件必须在 ② 情报采集卡内');
    assert.match(toggle.textContent, /开启/, '按钮文案须是开启动作');
    toggle.dispatchEvent(new win.MouseEvent('click', { bubbles: true }));
    await sleep(60);
    const saved = JSON.parse(win.localStorage.getItem('wb.douyin.config.v1'));
    assert.strictEqual(saved.enabled, true, '点击后配置持久化 enabled=true');
    // 再开设置页，开关显示已开启（可关闭）
    main = await openView(win, 'settings');
    const t2 = main.querySelector('#dy-toggle');
    assert.match(t2.textContent, /关闭/, '已开启态按钮应提供关闭动作');
  } finally { closeAll(win); }
});

test('B3 · 超红线配置被常量拦住（验收④）：轮次 20→6、间隔 3→12、词表 50 词截到 40', async () => {
  const { win } = bootApp();
  try {
    let main = await openView(win, 'settings');
    main.querySelector('#dy-toggle').dispatchEvent(new win.MouseEvent('click', { bubbles: true }));
    await sleep(50);
    main = await openView(win, 'settings');
    const rounds = main.querySelector('#dy-rounds');
    const interval = main.querySelector('#dy-interval');
    const words = main.querySelector('#dy-words');
    assert.ok(rounds && interval && words, '频次三控件必须在');
    rounds.value = '20';
    rounds.dispatchEvent(new win.Event('change', { bubbles: true }));
    interval.value = '3';
    interval.dispatchEvent(new win.Event('change', { bubbles: true }));
    words.value = Array.from({ length: 50 }, (_, i) => '词' + i).join('\n');
    words.dispatchEvent(new win.Event('change', { bubbles: true }));
    await sleep(80);
    const saved = JSON.parse(win.localStorage.getItem('wb.douyin.config.v1'));
    assert.strictEqual(saved.roundsPerDay, 6, '输入 20 轮必须被常量钳到 6');
    assert.strictEqual(saved.queryIntervalSec, 12, '输入 3 秒必须被常量钳到 12');
    assert.strictEqual(saved.words.length, 40, '50 个词必须截到红线上限 40');
    const status = (await openView(win, 'settings')).querySelector('#dy-status');
    assert.ok(status.textContent.length > 0, '钳制结果必须如实提示，不许静默');
  } finally { closeAll(win); }
});

test('B4 · 激活后面板：红线提示 / 词表 / 导入区 / 空列表（不预置演示条目）', async () => {
  const { win } = bootApp({ seed: { 'wb.douyin.config.v1': { enabled: true } } });
  try {
    const main = await openDouyinTab(win);
    const pane = main.querySelector('#dy-pane');
    assert.ok(pane, '激活后必须渲染巡检面板');
    assert.ok(main.textContent.includes('12'), '红线提示须含间隔常量');
    assert.match(main.textContent, /每日\s*4–6\s*轮|每日\s*4-6\s*轮/, '红线提示须含轮次口径');
    assert.match(main.textContent, /20–40|20-40/, '红线提示须含词数口径');
    assert.ok(main.querySelector('#dy-paste'), '粘贴导入区必须在');
    assert.ok(main.textContent.includes('暂无召回条目') || main.querySelectorAll('[data-dy-item]').length === 0, '空态不得预置演示条目');
    assert.ok(!main.textContent.includes('接口额度待接入'), '激活后不再是占位文案');
  } finally { closeAll(win); }
});

test('B5 · 导入→筛判→留痕→撤销→转待办 全链路（验收③）', async () => {
  const { win, invoked } = bootApp({ seed: { 'wb.douyin.config.v1': { enabled: true } } });
  try {
    let main = await openDouyinTab(win);
    // 导入 3 条
    main.querySelector('#dy-paste').value = '2026-11-24 电池炸机售后推诿\n2026-11-24 植保机云台故障\n2026-11-23 换新纠纷';
    main.querySelector('#dy-import').dispatchEvent(new win.MouseEvent('click', { bubbles: true }));
    await sleep(80);
    main = win.document.querySelector('#main');
    let rows = main.querySelectorAll('[data-dy-item]');
    assert.strictEqual(rows.length, 3, '导入后应出现 3 条召回');
    assert.strictEqual(main.querySelectorAll('.dy-badge--unsorted').length, 3, '初始全部未筛');
    // 筛判第一条 → 已筛 + 留痕
    rows[0].querySelector('[data-dy-act="screen"]').dispatchEvent(new win.MouseEvent('click', { bubbles: true }));
    await sleep(60);
    main = win.document.querySelector('#main');
    let first = main.querySelector('[data-dy-item]');
    assert.strictEqual(first.querySelectorAll('.dy-badge--screened').length, 1, '状态徽标变已筛');
    const saved1 = JSON.parse(win.localStorage.getItem('wb.douyin.items.v1'));
    assert.ok(saved1[0].screenedAt, '留痕字段 screenedAt 必须落存储');
    assert.ok(saved1[0].screenedBy, '留痕字段 screenedBy 必须落存储');
    // 忽略第二条
    main.querySelectorAll('[data-dy-item]')[1].querySelector('[data-dy-act="ignore"]').dispatchEvent(new win.MouseEvent('click', { bubbles: true }));
    await sleep(60);
    // 已筛条目转待办：invoke 必须收到 todo_create，标题带舆情标记
    main.querySelector('[data-dy-act="todo"]').dispatchEvent(new win.MouseEvent('click', { bubbles: true }));
    await sleep(80);
    const todoCall = invoked.find((c) => c.cmd === 'todo_create');
    assert.ok(todoCall, '转待办必须调用 todo_create');
    assert.ok(todoCall.payload.title.includes('舆情'), '待办标题须可识别为舆情来源');
    // 撤销：已筛回未筛且清痕迹
    main = win.document.querySelector('#main');
    main.querySelector('[data-dy-act="undo"]').dispatchEvent(new win.MouseEvent('click', { bubbles: true }));
    await sleep(60);
    const saved2 = JSON.parse(win.localStorage.getItem('wb.douyin.items.v1'));
    const undone = saved2.find((r) => r.screenedAt === null && r.state === win.__DM.ST.UNSORTED);
    assert.ok(undone, '撤销后回未筛且无判定痕迹');
  } finally { closeAll(win); }
});

test('B6 · 轮闸门在 UI 上拦人：今日满轮后导入按钮禁用并给原因', async () => {
  const now = new Date();
  const today = `${now.getFullYear()}-${String(now.getMonth() + 1).padStart(2, '0')}-${String(now.getDate()).padStart(2, '0')}`;
  const items = Array.from({ length: 6 }, (_, i) => ({
    id: 'g' + i, title: '条目' + i, source: '黑猫', dateKey: today, state: '未筛',
    recalledAt: `${today}T0${i}:00:00+08:00`, roundNo: i + 1,
  }));
  const { win } = bootApp({ seed: {
    'wb.douyin.config.v1': { enabled: true, roundsPerDay: 4 },
    'wb.douyin.items.v1': items,
  } });
  try {
    const main = await openDouyinTab(win);
    const btn = main.querySelector('#dy-import');
    assert.ok(btn.disabled, '今日已满轮必须禁用导入按钮');
    assert.match(main.textContent, /轮/, '禁用须给出轮次原因');
    assert.strictEqual(main.querySelectorAll('[data-dy-item]').length, 6, '6 条召回照常展示');
  } finally { closeAll(win); }
});

test('B7 · 月度汇总渲染：召回 / 已筛 / 未动 数字与存储一致', async () => {
  const now = new Date();
  const ym = `${now.getFullYear()}-${String(now.getMonth() + 1).padStart(2, '0')}`;
  const day = `${ym}-15`;
  const mk = (id, state) => ({ id, title: 't' + id, source: '黑猫', dateKey: day, state, recalledAt: `${day}T10:00:00+08:00`, roundNo: 1, screenedBy: state === '未筛' ? null : '我', screenedAt: state === '未筛' ? null : `${day}T11:00:00+08:00`, reason: state === '未筛' ? null : 'r' });
  const { win } = bootApp({ seed: {
    'wb.douyin.config.v1': { enabled: true },
    'wb.douyin.items.v1': [mk(1, '未筛'), mk(2, '已筛'), mk(3, '已筛'), mk(4, '已忽略'), mk(5, '未筛')],
  } });
  try {
    const main = await openDouyinTab(win);
    const sum = main.querySelector('#dy-month');
    assert.ok(sum, '月度汇总区必须在');
    assert.match(sum.textContent, /5/, '召回总量 5');
    assert.match(sum.textContent, /2/, '未筛 2 / 已筛 2');
    assert.ok(main.textContent.includes('属地') || main.textContent.includes('人工'), '须提示属地筛判靠人工（免费通道无作者属地能力）');
  } finally { closeAll(win); }
});
