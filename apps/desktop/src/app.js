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

// ── 渲染主循环（第三章：四阶段 240ms = 拆解72 + 传递84 + 重组60 + 锁定24）──
// ① 拆解（accelerate）→ ② 传递（旧形态淡出）→ ③ 重组（decelerate，逐卡变形只标 ≤6 张）
// → ④ 锁定（brand 描边闪 24ms）。容器只动 transform/opacity，逐卡才允许 clip-path。
const MORPH_SWAP_MS = 132;  // 出场 156ms 的后 24ms 与进场重叠 → 132ms 处换帧
const MORPH_IN_MS = 84;
const MORPH_LOCK_MS = 24;
let morphTimer = 0;
let prevView = "brief";

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
  add: viewAdd, settings: viewSettings,
};

function render(mode) {
  const main = $("#main");
  if (morphTimer) { clearTimeout(morphTimer); morphTimer = 0; }
  main.classList.remove("morph-out", "morph-in", "morph-lock", "reflow-fade");
  delete main.dataset.morph;

  if (mode === "reflow") {
    // 筛选重排（3.4）：同视图 ≤120ms 只淡入，不套变形、不逐卡动画
    main.innerHTML = (VIEWS[state.view] || viewBrief)();
    bindView();
    void main.offsetWidth;
    main.classList.add("reflow-fade");
    prevView = state.view;
    return;
  }

  const axis = morphAxis(prevView, state.view);
  const busy = main.innerHTML.trim() !== "";

  const assemble = () => {
    main.classList.remove("morph-out");
    main.innerHTML = (VIEWS[state.view] || viewBrief)();
    bindView();
    if (axis) {
      // 3.5 并发上限：逐卡变形只标 ≤6 张（超出直接跳终态），其余卡随容器淡入
      main.dataset.morph = axis;
      main.querySelectorAll(".card").forEach((c, k) => { if (k < 6) c.classList.add("morph-card"); });
    }
    void main.offsetWidth;
    main.classList.add("morph-in");
    morphTimer = setTimeout(() => {
      main.classList.add("morph-lock"); // ④ 锁定 24ms
      morphTimer = setTimeout(() => {
        main.classList.remove("morph-lock", "morph-in");
        delete main.dataset.morph;
        morphTimer = 0;
      }, MORPH_LOCK_MS);
    }, MORPH_IN_MS);
  };

  if (busy) {
    main.classList.add("morph-out"); // ①② 拆解 + 传递（156ms 出场）
    morphTimer = setTimeout(assemble, MORPH_SWAP_MS);
  } else {
    assemble(); // 首屏无旧形态，直接进重组
  }
  prevView = state.view;
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
$("#nav").addEventListener("click", (e) => {
  const btn = e.target.closest("button[data-view]");
  if (!btn) return;
  state.view = btn.dataset.view;
  uiTick();
  document.querySelectorAll("#nav button").forEach((b) =>
    b.setAttribute("aria-current", String(b === btn)));
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
    // v2.1 修复：后端返回字段是 pending 布尔（此前误读 .date，横幅永远不弹）
    if (pending && pending.pending) {
      $("#undelivered").hidden = false;
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
  $("#undelivered-ok").addEventListener("click", async () => {
    $("#undelivered").hidden = true;
    try { await invoke("mark_brief_delivered"); } catch (_) {}
  });
  bindShellEvents();
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

  // 调度线程在启动时发现昨天/今天有未送达的晨报
  await listen("brief-pending", () => {
    const el = $("#undelivered");
    if (el) el.hidden = false;
  });

  // 晨报已送达（系统通知已弹）→ 刷新晨报页、收起补推横幅、按偏好语音播报要点。
  // F10（v2.1 上线）：播报=系统中文 TTS 读要点；免打扰时段只通知不播报；
  // 开关实时跟随主界面右上角总开关（数据库是唯一事实源）。
  await listen("brief-delivered", (e) => {
    const el = $("#undelivered");
    if (el) el.hidden = true;
    if (state.view === "brief") reload();
    const d = e.payload || {};
    if (d.voice_enabled && !d.notification_suppressed && $("#f10-master").checked && d.speech) {
      speakBrief(d.speech);
    }
  });

  // 送达失败：横幅留痕，不静默
  await listen("brief-error", (e) => {
    const el = $("#undelivered");
    if (el) {
      el.hidden = false;
      const span = el.querySelector("span");
      if (span) span.textContent = `晨报送达失败：${e.payload ?? "未知原因"}`;
    }
  });

  // 托盘菜单「查看晨报」→ 切到晨报页
  await listen("brief-open", () => {
    state.view = "brief";
    document.querySelectorAll("#nav button").forEach((b) =>
      b.setAttribute("aria-current", String(b.dataset.view === "brief")));
    render();
  });

  // 启动静默检查发现新版 → 横幅提示，用户确认后才下载安装（v2.1 默认策略）
  await listen("update-available", (e) => {
    const d = e.payload || {};
    const bar = $("#update-bar");
    if (!bar) return;
    bar.hidden = false;
    const span = bar.querySelector("span");
    if (span) span.textContent = `◆ 发现新版本 v${d.version}，可以更新。`;
    $("#update-go")?.addEventListener("click", async () => {
      if (!confirm(`下载并安装 v${d.version}？下载完成后应用会重启进入新版。`)) return;
      try {
        bar.hidden = true;
        await invoke("install_update");
      } catch (err) { alert("更新安装失败：" + err); }
    });
  });
}

/** F10 语音播报：WebView2 speechSynthesis，选系统 zh 语音读要点。
 * 选型理由（v2.1）：Windows 上 WebView2 的 TTS 直接映射系统已装的中文语音
 * （Huihui/Kangkang 等，即 SAPI 语音体系），零新增依赖、窗口隐藏到托盘仍可出声；
 * 红线：只用系统 TTS，不做真人声纹克隆。 */
function speakBrief(text) {
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
  } catch (_) { /* 语音失败绝不阻断晨报主流程 */ }
}
