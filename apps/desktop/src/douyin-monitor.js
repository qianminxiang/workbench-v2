/* v3.4.0 · W6+ 抖音舆情免费通道低密度巡检小闭环（douyin-monitor.js）
 * ============================================================================
 * 依据：任务书附加条款②③④与验收①③④；用户拍板（2026-11-25）：开发基线
 * v3.4-gate（f61cd27）；巡检频次口径 = 任务书正文/试跑报告 §8.1：
 * 每日 4–6 轮 × 每轮 20–40 词，查询间隔 ≥12 秒。
 *
 * 边界（红线）：
 * - 0 付费：只做免费通道（黑猫/B站/微头条/抖音）的人工巡检录入闭环；
 *   本模块不内置任何自动抓取——黑猫实测只有浏览器滚动可取（试跑 §11.5），
 *   站内 fetch / feed API 均不可用，故采集动作留在浏览器侧、由人执行，
 *   本模块承担：词表红线钳制 + 轮次闸门 + 召回条目三态人工筛判留痕 + 月度汇总。
 * - 属地过滤不可实现：免费通道无作者属地能力，属地靠召回后人工筛，
 *   不做自动化告警承诺（误报率约 90%）。
 *
 * 挂载与 todo-views.js 同一模式（纯加法，app.js 只做调度与缺模块降级）：
 *   1. index.html 在 app.js 之前引入本文件；
 *   2. MON_TABS 抖音页签 render 调 DM.view()（未激活回落原占位说明）；
 *   3. 设置页 ② 情报采集卡内嵌 DM.settingsRows()，绑定走 DM.bindSettings()。
 *
 * 存储（v3.4.0 前端闭环的有意取舍）：localStorage（wb.douyin.config.v1 /
 * wb.douyin.items.v1），本机持久、随应用数据目录存活；不进 sqlite、不参与
 * 服务器同步。与 intel-projects.js 的 localStorage 降级通道同源。后续若需要
 * 跨设备/入库，再迁 monitor_douyin_* 表（需后端命令，另行评审）。
 *
 * 零残留（验收①）：默认 enabled:false，监控页签保持原占位文案；
 * 未开启时不写任何 wb.douyin.* 键。
 * ============================================================================
 */
"use strict";

(function () {
  const DM = {};
  window.__DM = DM;

  const esc = (s) => String(s ?? "").replace(/[&<>"']/g, (c) =>
    ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" }[c]));

  // ── 红线常量（附加条款②：硬编码为代码常量，配置只能红线内收紧）──────────
  // 拍板口径：每日 4–6 轮 × 每轮 20–40 词、查询间隔 ≥12 秒（任务书正文与试跑 §8.1）。
  // 常量经 Object.freeze 冻结，运行期不可改；钳制语义：
  //   间隔 <12 钳回 12（只能调大不能调小）；轮次 4–6 之外钳回边界；
  //   词数 20–40 之外钳回边界；轮间隔 <30 钳回 30；验证码冷却 <2 钳回 2。
  const HARD = Object.freeze({
    MIN_QUERY_INTERVAL_SEC: 12,   // 查询间隔下限（秒）：实测 4s 连查约 10 次即触发滑块验证码
    MIN_ROUND_GAP_MIN: 30,        // 轮与轮之间最小间隔（分钟）：试跑 §8.1「轮与轮之间留 30 分钟以上」
    MAX_ROUNDS_PER_DAY: 6,        // 每日轮次上限
    MIN_ROUNDS_PER_DAY: 4,        // 每日轮次下限（低于 4 轮等于没开，钳回 4）
    MIN_WORDS_PER_ROUND: 20,      // 单轮词数下限
    MAX_WORDS_PER_ROUND: 40,      // 单轮词数上限
    COOLDOWN_AFTER_CAPTCHA_MIN: 2 // 触发验证码后的冷却（分钟），实测约 2 分钟可恢复
  });
  DM.HARD = HARD;

  const DEFAULTS = Object.freeze({
    enabled: false,                                  // 附加条款④：默认关闭，设置页手动开启
    queryIntervalSec: HARD.MIN_QUERY_INTERVAL_SEC,
    roundGapMin: HARD.MIN_ROUND_GAP_MIN,
    roundsPerDay: HARD.MIN_ROUNDS_PER_DAY,
    wordsPerRound: HARD.MIN_WORDS_PER_ROUND,
    cooldownAfterCaptchaMin: HARD.COOLDOWN_AFTER_CAPTCHA_MIN
  });

  /** 数值钳制：类型先拦（bool / 非有限 / 非整数 → 回退默认），再收敛到 [lo, hi]。 */
  function clampInt(raw, lo, hi, fallback) {
    if (raw === null || raw === undefined || raw === "") return fallback;
    // bool 是 int 的子类，必须先排除，否则 true 会被当成 1
    if (typeof raw === "boolean") return fallback;
    const n = typeof raw === "number" ? raw : Number(String(raw).trim());
    if (!Number.isFinite(n) || !Number.isInteger(n)) return fallback;
    if (n < lo) return lo;
    if (n > hi) return hi;
    return n;
  }

  /** 把任意配置收敛进红线区间。返回 { value, clamped, redline }：
   *  clamped 逐项记录 { key, wanted, got }，让上层把「配置被钳」如实告知用户，
   *  不允许静默改写（静默改写会让用户以为自己设的值生效了）。 */
  DM.clampSettings = function clampSettings(raw) {
    const src = raw && typeof raw === "object" ? raw : {};
    const clamped = [];
    const note = (key, wanted, got) => clamped.push({ key, wanted, got });

    const interval = clampInt(src.queryIntervalSec, HARD.MIN_QUERY_INTERVAL_SEC, 600, DEFAULTS.queryIntervalSec);
    if (interval !== src.queryIntervalSec && src.queryIntervalSec !== undefined) note("queryIntervalSec", src.queryIntervalSec, interval);

    const gap = clampInt(src.roundGapMin, HARD.MIN_ROUND_GAP_MIN, 720, DEFAULTS.roundGapMin);
    if (gap !== src.roundGapMin && src.roundGapMin !== undefined) note("roundGapMin", src.roundGapMin, gap);

    const rounds = clampInt(src.roundsPerDay, HARD.MIN_ROUNDS_PER_DAY, HARD.MAX_ROUNDS_PER_DAY, DEFAULTS.roundsPerDay);
    if (rounds !== src.roundsPerDay && src.roundsPerDay !== undefined) note("roundsPerDay", src.roundsPerDay, rounds);

    const words = clampInt(src.wordsPerRound, HARD.MIN_WORDS_PER_ROUND, HARD.MAX_WORDS_PER_ROUND, DEFAULTS.wordsPerRound);
    if (words !== src.wordsPerRound && src.wordsPerRound !== undefined) note("wordsPerRound", src.wordsPerRound, words);

    const cooldown = clampInt(src.cooldownAfterCaptchaMin, HARD.COOLDOWN_AFTER_CAPTCHA_MIN, 60, DEFAULTS.cooldownAfterCaptchaMin);
    if (cooldown !== src.cooldownAfterCaptchaMin && src.cooldownAfterCaptchaMin !== undefined) note("cooldownAfterCaptchaMin", src.cooldownAfterCaptchaMin, cooldown);

    // enabled 只做类型归一：字符串 "true" / 1 都不算开启，必须是布尔 true
    const enabled = src.enabled === true;

    return {
      value: {
        enabled,
        queryIntervalSec: interval,
        roundGapMin: gap,
        roundsPerDay: rounds,
        wordsPerRound: words,
        cooldownAfterCaptchaMin: cooldown
      },
      clamped,
      redline: HARD
    };
  };

  // ── 三态状态机（附加条款③：召回→人工筛可追溯）──────────────────────────
  const ST = Object.freeze({ UNSORTED: "未筛", SCREENED: "已筛", IGNORED: "已忽略" });
  DM.ST = ST;

  /** 筛判流转：必须留痕（谁判的、何时判的、为什么）。回到未筛必须走 undo。 */
  DM.screen = function screen(row, next, meta) {
    if (!Object.values(ST).includes(next)) throw new Error(`非法状态 ${next}`);
    if (next === ST.UNSORTED) throw new Error("回到未筛必须走 undo，不能直接 screen");
    const m = meta || {};
    return { ...row, state: next, screenedBy: m.by ?? null, screenedAt: m.at ?? null, reason: m.reason ?? null };
  };

  /** 撤销：回未筛并清痕迹。 */
  DM.undo = function undo(row) {
    return { ...row, state: ST.UNSORTED, screenedBy: null, screenedAt: null, reason: null };
  };

  /** 重新召回合并：人工改判不被重新召回覆盖（对齐仓库既有 AC-2-7）。 */
  DM.reRecall = function reRecall(existing, incoming) {
    const byId = new Map(existing.map((r) => [r.id, r]));
    return incoming.map((n) => {
      const old = byId.get(n.id);
      if (old && old.state !== ST.UNSORTED) {
        return { ...n, state: old.state, screenedBy: old.screenedBy, screenedAt: old.screenedAt, reason: old.reason, todoId: old.todoId };
      }
      return old ? { ...n, todoId: old.todoId } : n;
    });
  };

  /** 月度汇总：区分「召回总量」与「被用户看过判过的量」。screenedRate 空表为 null。 */
  DM.monthlySummary = function monthlySummary(rows) {
    const total = rows.length;
    const seen = rows.filter((r) => r.state !== ST.UNSORTED).length;
    return {
      recalled: total,
      screened: seen,
      untouched: total - seen,
      screenedRate: total === 0 ? null : Number((seen / total).toFixed(3)),
      byState: {
        [ST.UNSORTED]: rows.filter((r) => r.state === ST.UNSORTED).length,
        [ST.SCREENED]: rows.filter((r) => r.state === ST.SCREENED).length,
        [ST.IGNORED]: rows.filter((r) => r.state === ST.IGNORED).length
      }
    };
  };

  // ── 内容键（去重边界用内容键，不用序号——序号会被源站整体重编）──────────
  /** 同内容同键、日期或来源变则键变；空白差异不产生新键。 */
  DM.contentKey = function contentKey(source, dateKey, title) {
    const s = `${String(source ?? "")}|${String(dateKey ?? "")}|${String(title ?? "").replace(/\s+/g, " ").trim()}`;
    let h = 5381;
    for (let i = 0; i < s.length; i += 1) h = ((h << 5) + h + s.charCodeAt(i)) >>> 0;
    return "dy" + h.toString(16).padStart(8, "0");
  };

  // ── 粘贴解析：一行一条；可选 `YYYY-MM-DD ` 日期前缀；空行跳过；批内去重 ──
  DM.parseRecallPaste = function parseRecallPaste(text, opt) {
    const o = opt || {};
    const out = [];
    const seen = new Set();
    for (const lineRaw of String(text ?? "").split("\n")) {
      const line = lineRaw.trim();
      if (!line) continue;
      const m = line.match(/^(\d{4}-\d{2}-\d{2})\s+(.+)$/);
      const dateKey = m ? m[1] : (o.dateKey || dateStrOf(new Date()));
      const title = (m ? m[2] : line).trim();
      if (!title) continue;
      const id = DM.contentKey(o.source || "黑猫", dateKey, title);
      if (seen.has(id)) continue;
      seen.add(id);
      out.push({
        id, source: o.source || "黑猫", dateKey, title,
        recalledAt: o.now ?? null, roundNo: o.roundNo ?? null,
        state: ST.UNSORTED, screenedBy: null, screenedAt: null, reason: null, todoId: null
      });
    }
    return out;
  };

  /** 导入合并：新条目未筛；已有人工判定的条目保判（AC-2-7）、只刷新抓取信息。 */
  DM.importItems = function importItems(existing, incoming) {
    const byId = new Map((existing || []).map((r) => [r.id, r]));
    let added = 0;
    let updated = 0;
    for (const n of incoming || []) {
      const old = byId.get(n.id);
      if (old) {
        updated += 1;
        byId.set(n.id, old.state !== ST.UNSORTED
          ? { ...n, state: old.state, screenedBy: old.screenedBy, screenedAt: old.screenedAt, reason: old.reason, todoId: old.todoId }
          : { ...n, todoId: old.todoId });
      } else {
        added += 1;
        byId.set(n.id, n);
      }
    }
    return { items: Array.from(byId.values()), added, updated };
  };

  // ── 轮次闸门：超轮 / 间隔未到都给出可读原因 ─────────────────────────────
  DM.roundGate = function roundGate(cfg, items, now) {
    const c = cfg || {};
    const maxRounds = typeof c.roundsPerDay === "number" ? c.roundsPerDay : DEFAULTS.roundsPerDay;
    const gapMin = typeof c.roundGapMin === "number" ? c.roundGapMin : DEFAULTS.roundGapMin;
    const tk = dateStrOf(now);
    const todayItems = (items || []).filter((r) => String(r.recalledAt ?? "").slice(0, 10) === tk);
    const roundsToday = new Set(todayItems.map((r) => r.roundNo ?? r.recalledAt)).size;
    if (roundsToday >= maxRounds) {
      return { ok: false, roundsToday, maxRounds, reason: `今日已巡 ${roundsToday} 轮，已达每日 ${maxRounds} 轮上限（红线常量）` };
    }
    const lastAt = Math.max(0, ...todayItems.map((r) => {
      const t = new Date(r.recalledAt).getTime();
      return Number.isFinite(t) ? t : 0;
    }));
    const gapMs = gapMin * 60000;
    if (lastAt > 0 && now.getTime() - lastAt < gapMs) {
      const waitMin = Math.ceil((gapMs - (now.getTime() - lastAt)) / 60000);
      return { ok: false, roundsToday, maxRounds, reason: `距上一轮不足 ${waitMin} 分钟（轮间隔红线 ≥${gapMin} 分钟）` };
    }
    return { ok: true, roundsToday, maxRounds, reason: `可开始今日第 ${roundsToday + 1} 轮` };
  };

  // ── 存储（localStorage；读写全防异常，存储不可用时退化为会话内不可写）────
  const LS_CONFIG = "wb.douyin.config.v1";
  const LS_ITEMS = "wb.douyin.items.v1";

  const safeGet = (k) => {
    try { const raw = window.localStorage.getItem(k); return raw ? JSON.parse(raw) : null; } catch (_) { return null; }
  };
  const safeSet = (k, v) => {
    try { window.localStorage.setItem(k, JSON.stringify(v)); } catch (_) { /* 存储不可用：本次不落盘 */ }
  };

  /** 读配置：持久化损坏/超红线的值读取即钳回红线并修复落盘（钳制事件如实返回）。 */
  DM.loadConfig = function loadConfig() {
    const raw = safeGet(LS_CONFIG);
    const r = DM.clampSettings(raw || {});
    if (raw && r.clamped.length) safeSet(LS_CONFIG, r.value);
    return r;
  };

  /** 写配置：merge → clamp → 词表归一化（trim/去空/去重/上限 40 截断）→ 落盘。 */
  DM.saveConfig = function saveConfig(patch) {
    const cur = safeGet(LS_CONFIG) || {};
    const merged = { ...cur, ...(patch || {}) };
    const r = DM.clampSettings(merged);
    const rawWords = Array.isArray(merged.words) ? merged.words : [];
    const words = [];
    for (const w of rawWords) {
      const t = String(w ?? "").trim();
      if (t && !words.includes(t)) words.push(t);
    }
    let wordsTruncated = false;
    if (words.length > HARD.MAX_WORDS_PER_ROUND) { words.length = HARD.MAX_WORDS_PER_ROUND; wordsTruncated = true; }
    const value = { ...r.value, words };
    safeSet(LS_CONFIG, value);
    return { value, clamped: r.clamped, wordsTruncated };
  };

  DM.loadItems = function loadItems() {
    const arr = safeGet(LS_ITEMS);
    return Array.isArray(arr) ? arr : [];
  };

  /** 落盘按召回时间倒序（稳定排序，同刻保持导入序）。 */
  DM.saveItems = function saveItems(items) {
    const arr = (Array.isArray(items) ? items.slice() : [])
      .sort((a, b) => String(b.recalledAt ?? "").localeCompare(String(a.recalledAt ?? "")));
    safeSet(LS_ITEMS, arr);
    return arr;
  };

  /** 激活态：只有布尔 true 算开启（附加条款④：默认关闭）。 */
  DM.activated = function activated() {
    return DM.loadConfig().value.enabled === true;
  };

  // ── 时间工具 ──
  const pad = (n) => String(n).padStart(2, "0");
  function dateStrOf(d) { return `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())}`; }
  function tzOf(d) {
    const o = -d.getTimezoneOffset();
    const s = o >= 0 ? "+" : "-";
    const a = Math.abs(o);
    return `${s}${pad(Math.floor(a / 60))}:${pad(a % 60)}`;
  }
  function nowIso() {
    const d = new Date();
    return `${dateStrOf(d)}T${pad(d.getHours())}:${pad(d.getMinutes())}:${pad(d.getSeconds())}${tzOf(d)}`;
  }

  const KEY_ZH = {
    queryIntervalSec: "查询间隔(秒)",
    roundGapMin: "轮间隔(分钟)",
    roundsPerDay: "每日轮次",
    wordsPerRound: "每轮词数",
    cooldownAfterCaptchaMin: "验证码冷却(分钟)"
  };
  const clampNote = (events) => events.map((e) => `${KEY_ZH[e.key] || e.key} ${e.wanted} → ${e.got}`).join("；");

  /** 导入后的瞬时反馈（下次重渲染清空）。 */
  let lastNote = "";

  // ── 视图：监控页签 · 抖音舆情巡检面板 ────────────────────────────────────
  function badgeHtml(state) {
    const CLS = { [ST.UNSORTED]: "unsorted", [ST.SCREENED]: "screened", [ST.IGNORED]: "ignored" };
    const style = state === ST.SCREENED
      ? "background:var(--brand);color:#fff;"
      : state === ST.IGNORED
        ? "background:var(--bg-inset);color:var(--text-3);text-decoration:line-through;"
        : "background:var(--bg-inset);color:var(--text-2);";
    return `<span class="dy-badge dy-badge--${CLS[state]}" style="font-size:11px;padding:1px 6px;border-radius:4px;${style}">${state}</span>`;
  }

  function rowHtml(r) {
    const state = Object.values(ST).includes(r.state) ? r.state : ST.UNSORTED;
    const btns = state === ST.UNSORTED
      ? `<button class="btn btn--ghost btn--sm" data-dy-act="screen">已筛</button>
         <button class="btn btn--ghost btn--sm" data-dy-act="ignore">忽略</button>`
      : state === ST.SCREENED
        ? `${r.todoId
            ? `<button class="btn btn--ghost btn--sm" disabled>已转待办</button>`
            : `<button class="btn btn--ghost btn--sm" data-dy-act="todo">转待办</button>`}
           <button class="btn btn--ghost btn--sm" data-dy-act="ignore">忽略</button>
           <button class="btn btn--ghost btn--sm" data-dy-act="undo">撤销</button>`
        : `<button class="btn btn--ghost btn--sm" data-dy-act="undo">撤销</button>`;
    const meta = [r.source, r.dateKey, r.roundNo ? `第${r.roundNo}轮` : null,
      r.screenedAt ? `判于 ${String(r.screenedAt).slice(0, 10)}` : null].filter(Boolean).join(" · ");
    return `<div class="settings-row" data-dy-item="${esc(r.id)}" style="flex-wrap:wrap;align-items:flex-start">
      <span style="flex:1;min-width:220px">
        ${badgeHtml(state)} <b>${esc(r.title)}</b>
        <span class="mini">${esc(meta)}</span>
      </span>
      <span style="display:flex;gap:6px;flex-wrap:wrap">${btns}</span>
    </div>`;
  }

  DM.view = function view() {
    const { value: cfg } = DM.loadConfig();
    const items = DM.loadItems();
    const now = new Date();
    const gate = DM.roundGate(cfg, items, now);
    const ym = `${now.getFullYear()}-${pad(now.getMonth() + 1)}`;
    const sum = DM.monthlySummary(items.filter((r) => String(r.dateKey ?? "").startsWith(ym)));
    const words = Array.isArray(cfg.words) ? cfg.words : [];
    const chips = words.length
      ? words.map((w) => `<span class="mini" style="background:var(--bg-inset);border-radius:4px;padding:1px 6px;margin:0 4px 4px 0;display:inline-block">${esc(w)}</span>`).join("")
      : '<span class="mini">词表为空：到「设置 → ② 情报采集」填写巡检词表（每行一词）。</span>';
    const rowsHtml = items.length
      ? items.map(rowHtml).join("")
      : '<div class="empty">暂无召回条目。完成一轮浏览器巡检（黑猫 / B站 / 微头条）后，把结果粘贴到上方导入。</div>';
    const rate = sum.screenedRate == null ? "—" : `${Math.round(sum.screenedRate * 100)}%`;
    return `<div id="dy-pane" class="panel" style="padding:16px">
      <div class="mini text-3" style="margin-bottom:10px">
        <b>免费通道低密度巡检 · 红线常量</b>：查询间隔 ≥${HARD.MIN_QUERY_INTERVAL_SEC} 秒 · 每日 ${HARD.MIN_ROUNDS_PER_DAY}–${HARD.MAX_ROUNDS_PER_DAY} 轮 · 每轮 ${HARD.MIN_WORDS_PER_ROUND}–${HARD.MAX_WORDS_PER_ROUND} 词 · 轮间隔 ≥${HARD.MIN_ROUND_GAP_MIN} 分钟 · 验证码后冷却 ${HARD.COOLDOWN_AFTER_CAPTCHA_MIN} 分钟（常量不可配置调大，设置页只能红线内收紧）
      </div>
      <div class="settings-row" style="flex-wrap:wrap">
        <span><b>巡检词表</b>（${words.length}/${HARD.MAX_WORDS_PER_ROUND}）</span>
        <span style="flex:1;min-width:200px">${chips}</span>
      </div>
      <div class="settings-row">
        <span><b>今日轮次</b>：${gate.roundsToday}/${cfg.roundsPerDay}</span>
        <span class="mini">${esc(gate.reason)}</span>
      </div>
      <div class="settings-row" style="flex-wrap:wrap">
        <span style="display:flex;gap:8px;align-items:center;flex-wrap:wrap">
          <span>渠道</span>
          <select id="dy-src"><option>黑猫</option><option>B站</option><option>微头条</option><option>抖音</option></select>
          <span>事件日期</span>
          <input type="date" id="dy-date" value="${dateStrOf(now)}">
        </span>
      </div>
      <div class="settings-row" style="flex-wrap:wrap">
        <textarea id="dy-paste" rows="4" style="flex:1;min-width:240px;background:var(--bg-card);color:var(--text-1);border:1px solid var(--border-default);border-radius:6px;padding:5px 8px;font:inherit" placeholder="每行一条：2026-11-24 标题（日期可省略，默认当天）"></textarea>
      </div>
      <div class="settings-row" style="justify-content:flex-end">
        <button class="btn" id="dy-import" ${gate.ok ? "" : "disabled"} title="${esc(gate.reason)}">${gate.ok ? `导入第 ${gate.roundsToday + 1} 轮` : "本轮不可导入"}</button>
      </div>
      <div class="settings-row"><span class="mini" id="dy-note">${esc(lastNote)}</span></div>
      <div class="settings-row" style="flex-wrap:wrap"><b>召回列表（时间倒序）</b></div>
      ${rowsHtml}
      <div class="settings-row" style="flex-wrap:wrap" id="dy-month">
        <span><b>本月汇总</b>：召回 ${sum.recalled} · 已筛 ${sum.byState[ST.SCREENED]} · 已忽略 ${sum.byState[ST.IGNORED]} · 未筛 ${sum.byState[ST.UNSORTED]} · 人工筛判率 ${rate}（未动 ${sum.untouched} 条）</span>
      </div>
      <div class="mini text-3">属地筛判靠人工：免费通道无作者属地能力，不做自动化属地告警（召回噪声率高，以人工筛判为准）；转待办后到「待办」版块跟进。</div>
    </div>`;
  };

  /** 局部重渲染：只替换面板节点并重绑，不动整页（避免重置其他页签/动画）。 */
  function refreshPane() {
    const pane = document.getElementById("dy-pane");
    if (!pane) return;
    pane.outerHTML = DM.view();
    DM.bind();
  }

  function setNote(text) {
    lastNote = text;
    const el = document.getElementById("dy-note");
    if (el) el.textContent = text;
  }

  function doImport() {
    const srcSel = document.getElementById("dy-src");
    const dateEl = document.getElementById("dy-date");
    const ta = document.getElementById("dy-paste");
    const text = ta ? ta.value : "";
    if (!String(text ?? "").trim()) { setNote("先粘贴本轮巡检结果（每行一条）再导入。"); return; }
    const cfg = DM.loadConfig().value;
    const items = DM.loadItems();
    const gate = DM.roundGate(cfg, items, new Date());
    if (!gate.ok) { setNote(`本轮不可导入：${gate.reason}`); return; }
    const dateKey = dateEl && dateEl.value ? dateEl.value : dateStrOf(new Date());
    const fresh = DM.parseRecallPaste(text, {
      source: srcSel ? srcSel.value : "黑猫",
      dateKey, now: nowIso(), roundNo: gate.roundsToday + 1
    });
    if (!fresh.length) { setNote("没有解析出任何条目（每行一条：日期可省略）。"); return; }
    const r = DM.importItems(items, fresh);
    DM.saveItems(r.items);
    if (ta) ta.value = "";
    setNote(`已导入：新增 ${r.added} 条、更新 ${r.updated} 条（第 ${gate.roundsToday + 1} 轮）。`);
    refreshPane();
  }

  async function doTodo(item, idx, items) {
    const title = `【舆情】${item.title}`;
    let todoId = null;
    try {
      const invoke = window.__TAURI__ && window.__TAURI__.core && window.__TAURI__.core.invoke;
      if (!invoke) throw new Error("后端命令不可用");
      const res = await invoke("todo_create", { title, dueDate: null, projectId: null, owner: null });
      todoId = (res && (res.id || res.todo_id || (res.todo && res.todo.id))) || "todo:created";
    } catch (e) {
      setNote(`转待办失败：${e && e.message ? e.message : e}`);
      return;
    }
    items[idx] = { ...items[idx], todoId };
    DM.saveItems(items);
    setNote(`已转待办：${title}`);
    refreshPane();
  }

  function applyAct(act, id) {
    const items = DM.loadItems();
    const idx = items.findIndex((r) => r.id === id);
    if (idx < 0) return;
    const item = items[idx];
    if (act === "screen") items[idx] = DM.screen(item, ST.SCREENED, { by: "我", at: nowIso(), reason: "人工筛判" });
    else if (act === "ignore") items[idx] = DM.screen(item, ST.IGNORED, { by: "我", at: nowIso(), reason: "人工判为无关" });
    else if (act === "undo") items[idx] = DM.undo(item);
    else if (act === "todo") { void doTodo(item, idx, items); return; }
    else return;
    DM.saveItems(items);
    refreshPane();
  }

  /** 面板事件绑定（幂等：同一节点只绑一次）。 */
  DM.bind = function bind() {
    const pane = document.getElementById("dy-pane");
    if (!pane || pane.__dyBound) return;
    pane.__dyBound = true;
    pane.addEventListener("click", (e) => {
      const btn = e.target.closest("button");
      if (!btn) return;
      if (btn.id === "dy-import") { if (!btn.disabled) doImport(); return; }
      const act = btn.dataset ? btn.dataset.dyAct : null;
      if (!act) return;
      const row = btn.closest("[data-dy-item]");
      if (!row) return;
      applyAct(act, row.dataset.dyItem);
    });
  };

  // ── 设置页 ② 情报采集卡内嵌行（六节名称与顺序不动，纯加行）──────────────
  DM.settingsRows = function settingsRows() {
    const { value: cfg } = DM.loadConfig();
    const words = Array.isArray(cfg.words) ? cfg.words : [];
    return `<div class="settings-row" style="flex-wrap:wrap">
      <span>抖音舆情巡检（免费通道）</span>
      <span style="display:flex;gap:8px;align-items:center;flex-wrap:wrap">
        <button class="btn btn--ghost" id="dy-toggle">${cfg.enabled ? "关闭" : "开启"}</button>
        <span class="mini">${cfg.enabled ? "已开启 · 监控版块出巡检页签" : "默认关闭 · 开启后监控版块出巡检页签"}</span>
      </span>
    </div>
    <div class="settings-row" style="flex-wrap:wrap">
      <span>巡检词表（每行一词，上限 ${HARD.MAX_WORDS_PER_ROUND}）</span>
      <textarea id="dy-words" rows="3" style="flex:1;min-width:220px;background:var(--bg-card);color:var(--text-1);border:1px solid var(--border-default);border-radius:6px;padding:5px 8px;font:inherit" placeholder="炸机${"\n"}维修${"\n"}售后…">${esc(words.join("\n"))}</textarea>
    </div>
    <div class="settings-row" style="flex-wrap:wrap">
      <span>每日轮次（${HARD.MIN_ROUNDS_PER_DAY}–${HARD.MAX_ROUNDS_PER_DAY}） / 查询间隔秒（≥${HARD.MIN_QUERY_INTERVAL_SEC}）</span>
      <span style="display:flex;gap:8px;align-items:center">
        <input type="number" id="dy-rounds" min="${HARD.MIN_ROUNDS_PER_DAY}" max="${HARD.MAX_ROUNDS_PER_DAY}" value="${cfg.roundsPerDay}" style="width:64px">
        <input type="number" id="dy-interval" min="${HARD.MIN_QUERY_INTERVAL_SEC}" value="${cfg.queryIntervalSec}" style="width:64px">
      </span>
    </div>
    <div class="settings-row"><span class="mini" id="dy-status">当前：每日 ${cfg.roundsPerDay} 轮 · 每轮 ${cfg.wordsPerRound}–${HARD.MAX_WORDS_PER_ROUND} 词 · 间隔 ≥${cfg.queryIntervalSec} 秒 · 轮间隔 ≥${cfg.roundGapMin} 分钟 · 词表 ${words.length}/${HARD.MAX_WORDS_PER_ROUND}（红线常量钳制生效）</span></div>`;
  };

  /** 设置页绑定（幂等：控件不存在时直接返回）。 */
  DM.bindSettings = function bindSettings() {
    const toggle = document.getElementById("dy-toggle");
    if (!toggle) return;

    toggle.addEventListener("click", () => {
      const cur = DM.loadConfig().value;
      DM.saveConfig({ enabled: !cur.enabled });
      const sec = document.getElementById("dy-sec");
      if (sec) { sec.innerHTML = DM.settingsRows(); DM.bindSettings(); }
    });

    const rounds = document.getElementById("dy-rounds");
    const interval = document.getElementById("dy-interval");
    const words = document.getElementById("dy-words");
    const status = document.getElementById("dy-status");
    const showStatus = (text) => { if (status) status.textContent = text; };

    if (rounds) rounds.addEventListener("change", () => {
      const r = DM.saveConfig({ roundsPerDay: Number(rounds.value) });
      rounds.value = r.value.roundsPerDay;
      showStatus(r.clamped.length ? `已按红线钳制：${clampNote(r.clamped)}` : "已保存。");
    });
    if (interval) interval.addEventListener("change", () => {
      const r = DM.saveConfig({ queryIntervalSec: Number(interval.value) });
      interval.value = r.value.queryIntervalSec;
      showStatus(r.clamped.length ? `已按红线钳制：${clampNote(r.clamped)}` : "已保存。");
    });
    if (words) words.addEventListener("change", () => {
      const r = DM.saveConfig({ words: words.value.split("\n") });
      const parts = [];
      if (r.clamped.length) parts.push(`已按红线钳制：${clampNote(r.clamped)}`);
      if (r.wordsTruncated) parts.push(`词表超上限，仅保留前 ${HARD.MAX_WORDS_PER_ROUND} 词`);
      if (!parts.length) parts.push(`已保存词表 ${r.value.words.length} 词。`);
      showStatus(parts.join("；"));
    });
  };
})();
