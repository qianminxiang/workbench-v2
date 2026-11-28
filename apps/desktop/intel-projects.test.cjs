/* 情报「项目进程」（同一项目阶段聚合）界面层守卫（node --test，零依赖）。
 *
 * 为什么需要它：本视图的绝大部分约束是**规格文字**而不是可运行断言——六段口径、
 * 「终止是侧状态不推进进程」、空段不画成 0、Calm Slate 令牌与「全页零 box-shadow」、
 * 暗色档必须全量覆盖、手动项目计数口径。这些一旦被后来人「顺手简化」，
 * 界面照样能跑，只是悄悄偏离规格（这类回归在评审里最难看出来）。
 * 所以这里把规格 v1.2 的关键条款钉成静态断言。
 *
 * 只扫源码文本，不启动浏览器、不依赖 @tauri。判据来源全部来自设计规格 v1.2
 *（飞书文件 W238b0cSaowLWJxaANBcyV95nze）§2 / §3 / §4 / §5 / §6。
 */
"use strict";

const test = require("node:test");
const assert = require("node:assert");
const fs = require("node:fs");
const path = require("node:path");

const SRC = path.join(__dirname, "src");
const MOD = path.join(SRC, "intel-projects.js");
const APP = path.join(SRC, "app.js");
const TOKENS = path.join(SRC, "tokens.css");
const HTML = path.join(SRC, "index.html");

const read = (p) => {
  assert.ok(fs.existsSync(p), `找不到文件 ${p}`);
  return fs.readFileSync(p, "utf8");
};
const mod = read(MOD);
const app = read(APP);
const tokens = read(TOKENS);
const html = read(HTML);

// ── 对比度工具（与 contrast-tokens.test.cjs 同公式，独立实现，避免共用一份实现
//    导致「两个法官其实是一个人」）─────────────────────────────────────────
function lin(c) { const v = c / 255; return v <= 0.04045 ? v / 12.92 : Math.pow((v + 0.055) / 1.055, 2.4); }
function lum(hex) {
  const h = hex.replace("#", "");
  const r = parseInt(h.slice(0, 2), 16), g = parseInt(h.slice(2, 4), 16), b = parseInt(h.slice(4, 6), 16);
  return 0.2126 * lin(r) + 0.7152 * lin(g) + 0.0722 * lin(b);
}
function ratio(a, b) {
  const la = lum(a), lb = lum(b);
  const hi = Math.max(la, lb), lo = Math.min(la, lb);
  return (hi + 0.05) / (lo + 0.05);
}

// 只在 `.ip-scope {` 那个块里取浅色档（避免 dark 块的同名 token 覆盖）
function ipLightTokens() {
  const start = tokens.indexOf(".ip-scope {");
  assert.ok(start > 0, "tokens.css 里找不到 .ip-scope 浅色令牌块");
  const end = tokens.indexOf("}", start);
  assert.ok(end > start, ".ip-scope 块没有闭合");
  const block = tokens.slice(start, end);
  const out = {};
  for (const m of block.matchAll(/(--ip-[a-z0-9-]+)\s*:\s*(#[0-9A-Fa-f]{6})/g)) out[m[1]] = m[2].toUpperCase();
  return out;
}

test("规格 §2：六段口径与终止侧状态", () => {
  // 六段定义必须在 STAGES 定义块里逐段写全（顺序即进程顺序）。
  // 注意：必须在 STAGES 数组块内取位置，不能在整个文件里 indexOf——
  // 文件头注释里也写着「采购公告」等词，会抢在定义之前命中，导致顺序断言假红。
  const blockStart = mod.indexOf("IP.STAGES = [");
  assert.ok(blockStart > 0, "找不到 IP.STAGES 定义块");
  const blockEnd = mod.indexOf("];", blockStart);
  assert.ok(blockEnd > blockStart, "IP.STAGES 块没有闭合");
  const stagesBlock = mod.slice(blockStart, blockEnd);
  const LABELS = ["意向 / 需求", "采购公告", "更正 / 澄清", "中标成交", "合同 / 验收", "终止 / 废标"];
  const idx = LABELS.map((l) => stagesBlock.indexOf(l));
  assert.ok(idx.every((i) => i >= 0), `六段标签必须齐全，缺：${LABELS.filter((_, k) => idx[k] < 0).join("、")}`);
  // 顺序：后一段必须出现在前一段之后（防止有人按字母序重排或调错序）
  for (let i = 1; i < idx.length; i++) {
    assert.ok(idx[i] > idx[i - 1], `六段顺序错乱：${LABELS[i]} 应排在 ${LABELS[i - 1]} 之后`);
  }
  // 终止是侧状态：order 必须为 -1（不参与推进），且标 side
  assert.match(mod, /\{\s*key:\s*"terminate"[^}]*order:\s*-1[^}]*side:\s*true\s*\}/,
    "终止/废标必须是 side:true 且 order:-1 的侧状态——它是旁路，不占进程位、不推进当前阶段");
  // 当前阶段只取「非侧段」里最靠后的有公告段
  assert.match(mod, /if \(s\.def\.side\) continue;/, "计算当前阶段时必须跳过侧段");
});

test("规格 §2 / §5：空段跳过，不画成 0 条", () => {
  assert.match(mod, /data-st="skip"|data-st="\$\{st\}"|"skip"/, "点条/轴必须存在「跳过」态");
  assert.ok(mod.includes("此阶段无公告"), "跳过态必须有文字说明（不能只靠斜线图形）");
  // 轴里空段不能渲染「0 条」
  assert.ok(!/ip-seg-count">0 条/.test(mod), "空段不得渲染成「0 条」（应显式写「此阶段无公告」）");
  // 明确禁止在跳过的段上打印 0
  assert.ok(!/\.length \? .* : "0 条"/.test(mod), "空段不能退化成字面「0 条」");
});

test("规格 §3：Calm Slate 令牌齐全、零 box-shadow、暗色全量覆盖", () => {
  const T = ipLightTokens();
  const REQUIRED = [
    "--ip-canvas", "--ip-canvas-subtle", "--ip-card", "--ip-card-muted", "--ip-inner",
    "--ip-divider", "--ip-border-card", "--ip-border-clickable", "--ip-border-interactive",
    "--ip-text-primary", "--ip-text-secondary", "--ip-text-muted",
    "--ip-icon-brand-on-canvas", "--ip-primary-action", "--ip-primary-action-hover",
    "--ip-primary-action-active", "--ip-primary-200", "--ip-primary-800", "--ip-primary-900",
    "--ip-link", "--ip-success", "--ip-warning", "--ip-error", "--ip-aux",
  ];
  const missing = REQUIRED.filter((k) => !T[k]);
  assert.deepStrictEqual(missing, [], `规格 §3 令牌缺失：${missing.join("、")}`);

  // 四条硬结论之一的色值逐项钉死（改色即红）
  assert.strictEqual(T["--ip-primary-action"], "#2174E1", "主按钮必须是 #2174E1（白字 4.52:1）");
  assert.strictEqual(T["--ip-icon-brand-on-canvas"], "#3787F0", "浅画布主色图标必须是 #3787F0（3.30:1），不能换成 #4992F1");
  assert.strictEqual(T["--ip-canvas"], "#F4F6FA", "画布必须是 Calm Slate #F4F6FA");

  // 全页零 box-shadow：本作用域内所有规则不得出现 box-shadow（规格 §3 硬结论 3）
  const start = tokens.indexOf("/* ══════════════════════════════════════════════════════════════════════════\n   v3.2.0 · 情报「项目进程」");
  assert.ok(start > 0, "找不到本视图的令牌/组件块起点注释");
  // v3.4.0 起 tokens.css 末尾追加了「任务版块 pv-*」（W3）块——那是另一套设计语言
  // （磨砂浮层带投影档④，v3.4 规格 §5.4），不属于本视图作用域；零投影结论只约束
  // 到 pv 块起点为止，不能扫到文件尾把别的作用域的投影算成本视图回归。
  const pvStart = tokens.indexOf("任务版块 pv-*（W3");
  const ipBlock = tokens.slice(start, pvStart > start ? pvStart : undefined);
  const shadows = [...ipBlock.matchAll(/box-shadow\s*:\s*([^;]+);/g)].map((m) => m[1].trim());
  // 唯一允许的是显式 `none`（用来覆盖父级可能带来的阴影）
  const bad = shadows.filter((v) => v !== "none");
  assert.deepStrictEqual(bad, [], `本视图作用域内出现了真实投影：${bad.join(" | ")}（规格要求二维立体只靠 1px 描边）`);

  // 暗色档必须覆盖同一套语义（数量对得上才不会「漏几个 token 变浅字浅底」）
  const darkStart = tokens.indexOf('.ip-scope[data-ip-theme="dark"]');
  assert.ok(darkStart > 0, "缺少暗色档 .ip-scope[data-ip-theme=\"dark\"]");
  const darkEnd = tokens.indexOf("}", darkStart);
  const darkBlock = tokens.slice(darkStart, darkEnd);
  const dark = {};
  for (const m of darkBlock.matchAll(/(--ip-[a-z0-9-]+)\s*:\s*(#[0-9A-Fa-f]{6})/g)) dark[m[1]] = m[2];
  const uncovered = Object.keys(T).filter((k) => !dark[k]);
  assert.deepStrictEqual(uncovered, [], `暗色档漏定义：${uncovered.join("、")}（漏一个就会在暗色下退回浅色值）`);
});

test("规格 §3 实测对比度：视图令牌全部达标（正文 4.5 / 图形 3.0）", () => {
  const T = ipLightTokens();
  const pairs = [
    ["主文 / 卡", "--ip-text-primary", "--ip-card", 4.5],
    ["主文 / 画布", "--ip-text-primary", "--ip-canvas", 4.5],
    ["次文 / 卡", "--ip-text-secondary", "--ip-card", 4.5],
    ["弱文 / 卡", "--ip-text-muted", "--ip-card", 4.5],
    ["弱文 / 画布", "--ip-text-muted", "--ip-canvas", 4.5],
    ["白字 / 主按钮", "#FFFFFF", "--ip-primary-action", 4.5],
    ["链接 / 画布", "--ip-link", "--ip-canvas", 4.5],
    ["内衬字 / 内衬底", "--ip-primary-900", "--ip-inner", 4.5],
    ["成功字 / 成功底", "--ip-success", "--ip-success-bg", 4.5],
    ["警告字 / 警告底", "--ip-warning", "--ip-warning-bg", 4.5],
    ["错误字 / 错误底", "--ip-error", "--ip-error-bg", 4.5],
    ["辅助字 / 辅助底", "--ip-aux", "--ip-aux-bg", 4.5],
    ["主色图标 / 画布", "--ip-icon-brand-on-canvas", "--ip-canvas", 3.0],
    ["可点击描边 / 画布", "--ip-border-clickable", "--ip-canvas", 3.0],
    ["输入描边 / 卡", "--ip-border-interactive", "--ip-card", 3.0],
    ["主按钮 / 卡（图形）", "--ip-primary-action", "--ip-card", 3.0],
  ];
  const fails = [];
  for (const [label, fg, bg, min] of pairs) {
    const f = fg.startsWith("--") ? T[fg] : fg;
    const b = bg.startsWith("--") ? T[bg] : bg;
    assert.ok(f && b, `配对「${label}」引用了未定义的令牌（${f ? "" : fg + " "}${b ? "" : bg}）`);
    const r = ratio(f, b);
    if (r < min) fails.push(`${label}: ${r.toFixed(2)} < ${min}`);
  }
  assert.deepStrictEqual(fails, [], `对比度跌破门槛：\n${fails.join("\n")}`);
});

test("v3.3.1 口径：公告清单页删除，源站状态/补贴导出迁入设置，线索转待办保留", () => {
  // 公告清单切换入口必须整体移除（app.js 与 intel-projects.js 两处）
  assert.ok(!app.includes("intelMode"), "app.js 的 state.intelMode 应随公告清单一起删除");
  assert.ok(!app.includes("switcherOnly"), "app.js 不应再引用切换条");
  assert.ok(!app.includes("data-ip-mode="), "app.js 不应再有形态切换按钮");
  assert.ok(!mod.includes('data-ip-mode="list"'), "项目进程视图不应再有「公告清单」切换入口");
  assert.ok(!app.includes("data-verdict-key"), "公告卡片人工改判按钮应随清单页删除（项目视图工具条另有 data-ip-mark 入口）");
  // 源站状态与补贴导出必须迁入设置页，线索转待办保留在项目视图下方。
  // v3.4.0：设置页改两列卡片布局，节标题从 `<b>` 换成 `.set-card__head` 结构，
  // 所以判据改成「节标题文本 + 节卡片 data-set 标记」——不钉具体标签形式，
  // 钉的是「这一节在不在、是不是这一节」。改标签不改归属，钉标签会假红。
  assert.match(app, /data-set="intel"[\s\S]{0,200}跟踪采集/,
    "设置页必须有 ② 跟踪采集节（源站状态）");
  assert.match(app, /data-set="subsidy"[\s\S]{0,200}补贴明细导出/,
    "补贴明细导出必须保留（③ 节，位于设置页）");
  assert.ok(app.includes("<b>线索转待办</b>"), "线索转待办必须保留（现挂项目进程视图下方）");
  assert.match(app, /function viewIntelSites\(\)/, "源站状态表渲染函数必须存在");
  // v3.3.1 弹窗修复回归钉：openModal 必须挂 .ip-scope 容器（挂 body 会脱离
  // --ip-* 变量作用域 → 弹窗全透明，实测主按钮文字仅 1.36:1）
  assert.match(mod, /querySelector\("\.ip-scope"\)\s*\|\|\s*document\.body/,
    "openModal 必须优先挂 .ip-scope 容器，缺变量作用域兜底");
  // 迁移后绑定必须挂在设置分支，intel 分支不得再绑采集/补贴控件
  assert.match(app, /if \(state\.view === "settings"\) \{[\s\S]*?bindIntelCollectControls\(\)/, "设置分支必须绑定情报采集控件");
});

test("规格 §4：方案 A 主从双栏（左 340px 固定 / 右自适应）与底部工具条三动作", () => {
  assert.match(tokens, /\.ip-wrap\s*\{[\s\S]*?grid-template-columns:\s*340px minmax\(0, 1fr\)/,
    "方案 A 左栏必须固定 340px、右栏自适应");
  // 底部工具条三动作：转待办（主）/ 标记相关·无关 / 导出
  for (const id of ['id="ip-to-todo"', 'data-ip-mark="core"', 'data-ip-mark="excluded"', 'id="ip-export"']) {
    assert.ok(mod.includes(id), `底部工具条缺动作入口：${id}`);
  }
  // 转待办是主按钮（ip-btn），标记是次按钮（ip-btn--secondary），导出是文字按钮（ip-btn--text）
  const toolbar = mod.slice(mod.indexOf('id="ip-to-todo"') - 260, mod.indexOf('id="ip-export"') + 40);
  assert.match(toolbar, /class="ip-btn" id="ip-to-todo"/, "转待办必须是主按钮（实心 #2174E1）");
  assert.match(toolbar, /ip-btn--secondary[^"]*"[^>]*data-ip-mark/, "标记相关/无关必须是次按钮描边");
  assert.match(toolbar, /ip-btn--text[^"]*"[^>]*id="ip-export"/, "导出该项目进程必须是文字按钮");
});

test("规格 A-7：手动建档（两项必填 + 建档即查重 + 两个出口）与手动卡视觉", () => {
  assert.ok(mod.includes("＋ 手动建档"), "左栏缺「＋ 手动建档」入口");
  // 必填仅项目名 + 采购人
  assert.match(mod, /if \(!name \|\| !org\) \{ msg\.textContent = "项目名与采购人均必填。"/,
    "建档必须只强制项目名 + 采购人两项");
  // 建档即查重：实时比对已有归一化键，命中给「打开该聚合卡」/「仍单独建档」两个出口
  assert.ok(mod.includes("已存在疑似同一项目"), "建档面板缺查重命中提示");
  assert.ok(mod.includes("打开该聚合卡"), "查重命中后缺「打开该聚合卡」出口");
  assert.ok(mod.includes("仍单独建档"), "查重命中后缺「仍单独建档」出口（用户明示才允许建重复卡）");
  // 手动卡视觉：手动 chip + 六段全空心（无节点 → currentSeg 为 null → 全部 future）
  assert.ok(mod.includes('ip-chip--manual">手动'), "手动项目卡缺「手动」chip");
  assert.ok(mod.includes("暂无公告 · 手动建档"), "手动项目卡/轴缺「暂无公告 · 手动建档」标注");
  // 手动项目计入默认列表统计与计数（口径留痕 1）
  assert.match(mod, /const manualCount = projects\.filter\(\(p\) => p\.manual\)\.length/,
    "手动项目必须计入左栏项目计数（口径：计入、单独标「手动」）");
});

test("规格 A-7：疑似关联提示条 + 并排对照（差异高亮 + 合并/忽略二选一）", () => {
  assert.ok(mod.includes("检测到 "), "缺「检测到 N 条疑似关联公告」提示条");
  assert.ok(mod.includes("查看对照"), "提示条缺「查看对照」入口");
  // 并排对照：两列 + 逐字段差异高亮（样式规则在 tokens.css，不在 JS 模块里查）
  assert.match(tokens, /\.ip-cmp\s*\{\s*display:\s*grid;\s*grid-template-columns:\s*1fr 1fr/,
    "对照视图必须是并排两列");
  assert.ok(tokens.includes('.ip-cmp-row[data-diff="true"]'), "对照视图缺少差异行高亮样式");
  // 差异必须同时有色和文字（无障碍：不只靠颜色）
  assert.match(mod, /· 不一致/, "差异项必须有文字标注，不能只靠底色");
  // 二选一：合并 / 忽略
  assert.ok(mod.includes("合并到该项目流程"), "对照视图缺「合并到该项目流程」");
  assert.ok(mod.includes("不是同一项目（忽略）"), "对照视图缺「不是同一项目（忽略）」");
  // 口径留痕 2：只画强命中——归一化键完全相等，不做阈值模糊匹配
  assert.match(mod, /IP\.normKey\(q\.org, q\.project_name\) === key/,
    "近似命中的判据必须是「归一化键完全相等」（弱命中阈值归引擎侧，界面不自己定阈值）");
});

test("规格 §5 / §6：骨架加载态、极端折叠、三档视口", () => {
  // 加载态是骨架卡，不是 spinner
  assert.ok(!/spinner|loader\.gif|⏳/.test(mod), "加载态必须是骨架卡，不得用 spinner");
  assert.match(mod, /ip-sk/, "缺少骨架卡样式/渲染");
  // 极端：单段 ≥8 条折叠「展开其余 N 条」
  assert.match(mod, /const FOLD_AT = 8;/, "单段折叠阈值必须显式存在");
  assert.ok(mod.includes("展开其余"), "缺「展开其余 N 条」折叠入口");
  // 极端：200 项目分页 20/页
  assert.match(mod, /Pages? = Math\.max\(1, Math\.ceil\(total \/ 20\)\)|const pages = Math\.max\(1, Math\.ceil\(total \/ 20\)\)/,
    "批量项目必须按 20/页分页");
  // 三档视口：1280（基线 340） / 768（收 280 或抽屉） / 375（单列堆叠）
  assert.match(tokens, /@media \(max-width: 1100px\)[\s\S]*?grid-template-columns:\s*280px minmax\(0, 1fr\)/,
    "768 档必须把左栏收到 280px");
  assert.match(tokens, /@media \(max-width: 480px\)/, "375 档缺窄屏规则");
  assert.ok(tokens.includes('data-ip-full="page"'), "375 档缺「点卡进全屏进程页」的形态切换");
});

test("规格 §6 无障碍：动效尊重 prefers-reduced-motion、焦点环可见", () => {
  assert.match(tokens, /@media \(prefers-reduced-motion: reduce\)[\s\S]*?\.ip-modal-mask \{ animation: none; \}/,
    "缺 prefers-reduced-motion 降级");
  assert.match(tokens, /\.ip-btn:focus-visible \{ outline: 2px solid var\(--ip-link\)/,
    "按钮必须有可见焦点环（2px --ip-link）");
  assert.match(tokens, /\.ip-kcard:focus-visible \{ outline: 2px solid var\(--ip-link\)/,
    "卡片必须有可见焦点环");
  // 弹层 Esc 关闭
  assert.ok(mod.includes('e.key === "Escape"'), "弹层必须支持 Esc 关闭");
});

test("只做加法：不新增一级入口、不动既有导航与设置层", () => {
  const navBlock = html.slice(html.indexOf('<nav class="nav" id="nav"'), html.indexOf("</nav>"));
  const viewNames = [...navBlock.matchAll(/data-view="([a-z]+)"/g)].map((m) => m[1]);
  // v3.4.0 规格 §2.1：五版块导航。W8 退役清理后剩 6 个按钮；单B 并行两步：
  // W7 把「成员」入口迁入设置页（§4.3 账号管理），W6 把「网点」升级为「监控」
  // 页签容器（§3.4）。本守卫防的是顺序漂移与退役入口复活，而不是拒绝规格内的演进：
  // W1/W3（12 项）→ W5（+todo，13 项）→ W8（-7 退役项，6 项）→ W6（dealer→monitor）
  // → v3.4.0 用户标注处理（2026-11-28）：「添加」入口删除，5 项定稿。
  assert.deepStrictEqual(viewNames, ["project", "todo", "intel", "monitor", "settings"],
    "一级入口必须是 W8 退役 + W6 网点→监控 + 添加入口删除后的 5 项（project 第一、todo 第二，§2.1），退役入口不得复活");
  // 迁入/升级必须是「移动」而不是「两处并存」——旧入口不得残留
  assert.ok(!viewNames.includes("members"), "「成员」入口必须已迁入设置页，不得在导航里残留");
  assert.ok(!viewNames.includes("dealer"), "「网点」已升级为「监控」，不得以旧名残留在导航");
  // 新模块必须在 app.js 之前引入（app.js 里 viewIntel 会读 window.__IP）
  const iMod = html.indexOf('<script src="intel-projects.js">');
  const iApp = html.indexOf('<script src="app.js">');
  assert.ok(iMod > 0 && iApp > iMod, "intel-projects.js 必须在 app.js 之前引入");
});

test("不编造后端接口：只用已存在的 tauri 命令名", () => {
  const CMD = [...mod.matchAll(/invoke\("([a-z_]+)"/g)].map((m) => m[1]);
  const ALLOWED = [
    "intel_projects_get", "intel_project_get", "intel_lead_act", "intel_set_verdict",
    "manual_project_create", "manual_project_list", "manual_project_links",
    "manual_project_merge", "manual_project_ignore", "manual_project_reopen",
  ];
  const unknown = CMD.filter((c) => !ALLOWED.includes(c));
  assert.deepStrictEqual(unknown, [],
    `界面层调用了后端不存在的命令：${unknown.join("、")}。数据接线轨补齐前，前端不得自行发明命令名。`);
});

/* ── 注册存在性守卫：界面的每个 invoke 命令必须在 lib.rs 的 invoke_handler 里注册 ──
 *
 * Tauri 2.x 里 `#[tauri::command]` 只是定义，不进 `generate_handler![]` 清单就
 * 等于命令不存在（运行时报 unknown command）。上一条白名单只保证「名字认识」，
 * 这一条直接对 lib.rs 源码：界面调的每个命令名都必须出现在注册清单里，
 * 防止「壳层函数写了、注册漏了」这类静默断线。 */
test("界面 invoke 的每个命令名必须注册在 lib.rs 的 invoke_handler（未注册 = 命令不存在）", () => {
  const libPath = path.join(__dirname, "src-tauri", "src", "lib.rs");
  assert.ok(fs.existsSync(libPath), `找不到 lib.rs：${libPath}（路径变了请同步更新本守卫）`);
  const lib = fs.readFileSync(libPath, "utf8");
  const block = lib.match(/generate_handler!\s*\[([\s\S]*?)\]/);
  assert.ok(block, "lib.rs 里找不到 generate_handler![...] 注册清单");
  // 注册清单每行一个命令名（逗号结尾）；逐行匹配而不是全文子串，
  // 避免把注释里的英文单词误认成注册项。
  const registered = new Set([...block[1].matchAll(/^\s*([a-z_]+)\s*,\s*$/gm)].map((x) => x[1]));
  assert.ok(registered.size >= 40, `注册清单应有几十个命令，实测 ${registered.size} 个——匹配口径可能失效`);
  const CMD = [...new Set([...mod.matchAll(/invoke\("([a-z_]+)"/g)].map((x) => x[1]))];
  assert.ok(CMD.length > 0, "界面层应至少调用一个后端命令（正则口径失效会走到这里）");
  const missing = CMD.filter((c) => !registered.has(c));
  assert.deepStrictEqual(missing, [],
    `界面调用了 invoke 但 lib.rs invoke_handler 未注册：${missing.join("、")}——未注册的命令在运行时不存在`);
});


/* ── 门禁的门禁：CI 必须以「目录通配」跑全部桌面端守卫，不得点名式跑 ──────────
 *
 * 为什么单独立一条：11-19 的教训不是「某个测试写错了」，而是「测试根本没被执行」。
 * CI 里点名 `node --test a.test.cjs b.test.cjs`，目录里第 3、第 4 个文件就静默漏跑，
 * 且流水线照样全绿——这类缺陷不会自己报警，只能靠一条断言盯着。
 *
 * 判据是扫目录内容（含 node:test）而不是人工名单：只要 apps/desktop 下存在
 * *.test.cjs，就必须满足「工作流里出现整目录通配 / 显式枚举目录」，两者取其一。
 * 这样将来新增守卫文件时，本断言不会因为「名单里没写」而失真。
 */
test("门禁的门禁：CI 必须以目录口径跑全部桌面端守卫，不得点名式漏跑", () => {
  const desktopDir = path.join(__dirname);
  // 目录里的真实守卫文件（扫内容判定，不看文件名猜测）
  const guardFiles = fs
    .readdirSync(desktopDir)
    .filter((f) => f.endsWith(".test.cjs"))
    .filter((f) => fs.readFileSync(path.join(desktopDir, f), "utf8").includes("node:test"));
  assert.ok(guardFiles.length >= 2, `守卫文件应至少 2 个，实际 ${guardFiles.length}`);

  const ciDir = path.join(__dirname, "..", "..", ".github", "workflows");
  const wfFiles = fs.readdirSync(ciDir).filter((f) => f.endsWith(".yml") || f.endsWith(".yaml"));
  assert.ok(wfFiles.length > 0, "找不到任何 workflow 文件");

  // 逐工作流判定：是否有「跑桌面端守卫」的步骤，且口径是目录级
  // 目录级 = 出现 *.test.cjs 通配（bash 原样）或 显式 Get-ChildItem/枚举 apps/desktop 目录（pwsh）
  const violating = [];
  let compliant = [];
  for (const f of wfFiles) {
    const body = fs.readFileSync(path.join(ciDir, f), "utf8");
    if (!/apps\/desktop/.test(body)) continue;              // 与桌面端守卫无关的工作流，跳过
    if (!/node\s+--test/.test(body)) continue;               // 没跑 node 测试的，跳过
    // 2026-11-25 追加授权适配：node --test 与目录通配之间现在有 --test-reporter=tap
    // （GitHub runner 默认 spec 风格无汇总行，guard-gate 按 TAP 解析会误判，两条
    // workflow 均已固定该 flag）。目录口径的语义不变——只是允许 flag 存在于命令里。
    const FLAG = "(?:--[\\w-]+=[\\w]+\\s+)*";
    const dirLevel =
      new RegExp(`node\\s+--test\\s+${FLAG}apps/desktop/\\*\\.test\\.cjs`).test(body) ||
      (/Get-ChildItem[^\n]*apps\/desktop/.test(body) && /node\s+--test\s+\$files/.test(body));
    // 点名式：node --test 后面直接跟具体文件名（而不是通配或变量）
    const pointNamed = new RegExp(`node\\s+--test\\s+${FLAG}apps/desktop/(?!\\*)[^\\s*]+\\.test\\.cjs`).test(body);
    if (dirLevel) compliant.push(f);
    else if (pointNamed) violating.push(f);
  }
  assert.equal(violating.length, 0,
    `以下 workflow 点名式跑守卫，会让目录里其余测试静默漏跑：${violating.join("、")}`);
  assert.ok(compliant.length >= 1,
    `必须有至少一个 workflow 以目录口径跑守卫（实际 0 个）。已检查：${wfFiles.join("、")}`);

  // 反向钉住：windows-release.yml 必须点名覆盖 intel-projects 所在目录（防有人把通配改回点名）
  const rel = fs.readFileSync(path.join(ciDir, "windows-release.yml"), "utf8");
  // 同上：允许 --test-reporter=tap 等 flag 插在 --test 与通配之间（口径语义不变）
  assert.match(rel, /node\s+--test\s+(?:--[\w-]+=[\w]+\s+)*apps\/desktop\/\*\.test\.cjs/,
    "windows-release.yml 的守卫步骤必须保持目录通配 apps/desktop/*.test.cjs");

  // 防空转：必须至少存在 1 处「多包 cargo test」写法，否则上面的通配检查可能形同虚设
  const cargoMulti = wfFiles
    .map((f) => fs.readFileSync(path.join(ciDir, f), "utf8"))
    .filter((b) => /cargo\s+test\s+-p\s+[^\n]*\s-p\s+/.test(b)).length;
  assert.ok(cargoMulti >= 1, "工作流里应至少有一处多包 cargo test（否则本守卫的上下文已漂移）");

  // 多包 cargo test 必须带 --no-fail-fast（逐行判定，同一行里出现 ≥2 个 -p 才算多包）。
  // 原因：cargo 默认 fail-fast 以**测试目标**为单位短路，前一个包红了后一个包就不执行，
  // 日志里只出现一条 test result 行，极易被读成「两个包都验过」。门禁强度没变，
  // 被削掉的是红侧诊断信息 —— 这类缺陷落地时全绿，出事时才暴露，必须静态钉住。
  const offenders = [];
  for (const f of wfFiles) {
    const body = fs.readFileSync(path.join(ciDir, f), "utf8");
    body.split("\n").forEach((line, i) => {
      // 注释行不算真实调用：YAML 注释里常引用命令原文（本文件第 24 行就是），
      // 若一并计入，注释里的示例会被误判成缺 flag（实测踩过，报在 :24）。
      if (/^\s*#/.test(line)) return;
      if (!/cargo\s+test/.test(line)) return;
      const pkgCount = (line.match(/\s-p\s+/g) || []).length;
      if (pkgCount >= 2 && !/--no-fail-fast/.test(line)) {
        offenders.push(`${f}:${i + 1}（${pkgCount} 个 -p 缺 --no-fail-fast）`);
      }
    });
  }
  assert.equal(offenders.length, 0,
    `多包 cargo test 必须带 --no-fail-fast，否则会 fail-fast 短路：${offenders.join("；")}`);
});

// ── 18. 渲染模板的两类「看起来对、实际没生效」缺陷 ─────────────────────
// 都是本轮真机上抓到的：静态看代码完全正常，只有跑起来才发现。
//  (a) 重复 id：同一段渲染里两个按钮共用 id="ip-mark"。绑定侧用的是
//      querySelectorAll("[data-ip-mark]") 所以两个都能点，但 id 重复本身
//      违反 DOM 唯一性：任何 getElementById / label for / aria 关联都会只取第一个，
//      且日后一旦有人改用 q("#ip-mark") 就会静默只绑一半。
//  (b) 死三元 `${cond ? "" : ""}`：两个分支同值，条件白写，多半是想写样式差异却没写成。
test("渲染模板：不得重复 id、不得出现两支同值的三元", () => {
  const mod = fs.readFileSync(MOD, "utf8");

  // 只统计模板字符串里真实 emit 出来的 id="..."（含 ${...} 占位）
  const ids = [...mod.matchAll(/id="([^"]+)"/g)].map((m) => m[1]);
  const seen = new Map();
  const dups = [];
  for (const id of ids) {
    if (seen.has(id)) dups.push(id);
    else seen.set(id, true);
  }
  assert.equal(dups.length, 0,
    `渲染模板里出现重复 id（DOM 唯一性被破坏，改用 class/data-* 区分）：${[...new Set(dups)].join("、")}`);

  // 死三元：? 与 : 两侧是完全相同的字面量（"" 或 ''）
  const dead = [...mod.matchAll(/'?\s*\?\s*(""|'')\s*:\s*(""|'')\s*\}?/g)]
    .filter((m) => m[1] === m[2]);
  assert.equal(dead.length, 0, `发现两支同值的死三元表达式（条件白写）：${dead.map((m) => m[0].trim()).join("、")}`);
});

// ── 19. 六段码跨轨一致性：界面别名表必须覆盖后端 intel_segment 的真实码表 ────
// 后端有两套阶段口径在同一仓里并存：
//   crates/wb-core/src/intel_segment.rs（CLI payload，六段）: intent/tender/correction/award/contract/termination
//   crates/wb-core/src/intel_stage.rs（Tauri 命令，五段）   : intent/review/contract/acceptance/operation/unknown
// 界面内部键沿用规格 §2 文案（publish/correct/terminate）。若数据轨把 Tauri 命令
// 切到六段 payload，而界面别名表没覆盖 tender/correction/termination，
// 这三个段会被静默判成「阶段未知」——界面照样能跑，只是段全空，最难在评审里看出来。
// 本断言直接读 Rust 侧码表，钉住「后端有的码，界面必须认识」。
test("六段码跨轨一致：界面别名表覆盖 intel_segment 全部码值", () => {
  const segRs = path.join(__dirname, "..", "..", "crates", "wb-core", "src", "intel_segment.rs");

  // 仓库根相对 apps/desktop 是 ../..；但守卫也可能在只含 apps 的裁剪树里跑，
  // 故读不到就明确 skip（不静默通过、不假绿）。
  if (!fs.existsSync(segRs)) {
    console.log("# 跳过：未找到 crates/wb-core/src/intel_segment.rs（裁剪树），本断言在后端同仓时生效");
    return;
  }
  const rs = fs.readFileSync(segRs, "utf8");

  // 抽 code() 里所有 => "xxx" 字面量
  const codeFn = rs.slice(rs.indexOf("pub fn code(self)"), rs.indexOf("pub fn label(self)"));
  const backendCodes = [...codeFn.matchAll(/=>\s*"([a-z]+)"/g)].map((m) => m[1]);
  assert.ok(backendCodes.length >= 6,
    `intel_segment 的 code() 应至少给出 6 个码值，实际抓到 ${backendCodes.length}：${backendCodes.join("、")}`);

  const mod = fs.readFileSync(MOD, "utf8");
  const missing = backendCodes.filter((c) => !new RegExp(`${c}:\\s*"`).test(mod));
  assert.equal(missing.length, 0,
    `界面别名表缺少后端码值（切到六段 payload 后这些段会被判成阶段未知）：${missing.join("、")}`);
});
