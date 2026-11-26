/* v3.4.0 · 任务版块（项目跟踪过程）界面模块
 * ============================================================================
 * 依据：设计规格 v3.4.0 §3.1（任务版块）/ §5.1（录入弹层）/ §5.2（项目内时间线）/
 *      §5.4（手动建档两处定点）/ §6.1（空态铁律）。
 * 覆盖范围（工作项 W3）：项目列表 + 项目详情抽屉（560px）+ 过程记录录入弹层 +
 *      新建任务弹层 + 存量迁移向导（预览 → 确认，不自动执行）。
 *      全年河流视图与 Bento 摘要带属 W4，本模块不预置其 DOM。
 *
 * 挂载三处均为加法（与 intel-projects.js 同一模式，避免与并行轨道 git 硬冲突）：
 *   1. index.html 在 app.js 之前引入本文件；
 *   2. app.js 的 viewProject() 只调 PV.view()、bindView 的 project 分支只调 PV.bind()；
 *   3. app.js 的 render() 顶部调 PV.sync(state.view)——切走视图时拆除本模块浮层。
 *
 * 空态铁律（§6.1，硬约束③）：本模块不编造任何数据——列表、徽标、计数、空态
 * 文案全部来自后端真实返回或规格原文；无数据的位置如实留白或按规格给下一步。
 * ============================================================================
 */
"use strict";

(function () {
  const PV = {};
  window.__PV = PV;

  // ── 常量 ─────────────────────────────────────────────────────────────────
  // 阶段文案与 wb-core::project::Stage::zh 逐字对齐（筹备中/推进中/收尾中/已结项/已归档）。
  const STAGES = [
    ["planned", "筹备中"], ["active", "推进中"], ["closing", "收尾中"],
    ["closed", "已结项"], ["archived", "已归档"],
  ];
  const STAGE_ZH = Object.fromEntries(STAGES);
  // Stage 枚举 derive(Serialize/Deserialize) 无 rename 属性（project.rs 实读核验），
  // serde 变体名是 PascalCase；DB 存的是 code() 小写码。展示/筛选用小写码，提交 draft.stage 用 serde 名。
  const STAGE_SERDE = { planned: "Planned", active: "Active", closing: "Closing", closed: "Closed", archived: "Archived" };
  const KIND_ZH = { note: "记录", visit: "拜访", bid: "投标", deliver: "交付", milestone: "里程碑" };
  const WEEK = ["周日", "周一", "周二", "周三", "周四", "周五", "周六"];
  const IMG_EXTS = ["png", "jpg", "jpeg", "gif", "webp", "bmp"];
  // §5.1 定值：动作摘要 ≤500 字、图片 ≤9 张、附件 ≤5 个（计数器 / 角标如实呈现）。
  const BODY_MAX = 500, MAX_IMAGES = 9, MAX_FILES = 5;

  // ── 会话内状态（不落盘） ──────────────────────────────────────────────────
  const st = {
    loaded: false, loading: false, skeleton: false,
    projects: null,           // ProjectDigest[] | null（null = 尚未读到）
    error: null,              // 读取失败时的错误文本
    filter: { stage: "", area: "", kw: "" },
    drawer: null,             // { id, project, notes, atts, err } | null
    wizardShown: false,       // 本次会话是否已检查过迁移预览
    wizardDismissed: false,   // 用户点过「暂不迁移」（仅本会话内不再弹）
  };

  // ── 工具（app.js 的全局绑定在调用期已就绪——本文件先于 app.js 加载，
  //    但所有引用都在函数体内，真正执行时两个文件都已求值） ──────────────────
  const $all = (sel, root) => Array.from((root || document).querySelectorAll(sel));
  const todayStr = () => {
    const d = new Date();
    return `${d.getFullYear()}-${String(d.getMonth() + 1).padStart(2, "0")}-${String(d.getDate()).padStart(2, "0")}`;
  };
  /** occurred_at 是用户本地日期拼成（W2 口径），直接取前 10 位即真实日期，不做时区换算。 */
  const datePart = (iso) => String(iso || "").slice(0, 10);
  const dateZh = (iso) => {
    const d = datePart(iso);
    const t = Date.parse(`${d}T00:00:00`);
    return Number.isNaN(t) ? d : `${d} ${WEEK[new Date(t).getDay()]}`;
  };
  const timePart = (iso) => {
    const s = String(iso || "");
    return s.length >= 16 ? s.slice(11, 16) : "";
  };
  /** 逾期 / 临期（§3.1：互斥，取更严重者；已结项/已归档不再警示）。 */
  function endWarn(p) {
    if (!p.expect_end_date || p.stage === "closed" || p.stage === "archived") return "";
    const today = todayStr();
    if (p.expect_end_date < today) return "over";
    const diff = (Date.parse(`${p.expect_end_date}T00:00:00`) - Date.parse(`${today}T00:00:00`)) / 86400000;
    return diff <= 7 ? "due" : "";
  }
  const isImgExt = (ext) => IMG_EXTS.includes(String(ext || "").toLowerCase().replace(/^\./, ""));
  /** 本地文件 → webview 可显示的地址。asset 协议未开通（tauri.conf.json）时退化为占位块。 */
  const fileSrc = (p) => {
    try {
      const f = window.__TAURI__?.core?.convertFileSrc;
      return typeof f === "function" ? f(p) : null;
    } catch { return null; }
  };
  // 数据目录绝对路径（一次取、缓存）：storage_path 是相对数据目录的路径（架构 §3.2），
  // asset 协议只认绝对路径，与 lib.rs 的 data_dir_path 命令同源、不走 path 插件。
  let dataDirCache = null;
  async function ensureDataDir() {
    if (dataDirCache !== null) return dataDirCache;
    try { dataDirCache = await invoke("data_dir_path", {}); } catch { dataDirCache = ""; }
    return dataDirCache;
  }
  /** 附件行 → asset 地址。dataDir 未就绪 / 协议不可用时返回 null（渲染占位块）。 */
  const attSrc = (a, dataDir) => {
    if (!dataDir || !a?.storage_path) return null;
    const base = String(dataDir).replace(/[\\/]+$/, "");
    return fileSrc(`${base}/${String(a.storage_path).replace(/^[\\/]+/, "")}`);
  };

  // ── 列表数据 ─────────────────────────────────────────────────────────────
  async function load() {
    if (st.loading) return;
    st.loading = true; st.error = null;
    void ensureDataDir(); // 与 project_list 并行，不阻塞列表
    // §3.1 加载态：超过 2s 才出现骨架，之前保持上一帧（首帧无上一帧 = 留白）
    const skelTimer = setTimeout(() => {
      if (st.loading && !st.loaded) { st.skeleton = true; render("reflow"); }
    }, 2000);
    try {
      const r = await invoke("project_list", { year: YEAR, includeArchived: false });
      st.projects = r.projects || [];
      st.loaded = true;
      clearTimeout(skelTimer);
      render("reflow");
      if (st.projects.length === 0) maybeOfferMigration();
    } catch (e) {
      st.error = String(e);
      clearTimeout(skelTimer);
      render("reflow");
    } finally {
      st.loading = false;
    }
  }

  /** 首启动迁移向导：只在真零项目时检查预览；预览只读、执行必须经用户确认（§3.5）。 */
  async function maybeOfferMigration() {
    if (st.wizardShown || st.wizardDismissed) return;
    st.wizardShown = true;
    let plan;
    try {
      const r = await invoke("project_migration_preview");
      plan = r.plan;
    } catch { return; } // 预览读失败不打扰空态，下次启动再试
    if (!plan || !plan.pending_items) return;
    mountWizard(plan);
  }

  // ── 视图（app.js 的 viewProject 只调这里） ──────────────────────────────
  PV.view = function view() {
    const head = `
      <div class="pv-head">
        <div class="pv-head-txt">
          <h2>任务</h2>
          <p class="pv-brief">全年河流、任务列表与时间线共用同一份任务池。过程记录按真实日期归月生长</p>
        </div>
        <button class="btn pv-new" id="pv-new">新建任务</button>
      </div>`;
    if (st.error) return head + errCard(st.error);
    if (st.projects === null) return head + (st.skeleton ? skeleton() : `<div class="pv-list" style="min-height:120px"></div>`);
    if (st.projects.length === 0) return head + emptyCard();
    return head + filterBar() + `<div class="pv-list">${rows()}</div>`;
  };

  const errCard = (msg) => `
    <div class="pv-err">
      <h3>读取本地数据失败</h3>
      <p class="pv-err-code">${esc(msg).slice(0, 200)}</p>
      <div class="pv-err-acts">
        <button class="btn" id="pv-retry">重试</button>
        <button class="btn btn--ghost pv-go-diag">查看诊断</button>
      </div>
    </div>`;

  const skeleton = () => `
    <div class="pv-list" aria-busy="true">
      <div class="pv-skel"></div><div class="pv-skel"></div><div class="pv-skel"></div>
    </div>`;

  /** §3.1 真零空态（文案逐字取自规格；不显示示例项目、不显示 0 项统计卡）。 */
  const emptyCard = () => `
    <div class="pv-empty">
      <h3>从零开始，先建一条任务</h3>
      <p>任务库为空。新建一条任务、记一次过程记录，全年河流、待办与提醒都会随真实操作长出来；网点与情报可在对应版块恢复采集快照。</p>
      <div class="pv-empty-acts">
        <button class="btn" id="pv-new">新建任务</button>
        <button class="btn btn--ghost" id="pv-go-intel">先建情报项目</button>
      </div>
    </div>`;

  /** §3.1 筛选条：状态 + 地区 + 关键词。**不含板块维度**（6 板块口径已退役）。 */
  function filterBar() {
    const areas = Array.from(new Set(st.projects
      .map((d) => (d.project.area || "").trim()).filter(Boolean))).sort();
    const vis = filtered();
    const opt = (v, zh, cur) => `<option value="${esc(v)}"${v === cur ? " selected" : ""}>${esc(zh)}</option>`;
    return `
      <div class="pv-filter">
        <select class="pv-f" data-f="stage">${opt("", "全部状态", st.filter.stage)}${STAGES.map(([v, zh]) => opt(v, zh, st.filter.stage)).join("")}</select>
        <select class="pv-f" data-f="area">${opt("", "全部地区", st.filter.area)}${areas.map((a) => opt(a, a, st.filter.area)).join("")}</select>
        <input class="pv-f pv-kw" data-f="kw" type="search" placeholder="搜索项目名 / 客户 / 简介" value="${esc(st.filter.kw)}">
        <span class="mini">${vis.length} / ${st.projects.length}</span>
      </div>`;
  }

  function filtered() {
    const f = st.filter;
    const kw = f.kw.trim().toLowerCase();
    return st.projects.filter((d) => {
      const p = d.project;
      if (f.stage && p.stage !== f.stage) return false;
      if (f.area && (p.area || "").trim() !== f.area) return false;
      if (kw && !`${p.name}\n${p.client || ""}\n${p.summary || ""}`.toLowerCase().includes(kw)) return false;
      return true;
    });
  }

  /** §3.1 任务列表行：状态徽标 + 项目名 + 次级信息行 + 右侧警示 + 关联计数徽标。
   *  进度条无数据源（W2 schema 无进度字段），按「不编造数据」铁律不渲染（PR 说明有交代）。 */
  function rows() {
    return filtered().map((d) => {
      const p = d.project;
      const warn = endWarn(p);
      const metaBits = [];
      if (p.client) metaBits.push(esc(p.client));
      if (p.area) metaBits.push(esc(p.area));
      if (p.owner) metaBits.push(`负责人 ${esc(p.owner)}`);
      if (d.last_occurred_at) metaBits.push(`最近动作 ${esc(dateZh(d.last_occurred_at))}`);
      const warnHtml = warn === "over"
        ? `<span class="pv-flag pv-flag--danger">逾期</span>`
        : warn === "due" ? `<span class="pv-flag pv-flag--warn">临期</span>` : "";
      return `
        <div class="pv-row" data-id="${esc(p.id)}" role="button" tabindex="0">
          <span class="pv-badge pv-badge--${esc(p.stage)}">${esc(STAGE_ZH[p.stage] || p.stage)}</span>
          <div class="pv-row-main">
            <div class="pv-row-name">${esc(p.name)}</div>
            <div class="pv-row-meta">${metaBits.length ? metaBits.join(" · ") : '<span class="pv-row-none">还没有过程记录</span>'}</div>
          </div>
          <div class="pv-row-side">
            ${warnHtml}
            <button class="pv-count" data-id="${esc(p.id)}" title="过程记录数">${d.note_count} 条记录</button>
          </div>
        </div>`;
    }).join("");
  }

  // ── 事件绑定（app.js 的 bindView project 分支只调这里） ─────────────────
  PV.bind = function bind() {
    if (!st.loaded && !st.loading) load();
    $("#pv-new")?.addEventListener("click", () => openNewTask());
    $("#pv-go-intel")?.addEventListener("click", () => gotoView("intel"));
    $("#pv-retry")?.addEventListener("click", () => { st.error = null; st.loaded = false; load(); });
    $all(".pv-go-diag").forEach((b) => b.addEventListener("click", () => gotoView("settings")));
    $all(".pv-f").forEach((el) => {
      const key = el.dataset.f;
      if (key === "kw") {
        let t = null;
        el.addEventListener("input", (e) => {
          clearTimeout(t);
          const v = e.target.value;
          t = setTimeout(() => { st.filter.kw = v; render("reflow"); }, 300);
        });
      } else {
        el.addEventListener("change", (e) => { st.filter[key] = e.target.value; render("reflow"); });
      }
    });
    $all(".pv-row").forEach((el) => {
      const open = () => openDrawer(el.dataset.id);
      el.addEventListener("click", (e) => { if (!e.target.closest(".pv-count")) open(); });
      el.addEventListener("keydown", (e) => { if (e.key === "Enter" || e.key === " ") { e.preventDefault(); open(); } });
    });
    // 关联计数徽标可点（§3.1：点开同一抽屉）
    $all(".pv-count").forEach((b) => b.addEventListener("click", () => openDrawer(b.dataset.id)));
  };

  /** app.js render() 每帧调用：切走任务版块时拆除本模块浮层（reflow 同视图不拆）。 */
  PV.sync = function sync(view) {
    if (view !== "project") closeAllOverlays();
  };

  // ── 浮层骨架（遮罩按 §1.3：rgba(20,32,54,.34) + blur(5px)） ─────────────
  function overlay(cls, inner) {
    const mask = document.createElement("div");
    mask.className = `pv-mask ${cls}`;
    mask.innerHTML = inner;
    document.body.appendChild(mask);
    return mask;
  }
  function closeAllOverlays() {
    $all(".pv-mask").forEach((m) => m.remove());
    if (st.drawer) st.drawer = null;
  }

  // Esc 关闭最上层浮层（一个监听器，随最上层浮层逐层退）
  document.addEventListener("keydown", (e) => {
    if (e.key !== "Escape") return;
    const masks = $all(".pv-mask");
    if (!masks.length) return;
    e.preventDefault();
    const top = masks[masks.length - 1];
    if (top.classList.contains("pv-viewer")) { top.remove(); return; }
    if (top.classList.contains("pv-drawer-mask")) { top.remove(); st.drawer = null; return; }
    // 弹层（记录 / 建档 / 新建任务 / 向导）：取消不执行任何写操作
    top.remove();
  });

  // ── 项目详情抽屉（§3.1：右滑 560px，磨砂档④；字段区 + 过程记录区 + 时间线预览） ──
  async function openDrawer(id) {
    // 重复打开另一项目时，先拆掉旧浮层，避免叠加多层遮罩。
    $all(".pv-drawer-mask").forEach((m) => m.remove());
    st.drawer = { id, project: null, notes: null, atts: {}, err: null };
    const mask = overlay("pv-drawer-mask", `
      <aside class="pv-drawer" role="dialog" aria-label="项目详情">
        <div class="pv-drawer-body pv-drawer-loading">读取项目…</div>
      </aside>`);
    mask.addEventListener("mousedown", (e) => { if (e.target === mask) { mask.remove(); st.drawer = null; } });
    try {
      const [pg, nt] = await Promise.all([
        invoke("project_get", { id }),
        invoke("project_notes", { projectId: id }),
        ensureDataDir(), // 缩略图要拼绝对路径（asset 协议），先就绪再渲染
      ]);
      if (!st.drawer || st.drawer.id !== id) return; // 已被关闭/切换
      st.drawer.project = pg.project;
      st.drawer.notes = nt.notes || [];
      const need = st.drawer.notes.filter((n) => n.attachment_count > 0);
      await Promise.all(need.map(async (n) => {
        try {
          const r = await invoke("project_attachments", { noteId: n.id });
          st.drawer.atts[n.id] = r.attachments || [];
        } catch { st.drawer.atts[n.id] = null; } // 缩略缺失按 §6.2 部分失败口径处理
      }));
      if (!st.drawer || st.drawer.id !== id) return;
      updateDrawer();
    } catch (e) {
      if (st.drawer && st.drawer.id === id) {
        st.drawer.err = String(e);
        updateDrawer();
      }
    }
  }

  function drawerEl() { return document.querySelector(".pv-drawer"); }

  function updateDrawer(highlightId) {
    const el = drawerEl();
    if (!el || !st.drawer) return;
    const d = st.drawer;
    if (d.err) {
      el.innerHTML = `<div class="pv-drawer-body">${errCard(d.err)}
        <button class="btn btn--ghost" id="pv-drawer-close">关闭</button></div>`;
      bindDrawerShell(el);
      return;
    }
    if (!d.project) return; // 仍在读取
    const p = d.project;
    const warn = endWarn(p);
    const fieldRow = (k, v) => v ? `<div class="pv-fld"><span>${k}</span><b>${esc(v)}</b></div>` : "";
    const notesHtml = !d.notes.length
      ? `<div class="pv-none">还没有过程记录</div>
         <div class="pv-none-act"><button class="btn btn--sm" id="pv-record-first">记录第一次动作</button></div>`
      : timeLine(d.notes, d.atts, highlightId);
    el.innerHTML = `
      <header class="pv-drawer-head">
        <span class="pv-badge pv-badge--${esc(p.stage)}">${esc(STAGE_ZH[p.stage] || p.stage)}</span>
        <h3 class="pv-drawer-name">${esc(p.name)}</h3>
        ${warn === "over" ? '<span class="pv-flag pv-flag--danger">逾期</span>' : warn === "due" ? '<span class="pv-flag pv-flag--warn">临期</span>' : ""}
        <span class="pv-drawer-acts">
          <button class="btn btn--ghost btn--sm" id="pv-drawer-create">手动建档</button>
          <button class="pv-x" id="pv-drawer-close" aria-label="关闭">×</button>
        </span>
      </header>
      <div class="pv-drawer-body">
        <section class="pv-fields">
          ${fieldRow("客户", p.client)}${fieldRow("地区", p.area)}${fieldRow("负责人", p.owner)}
          ${fieldRow("计划开始", p.start_date ? dateZh(p.start_date) : "")}${fieldRow("预期结束", p.expect_end_date ? dateZh(p.expect_end_date) : "")}
          ${p.summary ? `<div class="pv-fld pv-fld--full"><span>简介</span><b>${esc(p.summary)}</b></div>` : ""}
        </section>
        <section class="pv-notes">
          <h4>过程记录 <span class="mini">${d.notes.length} 条</span></h4>
          ${notesHtml}
        </section>
      </div>
      <footer class="pv-drawer-foot">
        <button class="btn" id="pv-record">记录过程</button>
      </footer>`;
    bindDrawerShell(el);
    if (highlightId) {
      const row = el.querySelector(`[data-note="${esc(highlightId)}"]`);
      if (row) { row.classList.add("pv-new"); row.scrollIntoView({ block: "center" }); setTimeout(() => row.classList.remove("pv-new"), 1500); }
    }
  }

  function bindDrawerShell(el) {
    el.querySelector("#pv-drawer-close")?.addEventListener("click", () => { el.closest(".pv-mask").remove(); st.drawer = null; });
    el.querySelector("#pv-drawer-create")?.addEventListener("click", () => openNoteModal({ mode: "create" })); // §5.4：抽屉顶部右侧手动建档
    el.querySelector("#pv-record")?.addEventListener("click", () => openNoteModal({ mode: "note", projectId: st.drawer?.project?.id }));
    el.querySelector("#pv-record-first")?.addEventListener("click", () => openNoteModal({ mode: "note", projectId: st.drawer?.project?.id }));
  }

  /** §5.2 时间线（项目内视图）：竖线 2px + 节点 10px；日期含星期；摘要 ≤3 行；
   *  缩略 ≤4 张超出 +N；附件数仅 >0 显示。照片文件缺失显示占位方块 + 角标「缺失」。 */
  function timeLine(notes, atts, highlightId) {
    const asc = [...notes].sort((a, b) => a.occurred_at.localeCompare(b.occurred_at));
    return `<div class="pv-tl">${asc.map((n) => {
      const list = atts[n.id];
      const imgs = (list || []).filter((a) => isImgExt(a.ext));
      const others = (list || []).filter((a) => !isImgExt(a.ext));
      const shown = imgs.slice(0, 4);
      const more = imgs.length - shown.length;
      const thumbs = list === null
        ? `<span class="pv-thumb pv-thumb--miss" title="附件列表读取失败">附件读取失败</span>`
        : shown.map((a) => {
            const src = attSrc(a, dataDirCache);
            return src
              ? `<img class="pv-thumb" data-view="${esc(a.id)}" src="${esc(src)}" alt="${esc(a.file_name)}"
                   onerror="this.replaceWith(Object.assign(document.createElement('span'),{className:'pv-thumb pv-thumb--miss',textContent:'缺失'}))">`
              : `<span class="pv-thumb pv-thumb--miss">缺失</span>`;
          }).join("");
      const meta = [
        n.place ? esc(n.place) : "",
        imgs.length ? `照片 ${imgs.length}` : "",
        others.length ? `附件 ${others.length}` : "",
      ].filter(Boolean).join(" · ");
      return `
        <div class="pv-tl-item${highlightId === n.id ? " pv-new" : ""}" data-note="${esc(n.id)}">
          <span class="pv-tl-dot" aria-hidden="true"></span>
          <div class="pv-tl-card">
            <div class="pv-tl-date">${esc(dateZh(n.occurred_at))}${timePart(n.occurred_at) ? " " + esc(timePart(n.occurred_at)) : ""}${n.kind && n.kind !== "note" ? ` · ${esc(KIND_ZH[n.kind] || n.kind)}` : ""}</div>
            ${n.title ? `<div class="pv-tl-title">${esc(n.title)}</div>` : ""}
            ${n.body ? `<div class="pv-tl-body">${esc(n.body)}</div>` : ""}
            ${meta ? `<div class="pv-tl-meta">${meta}</div>` : ""}
            ${thumbs ? `<div class="pv-tl-thumbs">${thumbs}${more > 0 ? `<span class="pv-thumb pv-thumb--more">+${more}</span>` : ""}</div>` : ""}
          </div>
        </div>`;
    }).join("")}</div>`;
  }

  // ── 图片预览层（§5.1：点击缩略进预览层，可左右切换） ──────────────────────
  function openViewer(items, idx) {
    const draw = (i) => {
      const a = items[i];
      const src = attSrc(a, dataDirCache);
      box.innerHTML = `
        <div class="pv-viewer-top">
          <span>${esc(a.file_name)} · ${i + 1}/${items.length}</span>
          <button class="pv-x" id="pv-viewer-close" aria-label="关闭">×</button>
        </div>
        ${src ? `<img class="pv-viewer-img" src="${esc(src)}" alt="${esc(a.file_name)}">` : `<div class="pv-viewer-miss">本地文件缺失</div>`}
        <div class="pv-viewer-nav">
          <button class="btn btn--ghost btn--sm" id="pv-viewer-prev" ${i === 0 ? "disabled" : ""}>上一张</button>
          <button class="btn btn--ghost btn--sm" id="pv-viewer-next" ${i === items.length - 1 ? "disabled" : ""}>下一张</button>
        </div>`;
      box.querySelector("#pv-viewer-close").onclick = () => box.remove();
      box.querySelector("#pv-viewer-prev").onclick = () => i > 0 && draw(i - 1);
      box.querySelector("#pv-viewer-next").onclick = () => i < items.length - 1 && draw(i + 1);
    };
    const box = overlay("pv-viewer", "");
    box.addEventListener("mousedown", (e) => { if (e.target === box) box.remove(); });
    draw(idx);
  }

  // ── 弹层通用件 ─────────────────────────────────────────────────────────
  const fieldHtml = (key, label, req, inner) => `
    <label class="pv-field"><span>${label}${req ? ' <i class="pv-req">*</i>' : ""}</span>${inner}
      <em class="pv-hint" data-hint="${key}"></em></label>`;

  function hintOf(root, key, msg) {
    const em = root.querySelector(`[data-hint="${key}"]`);
    if (em) em.textContent = msg || "";
  }

  /** §5.1 录入弹层。两模式共用同一套字段与校验，不因入口不同而字段不同：
   *  mode "note"   —— 记录到既有项目（抽屉 footer「记录过程」）；
   *  mode "create" —— 手动建档（§5.4：项目字段切为「新建项目」+ 项目名 + 地区）。 */
  function openNoteModal(cfg) {
    const m = {
      mode: cfg.mode, projectId: cfg.projectId || "",
      images: [], files: [],
      saving: false,
    };
    const projOpts = () => (st.projects || [])
      .map((d) => `<option value="${esc(d.project.id)}"${d.project.id === m.projectId ? " selected" : ""}>${esc(d.project.name)}</option>`).join("");
    const own = overlay("pv-modal-mask", `
      <div class="pv-modal" role="dialog" aria-label="过程记录">
        <header class="pv-modal-head"><h3>${m.mode === "create" ? "手动建档" : "记录过程"}</h3>
          <button class="pv-x" id="pv-m-close" aria-label="关闭">×</button></header>
        <div class="pv-modal-body">
          ${m.mode === "create" ? `
            ${fieldHtml("pname", "新建项目 · 项目名", true, `<input class="pv-input" data-k="pname" type="text" maxlength="80" autocomplete="off">`)}
            ${fieldHtml("parea", "地区", false, `<input class="pv-input" data-k="parea" type="text" maxlength="40" autocomplete="off">`)}` : `
            ${fieldHtml("project", "归属项目", true, `<select class="pv-input" data-k="project">${projOpts()}</select>`)}
            <p class="pv-modal-note">新建项目请从项目详情抽屉右上角「手动建档」进入。</p>`}
          <div class="pv-row2">
            ${fieldHtml("date", "发生日期（可补录历史日期）", true, `<input class="pv-input" data-k="date" type="date" value="${todayStr()}">`)}
            ${fieldHtml("time", "发生时刻", false, `<input class="pv-input" data-k="time" type="time">`)}
          </div>
          ${fieldHtml("place", "地点", false, `<input class="pv-input" data-k="place" type="text" maxlength="60" autocomplete="off">`)}
          ${fieldHtml("body", "动作摘要", true, `
            <textarea class="pv-input pv-body" data-k="body" rows="4" maxlength="${BODY_MAX}"></textarea>
            <span class="pv-cnt"><b data-k="cnt">0</b>/${BODY_MAX}</span>`)}
          ${fieldHtml("stage", "阶段回写（选填，写回项目阶段）", false, `
            <select class="pv-input" data-k="stage">
              <option value="">不变</option>
              ${STAGES.map(([v, zh]) => `<option value="${v}">${zh}</option>`).join("")}
            </select>`)}
          <div class="pv-field">
            <span>图片（选填，≤${MAX_IMAGES} 张）</span>
            <div class="pv-pick-row">
              <button class="btn btn--ghost btn--sm" id="pv-pick-img">选择图片</button>
              <div class="pv-thumbs" data-k="imgs"></div>
            </div>
          </div>
          <div class="pv-field">
            <span>附件（选填，≤${MAX_FILES} 个）</span>
            <div class="pv-pick-row">
              <button class="btn btn--ghost btn--sm" id="pv-pick-file">选择文件</button>
              <div class="pv-files" data-k="files"></div>
            </div>
          </div>
        </div>
        <footer class="pv-modal-foot">
          <em class="pv-form-err" id="pv-m-err"></em>
          <button class="btn btn--ghost" id="pv-m-cancel">取消</button>
          <button class="btn" id="pv-m-save" disabled>保存记录</button>
        </footer>
      </div>`);
    const root = own.querySelector(".pv-modal");
    const err = root.querySelector("#pv-m-err");
    const save = root.querySelector("#pv-m-save");
    const val = (k) => root.querySelector(`[data-k="${k}"]`);
    const curStage = () => {
      if (m.mode !== "note") return "";
      const p = st.projects?.find((d) => d.project.id === m.projectId)?.project;
      return p ? p.stage : "";
    };

    const validate = (silentOn) => {
      const body = (val("body").value || "").trim();
      const date = val("date").value;
      const name = m.mode === "create" ? (val("pname").value || "").trim() : "";
      let ok = true;
      if (m.mode === "create") {
        const dup = (st.projects || []).some((d) => d.project.name === name);
        if (!name) { ok = false; if (!silentOn) hintOf(root, "pname", "项目名必填"); }
        else if (dup) { ok = false; if (!silentOn) hintOf(root, "pname", "已有同名项目"); }
        else hintOf(root, "pname", "");
      } else {
        m.projectId = val("project").value; // 以选择为准
        if (!m.projectId) ok = false;
      }
      if (!date) { ok = false; if (!silentOn) hintOf(root, "date", "发生日期必填"); }
      else hintOf(root, "date", "");
      if (!body) { ok = false; if (!silentOn) hintOf(root, "body", "内容必填"); }
      else if (body.length > BODY_MAX) { ok = false; }
      else hintOf(root, "body", "");
      save.disabled = !ok || m.saving;
      return ok;
    };
    // §5.1 校验态：未填必填项即置禁用保存；失焦后才显示红字（不打断输入）
    ["pname", "date", "body"].forEach((k) => {
      val(k)?.addEventListener("input", () => { validate(true); if (k === "body") val("cnt").textContent = String(val("body").value.length); });
      val(k)?.addEventListener("blur", () => validate(false));
    });
    val("project")?.addEventListener("change", () => validate(true));
    validate(true);

    const drawPicks = () => {
      const box = val("imgs");
      box.innerHTML = m.images.map((f, i) => {
        const src = fileSrc(f.path);
        return `<span class="pv-thumb-wrap">
          ${src ? `<img class="pv-thumb" src="${esc(src)}" alt="${esc(f.name)}"
            onerror="this.replaceWith(Object.assign(document.createElement('span'),{className:'pv-thumb pv-thumb--miss',textContent:'预览不可用'}))">`
            : `<span class="pv-thumb pv-thumb--miss">预览不可用</span>`}
          <button class="pv-del" data-i="${i}" aria-label="移除">×</button></span>`;
      }).join("");
      box.querySelectorAll(".pv-del").forEach((b) => b.addEventListener("click", () => {
        m.images.splice(Number(b.dataset.i), 1); drawPicks();
      }));
      const fb = val("files");
      fb.innerHTML = m.files.map((f, i) => `<span class="pv-chip">${esc(f.name)}<button class="pv-del pv-del--chip" data-i="${i}" aria-label="移除">×</button></span>`).join("");
      fb.querySelectorAll(".pv-del").forEach((b) => b.addEventListener("click", () => {
        m.files.splice(Number(b.dataset.i), 1); drawPicks();
      }));
    };

    const pick = async (kind) => {
      const dlg = window.__TAURI__?.dialog;
      if (!dlg || typeof dlg.open !== "function") { err.textContent = "当前环境不支持原生文件选择器。"; return; }
      const cap = kind === "img" ? MAX_IMAGES - m.images.length : MAX_FILES - m.files.length;
      if (cap <= 0) { err.textContent = kind === "img" ? `图片已达上限 ${MAX_IMAGES} 张` : `附件已达上限 ${MAX_FILES} 个`; return; }
      try {
        const picked = await dlg.open({
          multiple: true, directory: false, title: kind === "img" ? "选择图片（可多选）" : "选择附件（可多选）",
          filters: kind === "img" ? [{ name: "图片", extensions: IMG_EXTS }] : undefined,
        });
        const arr = Array.isArray(picked) ? picked : picked ? [picked] : [];
        if (!arr.length) return;
        const list = kind === "img" ? m.images : m.files;
        arr.slice(0, cap).forEach((p) => list.push({ path: p, name: String(p).split(/[\\/]/).pop() }));
        drawPicks();
      } catch (e) { err.textContent = "选择文件失败：" + e; }
    };
    root.querySelector("#pv-pick-img").addEventListener("click", () => pick("img"));
    root.querySelector("#pv-pick-file").addEventListener("click", () => pick("file"));

    const close = () => own.remove();
    root.querySelector("#pv-m-close").onclick = close;
    root.querySelector("#pv-m-cancel").onclick = close;

    root.querySelector("#pv-m-save").addEventListener("click", async () => {
      if (!validate(false) || m.saving) return;
      m.saving = true;
      save.disabled = true;
      save.textContent = "保存中…";
      err.textContent = "";
      try {
        let projectId = m.projectId;
        if (m.mode === "create") {
          const name = val("pname").value.trim();
          const area = val("parea").value.trim() || null;
          const stageSel = val("stage").value || null;
          const r = await invoke("project_create", {
            draft: { name, client: null, area, stage: stageSel ? STAGE_SERDE[stageSel] : null, owner: null, summary: null, start_date: null, expect_end_date: null },
            year: YEAR,
          });
          projectId = r.project.id;
        }
        const body = val("body").value.trim();
        const note = await invoke("project_note_create", {
          draft: {
            project_id: projectId,
            occurred_date: val("date").value,
            occurred_time: val("time").value || null,
            title: body.split("\n")[0].slice(0, 40),
            body,
            place: val("place").value.trim() || null,
            kind: null,
          },
          actor: "ui",
        });
        // 附件逐个落盘（sha256 跨记录去重由后端负责；失败只计数，不回滚记录——§5.1 口径）
        let imgFail = 0, fileFail = 0;
        for (let i = 0; i < m.images.length; i++) {
          try {
            await invoke("project_attach", { noteId: note.note.id, sourcePath: m.images[i].path, fileName: m.images[i].name, sortNo: i });
          } catch { imgFail++; }
        }
        for (let i = 0; i < m.files.length; i++) {
          try {
            await invoke("project_attach", { noteId: note.note.id, sourcePath: m.files[i].path, fileName: m.files[i].name, sortNo: 100 + i });
          } catch { fileFail++; }
        }
        // 阶段回写（§5.1：阶段变更写回任务；记录模式下带全量字段防清空）
        const stageSel = val("stage").value;
        if (stageSel && stageSel !== curStage()) {
          try {
            const p = st.projects?.find((d) => d.project.id === projectId)?.project;
            if (p) {
              await invoke("project_update", {
                id: projectId,
                draft: {
                  name: p.name, client: p.client, area: p.area, stage: STAGE_SERDE[stageSel],
                  owner: p.owner, summary: p.summary, start_date: p.start_date, expect_end_date: p.expect_end_date,
                },
                actor: "ui",
              });
            }
          } catch { /* 阶段回写失败不吞掉保存成功：记录已落库 */ }
        }
        close();
        if (imgFail || fileFail) toast(`${imgFail} 张图片、${fileFail} 个附件未能保存，记录已保存`, 4000);
        else toast("已保存过程记录");
        st.loaded = false; st.projects = null; // 强制重读列表
        load();
        if (st.drawer || m.mode === "create") {
          const target = m.mode === "create" ? projectId : st.drawer?.id;
          if (target) openDrawer(target).then(() => updateDrawer(note.note.id));
        }
      } catch (e) {
        // §5.1 错误态：红字说明 + 保留用户已填全部内容
        err.textContent = "保存失败：" + e;
        m.saving = false;
        save.textContent = "保存记录";
        validate(true);
      }
    });
  }

  // ── 新建任务弹层（空态与标题行「新建任务」共用；仅建档不加记录） ──────────
  function openNewTask() {
    const own = overlay("pv-modal-mask", `
      <div class="pv-modal" role="dialog" aria-label="新建任务">
        <header class="pv-modal-head"><h3>新建任务</h3>
          <button class="pv-x" id="pv-n-close" aria-label="关闭">×</button></header>
        <div class="pv-modal-body">
          ${fieldHtml("name", "项目名", true, `<input class="pv-input" data-k="name" type="text" maxlength="80" autocomplete="off">`)}
          <div class="pv-row2">
            ${fieldHtml("client", "客户", false, `<input class="pv-input" data-k="client" type="text" maxlength="60">`)}
            ${fieldHtml("area", "地区", false, `<input class="pv-input" data-k="area" type="text" maxlength="40">`)}
          </div>
          <div class="pv-row2">
            ${fieldHtml("stage", "阶段", false, `<select class="pv-input" data-k="stage"><option value="planned" selected>筹备中</option>${STAGES.slice(1).map(([v, zh]) => `<option value="${v}">${zh}</option>`).join("")}</select>`)}
            ${fieldHtml("owner", "负责人", false, `<input class="pv-input" data-k="owner" type="text" maxlength="40">`)}
          </div>
          <div class="pv-row2">
            ${fieldHtml("start", "计划开始", false, `<input class="pv-input" data-k="start" type="date">`)}
            ${fieldHtml("end", "预期结束", false, `<input class="pv-input" data-k="end" type="date">`)}
          </div>
          ${fieldHtml("summary", "简介", false, `<textarea class="pv-input" data-k="summary" rows="2" maxlength="300"></textarea>`)}
        </div>
        <footer class="pv-modal-foot">
          <em class="pv-form-err" id="pv-n-err"></em>
          <button class="btn btn--ghost" id="pv-n-cancel">取消</button>
          <button class="btn" id="pv-n-save" disabled>新建</button>
        </footer>
      </div>`);
    const root = own.querySelector(".pv-modal");
    const err = root.querySelector("#pv-n-err");
    const save = root.querySelector("#pv-n-save");
    const val = (k) => root.querySelector(`[data-k="${k}"]`);
    const validate = (silentOn) => {
      const name = val("name").value.trim();
      const dup = (st.projects || []).some((d) => d.project.name === name);
      let ok = true;
      if (!name) { ok = false; if (!silentOn) hintOf(root, "name", "项目名必填"); }
      else if (dup) { ok = false; if (!silentOn) hintOf(root, "name", "已有同名项目"); }
      else hintOf(root, "name", "");
      if (val("start").value && val("end").value && val("end").value < val("start").value) {
        ok = false; if (!silentOn) hintOf(root, "end", "预期结束不能早于计划开始");
      } else hintOf(root, "end", "");
      save.disabled = !ok;
      return ok;
    };
    ["name", "end"].forEach((k) => {
      val(k).addEventListener("input", () => validate(true));
      val(k).addEventListener("blur", () => validate(false));
    });
    validate(true);
    const close = () => own.remove();
    root.querySelector("#pv-n-close").onclick = close;
    root.querySelector("#pv-n-cancel").onclick = close;
    root.querySelector("#pv-n-save").addEventListener("click", async () => {
      if (!validate(false)) return;
      save.disabled = true; save.textContent = "新建中…";
      try {
        const r = await invoke("project_create", {
          draft: {
            name: val("name").value.trim(),
            client: val("client").value.trim() || null,
            area: val("area").value.trim() || null,
            stage: STAGE_SERDE[val("stage").value],
            owner: val("owner").value.trim() || null,
            summary: val("summary").value.trim() || null,
            start_date: val("start").value || null,
            expect_end_date: val("end").value || null,
          },
          year: YEAR,
        });
        close();
        toast("已新建任务，可在详情抽屉记录第一次过程动作");
        st.loaded = false; st.projects = null;
        load();
        openDrawer(r.project.id);
      } catch (e) {
        err.textContent = "新建失败：" + e;
        save.disabled = false; save.textContent = "新建";
      }
    });
  }

  // ── 首启动迁移向导（预览只读 → 用户确认才执行；W2 已定口径） ──────────────
  function mountWizard(plan) {
    const own = overlay("pv-modal-mask", `
      <div class="pv-modal" role="dialog" aria-label="存量任务迁移">
        <header class="pv-modal-head"><h3>把存量任务归入项目跟踪</h3>
          <button class="pv-x" id="pv-w-close" aria-label="关闭">×</button></header>
        <div class="pv-modal-body">
          <p class="pv-modal-note">检测到 <b>${plan.pending_items}</b> 条既有任务还没有归属项目。确认迁移将按板块分组建立项目并回填归属——只改「归属项目」一列，不动板块与任何既有字段。</p>
          <div class="pv-wizard-rows">
            ${plan.rows.map((r) => `
              <div class="pv-wizard-row">
                <span class="pv-wizard-src">${esc(r.board_zh)} <span class="mini">× ${r.item_count} 条</span></span>
                <span class="pv-wizard-arrow" aria-hidden="true">→</span>
                <span class="pv-wizard-dst">${esc(r.project_name)}
                  <span class="pv-badge ${r.existing_project_id ? "pv-badge--reused" : "pv-badge--new"}">${r.existing_project_id ? "复用既有项目" : "新建项目"}</span></span>
              </div>`).join("")}
          </div>
        </div>
        <footer class="pv-modal-foot">
          <em class="pv-form-err" id="pv-w-err"></em>
          <button class="btn btn--ghost" id="pv-w-skip">暂不迁移</button>
          <button class="btn" id="pv-w-go">确认迁移</button>
        </footer>
      </div>`);
    const root = own.querySelector(".pv-modal");
    const err = root.querySelector("#pv-w-err");
    const go = root.querySelector("#pv-w-go");
    const close = () => own.remove();
    root.querySelector("#pv-w-close").onclick = close;
    root.querySelector("#pv-w-skip").onclick = () => { st.wizardDismissed = true; close(); };
    go.addEventListener("click", async () => {
      go.disabled = true; go.textContent = "迁移中…";
      try {
        const r = await invoke("project_migration_run", { plan, year: YEAR, actor: "ui" });
        close();
        toast(`迁移完成：新建 ${r.result.projects_created} 个项目、复用 ${r.result.projects_reused} 个、回填 ${r.result.items_linked} 条任务`);
        st.loaded = false; st.projects = null;
        load();
      } catch (e) {
        err.textContent = "迁移失败：" + e;
        go.disabled = false; go.textContent = "确认迁移";
      }
    });
  }

  // 时间线缩略点击 → 预览层（对既有附件）
  document.addEventListener("click", (e) => {
    const t = e.target.closest(".pv-tl-thumbs .pv-thumb[data-view]");
    if (!t || !st.drawer) return;
    const list = st.drawer.atts?.[t.closest(".pv-tl-item")?.dataset.note] || [];
    const imgs = list.filter((a) => isImgExt(a.ext) && attSrc(a, dataDirCache));
    const idx = imgs.findIndex((a) => a.id === t.dataset.view);
    if (idx >= 0) openViewer(imgs, idx);
  });
})();
