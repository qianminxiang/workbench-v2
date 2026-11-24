// W7 设置版块重组 + 成员迁入 · 渲染级断言（不是静态 grep）
//
// 判据来源：《设计基线落地规范》§4.1 布局层级 / §4.2 六节功能项逐项映射表 /
// §4.3 账号管理保护规则成稿。
//
// 为什么用 jsdom 真渲染而不是读源码字符串：
//   「控件 ID 还在」和「控件真的渲染出来且绑上了」是两件事。字符串里有
//   `id="imp-go"` 不代表渲染后的 DOM 里有，也不代表 bindView 跑完监听真的挂上。
//   本测试载入真实 index.html + intel-projects.js + app.js，让 bindView 真跑一遍。
//
// 注意：app.js 的 bindMarquee() 里有一个 30s 的 setInterval（免打扰边界恢复心跳），
// 它会一直挂住 node 事件循环 ⇒ 每个用例结束必须 win.close() 清定时器，
// 否则 `node --test` 永远不返回（本轮实测踩过：600s 超时被杀）。
const test = require('node:test');
const assert = require('node:assert');
const fs = require('node:fs');
const path = require('node:path');
const { JSDOM } = require('/home/gem/.aily/.cli/npm/node_modules/jsdom');

const SRC = path.join(__dirname, 'src');

// §4.2 映射表 + 架构约束 A 列的 22 个控件 ID（一个不少、一个不改名）。
// **⑤ 服务器同步是双态互斥的**（规范 §4.2：⑤-1 未登录态表单 / ⑤-2 已登录态状态区）：
//   - 未登录态渲染 sync-url / sync-user / sync-pass / sync-setup / sync-login
//   - 已登录态渲染 sync-logout / sync-now
// 所以「22 个一个不少」必须两态各跑一遍合起来算，不能单态断言 22 个都在——
// 单态断言会把正常的互斥当缺陷（本轮实测踩过这条）。
const IDS_COMMON = [
  'imp-pick', 'imp-path', 'imp-go', 'bk-go',                       // ① 数据
  'sub-pick', 'sub-filter', 'sub-go', 'sub-status',                // ③ 补贴导出
  'upd-go', 'upd-status',                                          // ⑥ 更新
  'quiet-start', 'quiet-end', 'quiet-saved',                       // ④ 提醒送达
  'sync-box', 'sync-msg',                                          // ⑤ 同步（两态共有的容器）
];
const IDS_LOGGED_OUT = ['sync-url', 'sync-user', 'sync-pass', 'sync-setup', 'sync-login']; // ⑤-1
const IDS_LOGGED_IN = ['sync-logout', 'sync-now'];                                          // ⑤-2
// ② 情报采集节的采集按钮（源站状态小窗由 viewIntelSites 渲染）
const INTEL_ID = 'intel-refresh';

/** 建一个能跑 app.js 的环境：**真实 index.html 全文**，注入 TAURI 桩。
 *  app.js 在顶层就绑定 #filter-clear / 导航 / 字幕条等常驻节点，缺一个就在
 *  eval 阶段抛 null —— 用真实骨架才不会自欺。 */
function bootApp({ members, me, intel, prefs, syncStatus } = {}) {
  const rawHtml = fs.readFileSync(path.join(SRC, 'index.html'), 'utf8');
  const html = rawHtml.replace(/<script[^>]*><\/script>/g, '');
  const dom = new JSDOM(html, { runScripts: 'outside-only', pretendToBeVisual: true });
  const win = dom.window;

  const invoked = [];
  win.__TAURI__ = {
    core: {
      invoke: async (cmd, payload) => {
        invoked.push({ cmd, payload });
        if (cmd === 'get_prefs') return prefs || { quiet_start: '22:00', quiet_end: '08:00' };
        if (cmd === 'sync_status') {
          return syncStatus || { logged_in: false, state: '未登录', base_url: '', pending: 0, cursor: 0 };
        }
        if (cmd === 'member_list') return { members: members || [], me: me || null };
        if (cmd === 'fiscal_year') return 2027;
        if (cmd === 'list_items') return [];
        if (cmd === 'alerts') return { brief: null };
        if (cmd === 'kpi_overview') return [];
        if (cmd === 'units') return [];
        if (cmd === 'intel_overview') return intel || null;
        return {};
      },
    },
    dialog: { open: async () => null },
    event: { listen: async () => () => {} },
  };
  win.__TAURI__.window = { getCurrent: () => ({ label: 'main' }) };

  for (const f of ['intel-projects.js', 'app.js']) {
    win.eval(fs.readFileSync(path.join(SRC, f), 'utf8'));
  }
  return { win, invoked };
}

/** 打开设置页并等异步回填完成；返回 { win, main }，用完必须 close。 */
async function openSettings(opts) {
  const { win, invoked } = bootApp(opts);
  await sleep(150); // 等 boot 期 get_prefs / sync_status / member_list 回填
  const btn = win.document.querySelector('#nav button[data-view="settings"]');
  assert.ok(btn, '导航缺少「设置」入口');
  btn.dispatchEvent(new win.MouseEvent('click', { bubbles: true }));
  await sleep(80);
  return { win, invoked, main: win.document.querySelector('#main') };
}
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const closeAll = (w) => { try { w.close(); } catch (_) {} };

test('W7 · 设置页渲染出 22 个控件 ID（未登录态 + 已登录态各跑一遍合起来算）', async () => {
  const members = [{ member_id: 'm1', display_name: '张三', role: 'admin', status: 'active' }];
  const me = { known: true, role: 'admin', member_name: '张三' };
  const seen = new Set();

  // 未登录态（⑤-1）：应出 sync-url / sync-user / sync-pass / sync-setup / sync-login
  {
    const { win, main } = await openSettings({ members, me });
    try {
      for (const id of [...IDS_COMMON, ...IDS_LOGGED_OUT, INTEL_ID]) {
        assert.ok(main.querySelector(`#${id}`), `未登录态设置页缺少 #${id}`);
        seen.add(id);
      }
      // 反证：未登录态不该出现已登录态的按钮
      assert.ok(!main.querySelector('#sync-logout'), '未登录态不应出现 #sync-logout');
    } finally { closeAll(win); }
  }
  // 已登录态（⑤-2）：应出 sync-logout / sync-now
  {
    const { win, main } = await openSettings({
      members, me,
      syncStatus: {
        logged_in: true, state: '已连接', base_url: 'https://ahxbd.work',
        member_name: '张三', pending: 3, cursor: 12, last_ok_at: '2026-11-24T09:00:00',
      },
    });
    try {
      for (const id of [...IDS_COMMON, ...IDS_LOGGED_IN]) {
        assert.ok(main.querySelector(`#${id}`), `已登录态设置页缺少 #${id}`);
        seen.add(id);
      }
      assert.ok(!main.querySelector('#sync-login'), '已登录态不应出现 #sync-login');
    } finally { closeAll(win); }
  }
  // 合并两态：22 个映射表 ID 必须全部出现过（一个不少、一个不改名）
  const expected22 = [...IDS_COMMON, ...IDS_LOGGED_OUT, ...IDS_LOGGED_IN];
  assert.strictEqual(expected22.length, 22, `映射表 ID 应为 22 个，实际 ${expected22.length}`);
  for (const id of expected22) {
    assert.ok(seen.has(id), `映射表 ID #${id} 在两态里都没出现（被改名或删掉了）`);
  }
});

test('W7 · 六节顺序 ①→⑥ + 账号管理在最后（§4.1 顺序固定）', async () => {
  const { win, main } = await openSettings({
    members: [{ member_id: 'm1', display_name: '张三', role: 'admin', status: 'active' }],
    me: { known: true, role: 'admin', member_name: '张三' },
  });
  try {
    const order = Array.from(main.querySelectorAll('[data-set]')).map((el) => el.dataset.set);
    assert.deepStrictEqual(
      order,
      ['data', 'intel', 'subsidy', 'quiet', 'sync', 'update', 'members'],
      '六节顺序必须是 ①数据 → ②情报 → ③补贴 → ④提醒 → ⑤同步 → ⑥更新 → 账号管理'
    );
  } finally { closeAll(win); }
});

test('W7 · 设置页为两列网格，⑤⑥ 与账号管理独占整行', async () => {
  const { win, main } = await openSettings({
    members: [], me: { known: true, role: 'admin', member_name: '我自己' },
  });
  try {
    assert.ok(main.querySelector('.settings-grid'), '设置页根容器必须是 .settings-grid（§4.1 两列网格）');
    assert.ok(main.querySelector('.set-card[data-set="sync"].set-card--wide'), '⑤ 服务器同步须独占整行');
    assert.ok(main.querySelector('.set-card[data-set="update"].set-card--wide'), '⑥ 更新须独占整行');
    assert.ok(main.querySelector('.settings-migrated[data-set="members"]'), '账号管理须为迁入区（分隔线隔开）');
  } finally { closeAll(win); }
});

test('W7 · 唯一启用中管理员：危险按钮真实 disabled + 常驻文案 + tooltip（三通道）', async () => {
  const members = [
    { member_id: 'm1', display_name: '张三', role: 'admin', status: 'active' }, // 唯一启用中管理员
    { member_id: 'm2', display_name: '李四', role: 'member', status: 'active' },
  ];
  const { win } = await openSettings({ members, me: { known: true, role: 'admin', member_name: '张三' } });
  try {
    const row = win.document.querySelector('[data-member-for="m1"]');
    assert.ok(row, '管理员行未渲染');
    const roleBtn = row.querySelector('[data-role-set]');
    const stBtn = row.querySelector('[data-status-set]');
    assert.ok(roleBtn.disabled, '唯一管理员「降为成员」按钮必须 disabled（真拦截，不是只变灰）');
    assert.ok(stBtn.disabled, '唯一管理员「停用」按钮必须 disabled');
    const warn = row.querySelector('.set-warn-inline');
    assert.ok(warn && warn.textContent.includes('唯一启用中的管理员'), '须有常驻 13px 警告文案');
    assert.strictEqual(roleBtn.getAttribute('title'), '唯一启用中的管理员，需先新增另一名管理员');
    assert.strictEqual(stBtn.getAttribute('title'), '唯一启用中的管理员，需先新增另一名管理员');
    assert.ok(roleBtn.classList.contains('btn--locked'), '禁用按钮须带 .btn--locked 可视态');
    const row2 = win.document.querySelector('[data-member-for="m2"]');
    assert.ok(!row2.querySelector('[data-role-set]').disabled, '普通成员行的角色按钮不应被锁');
  } finally { closeAll(win); }
});

test('W7 · 两名启用中管理员时按钮解锁（保护规则只锁最后一名）', async () => {
  const members = [
    { member_id: 'm1', display_name: '张三', role: 'admin', status: 'active' },
    { member_id: 'm2', display_name: '李四', role: 'admin', status: 'active' },
  ];
  const { win } = await openSettings({ members, me: { known: true, role: 'admin', member_name: '张三' } });
  try {
    const row = win.document.querySelector('[data-member-for="m1"]');
    assert.ok(!row.querySelector('[data-role-set]').disabled, '有第二名管理员时不应锁');
    assert.ok(!row.querySelector('[data-status-set]').disabled, '有第二名管理员时不应锁');
  } finally { closeAll(win); }
});

test('W7 · 非管理员身份只能查看：不出操作按钮，只出「需管理员」', async () => {
  const members = [{ member_id: 'm1', display_name: '张三', role: 'admin', status: 'active' }];
  const { win, main } = await openSettings({ members, me: { known: true, role: 'member', member_name: '王五' } });
  try {
    assert.ok(!main.querySelector('[data-role-set]'), '非管理员不应看到改角色按钮');
    assert.ok(!main.querySelector('#member-add'), '非管理员不应看到添加成员入口');
    assert.ok(main.textContent.includes('只能查看'), '非管理员须看到只读提示');
  } finally { closeAll(win); }
});
