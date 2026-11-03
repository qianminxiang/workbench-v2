/* 语音播报「执行层」单测（node --test，零依赖）。
 *
 * 与 speech-plan.test.cjs 的分工：
 *   - speech-plan.test.cjs 测「该播什么」（纯函数编排，不碰 DOM）
 *   - 本文件测「怎么播」（speakBrief / playWavHead / speakTts 三个真实函数）
 *
 * 为什么必须有这一层：编排正确不代表真有声音。用户明确要求保留的兜底是
 * 「样音缺失/播放失败整段回落 TTS、语音失败不阻断主流程」——这些分支全在
 * 执行层，只测编排等于没验。
 *
 * 做法：从 app.js 抽取这三个函数的源码原文，注入假 window / Audio /
 * SpeechSynthesisUtterance 后真实执行。不 mock 被测逻辑本身，只 mock 环境。
 */
"use strict";

const test = require("node:test");
const assert = require("node:assert");
const fs = require("node:fs");
const path = require("node:path");
const vm = require("node:vm");

const APP_JS = path.join(__dirname, "src/app.js");
const SRC = fs.readFileSync(APP_JS, "utf8");

/** 抽取函数源码：从 `function name(` 起按花括号配平截取，避免靠行号硬编码。 */
function extractFn(name) {
  const start = SRC.indexOf(`function ${name}(`);
  assert.ok(start >= 0, `app.js 里找不到 function ${name}`);
  let i = SRC.indexOf("{", start);
  let depth = 0;
  for (; i < SRC.length; i++) {
    if (SRC[i] === "{") depth++;
    else if (SRC[i] === "}") {
      depth--;
      if (depth === 0) return SRC.slice(start, i + 1);
    }
  }
  throw new Error(`function ${name} 的花括号未配平`);
}

const CODE = ["speakBrief", "playWavHead", "speakTts"].map(extractFn).join("\n");

/** 造一个可控的假环境，返回 { win, spoken, audioCalls, timers }。 */
function makeEnv(opts) {
  const o = opts || {};
  const spoken = [];        // TTS 真正读出去的文本
  const audioCalls = [];     // new Audio 的 src

  class FakeAudio {
    constructor(src) {
      audioCalls.push(src);
      this.src = src;
      this.currentTime = 0;
      this._h = {};
      if (o.audioThrow) throw new Error("构造 Audio 失败");
    }
    addEventListener(type, fn) { (this._h[type] = this._h[type] || []).push(fn); }
    play() {
      if (o.playRejects) return Promise.reject(new Error("播放被拒绝（自动播放策略）"));
      if (o.audioError) {
        // 资源缺失/解码失败：异步派发 error 事件
        setImmediate(() => (this._h.error || []).forEach((f) => f()));
        return Promise.resolve();
      }
      if (o.headCompletes) {
        // 模拟正常播到 until：推进 currentTime 并派发 timeupdate
        setImmediate(() => {
          this.currentTime = 99;
          (this._h.timeupdate || []).forEach((f) => f());
        });
      }
      return Promise.resolve();
    }
    pause() { if (o.pauseThrows) throw new Error("pause 失败"); }
  }

  const timers = [];
  const win = {
    WbSpeechPlan: o.planner === undefined ? null : o.planner,
    speechSynthesis: o.noSynth ? null : {
      cancel() {},
      getVoices() { return [{ lang: "zh-CN", name: "zh" }]; },
      speak(u) { spoken.push(u.text); },
    },
  };
  const ctx = {
    window: win,
    // app.js 里 state 是**裸全局**（不是 window.state），必须作为上下文全局提供，
    // 否则 ReferenceError 会被 speakBrief 的 try/catch 吞掉，表现为「没声音」。
    state: { prefs: { voice_pack: "pack" } },
    Audio: FakeAudio,
    SpeechSynthesisUtterance: class { constructor(t) { this.text = t; } },
    setTimeout: (fn, ms) => { timers.push({ fn, ms }); return timers.length; },
    setImmediate,
    Promise,
    Error,
    console,
  };
  ctx.globalThis = ctx;
  vm.createContext(ctx);
  vm.runInContext(CODE, ctx, { filename: "app.js#extract" });
  return { ctx, win, spoken, audioCalls, timers };
}

/** 等异步回调（setImmediate / promise）跑完。 */
const tick = () => new Promise((r) => setImmediate(r));


test("system 模式：直接读实时文案，不 new Audio", async () => {
  const env = makeEnv({ planner: require("./src/speech-plan.js") });
  env.ctx.state = { prefs: { voice_pack: "system" } };
  env.ctx.speakBrief("早上好。今日晨报。今日到期 3 项。", "brief");
  await tick();
  assert.deepStrictEqual(env.audioCalls, [], "system 模式不应播样音");
  assert.deepStrictEqual(env.spoken, ["早上好。今日晨报。今日到期 3 项。"]);
});

test("pack 模式：真实播放一次样音开场 + TTS 读实时文案（两段都必须发生）", async () => {
  const env = makeEnv({ planner: require("./src/speech-plan.js"), headCompletes: true });
  env.ctx.state = { prefs: { voice_pack: "pack" } };
  const text = "早上好，指挥官。现在是补发晨报。今日到期 3 项，临期 2 项，逾期 1 项。";
  env.ctx.speakBrief(text, "brief_late");
  // 开场是异步完成的，等事件与后续递归走完
  for (let i = 0; i < 6; i++) await tick();
  assert.deepStrictEqual(env.audioCalls, ["./voice/s2_brief_late.wav"], "应播放 s2 样音开场");
  assert.strictEqual(env.spoken.length, 1, "开场之后必须还有一段 TTS");
  assert.ok(env.spoken[0].includes("3 项"), `TTS 必须读到实时数字，实际：${env.spoken[0]}`);
  assert.ok(!env.spoken[0].includes("现在是补发晨报"), "问候语不应被重复");
});

test("兜底 A：样音资源缺失（error 事件）→ 仍然发声，且读到实时文案", async () => {
  const env = makeEnv({ planner: require("./src/speech-plan.js"), audioError: true });
  env.ctx.state = { prefs: { voice_pack: "pack" } };
  env.ctx.speakBrief("今日到期 3 项，临期 2 项。", "brief_late");
  for (let i = 0; i < 6; i++) await tick();
  assert.deepStrictEqual(env.audioCalls, ["./voice/s2_brief_late.wav"]);
  assert.ok(env.spoken.length >= 1, "样音失败后必须回落 TTS，不能静默");
  assert.ok(env.spoken.join("").includes("3 项"), "回落 TTS 必须读到实时文案");
});

test("兜底 B：play() promise 被拒绝（自动播放策略拦截）→ 仍然发声", async () => {
  const env = makeEnv({ planner: require("./src/speech-plan.js"), playRejects: true });
  env.ctx.state = { prefs: { voice_pack: "pack" } };
  env.ctx.speakBrief("今日到期 3 项。", "brief_late");
  for (let i = 0; i < 6; i++) await tick();
  assert.ok(env.spoken.length >= 1, "播放被拒后必须回落 TTS");
  assert.ok(env.spoken.join("").includes("3 项"));
});

test("兜底 C：new Audio 直接抛错 → 仍然发声", async () => {
  const env = makeEnv({ planner: require("./src/speech-plan.js"), audioThrow: true });
  env.ctx.state = { prefs: { voice_pack: "pack" } };
  env.ctx.speakBrief("今日到期 3 项。", "brief_late");
  for (let i = 0; i < 6; i++) await tick();
  assert.ok(env.spoken.length >= 1, "Audio 构造失败后必须回落 TTS");
  assert.ok(env.spoken.join("").includes("3 项"));
});

test("兜底 D：timeupdate 从不触发 → 计时器兜底收口，不卡住后续 TTS", async () => {
  const env = makeEnv({ planner: require("./src/speech-plan.js") }); // headCompletes=false
  env.ctx.state = { prefs: { voice_pack: "pack" } };
  env.ctx.speakBrief("今日到期 3 项。", "brief_late");
  await tick();
  assert.deepStrictEqual(env.spoken, [], "开场还没收口时不应提前念 TTS");
  assert.strictEqual(env.timers.length, 1, "必须挂了一个兜底计时器");
  // 触发兜底计时器，模拟 until+0.6s 到点
  env.timers[0].fn();
  await tick();
  assert.ok(env.spoken.join("").includes("3 项"), "兜底收口后必须继续念实时文案");
});

test("编排模块缺席（脚本没加载）→ 回落 TTS，绝不静默", async () => {
  const env = makeEnv({ planner: null }); // window.WbSpeechPlan 不存在
  env.ctx.state = { prefs: { voice_pack: "pack" } };
  env.ctx.speakBrief("早上好。今日晨报。今日到期 3 项。", "brief");
  await tick();
  assert.deepStrictEqual(env.audioCalls, [], "拿不到编排就不该妄自播样音");
  assert.deepStrictEqual(env.spoken, ["早上好。今日晨报。今日到期 3 项。"],
    "编排缺席必须由 TTS 直接读实时文案");
});

test("编排模块存在但 planSpeech 不是函数 → 同样回落 TTS", async () => {
  const env = makeEnv({ planner: { planSpeech: "not-a-function" } });
  env.ctx.state = { prefs: { voice_pack: "pack" } };
  env.ctx.speakBrief("今日到期 3 项。", "brief");
  await tick();
  assert.deepStrictEqual(env.spoken, ["今日到期 3 项。"]);
});

test("TTS 完全不可用（无 speechSynthesis）→ 不抛异常、主流程不被打断", async () => {
  const env = makeEnv({ planner: require("./src/speech-plan.js"), noSynth: true });
  env.ctx.state = { prefs: { voice_pack: "pack" } };
  assert.doesNotThrow(() => env.ctx.speakBrief("今日到期 3 项。", "brief_late"));
  for (let i = 0; i < 6; i++) await tick();
  // 没有 synth 时 speakTts 静默 return，但绝不能抛出到主流程
  assert.deepStrictEqual(env.spoken, []);
});

test("无实时文案 → 既不播样音也不念空串（宁可静默）", async () => {
  const env = makeEnv({ planner: require("./src/speech-plan.js"), headCompletes: true });
  env.ctx.state = { prefs: { voice_pack: "pack" } };
  env.ctx.speakBrief("", "brief_late");
  for (let i = 0; i < 6; i++) await tick();
  assert.deepStrictEqual(env.audioCalls, [], "没有文案就不该播过期样音");
  assert.deepStrictEqual(env.spoken, [], "不该念空串");
});

test("开场只收口一次：timeupdate 与计时器都触发也不会重复念 TTS", async () => {
  const env = makeEnv({ planner: require("./src/speech-plan.js"), headCompletes: true });
  env.ctx.state = { prefs: { voice_pack: "pack" } };
  env.ctx.speakBrief("今日到期 3 项。", "brief_late");
  for (let i = 0; i < 4; i++) await tick();
  const afterEvent = env.spoken.length;
  assert.strictEqual(afterEvent, 1, "开场收口后应恰好念一次 TTS");
  // 再触发残留的兜底计时器
  env.timers.forEach((t) => t.fn());
  await tick();
  assert.strictEqual(env.spoken.length, 1, "重复收口不得导致 TTS 念第二遍");
});
