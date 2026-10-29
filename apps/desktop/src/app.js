/* 综合工作台 v2.0 · 前端。
 * 分层判据（架构 §3.1）：这里只做呈现与交互意图表达，不含任何业务规则——
 * 提醒、晨报、KPI 全部由 wb-core 纯函数产出，本文件只负责把它们摆到屏上。
 * 形态切换 ≤240ms（tokens.css --motion-switch），筛选状态三形态共享。
 */
"use strict";

const invoke = (cmd, payload) => window.__TAURI__.core.invoke(cmd, payload);
// 业务年度（v2.2.1）：默认 2027；启动时由后端 fiscal_year 命令回填，两端同源。
let YEAR = 2027;

// 当前生效的主题档位（dark=机甲态 / light=办公态 / system=跟随系统）
let currentTheme = "dark";

/** 主题档位落到 <html data-theme>。默认锁机甲态（v2.2.1 对症项：
 *  原默认「跟随系统」，用户在亮色系统上打开看到的是素净办公界面，机甲视觉完全没生效）。 */
function applyTheme(theme) {
  const t = ["dark", "light", "system"].includes(theme) ? theme : "dark";
  const root = document.documentElement;
  if (t === "system") root.removeAttribute("data-theme");
  else root.dataset.theme = t;
  currentTheme = t;
  return t;
}

/** 轻量浮层提示：不打断流程的即时反馈（如「该通知已处理」）。 */
function toast(text, ms) {
  const dur = ms || 2600;
  let el = document.getElementById("wb-toast");
  if (!el) {
    el = document.createElement("div");
    el.id = "wb-toast";
    el.className = "wb-toast";
    document.body.appendChild(el);
  }
  el.textContent = text;
  el.hidden = false;
  clearTimeout(el._t);
  el._t = setTimeout(() => { el.hidden = true; }, dur);
}

// ── 状态 ──
const state = {
  view: "brief",
  items: [],
  filterBoard: "",
  alertsData: null,
  kpis: [],
  units: [],
  prefs: null, // 单机偏好（免打扰窗口 + 语音开关），boot 时读、改时写
};

const $ = (sel) => document.querySelector(sel);
const esc = (s) => String(s ?? "").replace(/[&<>"']/g, (c) =>
  ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" }[c]));

const BOARD_ZH = { policy: "政策项目", gov: "政府关系", industry: "行业资源", media: "媒体品牌", event: "活动管理", customer: "客户线索" };
const CODE_OF = Object.fromEntries(Object.entries(BOARD_ZH).map(([k, v]) => [v, k]));
const BOARD_CLASS = { "政策项目": "b1", "政府关系": "b2", "行业资源": "b3", "媒体品牌": "b4", "活动管理": "b5", "客户线索": "b6" };
/** v2.2.2（需求15）：板块色改为 CSS 静态映射（data-board="b1..b6" → var(--board-bN)）。
 *  原实现用内联 `style="background:var(--board-bN)"`，语义色写死在 JS 字符串里、
 *  tokens.css 里一个 var() 消费点都没有——这正是「板块色看不出机甲风」的机械原因。 */
const boardClass = (b) => BOARD_CLASS[BOARD_ZH[b] ?? b] || "b1";

// ── 数据载入 ──
async function reload() {
  try {
    state.items = await invoke("list_items", { year: YEAR });
    state.alertsData = await invoke("alerts", { year: YEAR });
    state.kpis = await invoke("kpi_overview", { year: YEAR });
    state.units = await invoke("units", { year: YEAR });
    if (state.view === "intel") await loadIntel();
    if (state.view === "dealer") await loadDealer();
    render();
    updateFilterCount();
  } catch (e) {
    $("#main").innerHTML = `<div class="panel"><div class="empty">
      数据未导入。请到「设置」页导入存量 Excel，或确认数据库文件完好。<br>
      <span class="mini">${esc(e)}</span></div></div>`;
  }
}

// ── 筛选（三形态共享） ──
function filtered() {
  const code = state.filterBoard ? CODE_OF[state.filterBoard] : "";
  return code ? state.items.filter((i) => i.board === code) : state.items;
}
function updateFilterCount() {
  $("#filter-count").textContent = `${filtered().length} / ${state.items.length}`;
}

// ── 呈现辅助 ──
const pulsedCards = new Set(); // 4.4：警示脉冲每卡每会话 1 次
function alertOf(item) {
  return (state.alertsData?.alerts || []).find((x) => x.item_id === item.id) || null;
}
function lampFor(item) {
  const a = alertOf(item);
  if (!a) return "";
  if (a.kind !== "overdue" && a.kind !== "due_soon") return "";
  const kind = a.kind === "overdue" ? "red" : "amber";
  // 4.4 警报疲劳控制：脉冲每卡每会话只播 1 次，不循环
  let pulse = "";
  if (!pulsedCards.has(item.id)) { pulsedCards.add(item.id); pulse = " lamp--pulse"; sfxAlert(); }
  return `<span class="lamp lamp--${kind}${pulse}">${kind === "red" ? "逾期" : "临期"}</span>`;
}
/** 警示卡（4.4 第四通道——位置即异常）：临期/逾期卡右上切角 + 左侧警示条。 */
function cardAlertCls(item) {
  const a = alertOf(item);
  if (!a) return "";
  if (a.kind === "overdue") return " card--alert card--red";
  if (a.kind === "due_soon") return " card--alert card--amber";
  return "";
}
function alertStripOf(item) { return cardAlertCls(item) ? `<span class="alert-strip"></span>` : ""; }
function gauge(pct) {
  const on = Math.round(pct / 20);
  let cells = "";
  for (let k = 0; k < 5; k++) cells += `<i class="${k < on ? "on" : ""}"></i>`;
  return `<div class="gauge" title="${pct}%">${cells}</div>`;
}
function dateCell(item) {
  // 时间线口径（红线）：只显示有依据的精度，月粒度绝不显示为具体日
  const fmt = (d, p) => {
    if (!d) return "—";
    if (p === "month") return d.slice(0, 7) + "（月）";
    if (p === "quarter") return d.slice(0, 4) + "（季度）";
    if (p === "ongoing") return "持续";
    if (p === "unknown") return "待定";
    return d;
  };
  return `${fmt(item.planned_start_date, item.planned_start_precision)} → ${fmt(item.planned_end_date, item.planned_end_precision)}`;
}

// ── 视图：项目情报（FR-2，v2.2）──
// 红线：抓取失败只影响本视图；离线可启动（AC-2-11），失败原因如实可见（AC-2-5）
async function loadIntel() {
  try { state.intel = await invoke("intel_get"); } catch (_) { state.intel = null; }
}
function relBadge(r) {
  if (r === "core") return '<span class="lamp lamp--green">相关</span>';
  if (r === "suspect") return '<span class="lamp lamp--amber">疑似不相关</span>';
  return "";
}
function tierZh(t) {
  return { provincial_main: "省级主通道", provincial_fallback: "省级兜底", city_batch1: "一批",
    city_batch2: "二批", city_weekly: "周频观察", manual_only: "人工通道", infeasible: "不可行" }[t] || t;
}
function viewIntel() {
  const d = state.intel;
  if (!d) {
    return `<div class="panel"><div class="empty">情报数据加载失败（离线或数据库异常）。<br>
      <button class="btn" id="intel-refresh">重试</button></div></div>`;
  }
  const cov = d.coverage || {};
  const sites = d.sites || {};
  const reg = d.registry || [];
  const siteRows = reg.map((r) => {
    const st = sites[r.id] || {};
    const status = !r.scheduled
      ? '<span class="mini">不进自动调度</span>'
      : st.last_status === "ok"
        ? `<span class="mini">✅ 成功（${st.last_success || ""}）</span>`
        : st.last_status
          ? `<span class="mini" style="color:var(--warn,#c0392b)">${st.last_status === "blocked" ? "被拦截" : st.last_status === "network" ? "网络失败" : "解析失败"}：${esc(st.last_detail || "")}</span>`
          : '<span class="mini">未采集</span>';
    return `<tr><td>${r.name}</td><td>${r.city}</td><td>${tierZh(r.tier)}</td><td>${status}</td><td class="mini">${esc(r.note)}</td></tr>`;
  }).join("");
  const items = (d.items || []).map((it) => {
    const verdictBtns = it.manual_verdict
      ? '<span class="mini">已人工改判</span>'
      : `<button class="btn btn--sm" data-verdict-key="${esc(it.key)}" data-verdict-val="core">✓ 相关</button>
         <button class="btn btn--sm" data-verdict-key="${esc(it.key)}" data-verdict-val="excluded">✗ 无关</button>`;
    return `<div class="card card--intel${it.relevance === "core" ? " card--selected" : ""}">
      <div class="body">
        <div class="card-title">${it.incomplete ? "⚠ " : ""}${esc(it.title)}</div>
        <div class="mini">${esc(it.org || "单位缺失")} · ${esc(it.area || "地区缺失")} · ${esc(it.date || "日期缺失")}${it.amount ? " · " + esc(it.amount) : ""}</div>
        <div class="mini">来源：${esc(it.site_name)} · <a href="${esc(it.url)}" target="_blank">原文链接</a></div>
        ${it.reasons?.length ? `<div class="mini">命中：${esc(it.reasons.join("，"))}</div>` : ""}
        <div class="intel-actions">${relBadge(it.relevance)} ${verdictBtns}</div>
      </div>
    </div>`;
  }).join("");
  return `
  <div class="panel">
    <div><b>项目情报</b> · 最近采集：${esc(d.last_run_date || "从未")} · 已接入 ${cov.connected ?? "?"} / 计划覆盖 ${cov.planned ?? "?"} 源 · 库存 ${d.total_stored ?? 0} 条
      <button class="btn" id="intel-refresh">立即刷新</button></div>
  </div>
  <div class="panel">
    <div><b>源站状态</b>（${reg.length} 源）</div>
    <table class="tbl"><tr><th>站点</th><th>地区</th><th>批次</th><th>状态</th><th>依据</th></tr>${siteRows}</table>
  </div>
  <div class="panel">
    <div><b>相关项目</b>（${(d.items || []).length} 条，按发布日期倒序；「疑似不相关」可人工改判）</div>
    ${items || '<div class="empty">今日 0 条相关——已检查的源均无命中，与「没采到」区分见上表各源状态。</div>'}
  </div>`;
}

// ── 视图：渠道网点（FR-1，v2.2）──
async function loadDealer() {
  try { state.dealer = await invoke("dealer_get"); } catch (_) { state.dealer = null; }
}
function viewDealer() {
  const d = state.dealer;
  if (!d) {
    return `<div class="panel"><div class="empty">网点数据加载失败。<br><button class="btn" id="dealer-refresh">重试</button></div></div>`;
  }
  const diff = d.last_diff;
  const byCityList = (rows, label) => (rows || []).map((g) => `
      <div class="mini"><b>${esc(g.city)}（${(g.nodes || []).length} 家）：</b>
        <ul class="tree-dealers">${(g.nodes || []).map((n) => `<li>${esc(n.name)}${n.area ? "（" + esc(n.area) + "）" : ""}</li>`).join("")}</ul></div>`).join("");
  let diffHtml = "";
  if (!diff) {
    diffHtml = '<div class="empty">尚未执行周比对（每周自动一次，也可手动刷新建首周基线）。</div>';
  } else if (diff.baseline_week) {
    diffHtml = `<div class="empty">首周基线已建立：共 ${diff.total} 家网点。下周起开始增减比对。</div>`;
  } else {
    diffHtml = `<div>本周比对：新增 ${(diff.added || []).length} 家 / 消失 ${(diff.removed || []).length} 家 · 当前共 ${diff.total} 家</div>
      ${byCityList(d.added_by_city, "新增")}
      ${byCityList(d.removed_by_city, "消失")}`;
  }
  // 三级树：安徽省 → 16 地市 → 区县（v2.2.1 改版：~190 家平铺观感是文字墙）
  const tree = d.tree || [];
  const total = (d.snapshot || []).length;
  const treeHtml = tree.length
    ? tree.map((c, ci) => `
      <details class="tree-city"${ci === 0 ? " open" : ""}>
        <summary>${esc(c.city)}<span class="tree-badge">${c.total}</span></summary>
        ${(c.districts || []).map((dist) => `
        <details class="tree-district">
          <summary>${esc(dist.district)}<span class="tree-badge">${dist.count}</span></summary>
          <ul class="tree-dealers">${(dist.dealers || []).map((n) => `<li>${esc(n)}</li>`).join("")}</ul>
        </details>`).join("")}
      </details>`).join("")
    : '<div class="empty">暂无网点数据</div>';
  return `
  <div class="panel">
    <div><b>渠道网点（极飞·安徽）</b> · 上次比对：${esc(d.last_run_date || "从未")}
      <button class="btn" id="dealer-refresh">立即刷新</button></div>
    ${diffHtml}
  </div>
  <div class="panel">
    <div><b>当前快照</b>（安徽省 · ${total} 家，按地市/区县归组）</div>
    ${treeHtml}
  </div>`;
}

// ── 视图：晨报（F2） ──
function viewBrief() {
  const b = state.alertsData?.brief;
  if (!b) return `<div class="empty">暂无晨报数据</div>`;
  const list = (arr, kind) => (arr || []).map((a) =>
    `<div class="card${kind ? ` card--alert card--${kind}` : ""}" tabindex="0">${kind ? `<span class="alert-strip"></span>` : ""}<div class="rowline">
      <span class="board-strip" data-board="${boardClass(a.board)}"></span>
      <div style="flex:1"><b>${esc(a.title)}</b>
        <div class="mini">${esc(a.detail)}</div></div>
      ${kind ? `<span class="lamp lamp--${kind}">${kind === "red" ? "逾期" : "临期"}</span>` : ""}
    </div></div>`).join("");
  return `
    <div class="panel panel--chamfer" style="--panel-strip: var(--mech-ok); padding:16px">
      <b>晨报 · <span class="mono">${esc(b.date)}</span><span class="radar" title="扫描中" aria-hidden="true"></span></b>
      <div><span class="text-2">① 当日到期 / 应启动</span>${list(b.due_today) || '<div class="mini">无</div>'}</div>
      <hr class="divider">
      <div><span class="text-2">② 临期</span>${list(b.due_soon, "amber") || '<div class="mini">无</div>'}</div>
      <hr class="divider">
      <div><span class="text-2">② 逾期</span>${list(b.overdue, "red") || '<div class="mini">无</div>'}</div>
    </div>`;
}

// ── 视图：看板（L1：选中板块时该板块 2 列聚焦，未筛选时 3×2 全景，不做 6 列并置） ──
function viewKanban() {
  const boards = ["policy", "gov", "industry", "media", "event", "customer"];
  const cardOf = (i, zh) => `<div class="card${cardAlertCls(i)}" tabindex="0">${alertStripOf(i)}
    <span class="board-strip" data-board="${boardClass(zh)}"></span>
    <div class="body"><b>${esc(i.title)}</b>
      <div class="rowline" style="margin-top:4px">${gauge(i.progress_pct)}<span class="mini mono">${i.progress_pct}%</span>${lampFor(i)}</div>
    </div>
  </div>`;
  const focus = state.filterBoard ? CODE_OF[state.filterBoard] : "";
  if (focus) {
    const zh = BOARD_ZH[focus];
    const items = filtered().filter((i) => i.board === focus);
    const open = items.filter((i) => i.status !== "done");
    const done = items.filter((i) => i.status === "done");
    const col = (title, arr) => `<section class="panel" data-board="${boardClass(zh)}" style="padding:12px">
      <div class="rowline" style="margin-bottom:8px">
        <b>${esc(zh)} · ${title}</b><span class="mini" style="margin-left:auto">${arr.length} 项</span>
      </div>
      ${arr.map((i) => cardOf(i, zh)).join("") || '<div class="mini">（空）</div>'}
    </section>`;
    return `<div class="kanban kanban--focus">${col("未启 / 进行", open)}${col("已闭环", done)}</div>`;
  }
  return `<div class="kanban">` + boards.map((code) => {
    const zh = BOARD_ZH[code];
    const items = filtered().filter((i) => i.board === code);
    const status = (...ss) => items.filter((i) => ss.includes(i.status ?? "")).length;
    return `<section class="panel" data-board="${boardClass(zh)}" style="padding:12px">
      <div class="rowline" style="margin-bottom:8px">
        <b>${zh}</b><span class="mini" style="margin-left:auto">${items.length} 项</span>
      </div>
      <div class="mini text-2" style="margin-bottom:8px">未启 ${status("pending", "unprepared")} · 进行 ${status("active")} · 闭环 ${status("done")}</div>
      ${items.map((i) => cardOf(i, zh)).join("") || '<div class="mini">（空）</div>'}
    </section>`;
  }).join("") + `</div>`;
}

// ── 视图：明细（L4：9 列砍 7 列——类型+优先级合并双行，下一步移出明细表；区间/进度 mono） ──
function viewDetail() {
  const rows = filtered().map((i) => `<tr>
    <td>${esc(BOARD_ZH[i.board] ?? i.board)}</td>
    <td><b>${esc(i.title)}</b></td>
    <td>${esc(i.category ?? "—")}<div class="mini text-3">${esc(i.priority ?? "—")}</div></td>
    <td class="mono">${dateCell(i)}</td>
    <td>${gauge(i.progress_pct)}<span class="mini mono">${i.progress_pct}%</span></td>
    <td>${esc(i.status ?? "—")}</td>
    <td>${lampFor(i)}</td>
  </tr>`).join("");
  return `<div class="panel panel--chamfer" style="padding:0; overflow:auto; max-height:70vh">
    <table><thead><tr>
      <th>板块</th><th style="min-width:580px">事项</th><th>类型 / 优先级</th><th>计划区间</th><th>进度</th><th>状态</th><th>警示</th>
    </tr></thead><tbody>${rows || '<tr><td colspan="7" class="empty">无匹配项</td></tr>'}</tbody></table>
  </div>`;
}

// ── 视图：时间线（F6：画区间带，月粒度画整月，不编日精度） ──
function viewTimeline() {
  const M0 = 0, M1 = 11; // 2026 年 1–12 月
  const span = M1 - M0 + 1;
  const toX = (iso) => {
    if (!iso) return null;
    const y = +iso.slice(0, 4), m = +iso.slice(5, 7);
    if (y < YEAR) return 0;
    if (y > YEAR) return 100;
    return ((m - 1) / span) * 100;
  };
  const scale = `<div class="tl-scale">${Array.from({ length: span }, (_, k) =>
    `<span>${k + 1}月</span>`).join("")}</div>`;
  const rows = filtered().filter((i) => i.planned_start_date || i.planned_end_date).map((i) => {
    let x0 = toX(i.planned_start_date) ?? 0;
    let x1 = toX(i.planned_end_date) ?? (i.planned_start_date ? x0 : null);
    if (x1 != null && x1 < x0) x1 = x0;
    const w = Math.max(x1 - x0, 100 / span / 2);
    const monthish = i.planned_end_precision === "month" || i.planned_start_precision === "month";
    return `<div class="tl-grid" style="grid-template-columns:220px 1fr; margin-bottom:6px">
      <div class="mini" style="white-space:nowrap;overflow:hidden;text-overflow:ellipsis">${esc(i.title)}</div>
      <div style="position:relative;height:16px">
        <div class="tl-band ${monthish ? "tl-band--month" : ""}"
             data-board="${boardClass(i.board)}" style="position:absolute;left:${x0}%;width:${w}%;background:var(--board-accent)"></div>
      </div>
    </div>`;
  }).join("");
  return `<div class="panel panel--chamfer" style="padding:16px">
    <b>年度时间线（${YEAR}）</b>
    <div class="mini text-2" style="margin:2px 0 12px">月粒度计划画整月区间带（带斜纹），不编造日精度 · 只画有计划区间的项</div>
    ${scale}${rows || '<div class="empty">无带区间的项</div>'}
  </div>`;
}

// ── 视图：KPI（B2） ──
function viewKpi() {
  const rows = state.kpis.map((k) => `<tr>
    <td><b>${esc(k.name)}</b></td>
    <td>${k.target}</td>
    <td>${k.current == null ? '<span class="muted">—（口径未落板块，不估）</span>' : k.current}</td>
    <td class="mini">${esc(k.rule)}</td>
  </tr>`).join("");
  return `<div class="panel panel--chamfer" style="padding:0; overflow:auto">
    <table><thead><tr><th>指标</th><th>目标值</th><th>当前完成（派生）</th><th>口径</th></tr></thead>
    <tbody>${rows || '<tr><td colspan="4" class="empty">未导入</td></tr>'}</tbody></table>
  </div>`;
}

// ── 视图：单位（F8 骨架） ──
function viewUnits() {
  const rows = state.units.map((u) => `<tr>
    <td><b>${esc(u.name)}</b></td>
    <td>${u.item_count}</td>
    <td>${u.open_count}</td>
    <td class="mini">${u.items.map((i) => esc(i.title)).join("；")}</td>
  </tr>`).join("");
  return `<div class="panel panel--chamfer" style="padding:0; overflow:auto; max-height:70vh">
    <table><thead><tr><th>单位</th><th>关联项</th><th>未闭环</th><th>工作项（骨架聚合）</th></tr></thead>
    <tbody>${rows || '<tr><td colspan="4" class="empty">未导入</td></tr>'}</tbody></table>
    <div class="mini" style="padding:8px 12px">F8 首版骨架：档案 + 关联工作项。拜访时间线与合并迁移按 PRD 属二期。</div>
  </div>`;
}

// ── 视图：快速添加（PRD F1 手工录入最小可用形态）──
function viewAdd() {
  const boards = [
    ["gov", "政府关系"], ["industry", "行业资源"], ["media", "媒体品牌"],
    ["event", "活动执行"], ["customer", "客户线索"], ["policy", "政策项目"],
  ];
  return `<div class="panel panel--chamfer" style="padding:16px; max-width:720px">
    <b>快速添加</b>
    <div class="settings-row">
      <span>标题 <span class="mini">（必填）</span></span>
      <input type="text" id="add-title" placeholder="例如：拜访省发改委" style="width:340px">
    </div>
    <div class="settings-row">
      <span>所属板块</span>
      <select id="add-board">
        ${boards.map(([v, z]) => `<option value="${v}">${z}</option>`).join("")}
      </select>
    </div>
    <div class="settings-row">
      <span>计划开始</span>
      <input type="text" id="add-start" placeholder="下月3号" style="width:340px">
    </div>
    <div class="settings-row">
      <span>计划结束</span>
      <input type="text" id="add-end" placeholder="留空即可" style="width:340px">
    </div>
    <div class="settings-row" style="justify-content:flex-end">
      <button class="btn" id="add-go">添加</button>
    </div>
    <div id="add-result" class="mini" style="padding:8px 0"></div>
  </div>`;
}

// ── 视图：设置 ──
// v2.1：多端同步入口已移除（用户放弃多端模式，本地功能离线完整可用）。
// 免打扰 time input 接真实读写（get_prefs/set_prefs → sync_state 键值表）。
function viewSettings() {
  return `<div class="panel panel--chamfer" style="padding:16px; max-width:720px">
    <b>数据</b>
    <div class="settings-row">
      <span>导入存量 Excel</span>
      <span><input type="text" id="imp-path" placeholder="…\\work_progress.xlsx" style="width:280px">
      <button class="btn btn--ghost" id="imp-pick">选择文件…</button>
      <button class="btn" id="imp-go">导入</button></span>
    </div>
    <div class="settings-row">
      <span>立即全量备份</span>
      <button class="btn btn--ghost" id="bk-go">立即备份</button>
    </div>
    <hr class="divider">
    <b>外观</b>
    <div class="settings-row">
      <span>主题</span>
      <select id="theme">
        <option value="dark"${(state.prefs?.theme || "dark") === "dark" ? " selected" : ""}>机甲态（默认）</option>
        <option value="light"${(state.prefs?.theme || "dark") === "light" ? " selected" : ""}>办公态</option>
        <option value="system"${(state.prefs?.theme || "dark") === "system" ? " selected" : ""}>跟随系统</option>
      </select>
    </div>
    <hr class="divider">
    <b>提醒送达</b>
    <div class="settings-row">
      <span>免打扰时段</span>
      <span style="display:flex;gap:8px;align-items:center">
        <input type="time" id="quiet-start">
        –
        <input type="time" id="quiet-end">
      </span>
    </div>
    <div class="settings-row">
      <span class="mini" id="quiet-saved"></span>
    </div>
    <hr class="divider">
    <b>语音播报</b>
    <div class="settings-row">
      <span class="mini">总开关在主界面右上角</span>
    </div>
    <hr class="divider">
    <b>音效反馈</b>
    <div class="settings-row">
      <span>界面音效<span class="mini">（点击/选中/气闸，独立开关，不受语音播报影响）</span></span>
      <label class="switch" title="界面音效：点击、选中、筛选与气闸切换的反馈音">
        <input id="sfx-master" type="checkbox">
      </label>
    </div>
    <div class="settings-row">
      <span>点击音采样<span class="mini">（红警风格样本，可逐个试听）</span></span>
      <span style="display:flex;gap:8px;align-items:center">
        <select id="sfx-sample">
          <option value="tick-a">样本 A · 机械拨杆</option>
          <option value="tick-b">样本 B · 继电器双击</option>
          <option value="tick-c">样本 C · 装甲闷响</option>
        </select>
        <button class="btn btn--ghost" id="sfx-preview">试听</button>
        <span class="mini" id="sfx-saved"></span>
      </span>
    </div>
    <hr class="divider">
    <b>更新</b>
    <div class="settings-row">
      <span>检查更新</span>
      <span style="display:flex;gap:8px;align-items:center">
        <button class="btn btn--ghost" id="upd-go">检查更新</button>
        <span class="mini" id="upd-status"></span>
      </span>
    </div>
  </div>`;
}

/* ── 渲染主循环 · v2.2 气闸开合（UI 规范第四章，替代 v2.1 四阶段 240ms）──
   ① 合盖 160ms（容器 scaleY 收束，内容先行淡出）→ ② 弹出 140ms（中线展开 + 2% 位移补偿）
   → ③ 锁定 60ms（描边 1 次品牌色脉冲）。容器只动 transform/opacity（红线延续）。
   全量逐卡装甲翻入：12ms 交错、交错窗封顶 180ms、rAF 帧率兜底（连续 2 帧 >24ms
   剩余卡直接终态）；will-change 即用即还。打断：任一阶段点新页签立即取消当前
   序列、新目标从头执行完整 360ms（红警「指令即时覆盖」）。旧 --motion-morph 240ms
   与降级路径保留在 tokens.css 作回退。 */
const AIRLOCK_CLOSE_MS = 160;   // --motion-airlock-close
const AIRLOCK_OPEN_MS = 140;    // --motion-airlock-open
const AIRLOCK_LOCK_MS = 60;     // --motion-airlock-lock
const STAGGER_MS = 12;          // --stagger-card
const STAGGER_CAP_MS = 180;     // 交错窗封顶（第 15 张起共享 180ms）
let airlockTimer = 0;
let airlockGen = 0;             // 打断凭据：自增后旧序列的 rAF/超时回调全部失效
let prevView = "brief";

function cancelAirlock(main) {
  clearTimeout(airlockTimer);
  airlockTimer = 0;
  airlockGen++;
  main.classList.remove("airlock-close", "airlock-open", "airlock-lock");
  main.querySelectorAll(".armor-flip").forEach((c) => {
    c.classList.remove("armor-flip");
    c.style.animationDelay = "";
    c.style.willChange = "";
  });
}

/** 全量逐卡装甲翻入（4.2）+ rAF 帧率兜底。axis: 'h'|'v'|''（沿 v2.1 变形轴向参数）。 */
function armorFlipCards(main) {
  const cards = [...main.querySelectorAll(".card")];
  if (!cards.length || reducedMotion()) return; // 降级态：CSS 已把 armor-flip 变 80ms 淡入且延迟清零
  const gen = airlockGen;
  cards.forEach((c, k) => {
    c.style.animationDelay = `${Math.min(k * STAGGER_MS, STAGGER_CAP_MS)}ms`;
    c.classList.add("armor-flip");
    c.addEventListener("animationend", () => {
      // will-change 即用即还：animationend 立即摘除，避免常驻合成层
      c.classList.remove("armor-flip");
      c.style.animationDelay = "";
      c.style.willChange = "";
    }, { once: true });
  });
  let last = performance.now(), slow = 0;
  const deadline = last + AIRLOCK_CLOSE_MS + AIRLOCK_OPEN_MS + STAGGER_CAP_MS;
  const tick = (t) => {
    if (gen !== airlockGen) return; // 序列已被打断：兜底监控随之作废
    const dt = t - last; last = t;
    slow = dt > 24 ? slow + 1 : 0;
    if (slow >= 2) {
      // 帧率兜底：本次剩余未启动的卡片直接呈终态，下次切换重新尝试全量
      cards.forEach((c) => { c.classList.remove("armor-flip"); c.style.animationDelay = ""; c.style.willChange = ""; });
      return;
    }
    if (t < deadline) requestAnimationFrame(tick);
  };
  requestAnimationFrame(tick);
}

/** 变形轴向（3.3）：看板↔时间线=横向 h；看板↔明细=纵向 v；其余视图容器淡入、不逐卡变形。 */
function morphAxis(a, b) {
  const pair = (x, y) => (a === x && b === y) || (a === y && b === x);
  if (pair("kanban", "timeline")) return "h";
  if (pair("kanban", "detail")) return "v";
  return "";
}

const VIEWS = {
  brief: viewBrief, kanban: viewKanban, detail: viewDetail,
  timeline: viewTimeline, kpi: viewKpi, units: viewUnits,
  intel: viewIntel, dealer: viewDealer,
  add: viewAdd, settings: viewSettings,
};

function render(mode) {
  const main = $("#main");
  cancelAirlock(main); // 交互打断（4.4）：立即取消当前序列，新目标从头执行完整 360ms
  main.classList.remove("reflow-fade");

  if (mode === "reflow") {
    // 筛选重排（3.4 沿用）：同视图 ≤120ms 只淡入，不套气闸、不逐卡动画
    main.innerHTML = (VIEWS[state.view] || viewBrief)();
    bindView();
    decoratePanels(main);
    void main.offsetWidth;
    main.classList.add("reflow-fade");
    prevView = state.view;
    return;
  }

  const axis = morphAxis(prevView, state.view);
  const busy = main.innerHTML.trim() !== "";

  const swap = () => {
    main.classList.remove("airlock-close");
    main.innerHTML = (VIEWS[state.view] || viewBrief)();
    bindView();
    decoratePanels(main); // 7.1 面板结构框：1.5px 描边 + 8 颗铆钉
    if (axis) main.dataset.morph = axis; // 逐卡翻入的轴向参数（CSS 选择器用）
    void main.offsetWidth;
    if (!reducedMotion()) sfxServo(true); // ② 弹出音（降级态静音，4.3）
    main.classList.add("airlock-open");
    armorFlipCards(main);
    airlockTimer = setTimeout(() => {
      main.classList.add("airlock-lock"); // ③ 锁定 60ms 品牌色脉冲
      airlockTimer = setTimeout(() => {
        main.classList.remove("airlock-lock", "airlock-open");
        delete main.dataset.morph;
        airlockTimer = 0;
      }, AIRLOCK_LOCK_MS);
    }, AIRLOCK_OPEN_MS);
  };

  if (busy) {
    if (!reducedMotion()) sfxServo(false); // ① 合盖音（伺服下落）
    main.classList.add("airlock-close");
    airlockTimer = setTimeout(swap, AIRLOCK_CLOSE_MS);
  } else {
    swap(); // 首屏无旧形态，直接弹出
  }
  prevView = state.view;
}

/** 跳转到指定视图（通知主操作 / 托盘菜单共用），保持 nav 高亮同步。 */
function gotoView(view) {
  state.view = view;
  document.querySelectorAll("#nav button").forEach((b) =>
    b.setAttribute("aria-current", String(b.dataset.view === view)));
  render();
}

function bindView() {
  if (state.view === "add") {
    $("#add-go")?.addEventListener("click", async () => {
      const title = $("#add-title").value.trim();
      const board = $("#add-board").value;
      const startRaw = $("#add-start").value.trim();
      const endRaw = $("#add-end").value.trim();
      const out = $("#add-result");
      if (!title) { out.textContent = "标题必填。"; return; }
      out.textContent = "正在写入…";
      try {
        const r = await invoke("add_item", {
          title, board,
          planStartRaw: startRaw,
          planEndRaw: endRaw,
          year: YEAR,
        });
        const bits = [`已添加：${r.title ?? title}（${r.id}）`];
        if (r.plan_start_date) {
          bits.push(`开始 ${r.plan_start_date}（${precisionZh(r.plan_start_precision)}）`);
        }
        if (r.plan_end_date) bits.push(`结束 ${r.plan_end_date}（${precisionZh(r.plan_end_precision)}）`);
        if (r.needs_confirm) bits.push("时间待确认，已进待确认队列");
        if (r.queued) bits.push("已留存本地待同步记录（单机版仅本地保存）");
        out.textContent = bits.join(" · ");
        reload();
      } catch (e) {
        out.textContent = "添加失败：" + e;
      }
    });
    return;
  }

  if (state.view === "intel") {
    $("#intel-refresh")?.addEventListener("click", async (e) => {
      e.target.textContent = "采集中…（约 1 分钟，同站间隔≥2s）";
      e.target.disabled = true;
      try { await invoke("intel_refresh"); } catch (err) { alert("启动采集失败：" + err); }
    });
    document.querySelectorAll("[data-verdict-key]").forEach((b) => {
      b.addEventListener("click", async () => {
        try {
          await invoke("intel_set_verdict", { key: b.dataset.verdictKey, verdict: b.dataset.verdictVal });
          await loadIntel();
          render("reflow");
        } catch (err) { alert("改判失败：" + err); }
      });
    });
    return;
  }

  if (state.view === "dealer") {
    $("#dealer-refresh")?.addEventListener("click", async (e) => {
      e.target.textContent = "比对中…";
      e.target.disabled = true;
      try { await invoke("dealer_refresh"); } catch (err) { alert("启动比对失败：" + err); }
    });
    return;
  }

  if (state.view === "settings") {
    const doImport = async (rawPath) => {
      const path = (rawPath || "").trim();
      if (!path) { alert("请先选择要导入的 Excel 文件（.xlsx）。"); return; }
      try {
        const r = await invoke("import_excel", { xlsxPath: path, year: YEAR });
        alert(`导入完成：${r.persist.items_written} 项 / ${r.persist.kpis_written} KPI 落库，回读一致 ${r.persist.items_readback}。`);
        reload();
      } catch (e) {
        alert(`导入失败：${e}\n\n请确认路径存在、文件是 .xlsx 工作表，且表头能被识别。`);
      }
    };
    // v2.2.1：原生文件选择器。此前只有手填路径文本框，路径为空点击直接 return，
    // 用户实机反馈「无法拉起」——静默无反应比报错更让人无从下手。
    const pickExcel = async () => {
      try {
        const dlg = window.__TAURI__?.dialog;
        if (!dlg || typeof dlg.open !== "function") {
          alert("当前环境不支持原生文件选择器，请手动填写文件路径。");
          return null;
        }
        const picked = await dlg.open({
          multiple: false, directory: false, title: "选择要导入的 Excel 工作表",
          filters: [{ name: "Excel 工作表", extensions: ["xlsx", "xlsm"] }],
        });
        if (typeof picked === "string" && picked) return picked;
        if (Array.isArray(picked) && picked.length) return picked[0];
        return null; // 用户取消：无任何副作用
      } catch (e) { alert("打开文件选择器失败：" + e); return null; }
    };
    const pickAndImport = async () => {
      const p = await pickExcel();
      if (!p) return;
      const input = $("#imp-path");
      if (input) input.value = p;
      await doImport(p);
    };
    $("#imp-pick")?.addEventListener("click", pickAndImport);
    $("#imp-go")?.addEventListener("click", async () => {
      const typed = ($("#imp-path")?.value || "").trim();
      if (typed) { await doImport(typed); return; }
      await pickAndImport(); // 没填路径也不再静默：直接拉起选择器
    });
    $("#bk-go")?.addEventListener("click", async () => {
      try {
        const r = await invoke("backup_now");
        alert(`备份完成：${r.bytes} 字节 · integrity ${r.integrity} · ${r.json_tables} 张表 JSON。`);
      } catch (e) { alert("备份失败：" + e); }
    });
    // ── 更新：手动检查（真实下载安装链路，替换旧只读校验）──
    $("#upd-go")?.addEventListener("click", async () => {
      const status = $("#upd-status");
      status.textContent = "检查中…";
      try {
        const r = await invoke("check_update");
        if (!r.available) { status.textContent = "已是最新版本。"; return; }
        status.textContent = `发现新版本 ${r.version}`;
        if (confirm(`发现新版本 ${r.version}，现在下载并安装？\n\n下载完成后应用会重启进入新版。`)) {
          status.textContent = "下载中…（安装完成后自动重启）";
          await invoke("install_update");
        }
      } catch (e) {
        // 检查失败（离线/端点未就绪）如实展示，绝不静默当「已是最新」
        status.textContent = "检查失败（离线时保留当前版本）：" + e;
      }
    });

    // ── 免打扰窗口：打开设置页即回填，修改即保存 ──
    (async () => {
      try {
        state.prefs = await invoke("get_prefs");
        $("#quiet-start").value = state.prefs.quiet_start;
        $("#quiet-end").value = state.prefs.quiet_end;
      } catch (_) { /* 回填失败不阻断设置页 */ }
    })();
    const saveQuiet = async () => {
      const qs = $("#quiet-start").value || "22:00";
      const qe = $("#quiet-end").value || "08:00";
      try {
        const cur = state.prefs || await invoke("get_prefs");
        state.prefs = await invoke("set_prefs", {
          quietStart: qs, quietEnd: qe,
          voiceEnabled: $("#f10-master").checked,
          voicePack: cur.voice_pack === "pack" ? "pack" : "system",
          theme: currentTheme,
          sfxEnabled: cur.sfx_enabled !== false,
          sfxSample: cur.sfx_sample || SFX_DEFAULT_SAMPLE,
        });
        $("#quiet-saved").textContent = `已保存：${state.prefs.quiet_start}–${state.prefs.quiet_end}`;
      } catch (e) {
        $("#quiet-saved").textContent = "保存失败：" + e;
      }
    };
    $("#quiet-start")?.addEventListener("change", saveQuiet);
    $("#quiet-end")?.addEventListener("change", saveQuiet);

    $("#theme")?.addEventListener("change", async (e) => {
      const theme = applyTheme(e.target.value);
      try {
        const cur = state.prefs || await invoke("get_prefs");
        state.prefs = await invoke("set_prefs", {
          quietStart: cur.quiet_start, quietEnd: cur.quiet_end,
          voiceEnabled: cur.voice_enabled,
          voicePack: cur.voice_pack || "system",
          theme, // 原实现这里多写了一个 theme: currentTheme 的重复键，已删除
          sfxEnabled: cur.sfx_enabled !== false,
          sfxSample: cur.sfx_sample || SFX_DEFAULT_SAMPLE,
        });
      } catch (err) { toast("主题未保存（下次启动可能回退）：" + err); }
    });

    // ── v2.2.2（需求15）：音效反馈独立开关 + 点击音采样选择 ──
    // 回填与绑定都在这里做：进入设置页即可看到真实状态，改一下立刻落库。
    (async () => {
      try {
        state.prefs = await invoke("get_prefs");
        syncSfxUi();
      } catch (_) { /* 回填失败不阻断设置页 */ }
    })();
    const saveSfx = async () => {
      const master = $("#sfx-master");
      const sample = $("#sfx-sample");
      if (!master || !sample) return;
      try {
        const cur = state.prefs || await invoke("get_prefs");
        state.prefs = await invoke("set_prefs", {
          quietStart: cur.quiet_start, quietEnd: cur.quiet_end,
          voiceEnabled: cur.voice_enabled,
          voicePack: cur.voice_pack || "system",
          theme: currentTheme,
          sfxEnabled: master.checked,
          sfxSample: sample.value,
        });
        $("#sfx-saved").textContent = `已保存：${state.prefs.sfx_enabled ? "开" : "关"} · ${state.prefs.sfx_sample}`;
      } catch (e) {
        $("#sfx-saved").textContent = "保存失败：" + e;
      }
    };
    $("#sfx-master")?.addEventListener("change", saveSfx);
    $("#sfx-sample")?.addEventListener("change", saveSfx);
    // 试听：无视当前开关状态先响一声，让用户直接比较三个采样（改完不存偏好）
    $("#sfx-preview")?.addEventListener("click", () => {
      const sample = $("#sfx-sample")?.value || SFX_DEFAULT_SAMPLE;
      playSfx("tick", { sample, volume: 1, forcePlay: true });
    });

  }
}

/** 把偏好里的音效状态回填到设置页控件（页面重新渲染后也要重新调用）。 */
function syncSfxUi() {
  const master = $("#sfx-master");
  const sample = $("#sfx-sample");
  const prefs = state.prefs || {};
  if (master) master.checked = prefs.sfx_enabled !== false; // 缺省开启
  if (sample) sample.value = SFX_CATALOG.tick.includes(prefs.sfx_sample) ? prefs.sfx_sample : SFX_DEFAULT_SAMPLE;
}

/** 把计划精度翻成人话，避免界面直出 day/month/quarter 这类枚举值。 */
function precisionZh(p) {
  return ({ day: "日精度", month: "月精度", quarter: "季精度", ongoing: "长期", unknown: "待确认" })[p] ?? (p || "—");
}

/* ── v2.2.2（需求15）：交互音效改为「可替换音频采样 + 独立开关」 ──
   原实现是 WebAudio 现场合成（beep / uiTick / sfxAlert / sfxServo 硬编码频率），
   用户要求的红警采样风格无法替换、且与语音播报共用 voice_enabled 总开关
   （关掉语音播报连点击反馈音一起消失）。改法：
   ① 所有音效走 SFX_CATALOG 里的采样文件（apps/desktop/src/sfx/*.wav），
      换风格只改目录 + 首选项里的样本 id，不动一行播放逻辑；
   ② 独立开关 sfx_enabled（数据库 sync_state 键，与 voice_enabled 完全解耦）；
   ③ 采样缺失/播放失败一律静默跳过，绝不阻断交互。
   红警具体采样风格由用户挑选（候选 tick-a / tick-b / tick-c 随安装包提供）。 */
const SFX_DIR = "./sfx/";
const SFX_CATALOG = {
  tick: ["tick-a", "tick-b", "tick-c"], // 点击/选中/筛选：机械咔哒，可换
  alert1: ["alert-1"],                   // 警示入场首音
  alert2: ["alert-2"],                   // 警示入场次音（110ms 后）
  airlockClose: ["airlock-close"],       // 气闸 ① 合盖
  airlockOpen: ["airlock-open"],         // 气闸 ② 弹出
};
const SFX_DEFAULT_SAMPLE = "tick-a";
/** 独立音效开关：只看 sfx_enabled，不再回落到语音播报总开关。 */
function sfxOn() { return !!(state.prefs && state.prefs.sfx_enabled); }
/** 点击音样本 id：偏好里存的是 tick-a / tick-b / tick-c，非法值回落默认。 */
function tickSample() {
  const id = state.prefs && state.prefs.sfx_sample;
  return SFX_CATALOG.tick.includes(id) ? id : SFX_DEFAULT_SAMPLE;
}
/** 采样播放：每次都新建 Audio 实例，允许同一音效连点重叠；preload 减少首击延迟。 */
function playSfx(name, opts) {
  const o = opts || {};
  if (!sfxOn() && !o.forcePlay) return null;
  if (o.respectReducedMotion && reducedMotion()) return null;
  const sample = o.sample || (SFX_CATALOG[name] || [])[0];
  if (!sample) return null;
  try {
    const a = new Audio(`${SFX_DIR}${sample}.wav`);
    a.volume = o.volume == null ? 1 : o.volume;
    const p = a.play();
    if (p && p.catch) p.catch(() => { /* 自动播放策略/无音频设备/文件缺失：静默 */ });
    return a;
  } catch (_) { return null; } // 构造失败绝不阻断交互
}
/** 点击反馈音（导航/选中/筛选）。 */
function uiTick() { playSfx("tick", { sample: tickSample() }); }
/** 警示卡入场：短双音，110ms 间隔（受 4.4 疲劳控制：每卡每会话 1 次）。 */
function sfxAlert() {
  playSfx("alert1");
  setTimeout(() => playSfx("alert2"), 110);
}
/** v2.2 气闸切换音：合盖 = 下落采样、弹出 = 上扬采样；降级态视为静音。 */
function sfxServo(up) {
  playSfx(up ? "airlockOpen" : "airlockClose", { respectReducedMotion: true });
}

/* RA 单位选中：点击/回车卡片 → 四角括号选中 + 咔哒音，再点取消；一次一张主选。
   #main 是常驻节点，事件委托一次即可，重渲染后自动作用于新卡片。 */
let selectedCard = null;
$("#main").addEventListener("click", (e) => {
  if (e.target.closest("button, a, input, select, textarea")) return;
  const card = e.target.closest(".card");
  if (!card) return;
  if (selectedCard === card) {
    card.classList.remove("card--selected");
    selectedCard = null;
  } else {
    if (selectedCard) selectedCard.classList.remove("card--selected");
    card.classList.add("card--selected");
    selectedCard = card;
  }
  uiTick();
});
$("#main").addEventListener("keydown", (e) => {
  if (e.key !== "Enter" && e.key !== " ") return;
  const card = e.target && e.target.closest ? e.target.closest(".card") : null;
  if (card) { e.preventDefault(); card.click(); }
});

// ── 导航与筛选绑定 ──
$("#nav").addEventListener("click", async (e) => {
  const btn = e.target.closest("button[data-view]");
  if (!btn) return;
  // v2.2：导航反馈音升级为气闸合盖/弹出伺服音（4.3），不再叠加 uiTick
  state.view = btn.dataset.view;
  document.querySelectorAll("#nav button").forEach((b) =>
    b.setAttribute("aria-current", String(b === btn)));
  // FR-2/FR-1：进入情报/网点视图时按需拉取（离线失败只影响本视图）
  if (state.view === "intel") await loadIntel();
  if (state.view === "dealer") await loadDealer();
  render();
});
document.querySelectorAll(".chip").forEach((chip) => {
  chip.addEventListener("click", () => {
    if (chip.dataset.filter === "board") {
      state.filterBoard = chip.value;
      uiTick();
      document.querySelectorAll('.chip[data-filter="board"]').forEach((c) =>
        c.setAttribute("data-active", String(c === chip)));
      updateFilterCount();
      render("reflow");
    }
  });
});
$("#filter-clear").addEventListener("click", () => {
  state.filterBoard = "";
  uiTick();
  document.querySelectorAll('.chip[data-filter="board"]').forEach((c) =>
    c.setAttribute("data-active", String(!c.value)));
  updateFilterCount();
  render("reflow");
});

// B7 补推：启动时检查上次未送达的晨报
(async function boot() {
  // v2.2.1：业务年度以后端 wb_core::FISCAL_YEAR 为准（默认 2027），
  // 看板 / 明细 / 年度时间线 / 晨报用同一个年度，避免前后端各写一份。
  try { YEAR = await invoke("fiscal_year"); } catch (_) { /* 后端不可用则沿用默认值 */ }
  try {
    const pending = await invoke("get_pending_brief");
    // v2.2 FR-5：补推不再用横幅，入字幕队列（AC-5-1 数据来源沿用）
    if (pending && pending.pending) {
      enqueueNotice({
        key: `brief-pending-${pending.pending_date || "today"}`,
        level: "high",
        title: "晨报待补发",
        text: `◆ 有未送达的晨报（${pending.pending_date || ""}），可点击查看`,
        action: { label: "查看晨报", run: () => gotoView("brief") },
      });
    }
  } catch (_) { /* 无记录即正常 */ }
  // F10 总开关 + v2.2 语音包开关初始化：从偏好库读真实状态，不再写死 disabled
  try {
    state.prefs = await invoke("get_prefs");
    // v2.2.1：主题回填。index.html 已预置 data-theme="dark"（打开即机甲态，
    // 兼顾避免亮色闪屏），这里按用户已存偏好纠正（办公态/跟随系统）。
    applyTheme(state.prefs?.theme || "dark");
    const sw = $("#f10-master");
    sw.disabled = false;
    sw.checked = !!state.prefs.voice_enabled;
    const vp = $("#voice-pack");
    vp.disabled = false;
    vp.checked = state.prefs.voice_pack === "pack";
    const savePrefs = async () => {
      try {
        const cur = state.prefs || await invoke("get_prefs");
        state.prefs = await invoke("set_prefs", {
          quietStart: cur.quiet_start, quietEnd: cur.quiet_end,
          voiceEnabled: sw.checked,
          voicePack: vp.checked ? "pack" : "system",
          theme: currentTheme,
          sfxEnabled: $("#sfx-master")?.checked ?? cur.sfx_enabled !== false,
          sfxSample: cur.sfx_sample || SFX_DEFAULT_SAMPLE,
        });
        // 音效开关就地生效（下一声立刻按新状态判定），不必等下次启动。
        syncSfxUi();
      } catch (_) { /* 写失败时开关状态可能不同步，下次启动回读纠偏 */ }
    };
    sw.addEventListener("change", savePrefs);
    vp.addEventListener("change", savePrefs);
  } catch (_) { /* 偏好读取失败：开关保持 disabled，不误播报 */ }
  bindShellEvents();
  bindMarquee();
  bindCmdBar();
  reload();
})();

/**
 * 壳 → 前端的事件绑定（B7 晨报 / F10 播报 / 自动更新）。
 * 这些事件是桌面壳在 setup / 调度线程里 emit 出来的，前端不订阅就等于
 * 晨报送达了界面也不知道、托盘点了「查看晨报」也不跳转。
 */
async function bindShellEvents() {
  if (!window.__TAURI__?.event) return; // 浏览器里直接打开时安静降级
  const { listen } = window.__TAURI__.event;

  // 调度线程在启动时发现昨天/今天有未送达的晨报 → 入字幕队列（FR-5）
  await listen("brief-pending", (e) => {
    const d = e.payload || {};
    const dt = d.brief_date || "";
    enqueueNotice({
      key: `brief-pending-${dt || "today"}`,
      level: "high",
      title: "晨报待补发",
      text: `◆ 有未送达的晨报（${dt}），可点击查看`,
      action: { label: "查看晨报", run: () => gotoView("brief") },
    });
  });

  // 晨报已送达 → 字幕播报 + 刷新晨报页 + 按偏好语音播报要点。
  // FR-6：版本即时送达的晨报 payload 带 new_version=true，字幕文案标注「新版本」。
  // F10：免打扰时段只通知不播报；开关实时跟随右上角总开关。
  await listen("brief-delivered", (e) => {
    const d = e.payload || {};
    const vTag = d.new_version ? "（新版本）" : "";
    enqueueNotice({
      key: `brief-delivered-${d.date || "today"}`,
      level: "high",
      title: `晨报${vTag}已送达`,
      text: d.new_version
        ? `◆ 检测到版本更新 ${d.version_from || "?"} → ${d.version_to || "?"}，晨报已即时送达`
        : `◆ 今日晨报已送达，可点击查看`,
      action: { label: "查看晨报", run: () => gotoView("brief") },
    });
    if (state.view === "brief") reload();
    if (d.voice_enabled && !d.notification_suppressed && $("#f10-master").checked && d.speech) {
      // v2.2 语音包场景推导：新版本即时晨报 → S1；迟到补发 → S2；
      // 周一带周聚合 → S3；其余 → S1 晨报准点。
      const scene = d.new_version ? "version_boot"
        : (d.late_minutes > 0 ? "brief_late"
          : (d.counts && d.counts.weekly ? "weekly" : "brief"));
      speakBrief(d.speech, scene);
    }
  });

  // 送达失败：严重级字幕 + 红灯带，不静默
  await listen("brief-error", (e) => {
    enqueueNotice({
      key: `brief-error-${Date.now()}`,
      level: "high",
      severe: true,
      title: "晨报送达失败",
      text: `◆ 晨报送达失败：${e.payload ?? "未知原因"}`,
    });
  });

  // 托盘菜单「查看晨报」→ 切到晨报页
  await listen("brief-open", () => gotoView("brief"));

  // FR-2 情报日采集完成 → 字幕摘要「今日新增相关项目 N 条」（PRD：不把正文塞进通知）
  await listen("intel-updated", (e) => {
    const d = e.payload || {};
    enqueueNotice({
      key: `intel-${d.manual ? "manual-" : ""}${new Date().toISOString().slice(0, 10)}`,
      level: d.failed > 0 ? "normal" : "normal",
      severe: false,
      title: "项目情报已更新",
      text: `◆ ${d.summary || "情报采集完成"}`,
      action: { label: "查看情报", run: async () => { await loadIntel(); gotoView("intel"); } },
    });
    if (state.view === "intel") { loadIntel().then(() => render("reflow")); }
  });

  // FR-1 网点周比对完成
  await listen("dealer-updated", (e) => {
    const d = e.payload || {};
    if (d.ok === false) {
      enqueueNotice({
        key: `dealer-fail-${Date.now()}`,
        level: "high",
        severe: true,
        title: "网点比对失败",
        text: `◆ 极飞网点比对失败：${d.status} ${d.detail || ""}`,
      });
      return;
    }
    enqueueNotice({
      key: `dealer-ok-${new Date().toISOString().slice(0, 10)}`,
      level: "normal",
      title: "网点比对完成",
      text: "◆ 极飞安徽网点比对已完成，可到「网点」页查看增减。",
      action: { label: "查看网点", run: async () => { await loadDealer(); gotoView("dealer"); } },
    });
    if (state.view === "dealer") { loadDealer().then(() => render("reflow")); }
  });

  // 启动静默检查发现新版 → 字幕通知 + 指令条确认（44px 底部指令条，UI 规范 7.5）
  await listen("update-available", (e) => {
    const d = e.payload || {};
    enqueueNotice({
      key: `update-${d.version}`,
      level: "normal",
      title: "发现新版本",
      text: `◆ 发现新版本 v${d.version}，可以更新。`,
      action: {
        label: "下载并安装",
        run: async () => {
          const ok = await commandBar(
            "更新确认",
            `下载并安装 v${d.version}？下载完成后应用会重启进入新版。`,
          );
          if (!ok) return;
          try {
            await invoke("install_update");
          } catch (err) {
            enqueueNotice({
              key: `update-fail-${d.version}`,
              level: "high",
              severe: true,
              title: "更新失败",
              text: `◆ 更新安装失败：${err}`,
            });
          }
        },
      },
    });
    // v2.2 语音包 S8（银刃）：更新提示语音。口径与晨报一致——
    // 免打扰时段只滚动不播报、跟随 F10 总开关。
    if (state.prefs?.voice_enabled && !inQuietNow() && $("#f10-master").checked) {
      speakBrief(`发现新版本 v${d.version}，可以更新。`, "update");
    }
  });
}


/* ══════════════════════════════════════════════════════════════════════
   v2.2 FR-5 首行滚动字幕 + 通知中心 + 底部指令条（装甲指挥舱 UI 规范六/7.4/7.5）
   ──────────────────────────────────────────────────────────────────────
   队列语义（PRD FR-5 + UI 规范 6.3，已确认口径）：
   - 播放队列 ≤5（--marquee-queue-cap）；业务键去重（后到覆盖先到）；
     严重级（业务失败）插队——当前条播完即接；普通级 FIFO。
   - 溢出条目不丢：进通知中心（铃铛入口，未读角标），历史封顶 50 条防内存增长
     （AC：长时间运行内存占用不随队列增长）。
   - 免打扰（22:00–8:00，口径同后端 quiet 偏好）：非高优先级不滚动、队列保留
     （AC-5-6）；高优先级照滚但不出声（语音抑制沿用 F10 既有口径）。
   - 队列空整条隐藏（AC-5-5）；「无消息循环播系统状态摘要」属 UI 规范中
     标注「需用户确认」的条目，未获确认，不实现。
   - 交互（UI 规范 6.2 决策卡，用户已确认）：悬停即停（保存位移续播）/
     点击开详情条（88px/120ms）/ Esc 与点空白收起 / 主操作执行后本条出队。
   - 无障碍红线：滚动容器 aria-hidden，每条入队写一次 aria-live=polite；
     灯带红/绿变化附 ▲ 徽标非颜色冗余；字幕条目键盘可达（Tab+Enter）。
   ══════════════════════════════════════════════════════════════════════ */
const MARQUEE_SPEED = 40;      // px/s（--marquee-speed）
const MARQUEE_CAP = 5;         // --marquee-queue-cap
const MARQUEE_DWELL_MS = 700;  // 单条播完停一小段再接下一条（PRD「滚完一轮停一小段」）
const HISTORY_CAP = 50;

const notice = {
  queue: [],      // {key, level:'high'|'normal', severe, text, detail, action:{label,run}, ts}
  history: [],    // 通知中心（含已播过/溢出的，≤50）
  idx: 0,         // 当前播放下标
  dwellTimer: 0,
  blinkTimer: 0,
  detailKey: null, // 详情条当前展示的条目 key（只允许是还在队列里的条目）
};

const reducedMotion = () => window.matchMedia("(prefers-reduced-motion: reduce)").matches;

/** 免打扰判定（口径与后端 brief::QuietHours 一致：闭开区间、可跨零点）。 */
function inQuietHours() {
  const p = state.prefs;
  if (!p || !p.quiet_start || !p.quiet_end) return false;
  const [sh, sm] = String(p.quiet_start).split(":").map(Number);
  const [eh, em] = String(p.quiet_end).split(":").map(Number);
  if ([sh, sm, eh, em].some(Number.isNaN)) return false;
  const now = new Date();
  const m = now.getHours() * 60 + now.getMinutes();
  const start = sh * 60 + sm, end = eh * 60 + em;
  return start < end ? (m >= start && m < end) : (m >= start || m < end);
}

/** 入队。n = {key, level, severe, text, detail, action}。
 *  level: 'high'（晨报类，免打扰照滚）| 'normal'（更新/导入，免打扰不滚）。
 *  severe: 业务失败（插队 + 红灯带 + ▲ 徽标）。 */
function enqueueNotice(n) {
  if (!n || !n.key) return;
  const item = { level: "normal", severe: false, ...n, ts: Date.now() };
  const qi = notice.queue.findIndex((q) => q.key === item.key);
  if (qi >= 0) {
    notice.queue[qi] = item; // 业务键去重：后到覆盖先到，保留原位
  } else {
    if (notice.queue.length >= MARQUEE_CAP) {
      // 溢出：先丢一条最低优先级（最早的普通级；全高优时丢最早非严重）腾位
      let dropIdx = notice.queue.findIndex((q) => !q.severe && q.level !== "high");
      if (dropIdx < 0) dropIdx = notice.queue.findIndex((q) => !q.severe);
      if (dropIdx < 0) dropIdx = 0;
      notice.queue.splice(dropIdx, 1);
      if (notice.idx > dropIdx) notice.idx--;
    }
    if (item.severe) {
      // 严重级插队：当前条播完即接
      notice.queue.splice(Math.min(notice.idx + 1, notice.queue.length), 0, item);
    } else {
      notice.queue.push(item);
    }
  }
  // 通知中心历史：去重覆盖、未读重计、封顶 50
  const hi = notice.history.findIndex((h) => h.key === item.key);
  const rec = { ...item, read: false };
  if (hi >= 0) notice.history[hi] = rec; else notice.history.unshift(rec);
  if (notice.history.length > HISTORY_CAP) notice.history.length = HISTORY_CAP;
  // 无障碍：入队即向 aria-live 区播报一次
  const live = $("#mq-live");
  if (live) live.textContent = item.text;
  blinkMarqueeLamp();
  renderNoticeCenter();
  marqueeSchedule();
}

/** 灯带闪 2 次（200ms 亮/灭 ×2）；严重级入队后 updateLamp 会转常亮红。 */
function blinkMarqueeLamp() {
  const bar = $("#marquee-bar");
  if (!bar) return;
  bar.classList.remove("mq-blink-fx");
  void bar.offsetWidth;
  bar.classList.add("mq-blink-fx");
  clearTimeout(notice.blinkTimer);
  notice.blinkTimer = setTimeout(() => bar.classList.remove("mq-blink-fx"), 900);
}

/** 灯带常态：有严重级在队/在播 → 红常亮 + ▲ 徽标；否则绿。 */
function updateMarqueeLamp() {
  const bar = $("#marquee-bar");
  if (!bar) return;
  const severe = notice.queue.some((q) => q.severe);
  bar.classList.toggle("mq-severe", severe);
  const badge = $("#mq-alert-badge");
  if (badge) badge.hidden = !severe;
}

/** 播放调度：队列空隐藏（AC-5-5）；免打扰跳过非高优（AC-5-6，队列保留）。 */
function marqueeSchedule() {
  clearTimeout(notice.dwellTimer);
  const bar = $("#marquee-bar");
  if (!bar) return;
  if (!notice.queue.length) {
    bar.hidden = true;
    closeMarqueeDetail();
    updateMarqueeLamp();
    return;
  }
  if (notice.idx >= notice.queue.length) notice.idx = 0;
  if (inQuietHours()) {
    // 免打扰：跳到下一条高优先级；全被抑制则停播（bar 隐藏，队列保留）
    let i = notice.idx;
    while (i < notice.queue.length && notice.queue[i].level !== "high") i++;
    if (i >= notice.queue.length) {
      bar.hidden = true;
      updateMarqueeLamp();
      return; // 次日 8:00 后由 30s 心跳 marqueeSchedule 自动恢复
    }
    notice.idx = i;
  }
  bar.hidden = false;
  updateMarqueeLamp();
  const cur = notice.queue[notice.idx];
  // v2.2.1：详情条只展示**当前队列条目**。队列推进/出队后自动收起——
  // 否则会出现「字幕播着网点比对、详情条却挂着已出队的情报新增」的错位观感。
  if (notice.detailKey && notice.detailKey !== cur.key) closeMarqueeDetail();
  playMarqueeItem(cur, notice.idx);
}

/** 播放单条：40px/s 匀速横滚，时长按（视口宽+文本宽）/速度换算。 */
function playMarqueeItem(item, idx) {
  const track = $("#mq-track");
  const pos = $("#mq-pos");
  const viewport = $("#mq-viewport");
  if (!track || !viewport) return;
  pos.textContent = `${idx + 1}/${notice.queue.length}`;
  track.textContent = item.text;
  viewport.setAttribute("aria-label", `通知 ${idx + 1}/${notice.queue.length}：${item.text}，按回车查看详情`);
  track.classList.remove("mq-run");
  void track.offsetWidth; // 重启动画
  if (reducedMotion()) {
    track.classList.add("mq-run"); // 降级态：CSS 停滚静态显示
    return;
  }
  const vw = viewport.clientWidth;
  const tw = track.scrollWidth;
  const dur = Math.max((vw + tw) / MARQUEE_SPEED, 1.5);
  track.style.setProperty("--mq-dur", `${dur}s`);
  track.style.setProperty("--mq-from", `${vw}px`);
  track.style.setProperty("--mq-to", `${-tw}px`);
  track.classList.add("mq-run");
}

/** 出队（主操作执行后 / 手动移除）：同步 1/N 计数与通知中心未读。 */
function removeNotice(key) {
  const i = notice.queue.findIndex((q) => q.key === key);
  if (i >= 0) {
    notice.queue.splice(i, 1);
    if (notice.idx > i) notice.idx--;
    if (notice.idx >= notice.queue.length) notice.idx = 0;
  }
  const h = notice.history.find((x) => x.key === key);
  if (h) h.read = true;
  if (notice.detailKey === key) closeMarqueeDetail(); // 出队即收起详情条
  renderNoticeCenter();
  marqueeSchedule();
}

/** 详情条（6.2）：当前条完整内容 + 主操作；展开期间滚动暂停。 */
function openMarqueeDetail(item) {
  const det = $("#mq-detail");
  const bar = $("#marquee-bar");
  if (!det || !bar || !item) return;
  notice.detailKey = item.key; // 记录详情条展示的是哪一条（队列变化时收起）
  det.innerHTML = `
    <div class="mq-detail-body">${esc(item.detail || item.text)}</div>
    <div class="mq-detail-actions">
      ${item.action ? `<button class="btn" id="mq-act">${esc(item.action.label)}</button>` : ""}
      <button class="btn--ghost btn" id="mq-dismiss">移除本条</button>
    </div>`;
  det.hidden = false;
  bar.classList.add("mq-open");
  $("#mq-act")?.addEventListener("click", async () => {
    closeMarqueeDetail();
    try { if (item.action) await item.action.run(); } catch (_) { /* 动作失败：条目留队，不吞错误页 */ }
    removeNotice(item.key);
  });
  $("#mq-dismiss")?.addEventListener("click", () => {
    closeMarqueeDetail();
    removeNotice(item.key);
  });
  $("#mq-act")?.focus();
}
function closeMarqueeDetail() {
  const det = $("#mq-detail");
  const bar = $("#marquee-bar");
  if (det) det.hidden = true;
  if (bar) bar.classList.remove("mq-open");
  notice.detailKey = null;
}

/** 通知中心（7.5）：铃铛 + 未读角标 + 320px 下拉历史列表。 */
function renderNoticeCenter() {
  const badge = $("#nc-badge");
  const unread = notice.history.filter((h) => !h.read).length;
  if (badge) {
    badge.hidden = unread === 0;
    badge.textContent = String(Math.min(unread, 99));
  }
  const panel = $("#nc-panel");
  if (!panel || panel.hidden) return;
  const fmtTime = (ts) => {
    const d = new Date(ts);
    const p2 = (x) => String(x).padStart(2, "0");
    return `${p2(d.getMonth() + 1)}-${p2(d.getDate())} ${p2(d.getHours())}:${p2(d.getMinutes())}`;
  };
  panel.innerHTML = notice.history.length
    ? notice.history.map((h) => `
      <div class="nc-item" data-key="${esc(h.key)}" tabindex="0" role="button">
        <span class="nc-time">${fmtTime(h.ts)}</span><span class="nc-badge-lv nc-lv-${h.severe ? "severe" : h.level}">${h.severe ? "▲严重" : h.level === "high" ? "高" : "普通"}</span>${h.read ? "" : " ●"}
        <span class="nc-text">${esc(h.text)}</span>
      </div>`).join("")
    : `<div class="nc-empty">暂无历史通知</div>`;
  panel.querySelectorAll(".nc-item").forEach((el) => {
    const open = () => {
      const h = notice.history.find((x) => x.key === el.dataset.key);
      if (!h) return;
      h.read = true;
      renderNoticeCenter();
      const q = notice.queue.find((x) => x.key === h.key);
      // v2.2.1：历史里**已出队**的条目不再当「当前通知」打开。此前的 `|| h` 兜底
      // 会把旧通知塞进详情条，与字幕正在播的条目对不上——观感像旧横幅残留。
      if (!q) { toast("该通知已处理"); return; }
      openMarqueeDetail(q);
    };
    el.addEventListener("click", open);
    el.addEventListener("keydown", (e) => { if (e.key === "Enter" || e.key === " ") { e.preventDefault(); open(); } });
  });
}

function bindMarquee() {
  const bar = $("#marquee-bar");
  const viewport = $("#mq-viewport");
  if (!bar || !viewport) return;
  // 点击字幕条目 → 开详情条（UI 规范 6.2 已确认交互；主操作跳转/执行后本条出队，
  // 对应 PRD AC-5-4 的「跳转 + 移除」语义经详情条主操作完成）
  viewport.addEventListener("click", () => {
    if (notice.queue.length) openMarqueeDetail(notice.queue[notice.idx]);
  });
  viewport.addEventListener("keydown", (e) => {
    if (e.key === "Enter" || e.key === " ") {
      e.preventDefault();
      if (notice.queue.length) openMarqueeDetail(notice.queue[notice.idx]);
    }
  });
  // 单条滚完 → 停一小段接下一条
  $("#mq-track")?.addEventListener("animationend", (e) => {
    if (e.target.id !== "mq-track" || e.animationName !== "marquee-scroll") return;
    notice.idx = (notice.idx + 1) % notice.queue.length;
    closeMarqueeDetail(); // 字幕推进到下一条 → 详情条收起（只展示当前队列条目）
    notice.dwellTimer = setTimeout(marqueeSchedule, MARQUEE_DWELL_MS);
  });
  // Esc / 点击空白收起详情并恢复滚动
  document.addEventListener("keydown", (e) => {
    if (e.key === "Escape") closeMarqueeDetail();
  });
  document.addEventListener("click", (e) => {
    if (!$("#marquee-bar")?.classList.contains("mq-open")) return;
    if (e.target.closest("#marquee-bar")) return;
    closeMarqueeDetail();
  });
  // 通知中心铃铛
  const ncBtn = $("#nc-btn");
  const ncPanel = $("#nc-panel");
  ncBtn?.addEventListener("click", (e) => {
    e.stopPropagation();
    ncPanel.hidden = !ncPanel.hidden;
    ncBtn.setAttribute("aria-expanded", String(!ncPanel.hidden));
    if (!ncPanel.hidden) {
      notice.history.forEach((h) => { h.read = true; });
      renderNoticeCenter();
    }
  });
  document.addEventListener("click", (e) => {
    if (!ncPanel || ncPanel.hidden) return;
    if (e.target.closest("#nc-btn")) return;
    ncPanel.hidden = true;
    ncBtn?.setAttribute("aria-expanded", "false");
  });
  // 免打扰边界自动恢复（AC-5-6 次日 8:00 后自动开始滚动）+ 队列兜底心跳
  setInterval(() => {
    if (!notice.queue.length) return;
    if ($("#marquee-bar").hidden) marqueeSchedule();
  }, 30_000);
}

/* ── 7.4 底部指令条：破坏性/高影响操作确认（44px，8s 超时，Enter/Esc 全键盘）── */
let cmdTimer = 0, cmdResolve = null;
function commandBar(opName, summary) {
  return new Promise((resolve) => {
    const bar = $("#cmd-bar");
    if (!bar) { resolve(window.confirm(`确认${opName}？\n${summary}`)); return; }
    $("#cmd-text").innerHTML = `确认${esc(opName)}？<br><span class="mini">${esc(summary)}</span>`;
    bar.classList.remove("cmd-in", "cmd-out");
    bar.hidden = false;
    void bar.offsetWidth;
    bar.classList.add("cmd-in");
    clearTimeout(cmdTimer);
    cmdResolve = resolve;
    $("#cmd-ok").focus();
    cmdTimer = setTimeout(() => finishCmdBar(false), 8000); // --bar-timeout
  });
}
function finishCmdBar(ok) {
  const bar = $("#cmd-bar");
  if (!bar || bar.hidden) return;
  clearTimeout(cmdTimer);
  bar.classList.remove("cmd-in");
  if (reducedMotion()) {
    bar.hidden = true;
  } else {
    bar.classList.add("cmd-out");
    setTimeout(() => { bar.hidden = true; bar.classList.remove("cmd-out"); }, 120);
  }
  if (cmdResolve) { cmdResolve(ok); cmdResolve = null; }
}
function bindCmdBar() {
  $("#cmd-ok")?.addEventListener("click", () => finishCmdBar(true));
  $("#cmd-cancel")?.addEventListener("click", () => finishCmdBar(false));
  document.addEventListener("keydown", (e) => {
    const bar = $("#cmd-bar");
    if (!bar || bar.hidden) return;
    if (e.key === "Enter" && document.activeElement?.closest("#cmd-bar")) { e.preventDefault(); finishCmdBar(true); }
    if (e.key === "Escape") { e.preventDefault(); finishCmdBar(false); }
    if (e.key === "Tab") { // FocusTrap：焦点锁定在指令条双按钮内循环
      e.preventDefault();
      const btns = [$("#cmd-ok"), $("#cmd-cancel")];
      const cur = btns.indexOf(document.activeElement);
      btns[(cur + (e.shiftKey ? btns.length - 1 : 1)) % btns.length]?.focus();
    }
  });
}

/* ── 7.1 面板结构框装饰：1.5px 描边 + 8 颗 aria-hidden 铆钉（每次渲染后追加）── */
const RIVET_POS = ["tl", "tr", "bl", "br", "tc", "bc", "ml", "mr"];
function decoratePanels(root) {
  (root || document).querySelectorAll(".panel:not(.panel--frame)").forEach((p) => {
    p.classList.add("panel--frame");
    RIVET_POS.forEach((pos) => {
      const r = document.createElement("span");
      r.className = "rivet";
      r.dataset.pos = pos;
      r.setAttribute("aria-hidden", "true");
      p.appendChild(r);
    });
  });
}
/** 指令脉冲（7.1）：确认执行后对应面板灯带闪 1 次。 */
function pulsePanel(panel) {
  if (!panel) return;
  panel.classList.remove("panel--pulse");
  void panel.offsetWidth;
  panel.classList.add("panel--pulse");
  setTimeout(() => panel.classList.remove("panel--pulse"), 300);
}

/** 免打扰窗口判定（前端侧，用于 update 等前端直发事件的语音抑制；
 * 晨报链路由后端 notification_suppressed 判定，两端口径一致：HH:MM 跨午夜回绕）。
 * 偏好未读取/格式异常时按「不在免打扰」处理（宁可多播不静默丢提示）。 */
function inQuietNow() {
  const p = state.prefs;
  if (!p || !p.quiet_start || !p.quiet_end) return false;
  const [sh, sm] = String(p.quiet_start).split(":").map(Number);
  const [eh, em] = String(p.quiet_end).split(":").map(Number);
  if ([sh, sm, eh, em].some((x) => !Number.isFinite(x))) return false;
  const now = new Date();
  const cur = now.getHours() * 60 + now.getMinutes();
  const s = sh * 60 + sm;
  const e = eh * 60 + em;
  return s <= e ? (cur >= s && cur < e) : (cur >= s || cur < e);
}

/** F10 语音播报：v2.2 语音包 · 按场景选包。
 * 两档模式（voice_pack 偏好键，sync_state 键值表为唯一事实源）：
 * - "system"（默认）：WebView2 speechSynthesis 选系统 zh 语音朗读实时文案；
 * - "pack"：播 2026-10-28 定稿样音（S1–S9 场景映射见 VOICE_PACK_SCENES）。
 *   C 档（破阵·浑厚版）金属电子音系统 TTS 无法合成，必须预录 WAV 随包分发
 *   （apps/desktop/src/voice/，16bit/40kHz/立体声，9 条共约 10.3MB）。
 * 边界（voice.md 第六章）：定稿样音验证的是音色，内容为示例句；
 * 实时动态数字以字幕与界面文案为准——pack 模式下字幕/通知照发真实文案。
 * 播放失败（资源缺失/被策略拦截）回落系统 TTS，语音失败绝不阻断主流程。
 * 红线：不做真人声纹克隆；样音均为合成音色。 */
const VOICE_PACK_SCENES = {
  brief: "s1_brief",           // S1 晨报准点（破阵·浑厚版）
  version_boot: "s1_brief",    // FR-6 新版本即时晨报：内容仍是晨报 → S1 音色
  brief_late: "s2_brief_late", // S2 晨报迟到补发（银刃）
  weekly: "s3_weekly",         // S3 周一·周聚合（破阵·浑厚版）
  overdue: "s4_overdue",       // S4 逾期提醒（破阵·浑厚版；触发链路待后续版本接入）
  duesoon: "s5_duesoon",       // S5 临期提醒（破阵·浑厚版；触发链路待后续版本接入）
  behind: "s6_behind",         // S6 进度预警（银刃；P1 默认关闭）
  schedule: "s7_schedule",     // S7 今日行程（银刃；schedule_block 落地前恒空）
  update: "s8_update",         // S8 新版本更新提示（银刃）
  todos: "s9_todos",           // S9 待办主动推送（银刃；触发链路待后续版本接入）
};

function speakBrief(text, scene) {
  try {
    if (scene) window.__briefScene = scene;
    const sc = scene || window.__briefScene;
    const pack = !!(state.prefs && state.prefs.voice_pack === "pack");
    const wav = pack && sc ? VOICE_PACK_SCENES[sc] : null;
    if (wav) {
      const a = new Audio(`./voice/${wav}.wav`);
      a.play().catch(() => speakTts(text)); // 资源缺失/播放失败 → 系统 TTS 兜底
      return;
    }
    speakTts(text);
  } catch (_) { /* 语音失败绝不阻断晨报主流程 */ }
}

/** 系统 TTS 路径（v2.1 原实现）：选系统 zh 语音读实时文案。 */
function speakTts(text) {
  try {
    const synth = window.speechSynthesis;
    if (!synth) return;
    synth.cancel();
    const u = new SpeechSynthesisUtterance(text);
    u.lang = "zh-CN";
    const zh = (synth.getVoices() || []).find((v) => (v.lang || "").toLowerCase().startsWith("zh"));
    if (zh) u.voice = zh;
    u.rate = 1.0;
    synth.speak(u);
  } catch (_) { /* 语音失败绝不阻断主流程 */ }
}
