/* v3.4.0 · 待办版块（W5）界面模块
 * ============================================================================
 * 依据：设计规格 v3.4.0 §3.2（待办版块）/ §2.3（穿透路径）/ §5.4（手动建档两处定点）/
 *      §6.1（空态铁律）/ §6.2（加载与错误态）。
 * 数据源：todo_list / todo_create / todo_backfill（lib.rs W5 命令）；
 *         闭环复用 update_item（status="已闭环"，from_zh 认 Done）。
 *
 * 挂载与 project-views.js 同一模式（纯加法，避免并行轨道 git 硬冲突）：
 *   1. index.html 在 app.js 之前引入本文件；
 *   2. app.js 的 viewTodo() 只调 TV.view()、bindView 的 todo 分支只调 TV.bind()；
 *   3. app.js 的 render() 顶部调 TV.sync(state.view)——切走视图时拆除本模块浮层。
 *
 * 空态铁律（§6.1）：不编造任何数据——徽标只认 ext.v34_source 的真实值，
 * 「舆情」「网点」徽标枚举保留但本期无产生路径，不造数据（§3.4.1 口径）。
 * ============================================================================
 */
"use strict";

(function () {
  const TV = {};
  window.__TV = TV;

  // ── 常量 ─────────────────────────────────────────────────────────────────
  /** 闭环口径 = is_closed（Done | Cancelled）：已完成 / 已闭环 / 已取消。 */
  const CLOSED = new Set(["已完成", "已闭环", "已取消"]);
  /** 来源徽标（§3.2 三类）：ext.v34_source 的真实值才显示；人工新建无此键不显示。 */
  const SRC_ZH = { intel: "跟踪导出", douyin: "舆情", dealer: "网点" };
  /** 状态筛选四档（§3.2：全部 / 待处理 / 今天到期 / 已闭环）。 */
  const FILTERS = [["all", "全部"], ["open", "待处理"], ["today", "今天到期"], ["closed", "已闭环"]];
  const WEEK = ["周日", "周一", "周二", "周三", "周四", "周五", "周六"];

  // ── 状态 ─────────────────────────────────────────────────────────────────
  const st = {
    loaded: false, todos: null, error: "",
    filter: "all",            // all | open | today | closed
    detail: null,             // 待办 id | null（详情二级页）
    projects: null,           // 归属项目下拉缓存（project_list）
    detailData: null, detailErr: "",
  };

  // ── 工具（同 PV：app.js 全局在调用期已就绪） ──────────────────────────────
  const $all = (sel, root) => Array.from((root || document).querySelectorAll(sel));
  const todayStr = () => {
    const d = new Date();
    return `${d.getFullYear()}-${String(d.getMonth() + 1).padStart(2, "0")}-${String(d.getDate()).padStart(2, "0")}`;
  };
  const isClosed = (t) => CLOSED.has(t.status || "");
  // toast / invoke / esc / YEAR / gotoView 均为 app.js 全局（执行期已就绪，同 PV 模式）

  // ── 数据加载 ─────────────────────────────────────────────────────────────
  async function load(force) {
    if (st.loaded && !force) return;
    st.error = "";
    try {
      const r = await invoke("todo_list", { year: YEAR });
      st.todos = r.todos || [];
      st.loaded = true;
    } catch (e) {
      // 错误态不降级（§6.1 铁律③）：读取失败 ≠ 待办池为空
      st.error = String(e);
    }
    refresh();
  }

  function refresh() {
    const main = document.getElementById("main");
    if (!main || !document.querySelector('[data-view="todo"][aria-current="true"]')) return;
    main.innerHTML = TV.view();
    TV.bind();
  }

  // ── 行渲染 ───────────────────────────────────────────────────────────────
  /** 截止日呈现：逾期 danger / 今日 warn（§2.2 待办到期口径，互斥取更严重）。 */
  function dueBadge(t) {
    if (!t.dueDate) return `<span class="td-due td-due--none">无截止</span>`;
    const d = t.dueDate;
    const cls = !isClosed(t) && d < todayStr() ? "td-due--over"
      : !isClosed(t) && d === todayStr() ? "td-due--today" : "";
    return `<span class="td-due ${cls}">${esc(d.slice(5))}</span>`;
  }

  /** 第二行：来源徽标 + 项目名 › 当前环节（§3.2 穿透是核心）。 */
  function subLine(t) {
    const src = t.ext && t.ext.v34_source ? SRC_ZH[t.ext.v34_source] : "";
    const badge = src ? `<span class="td-src td-src--${esc(t.ext.v34_source)}">${esc(src)}</span>` : "";
    if (t.projectId && t.projectName && t.projectAlive) {
      return `${badge}<button class="td-link" data-pv-open="${esc(t.projectId)}">${esc(t.projectName)} › ${esc(t.projectStage || "")}</button>`;
    }
    if (t.projectId && !t.projectAlive) {
      // 数据缺失态（§3.2）：原项目已不存在 → 置灰显示，不隐藏该行，仍可闭环
      return `${badge}<span class="td-dead">原项目已不存在</span>`;
    }
    return badge ? badge : `<span class="td-nolink">未关联项目</span>`;
  }

  function rowHtml(t) {
    const pri = t.priority ? `<span class="td-pri td-pri--${esc(t.priority)}" title="优先级 ${esc(t.priority)}"></span>` : "";
    return `
    <li class="td-row${isClosed(t) ? " td-row--closed" : ""}">
      ${pri}
      <div class="td-main">
        <div class="td-title">${esc(t.title)}</div>
        <div class="td-sub">${subLine(t)}</div>
      </div>
      <div class="td-side">
        ${dueBadge(t)}
        <button class="btn btn--ghost btn--sm" data-td-detail="${esc(t.id)}">详情</button>
      </div>
    </li>`;
  }

  function listView() {
    if (!st.loaded && !st.error) {
      // 加载态（§3.2）：3 条骨架行，行高与真实一致
      return `<section class="td-wrap"><div class="td-head"><h2>待办</h2></div>
        <ul class="td-list">${'<li class="td-row td-skel"></li>'.repeat(3)}</ul></section>`;
    }
    if (st.error) {
      // 错误态（§3.2）：同任务版块错误卡，文案改「读取待办失败」
      return `<section class="td-wrap"><div class="pv-err">
        <h3>读取待办失败</h3>
        <p class="pv-err-code">${esc(st.error).slice(0, 200)}</p>
        <button class="btn" id="td-retry">重试</button>
        <button class="btn btn--ghost td-go-diag">查看诊断</button>
      </div></section>`;
    }
    const todos = st.todos || [];
    if (!todos.length) {
      // 空态（§3.2 实测文案逐字）：不显示示例待办
      return `<section class="td-wrap"><div class="pv-empty">
        <h3>待办池是空的</h3>
        <p>待办不会凭空出现：给任务填下次计划日期、把跟踪推进到停滞、或从舆情与网点转一条待办，这里就会实时长出来。</p>
        <div class="pv-empty-acts">
          <button class="btn" id="td-empty-create">手动建档</button>
          <button class="btn btn--ghost" id="td-empty-goto">去建任务</button>
        </div>
      </div></section>`;
    }
    const today = todayStr();
    const shown = todos.filter((t) =>
      st.filter === "closed" ? isClosed(t)
      : st.filter === "open" ? !isClosed(t)
      : st.filter === "today" ? !isClosed(t) && t.dueDate === today
      : true);
    return `<section class="td-wrap">
      <div class="td-head"><h2>待办</h2>
        <button class="btn" id="td-new">新建待办</button></div>
      <div class="td-filters">${FILTERS.map(([v, zh]) =>
        `<button class="td-f${st.filter === v ? " is-on" : ""}" data-td-f="${v}">${zh}</button>`).join("")}</div>
      <ul class="td-list">${shown.length ? shown.map(rowHtml).join("")
        : `<li class="td-none">该筛选下没有待办。</li>`}</ul>
    </section>`;
  }

  // ── 详情二级页（§2.1：返回条 + 标题行 + 内容三段式） ─────────────────────
  function detailView() {
    const t = st.detailData;
    if (!t) return listView();
    return `<section class="td-wrap td-detail">
      <div class="td-back"><button class="btn btn--ghost btn--sm" id="td-back">← 返回待办列表</button></div>
      <div class="td-head">
        <h2>${esc(t.title)}</h2>
        <!-- §5.4 第二处手动建档入口（两处定点，不做第三处） -->
        <button class="btn btn--ghost btn--sm" id="td-create">手动建档</button>
      </div>
      <div class="td-detail-body">
        <dl class="td-kv">
          <dt>状态</dt><dd>${isClosed(t) ? "已闭环" : "待处理"}</dd>
          <dt>截止日</dt><dd>${esc(t.dueDate || "—")}</dd>
          <dt>下一步</dt><dd>${esc(t.nextAction || "—")}</dd>
          <dt>负责人</dt><dd>${esc(t.owner || "—")}</dd>
          <dt>关联项目</dt><dd>${t.projectId && t.projectName && t.projectAlive
            ? `<button class="td-link" data-pv-open="${esc(t.projectId)}">${esc(t.projectName)} › ${esc(t.projectStage || "")}</button>`
            : t.projectId ? `<span class="td-dead">原项目已不存在</span>` : "—"}</dd>
          <dt>来源</dt><dd>${t.ext && t.ext.v34_source ? esc(SRC_ZH[t.ext.v34_source] || t.ext.v34_source) : "人工新建"}</dd>
        </dl>
        ${isClosed(t) ? "" : `<button class="btn" id="td-close">标记闭环</button>`}
      </div>
    </section>`;
  }

  // ── 视图入口 ─────────────────────────────────────────────────────────────
  TV.view = function view() {
    if (!st.loaded && !st.error) load(); // 首载（异步，先出骨架）
    return st.detail ? detailView() : listView();
  };

  TV.bind = function bind() {
    $all("#td-retry").forEach((b) => b.addEventListener("click", () => load(true)));
    $all(".td-go-diag").forEach((b) => b.addEventListener("click", () => gotoView("settings")));
    $all("#td-empty-goto").forEach((b) => b.addEventListener("click", () => gotoView("project")));
    // 空态「手动建档」（§5.4 同一弹层 + afterCreate 自动生成指向该项目的待办）
    $all("#td-empty-create, #td-create").forEach((b) => b.addEventListener("click", () => {
      if (!window.__PV) return;
      window.__PV.openCreateModal({
        afterCreate: async (projectId, createdName) => {
          try {
            await invoke("todo_create", { title: `跟进：${createdName}`, dueDate: null, projectId, owner: null });
            toast("已建档并生成一条关联待办");
            st.loaded = false; st.detail = null; st.detailData = null;
            load(true);
          } catch (e) { toast("建档成功，但待办生成失败：" + e, 4000); }
        },
      });
    }));
    $all("[data-td-f]").forEach((b) => b.addEventListener("click", () => {
      st.filter = b.dataset.tdF; refresh();
    }));
    $all("[data-td-detail]").forEach((b) => b.addEventListener("click", () => openDetail(b.dataset.tdDetail)));
    $all("#td-back").forEach((b) => b.addEventListener("click", () => { st.detail = null; st.detailData = null; refresh(); }));
    $all("#td-close").forEach((b) => b.addEventListener("click", async (e) => {
      const btn = e.target; btn.disabled = true; btn.textContent = "闭环中…";
      try {
        await invoke("update_item", { itemId: st.detail, progressPct: null, status: "已闭环", note: null });
        toast("已闭环");
        st.loaded = false; st.detail = null; st.detailData = null;
        load(true);
      } catch (err) {
        toast("闭环失败：" + err, 4000);
        btn.disabled = false; btn.textContent = "标记闭环";
      }
    }));
    // 穿透（§2.3）：待办 → 所属项目详情（PV 抽屉）；点击行内项目名即跳
    $all("[data-pv-open]").forEach((b) => b.addEventListener("click", () => {
      const id = b.dataset.pvOpen;
      if (window.__PV) window.__PV.openProjectById(id);
    }));
    $all("#td-new").forEach((b) => b.addEventListener("click", openNewModal));
  };

  function openDetail(id) {
    const t = (st.todos || []).find((x) => x.id === id);
    if (!t) return;
    st.detail = id; st.detailData = t;
    refresh();
  }

  /** app.js render() 每帧调用：切走待办版块时拆除本模块浮层。 */
  TV.sync = function sync(view) {
    if (view !== "todo") { $all(".td-mask").forEach((m) => m.remove()); st.detail = null; st.detailData = null; }
  };

  // ── 新建待办弹层（§3.2 标题行「新建待办」） ───────────────────────────────
  async function openNewModal() {
    if (!st.projects) {
      try { const r = await invoke("project_list", { year: YEAR, includeArchived: false }); st.projects = r.projects || []; }
      catch { st.projects = []; }
    }
    const opts = st.projects
      .map((d) => `<option value="${esc(d.project.id)}">${esc(d.project.name)}</option>`).join("");
    const mask = document.createElement("div");
    mask.className = "pv-mask td-mask";
    mask.innerHTML = `
      <div class="pv-modal td-modal" role="dialog" aria-label="新建待办">
        <header class="pv-modal-head"><h3>新建待办</h3>
          <button class="pv-x" id="td-m-close" aria-label="关闭">×</button></header>
        <div class="pv-modal-body">
          <label class="pv-field"><span>待办标题</span>
            <input class="pv-input" id="td-m-title" type="text" maxlength="80"></label>
          <div class="pv-row2">
            <label class="pv-field"><span>截止日</span>
              <input class="pv-input" id="td-m-due" type="date"></label>
            <label class="pv-field"><span>归属项目（选填）</span>
              <select class="pv-input" id="td-m-proj"><option value="">不关联</option>${opts}</select></label>
          </div>
          <p class="pv-modal-note">人工新建的待办不显示来源徽标（§3.2）。</p>
          <p class="pv-form-err" id="td-m-err"></p>
        </div>
        <footer class="pv-modal-foot">
          <button class="btn btn--ghost" id="td-m-cancel">取消</button>
          <button class="btn" id="td-m-save">保存</button>
        </footer>
      </div>`;
    document.body.appendChild(mask);
    const $ = (s) => mask.querySelector(s);
    const close = () => mask.remove();
    $("#td-m-close").onclick = close;
    $("#td-m-cancel").onclick = close;
    $("#td-m-save").addEventListener("click", async () => {
      const title = $("#td-m-title").value.trim();
      const err = $("#td-m-err");
      if (!title) { err.textContent = "待办标题不能为空"; return; }
      const save = $("#td-m-save"); save.disabled = true; save.textContent = "保存中…";
      try {
        await invoke("todo_create", {
          title, dueDate: $("#td-m-due").value || null,
          projectId: $("#td-m-proj").value || null, owner: null,
        });
        close(); toast("已新建待办");
        st.loaded = false; load(true);
      } catch (e) {
        err.textContent = "保存失败：" + e;
        save.disabled = false; save.textContent = "保存";
      }
    });
  }
})();
