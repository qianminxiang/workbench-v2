/* v3.2.0 · 情报「项目进程」（同一项目阶段聚合展示）· 界面层轨道
 * ============================================================================
 * 依据：设计规格 v1.2（飞书文件 W238b0cSaowLWJxaANBcyV95nze）§1–§6，
 *      用户 2026-11-20 08:34 决策卡拍板「方案 A 主从双栏」。
 *
 * 本文件只做界面层，独立成模块，避免与并行数据接线轨（高级开发工程师）
 * 改动 app.js / lib.rs 时在 git 上硬冲突。挂载点两处，均为加法：
 *   1. index.html 在 app.js 之前引入本文件；
 *   2. app.js 的 viewIntel() 改为调度：state.intelMode 决定走本模块还是原有清单。
 *
 * ── 阶段口径（关键差异，必须看懂再改） ─────────────────────────────────────
 * 后端 crates/wb-core/src/intel_stage.rs 的 Stage 是 **5 段**：
 *   intent(意向) / review(评审) / contract(签约) / acceptance(验收) / operation(运维) + unknown
 * 规格 §2 要求的是 **6 段**：
 *   意向 → 采购公告 → 更正澄清 → 中标成交 → 合同验收 → 终止废标
 * 二者不是改名关系：后端把规格的「2 采购公告 + 3 更正澄清」并成 review，
 * 把「4 中标成交 + 5 合同验收」并成 contract，且**没有终止/废标段**。
 *
 * 因此本模块带一个 **界面层适配器** stage6Of()：在已有 stage code 之上，
 * 再用节点标题关键词把合并段拆回规格 6 段，并用标题关键词识别终止/废标侧状态。
 * 它不碰后端、不改库，只决定「这条公告画在哪一段」。
 * ⚠ 这是临时适配：后端若按规格直接产出 6 段（数据轨收口），
 *   本函数应降级为直读字段（见 stage6Of 内的 note6 分支），不要留着两套口径并存。
 * ========================================================================== */
"use strict";

(function () {
  const IP = {};

  // ── 六段定义（规格 §2 表格逐行）───────────────────────────────────────────
  // order 用于点条与轴的推进顺序；terminate 是侧状态，order = -1 表示不参与推进。
  IP.STAGES = [
    { key: "intent", label: "意向 / 需求", short: "意向", order: 0 },
    { key: "publish", label: "采购公告", short: "采购", order: 1 },
    { key: "correct", label: "更正 / 澄清", short: "更正", order: 2 },
    { key: "award", label: "中标成交", short: "中标", order: 3 },
    { key: "contract", label: "合同 / 验收", short: "合同", order: 4 },
    { key: "terminate", label: "终止 / 废标", short: "终止", order: -1, side: true },
  ];

  // 标题关键词 → 六段（顺序敏感：先判更正/终止这类"覆盖型"词，再判主类型）
  const KW = {
    terminate: ["终止公告", "终止", "废标", "流标", "废标公告", "采购失败"],
    correct: ["更正公告", "更正", "变更公告", "澄清", "答疑", "延期公告", "延期", "补遗"],
    intent: ["采购意向", "意向公开", "意向公告", "需求公示", "需求公告"],
    award: ["中标（成交）结果", "中标(成交)结果", "中标结果", "成交结果", "中选结果", "中标公告", "成交公告", "中选公告", "结果公告", "中标通知书"],
    contract: ["合同公告", "合同公示", "政府采购合同", "签约", "验收", "履约", "运维服务", "运维公告"],
    publish: ["招标公告", "采购公告", "公开招标", "邀请招标", "资格预审", "竞争性谈判", "竞争性磋商", "磋商公告", "谈判公告", "询价公告", "单一来源", "招标文件"],
  };

  const hasKw = (text, list) => {
    const t = String(text || "");
    return list.some((k) => t.includes(k));
  };

  /** 界面层适配：后端 5 段 + 标题关键词 → 规格 6 段 key。
   *  ctx 传 {stage, title, reason}。返回 {seg, why}：
   *   seg = "intent"|"publish"|"correct"|"award"|"contract"|"terminate"|null
   *   null = 判不出（阶段未知），不进点条也不进任何段，单列在轴末尾。
   */
  IP.stage6Of = function stage6Of(node) {
    const stage = String((node && node.stage) || "");
    const title = String((node && (node.title || "")) || "");
    const reason = String((node && node.stage_reason) || "");

    // ── 六段码表归一（跨轨一致的关键）────────────────────────────────────
    // 同一个仓里有两套阶段口径，命名还不一样：
    //   crates/wb-core/src/intel_segment.rs（六段 payload / CLI）: intent / tender / correction / award / contract / termination
    //   crates/wb-core/src/intel_stage.rs  （五段 / 现行 Tauri 命令）: intent / review / contract / acceptance / operation / unknown
    // 界面内部键沿用规格 §2 文案：intent / publish / correct / award / contract / terminate。
    // 若不做归一，数据轨把 Tauri 命令切到六段 payload 后，tender / correction / termination
    // 三个段会被静默判成「阶段未知」——界面照样能跑，只是那三段全空，最难看出来。
    const SIX_TO_UI = {
      intent: "intent", tender: "publish", correction: "correct",
      award: "award", contract: "contract", termination: "terminate",
      // 界面内部键也接受（幂等），便于将来直传
      publish: "publish", correct: "correct", terminate: "terminate",
    };

    // 路径 A：六段 payload 自带 segment 字段 → 语义无歧义，contract 在此只可能是「合同/验收」。
    const segField = String((node && (node.segment || node.segment_code)) || "");
    if (segField && SIX_TO_UI[segField]) {
      return { seg: SIX_TO_UI[segField], why: "六段 payload 的 segment 字段" };
    }

    // 路径 B：只有 stage 字段。**裸 contract 不能直采**：五段码 contract 并了规格 4+5，
    // 含「中标结果公告」这类多数派，直采会让「中标成交」段永久空 —— 必须留给下面的关键词拆分。
    const stageKey = SIX_TO_UI[stage];
    if (stageKey && stage !== "contract") {
      return { seg: stageKey, why: "后端直出六段口径" };
    }

    // 终止/废标：后端无此段，只能从标题判；命中即侧状态，不推进进程（规格 §2）。
    if (hasKw(title, KW.terminate)) return { seg: "terminate", why: "标题含终止/废标词" };

    if (stage === "review") {
      // 后端 review 并了规格 2+3：含更正类词归「更正 / 澄清」，否则归「采购公告」。
      return hasKw(title, KW.correct)
        ? { seg: "correct", why: "评审段内命中更正类词" }
        : { seg: "publish", why: "评审段内未命中更正类词" };
    }
    if (stage === "contract") {
      // 后端 contract 并了规格 4+5：合同/验收类词归「合同 / 验收」，否则归「中标成交」。
      return hasKw(title, KW.contract)
        ? { seg: "contract", why: "签约段内命中合同/验收类词" }
        : { seg: "award", why: "签约段内未命中合同/验收类词" };
    }
    if (stage === "intent") return { seg: "intent", why: "后端意向段" };
    if (stage === "acceptance" || stage === "operation") {
      // 六段最末实段是「合同 / 验收」；运维并入该段，节点 chip 仍显示后端原标签。
      return { seg: "contract", why: stage === "acceptance" ? "后端验收段" : "后端运维段并入合同/验收" };
    }
    if (hasKw(title, KW.intent)) return { seg: "intent", why: "阶段未知但标题含意向词" };
    if (hasKw(title, KW.award)) return { seg: "award", why: "阶段未知但标题含中标/成交词" };
    if (hasKw(title, KW.publish)) return { seg: "publish", why: "阶段未知但标题含采购公告词" };
    if (reason) return { seg: null, why: "阶段未知（改判记录：" + reason + "）" };
    return { seg: null, why: "阶段未知" };
  };

  /** 把一个 ProjectAggregate 折算成六段视图模型。
   *  返回 {segs:[{def, nodes:[]}×6], unknown:[节点], currentSeg, terminated, skipped:[段key]} */
  IP.projectView = function projectView(p) {
    const nodes = (p && p.nodes) || [];
    const segs = IP.STAGES.map((def) => ({ def, nodes: [] }));
    const unknown = [];
    const byKey = {};
    segs.forEach((s) => { byKey[s.def.key] = s; });

    nodes.forEach((n) => {
      const r = IP.stage6Of(n);
      if (r.seg) {
        byKey[r.seg].nodes.push(n);
      } else {
        unknown.push(n);
      }
    });

    // 段内排序：日期升序（同一天保持后端给定次序）
    segs.forEach((s) => {
      s.nodes.sort((a, b) => String(a.date || "").localeCompare(String(b.date || "")));
    });

    const terminated = byKey.terminate.nodes.length > 0;
    // 当前段 = 有公告的最靠后实段（终止不参与推进，规格 §2）
    let currentSeg = null;
    for (const s of segs) {
      if (s.def.side) continue;
      if (s.nodes.length) currentSeg = s.def.key;
    }
    // 跳过的段 = 位于 currentSeg 之前却没有任何公告的实段（画成"跳过"，不画成 0）
    const skipped = [];
    if (currentSeg) {
      const curOrder = IP.STAGES.find((d) => d.key === currentSeg).order;
      for (const s of segs) {
        if (s.def.side) continue;
        if (s.def.order < curOrder && !s.nodes.length) skipped.push(s.def.key);
      }
    }
    return { segs, unknown, currentSeg, terminated, skipped };
  };

  window.__IP = IP;
})();

/* ── 模块内工具与本地占位存储 ─────────────────────────────────────────────── */
(function () {
  const IP = window.__IP;

  const esc = (s) => String(s ?? "").replace(/[&<>"']/g, (c) =>
    ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" }[c]));

  IP.esc = esc;

  /** 项目键归一化（规格 §2：剔除「（第 N 次）」「重新招标」「废标重招」等后缀与全半角差异）。
   *  只做规则化清洗，**不做模糊匹配**——近似命中阈值归引擎侧（口径见任务书）。 */
   IP.normKey = function normKey(org, name) {
    let n = String(name || "");
    n = n.replace(/[\uFF01-\uFF5E]/g, (ch) => String.fromCharCode(ch.charCodeAt(0) - 0xFEE0)); // 全角→半角
    n = n.replace(/（/g, "(").replace(/）/g, ")").replace(/：/g, ":").replace(/／/g, "/");
    // 循环剥离迭代类后缀（可能叠加：重新招标 + （第二次））
    const SUFFIX = [/\s*\(第[一二三四五六七八九十\d]+次\)/g, /\s*第[一二三四五六七八九十\d]+次/g,
      /\s*\(重新招标\)/g, /\s*重新招标/g, /\s*\(废标重招\)/g, /\s*废标重招/g,
      /\s*\(二次\)/g, /\s*\(三次\)/g];
    for (let i = 0; i < 4; i++) {
      const before = n;
      SUFFIX.forEach((re) => { n = n.replace(re, ""); });
      if (n === before) break;
    }
    n = n.replace(/[\s\u3000]+/g, "").toLowerCase();
    const o = String(org || "").replace(/[\uFF01-\uFF5E]/g, (ch) => String.fromCharCode(ch.charCodeAt(0) - 0xFEE0))
      .replace(/[\s\u3000]+/g, "").toLowerCase();
    return o + "|" + n;
  };

  /** 手动档案读取/写入：主路径走后端命令（`manual_project_list` / `manual_project_create`，
   *  落库与 wbd CLI 同一批 wb-core 实现，进引擎去重池）；localStorage 只在
   *  invoke 不可用（老后端 / 纯浏览器调试）时作降级兜底，不再是主路径。 */
   const LS_KEY = "wb.intel.manualProjects.v1";
   /** localStorage 读取：只作为降级兜底（后端命令不可用时），不再是主路径。
    *  挂在 IP 上是因为建档/对照合并的降级路径在别的 IIFE 里也要用。 */
   IP.loadManualLS = function loadManualLS() {
    try {
      const raw = window.localStorage.getItem(LS_KEY);
      const arr = raw ? JSON.parse(raw) : [];
      return Array.isArray(arr) ? arr : [];
    } catch (_) { return []; }
  };
   /** 手动档案读取（主路径 = 后端 `manual_project_list`，与 wbd CLI 同一落库实现）；
    *  invoke 不可用（老后端 / 纯浏览器调试）时降级 localStorage。
    *  已合并（merged_into 非空）的档案不再作为独立卡显示——公告侧聚合卡已在列表里。 */
   IP.loadManual = async function loadManual() {
    const t = window.__TAURI__ && window.__TAURI__.core;
    if (t && typeof t.invoke === "function") {
      try {
        const r = await t.invoke("manual_project_list", {});
        const arr = (r && r.manuals) || [];
        if (Array.isArray(arr)) return arr.filter((m) => !m.merged_into);
      } catch (_) { /* 命令不存在或后端异常：降级 localStorage */ }
    }
    return IP.loadManualLS().filter((m) => !m.merged_into && !m.merged_at);
  };
   IP.saveManual = function saveManual(arr) {
    try { window.localStorage.setItem(LS_KEY, JSON.stringify(arr)); return true; }
    catch (_) { return false; }   // 存储不可用：调用方据返回值如实提示，不静默成功
  };

  /** 转待办的节点键重建：`IntelItem.key` 的构造口径是 `site_id|url`
   *  （见 crates/wb-core/src/intel.rs：`key: format!("{}|{}", spec.id, link)`，
   *  而 ProjectNode 的 `url` 字段就取自同一条 `it.url`）。
   *  后端若改变该构造口径，改判会返回「条目不存在」——由调用方如实展示，不吞掉。 */
   IP.itemKeyOf = function itemKeyOf(n) { return String(n.source_id || "") + "|" + String(n.url || ""); };

  const fmtDate = (d) => String(d || "").slice(0, 10);
  IP.fmtDate = fmtDate;

  /** 「N 天前」：只按日历日差，空值返回空串（不写「0 天前」冒充新鲜）。 */
  IP.daysAgo = function daysAgo(dateStr) {
    if (!dateStr) return "";
    const d = new Date(String(dateStr).slice(0, 10) + "T00:00:00");
    if (Number.isNaN(d.getTime())) return "";
    const today = new Date();
    const t0 = Date.UTC(d.getFullYear(), d.getMonth(), d.getDate());
    const t1 = Date.UTC(today.getFullYear(), today.getMonth(), today.getDate());
    const n = Math.round((t1 - t0) / 86400000);
    if (n <= 0) return n === 0 ? "今天" : "";
    return n + " 天前";
  };
})();

/* ── 6 点阶段点条 StageDots（规格 §5） ─────────────────────────────────────── */
(function () {
  const IP = window.__IP;
  const esc = IP.esc;

  /** 画点条。view = IP.projectView() 结果。
   *  五态：已完成(done) / 当前(now) / 未达(future) / 跳过(skip) / 终止侧标。
   *  终止是侧状态：整条点条后加 6px 红方块，**不占第 6 个点**（第 6 段是侧段）。 */
  IP.dots = function dots(view, opts) {
    const o = opts || {};
    const real = IP.STAGES.filter((d) => !d.side);
    const curOrder = view.currentSeg
      ? IP.STAGES.find((d) => d.key === view.currentSeg).order
      : -1;
    const dots = real.map((d) => {
      const seg = view.segs.find((s) => s.def.key === d.key);
      let st;
      if (d.order < curOrder) {
        st = seg.nodes.length ? "done" : "skip";      // 无公告 → 跳过（斜线 + tooltip），不画成 0
      } else if (d.order === curOrder) {
        st = "now";
      } else {
        st = "future";
      }
      const tip = st === "skip" ? "此阶段无公告" : (seg.nodes.length ? d.label + "：" + seg.nodes.length + " 条公告" : d.label);
      const inner = `<span class="ip-dot ip-dot--${st}" title="${esc(tip)}" aria-hidden="true"></span>`;
      const wrap = st === "now"
        ? `<span class="ip-dotwrap ip-dotwrap--now" title="${esc(tip)}">${inner}</span>`
        : `<span class="ip-dotwrap" title="${esc(tip)}">${inner}</span>`;
      return wrap;
    });
    const joined = dots.map((d, i) => (i === 0 ? d : `<span class="ip-conn" aria-hidden="true"></span>` + d)).join("");
    const term = view.terminated ? `<span class="ip-term" title="该项目出现终止/废标公告（侧状态，不参与进程推进）" aria-hidden="true"></span>` : "";
    // 点条必须同时有文字（规格 §6 无障碍）：当前段 chip 写阶段名，不只靠颜色。
    const curLabel = view.currentSeg
      ? IP.STAGES.find((d) => d.key === view.currentSeg).label
      : "阶段未知";
    const labelHtml = o.hideLabel ? "" : `<span class="ip-chip${view.currentSeg ? " ip-chip--cur" : ""}">${esc(curLabel)}</span>`;
    return `<div class="ip-dots" role="img" aria-label="阶段进程：当前 ${esc(curLabel)}${view.terminated ? "，已终止" : ""}">${joined}${term}${labelHtml}</div>`;
  };

  /** 左栏项目聚合卡（规格 §4：项目名 2 行截断 / 采购人·地区·预算 / 点条 / 底部元信息）。 */
  IP.kcard = function kcard(p, opts) {
    const o = opts || {};
    const view = p.__view || IP.projectView(p);
    const manual = !!p.manual;
    const name = p.project_name || "(无项目名)";
    const org = p.org || "采购人缺失";
    const area = p.area || "地区缺失";
    const budget = p.budget || p.amount || "";
    const nodeCount = (p.nodes || []).length;
    const newest = p.last_date || "";
    const ago = IP.daysAgo(newest);
    const meta = manual
      ? "暂无公告 · 手动建档"
      : `${nodeCount} 条公告${newest ? " · 最新 " + (ago || fmtDateOrDash(newest)) : ""}`;
    const hit = (p.hit_words || []).slice(0, 2)
      .map((w) => `<span class="ip-chip ip-chip--hit">${esc(w)}</span>`).join("");
    return `<button class="ip-kcard" data-ip-key="${esc(p.project_key)}" aria-current="${o.selected ? "true" : "false"}">
      <div class="ip-kcard-head">
        <span class="ip-kname" title="${esc(name)}">${esc(name)}</span>
        ${manual ? '<span class="ip-chip ip-chip--manual">手动</span>' : ""}
      </div>
      <div class="ip-kmeta">${esc(org)} · ${esc(area)}${budget ? " · " + esc(budget) : ""}</div>
      ${IP.dots(view, { hideLabel: false })}
      <div class="ip-kfoot">${esc(meta)} ${hit}</div>
    </button>`;
  };

  function fmtDateOrDash(d) { return IP.fmtDate(d) || "日期缺失"; }
})();

/* ── 右栏：项目抬头 + 阶段导航 + 纵向六段进程轴（方案 A 焦点栏） ─────────────── */
(function () {
  const IP = window.__IP;
  const esc = IP.esc;

  const segByKey = (view, k) => view.segs.find((s) => s.def.key === k);

  /** 单条公告条目卡：日期 · 公告类型 chip · 标题 · 命中依据 · 原文链接（规格 §4）。 */
  IP.annRow = function annRow(n, segLabel) {
    const hit = String(n.stage_reason || "");
    const src = n.stage_source === "rule" ? "规则命中" : n.stage_source === "heuristic" ? "启发式兜底" : n.stage_source === "manual" ? "人工改判" : "阶段未知";
    return `<div class="ip-ann">
      <div class="ip-ann-title">${esc(n.title || "(无标题)")}</div>
      <div class="ip-ann-meta">
        <span>${esc(IP.fmtDate(n.date) || "日期缺失")}</span>
        <span class="ip-chip">${esc(segLabel)}</span>
        <span class="ip-chip${n.stage_source === "rule" ? "" : " ip-chip--warn"}">${esc(src)}</span>
        <span>来源：${esc(n.source_name || "—")}</span>
        <a class="ip-link" href="${esc(n.url || "#")}" target="_blank" rel="noopener">原文链接</a>
      </div>
      ${hit ? `<div class="ip-ann-hit">命中依据：${esc(hit)}</div>` : `<div class="ip-ann-hit">命中依据：未记录</div>`}
    </div>`;
  };

  /** 纵向进程轴：只画有公告的段 + 已跳过的段（跳过的段画成"跳过"，不画成 0 条）。 */
  IP.axis = function axis(p, view) {
    const FOLD_AT = 8;   // 单段超过 8 条折叠「展开其余 N 条」（规格 §5 极端项）
    const blocks = [];
    for (const s of view.segs) {
      if (!s.nodes.length) continue;
      const st = s.def.key === view.currentSeg ? "now" : (s.def.side ? "terminate" : "done");
      const foldHit = s.nodes.length > FOLD_AT;
      const shown = foldHit ? s.nodes.slice(0, FOLD_AT) : s.nodes;
      const rest = s.nodes.slice(FOLD_AT);
      blocks.push(`<section class="ip-seg" id="ip-seg-${esc(s.def.key)}" data-ceg-current="${st === "now" ? "true" : "false"}">
        <div class="ip-seg-head">
          <span class="ip-seg-node" data-st="${st}" aria-hidden="true"></span>
          <span class="ip-seg-title">${esc(s.def.label)}</span>
          <span class="ip-seg-count">${s.nodes.length} 条</span>
          ${s.def.side ? '<span class="ip-chip ip-chip--err">侧状态 · 不推进进程</span>' : ""}
          ${st === "now" ? '<span class="ip-chip ip-chip--cur">当前阶段</span>' : ""}
        </div>
        <div class="ip-seg-body">${shown.map((n) => IP.annRow(n, s.def.label)).join("")}
          ${foldHit ? `<div class="ip-ann" data-ip-fold="1" hidden>${rest.map((n) => IP.annRow(n, s.def.label)).join("")}</div>
          <button class="ip-btn--text ip-btn" data-ip-expand="${esc(s.def.key)}">展开其余 ${rest.length} 条</button>` : ""}
        </div>
      </section>`);
    }
    // 跳过段：显式写出来，避免「没发过公告」被读成「发了 0 条」（规格 §2 / §5）
    const skipBlocks = view.skipped.map((k) => {
      const def = IP.STAGES.find((d) => d.key === k);
      return `<section class="ip-seg" id="ip-seg-${esc(k)}" data-ceg-current="false">
        <div class="ip-seg-head">
          <span class="ip-seg-node" data-st="skip" aria-hidden="true"></span>
          <span class="ip-seg-title">${esc(def.label)}</span>
          <span class="ip-seg-count">此阶段无公告（已跳过）</span>
        </div>
      </section>`;
    }).join("");
    const unknown = view.unknown.length
      ? `<section class="ip-seg" id="ip-seg-unknown">
        <div class="ip-seg-head"><span class="ip-seg-node" data-st="skip" aria-hidden="true"></span>
          <span class="ip-seg-title">阶段未知</span><span class="ip-seg-count">${view.unknown.length} 条（判不出阶段，未计入进程）</span></div>
        <div class="ip-seg-body">${view.unknown.map((n) => IP.annRow(n, "阶段未知")).join("")}</div>
      </section>` : "";
    if (!blocks.length && !skipBlocks && !unknown) {
      return `<div class="ip-empty">该项目暂无公告。<br>引擎采集到该项目的第一条公告后，这里会出现进程轴。</div>`;
    }
    return `<div class="ip-axis">${blocks.join("")}${skipBlocks}${unknown}</div>`;
  };

  /** 阶段导航 chip：点击跳到该段锚点（无公告的段也能跳，落到跳过提示）。 */
  IP.navChips = function navChips(view) {
    return `<div class="ip-navchips" role="group" aria-label="阶段导航">${IP.STAGES.map((d) => {
      const seg = segByKey(view, d.key);
      const on = seg.nodes.length > 0;
      return `<button class="ip-navchip" data-ip-nav="${esc(d.key)}" aria-pressed="${d.key === view.currentSeg}">
        ${esc(d.label)}${on ? " · " + seg.nodes.length : ""}</button>`;
    }).join("")}</div>`;
  };

  /** 右侧抬头（项目名 / 采购人 / 地区 / 预算 / 聚合依据）。 */
  IP.headHtml = function headHtml(p, view) {
    const manual = !!p.manual;
    const name = p.project_name || "(无项目名)";
    const basis = manual
      ? "手动建档项目：无公告来源，阶段判定未启用"
      : `按「采购人 + 项目名归一化」聚合 ${(p.nodes || []).length} 条公告` +
        (p.skipped_stages ? ` · 跳段 ${p.skipped_stages}` : "");
    return `<div class="ip-head">
      <h2>${esc(name)}${manual ? ' <span class="ip-chip ip-chip--manual">手动</span>' : ""}</h2>
      <div class="ip-head-meta">
        采购人：${esc(p.org || "缺失")} · 地区：${esc(p.area || "缺失")}${(p.budget || p.amount) ? " · 预算：" + esc(p.budget || p.amount) : ""}
      </div>
      <div class="ip-head-basis">${esc(basis)}</div>
      ${manual ? `<div class="ip-head-basis">录入：${esc(p.note || "（无备注）")}${p.budget ? " · 预算 " + esc(p.budget) : ""}</div>` : ""}
    </div>`;
  };
})();

/* ── 数据载入（只读既有命令，不新增后端接口） ────────────────────────────── */
(function () {
  const IP = window.__IP;

  IP.state = {
    loading: false,
    error: "",          // 非空 = 读失败，如实显示（不伪装成"没有新项目"）
    projects: null,     // null = 未加载；[] = 真的没有项目
    selected: "",       // 选中项目的 project_key
    selScroll: {},      // 各项目右栏滚动位置记忆（规格 §4 交互）
    stageFilter: "",    // 阶段筛选（六段 key；空 = 全部）
    page: 1,            // 分批 200 项目分页 20/页（规格 §5 极端项）
    flash: "",          // 底部工具条提示 {tone, text}
  };

  const PAGE_SIZE = 20;

  /** 读项目聚合列表。`intel_projects_get` 不传 stage 时返回全部；阶段筛选在前端做——
   *  原因：后端 Stage::parse_filter 只认它自己的 5 段码，喂不进规格的六段 key，
   *  传错会直接报错（"stage ..."），比前端筛一次更容易误判成"没有项目"。 */
  IP.load = async function load() {
    const st = IP.state;
    st.loading = true;
    st.error = "";
    try {
      const r = await window.__TAURI__.core.invoke("intel_projects_get", { stage: "", limit: 0 });
      st.projects = (r && r.projects) || [];
    } catch (e) {
      st.projects = null;
      st.error = String(e || "读取失败");
    }
    // 手动档案并进列表（规格 A-6 口径 4：计入默认列表统计与计数，单独标「手动」chip）
    await IP.mergeManual();
    st.loading = false;
    return st.projects;
  };

  /** 手动档案 → 与引擎项目同形的聚合对象，插到列表最前（最近建档在上）。 */
  IP.mergeManual = async function mergeManual() {
    const st = IP.state;
    const base = (st.projects || []).filter((p) => !p.manual);
    const man = (await IP.loadManual()).map((m) => ({
      project_key: m.project_key,
      manual_id: m.manual_id,       // 合并/忽略/重开的定位键（后端主路径）
      agg_key: m.agg_key,           // 疑似关联检测的公告侧匹配键
      project_name: m.project_name,
      org: m.org,
      area: m.area,
      budget: m.budget,
      note: m.note,
      dedup_layer: "manual",
      nodes: [],
      node_count: 0,
      first_date: "", last_date: "",
      manual: true,
      created_at: m.created_at,
    }));
    st.projects = [...man, ...base];
  };

  /** 选中项目（保持滚动位置记忆）。 */
  IP.select = function select(key) {
    const st = IP.state;
    st.selScroll[st.selected] = (document.querySelector(".ip-right") || {}).scrollTop || 0;
    st.selected = key;
    st.page = 1;
  };
})();

/* ── 主视图渲染（方案 A 主从双栏） ───────────────────────────────────────── */
(function () {
  const IP = window.__IP;
  const esc = IP.esc;
  const st = () => IP.state;

  /** 呈现形态切换：公告清单（现有，原样保留）/ 项目进程（新增，默认）。
   *  规格 §1：默认「项目进程」；此处只做切换条，不改既有清单视图任何内容。 */
  IP.switcher = function switcher(mode) {
    return `<div class="ip-scope" style="padding:0 12px 8px">
      <div style="display:flex;align-items:center;gap:10px;flex-wrap:wrap">
        <span class="ip-switch" role="group" aria-label="情报呈现形态">
          <button data-ip-mode="projects" aria-pressed="${mode === "projects"}">项目进程</button>
          <button data-ip-mode="list" aria-pressed="${mode !== "projects"}">公告清单</button>
        </span>
        <span style="font-size:12px;color:var(--ip-text-muted)">同一项目多阶段公告聚合为一张进程卡；「公告清单」保留原有逐条流水。</span>
      </div>
    </div>`;
  };

  function themed() {
    const dark = st().theme === "dark";
    return `<div class="ip-scope" data-ip-theme="${dark ? "dark" : "light"}">`;
  }

  /** 左栏工具行 + 卡列表（200 项目按 20/页分批，规格 §5 极端项）。 */
  function leftCol(projects) {
    const s = st();
    const total = projects.length;
    const pages = Math.max(1, Math.ceil(total / 20));
    const page = Math.min(s.page, pages);
    const shown = projects.slice((page - 1) * 20, page * 20);
    const manualCount = projects.filter((p) => p.manual).length;
    const cards = shown.length
      ? shown.map((p) => IP.kcard(p, { selected: p.project_key === s.selected })).join("")
      : `<div class="ip-empty" style="padding:24px">没有匹配的项目。<br>
           <button class="ip-btn--text ip-btn" data-ip-mode="list">切到「公告清单」看逐条流水</button></div>`;
    const pager = pages > 1
      ? `<div style="display:flex;gap:8px;justify-content:center;padding:8px 0;font-size:12px;color:var(--ip-text-muted)">
          <button class="ip-btn--secondary ip-btn" data-ip-page="${page - 1}" ${page <= 1 ? "disabled" : ""}>上一页</button>
          第 ${page} / ${pages} 页（共 ${total} 项）
          <button class="ip-btn--secondary ip-btn" data-ip-page="${page + 1}" ${page >= pages ? "disabled" : ""}>下一页</button>
        </div>` : "";
    return `<div class="ip-left">
      <div class="ip-left-tools">
        <b style="font-size:13px">项目 ${total}</b>
        <span style="font-size:12px;color:var(--ip-text-muted)">${manualCount ? `含手动 ${manualCount}` : "全部公告驱动"}</span>
        <span style="flex:1"></span>
        <button class="ip-btn--secondary ip-btn" id="ip-manual-new" style="padding:6px 10px;font-size:12px">＋ 手动建档</button>
      </div>
      <div class="ip-left-body">${cards}${pager}</div>
    </div>`;
  }

  /** 底部工具条：转待办（主）/ 标记相关·无关（次）/ 导出该项目进程（文字）。 */
  function toolbar(p, view) {
    const msg = st().flash;
    return `<div class="ip-toolbar">
      <button class="ip-btn" id="ip-to-todo">转待办</button>
      <button class="ip-btn--secondary ip-btn" data-ip-mark="core">标记相关</button>
      <button class="ip-btn--secondary ip-btn" data-ip-mark="excluded">标记无关</button>
      <button class="ip-btn--text ip-btn" id="ip-export">导出该项目进程</button>
      <span class="ip-toolbar-msg" ${msg ? `data-tone="${msg.tone}"` : ""}>${esc(msg ? msg.text : "")}</span>
    </div>`;
  }

  /** 疑似关联提示条（规格 A-7：只画强命中——归一化键完全相等）。 */
  function hintBar(p) {
    const cands = IP.candidatesOf(p);
    if (!cands.length) return "";
    return `<div class="ip-hintbar">
      <span>检测到 ${cands.length} 条疑似关联公告，字段可能不一致</span>
      <button class="ip-btn--secondary ip-btn" data-ip-compare="${esc(cands[0].project_key)}" style="padding:5px 10px;font-size:12px">查看对照</button>
    </div>`;
  }

  /** 手动项目右栏：空态轴 + 录入摘要 + 等待公告说明（规格 A-7）。 */
  function manualAxis(p) {
    return `<div class="ip-axis">
      <div class="ip-empty">
        <div style="margin-bottom:10px;font-weight:600;color:var(--ip-text-primary)">暂无公告 · 手动建档</div>
        该项目尚未采集到任何公告，阶段判定未启用；六段全空心表示「还没有任何一段有过公告」，
        不是「每段 0 条」。引擎后续采集到公告并经你确认合并后，这里会接回公告驱动的阶段判定。
      </div>
    </div>`;
  }

  IP.render = function render() {
    const s = st();
    let inner;
    if (s.loading && !s.projects) {
      const sk = `<div class="ip-sk"><b></b><b></b><b></b></div>`;
      inner = `<div class="ip-wrap"><div class="ip-left"><div class="ip-left-body">${sk}${sk}${sk}</div></div><div class="ip-right">
        <div class="ip-sk"><b></b><b></b><b></b></div><div class="ip-sk"><b></b><b></b><b></b></div>
        <div class="ip-sk"><b></b><b></b><b></b></div></div></div>`;
    } else if (s.error) {
      // 错误与「确实无新增」严格分开措辞（规格 §5）
      inner = `<div class="ip-wrap"><div class="ip-right"><div class="ip-empty">
        <b style="color:var(--ip-error)">项目聚合数据读取失败</b><br>
        <span style="font-size:12px">${esc(s.error)}</span><br>
        这不是「没有新项目」——是本地库没能读出聚合结果，原因见上。
        <div style="margin-top:12px"><button class="ip-btn--secondary ip-btn" id="ip-reload">重试</button></div>
      </div></div></div>`;
    } else {
      const all = s.projects || [];
      // 阶段筛选：只保留「当前段 == 选中段」的项目（手动项目在筛选下仍显示，因为它没有阶段）
      const filtered = s.stageFilter
        ? all.filter((p) => p.manual || IP.projectView(p).currentSeg === s.stageFilter)
        : all;
      const list = filtered;
      let sel = list.find((p) => p.project_key === s.selected);
      if (!sel && list.length) { s.selected = list[0].project_key; sel = list[0]; }
      if (!sel) {
        inner = `<div class="ip-wrap">${leftCol(list)}<div class="ip-right"><div class="ip-empty">
          首次运行还没有聚合项目。引擎采集到公告后，同一采购人的同一项目会在这里聚成一张卡。
          <div style="margin-top:12px"><button class="ip-btn--text ip-btn" data-ip-mode="list">切到公告清单看流水</button></div>
        </div></div></div>`;
      } else {
        const view = IP.projectView(sel);
        sel.__view = view;
        const axis = sel.manual ? manualAxis(sel) : IP.axis(sel, view);
        inner = `<div class="ip-wrap" data-ip-full="split">
          ${leftCol(list)}
          <div class="ip-right">
            ${IP.headHtml(sel, view)}
            ${hintBar(sel)}
            ${sel.manual ? "" : IP.navChips(view)}
            ${axis}
            ${toolbar(sel, view)}
          </div>
        </div>`;
      }
    }
    return themed() + inner + `<div class="ip-filterslot">${filterBar()}</div></div>`;
  };

  function filterBar() {
    const s = st();
    return `<div style="display:flex;gap:6px;flex-wrap:wrap;align-items:center;padding:10px 2px 0;font-size:12px;color:var(--ip-text-muted)">
      <span>阶段筛选</span>
      <button class="ip-navchip" data-ip-filter="" aria-pressed="${!s.stageFilter}">全部</button>
      ${IP.STAGES.map((d) => `<button class="ip-navchip" data-ip-filter="${esc(d.key)}" aria-pressed="${s.stageFilter === d.key}">${esc(d.short)}</button>`).join("")}
      <span style="flex:1"></span>
      <button class="ip-navchip" id="ip-theme">${s.theme === "dark" ? "切浅色" : "切暗色"}</button>
      <button class="ip-navchip" id="ip-reload2">刷新</button>
    </div>`;
  };

  /** 强命中候选（归一化键完全相等，口径：只画强命中）。排除已忽略与自身。 */
  IP.candidatesOf = function candidatesOf(p) {
    if (!p.manual) return [];
    const ignored = ignoredKeys();
    const key = IP.normKey(p.org, p.project_name);
    return (IP.state.projects || []).filter((q) => !q.manual
      && q.project_key !== p.project_key
      && !ignored.includes(q.project_key)
      && IP.normKey(q.org, q.project_name) === key);
  };

  function ignoredKeys() {
    try { return JSON.parse(window.localStorage.getItem("wb.intel.compareIgnored.v1") || "[]"); }
    catch (_) { return []; }
  }
  IP.ignoredKeys = ignoredKeys;
  IP.ignoreKey = function ignoreKey(k) {
    const arr = ignoredKeys().concat([k]);
    try { window.localStorage.setItem("wb.intel.compareIgnored.v1", JSON.stringify(arr)); } catch (_) { /* 存储不可用时本次会话内不再提示 */ }
    IP._sessionIgnored = (IP._sessionIgnored || []).concat([k]);
  };
})();

/* ── 轻量弹层：手动建档 / 转待办 / 并排对照（规格 A-1 / A-7，与转待办同一套视觉） ── */
(function () {
  const IP = window.__IP;
  const esc = IP.esc;

  IP.openModal = function openModal(html) {
    IP.closeModal();
    const mask = document.createElement("div");
    mask.className = "ip-modal-mask";
    mask.id = "ip-modal-mask";
    mask.innerHTML = `<div class="ip-modal" role="dialog" aria-modal="true">${html}</div>`;
    document.body.appendChild(mask);
    mask.addEventListener("click", (e) => { if (e.target === mask) IP.closeModal(); });
    document.addEventListener("keydown", IP._escHandler = (e) => { if (e.key === "Escape") IP.closeModal(); });
    const first = mask.querySelector("input, .ip-btn");
    if (first) first.focus();
    return mask;
  };

  IP.closeModal = function closeModal() {
    const m = document.getElementById("ip-modal-mask");
    if (m) m.remove();
    if (IP._escHandler) { document.removeEventListener("keydown", IP._escHandler); IP._escHandler = null; }
  };

  /** 手动建档面板（规格 A-7）：必填仅项目名 + 采购人；建档即查重，命中给两个出口。 */
  IP.manualPanel = function manualPanel() {
    return `<h3>手动建档</h3>
      <div class="ip-modal-sub">用于网上尚无痕迹、还没有任何公告的项目。必填两项：项目名 + 采购人；
        其余可后补。建档后该项目以「手动」标记出现在项目进程列表中。</div>
      <div class="ip-field"><span>项目名 <i class="ip-req" style="font-style:normal">*必填</i></span>
        <input class="ip-input" id="ip-m-name" maxlength="200" placeholder="如：某县植保无人机采购项目"></div>
      <div class="ip-field"><span>采购人 <i class="ip-req" style="font-style:normal">*必填</i></span>
        <input class="ip-input" id="ip-m-org" maxlength="120" placeholder="如：某县农业农村局"></div>
      <div class="ip-field"><span>地区（可选）</span><input class="ip-input" id="ip-m-area" maxlength="60"></div>
      <div class="ip-field"><span>预算（可选）</span><input class="ip-input" id="ip-m-budget" maxlength="60" placeholder="如：120 万元"></div>
      <div class="ip-field"><span>备注 / 线索来源（可选）</span><input class="ip-input" id="ip-m-note" maxlength="200"></div>
      <div id="ip-m-dup" hidden style="margin-top:8px"></div>
      <div id="ip-m-msg" style="font-size:12px;color:var(--ip-text-muted);margin-top:8px"></div>
      <div class="ip-modal-acts">
        <button class="ip-btn--text ip-btn" id="ip-m-cancel">取消</button>
        <button class="ip-btn" id="ip-m-save">建档</button>
      </div>`;
  };

  /** 转待办轻量确认面板（规格 A-1：不出第二个表单，只补三格）。 */
  IP.todoPanel = function todoPanel(p) {
    return `<h3>转待办</h3>
      <div class="ip-modal-sub">预填：项目名 + 当前阶段 + 来源引用，你只需补负责人 / 优先级 / 截止。</div>
      <div class="ip-field"><span>项目</span><input class="ip-input" value="${esc(p.project_name || "")}" readonly></div>
      <div class="ip-field"><span>负责人（可空 = 未分配池）</span><input class="ip-input" id="ip-t-owner" placeholder="默认落未分配池"></div>
      <div class="ip-field"><span>优先级</span>
        <select class="ip-input" id="ip-t-pri"><option value="P0">P0 · 24h 催办</option><option value="P1" selected>P1 · 72h</option><option value="P2">P2 · 7d</option></select></div>
      <div class="ip-field"><span>截止（可空 = 持续类，不进临期/逾期规则）</span><input class="ip-input" id="ip-t-due" type="date"></div>
      <div id="ip-t-msg" style="font-size:12px;color:var(--ip-text-muted);margin-top:8px"></div>
      <div class="ip-modal-acts">
        <button class="ip-btn--text ip-btn" id="ip-t-cancel">取消</button>
        <button class="ip-btn" id="ip-t-go">生成待办</button>
      </div>`;
  };

  /** 并排对照视图（规格 A-7）：左手动档案 / 右公告聚合卡，字段差异高亮，二选一。 */
  IP.comparePanel = function comparePanel(manual, cand) {
    const fields = [
      ["项目名", manual.project_name, cand.project_name],
      ["采购人", manual.org, cand.org],
      ["地区", manual.area || "（未填）", cand.area || "（公告未给）"],
      ["预算", manual.budget || "（未填）", cand.budget || cand.amount || "（公告未给）"],
      ["公告数", "0（手动建档）", String((cand.nodes || []).length) + " 条"],
    ];
    const rows = fields.map(([k, l, r]) => {
      const same = String(l || "").trim() === String(r || "").trim();
      return { k, l, r, same };
    });
    const col = (side) => rows.map((r) => `<div class="ip-cmp-row" data-diff="${r.same ? "false" : "true"}">
        <span class="k">${esc(r.k)}</span><span class="v">${esc(r[side])}${r.same ? "" : " · 不一致"}</span></div>`).join("");
    const diffN = rows.filter((r) => !r.same).length;
    return `<h3>疑似关联公告 · 并排对照</h3>
      <div class="ip-modal-sub">左侧是你手动录入的档案，右侧是引擎聚合出的公告卡。
        共有 ${diffN} 个字段不一致（已高亮）。合并后采购人、项目名以公告为准，地区/预算等手动补充字段保留并标注「手动补充」。</div>
      <div class="ip-cmp">
        <div class="ip-cmp-col"><h4>手动档案</h4>${col("l")}</div>
        <div class="ip-cmp-col"><h4>公告聚合卡</h4>${col("r")}</div>
      </div>
      <div id="ip-c-msg" style="font-size:12px;color:var(--ip-text-muted);margin-top:10px"></div>
      <div class="ip-modal-acts">
        <button class="ip-btn--text ip-btn" id="ip-c-ignore">不是同一项目（忽略）</button>
        <button class="ip-btn" id="ip-c-merge">合并到该项目流程</button>
      </div>`;
  };
})();

/* ── 交互绑定（全部事件在渲染后重绑；节点级，不累积） ─────────────────────── */
(function () {
  const IP = window.__IP;
  const esc = IP.esc;
  const q = (s, r) => (r || document).querySelector(s);
  const qa = (s, r) => Array.from((r || document).querySelectorAll(s));

  /** 导出该项目进程：生成 CSV。
   *  ⚠ 本版没有新增后端写入命令（文件写盘通道属数据接线轨范围），因此导出走
   *  浏览器下载（WebView2 支持时直接落盘）；不支持时退化为复制到剪贴板并如实提示。 */
  function exportCsv(p, view) {
    const head = ["阶段", "日期", "标题", "判据来源", "命中依据", "来源站", "原文链接"];
    const rows = [];
    view.segs.forEach((s) => s.nodes.forEach((n) => rows.push([
      s.def.label, IP.fmtDate(n.date), n.title, n.stage_source, n.stage_reason, n.source_name, n.url,
    ])));
    view.unknown.forEach((n) => rows.push(["阶段未知", IP.fmtDate(n.date), n.title, n.stage_source, n.stage_reason, n.source_name, n.url]));
    const head5 = [["项目名", p.project_name], ["采购人", p.org], ["地区", p.area], ["预算", p.budget || p.amount || ""], ["手动建档", p.manual ? "是" : "否"]];
    const csv = head5.map((r) => r.map(q1).join(",")).join("\r\n") + "\r\n\r\n" + head.map(q1).join(",") + "\r\n" + rows.map((r) => r.map(q1).join(",")).join("\r\n");
    function q1(v) { const s = String(v ?? ""); return /[",\r\n]/.test(s) ? '"' + s.replace(/"/g, '""') + '"' : s; }
    return { csv, count: rows.length };
  }

  function setFlash(tone, text) {
    IP.state.flash = { tone, text };
    const el = q(".ip-toolbar-msg");
    if (el) { el.textContent = text; el.setAttribute("data-tone", tone); }
  }

  function rerender() {
    const main = q("#main");
    if (main && window.__ipRenderIntel) window.__ipRenderIntel("reflow");
  }

  /** 渲染后绑定：由 app.js 的 bindView() 在 intel 分支调用。 */
  IP.bind = function bind() {
    // 形态切换（L2 情报顶部）
    qa("[data-ip-mode]").forEach((b) => b.addEventListener("click", () => {
      IP.state.mode = b.dataset.ipMode;
      if (window.__ipSetIntelMode) window.__ipSetIntelMode(b.dataset.ipMode);
      rerender();
    }));

    // 左栏选卡
    qa("[data-ip-key]").forEach((b) => b.addEventListener("click", () => {
      IP.select(b.dataset.ipKey);
      rerender();
      const right = q(".ip-right");
      if (right) right.scrollTop = IP.state.selScroll[b.dataset.ipKey] || 0;
    }));

    // 阶段导航 chip → 滚到该段
    qa("[data-ip-nav]").forEach((b) => b.addEventListener("click", () => {
      const el = q("#ip-seg-" + b.dataset.ipNav);
      if (el) el.scrollIntoView({ block: "nearest", behavior: "smooth" });
    }));

    // 分页
    qa("[data-ip-page]").forEach((b) => b.addEventListener("click", () => {
      const n = Number(b.dataset.ipPage);
      if (n > 0) { IP.state.page = n; rerender(); }
    }));

    // 阶段筛选
    qa("[data-ip-filter]").forEach((b) => b.addEventListener("click", () => {
      IP.state.stageFilter = b.dataset.ipFilter;
      IP.state.page = 1;
      rerender();
    }));

    // 展开折叠的段
    qa("[data-ip-expand]").forEach((b) => b.addEventListener("click", () => {
      const seg = q("#ip-seg-" + b.dataset.ipExpand);
      const fold = seg && q("[data-ip-fold]", seg);
      if (fold) { fold.hidden = !fold.hidden; b.textContent = fold.hidden ? "展开其余公告" : "收起"; }
    }));

    // 暗色 / 刷新
    q("#ip-theme")?.addEventListener("click", () => {
      IP.state.theme = IP.state.theme === "dark" ? "light" : "dark";
      rerender();
    });
    q("#ip-reload")?.addEventListener("click", reload);
    q("#ip-reload2")?.addEventListener("click", reload);

    async function reload() {
      await IP.load();
      rerender();
    }

    // ── 手动建档 ──
    q("#ip-manual-new")?.addEventListener("click", () => {
      IP.openModal(IP.manualPanel());
      const nameI = q("#ip-m-name"), orgI = q("#ip-m-org"), dup = q("#ip-m-dup");
      const check = () => {
        const name = nameI.value.trim(), org = orgI.value.trim();
        if (!name || !org) { dup.hidden = true; return; }
        const key = IP.normKey(org, name);
        const hit = (IP.state.projects || []).find((p) => IP.normKey(p.org, p.project_name) === key);
        if (!hit) { dup.hidden = true; return; }
        dup.hidden = false;
        dup.innerHTML = `<div class="ip-hintbar" style="margin:0">
          <span>已存在疑似同一项目：${esc(hit.project_name)}（${hit.manual ? "手动建档" : (hit.nodes || []).length + " 条公告"}）</span>
          <button class="ip-btn--secondary ip-btn" id="ip-dup-open" style="padding:4px 8px;font-size:12px">打开该聚合卡</button>
          <button class="ip-btn--text ip-btn" id="ip-dup-force" style="padding:4px 8px;font-size:12px">仍单独建档</button></div>`;
        q("#ip-dup-open", dup)?.addEventListener("click", () => {
          IP.select(hit.project_key); IP.closeModal(); rerender();
        });
        q("#ip-dup-force", dup)?.addEventListener("click", () => save(true));
      };
      nameI?.addEventListener("input", check);
      orgI?.addEventListener("input", check);
      q("#ip-m-cancel")?.addEventListener("click", IP.closeModal);

      async function save(force) {
        const name = nameI.value.trim(), org = orgI.value.trim();
        const msg = q("#ip-m-msg");
        if (!name || !org) { msg.textContent = "项目名与采购人均必填。"; msg.style.color = "var(--ip-error)"; return; }
        const key = IP.normKey(org, name);
        if (!force) {
          const hit = (IP.state.projects || []).find((p) => IP.normKey(p.org, p.project_name) === key);
          if (hit) { check(); msg.textContent = "命中已有项目，请先在上面二选一。"; msg.style.color = "var(--ip-warning)"; return; }
        }
        // 主路径：后端 manual_project_create（建档即查重、落库进引擎去重池，与 wbd CLI 同一实现）
        const t = window.__TAURI__ && window.__TAURI__.core;
        if (t && typeof t.invoke === "function") {
          try {
            const r = await t.invoke("manual_project_create", {
              project_name: name, buyer: org,
              area: q("#ip-m-area").value.trim() || null,
              budget: q("#ip-m-budget").value.trim() || null,
              note: q("#ip-m-note").value.trim() || null,
              force: !!force,
            });
            if (r && r.outcome === "created") {
              IP.select(r.agg_key); IP.closeModal(); IP.load().then(() => rerender()); return;
            }
            if (r && r.outcome === "duplicate") {
              // 「重复」（dup_kind=manual，同库同键）与「疑似关联」（dup_kind=announcement，公告侧聚合）分开出口
              msg.style.color = "var(--ip-warning)";
              const openHit = (aggKey, manualOnly) => {
                const cards = (IP.state.projects || []);
                const card = manualOnly
                  ? cards.find((p) => p.manual && p.agg_key === aggKey)
                  : cards.find((p) => !p.manual && p.agg_key === aggKey);
                if (!card) { msg.textContent = "命中的项目不在当前列表（可能被筛掉），请关闭后在列表里直接查找。"; return; }
                IP.select(card.project_key); IP.closeModal(); rerender();
              };
              const prompt = document.createElement("div");
              prompt.className = "ip-hintbar";
              prompt.style.margin = "0";
              prompt.innerHTML =
                `<span>${esc(r.prompt || "已存在疑似同一项目。")}</span>` +
                (r.manual_hit
                  ? `<button class="ip-btn ip-btn--secondary" data-dup="open" style="padding:4px 8px;font-size:12px">打开已有档案</button>`
                  : `<button class="ip-btn ip-btn--secondary" data-dup="open" style="padding:4px 8px;font-size:12px">打开疑似关联项目</button>`) +
                `<button class="ip-btn ip-btn--text" data-dup="force" style="padding:4px 8px;font-size:12px">仍单独建档</button>`;
              msg.textContent = "";
              msg.appendChild(prompt);
              const hitKey = r.manual_hit ? r.manual_hit.agg_key : r.agg_key;
              prompt.querySelector('[data-dup="open"]')?.addEventListener("click", () => openHit(hitKey, !!r.manual_hit));
              prompt.querySelector('[data-dup="force"]')?.addEventListener("click", () => save(true));
              return;
            }
            msg.textContent = "建档返回未知结果，未保存。"; msg.style.color = "var(--ip-error)"; return;
          } catch (e) {
            const es = String(e || "");
            if (!/not\s*found|unknown\s+command|unhandled\s+command|invalid\s+command/i.test(es)) {
              // 命令存在但被后端拒绝（必填校验等）——如实展示，不静默降级
              msg.textContent = "建档被拒：" + e; msg.style.color = "var(--ip-error)"; return;
            }
            // 命令不存在（老后端）：降级 localStorage 路径（下方）
          }
        }
        const rec = {
          project_key: (force ? "manual:x:" : "manual:") + key + ":" + Date.now(),
          project_name: name, org,
          area: q("#ip-m-area").value.trim(),
          budget: q("#ip-m-budget").value.trim(),
          note: q("#ip-m-note").value.trim(),
          created_at: new Date().toISOString().slice(0, 10),
        };
        const arr = IP.loadManualLS().concat([rec]);
        if (!IP.saveManual(arr)) { msg.textContent = "本地存储不可用，建档未保存（未静默成功）。"; msg.style.color = "var(--ip-error)"; return; }
        IP.select(rec.project_key);
        IP.closeModal();
        IP.load().then(() => rerender());
      }
      q("#ip-m-save")?.addEventListener("click", () => save(false));
    });

    // ── 底部工具条：转待办 / 标记相关·无关 / 导出 ──
    const sel = (IP.state.projects || []).find((p) => p.project_key === IP.state.selected);
    q("#ip-to-todo")?.addEventListener("click", () => {
      if (!sel) return;
      if (sel.manual) {
        setFlash("err", "手动建档项目没有公告来源，转待办需先合并公告（对照合并后可用）。");
        return;
      }
      IP.openModal(IP.todoPanel(sel));
      q("#ip-t-cancel")?.addEventListener("click", IP.closeModal);
      q("#ip-t-go")?.addEventListener("click", async () => {
        const msg = q("#ip-t-msg");
        const nodes = (sel.nodes || []);
        if (!nodes.length) { msg.textContent = "该项目没有公告节点，无法生成待办。"; msg.style.color = "var(--ip-error)"; return; }
        // 走既有线索通道：intel_lead_act（key 必须是线索 dedup_key）。项目键与线索键同口径
        // （`code:` / `fp:` / `body:` 前缀，见 intel_store 注释），因此直接用 project_key。
        msg.textContent = "正在生成…";
        // 先把输入读出来：弹层一关输入框就没了，事后再 query 只会拿到 null。
        const ownerRaw = q("#ip-t-owner").value.trim();
        const pri = q("#ip-t-pri").value;
        const due = q("#ip-t-due").value;
        try {
          const r = await window.__TAURI__.core.invoke("intel_lead_act", {
            action: "todo", key: sel.project_key, owner: ownerRaw || null,
          });
          IP.closeModal();
          const tid = r && r.todo && r.todo.todo_id ? "：" + r.todo.todo_id : "";
          setFlash("ok", `已生成待办${tid}（优先级 ${pri}，负责人 ${ownerRaw || "未分配池"}${due ? "，截止 " + due : "，无截止"}）`);
        } catch (e) {
          msg.textContent = "生成失败：" + e;
          msg.style.color = "var(--ip-error)";
        }
      });
    });

    qa("[data-ip-mark]").forEach((b) => b.addEventListener("click", async () => {
      if (!sel) return;
      if (sel.manual) { setFlash("err", "手动建档项目无公告条目，暂不能标记相关/无关。"); return; }
      const nodes = (sel.nodes || []);
      if (!nodes.length) { setFlash("err", "该项目没有公告条目可标记。"); return; }
      const verdict = b.dataset.ipMark;
      let ok = 0, fail = 0;
      for (const n of nodes) {
        try { await window.__TAURI__.core.invoke("intel_set_verdict", { key: IP.itemKeyOf(n), verdict }); ok++; }
        catch (_) { fail++; }
      }
      setFlash(fail ? "err" : "ok",
        `${verdict === "core" ? "标记相关" : "标记无关"}：成功 ${ok} 条${fail ? `，失败 ${fail} 条（条目不在当前情报快照，需先刷新采集）` : ""}`);
    }));

    q("#ip-export")?.addEventListener("click", () => {
      if (!sel) return;
      const { csv, count } = exportCsv(sel, sel.__view || IP.projectView(sel));
      const name = (sel.project_name || "项目").replace(/[\\/:*?"<>|]/g, "_") + "-进程.csv";
      try {
        const blob = new Blob(["\uFEFF" + csv], { type: "text/csv;charset=utf-8" });
        const a = document.createElement("a");
        a.href = URL.createObjectURL(blob); a.download = name;
        document.body.appendChild(a); a.click(); a.remove();
        setTimeout(() => URL.revokeObjectURL(a.href), 2000);
        setFlash("ok", `已导出 ${count} 条公告（CSV：${name}）；若浏览器未落盘，请用下方「复制到剪贴板」`);
      } catch (_) { copyFallback(); }
      function copyFallback() {
        navigator.clipboard.writeText(csv).then(
          () => setFlash("ok", `已复制 ${count} 条公告的 CSV 到剪贴板`),
          () => setFlash("err", "导出失败：当前环境既不能下载也不能访问剪贴板"),
        );
      }
    });

    // ── 并排对照（疑似关联） ──
    qa("[data-ip-compare]").forEach((b) => b.addEventListener("click", () => {
      const manual = sel;
      const cand = (IP.state.projects || []).find((p) => p.project_key === b.dataset.ipCompare);
      if (!manual || !cand) return;
      IP.openModal(IP.comparePanel(manual, cand));
      q("#ip-c-ignore")?.addEventListener("click", async () => {
        // 主路径：后端 ignore_link（写审计、置 ignored，同一对不再提示）
        const t = window.__TAURI__ && window.__TAURI__.core;
        if (t && typeof t.invoke === "function" && manual.manual_id && cand.agg_key) {
          try {
            await t.invoke("manual_project_ignore", {
              manual_id: manual.manual_id,
              target_project: cand.agg_key,
              actor: "ui",
            });
            IP.closeModal(); IP.load().then(() => rerender());
            return;
          } catch (e) {
            if (!/not\s*found|unknown\s+command|unhandled\s+command/i.test(String(e || ""))) {
              setFlash("err", "忽略失败：" + e); return;   // 命令在但被拒：如实展示，不静默降级
            }
          }
        }
        IP.ignoreKey(cand.project_key);   // 降级兜底（老后端 / 无 manual_id）
        IP.closeModal(); rerender();
      });
      q("#ip-c-merge")?.addEventListener("click", async () => {
        // 合并方向按规格 A-7：采购人/项目名以公告为准，手动补充字段保留并标注。
        const t = window.__TAURI__ && window.__TAURI__.core;
        if (t && typeof t.invoke === "function" && manual.manual_id && cand.agg_key) {
          try {
            await t.invoke("manual_project_merge", {
              manual_id: manual.manual_id,
              target_project: cand.agg_key,   // 公告聚合键（target 语义见 intel_manual::merge_into_announcement）
              name: cand.project_name,
              buyer: cand.org,
              actor: "ui",
            });
            IP.select(cand.project_key);
            IP.closeModal();
            IP.load().then(() => rerender());
            return;
          } catch (e) {
            if (!/not\s*found|unknown\s+command|unhandled\s+command/i.test(String(e || ""))) {
              setFlash("err", "合并失败：" + e); return;   // 命令在但被拒：如实展示，不静默降级
            }
          }
        }
        // 降级兜底（老后端 / 无 manual_id）：localStorage 旧路径
        const arr = IP.loadManualLS().filter((m) => m.project_key !== manual.project_key);
        arr.push({
          project_key: cand.project_key,
          project_name: cand.project_name, org: cand.org,
          area: manual.area, budget: manual.budget, note: manual.note,
          manual_note: "手动补充", merged_at: new Date().toISOString().slice(0, 10),
          manual: true,
        });
        IP.saveManual(arr);
        IP.select(cand.project_key);
        IP.closeModal();
        IP.load().then(() => rerender());
      });
    }));

  };
})();

/* ── 对外入口（app.js 的 viewIntel 只调这里） ─────────────────────────────── */
(function () {
  const IP = window.__IP;
  IP.state.mode = "projects";   // 规格 §1：默认「项目进程」
  IP.state.theme = "light";

  /** 供 app.js 调用的整块视图（不含既有清单内容）。mode = "projects"。 */
  IP.view = function view() {
    return IP.render();
  };

  /** app.js 的 viewIntel 在「公告清单」模式下用它取切换条，保证两种形态都有入口。 */
  IP.switcherOnly = function switcherOnly(mode) {
    return IP.switcher(mode || IP.state.mode);
  };
})();
