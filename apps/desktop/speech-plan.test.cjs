/* 语音播报编排单测（node --test，零依赖）。
 *
 * 针对用户 2026-10-30 报告的 P1 缺陷：「播报语音和实际数据不符合，
 * 应该是没有做动态内容调整」。
 *
 * 这些断言在修复前的语义下必然失败——旧 speakBrief 在 pack 模式
 * new Audio(固定 WAV) 后直接 return，text 参数被整段丢弃。
 */
"use strict";

const test = require("node:test");
const assert = require("node:assert");
const {
  planSpeech,
  SPEECH_HEAD_CUT,
  SCENE_FILE,
  stripSpokenGreeting,
} = require("./src/speech-plan.js");

/** 一条典型的实时晨报文案（来自 crates/wb-core/src/brief.rs speech_text()）。 */
const TEXT =
  "早上好。今日晨报。今日到期 3 项，临期 2 项，逾期 1 项。今日到期：政府数据共享平台验收。";

/** 全部 9 个真实场景 + 1 个版本启动别名。 */
const ALL_SCENES = Object.keys(SCENE_FILE);

function ttsTexts(plan) {
  return plan.filter((s) => s.kind === "tts").map((s) => s.text);
}

test("pack 模式必须读出实时文案（缺陷 1：不得只播固定样音）", () => {
  const plan = planSpeech({ pack: true, scene: "brief", text: TEXT });
  const spoken = ttsTexts(plan).join("");
  assert.ok(spoken.length > 0, "pack 模式下没有任何 TTS 步骤 → 文案被丢弃");
  assert.ok(
    spoken.includes("3 项"),
    `pack 模式下 TTS 未读到实时数字，实际播报：${spoken}`
  );
});

test("pack 与 system 两种模式读的是同一份实时文案", () => {
  const pack = ttsTexts(planSpeech({ pack: true, scene: "brief", text: TEXT })).join("");
  const sys = ttsTexts(planSpeech({ pack: false, scene: "brief", text: TEXT })).join("");
  assert.strictEqual(pack, sys, "两档模式播报内容必须一致，差异只应体现在音色");
});

test("9 条样音全含固定数值 → 不得整条作为开场", () => {
  // 除 brief_late 外，所有场景都不允许出现 wav 步骤
  for (const sc of ALL_SCENES) {
    const plan = planSpeech({ pack: true, scene: sc, text: TEXT });
    const hasWav = plan.some((s) => s.kind === "wav");
    if (!SPEECH_HEAD_CUT[sc]) {
      assert.strictEqual(hasWav, false, `场景 ${sc} 不应保留样音（其样音含固定数值）`);
    }
    assert.ok(ttsTexts(plan).length > 0, `场景 ${sc} 必须至少有一处实时 TTS 播报`);
  }
});

test("brief_late 保留音色开场，且切点落在静音区间内", () => {
  const plan = planSpeech({ pack: true, scene: "brief_late", text: TEXT });
  const wav = plan.find((s) => s.kind === "wav");
  assert.ok(wav, "brief_late 应保留音色开场");
  assert.strictEqual(wav.src, "./voice/s2_brief_late.wav");
  // 实测静音区间 2.81–3.62s，切点必须落在内部且不贴边
  assert.ok(wav.until > 2.81 && wav.until < 3.62, `切点 ${wav.until} 不在静音区间内`);
});

test("开场步骤必须排在 TTS 之前", () => {
  const plan = planSpeech({ pack: true, scene: "brief_late", text: TEXT });
  assert.strictEqual(plan[0].kind, "wav", "第一段必须是样音开场");
  assert.strictEqual(plan[plan.length - 1].kind, "tts", "最后一段必须是实时 TTS");
});

test("开场已念的问候语不得被 TTS 重复念一遍", () => {
  const late = "早上好，指挥官。现在是补发晨报。今日到期 3 项，临期 2 项，逾期 1 项。";
  const plan = planSpeech({ pack: true, scene: "brief_late", text: late });
  const spoken = ttsTexts(plan).join("");
  assert.ok(
    !spoken.includes("现在是补发晨报"),
    `开场样音已念过的问候语被 TTS 重复念了一遍：${spoken}`
  );
  assert.ok(spoken.includes("3 项"), "去重不得把实时数字一起丢掉");
});

test("无实时文案时不播任何东西（宁可静默也不播过期样音）", () => {
  for (const sc of ALL_SCENES) {
    assert.deepStrictEqual(planSpeech({ pack: true, scene: sc, text: "" }), []);
    assert.deepStrictEqual(planSpeech({ pack: true, scene: sc, text: "   " }), []);
    assert.deepStrictEqual(planSpeech({ pack: true, scene: sc, text: null }), []);
  }
});

test("未知场景 / 空场景回落纯 TTS，且仍读实时文案", () => {
  const p1 = planSpeech({ pack: true, scene: "not_a_scene", text: TEXT });
  assert.deepStrictEqual(p1.map((s) => s.kind), ["tts"]);
  assert.strictEqual(p1[0].text, TEXT);
  const p2 = planSpeech({ pack: true, scene: null, text: TEXT });
  assert.deepStrictEqual(p2.map((s) => s.kind), ["tts"]);
});

test("system 模式不含任何样音步骤", () => {
  for (const sc of ALL_SCENES) {
    const plan = planSpeech({ pack: false, scene: sc, text: TEXT });
    assert.strictEqual(plan.some((s) => s.kind === "wav"), false, `system 模式不应播样音：${sc}`);
  }
});

test("样音缺失 / 播放失败时有兜底：编排本身不依赖音频是否可用", () => {
  // 编排只描述意图；执行层遇到 error 必须转 TTS。这里锁定「无论样音是否可用，
  // TTS 步骤始终存在」这一不变式，兜底才不会丢文案。
  for (const sc of ALL_SCENES) {
    const plan = planSpeech({ pack: true, scene: sc, text: TEXT });
    assert.ok(plan.some((s) => s.kind === "tts"), `${sc}: 没有 TTS 兜底`);
  }
});

test("stripSpokenGreeting 只剥已知前缀，不误伤其它文案", () => {
  assert.strictEqual(
    stripSpokenGreeting("早上好，指挥官。现在是补发晨报。今日到期 3 项。", "brief_late"),
    "今日到期 3 项。"
  );
  // 非该场景不得被剥
  assert.strictEqual(stripSpokenGreeting("早上好。今日晨报。", "brief"), "早上好。今日晨报。");
  // 剥完为空则返回原串
  assert.strictEqual(
    stripSpokenGreeting("早上好，指挥官。现在是补发晨报。", "brief_late"),
    "早上好，指挥官。现在是补发晨报。"
  );
});


/* ── 接线门禁：防「脚本没加载 → 静默完全没声音」这类最难查的故障 ──
 * index.html 必须加载 speech-plan.js，且必须排在 app.js 之前。
 * 顺序错了会让 window.WbSpeechPlan 为 undefined，异常被 speakBrief 的
 * try/catch 吞掉，表现为「一点声音都没有」——不报错、不提示，极难定位。 */
test("index.html 必须加载 speech-plan.js，且顺序在 app.js 之前", () => {
  const fs = require("node:fs");
  const path = require("node:path");
  const html = fs.readFileSync(path.join(__dirname, "src/index.html"), "utf8");
  const planAt = html.indexOf('<script src="speech-plan.js">');
  const appAt = html.indexOf('<script src="app.js">');
  assert.ok(planAt >= 0, "index.html 未加载 speech-plan.js → 语音编排不可用");
  assert.ok(appAt >= 0, "index.html 未加载 app.js");
  assert.ok(planAt < appAt, "speech-plan.js 必须排在 app.js 之前，否则挂载晚于使用");
});

/* 打包门禁：tauri 的 frontendDist 必须覆盖 src 目录，新脚本才会随包分发。 */
test("tauri frontendDist 覆盖 src 目录（新脚本才会进安装包）", () => {
  const fs = require("node:fs");
  const path = require("node:path");
  const conf = JSON.parse(
    fs.readFileSync(path.join(__dirname, "src-tauri/tauri.conf.json"), "utf8")
  );
  const dist = conf.build && conf.build.frontendDist;
  assert.ok(dist, "frontendDist 缺失");
  // ../src 相对于 src-tauri 目录 → 即 apps/desktop/src，speech-plan.js 所在处
  assert.strictEqual(dist.replace(/^\.\.\//, "").replace(/^\.\//, ""), "src");
});
