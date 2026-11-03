/* 播报触发条件单测（node --test，零依赖）。
 *
 * 为什么要单列一份：用户 P1 缺陷报告的第 3 条要求「保留全部兜底」，其中
 * 「免打扰只通知不播报」此前只以 inline 守卫的形式活在 app.js 的
 * brief-delivered 回调里——那一段没有任何测试能碰到，等于没验。
 * 本轮把这条判定抽成纯函数 shouldSpeakBrief()，让它可被单测直接覆盖。
 *
 * 判定输入全部来自真实 payload（lib.rs 返回的 brief-delivered 事件字段）
 * 与界面总开关状态，不引入任何新概念。
 */
"use strict";

const test = require("node:test");
const assert = require("node:assert");
const fs = require("node:fs");
const path = require("node:path");

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

const shouldSpeakBrief = new Function(
  `${extractFn("shouldSpeakBrief")}; return shouldSpeakBrief;`
)();

test("正常时段、语音开、总开关开、有文案 → 必须播报", () => {
  assert.equal(
    shouldSpeakBrief({
      voice_enabled: true,
      notification_suppressed: false,
      master_checked: true,
      speech: "早上好。今日晨报。",
    }),
    true
  );
});

test("免打扰时段 → 只通知、不播报（用户要求保留的兜底）", () => {
  // notification_suppressed 由 Rust 侧按 quiet_hours 算好传下来，
  // 界面不得自行改写这个结论——所以它就是唯一判据。
  assert.equal(
    shouldSpeakBrief({
      voice_enabled: true,
      notification_suppressed: true,
      master_checked: true,
      speech: "早上好。今日晨报。",
    }),
    false
  );
});

test("语音偏好关 → 不播报，但晨报本身照常送达", () => {
  assert.equal(
    shouldSpeakBrief({
      voice_enabled: false,
      notification_suppressed: false,
      master_checked: true,
      speech: "早上好。今日晨报。",
    }),
    false
  );
});

test("界面总开关关 → 立即不播报（实时跟随，不等下次偏好生效）", () => {
  assert.equal(
    shouldSpeakBrief({
      voice_enabled: true,
      notification_suppressed: false,
      master_checked: false,
      speech: "早上好。今日晨报。",
    }),
    false
  );
});

test("没有实时文案 → 不播（宁可静默，也不播可能过期的预录音）", () => {
  for (const empty of [undefined, null, "", "   "]) {
    assert.equal(
      shouldSpeakBrief({
        voice_enabled: true,
        notification_suppressed: false,
        master_checked: true,
        speech: empty,
      }),
      false,
      `空文案（${JSON.stringify(empty)}）不得播出任何声音`
    );
  }
});

test("四个条件缺任一都为假——不是三重否决", () => {
  // 逐项关掉，每次只关一个，确认每个条件都真的在起作用
  // （防止某个条件被写成 || 关系而测试仍全绿）。
  const base = {
    voice_enabled: true,
    notification_suppressed: false,
    master_checked: true,
    speech: "早上好。今日晨报。",
  };
  assert.equal(shouldSpeakBrief(base), true, "前置：全开时应为真");
  for (const k of ["voice_enabled", "master_checked"]) {
    assert.equal(shouldSpeakBrief({ ...base, [k]: false }), false, `${k}=false 必须否决`);
  }
  assert.equal(
    shouldSpeakBrief({ ...base, notification_suppressed: true }),
    false,
    "notification_suppressed=true 必须否决"
  );
  assert.equal(shouldSpeakBrief({ ...base, speech: "" }), false, "空文案必须否决");
});
