/* 晨报「播报 ↔ 界面」覆盖面对齐测试（node --test，零依赖）。
 *
 * 背景：用户 P1 缺陷「播报语音和实际数据不符合」。修完之后播报侧补齐了
 * 日期 / 计数 / 要点，但曾存在一个**反方向**的不一致：
 *   Rust 的 MorningBrief 曾是「四段结构」（架构 §6.2 D）：
 *     ① 当日到期/应启动 ② 临期+逾期 ③ 今日行程 ④ 周一附加周聚合
 *   speech_text() 会念 ④ 的「本周应闭环 N 项，应启动 M 项」，
 *   但界面 viewBrief() 只渲染 ①②②，**从不渲染 ④**，也不渲染 ③。
 *   于是周一播报说出「本周应闭环 5 项」，用户在界面上找不到这个 5。
 *   ③ today_schedule 则是三端不一致的恒空字段（核心层恒空、接口发出、前端不读）。
 *
 * 2026-11-03 16:05 用户通过决策卡拍板（本文件由「钉住不一致」改为「钉住下线」）：
 *   1. 周聚合整体下线（「两边都去掉」）：播报不再念、核心层不再算；
 *   2. today_schedule 恒空字段从接口删除（今日行程功能排 v3.x）。
 *
 * 本文件静态比对三端的生产代码：播报文本、核心层数据结构、接口 payload 里
 * 都不得再出现这两类字样——谁把周聚合或恒空字段加回来，这里立刻红。
 * 注意：只扫 #[cfg(test)] 之前的生产段——Rust 侧的反向断言测试自身
 * 必须引用这些字样（断言它们不出现在播报输出里），不能误伤。
 */
"use strict";

const test = require("node:test");
const assert = require("node:assert");
const fs = require("node:fs");
const path = require("node:path");

const APP_JS = path.join(__dirname, "src/app.js");
const BRIEF_RS = path.join(__dirname, "../../crates/wb-core/src/brief.rs");
const RULES_RS = path.join(__dirname, "../../crates/wb-core/src/rules.rs");
const TAURI_LIB_RS = path.join(__dirname, "src-tauri/src/lib.rs");

function read(p) {
  assert.ok(fs.existsSync(p), `找不到文件 ${p}`);
  return fs.readFileSync(p, "utf8");
}

/** 只取 #[cfg(test)] 之前的生产代码段（没有测试模块则整份）。 */
function prodPart(p) {
  const src = read(p);
  const i = src.indexOf("#[cfg(test)]");
  return i > 0 ? src.slice(0, i) : src;
}

test("周聚合整体下线：播报文本、核心层、接口 payload 不得再出现周聚合", () => {
  const briefProd = prodPart(BRIEF_RS);
  const rulesProd = prodPart(RULES_RS);
  const tauriProd = prodPart(TAURI_LIB_RS);
  const appJs = read(APP_JS);

  // 播报侧：speech_text / notification_text 的周聚合措辞必须整体消失
  assert.ok(
    !briefProd.includes("本周应闭环"),
    "brief.rs 生产段不得再出现「本周应闭环」——周聚合已于 2026-11-03 拍板整体下线" +
      "（用户选「两边都去掉」），播报念出界面上没有的数字，就是「播报与实际数据不符」"
  );
  assert.ok(
    !briefProd.includes("应启动 {} 项"),
    "brief.rs 生产段不得保留周聚合播报格式串（含注释里的残留模板）"
  );

  // 数据侧：核心层不再计算周聚合
  assert.ok(
    !rulesProd.includes("WeeklyAggregate"),
    "rules.rs 生产段不得再定义/引用 WeeklyAggregate（核心层不再计算周聚合）"
  );
  assert.ok(
    !/pub\s+weekly\s*:/.test(rulesProd),
    "MorningBrief 不得再带 weekly 字段"
  );

  // 接口侧：payload 不再发出；前端不再据其选 S3 场景
  assert.ok(
    !/"weekly"/.test(tauriProd),
    "Tauri 壳层 payload（alerts / deliver_brief counts）不得再发 weekly 键"
  );
  assert.ok(
    !/counts\s*\.\s*weekly/.test(appJs),
    "app.js 不得再读 counts.weekly 派发 S3 周聚合场景"
  );
});

test("today_schedule 已从接口删除：核心层、payload、前端不得再出现该字段", () => {
  const rulesProd = prodPart(RULES_RS);
  const tauriProd = prodPart(TAURI_LIB_RS);
  const appJs = read(APP_JS);

  for (const [name, src] of [["rules.rs 生产段", rulesProd], ["Tauri lib.rs 生产段", tauriProd]]) {
    assert.ok(
      !src.includes("today_schedule"),
      `${name} 不得再出现 today_schedule——恒空字段已于 2026-11-03 拍板从接口删除` +
        `（今日行程功能排 v3.x；数据来源 schedule_block 全仓零读写，实现前不得回填占位字段）`
    );
  }
  assert.ok(
    !appJs.includes("today_schedule"),
    "app.js 不得读取 today_schedule（读到只会是 undefined）"
  );
});
