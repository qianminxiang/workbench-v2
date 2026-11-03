/* 晨报「播报 ↔ 界面」覆盖面对齐测试（node --test，零依赖）。
 *
 * 背景：用户 P1 缺陷「播报语音和实际数据不符合」。修完之后播报侧补齐了
 * 日期 / 计数 / 要点，但本轮核实发现一个**反方向**的不一致：
 *   Rust 的 MorningBrief 是「四段结构」（架构 §6.2 D）：
 *     ① 当日到期/应启动 ② 临期+逾期 ③ 今日行程 ④ 周一附加周聚合
 *   speech_text() 会念 ④ 的「本周应闭环 N 项，应启动 M 项」，
 *   但界面 viewBrief() 只渲染 ①②②，**从不渲染 ④**，也不渲染 ③。
 *
 * 于是周一播报说出「本周应闭环 5 项」，用户在界面上找不到这个 5 ——
 * 这与「播报和实际数据不符」是同一个症状，只是方向反过来（播报多念了）。
 *
 * 本测试静态比对两侧：播报会念到的每一类事实，界面必须有地方能对上。
 */
"use strict";

const test = require("node:test");
const assert = require("node:assert");
const fs = require("node:fs");
const path = require("node:path");

const APP_JS = path.join(__dirname, "src/app.js");
const BRIEF_RS = path.join(__dirname, "../../crates/wb-core/src/brief.rs");

function read(p) {
  assert.ok(fs.existsSync(p), `找不到文件 ${p}`);
  return fs.readFileSync(p, "utf8");
}

/** viewBrief() 的渲染函数体。 */
function viewBriefBody() {
  const src = read(APP_JS);
  const start = src.indexOf("function viewBrief()");
  assert.ok(start >= 0, "app.js 里找不到 viewBrief()");
  const end = src.indexOf("\nfunction viewNext", start) > 0
    ? src.indexOf("\nfunction viewNext", start)
    : src.indexOf("\n// ──", start + 200);
  return src.slice(start, end);
}

/** speech_text() 会念出的「事实类别」。 */
function spokenFacts() {
  return read(BRIEF_RS).includes('"本周应闭环 {} 项，应启动 {} 项。"');
}

test("播报念到的周聚合，界面必须也能对上（否则周一就是「说了界面上没有的数」）", () => {
  assert.ok(spokenFacts(), "前置：speech_text 当前确实会念周聚合");
  const vb = viewBriefBody();
  const rendersWeekly =
    /b\.weekly/.test(vb) &&
    (vb.includes("should_close") || vb.includes("应闭环"));
  assert.ok(
    rendersWeekly,
    "界面 viewBrief() 没有渲染 brief.weekly —— 但播报会念「本周应闭环 N 项，应启动 M 项」。\n" +
      "周一播报说出的数字，用户在晨报页上找不到，就是「播报与实际数据不符」。\n" +
      "修法二选一：(a) 界面补渲染 ④ 周聚合；(b) 播报去掉周聚合。属规格取舍，需用户裁决。"
  );
});

test("接口发出 today_schedule，界面必须消费它（否则是恒空字段的空转）", () => {
  const src = read(APP_JS);
  // 现状：today_schedule 在核心层恒为空（rules.rs 里 `today_schedule: Vec::new()`），
  // 全仓无其它赋值点；lib.rs 仍把它发出去；前端完全不读。
  // 这条断言的作用是把「三端不一致」钉住，避免它悄悄被当成已实现。
  const consumed = src.includes("today_schedule");
  assert.ok(
    consumed,
    "today_schedule 三端不一致：核心层恒为空（rules.rs: `today_schedule: Vec::new()`，" +
      "全仓无其它赋值点）、lib.rs 仍发出、前端从不读取。\n" +
      "它既是未完成的 ③ 今日行程段，又是一个「接口声称有数据、实际永远空」的坑。\n" +
      "要么实现它，要么从 payload 里删掉，不该这样挂着。"
  );
});
