/* Rust → JS 事件字段契约测试（node --test，零依赖）。
 *
 * 为什么需要这一层：brief-delivered 的 payload 由 Rust 的 deliver_brief() 用
 * serde_json::json!{} 拼出，前端在 app.js 里按字段名取值。这中间没有任何
 * 类型检查——Rust 改个字段名（brief_date → date 或反之），JS 侧只会读到
 * undefined，不报错、不崩溃，只是「悄悄不生效」。
 *
 * 这正是用户 P1 缺陷「播报语音和实际数据不符合」的同一类失败模式：
 * 字段名对不上 → 功能静默失效 → 现场看不出原因。
 *
 * 本测试静态比对两侧：抽出 Rust 返回体的键集合，抽出 JS 在对应事件块里
 * 读用的 d.X 字段，断言后者是前者的子集。任一漂移即失败。
 */
"use strict";

const test = require("node:test");
const assert = require("node:assert");
const fs = require("node:fs");
const path = require("node:path");

const LIB_RS = path.join(__dirname, "src-tauri/src/lib.rs");
const APP_JS = path.join(__dirname, "src/app.js");

/** 从 lib.rs 的 deliver_brief 返回体里抽出顶层键名。
 *  定位方式：找到 "speech": speech 那一行所属的 json! 块，按行扫描
 *  `"key":` 形式，跳过嵌套对象内部的键（按大括号深度只在顶层取）。 */
function rustPayloadKeys() {
  const src = fs.readFileSync(LIB_RS, "utf8");
  const start = src.indexOf("fn deliver_brief(");
  assert.ok(start >= 0, "lib.rs 里找不到 fn deliver_brief");
  const jstart = src.indexOf("serde_json::json!({", start);
  assert.ok(jstart >= 0, "deliver_brief 里找不到 json! 返回体");
  let i = src.indexOf("{", jstart);
  let depth = 0;
  let body = "";
  for (; i < src.length; i++) {
    const c = src[i];
    if (c === "{") depth++;
    else if (c === "}") {
      depth--;
      if (depth === 0) break;
    }
    if (depth >= 1) body += c;
  }
  // body 起于 json!({ 的那个 { 本身，故顶层键出现在 ld === 1 的位置
  const keys = [];
  const lines = body.split("\n");
  let ld = 0;
  for (const line of lines) {
    const m = line.trim().match(/^"([a-z_]+)"\s*:/);
    if (ld === 1 && m) keys.push(m[1]);
    for (const c of line) {
      if (c === "{") ld++;
      else if (c === "}") ld--;
    }
  }
  assert.ok(keys.length > 0, "没能从 deliver_brief 返回体里抽到任何键");
  return keys;
}

/** 抽出 app.js 里某个 listen("xxx") 回调块内读用的 d.X 字段。 */
function jsReads(eventName) {
  const src = fs.readFileSync(APP_JS, "utf8");
  const i = src.indexOf(`listen("${eventName}"`);
  assert.ok(i >= 0, `app.js 里找不到 listen("${eventName}")`);
  // 取到下一个 listen( 之前
  const j = src.indexOf('listen("', i + 10);
  const blk = src.slice(i, j > 0 ? j : src.length);
  // 先剥掉行注释：说明文字里提到「此前写的是 d.date」这类内容不算真实读取，
  // 否则测试会被自己的注释误伤（曾真的发生过一次）。
  const code = blk
    .split("\n")
    .map((line) => line.replace(/\/\/.*$/, ""))
    .join("\n");
  const out = new Set();
  for (const m of code.matchAll(/d\.([a-z_]+)/g)) out.add(m[1]);
  // 去掉明显不是 payload 字段的（链式方法等）
  for (const x of ["get", "slice", "length", "map", "filter", "join"]) out.delete(x);
  return [...out].sort();
}

test("brief-delivered：JS 读的每个字段，Rust 返回体里都必须存在", () => {
  const rustKeys = rustPayloadKeys();
  const reads = jsReads("brief-delivered");
  const missing = reads.filter((k) => !rustKeys.includes(k));
  assert.deepEqual(
    missing,
    [],
    `字段漂移：JS 读 ${JSON.stringify(missing)}，但 Rust 返回体只有 ${JSON.stringify(rustKeys)}` +
      `——这些字段会恒为 undefined，功能静默失效`
  );
});

test("Rust 返回体必须含播报修复依赖的关键字段", () => {
  const rustKeys = rustPayloadKeys();
  // 这五个字段是 P1 播报缺陷修复与四条兜底的输入，少一个就有一项兜底失效
  for (const k of [
    "speech",
    "voice_enabled",
    "notification_suppressed",
    "late_minutes",
    "counts",
  ]) {
    assert.ok(rustKeys.includes(k), `deliver_brief 必须返回 ${k}（当前键：${rustKeys.join(", ")}）`);
  }
});

test("counts 子对象不得再含 weekly（周聚合已整体下线，S3 场景随之移除）", () => {
  const src = fs.readFileSync(LIB_RS, "utf8");
  const i = src.indexOf("fn deliver_brief(");
  const blk = src.slice(i, i + 4000);
  assert.ok(
    !blk.includes('"weekly"'),
    "counts 里不得再有 weekly 键——周聚合已于 2026-11-03 拍板整体下线（用户选「两边都去掉」），" +
      "保留该键只会让前端误以为还存在 S3 周聚合场景"
  );
});
