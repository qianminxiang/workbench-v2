/**
 * W8 退役清理守卫（arch §2.2 / design §2.1）
 * 红线：只清入口不清数据 —— 下掉晨报/看板/明细/时间线/KPI/单位/成员七个旧入口与
 * 6 板块筛选条；视图函数、VIEWS 映射、命令与数据表一律保留；晨报提醒必须以
 * level:'high' 入字幕队列（免打扰照滚，可达性命门），深链一律改任务版块。
 */
const test = require("node:test");
const assert = require("node:assert");
const fs = require("node:fs");
const path = require("node:path");

const DIR = __dirname;
const html = fs.readFileSync(path.join(DIR, "src/index.html"), "utf8");
const app = fs.readFileSync(path.join(DIR, "src/app.js"), "utf8");

const navBlock = html.slice(html.indexOf('<nav class="nav" id="nav"'), html.indexOf("</nav>"));
const RETIRED = ["brief", "kanban", "detail", "timeline", "kpi", "units", "members"];

test("W8·入口下掉：七个退役入口不得出现在导航", () => {
  for (const v of RETIRED) {
    assert.ok(!navBlock.includes(`data-view="${v}"`), `退役入口 ${v} 不得复活`);
  }
  const names = [...navBlock.matchAll(/data-view="([a-z]+)"/g)].map((m) => m[1]);
  // W6（单B）：「网点」升级为「监控」页签容器（design §3.4），退役后第 4 项为 monitor
  assert.deepEqual(names, ["project", "todo", "intel", "monitor", "add", "settings"],
    "退役后导航应为 6 项且顺序固定（W6 网点→监控）");
});

test("W8·6 板块筛选条下掉：宿主与模块级绑定必须同删（防 null 崩）", () => {
  // index.html：筛选条宿主（板块 chips / filter-count / filter-clear）不得存在
  assert.ok(!html.includes('data-filter="board"'), "板块 chips 不得残留");
  assert.ok(!html.includes('id="filter-count"'), "filter-count 宿主已删");
  assert.ok(!html.includes('id="filter-clear"'), "filter-clear 宿主已删");
  // app.js：指向已删宿主的模块级绑定不得残留（否则脚本加载即 null 崩）
  assert.ok(!app.includes('$("#filter-clear").addEventListener'), "filter-clear 绑定必须随宿主同删");
  assert.ok(!/document\.querySelectorAll\('\.chip\[data-filter="board"\]'\)/.test(app),
    "板块 chips 的 querySelectorAll 不得残留");
  // updateFilterCount 必须有 null 防御（退役视图深链渲染时计数无宿主）
  const fn = app.slice(app.indexOf("function updateFilterCount"), app.indexOf("}", app.indexOf("function updateFilterCount")));
  assert.ok(/if \(el\)/.test(fn), "updateFilterCount 必须对 #filter-count 缺失做防御");
});

test("W8·默认视图：晨报下掉后落到任务版块（第一入口）", () => {
  assert.ok(/view: "project",\s*\/\/ W8/.test(app), "state.view 默认值必须是 project（W8 注释锚点）");
  assert.ok(!app.includes('view: "brief"'), "默认视图不得再指向晨报");
  // render 兜底也必须换掉 viewBrief（晨报退役后不做兜底视图）
  assert.ok(!app.includes("|| viewBrief)()"), "render 兜底不得再是晨报");
  assert.ok(app.includes("|| viewProject)()"), "render 兜底应为任务版块");
  // aria-current 初始态应落在第一入口按钮上
  assert.ok(/data-view="project" aria-current="true"/.test(html), "初始选中态应在任务按钮");
});

test("W8·晨报深链：gotoView(\"brief\") 全量清零，四处深链改任务版块", () => {
  assert.equal((app.match(/gotoView\("brief"\)/g) || []).length, 0,
    "晨报视图入口已退役，不得再 gotoView 到 brief");
  // 启动补推 / brief-pending / brief-delivered / brief-open 四处深链
  const deepLinks = app.match(/gotoView\("project"\)/g) || [];
  assert.ok(deepLinks.length >= 4, "晨报四处深链必须改指任务版块");
  assert.ok(/await listen\("brief-open", \(\) => gotoView\("project"\)\);/.test(app),
    "brief-open（托盘「查看晨报」）必须深链到任务版块（arch §2.2）");
  assert.ok(!app.includes('action: { label: "查看晨报"'), "动作按钮文案不得再指向已退役的晨报页");
});

test("W8·提醒可达性命门：晨报类通知必须保持 level:'high'（免打扰照滚）", () => {
  // 启动补推 + brief-pending + brief-delivered 三处入队，level 必须是 high
  const spots = [
    app.slice(app.indexOf('key: `brief-pending-${pending.pending_date'), app.indexOf("}", app.indexOf('key: `brief-pending-${pending.pending_date') + 400)),
  ];
  const pendingSpots = [...app.matchAll(/key: `brief-pending-\$\{[^}]+\}`[\s\S]{0,220}?level: "high"/g)];
  assert.equal(pendingSpots.length, 2, "brief-pending 两处入队（启动补推 + 事件）都必须 level:'high'");
  const delivered = app.match(/key: `brief-delivered-\$\{[^}]+\}`[\s\S]{0,220}?level: "high"/);
  assert.ok(delivered, "brief-delivered 入队必须 level:'high'");
  void spots;
  // marqueeSchedule 的免打扰抑制口径不得动（level !== 'high' 跳过、队列保留）
  assert.ok(app.includes("notice.queue[i].level !== \"high\""), "免打扰抑制口径不得改动");
});

test("W8·只清入口不清数据：退役视图函数与 VIEWS 映射必须保留", () => {
  for (const v of ["viewBrief", "viewKanban", "viewDetail", "viewTimeline", "viewKpi", "viewUnits", "viewMembers"]) {
    assert.ok(app.includes(`function ${v}(`), `${v} 必须保留（数据呈现能力不删）`);
  }
  const vm = app.slice(app.indexOf("const VIEWS = {"), app.indexOf("};", app.indexOf("const VIEWS = {")));
  for (const k of RETIRED) {
    assert.ok(vm.includes(`${k}: view`), `VIEWS 映射必须保留 ${k}（只清入口）`);
  }
  // 晨报三命令（get_pending_brief 等被 UI 调用的部分）保留：启动补推仍读 get_pending_brief
  assert.ok(app.includes('invoke("get_pending_brief")'), "get_pending_brief 调用必须保留（提醒仍可达）");
  // 退役视图数据装载保留（reload 仍拉 kpi_overview / units / 成员）
  assert.ok(app.includes('invoke("kpi_overview"'), "kpi_overview 数据装载保留（命令不删）");
  assert.ok(app.includes('invoke("units"'), "units 数据装载保留（命令不删）");
});

test("W8·不触碰设置页与并行单 B 分工面", () => {
  // 设置入口保留（B 的 W7 设置页六节重组在并行轨道，本 PR 不得动设置页）
  assert.ok(navBlock.includes('data-view="settings"'), "设置入口必须保留");
  assert.ok(app.includes("function viewSettings("), "viewSettings 必须原样保留");
  // 快速添加入口保留（板块下拉去留属 add_item 语义议题，W8 不越界）
  assert.ok(navBlock.includes('data-view="add"'), "添加入口必须保留");
  assert.ok(app.includes('id="add-board"'), "快速添加的板块下拉保留（board_code NOT NULL 硬约束）");
});
