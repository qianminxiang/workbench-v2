/* 个人工作台 v2.0 · 前端。
 * 分层判据（架构 §3.1）：这里只做呈现与交互意图表达，不含任何业务规则——
 * 提醒、晨报、KPI 全部由 wb-core 纯函数产出，本文件只负责把它们摆到屏上。
 * 形态切换 ≤240ms（tokens.css --motion-switch），筛选状态三形态共享。
 */
"use strict";

const invoke = (cmd, payload) => window.__TAURI__.core.invoke(cmd, payload);
const YEAR = 2026;

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
const boardVar = (b) => `var(--board-${BOARD_CLASS[BOARD_ZH[b] ?? b] || "b1"})`;

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
    return `<div class="card${it.relevance === "core" ? " card--selected" : ""}">
      <div class="card-title">${it.incomplete ? "⚠ " : ""}${esc(it.title)}</div>
      <div class="mini">${esc(it.org || "单位缺失")} · ${esc(it.area || "地区缺失")} · ${esc(it.date || "日期缺失")}${it.amount ? " · " + esc(it.amount) : ""}</div>
      <div class="mini">来源：${esc(it.site_name)} · <a href="${esc(it.url)}" target="_blank">原文链接</a></div>
      ${it.reasons?.length ? `<div class="mini">命中：${esc(it.reasons.join("，"))}</div>` : ""}
      <div>${relBadge(it.relevance)} ${verdictBtns}</div>
    </div>`;
  }).join("");
  return `
  <div class="panel">
    <div><b>项目情报</b> · 最近采集：${esc(d.last_run_date || "从未")} · 已接入 ${cov.connected ?? "?"} / 计划覆盖 ${cov.planned ?? "?"} 源 · 库存 ${d.total_stored ?? 0} 条
      <button class="btn" id="intel-refresh">立即刷新</button></div>
  </div>
  <div class="panel">
    <div><b>源站状态</b>（${reg.length} 源；芜湖/池州为自有 API，适配器待第二批逐站验证）</div>
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
  let diffHtml = "";
  if (!diff) {
    diffHtml = '<div class="empty">尚未执行周比对（每周自动一次，也可手动刷新建首周基线）。</div>';
  } else if (diff.baseline_week) {
    diffHtml = `<div class="empty">首周基线已建立：共 ${diff.total} 家网点。下周起开始增减比对。</div>`;
  } else {
    const addList = (diff.added || []).map((n) => `<li>${esc(n.company_name)}（${esc(n.area)}）</li>`).join("");
    const rmList = (diff.removed || []).map((n) => `<li>${esc(n.company_name)}（${esc(n.area)}）</li>`).join("");
    diffHtml = `<div>本周比对：新增 ${(diff.added || []).length} 家 / 消失 ${(diff.removed || []).length} 家 · 当前共 ${diff.total} 家</div>
      ${addList ? `<div class="mini"><b>新增：</b><ul>${addList}</ul></div>` : ""}
      ${rmList ? `<div class="mini"><b>消失：</b><ul>${rmList}</ul></div>` : ""}`;
  }
  return `
  <div class="panel">
    <div><b>渠道网点（极飞·安徽）</b> · 上次比对：${esc(d.last_run_date || "从未")}
      <button class="btn" id="dealer-refresh">立即刷新</button></div>
    ${diffHtml}
  </div>
  <div class="panel">
    <div><b>当前快照</b>（${(d.snapshot || []).length} 家）</div>
    <div class="mini">${(d.snapshot || []).slice(0, 50).map((n) => esc(n.company_name)).join(" · ") || "空"}</div>
  </div>`;
}

// ── 视图：晨报（F2） ──
function viewBrief() {
  const b = state.alertsData?.brief;
  if (!b) return `<div class="empty">暂无晨报数据</div>`;
  const list = (arr, kind) => (arr || []).map((a) =>
    `<div class="card${kind ? ` card--alert card--${kind}` : ""}" tabindex="0">${kind ? `<span class="alert-strip"></span>` : ""}<div class="rowline">
      <span class="board-strip" style="background:${boardVar(a.board)}"></span>
      <div style="flex:1"><b>${esc(a.title)}</b>
        <div class="mini">${esc(a.detail)}</div></div>
      ${kind ? `<span class="lamp lamp--${kind}">${kind === "red" ? "逾期" : "临期"}</span>` : ""}
    </div></div>`).join("");
  return `
    <div class="panel panel--chamfer" style="--panel-strip: var(--mech-ok); padding:16px">
      <b>晨报 · <span class="mono">${esc(b.date)}</span><span class="radar" title="规则引擎扫描中" aria-hidden="true"></span></b>
      <div class="mini text-2" style="margin:2px 0 12px">规则引擎纯函数求值 · 各端本地 9:00 触发（架构 §6.2-D）</div>
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
    <span class="board-strip" style="background:${boardVar(zh)}"></span>
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
    const col = (title, arr) => `<section class="panel" style="--panel-strip:${boardVar(zh)}; padding:12px">
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
    return `<section class="panel" style="--panel-strip:${boardVar(zh)}; padding:12px">
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
             style="position:absolute;left:${x0}%;width:${w}%;background:${boardVar(i.board)}"></div>
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
    <div class="mini text-2" style="margin:2px 0 12px">
      计划时间可以直接写自然语言——「下月3号」「周五」「月底」「明天」都认；
      精度不确定的会进待确认队列，不编日精度。
    </div>
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
      <span>计划开始 <span class="mini">（可写「下月3号」）</span></span>
      <input type="text" id="add-start" placeholder="下月3号" style="width:340px">
    </div>
    <div class="settings-row">
      <span>计划结束 <span class="mini">（留空 = 单点）</span></span>
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
      <span>导入存量 Excel（写前自动备份，失败即阻断写入）</span>
      <span><input type="text" id="imp-path" placeholder="…\\work_progress.xlsx" style="width:280px">
      <button class="btn" id="imp-go">导入</button></span>
    </div>
    <div class="settings-row">
      <span>立即全量备份（L2：db + 每表 JSON，轮转前 integrity_check）</span>
      <button class="btn btn--ghost" id="bk-go">立即备份</button>
    </div>
    <hr class="divider">
    <b>外观</b>
    <div class="settings-row">
      <span>主题（档位 B：亮=办公态 / 暗=机甲态，默认跟随系统）</span>
      <select id="theme">
        <option value="">跟随系统</option><option value="light">锁定亮色</option><option value="dark">锁定暗色</option>
      </select>
    </div>
    <hr class="divider">
    <b>提醒送达（B7：系统通知 + 应用内；托盘常驻）</b>
    <div class="settings-row">
      <span>免打扰时段（期间只通知不播报，晨报照常生成不丢）</span>
      <span style="display:flex;gap:8px;align-items:center">
        <input type="time" id="quiet-start">
        –
        <input type="time" id="quiet-end">
      </span>
    </div>
    <div class="settings-row">
      <span class="mini">免打扰窗口按本地时钟生效（v2.1 修复 UTC 偏移：晨报本地 9:00 准点）</span>
      <span class="mini" id="quiet-saved"></span>
    </div>
    <hr class="divider">
    <b>语音播报（F10 · v2.1 上线）</b>
    <div class="settings-row">
      <span>总开关在主界面右上角，默认开启；关闭后只弹通知不出声。</span>
    </div>
    <div class="settings-row">
      <span class="mini">播报走系统中文语音（Windows 自带），读晨报要点不逐字念全部；不做真人声纹克隆。</span>
    </div>
    <hr class="divider">
    <b>更新（自动检查 + 确认后安装，Ed25519 验签）</b>
    <div class="settings-row">
      <span>检查更新（启动时已静默检查过一次，这里手动复查）</span>
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
    $("#imp-go")?.addEventListener("click", async () => {
      const path = $("#imp-path").value.trim();
      if (!path) return;
      try {
        const r = await invoke("import_excel", { xlsxPath: path, year: YEAR });
        alert(`导入完成：${r.persist.items_written} 项 / ${r.persist.kpis_written} KPI 落库，回读一致 ${r.persist.items_readback}。`);
        reload();
      } catch (e) { alert("导入失败：" + e); }
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
        state.prefs = await invoke("set_prefs", {
          quietStart: qs, quietEnd: qe,
          voiceEnabled: $("#f10-master").checked,
        });
        $("#quiet-saved").textContent = `已保存：${state.prefs.quiet_start}–${state.prefs.quiet_end}`;
      } catch (e) {
        $("#quiet-saved").textContent = "保存失败：" + e;
      }
    };
    $("#quiet-start")?.addEventListener("change", saveQuiet);
    $("#quiet-end")?.addEventListener("change", saveQuiet);

    $("#theme")?.addEventListener("change", (e) => {
      document.documentElement.dataset.theme = e.target.value;
    });

  }
}

/** 把计划精度翻成人话，避免界面直出 day/month/quarter 这类枚举值。 */
function precisionZh(p) {
  return ({ day: "日精度", month: "月精度", quarter: "季精度", ongoing: "长期", unknown: "待确认" })[p] ?? (p || "—");
}

/* ── 红色警戒参照交互音效（v2.1 追加评论）：WebAudio 现场合成 ──
   零素材、零版权、零新增依赖——不复用任何游戏原声，全部程序合成。
   ① uiTick：导航/选中/筛选的机械咔哒（RA 风格 UI 反馈音）
   ② sfxAlert：新警示卡入场的 EVA 式短双音——受 4.4 疲劳控制（每卡每会话 1 次，警示灯仍不循环）
   闸门：与语音播报共用 voice_enabled 总开关（数据库唯一事实源）；
   免打扰只约束晨报主动送达，用户主动点击产生的反馈音不受其限。失败静默，绝不阻断交互。 */
let sfxCtx = null;
function sfxOn() { return !!(state.prefs && state.prefs.voice_enabled); }
function beep(freq, ms, vol) {
  try {
    if (!sfxOn()) return;
    sfxCtx = sfxCtx || new (window.AudioContext || window.webkitAudioContext)();
    if (sfxCtx.state === "suspended") sfxCtx.resume();
    const o = sfxCtx.createOscillator(), g = sfxCtx.createGain();
    o.type = "square";
    o.frequency.value = freq;
    const t = sfxCtx.currentTime;
    g.gain.setValueAtTime(vol || 0.035, t);
    g.gain.exponentialRampToValueAtTime(0.0001, t + ms / 1000);
    o.connect(g);
    g.connect(sfxCtx.destination);
    o.start(t);
    o.stop(t + ms / 1000);
  } catch (_) { /* 无手势/无音频设备/被策略拦截：静默跳过 */ }
}
function uiTick() { beep(1150, 35); }
function sfxAlert() { beep(620, 70); setTimeout(() => beep(620, 70), 110); }

/* v2.2 气闸切换音（4.3）：WebAudio 纯合成伺服声——合盖约 90ms 下落、弹出约 120ms
   上扬；响度对齐既有档位；同一时刻只出一条；降级态（reduced-motion）视为静音态。 */
function sfxServo(up) {
  try {
    if (!sfxOn() || reducedMotion()) return;
    sfxCtx = sfxCtx || new (window.AudioContext || window.webkitAudioContext)();
    if (sfxCtx.state === "suspended") sfxCtx.resume();
    const o = sfxCtx.createOscillator(), g = sfxCtx.createGain();
    o.type = "sawtooth";
    const t = sfxCtx.currentTime;
    const dur = up ? 0.12 : 0.09;
    o.frequency.setValueAtTime(up ? 300 : 620, t);
    o.frequency.linearRampToValueAtTime(up ? 620 : 300, t + dur);
    g.gain.setValueAtTime(0.03, t);
    g.gain.exponentialRampToValueAtTime(0.0001, t + dur);
    o.connect(g); g.connect(sfxCtx.destination);
    o.start(t); o.stop(t + dur + 0.01);
  } catch (_) { /* 音频失败绝不阻断切换 */ }
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
  // F10 总开关初始化：从偏好库读真实状态（默认开启），不再写死 disabled
  try {
    state.prefs = await invoke("get_prefs");
    const sw = $("#f10-master");
    sw.disabled = false;
    sw.checked = !!state.prefs.voice_enabled;
    sw.addEventListener("change", async () => {
      try {
        const cur = state.prefs || await invoke("get_prefs");
        state.prefs = await invoke("set_prefs", {
          quietStart: cur.quiet_start, quietEnd: cur.quiet_end,
          voiceEnabled: sw.checked,
        });
      } catch (_) { /* 写失败时开关状态可能不同步，下次启动回读纠偏 */ }
    });
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
      speakBrief(d.speech, d.new_version ? "version_boot" : undefined);
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
  playMarqueeItem(notice.queue[notice.idx], notice.idx);
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
  renderNoticeCenter();
  marqueeSchedule();
}

/** 详情条（6.2）：当前条完整内容 + 主操作；展开期间滚动暂停。 */
function openMarqueeDetail(item) {
  const det = $("#mq-detail");
  const bar = $("#marquee-bar");
  if (!det || !bar || !item) return;
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
      const q = notice.queue.find((x) => x.key === h.key) || h;
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

/** F10 语音播报：WebView2 speechSynthesis，选系统 zh 语音读要点。
 * 选型理由（v2.1）：Windows 上 WebView2 的 TTS 直接映射系统已装的中文语音
 * （Huihui/Kangkang 等，即 SAPI 语音体系），零新增依赖、窗口隐藏到托盘仍可出声；
 * 红线：只用系统 TTS，不做真人声纹克隆。 */
function speakBrief(text, scene) {
  // v2.2 语音包：scene 为场景键（如 version_boot），预录样音接入后按场景选包；
  // 当前默认走系统 TTS（C 档样音资源接入在语音包阶段完成）。
  try {
    if (scene) window.__briefScene = scene;
    const synth = window.speechSynthesis;
    if (!synth) return;
    synth.cancel();
    const u = new SpeechSynthesisUtterance(text);
    u.lang = "zh-CN";
    const zh = (synth.getVoices() || []).find((v) => (v.lang || "").toLowerCase().startsWith("zh"));
    if (zh) u.voice = zh;
    u.rate = 1.0;
    synth.speak(u);
  } catch (_) { /* 语音失败绝不阻断晨报主流程 */ }
}
