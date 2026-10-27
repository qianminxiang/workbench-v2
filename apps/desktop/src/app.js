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
function lampFor(item) {
  const a = (state.alertsData?.alerts || []).find((x) => x.item_id === item.id);
  if (!a) return "";
  if (a.kind === "overdue") return `<span class="lamp lamp--red">逾期</span>`;
  if (a.kind === "due_soon") return `<span class="lamp lamp--amber">临期</span>`;
  return "";
}
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
  const list = (arr, lamp) => (arr || []).map((a) =>
    `<div class="card"><div class="rowline">
      <span class="board-strip" style="background:${boardVar(a.board)}"></span>
      <div style="flex:1"><b>${esc(a.title)}</b>
        <div class="mini">${esc(a.detail)}</div></div>
      ${lamp}
    </div></div>`).join("");
  return `
    <div class="panel panel--chamfer" style="--panel-strip: var(--mech-ok); padding:16px">
      <b>晨报 · ${esc(b.date)}</b>
      <div class="mini text-2" style="margin:2px 0 12px">规则引擎纯函数求值 · 各端本地 9:00 触发（架构 §6.2-D）</div>
      <div><span class="text-2">① 当日到期 / 应启动</span>${list(b.due_today, "") || '<div class="mini">无</div>'}</div>
      <hr class="divider">
      <div><span class="text-2">② 临期</span>${list(b.due_soon, '<span class="lamp lamp--amber">临期</span>') || '<div class="mini">无</div>'}</div>
      <hr class="divider">
      <div><span class="text-2">② 逾期</span>${list(b.overdue, '<span class="lamp lamp--red">逾期</span>') || '<div class="mini">无</div>'}</div>
    </div>`;
}

// ── 视图：看板 ──
function viewKanban() {
  const boards = ["policy", "gov", "industry", "media", "event", "customer"];
  return `<div class="kanban">` + boards.map((code) => {
    const zh = BOARD_ZH[code];
    const items = filtered().filter((i) => i.board === code);
    const status = (...ss) => items.filter((i) => ss.includes(i.status ?? "")).length;
    return `<section class="panel" style="--panel-strip:${boardVar(zh)}; padding:12px">
      <div class="rowline" style="margin-bottom:8px">
        <b>${zh}</b><span class="mini" style="margin-left:auto">${items.length} 项</span>
      </div>
      <div class="mini text-2" style="margin-bottom:8px">未启 ${status("pending", "unprepared")} · 进行 ${status("active")} · 闭环 ${status("done")}</div>
      ${items.map((i) => `<div class="card">
        <span class="board-strip" style="background:${boardVar(zh)}"></span>
        <div class="body"><b>${esc(i.title)}</b>
          <div class="rowline" style="margin-top:4px">${gauge(i.progress_pct)}<span class="mini">${i.progress_pct}%</span>${lampFor(i)}</div>
        </div>
      </div>`).join("") || '<div class="mini">（空）</div>'}
    </section>`;
  }).join("") + `</div>`;
}

// ── 视图：明细 ──
function viewDetail() {
  const rows = filtered().map((i) => `<tr>
    <td>${esc(BOARD_ZH[i.board] ?? i.board)}</td>
    <td><b>${esc(i.title)}</b></td>
    <td>${esc(i.category ?? "—")}</td>
    <td>${esc(i.priority ?? "—")}</td>
    <td>${dateCell(i)}</td>
    <td>${gauge(i.progress_pct)}<span class="mini">${i.progress_pct}%</span></td>
    <td>${esc(i.status ?? "—")}</td>
    <td>${lampFor(i)}</td>
    <td class="mini">${esc(i.next_action ?? "—")}</td>
  </tr>`).join("");
  return `<div class="panel panel--chamfer" style="padding:0; overflow:auto; max-height:70vh">
    <table><thead><tr>
      <th>板块</th><th>事项</th><th>类型</th><th>优先级</th><th>计划区间</th><th>进度</th><th>状态</th><th>警示</th><th>下一步</th>
    </tr></thead><tbody>${rows || '<tr><td colspan="9" class="empty">无匹配项</td></tr>'}</tbody></table>
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
function viewSettings() {
  return `<div class="panel panel--chamfer" style="padding:16px; max-width:720px">
    <b>数据</b>
    <div class="settings-row">
      <span>导入存量 Excel（写前自动备份，失败即阻断写入）</span>
      <span><input type="text" id="imp-path" placeholder="…\work_progress.xlsx" style="width:280px">
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
    <b>提醒送达（B7 首版：系统通知 + 应用内；托盘常驻）</b>
    <div class="settings-row">
      <span>免打扰时段（默认 22:00–8:00，期间不弹系统通知）</span>
      <span><input type="time" value="22:00"> – <input type="time" value="08:00"></span>
    </div>
    <hr class="divider">
    <b>团队同步（F5：自建薄服务端）</b>
    <div class="settings-row">
      <span>服务端地址（留空 = 纯本地模式，断网不影响本地功能）</span>
      <span style="display:flex;gap:8px;align-items:center">
        <input type="text" id="sync-url" placeholder="https://sync.example.com:8443" style="width:260px">
        <button class="btn btn--ghost" id="sync-save">保存地址</button>
      </span>
    </div>
    <div class="settings-row">
      <span>登录（每台设备一个令牌，最多 5 台并存）</span>
      <span style="display:flex;gap:8px;align-items:center;flex-wrap:wrap">
        <input type="text" id="sync-user" placeholder="用户名" style="width:130px">
        <input type="password" id="sync-pass" placeholder="口令" style="width:150px">
        <button class="btn" id="sync-login">登录</button>
      </span>
    </div>
    <div class="settings-row">
      <span>同步状态</span>
      <span style="display:flex;gap:8px;align-items:center">
        <span class="chip" id="sync-chip" data-state="unconfigured">未启用同步</span>
        <span class="mini" id="sync-detail">—</span>
      </span>
    </div>
    <div class="settings-row">
      <span>立即同步（断网时改动留队列，恢复后自动补推）</span>
      <span style="display:flex;gap:8px;align-items:center">
        <button class="btn btn--ghost" id="sync-now">立即同步</button>
        <button class="btn btn--ghost" id="sync-logout">退出登录</button>
      </span>
    </div>
    <div class="settings-row">
      <span class="text-2 mini">令牌只存在本机数据库，界面永不显示完整令牌；换设备/丢设备请在服务端按设备撤销。</span>
    </div>
    <hr class="divider">
    <b>更新（静态 JSON 清单 + Ed25519 签名，零鉴权）</b>
    <div class="settings-row">
      <span>检查更新</span>
      <button class="btn btn--ghost" id="upd-go">检查更新</button>
    </div>
    <hr class="divider">
    <b>语音播报（F10 · P2 二期）</b>
    <div class="settings-row">
      <span class="text-2">首版仅预留接口，总开关在主界面显眼位置且默认关闭；不做真人声纹克隆。</span>
    </div>
  </div>`;
}

// ── 渲染主循环 ──
function render() {
  const main = $("#main");
  const views = {
    brief: viewBrief, kanban: viewKanban, detail: viewDetail,
    timeline: viewTimeline, kpi: viewKpi, units: viewUnits,
    add: viewAdd, settings: viewSettings,
  };
  main.classList.remove("view");
  void main.offsetWidth; // 重启动画（≤240ms 轴向变换）
  main.classList.add("view");
  main.innerHTML = (views[state.view] || viewBrief)();
  bindView();
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
        if (r.queued) bits.push("已进同步队列，联网后自动补推");
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
    $("#upd-go")?.addEventListener("click", async () => {
      try {
        const manifestText = await (await fetch(UPDATE_URL)).text();
        const r = await invoke("verify_update", {
          manifestText, currentVersion: "2.0.0", platform: "windows-x86_64",
        });
        alert(r.decision === "available" ? `发现新版本 ${r.latest}` : "已是最新版本。");
      } catch (e) { alert("更新检查失败（离线时保留当前版本）：" + e); }
    });
    $("#theme")?.addEventListener("change", (e) => {
      document.documentElement.dataset.theme = e.target.value;
    });

    // ── 同步：地址保存 / 登录 / 状态 / 立即同步 / 退出登录 ──
    $("#sync-save")?.addEventListener("click", async () => {
      const raw = $("#sync-url").value.trim();
      try {
        const saved = await invoke("set_sync_url", { baseUrl: raw });
        $("#sync-url").value = saved;
        await refreshSyncStatus();
        if (raw && saved !== raw) $("#sync-detail").textContent = `地址已规范化为 ${saved}`;
      } catch (e) { alert("地址不合法：" + e); }
    });

    $("#sync-login")?.addEventListener("click", async () => {
      const baseUrl = $("#sync-url").value.trim();
      const username = $("#sync-user").value.trim();
      const password = $("#sync-pass").value;
      if (!baseUrl) { alert("请先填写服务端地址并保存。"); return; }
      if (!username || !password) { alert("用户名和口令都要填。"); return; }
      $("#sync-chip").textContent = "登录中…";
      try {
        const st = await invoke("sync_login", { baseUrl, username, password });
        $("#sync-pass").value = "";
        applySyncStatus(st);
        // 登录成功后立刻同步一次，把本地存量改动推上去
        try {
          const rep = await invoke("sync_now");
          applySyncReport(rep);
          reload();
        } catch (_) { /* 首同步失败不阻断登录成功这一事实 */ }
      } catch (e) {
        $("#sync-chip").textContent = "登录失败";
        $("#sync-detail").textContent = String(e);
      }
    });

    $("#sync-now")?.addEventListener("click", async () => {
      $("#sync-chip").textContent = "同步中…";
      try {
        const rep = await invoke("sync_now");
        applySyncReport(rep);
        reload();
      } catch (e) {
        $("#sync-chip").textContent = "同步异常";
        $("#sync-detail").textContent = String(e);
      }
    });

    $("#sync-logout")?.addEventListener("click", async () => {
      if (!confirm("退出登录会撤销本机令牌，本地数据保留。继续？")) return;
      try {
        const st = await invoke("sync_logout");
        applySyncStatus(st);
      } catch (e) { alert("退出登录失败：" + e); }
    });

    refreshSyncStatus();
  }
}

/** 把计划精度翻成人话，避免界面直出 day/month/quarter 这类枚举值。 */
function precisionZh(p) {
  return ({ day: "日精度", month: "月精度", quarter: "季精度", ongoing: "长期", unknown: "待确认" })[p] ?? (p || "—");
}

/** 服务端返回的是 SyncState 枚举（snake_case），映射到 chip 的样式与文案。 */
function applySyncStatus(st) {
  const chip = $("#sync-chip");
  const detail = $("#sync-detail");
  if (!chip) return;
  const map = {
    disabled: ["未启用同步", "未启用同步"],
    connected: ["已连接", "已连接"],
    pending: ["待补推", "待补推"],
    error: ["同步异常", "同步异常"],
    auth_required: ["需重新登录", "需重新登录"],
  };
  const [stateKey, label] = map[st.state] ?? ["未启用同步", st.state ?? "未知"];
  chip.textContent = label;
  chip.dataset.state = stateKey;
  const bits = [];
  if (st.member_name) bits.push(`成员 ${st.member_name}`);
  if (st.device_id) bits.push(`设备 ${st.device_id}`);
  if (st.pending > 0) bits.push(`待推 ${st.pending} 条`);
  if (st.last_ok_at) bits.push(`上次成功 ${st.last_ok_at}`);
  if (st.last_error) bits.push(`最近错误 ${st.last_error}`);
  detail.textContent = bits.length ? bits.join(" · ") : "—";
  if ($("#sync-url") && st.base_url) $("#sync-url").value = st.base_url;
}

function applySyncReport(rep) {
  const chip = $("#sync-chip");
  const detail = $("#sync-detail");
  if (!chip) return;
  const map = {
    disabled: "未启用同步", connected: "已连接", pending: "待补推",
    error: "同步异常", auth_required: "需重新登录",
  };
  chip.textContent = map[rep.state] ?? rep.state;
  chip.dataset.state = rep.state ?? "";
  const bits = [`推出 ${rep.pushed}`, `拉入 ${rep.pulled}`];
  if (rep.conflicts) bits.push(`冲突 ${rep.conflicts}`);
  if (rep.pending) bits.push(`待推 ${rep.pending}`);
  if (rep.error) bits.push(`错误 ${rep.error}`);
  detail.textContent = bits.join(" · ");
}

/** 拉一次真实状态回填表单；失败不弹窗（设置页打开时无登录也应安静）。 */
async function refreshSyncStatus() {
  try {
    const st = await invoke("sync_status");
    applySyncStatus(st);
  } catch (_) { /* 读状态失败不打断设置页 */ }
}

// 更新清单地址：静态 JSON，零鉴权（架构 §8；绕开历史 21001 问题）
const UPDATE_URL = "https://updates.example.com/workbench/latest.json";

// ── 导航与筛选绑定 ──
$("#nav").addEventListener("click", (e) => {
  const btn = e.target.closest("button[data-view]");
  if (!btn) return;
  state.view = btn.dataset.view;
  document.querySelectorAll("#nav button").forEach((b) =>
    b.setAttribute("aria-current", String(b === btn)));
  render();
});
document.querySelectorAll(".chip").forEach((chip) => {
  chip.addEventListener("click", () => {
    if (chip.dataset.filter === "board") {
      state.filterBoard = chip.value;
      document.querySelectorAll('.chip[data-filter="board"]').forEach((c) =>
        c.setAttribute("data-active", String(c === chip)));
      updateFilterCount();
      render();
    }
  });
});
$("#filter-clear").addEventListener("click", () => {
  state.filterBoard = "";
  document.querySelectorAll('.chip[data-filter="board"]').forEach((c) =>
    c.setAttribute("data-active", String(!c.value)));
  updateFilterCount();
  render();
});

// B7 补推：启动时检查上次未送达的晨报
(async function boot() {
  try {
    const pending = await invoke("get_pending_brief");
    if (pending && pending.date) {
      $("#undelivered").hidden = false;
    }
  } catch (_) { /* 无记录即正常 */ }
  $("#undelivered-ok").addEventListener("click", async () => {
    $("#undelivered").hidden = true;
    try { await invoke("mark_brief_delivered"); } catch (_) {}
  });
  bindShellEvents();
  reload();
})();

/**
 * 壳 → 前端的事件绑定（B7 与同步）。
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

  // 晨报已送达（系统通知已弹）→ 刷新晨报页并收起补推横幅
  await listen("brief-delivered", () => {
    const el = $("#undelivered");
    if (el) el.hidden = true;
    if (state.view === "brief") reload();
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

  // 托盘菜单「立即同步」→ 直接触发一次同步
  await listen("sync-requested", async () => {
    try {
      const rep = await invoke("sync_now");
      if (state.view === "settings") applySyncReport(rep);
      if (rep.pulled) reload();
    } catch (_) { /* 未配置同步时静默 */ }
  });
}
