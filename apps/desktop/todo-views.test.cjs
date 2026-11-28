/* W5 待办版块守卫：逐条对齐设计规格 §3.2 / §2.3 / §5.4 / §6.1。
 * 判据只认可执行代码（注释里的规格说明不算命中）。 */
"use strict";
const { test } = require("node:test");
const assert = require("node:assert/strict");
const fs = require("node:fs");
const path = require("node:path");

const read = (f) => fs.readFileSync(path.join(__dirname, f), "utf8");
const TV = "src/todo-views.js";
const APP = "src/app.js";
const HTML = "src/index.html";
const CSS = "src/tokens.css";
const RS = "../../crates/wb-core/src/intel_store.rs";
const LIB = "src-tauri/src/lib.rs";

test("W5 空态逐字（§3.2 实测文案）：标题、说明、两按钮，无示例待办", () => {
  const v = read(TV);
  assert.ok(v.includes("待办池是空的"), "空态标题被改动");
  assert.ok(v.includes("待办不会凭空出现：给任务填下次计划日期、把跟踪推进到停滞、或从舆情与网点转一条待办，这里就会实时长出来。"), "空态正文被改动");
  assert.ok(v.includes('id="td-empty-create">手动建档'), "缺「手动建档」按钮");
  assert.ok(v.includes('id="td-empty-goto">去建任务'), "缺「去建任务」按钮");
  const noComments = v.replace(/\/\*[\s\S]*?\*\//g, "").replace(/^\s*\/\/.*$/gm, "");
  assert.ok(!/示例待办|demoTodo/.test(noComments), "空态出现编造的示例数据");
});

test("W5 错误不降级（§6.1 铁律③）：读取失败 ≠ 待办池为空", () => {
  const v = read(TV);
  assert.ok(v.includes("读取待办失败"), "错误卡文案缺失");
  assert.ok(v.includes('id="td-retry">重试'), "缺重试按钮");
  assert.ok(v.includes("查看诊断"), "缺诊断按钮");
  // error 分支先于空态分支：错误时绝不渲染空态文案
  const errIdx = v.indexOf("读取待办失败");
  const emptyIdx = v.indexOf("待办池是空的");
  const viewIdx = v.indexOf("TV.view = function");
  assert.ok(errIdx < viewIdx && emptyIdx < viewIdx, "三态分支须在 view 入口之前定义");
  assert.ok(/if \(st\.error\)[\s\S]*?if \(!todos\.length\)/.test(v), "error 分支必须先于空态分支");
});

test("W5 加载态（§3.2）：3 条骨架行、行高与真实一致（64px）", () => {
  const v = read(TV);
  assert.ok(/'<li class="td-row td-skel"><\/li>'\.repeat\(3\)/.test(v), "加载骨架必须 3 条");
  const c = read(CSS);
  assert.ok(c.includes(".td-skel") && /min-height: 64px/.test(c), "骨架行高须与真实行一致");
});

test("W5 数据缺失态（§3.2）：原项目已不存在 → 置灰显示不隐藏、仍可闭环", () => {
  const v = read(TV);
  assert.ok(v.includes("原项目已不存在"), "缺数据缺失文案");
  assert.ok(v.includes("td-dead"), "缺置灰样式类");
  // 行渲染不因 projectAlive=false 跳过：dead 分支仍输出行内容
  assert.ok(/if \(t\.projectId && !t\.projectAlive\)/.test(v), "缺 projectAlive 判定分支");
});

test("W5 来源徽标口径（§3.2 三类）：ext.v34_source 真实值才显示，人工新建不显示", () => {
  const v = read(TV);
  assert.ok(/SRC_ZH = \{ intel: "跟踪导出", douyin: "舆情", dealer: "网点" \}/.test(v), "徽标三类缺失或被改");
  assert.ok(/t\.ext && t\.ext\.v34_source \? SRC_ZH/.test(v), "徽标必须认 ext.v34_source 真实值");
});

test("W5 穿透接线（§2.3）：待办行项目名 → __PV.openProjectById；app.js 三处挂载", () => {
  const v = read(TV);
  assert.ok(v.includes("window.__PV.openProjectById"), "缺穿透调用");
  const a = read(APP);
  assert.ok(/todo: viewTodo/.test(a), "VIEWS map 缺 todo");
  assert.ok(/window\.__TV\?\.sync\(state\.view\)/.test(a), "render 缺 TV.sync");
  assert.ok(/state\.view === "todo"[\s\S]*?__TV\.bind\(\)/.test(a), "bindView 缺 todo 分支");
  const h = read(HTML);
  assert.ok(/data-view="todo"/.test(h), "nav 缺待办入口");
  assert.ok(/<script src="todo-views\.js">/.test(h), "缺 script 引入");
});

test("W5 §5.4 两处建档入口：共用 __PV.openCreateModal 同一弹层 + afterCreate 生成待办", () => {
  const v = read(TV);
  assert.ok(v.includes("openCreateModal"), "建档必须复用 §5.4 同一弹层（不出现第二个建档窗）");
  assert.ok(/afterCreate[\s\S]*?todo_create/.test(v), "建档成功后必须生成指向该项目的待办");
  const pv = read("src/project-views.js");
  assert.ok(pv.includes("openCreateModal = function"), "PV 缺 openCreateModal 导出");
  assert.ok(pv.includes("afterCreate(projectId, createdName)"), "PV 缺 afterCreate 钩子回调");
});

test("W5 闭环口径：update_item status=已闭环（from_zh 认 Done，非缩写）", () => {
  const v = read(TV);
  assert.ok(/status: "已闭环"/.test(v), "闭环必须传中文「已闭环」");
  const m = fs.readFileSync(path.join(__dirname, "../../crates/wb-core/src/model.rs"), "utf8");
  assert.ok(/"已完成" \| "已闭环" => Some\(ItemStatus::Done\)/.test(m), "「已闭环」须被 from_zh 认为 Done");
});

test("W5 generate_todo → work_item 落库（arch §4.1）：INSERT OR IGNORE + 幂等键", () => {
  const r = read(RS);
  assert.ok(r.includes("INSERT OR IGNORE INTO work_item"), "缺幂等落库");
  assert.ok(r.includes("insert_todo_work_item"), "缺落库函数");
  assert.ok(/insert_todo_work_item\(conn, todo, &ts\)/.test(r), "generate_todo 成功路径未接落库");
});

test("W5 状态值口径：入库 status='待启动'（from_zh 认的全量中文，非「未启」缩写）", () => {
  const r = read(RS);
  assert.ok(/0, '待启动', NULL, \?5, \?6,/.test(r), "落库 status 必须是「待启动」");
  assert.ok(!/0, '未启',/.test(r), "「未启」是前端 chip 缩写，入库非法");
  const l = read(LIB);
  assert.ok(/0, '待启动', NULL, \?5, \?6,/.test(l), "todo_create 入库 status 必须是「待启动」");
});

test("W5 历史回填（arch §4.1）：按 todo_json 重放、幂等可重跑、不改写存量行", () => {
  const r = read(RS);
  assert.ok(r.includes("pub fn backfill_todos"), "缺回填函数");
  assert.ok(/state = 'todoed' AND todo_id IS NOT NULL AND todo_json IS NOT NULL/.test(r), "回填范围口径不对");
  assert.ok(r.includes("invalid_json"), "脏 todo_json 必须如实计数不搬运");
  const l = read(LIB);
  assert.ok(/todo_backfill/.test(l), "lib.rs 缺回填命令注册");
  assert.ok(/todo_list,/.test(l) && /todo_create,/.test(l), "lib.rs 缺 todo_list/todo_create 注册");
});

test("W5 筛选四档（§3.2）：全部/待处理/今天到期/已闭环", () => {
  const v = read(TV);
  assert.ok(/\[\["all", "全部"\], \["open", "待处理"\], \["today", "今天到期"\], \["closed", "已闭环"\]\]/.test(v), "筛选四档缺失或被改");
});

test("W5 不得混入成员管理（§3.2）", () => {
  const noComments = read(TV).replace(/\/\*[\s\S]*?\*\//g, "").replace(/^\s*\/\/.*$/gm, "");
  assert.ok(!/member_|成员名单|邀请/.test(noComments), "待办版块不得混入成员管理入口");
});

test("W5 样式闭合：JS 里的 td-* 类在 tokens.css 都有对应选择器", () => {
  const v = read(TV);
  const c = read(CSS);
  const stripped = v.replace(/id="td-[a-z0-9-]+"/g, "").replace(/id="pv-[a-z0-9--]*"/g, "").replace(/#td-[a-z0-9-]+/g, "");
  const used = new Set(stripped.match(/td-[a-z0-9-]+/g) || []);
  // 纯 JS 标记类（无视觉语义）：td-mask=拆层定位标记、td-go-diag=事件委托钩子
  const STYLELESS = new Set(["td-mask", "td-go-diag"]);
  const missing = [...used].filter((x) => !STYLELESS.has(x) && !c.includes(`.${x}`));
  assert.deepEqual(missing, [], `CSS 缺选择器：${missing.join(", ")}`);
});
