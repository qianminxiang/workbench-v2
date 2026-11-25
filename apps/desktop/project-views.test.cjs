/* W3 任务版块守卫测试（node --test，零依赖，静态源断言模式）。
 *
 * 钉住的是六条硬约束里归 W3 的三条 + 接线完整性：
 *   ③ 空库起步不编造数据 —— 空态/错误态文案逐字取自规格 ch3/ch6，
 *      错误态不得降级成空态，向导不得自动执行迁移；
 *   ① 清入口不清数据 —— 本工作项不动任何数据面（断言无写命令直删）；
 *   接线 —— index.html 脚本顺序 / app.js 只做调度 / tauri asset 协议配置。
 * 谁把示例数据、板块筛选、无确认自动迁移加回来，这里立刻红。
 */
"use strict";

const test = require("node:test");
const assert = require("node:assert");
const fs = require("node:fs");
const path = require("node:path");

const SRC = path.join(__dirname, "src");
const PV = path.join(SRC, "project-views.js");
const APP = path.join(SRC, "app.js");
const HTML = path.join(SRC, "index.html");
const CSS = path.join(SRC, "tokens.css");
const CONF = path.join(__dirname, "src-tauri/tauri.conf.json");
const LIB = path.join(__dirname, "src-tauri/src/lib.rs");

const read = (p) => {
  assert.ok(fs.existsSync(p), `找不到文件 ${p}`);
  return fs.readFileSync(p, "utf8");
};

test("接线：project-views.js 在 app.js 之前、intel-projects.js 之后引入", () => {
  const h = read(HTML);
  const iIntel = h.indexOf('<script src="intel-projects.js">');
  const iPv = h.indexOf('<script src="project-views.js">');
  const iApp = h.indexOf('<script src="app.js">');
  assert.ok(iIntel >= 0, "index.html 缺 intel-projects.js 引入");
  assert.ok(iPv > iIntel, "project-views.js 必须在 intel-projects.js 之后");
  assert.ok(iApp > iPv, "app.js 必须在 project-views.js 之后（模块先于调度方求值）");
});

test("接线：任务为导航第一入口（§2.1）", () => {
  const h = read(HTML);
  const firstBtn = h.match(/<nav[^>]*>\s*<!--[^>]*-->\s*<button data-view="([^"]+)"/s)
    || h.match(/<nav[^>]*>\s*<button data-view="([^"]+)"/s);
  assert.ok(firstBtn, "nav 结构变化，找不到第一个按钮");
  assert.equal(firstBtn[1], "project", "导航第一位必须是任务版块");
});

test("接线：app.js 四处调度齐全且只做调度", () => {
  const a = read(APP);
  assert.ok(/project: viewProject,/.test(a), "VIEWS map 缺 project");
  assert.ok(/window\.__PV\?\.sync\(state\.view\)/.test(a), "render() 缺 PV.sync");
  assert.ok(/state\.view === "project"\)\s*\{[^}]*window\.__PV\)\s+window\.__PV\.bind\(\)/s.test(a), "bindView 缺 project 分支");
  assert.ok(/function viewProject\(\)\s*\{[^}]*__PV\.view\(\)/s.test(a), "缺 viewProject 调度函数");
});

test("空态铁律（§6.1/ch3 逐字）：文案逐字、无示例项目、无统计卡", () => {
  const v = read(PV);
  assert.ok(v.includes("从零开始，先建一条任务"), "空态标题文案被改动");
  assert.ok(v.includes("任务库为空。新建一条任务、记一次过程记录，全年河流、待办与提醒都会随真实操作长出来；网点与情报可在对应版块恢复采集快照。"), "空态正文文案被改动");
  // 「无示例数据」断言只看可执行代码：注释里出现「不显示示例项目」这类规格说明
  // 不算编造——先剥掉块注释与行注释再断言。
  const noComments = v.replace(/\/\*[\s\S]*?\*\//g, "").replace(/^\s*\/\/.*$/gm, "");
  assert.ok(!/示例项目|demo|样例项目/.test(noComments), "空库起步出现编造的示例数据");
});

test("错误态（ch3 逐字）：读取本地数据失败 + 错误码 + 重试 + 查看诊断，不降级成空态", () => {
  const v = read(PV);
  assert.ok(v.includes("读取本地数据失败"), "错误态标题文案被改动");
  assert.ok(v.includes('id="pv-retry"'), "错误态缺重试按钮");
  assert.ok(v.includes("查看诊断"), "错误态缺查看诊断入口");
  assert.ok(v.includes(".slice(0, 200)"), "错误码应截断到 200 字防刷屏");
  // 错误分支必须先于空态分支（读取失败 ≠ 空库）。匹配完整语句而非片段——
  // view() 之外还有 load() 的 `st.projects.length === 0`（maybeOfferMigration 触发），
  // 按片段 indexOf 会先命中那一处，误判顺序。
  const iErr = v.indexOf("if (st.error) return head + errCard");
  const iEmpty = v.indexOf("if (st.projects.length === 0) return head + emptyCard");
  assert.ok(iErr >= 0 && iEmpty >= 0 && iErr < iEmpty, "错误态分支必须在空态分支之前");
});

test("筛选条（§3.1）：状态+地区+关键词，不含板块维度", () => {
  const v = read(PV);
  assert.ok(/data-f="stage"/.test(v), "缺状态筛选");
  assert.ok(/data-f="area"/.test(v), "缺地区筛选");
  assert.ok(/data-f="kw"/.test(v), "缺关键词筛选");
  const bar = v.slice(v.indexOf("function filterBar"), v.indexOf("function filtered"));
  assert.ok(!/板块|board/i.test(bar), "筛选条不得再含板块维度（6 板块口径已退役）");
});

test("弹层容量与校验（§5.1）：图片≤9、附件≤5、正文≤500、失焦显红字", () => {
  const v = read(PV);
  assert.ok(/MAX_IMAGES = 9/.test(v), "图片上限必须是 9");
  assert.ok(/MAX_FILES = 5/.test(v), "附件上限必须是 5");
  assert.ok(/BODY_MAX = 500/.test(v), "动作摘要上限必须是 500");
  assert.ok(/maxlength="\$\{BODY_MAX\}"/.test(v), "textarea 必须挂 maxlength");
  assert.ok(/addEventListener\("blur", \(\) => validate\(false\)\)/.test(v), "失焦才显示红字");
});

test("迁移向导（§3.5）：预览只读、pending_items>0 才弹、执行必须经确认", () => {
  const v = read(PV);
  assert.ok(/migration_preview/.test(v) && !/migration_preview.*then\s*run/.test(v), "预览必须只读");
  assert.ok(/!plan \|\| !plan\.pending_items\) return/.test(v), "pending_items 为 0 不得弹向导");
  assert.ok(/#pv-w-go/.test(v) && /#pv-w-skip/.test(v), "向导必须有确认/暂不迁移两个动作");
  const mount = v.slice(v.indexOf("function mountWizard"), v.lastIndexOf("function closeAllOverlays") > 0 ? v.lastIndexOf("function closeAllOverlays") : undefined);
  assert.ok(!/migration_run/.test(mount.slice(0, mount.indexOf("go.addEventListener"))), "弹层渲染期不得自动执行迁移");
  assert.ok(/project_migration_run", \{ plan, year/.test(v), "确认按钮才调 migration_run，且回传 plan 原样");
});

test("Stage serde 契约：展示用小写码、提交 draft.stage 用 PascalCase", () => {
  const v = read(PV);
  assert.ok(/STAGE_SERDE = \{ planned: "Planned"/.test(v), "缺 STAGE_SERDE 映射");
  assert.ok(!/stage: stageSel[,}]/.test(v), "存在未经 serde 映射的 stage 提交点");
  assert.ok(/stage: STAGE_SERDE\[/.test(v), "提交点必须经 STAGE_SERDE 转换");
});

test("浮层拆层：Esc 逐层退、切走视图时 PV.sync 全拆", () => {
  const v = read(PV);
  assert.ok(/key !== "Escape"/.test(v), "缺 Esc 处理");
  assert.ok(/PV\.sync = function sync\(view\)\s*\{\s*if \(view !== "project"\) closeAllOverlays\(\);/.test(v), "PV.sync 必须在切走时拆浮层");
  assert.ok(/\$all\("\.pv-drawer-mask"\)\.forEach\(\(m\) => m\.remove\(\)\)/.test(v), "重复开抽屉必须先清旧浮层");
});

test("asset 协议：tauri.conf.json 开通且 CSP 放行、作用域只开附件子目录", () => {
  const conf = JSON.parse(read(CONF));
  const sec = conf.app.security;
  assert.equal(sec.assetProtocol.enable, true, "assetProtocol 未启用");
  // v3.4.0 · H1：作用域收窄到附件存储子目录 attachments/（store.rs 的 Layout::attachments_dir
  // = <data_dir>/attachments，附件落盘形如 attachments/<sha八位>/<sha>.<ext>）。
  // 不允许退回 $APPDATA/**——那等于把 workbench.db / archive.db / backups / snapshots
  // 一并交给 webview 的 asset 协议读取，是质检点名的越权面。
  assert.deepEqual(sec.assetProtocol.scope.allow, ["$APPDATA/attachments/**"],
    "作用域必须收窄到附件子目录（不得回到整个 $APPDATA）");
  assert.ok(sec.csp.includes("asset:"), "CSP img-src 未放行 asset:");
  assert.ok(sec.csp.includes("http://asset.localhost"), "CSP 未放行 Windows asset 域");
});

test("附件缩略：相对路径必须经 data_dir_path 拼 absolute（asset 协议只认绝对路径）", () => {
  const v = read(PV);
  const l = read(LIB);
  assert.ok(/data_dir_path/.test(l), "lib.rs 缺 data_dir_path 命令");
  assert.ok(/data_dir_path,\s*\n?\s*\]\)/.test(l), "invoke_handler 未注册 data_dir_path");
  assert.ok(/attSrc\(a, dataDirCache\)/.test(v), "缩略渲染必须走 attSrc 绝对路径");
  assert.ok(!/fileSrc\(a\.storage_path\)/.test(v), "不得把相对 storage_path 直接送 convertFileSrc");
});

test("样式闭合：JS 里的 pv-* 类在 tokens.css 都有对应选择器", () => {
  const v = read(PV);
  const c = read(CSS);
  // 提取前先剥掉两类 id 上下文：`id="pv-…"` 属性值与 `#pv-…` id 选择器——
  // id 元素不需要 CSS 类选择器，把它们算进「缺选择器」名单全是假红。
  // class 上下文（class="pv-…" / classList.add("pv-…") / 类选择器 ".pv-…"）保留提取。
  const noIds = v.replace(/id="[^"]*"/g, "").replace(/#pv-[a-z0-9-]+/g, "");
  const used = new Set((noIds.match(/pv-[a-z0-9-]+/g) || [])
    // pv-go-diag：纯行为钩子（事件绑定目标），视觉完全由 .btn--ghost 提供，无需 CSS。
    .filter((x) => !x.startsWith("pv-badge--") && x !== "pv-del--chip" && x !== "pv-go-diag"));
  const missing = [...used].filter((x) => !c.includes(`.${x}`));
  assert.deepEqual(missing, [], `CSS 缺选择器：${missing.join(", ")}`);
});
