/* 综合工作台 v2.2.4 · 语音播报编排（纯函数，零依赖，Node/浏览器共用）。
 *
 * 背景：pack 模式曾直接播放整条固定 WAV、完全丢弃 text 参数（实时文案），
 * 导致用户报告的 P1 缺陷「播报语音和实际数据不符合」。本模块把「该播什么」
 * 与「怎么播」分离：这里只产出播报步骤，执行由 app.js 负责，因此可脱离 DOM 单测。
 *
 * 决定性约束（2026-10-30 实测，不是推测）：
 * 9 条定稿样音逐条 STT 转写后，每一条都含固定示例数值——
 *   s1「今日到期三项，临期两项，逾期一项」、s3「应闭环4项」、
 *   s4「已过6天」、s5「还有3天」、s6「60%…35%…25个百分点」、
 *   s7「两项…10点…3点」、s8「新版本2.2.0」、s9「5项待办」。
 * 所以样音不能整条参与播报，否则会先念一组旧数字再念真实数字，
 * 等于没修好同一个问题。
 *
 * 开场可切性（同样实测）：
 *   只有 brief_late 在 2.81s 处有 0.81s 句间静音，之前转写为
 *   「早上好，指挥官。现在是补发晨报。」——不含数字，可作音色开场。
 *   其余 8 条（含 s1）：5s 内没有任何 >=0.10s 的句间静音（s1 最长仅 0.12s
 *   且位于句中），切任何位置都会截断词尾，故一律不保留样音，直接读实时文案。
 *
 * 收尾同理不可用：可切尾段逐条转写后仍是残句或含数字
 *   （s2「…逾期一下」、s3「政策评审材料提交」、s7「下午3点政策评审会」、
 *    s8「发现新版本2.2.0」、s9「已按优先级排好」），没有中性收束语可用。
 *   该规格取舍已在任务评论上报，待用户裁决。
 */
"use strict";

(function (root, factory) {
  const api = factory();
  if (typeof module === "object" && module.exports) module.exports = api;
  else root.WbSpeechPlan = api;
})(typeof globalThis !== "undefined" ? globalThis : this, function () {
  /** 场景 → 预录音文件名。与 app.js 的 VOICE_PACK_SCENES 保持一致；
   *  单列一份是因为编排要能在 Node 里单测，不能依赖 DOM 侧全局对象。 */
  const SCENE_FILE = {
    brief: "s1_brief",
    version_boot: "s1_brief",
    brief_late: "s2_brief_late",
    // weekly（s3_weekly）场景已随周聚合整体下线移除（2026-11-03 用户拍板）：
    // 核心层不再计算 weekly，前端也不再派发该场景，保留映射只会误导后来者。
    overdue: "s4_overdue",
    duesoon: "s5_duesoon",
    behind: "s6_behind",
    schedule: "s7_schedule",
    update: "s8_update",
    todos: "s9_todos",
  };

  /** 场景 → 可安全裁作「音色开场」的秒数。
   *  判据（两条同时成立）：切点落在 >=0.4s 的句间静音内（不截断词尾），
   *  且切点之前的转写不含任何数值（不会先念旧数字）。
   *  实测仅 brief_late 满足：静音区间 2.81–3.62s（0.81s），
   *  取区间内 2.90s（距两端各留 0.09s 余量，避免切在边界上）。 */
  const SPEECH_HEAD_CUT = {
    brief_late: 2.90,
  };

  /** 开场样音已念过的问候语。播完开场后 TTS 必须跳过同义前缀，
   *  否则「早上好，指挥官。现在是补发晨报。」会被念两遍。 */
  const HEAD_SPOKEN_GREETING = {
    brief_late: [
      "早上好，指挥官。现在是补发晨报。",
      "早上好。现在是补发晨报。",
    ],
  };

  /** 剥掉开场样音已经念过的前缀；剥完为空则返回原串（宁可重复也不静默）。 */
  function stripSpokenGreeting(text, scene) {
    const prefixes = (scene && HEAD_SPOKEN_GREETING[scene]) || [];
    for (const p of prefixes) {
      if (text.startsWith(p)) {
        const rest = text.slice(p.length).trim();
        return rest || text;
      }
    }
    return text;
  }

  /** 产出播报步骤。
   * @param {{pack:boolean, scene:string|null, text:string}} opts
   * @returns {Array<{kind:"wav",src:string,until:number}|{kind:"tts",text:string}>}
   * 不变式：只要给了非空 text，最终一定有一步 tts 把它读出来；
   * 没有 text 时返回空数组——宁可静默，也不播可能过期的预录音。 */
  function planSpeech(opts) {
    const o = opts || {};
    const text = typeof o.text === "string" ? o.text.trim() : "";
    if (!text) return [];
    const out = [];
    const scene = o.pack && o.scene ? o.scene : null;
    const file = scene ? SCENE_FILE[scene] : null;
    const head = scene ? SPEECH_HEAD_CUT[scene] : null;
    let spoken = text;
    if (file && head) {
      out.push({ kind: "wav", src: `./voice/${file}.wav`, until: head });
      spoken = stripSpokenGreeting(text, scene);
    }
    out.push({ kind: "tts", text: spoken });
    return out;
  }

  return { planSpeech, SPEECH_HEAD_CUT, SCENE_FILE, stripSpokenGreeting };
});
