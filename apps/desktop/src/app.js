/* 综合工作台 v2.3.0 · 前端（商务风 · 亮蓝 B「内衬层叠轻拟物」）。
 * 分层判据（架构 §3.1）：这里只做呈现与交互意图表达，不含任何业务规则——
 * 提醒、晨报、KPI 全部由 wb-core 纯函数产出，本文件只负责把它们摆到屏上。
 * v2.3.0 商务风改版：装饰体系与音效体系全套移除——视图切换改 160ms 同层淡入
 * （tokens.css --motion-view），筛选重排 120ms（--motion-reflow），无装饰动效。
 * 保留：滚动字幕条 / 通知中心 / 底部指令条 / gotoView 深链 / 免打扰（字幕抑制）。
 */
"use strict";

const invoke = (cmd, payload) => window.__TAURI__.core.invoke(cmd, payload);
// 业务年度（v2.2.1）：默认 2027；启动时由后端 fiscal_year 命令回填，两端同源。
let YEAR = 2027;

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
  view: "project", // W8 退役清理：晨报入口下掉，默认视图改为任务版块（第一入口，§2.1）
  items: [],
  filterBoard: "",
  alertsData: null,
  kpis: [],
  units: [],
  prefs: null, // 单机偏好（免打扰窗口），boot 时读、改时写
  members: [],      // F5 成员表（成员页 / 指派下拉共用；null 表示还没读过）
  me: null,         // F5 本机身份 {member_name, role, known}
  intelRun: null, // 采集中状态 {done,total,site}；null = 当前没有采集在跑。
                  // 存在 state 而不是只写在按钮上：切到别的舱段再切回来时
                  // 按钮是重建的，不恢复就会变回「立即刷新」，让人以为采集已经停了。
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
    await loadMembers();
    if (state.view === "intel") { await loadIntelByMode(); await loadLeads(); }
    else if (state.view === "settings") await loadIntel(); // v3.3.1 设置页新增「情报采集」节
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
  // W8 退役清理：6 板块筛选条已随退役入口一并下掉（design §2.1「界面全删·含筛选条」）。
  // 退役视图函数保留（只清入口不清数据），深链渲染时计数无宿主则静默跳过。
  const el = $("#filter-count");
  if (el) el.textContent = `${filtered().length} / ${state.items.length}`;
}

const ICO = (id) => `<svg class="ico" aria-hidden="true"><use href="#ic-${id}"/></svg>`;
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
  if (!pulsedCards.has(item.id)) {
    pulsedCards.add(item.id);
    pulse = " lamp--pulse"; // 4.4 警报疲劳控制：入场强调每卡每会话只做 1 次
  }
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

/* ── 追加需求 C：进度 / 状态 / 动作记录（明细表与看板卡片共用一套控件） ──
   三处刻意的取舍：
   ① 状态枚举后端序列化成 snake_case（"active"），库里存的是中文（"进行中"）。
      **显示一律走中文**，写回一律传中文——因为 `ItemStatus::from_zh` 只认中文，
      直接把 "active" 发回去会被判成非法值（这条在 v2.2.4 修掉了明细表
      直接 `esc(i.status)` 会露出 "active" 的显示问题）。
   ② `update_item` 用 null 表示「这项不改」，所以**只在真的要改时传值**：
      数字框留空 ≠ 传 0，前者是不改、后者是把进度归零。
   ③ 快捷档位与后端 `PCT_STOPS` 同序（0/25/50/75/100），点一下即提交，
      不要求用户先去点输入框再按回车。 */
const ST_ZH = { pending: "待启动", unprepared: "未筹备", active: "进行中",
  done: "已完成", paused: "已暂停", cancelled: "已取消" };
/** 快捷状态：需求口径的三态（未启 / 进行 / 闭环），值必须是后端认的中文。 */
const ST_CHIPS = [["待启动", "未启"], ["进行中", "进行"], ["已完成", "闭环"]];
const PCT_STOPS_UI = [0, 25, 50, 75, 100];
const statusZh = (s) => ST_ZH[s] ?? (s || "—");
/** 当前状态对应的 chip 高亮：已暂停/已取消在库里是合法值，也要显示出来，
 *  否则这两条在界面上会看起来"没有任何状态"。 */
const statusChips = (i) => {
  const cur = ST_ZH[i.status] || "";
  const extra = cur && !ST_CHIPS.some(([v]) => v === cur) ? [[cur, cur]] : [];
  return [...ST_CHIPS, ...extra].map(([val, label]) =>
    `<button class="st chip--sm" data-st-set="${esc(i.id)}" data-st="${esc(val)}"
      data-active="${String(val === cur)}" title="置为${esc(val)}">${esc(label)}</button>`).join("");
};

/** 进度编辑条：数字框 + 5 个快捷档位。`compact` 用于看板卡片（尺寸更小）。 */
const pctEditor = (i, compact) => `<div class="pct-edit${compact ? " pct-edit--compact" : ""}">
  <input class="pct-input" type="number" min="0" max="100" step="1"
         value="${i.progress_pct}" data-pct-input="${esc(i.id)}"
         aria-label="进度百分比（0–100）"${compact ? "" : ""}><span class="mini text-3">%</span>
  ${PCT_STOPS_UI.map((s) => `<button class="pct-stop" data-pct-set="${esc(i.id)}" data-pct="${s}"
      data-active="${String(i.progress_pct === s)}">${s}</button>`).join("")}
</div>`;

/** 动作记录输入行（含「记一条」按钮）。 */
const noteRow = (i) => `<div class="note-row">
  <input class="note-input" type="text" maxlength="200" placeholder="动作备注，如「已与甲方电话沟通」"
         data-note-input="${esc(i.id)}" aria-label="动作备注">
  <button class="note-add" data-note-add="${esc(i.id)}">${ICO("export")}记一条</button>
</div>`;

/** 明细表与看板共用的整块编辑器；看板里靠 hover/focus 浮现，明细表里常驻。 */
/** 负责人行（PRD §3.3.2「负责人单选、全员可见」）。
 *  展示永远是本人姓名；没有就显示「未分配」——不写「—」，因为这不是缺字段，
 *  而是一个真实状态（未分配时谁都能推进）。 */
const ownerRow = (i, compact) => `<div class="owner-row${compact ? " owner-row--compact" : ""}">
  <span class="mini text-3">负责人</span>
  <span class="owner-name mono">${esc(i.owner || "未分配")}</span>
  <select class="owner-pick" data-owner-pick="${esc(i.id)}" aria-label="指定负责人">
    <option value="">未分配</option>
    ${state.members.filter((m) => m.status === "active").map((m) =>
      `<option value="${esc(m.display_name)}"${m.display_name === (i.owner || "") ? " selected" : ""}>${esc(m.display_name)}</option>`).join("")}
  </select>
</div>`;

const itemEditor = (i, compact) => `<div class="item-edit" data-edit-for="${esc(i.id)}">
  ${ownerRow(i, compact)}
  ${pctEditor(i, compact)}
  <div class="st-row">${statusChips(i)}</div>
  ${noteRow(i)}
  ${compact ? "" : `<button class="hist-toggle" data-hist-toggle="${esc(i.id)}"
      aria-expanded="false">${ICO("export")}动作历史</button>`}
  <div class="mini edit-msg" data-edit-msg="${esc(i.id)}"></div>
</div>`;
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
/** v3.3.1 公告清单页已删除：情报视图只保留「项目进程」。原情报快照（state.intel）
 *  仍要加载——设置页「情报采集」节与源站状态表都从它渲染。项目聚合读失败
 *  不得影响源站状态呈现（原规格 §5 措辞延续）。 */
async function loadIntelByMode() {
  await loadIntel();
  if (window.__IP && window.__IP.state.projects === null) {
    await window.__IP.load();
  }
}

// 改判一（AC-3）：线索清单——与情报同视图按需加载；读失败只影响线索面板，不影响情报渲染
async function loadLeads() {
  try { state.leads = await invoke("intel_leads_get"); } catch (_) { state.leads = null; }
}
function relBadge(r) {
  if (r === "core") return '<span class="lamp lamp--green">相关</span>';
  if (r === "suspect") return '<span class="lamp lamp--amber">疑似不相关</span>';
  return "";
}
// 诊断块文本（与后端 DiagEntry::to_block 同口径，缺项显式标出，不留空白）。
// 放在模块作用域：站点表渲染与「复制诊断」两处都要用，同一口径才能对得上。
const diagText = (g) => [
  `时间：${g.ts || "（未取到）"}`,
  `请求：${g.url || "（未发出请求）"}`,
  `HTTP 状态：${g.http_status == null ? "（无）" : g.http_status}`,
  `分类：${g.kind || "（未知）"}`,
  `尝试次数：${g.attempts || 1}`,
  `说明：${g.detail || "（无）"}`,
  ...(g.body_head ? [`响应体前 300 字符：\n${g.body_head}`] : []),
].join("\n");

function tierZh(t) {
  return { provincial_main: "省级主通道", provincial_fallback: "省级兜底", city_batch1: "一批",
    city_batch2: "二批", city_weekly: "周频观察", manual_only: "人工通道", infeasible: "不可行" }[t] || t;
}
function viewIntel() {
  // v3.3.1 · 公告清单页删除：情报舱只保留「项目进程」（方案 A 主从双栏）。
  //   采集统计/源站状态/补贴导出迁入设置页；线索转待办保留，挂在项目视图下方。
  //   改判入口不丢：项目进程底部工具条已有「标记相关/无关」（data-ip-mark）。
  const base = window.__IP
    ? window.__IP.view()
    : `<div class="panel"><div class="empty">项目进程视图未加载（intel-projects.js 缺失）。</div></div>`;
  return base + viewLeads();
}

/** v3.4.0 · W3 任务版块调度（与 viewIntel 同构）：本体在 project-views.js（window.__PV），
 *  app.js 只做调度与缺模块降级，保持与并行实现轨的 git 隔离。 */
function viewProject() {
  return window.__PV
    ? window.__PV.view()
    : `<div class="panel"><div class="empty">任务版块未加载（project-views.js 缺失）。</div></div>`;
}

/** v3.4.0 · W5 待办版块调度（与 viewProject 同构）：本体在 todo-views.js（window.__TV）。 */
function viewTodo() {
  return window.__TV
    ? window.__TV.view()
    : `<div class="panel"><div class="empty">待办版块未加载（todo-views.js 缺失）。</div></div>`;
}

/** v3.3.1 · 源站状态小窗（设置页「情报采集」节渲染）：公告清单页删除后，
 *  原顶部采集统计条 + 源站状态表整体迁入设置页，站点行渲染逻辑原样保留。 */
function viewIntelSites() {
  const d = state.intel;
  if (!d) {
    return `<div class="empty">情报数据加载失败（离线或数据库异常）。<br>
      <button class="btn" id="intel-refresh">${ICO("refresh")}重试</button></div>`;
  }
  const cov = d.coverage || {};
  const sites = d.sites || {};
  const reg = d.registry || [];
  const statusZh = (s) => ({ blocked: "被拦截", network: "网络失败", parse_error: "解析失败" }[s] || "未知");
  const siteRows = reg.map((r) => {
    const st = sites[r.id] || {};
    const diags = st.diags || [];
    const latest = diags[diags.length - 1];
    const status = !r.scheduled
      ? '<span class="mini">不进自动调度</span>'
      : st.last_status === "ok"
        ? `<span class="mini">✅ 成功（${st.last_success || ""}）</span>`
        : st.last_status
          ? `<span class="mini" style="color:var(--warn-red)">${statusZh(st.last_status)}：${esc(st.last_detail || "")}</span>`
          : '<span class="mini">未采集</span>';
    // 最近一次诊断直接展开（用户不用点开就知道卡在哪），历史 3 条折在 details 里
    const diagCell = latest
      ? `<details class="diag"><summary class="mini">诊断（保留最近 ${diags.length} 次）</summary>
           <div class="diag-body mini">${diags.map((g, i) => `<div class="diag-item${i === diags.length - 1 ? " diag-item--latest" : ""}"><pre>${esc(diagText(g))}</pre></div>`).join("")}</div>
           <button class="btn btn--sm" data-diag-copy="${esc(r.id)}">${ICO("export")}复制诊断</button>
         </details>`
      : '<span class="mini">暂无诊断记录</span>';
    return `<tr><td>${r.name}</td><td>${r.city}</td><td>${tierZh(r.tier)}</td><td>${status}</td>
      <td class="mini">${diagCell}</td><td class="mini">${esc(r.note)}</td></tr>`;
  }).join("");
  return `
    <div class="settings-row" style="flex-wrap:wrap">
      <span style="flex:1;min-width:340px">最近采集：${esc(d.last_run_date || "从未")} · 已接入 ${cov.connected ?? "?"} / 计划覆盖 ${cov.planned ?? "?"} 源 · 库存 ${d.total_stored ?? 0} 条</span>
      <button class="btn" id="intel-refresh">${ICO("refresh")}立即刷新</button>
    </div>
    <table class="tbl"><tr><th>站点</th><th>地区</th><th>批次</th><th>状态</th><th>诊断</th><th>依据</th></tr>${siteRows}</table>`;
}

// 改判一（AC-3）：补贴明细导出面板——v2.3.0 完整补贴明细视图的最小入口。
// 语义与引擎一致：按当前筛选条件**全量**导出（非当前页）。
// 筛选文本 → 引擎 ExportFilter：「字段=值」eq /「字段:值」contains，多条「；」分隔。
function parseSubFilters(raw) {
  const filters = [];
  for (const seg of String(raw || "").split(/[；;]/)) {
    const s = seg.trim();
    if (!s) continue;
    const eq = s.indexOf("=");
    const co = [s.indexOf(":"), s.indexOf("：")].filter((i) => i >= 0);
    if (eq > 0) {
      filters.push({ field: s.slice(0, eq).trim(), op: "eq", value: s.slice(eq + 1).trim() });
    } else if (co.length) {
      const i = Math.min(...co);
      filters.push({ field: s.slice(0, i).trim(), op: "contains", value: s.slice(i + 1).trim() });
    } else {
      return { error: `无法解析筛选「${s}」：需要「字段=值」或「字段:值」形式` };
    }
  }
  return { filters };
}
function viewSubsidyExport() {
  const fromName = state.subFrom ? state.subFrom.split(/[\\/]/).pop() : "";
  return `
    <div class="settings-row" style="flex-wrap:wrap">
      <span style="flex:1;min-width:340px;display:flex;gap:8px;align-items:center">
        <button class="btn btn--ghost" id="sub-pick">选择数据源（CSV）…</button>
        <span class="mini">${esc(fromName || "未选择全量数据 CSV（导出前先选择）")}</span>
        <span class="mini" style="flex:1"></span>
        <input type="text" id="sub-filter" placeholder="筛选（可选）：年份=2024；县:肥西" style="width:240px">
        <button class="btn" id="sub-go">${ICO("export")}导出 Excel</button>
      </span>
    </div>
    <div class="mini" id="sub-status">筛选格式：「字段=值」精确匹配、「字段:值」包含匹配，多条用「；」分隔；留空导出全量。字段须与 CSV 表头一致。</div>`;
}

// 改判一（AC-3）：线索转待办面板——发现 → 确认 → 转待办 闭环的应用内入口。
function viewLeads() {
  if (state.leads == null) { // == 同时覆盖未加载(undefined)
    return `<div class="panel"><div><b>线索转待办</b></div>
      <div class="empty">线索数据读取失败（数据库异常）。<button class="btn" id="leads-retry">重试</button></div></div>`;
  }
  const leads = state.leads.leads || [];
  const stateZh = { discovered: "待确认", confirmed: "已确认", rejected: "已忽略", todoed: "已转待办" };
  const counts = { discovered: 0, confirmed: 0, rejected: 0, todoed: 0 };
  leads.forEach((l) => { if (counts[l.state] != null) counts[l.state]++; });
  const cards = leads.map((l) => {
    const acts = l.state === "discovered"
      ? `<button class="btn btn--sm" data-lead-key="${esc(l.dedup_key)}" data-lead-act="confirm">✓ 确认</button>
         <button class="btn btn--sm" data-lead-key="${esc(l.dedup_key)}" data-lead-act="reject">✗ 忽略</button>`
      : l.state === "confirmed"
        ? `<button class="btn btn--sm" data-lead-key="${esc(l.dedup_key)}" data-lead-act="todo">→ 转待办</button>
           <button class="btn btn--sm" data-lead-key="${esc(l.dedup_key)}" data-lead-act="reject">✗ 忽略</button>`
        : `<span class="mini">${stateZh[l.state] || l.state}</span>`;
    return `<div class="card card--intel${l.state === "discovered" ? " card--selected" : ""}">
      <div class="body">
        <div class="card-title">${esc(l.title)}</div>
        <div class="mini">${esc(l.org || "单位缺失")} · ${esc(l.area || "地区缺失")} · ${esc(l.date || "日期缺失")}${l.amount ? " · " + esc(l.amount) : ""}</div>
        <div class="mini">判重层：${esc(l.dedup_layer)}${l.needs_primary_confirm ? " · 辅源线索（须主源确认后才可派发）" : ""} · <a href="${esc(l.url)}" target="_blank">原文链接</a></div>
        <div class="intel-actions"><span class="mini">${stateZh[l.state] || l.state}</span> ${acts}</div>
      </div>
    </div>`;
  }).join("");
  return `
  <div class="panel">
    <div><b>线索转待办</b>（待确认 ${counts.discovered} · 已确认 ${counts.confirmed} · 已转待办 ${counts.todoed} · 已忽略 ${counts.rejected}）
      <button class="btn" id="leads-reload">${ICO("refresh")}刷新</button></div>
    ${cards || '<div class="empty">暂无线索——采集运行后按三层去重自动产生。</div>'}
  </div>`;
}


// ── 视图：渠道网点（FR-1，v2.2）──
async function loadDealer() {
  try { state.dealer = await invoke("dealer_get"); } catch (_) { state.dealer = null; }
}
function viewDealer() {
  const d = state.dealer;
  if (!d) {
    return `<div class="panel"><div class="empty">网点数据加载失败。<br><button class="btn" id="dealer-refresh">${ICO("refresh")}重试</button></div></div>`;
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
      <button class="btn" id="dealer-refresh">${ICO("refresh")}立即刷新</button></div>
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
    <div class="panel" style="--panel-strip: var(--ok); padding:16px">
      <b>晨报 · <span class="mono">${esc(b.date)}</span></b>
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
      <div class="rowline" style="margin-top:4px">${gauge(i.progress_pct)}<span class="mini mono">${i.progress_pct}%</span><span class="mini st-label">${statusZh(i.status)}</span>${lampFor(i)}</div>
      ${itemEditor(i, true)}
    </div>
  </div>`;
  const focus = state.filterBoard ? CODE_OF[state.filterBoard] : "";
  /* v3.1.0 · rev41 §3 第 2 行：看板改 12 列骨架（6 板块 × 未启·进行 / 已闭环）。
     全景 = 只显 6 个「未启·进行」列（3×2 密度纪律）；选中板块 = 该板块两列聚焦。
     列可见性走 `hidden` 属性 + tokens.css 的 `[hidden]` 硬规则，
     板块身份由 `data-board` 承载、状态由 `data-col` 承载——**按值判，不按列序猜**。 */
  const col = (code, kind) => {
    const zh = BOARD_ZH[code];
    const items = filtered().filter((i) => i.board === code);
    const open = items.filter((i) => i.status !== "done");
    const done = items.filter((i) => i.status === "done");
    const arr = kind === "open" ? open : done;
    const stat = kind === "open"
      ? `未启 ${items.filter((i) => ["pending", "unprepared"].includes(i.status ?? "")).length} · 进行 ${items.filter((i) => i.status === "active").length}`
      : `闭环 ${done.length}`;
    const hideClosed = !focus && kind === "closed";
    return `<section class="board-col" data-board="${boardClass(zh)}" data-col="${kind}"${hideClosed ? " hidden" : ""}>
      <div class="col-head"><h3>${zh} · ${kind === "open" ? "未启 / 进行" : "已闭环"}</h3>
        <span class="col-count mono">${arr.length} 项</span></div>
      <div class="col-stat">${stat}</div>
      ${arr.map((i) => cardOf(i, zh)).join("") || '<div class="mini">（空）</div>'}
    </section>`;
  };
  const cols = focus
    ? [col(focus, "open"), col(focus, "closed")]
    : boards.reduce((acc, c) => acc.concat([col(c, "open"), col(c, "closed")]), []);
  return `<div class="kanban${focus ? " kanban--focus" : ""}">${cols.join("")}</div>`;
}

// ── 视图：明细（L4：9 列砍 7 列——类型+优先级合并双行，下一步移出明细表；区间/进度 mono） ──
function viewDetail() {
  const rows = filtered().map((i) => `<tr data-row-for="${esc(i.id)}">
    <td>${esc(BOARD_ZH[i.board] ?? i.board)}</td>
    <td><b>${esc(i.title)}</b>
      ${itemEditor(i, false)}
      <div class="act-hist" data-hist-for="${esc(i.id)}" hidden></div>
    </td>
    <td>${esc(i.category ?? "—")}<div class="mini text-3">${esc(i.priority ?? "—")}</div></td>
    <td class="mono">${dateCell(i)}</td>
    <td>${gauge(i.progress_pct)}<span class="mini mono">${i.progress_pct}%</span></td>
    <td>${esc(statusZh(i.status))}</td>
    <td>${lampFor(i)}</td>
  </tr>`).join("");
  return `<div class="panel" style="padding:0; overflow:auto; max-height:70vh">
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
  return `<div class="panel" style="padding:16px">
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
  return `<div class="panel" style="padding:0; overflow:auto">
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
  return `<div class="panel" style="padding:0; overflow:auto; max-height:70vh">
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
  return `<div class="panel" style="padding:16px; max-width:720px">
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
  return `<div class="panel" style="padding:16px; max-width:960px">
    <b>数据</b>
    <div class="settings-row">
      <span>导入存量 Excel</span>
      <span><input type="text" id="imp-path" placeholder="…\\work_progress.xlsx" style="width:280px">
      <button class="btn btn--ghost" id="imp-pick">选择文件…</button>
      <button class="btn" id="imp-go">${ICO("import")}导入</button></span>
    </div>
    <div class="settings-row">
      <span>立即全量备份</span>
      <button class="btn btn--ghost" id="bk-go">立即备份</button>
    </div>
    <hr class="divider">
    <b>情报采集（源站状态）</b>
    ${viewIntelSites()}
    <hr class="divider">
    <b>补贴明细导出</b>（按筛选条件全量导出，非当前页）
    ${viewSubsidyExport()}
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
    <b>服务器同步</b>
    <div id="sync-box" style="min-height:24px"><span class="mini">读取同步状态中…</span></div>
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

/* ── 渲染主循环（v2.3.0 商务风）：视图切换 160ms 同层淡入，筛选重排 120ms 交叉淡入。
   容器只动 opacity（红线延续：不动布局属性，避免重排抖动）。无 JS 计时器、
   无打断凭据——连续切换时新渲染直接覆盖旧 DOM，CSS 动画随 class 重挂自然重启。 */
// ── 视图：成员（F5 §3.3.1：可建、可改角色、可停用）──
// 边界：这里只是本机库里的成员表，不引入登录与账号切换。角色决定谁能在成员页
// 动手、以及「管理员可改任何人进度」这条判定。
function viewMembers() {
  const me = state.me;
  const isAdmin = !!me && me.known && me.role === "admin";
  const rows = state.members.map((m) => `<tr data-member-for="${esc(m.member_id)}">
    <td><b>${esc(m.display_name)}</b>${m.display_name === (me?.member_name || "") ? ' <span class="mini text-3">（本机身份）</span>' : ""}</td>
    <td>${m.role === "admin" ? "管理员" : "成员"}</td>
    <td>${m.status === "active" ? "启用" : "已停用"}</td>
    <td class="mono mini">${esc((m.last_active_at || "").replace("T", " ").slice(0, 16) || "—")}</td>
    <td>${isAdmin ? `<button class="btn btn--ghost chip--sm" data-role-set="${esc(m.member_id)}"
        data-role="${m.role === "admin" ? "member" : "admin"}">${m.role === "admin" ? "降为成员" : "升为管理员"}</button>
      <button class="btn btn--ghost chip--sm" data-status-set="${esc(m.member_id)}"
        data-status="${m.status === "active" ? "disabled" : "active"}">${m.status === "active" ? "停用" : "恢复"}</button>` : '<span class="mini text-3">需管理员</span>'}</td>
  </tr>`).join("");
  return `<div class="panel" style="padding:16px; max-width:860px">
    <b>成员</b>
    <div class="mini text-3" style="margin:6px 0 12px">
      只能在本机库里登记成员，不引入账号体系。负责人按姓名匹配，因此不允许重名；
      最后一名启用中的管理员不能被降级或停用。
    </div>
    ${isAdmin ? `<div class="settings-row">
      <span>新增成员</span>
      <span style="display:flex;gap:8px;align-items:center">
        <input type="text" id="member-name" placeholder="姓名" style="width:160px" maxlength="32">
        <select id="member-role"><option value="member">成员</option><option value="admin">管理员</option></select>
        <button class="btn" id="member-add">添加</button>
      </span>
    </div>` : `<div class="mini text-3">当前身份不是管理员，只能查看。</div>`}
    <table class="detail-table" style="width:100%;margin-top:12px">
      <thead><tr><th>姓名</th><th>角色</th><th>状态</th><th>最近活跃</th><th>操作</th></tr></thead>
      <tbody>${rows || '<tr><td colspan="5"><span class="mini text-3">暂无成员</span></td></tr>'}</tbody>
    </table>
    <div class="mini" id="member-msg" style="padding:8px 0"></div>
  </div>`;
}

const VIEWS = {
  brief: viewBrief, kanban: viewKanban, detail: viewDetail,
  timeline: viewTimeline, kpi: viewKpi, units: viewUnits,
  members: viewMembers,
  project: viewProject, // v3.4.0 · W3 任务版块（第一入口，§2.1）
  todo: viewTodo,      // v3.4.0 · W5 待办版块（第二入口，§2.1）
  intel: viewIntel, dealer: viewDealer,
  add: viewAdd, settings: viewSettings,
};

function render(mode) {
  const main = $("#main");
  main.classList.remove("reflow-fade", "view-fade");
  // W8 退役清理：兜底视图由晨报改为任务版块（晨报入口已下掉）
  main.innerHTML = (VIEWS[state.view] || viewProject)();
  // v3.4.0 · W3：切走任务版块时拆掉抽屉/弹层等浮层（PV.sync 幂等，其他视图无操作）
  window.__PV?.sync(state.view);
  // v3.4.0 · W5：切走待办版块时拆掉弹层并复位详情二级页（TV.sync 幂等）
  window.__TV?.sync(state.view);
  bindView();
  void main.offsetWidth; // 重启动画时间轴
  if (mode === "reflow") {
    // 筛选重排（同视图）：120ms 交叉淡入，不整页换动画
    main.classList.add("reflow-fade");
  } else {
    // 视图切换：160ms 同层淡入（--motion-view）
    main.classList.add("view-fade");
  }
}

/** 情报「项目进程」模块的回调钩子（intel-projects.js 内的局部重渲染 /
 *  形态切换都回到这里，不自己再造一套 render，避免两套动画与状态不同步）。 */
window.__ipRenderIntel = (mode) => render(mode || "reflow");

/** 跳转到指定视图（通知主操作 / 托盘菜单共用），保持 nav 高亮同步。 */
function gotoView(view) {
  state.view = view;
  document.querySelectorAll("#nav button").forEach((b) =>
    b.setAttribute("aria-current", String(b.dataset.view === view)));
  render();
}

/** v3.3.1 · 情报采集控件（设置页「情报采集」节）：公告清单页删除后从
 *  intel 分支迁来。runState 恢复与点击启动采集的逻辑原样保留——
 *  采集可能在上一个舱段里启动，重建出的按钮必须回到「进行中」形态。 */
function bindIntelCollectControls() {
  const runState = state.intelRun;
  const btn = $("#intel-refresh");
  if (btn && runState) {
    btn.textContent = runState.total
      ? `采集中… ${runState.done}/${runState.total} 站${runState.site ? `，正在跑 ${runState.site}` : ""}`
      : runState.busy
        ? "采集中…（上一轮还没跑完，请稍候）"   // 被拒：确实有一轮在跑
        : "采集中…（逐站推进）"; // 刚启动：首站还没跑完，不能说自己"被拒"
    btn.disabled = true;
  }
  btn?.addEventListener("click", async (e) => {
    // 时长不能写死一个乐观值：单请求最坏 140s（40s 超时 ×3 次 + 5s/15s 退避），
    // 地市站还要翻页（MAX_PAGES_PER_SITE = 5），单站最坏 300s。整轮绝对上限经实算
    // 为 4654s ≈ 77.6 分钟（14 个 Epoint 站 ×300s + 省级主站双请求 2×140s
    // + 徽采云 140s + 站间 17×2s）。注意不是 16 个：芜湖/池州 epoint=None，
    // 解析期即返回 ParseError、不发请求，不占网络时长。
    // 不写总时长承诺，只写「逐站推进」——中途失败即止步，
    // 真实耗时跨度极大，写任何一个固定数字都会在某种输入下变成谎话。
    e.target.textContent = "采集中…（逐站推进）";
    e.target.disabled = true;
    // 点击即置为进行中：第一个进度事件要等首站跑完才回来（地市站单站最坏 300s），
    // 这期间切走再切回来，按钮得还是「采集中」，不能变回可点的「立即刷新」。
    state.intelRun = { done: 0, total: 0, site: "", busy: false };
    try { await invoke("intel_refresh"); } catch (err) {
      state.intelRun = null;
      alert("启动采集失败：" + err);
      e.target.textContent = "立即刷新";
      e.target.disabled = false;
    }
  });
  // 「复制诊断」：整段（最近 3 次）一次带走，用户直接贴回群里，
  // 不必让他截图或描述——用户机器是唯一能连通政务站的环境，这段文本就是全部证据。
  document.querySelectorAll("[data-diag-copy]").forEach((b) => {
    b.addEventListener("click", async () => {
      const st = (state.intel?.sites || {})[b.dataset.diagCopy] || {};
      const reg = (state.intel?.registry || []).find((r) => r.id === b.dataset.diagCopy) || {};
      const head = `站点：${reg.name || b.dataset.diagCopy}（${b.dataset.diagCopy}）`;
      const body = (st.diags || []).map((g, i, a) => (i === a.length - 1 ? "【最近一次】\n" : "【历史】\n") + diagText(g)).join("\n");
      const text = `${head}\n${body || "（暂无诊断记录）"}`;
      try {
        await navigator.clipboard.writeText(text);
        const old = b.textContent;
        b.textContent = "已复制";
        setTimeout(() => { b.textContent = old; }, 1600);
      } catch (err) {
        // 剪贴板不可用时退化成弹窗让用户手动复制，不静默失败
        const w = window.open("", "_blank");
        if (w) { w.document.write(`<pre style="white-space:pre-wrap;font:13px/1.6 monospace">${esc(text)}</pre>`); }
        else { alert(text); }
      }
    });
  });
}

/** v3.3.1 · 补贴明细导出控件：随面板迁入设置页，绑定逻辑原样保留。 */
function bindSubsidyExportControls() {
  $("#sub-pick")?.addEventListener("click", async () => {
    try {
      const dlg = window.__TAURI__?.dialog;
      if (!dlg || typeof dlg.open !== "function") { alert("当前环境不支持原生文件选择器。"); return; }
      const picked = await dlg.open({
        multiple: false, directory: false, title: "选择补贴明细全量数据 CSV",
        filters: [{ name: "CSV 文件", extensions: ["csv"] }],
      });
      if (typeof picked === "string" && picked) { state.subFrom = picked; render("reflow"); }
    } catch (e) { alert("打开文件选择器失败：" + e); }
  });
  $("#sub-go")?.addEventListener("click", async () => {
    const status = $("#sub-status");
    if (!state.subFrom) { status.textContent = "请先点「选择数据源（CSV）」选取全量数据文件。"; return; }
    const parsed = parseSubFilters($("#sub-filter")?.value);
    if (parsed.error) { status.textContent = parsed.error; return; }
    let out;
    try {
      const dlg = window.__TAURI__?.dialog;
      if (!dlg || typeof dlg.save !== "function") { alert("当前环境不支持保存对话框。"); return; }
      out = await dlg.save({
        title: "导出补贴明细",
        defaultPath: "补贴明细-导出.xlsx",
        filters: [{ name: "Excel 工作表", extensions: ["xlsx"] }, { name: "CSV 文件", extensions: ["csv"] }],
      });
    } catch (e) { alert("打开保存对话框失败：" + e); return; }
    if (!out) return; // 用户取消：无任何副作用
    status.textContent = "导出中…（全量数据应用筛选，耗时取决于行数）";
    try {
      const r = await invoke("subsidy_export", { from: state.subFrom, out, filters: parsed.filters });
      status.textContent = `已导出 ${r.rows_exported} 行（数据源共 ${r.source_rows_total} 行）→ ${r.out}`;
    } catch (e) {
      status.textContent = "导出失败：" + e;
    }
  });
}

/** v3.3.1 · 线索转待办入口：公告清单页删除后保留，挂在项目进程视图下方。 */
function bindLeadActions() {
  document.querySelectorAll("[data-lead-act]").forEach((b) => {
    b.addEventListener("click", async () => {
      b.disabled = true;
      try {
        const r = await invoke("intel_lead_act", { action: b.dataset.leadAct, key: b.dataset.leadKey });
        if (r?.todo) alert(`已生成待办：${r.todo.title}\n截止 ${r.todo.due_date} · 负责人 ${r.todo.owner}\n下一步：${r.todo.next_action}`);
        await loadLeads();
        render("reflow");
      } catch (err) {
        alert("操作失败：" + err);
        b.disabled = false;
      }
    });
  });
  $("#leads-retry")?.addEventListener("click", async () => { await loadLeads(); render("reflow"); });
  $("#leads-reload")?.addEventListener("click", async () => { await loadLeads(); render("reflow"); });
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

  if (state.view === "project") {
    // v3.4.0 · W3 任务版块：绑定与首载全部在模块内（project-views.js）。
    if (window.__PV) window.__PV.bind();
    return;
  }

  if (state.view === "todo") {
    // v3.4.0 · W5 待办版块：绑定与首载全部在模块内（todo-views.js）。
    if (window.__TV) window.__TV.bind();
    return;
  }

  if (state.view === "intel") {
    // v3.3.1 · 公告清单页删除：本分支只留项目进程视图绑定 + 线索转待办；
    //   采集/源站状态/补贴导出控件已迁设置页（bindIntelCollectControls 等）。
    if (window.__IP) window.__IP.bind();
    bindLeadActions();
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

  if (state.view === "members") {
    $("#member-add")?.addEventListener("click", () => memberAct(async () => {
      const name = $("#member-name").value.trim();
      const role = $("#member-role").value;
      if (!name) throw new Error("姓名必填");
      await invoke("member_create", { displayName: name, role });
    }));
    return;
  }

  if (state.view === "settings") {
    bindIntelCollectControls(); // v3.3.1 设置页「情报采集（源站状态）」节
    bindSubsidyExportControls(); // v3.3.1 设置页「补贴明细导出」节
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
    // v3.0 集成：语音/音效/主题已整体移除，set_prefs 后端契约同步瘦身，
    // 现在只写免打扰窗口两个字段（前端与 Rust 两侧口径一致）。
    const saveQuiet = async () => {
      const qs = $("#quiet-start").value || "22:00";
      const qe = $("#quiet-end").value || "08:00";
      try {
        state.prefs = await invoke("set_prefs", { quietStart: qs, quietEnd: qe });
        $("#quiet-saved").textContent = `已保存：${state.prefs.quiet_start}–${state.prefs.quiet_end}`;
      } catch (e) {
        $("#quiet-saved").textContent = "保存失败：" + e;
      }
    };
    $("#quiet-start")?.addEventListener("change", saveQuiet);
    $("#quiet-end")?.addEventListener("change", saveQuiet);

    // ── v3.0 服务器中枢：真实同步（初始化 / 登录 / 状态 / 立即同步 / 登出）──
    // 传输层走后端 sync_setup/sync_login/sync_now（HTTPS）；口令只进登录请求，
    // 不落库不进日志；状态里也永远不含令牌。
    const syncBox = $("#sync-box");
    const renderSync = (st, msg) => {
      if (!st || !st.logged_in) {
        syncBox.innerHTML = `
      <div class="settings-row"><span>服务器地址</span>
        <span><input type="text" id="sync-url" placeholder="https://ahxbd.work" style="width:240px"></span></div>
      <div class="settings-row"><span>账号 / 口令</span>
        <span style="display:flex;gap:8px;align-items:center">
          <input type="text" id="sync-user" style="width:120px">
          <input type="password" id="sync-pass" style="width:120px">
        </span></div>
      <div class="settings-row"><span class="mini" id="sync-msg">${esc(msg || "首次使用先「初始化」（仅一次），之后各设备一律走「登录」。")}</span>
        <span><button class="btn btn--ghost" id="sync-setup">初始化</button>
        <button class="btn" id="sync-login">登录</button></span></div>`;
        $("#sync-url").value = st?.base_url || "";
        const go = async (kind) => {
          $("#sync-msg").textContent = "连接中…";
          try {
            const r = await invoke(kind, {
              baseUrl: $("#sync-url").value.trim(),
              username: $("#sync-user").value.trim(),
              password: $("#sync-pass").value,
            });
            $("#sync-pass").value = "";
            const rep = r.report || {};
            renderSync(r.status, `已连接：推 ${rep.pushed ?? 0} 行 / 拉 ${rep.pulled ?? 0} 行${rep.error ? "（" + rep.error + "）" : ""}`);
          } catch (e) { $("#sync-msg").textContent = "失败：" + e; }
        };
        $("#sync-setup")?.addEventListener("click", () => go("sync_setup"));
        $("#sync-login")?.addEventListener("click", () => go("sync_login"));
      } else {
        syncBox.innerHTML = `
      <div class="settings-row"><span class="mini">
        ${esc(st.state)} · ${esc(st.base_url)} · ${esc(st.member_name || "")} ·
        待推 ${st.pending} 条 · 游标 ${st.cursor}${st.last_error ? " · 上次错误：" + esc(st.last_error) : ""}
      </span></div>
      <div class="settings-row"><span class="mini" id="sync-msg">${esc(msg || ("上次成功：" + (st.last_ok_at || "（无）")))}</span>
        <span><button class="btn btn--ghost" id="sync-logout">登出</button>
        <button class="btn" id="sync-now">立即同步</button></span></div>`;
        $("#sync-now")?.addEventListener("click", async () => {
          $("#sync-msg").textContent = "同步中…";
          try {
            const r = await invoke("sync_now");
            const rep = r.report || {};
            renderSync(r.status, rep.error
              ? "同步失败：" + rep.error + "（离线改动保留在待推队列）"
              : `同步完成：推 ${rep.pushed} 行 / 拉 ${rep.pulled} 行 / 冲突 ${rep.conflicts} 条`);
          } catch (e) { $("#sync-msg").textContent = "同步失败：" + e; }
        });
        $("#sync-logout")?.addEventListener("click", async () => {
          try { renderSync(await invoke("sync_logout"), "已登出：本地数据与游标保留"); }
          catch (e) { $("#sync-msg").textContent = "登出失败：" + e; }
        });
      }
    };
    (async () => {
      try { renderSync(await invoke("sync_status")); }
      catch (e) { renderSync(null, "读取状态失败：" + e); }
    })();

  }
}

/** 把计划精度翻成人话，避免界面直出 day/month/quarter 这类枚举值。 */
function precisionZh(p) {
  return ({ day: "日精度", month: "月精度", quarter: "季精度", ongoing: "长期", unknown: "待确认" })[p] ?? (p || "—");
}

/* RA 单位选中：点击/回车卡片 → 选中描边，再点取消；一次一张主选。
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
});
$("#main").addEventListener("keydown", (e) => {
  if (e.key !== "Enter" && e.key !== " ") return;
  // 内联编辑器里的输入框/按钮自己处理按键，别让它们被当成「点卡片」——
  // 否则在数字框里按回车会连带触发卡片选中（旧实现只看 .card，没排除控件）。
  if (e.target.closest && e.target.closest("button, a, input, select, textarea")) return;
  const card = e.target && e.target.closest ? e.target.closest(".card") : null;
  if (card) { e.preventDefault(); card.click(); }
});

/* ── 追加需求 C：内联编辑的事件委托（明细表 + 看板卡片共用） ──
   一次性挂在 #main 上：视图重渲染后新控件自动生效，不必每次重绑。
   写成功后 `reload()` 重新拉库——看到的就是库里真实的值，
   而不是把用户输入的数字直接回显（否则越界被拒时会显示"改成功了"）。 */
const editMsg = (id, text, bad) => {
  const el = document.querySelector(`[data-edit-msg="${CSS.escape(id)}"]`);
  if (!el) return;
  el.textContent = text;
  el.dataset.bad = bad ? "1" : "0";
};
/** 轻量刷新：只重拉数据 + 120ms 交叉淡入，不走整页换视图动画。
 *  改一个进度数字不该整页重放一遍切换动画——切换动画是「换视图」的反馈，
 *  不是「改一个字段」的反馈。回显文字在重渲染后会丢，调用方重渲染之后再补写。 */
/** 读成员表：成员页与负责人下拉都靠它。失败不清空旧值——宁可显示上次的成员，
 *  也不要让负责人下拉突然变空、让人以为成员全被删了。 */
/** 成员页写操作收口：成功后重拉成员表并重渲染；失败把后端原因原样显示在页脚。
 *  放在模块作用域是刻意的——点击监听常驻 #main，视图重渲染后仍需复用同一套收口。 */
async function memberAct(fn) {
  try {
    await fn();
    await loadMembers();
    render();
  } catch (e) {
    const el = $("#member-msg");
    if (el) { el.textContent = String(e); el.style.color = "var(--red-fg)"; }
  }
}

async function loadMembers() {
  try {
    const r = await invoke("member_list");
    state.members = r.members || [];
    state.me = r.me || null;
  } catch (_) { /* 保持上次结果 */ }
}

async function reloadQuiet() {
  state.items = await invoke("list_items", { year: YEAR });
  state.alertsData = await invoke("alerts", { year: YEAR });
  state.kpis = await invoke("kpi_overview", { year: YEAR });
  state.units = await invoke("units", { year: YEAR });
  render("reflow");
  updateFilterCount();
}

/** 统一提交入口：三个字段**总是显式传**（不改的传 null）。
 *  不省略的理由：省略会让参数"缺字段"，跨 IPC 时可能被当成缺参报错；
 *  null 是后端约定的「这项不改」，语义清晰。传 0 才是把进度真正归零。 */
async function submitPatch(id, { pct, status, note } = {}) {
  const payload = {
    itemId: id,
    progressPct: typeof pct === "number" && !Number.isNaN(pct) ? pct : null,
    status: typeof status === "string" && status ? status : null,
    note: typeof note === "string" && note.trim() ? note.trim() : null,
  };
  if (payload.progressPct === null && !payload.status && !payload.note) {
    editMsg(id, "没有改动，未提交。"); return false;
  }
  editMsg(id, "提交中…");
  try {
    const r = await invoke("update_item", payload);
    const bits = [];
    if (r.from_pct !== r.to_pct) bits.push(`进度 ${r.from_pct}% → ${r.to_pct}%`);
    if (r.from_status !== r.to_status) {
      bits.push(`状态 ${statusZh(r.from_status)} → ${statusZh(r.to_status)}`);
    }
    if (r.action_logged && bits.length === 0) bits.push("已记一条动作");
    // 是否真有东西落库：后端可能「部分成功」（进度越界被拒、状态却改成了），
    // 这时不能笼统报失败——如实说清哪项落库、哪项被拒。
    const landed = r.from_pct !== r.to_pct || r.from_status !== r.to_status || r.action_logged;
    if (!landed) {
      editMsg(id, r.rejected_reason || "没有改动，未写入。", true);
      return false;
    }
    const tail = r.rejected_reason ? `；${r.rejected_reason}` : "";
    const text = bits.join(" · ") + tail;
    const bad = !!r.rejected_reason;
    // 以库里的值为准重拉（自动联动的状态、被拒绝的越界值都在这一步对账）；
    // 刷新失败不回滚——写已经落库了，此时只说"刷新失败"，不谎称没保存。
    try {
      await reloadQuiet();
    } catch (err) {
      editMsg(id, `${text}（界面刷新失败：${err}）`, true);
      return true;
    }
    editMsg(id, text, bad); // 重渲染后把回显补回去
    return true;
  } catch (e) {
    editMsg(id, "保存失败：" + e, true);
    return false;
  }
}

/* 负责人指派：下拉选择即提交。拒绝原因（非管理员/非负责人）原样显示在卡片上，
   与进度失权的提示同一处，用户不用去别处找为什么改不动。 */
$("#main").addEventListener("change", async (e) => {
  const pick = e.target.closest("[data-owner-pick]");
  if (!pick) return;
  const id = pick.dataset.ownerPick;
  const to = pick.value;
  try {
    const r = await invoke("assign_owner", { itemId: id, toOwner: to || null });
    await reloadQuiet();
    editMsg(id, r.changed ? `负责人已改为「${to || "未分配"}」` : "负责人没有变化", false);
  } catch (err) {
    editMsg(id, "改负责人失败：" + err, true);
    // 失败要把下拉回退到旧值，否则界面会显示一个并没落库的负责人
    const cur = state.items.find((it) => it.id === id);
    pick.value = cur?.owner || "";
  }
});

/* 成员页的角色 / 状态按钮：同样委托一次。
   必须注册在模块作用域：放进 bindView() 会随每次视图切换叠加一层，
   切 N 次后点一次「停止」会连发 N 个请求。 */
$("#main").addEventListener("click", (e) => {
  if (state.view !== "members") return;
  const roleBtn = e.target.closest("[data-role-set]");
  if (roleBtn) {
    memberAct(() => invoke("member_set_role", { memberId: roleBtn.dataset.roleSet, role: roleBtn.dataset.role }));
    return;
  }
  const stBtn = e.target.closest("[data-status-set]");
  if (stBtn) {
    memberAct(() => invoke("member_set_status", { memberId: stBtn.dataset.statusSet, status: stBtn.dataset.status }));
  }
});

$("#main").addEventListener("click", async (e) => {
  const pctBtn = e.target.closest("[data-pct-set]");
  if (pctBtn) {
    e.stopPropagation(); // 别触发卡片选中（那个 click 只认 .card）
    const id = pctBtn.dataset.pctSet;
    const cur = state.items.find((it) => it.id === id);
    const v = Number(pctBtn.dataset.pct);
    // 已经是这一档就不再写：避免点一下 50 又多一条「50%→50%」的空历史
    if (cur && cur.progress_pct === v) { editMsg(id, ""); return; }
    await submitPatch(id, { pct: v });
    return;
  }
  const stBtn = e.target.closest("[data-st-set]");
  if (stBtn) {
    e.stopPropagation();
    const id = stBtn.dataset.stSet;
    const cur = state.items.find((it) => it.id === id);
    if (cur && statusZh(cur.status) === stBtn.dataset.st) { editMsg(id, ""); return; }
    await submitPatch(id, { status: stBtn.dataset.st });
    return;
  }
  const addBtn = e.target.closest("[data-note-add]");
  if (addBtn) {
    e.stopPropagation();
    const id = addBtn.dataset.noteAdd;
    const input = document.querySelector(`[data-note-input="${CSS.escape(id)}"]`);
    const note = input?.value ?? "";
    if (!note.trim()) { editMsg(id, "备注不能为空。", true); input?.focus(); return; }
    await submitPatch(id, { note });
    return;
  }
  const histBtn = e.target.closest("[data-hist-toggle]");
  if (histBtn) {
    e.stopPropagation();
    const id = histBtn.dataset.histToggle;
    const box = document.querySelector(`[data-hist-for="${CSS.escape(id)}"]`);
    if (!box) return;
    const open = box.hidden;
    if (open) {
      try {
        const logs = await invoke("item_actions", { itemId: id });
        box.innerHTML = logs.length
          ? `<div class="act-list">${logs.map((a) => `<div class="act-row">
              <span class="mono act-ts">${esc((a.created_at || "").replace("T", " ").slice(0, 16))}</span>
              ${a.from_pct != null && a.from_pct !== a.to_pct
                ? `<span class="mono act-pct">${a.from_pct}%→${a.to_pct}%</span>` : ""}
              <span class="act-note">${esc(a.note)}</span>
              <span class="mini text-3">${esc(a.member || "")}</span>
            </div>`).join("")}</div>`
          : '<div class="mini text-3">暂无动作记录</div>';
      } catch (err) {
        box.innerHTML = `<div class="mini" style="color:var(--red-fg)">读取历史失败：${esc(err)}</div>`;
      }
    }
    box.hidden = !open;
    histBtn.setAttribute("aria-expanded", String(open));
    return;
  }
});

/* 数字框提交：只在「离开整个编辑区」时提交。
   为什么不监听 change：change 在失焦时先于 click 触发，用户手打 42 再点「50」档位
   会先后写两次（42→50），历史里多一条谁也解释不清的中间值。改为看 focusout 的
   relatedTarget——焦点还在同一个 .item-edit 里就交给那个控件处理，让最后一次操作生效。 */
/** 回车已提交过的输入框：让随之而来的 blur/focusout 不再重复写一次。
 *  用 WeakSet 而不是 dataset 标记——重渲染后元素会换，标记会残留到新元素上。 */
const alreadyCommitted = new WeakSet();

$("#main").addEventListener("focusout", async (e) => {
  const input = e.target.closest?.("[data-pct-input]");
  if (!input) return;
  if (alreadyCommitted.has(input)) { alreadyCommitted.delete(input); return; }
  const ed = input.closest(".item-edit");
  if (e.relatedTarget && ed && ed.contains(e.relatedTarget)) return;
  const id = input.dataset.pctInput;
  const raw = input.value.trim();
  if (raw === "") { editMsg(id, "进度不能为空（归零请点 0）。", true); return; }
  const v = Number(raw);
  if (!Number.isInteger(v)) { editMsg(id, "进度需为整数。", true); return; }
  const cur = state.items.find((it) => it.id === id);
  if (cur && v === cur.progress_pct) { editMsg(id, ""); return; }
  await submitPatch(id, { pct: v });
});

/* 进度框回车：立即提交，并登记「这次已提交」避免 blur 重复写 */
$("#main").addEventListener("keydown", async (e) => {
  if (e.key !== "Enter") return;
  const input = e.target.closest?.("[data-pct-input]");
  if (!input) return;
  e.preventDefault();
  e.stopPropagation();
  const id = input.dataset.pctInput;
  const raw = input.value.trim();
  const v = Number(raw);
  if (raw === "" || !Number.isInteger(v)) { editMsg(id, "进度需为 0–100 的整数。", true); return; }
  const cur = state.items.find((it) => it.id === id);
  if (cur && v === cur.progress_pct) { editMsg(id, ""); return; }
  alreadyCommitted.add(input);
  input.blur();
  await submitPatch(id, { pct: v });
});

/* 备注框回车：等同点「记一条」 */
$("#main").addEventListener("keydown", async (e) => {
  if (e.key !== "Enter") return;
  const input = e.target.closest?.("[data-note-input]");
  if (!input) return;
  e.preventDefault();
  e.stopPropagation();
  const id = input.dataset.noteInput;
  const btn = document.querySelector(`[data-note-add="${CSS.escape(id)}"]`);
  if (btn) btn.click();
});

// ── 导航与筛选绑定 ──
$("#nav").addEventListener("click", async (e) => {
  const btn = e.target.closest("button[data-view]");
  if (!btn) return;
  state.view = btn.dataset.view;
  document.querySelectorAll("#nav button").forEach((b) =>
    b.setAttribute("aria-current", String(b === btn)));
  // FR-2/FR-1：进入情报/网点视图时按需拉取（离线失败只影响本视图）
  if (state.view === "intel") { await loadIntelByMode(); await loadLeads(); }
  if (state.view === "dealer") await loadDealer();
  render();
});
// W8 退役清理：6 板块筛选条（板块 chips + filter-clear）已随退役入口下掉，
// 其模块级绑定一并移除（筛选条宿主不存在，绑定会 null 崩）。
// state.filterBoard / filtered() 保留：退役视图函数经通知深链渲染时仍依赖。

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
        // W8 退役清理：晨报入口已下掉，动作深链到任务版块（arch §2.2）
        action: { label: "去任务版块", run: () => gotoView("project") },
      });
    }
  } catch (_) { /* 无记录即正常 */ }
  // PRD §3.3.2：负责人变更在**下次打开**时出现在提醒区。
  // 后端只返回「与本机身份相关」且尚未提醒过的那几条（to_owner 或 from_owner 命中本机
  // 登记名），推完立刻 ack，下一次启动就不会重复打扰。成员表为空时后端返回空数组，
  // 单机老用户不会看到任何多出来的提示。
  try {
    const pend = await invoke("pending_owner_changes");
    for (const c of pend || []) {
      const isNew = c.side === "to";
      enqueueNotice({
        key: `owner-${c.id}`,
        level: "high",
        title: isNew ? "你被指定为负责人" : "负责人已变更",
        text: `◆ 「${c.item_title}」：${c.from_owner || "未分配"} → ${c.to_owner || "未分配"}`,
        action: { label: "查看明细", run: () => gotoView("detail") },
      });
    }
    if (pend && pend.length) {
      await invoke("ack_owner_changes", { ids: pend.map((c) => c.id) }).catch(() => {});
    }
  } catch (_) { /* 成员表为空 / 后端不可用：不提示即正常 */ }
  // 偏好回填：免打扰窗口判定（inQuietHours）依赖 quiet_start / quiet_end。
  // 语音/音效/主题偏好已随商务风改版整体移除，不再回填、不再提供开关。
  try { state.prefs = await invoke("get_prefs"); }
  catch (_) { /* 偏好读取失败：免打扰按未配置处理（不抑制滚动） */ }
  // v3.0 服务器中枢：启动时若已登录，后台静默补一轮同步（断网自动留在待推队列，
  // 不打扰界面；失败只在设置页的状态里可见）。
  try {
    const st = await invoke("sync_status");
    if (st && st.logged_in) invoke("sync_now").catch(() => {});
  } catch (_) { /* 同步状态读不到：单机模式照常使用 */ }
  bindShellEvents();
  bindMarquee();
  bindCmdBar();
  reload();
})();

/**
 * 壳 → 前端的事件绑定（B7 晨报 / 自动更新 / 情报与网点采集）。
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
      // W8 退役清理：晨报入口已下掉，动作深链到任务版块（arch §2.2）
      action: { label: "去任务版块", run: () => gotoView("project") },
    });
  });

  // 晨报已送达 → 字幕播报 + 刷新晨报页。
  // FR-6：版本即时送达的晨报 payload 带 new_version=true，字幕文案标注「新版本」。
  // v2.3.0：语音要点播报已整体移除，送达反馈只保留滚动字幕与通知中心。
  await listen("brief-delivered", (e) => {
    const d = e.payload || {};
    const vTag = d.new_version ? "（新版本）" : "";
    enqueueNotice({
      // 去重键必须用 payload 里真实存在的 brief_date（Rust 侧键名就是 brief_date）。
      // 此前这里写的是 d.date，恒为 undefined，去重键退化成常量 "today"：应用跨午夜
      // 之后，新一天的晨报会覆盖前一天那条历史，通知中心里永远只剩一条晨报记录。
      key: `brief-delivered-${d.brief_date || "today"}`,
      level: "high",
      title: `晨报${vTag}已送达`,
      text: d.new_version
        ? `◆ 检测到版本更新 ${d.version_from || "?"} → ${d.version_to || "?"}，晨报已即时送达`
        : `◆ 今日晨报已送达，可点击查看`,
      // W8 退役清理：晨报入口已下掉，动作深链到任务版块（arch §2.2）
      action: { label: "去任务版块", run: () => gotoView("project") },
    });
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

  // 托盘菜单「查看晨报」→ W8 退役清理：晨报入口已下掉，brief-open 改为
  // 深链到任务版块（arch §2.2：提醒能力转入滚动字幕条，晨报数据仍在库里）。
  await listen("brief-open", () => gotoView("project"));

  // 已有采集在跑（防重入）：明确告诉用户「这一轮还在跑」，不装作刚启动了一轮。
  await listen("intel-busy", () => {
    state.intelRun = { done: 0, total: 0, site: "", busy: true };
    const b = $("#intel-refresh");
    if (!b) return;
    b.textContent = "采集中…（上一轮还没跑完，请稍候）";
    b.disabled = true;
  });

  // FR-2 情报采集逐站进度：整轮绝对上限 77.6 分钟（4654s；单站最坏 300s，翻页 5 页，14 个 Epoint 站），
  // 没有进度就只能靠按钮上那行字干等。后端每站完成即 emit 一次，这里把它变成「N/18 站」。
  await listen("intel-progress", (e) => {
    const d = e.payload || {};
    const done = d.done ?? 0, total = d.total ?? 0, site = d.site_name || "";
    state.intelRun = { done, total, site, busy: false };
    const b = $("#intel-refresh");
    if (!b) return;
    const tail = site ? `，正在跑 ${site}` : "";
    b.textContent = `采集中… ${done}/${total} 站${tail}`;
  });

  // FR-2 情报日采集完成 → 字幕摘要「今日新增相关项目 N 条」（PRD：不把正文塞进通知）
  await listen("intel-updated", (e) => {
    const d = e.payload || {};
    state.intelRun = null; // 本轮结束：按钮在下一次 render 时恢复成「立即刷新」
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
   v2.2 FR-5 首行滚动字幕 + 通知中心 + 底部指令条（UI 规范六/7.4/7.5；v2.3.0 商务风保留）
   ──────────────────────────────────────────────────────────────────────
   队列语义（PRD FR-5 + UI 规范 6.3，已确认口径）：
   - 播放队列 ≤5（--marquee-queue-cap）；业务键去重（后到覆盖先到）；
     严重级（业务失败）插队——当前条播完即接；普通级 FIFO。
   - 溢出条目不丢：进通知中心（铃铛入口，未读角标），历史封顶 50 条防内存增长
     （AC：长时间运行内存占用不随队列增长）。
   - 免打扰（22:00–8:00，口径同后端 quiet 偏好）：非高优先级不滚动、队列保留
     （AC-5-6）；高优先级照滚（语音体系已随 v2.3.0 移除，滚动本身无声）。
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

