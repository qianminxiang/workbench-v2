// 对比度守卫（v3.0 集成版）：把商务风 2026-11-05 收口时的「29 项全过」
// 从一次性复测变成可持续的回归门。
//
// 与 v2.2.6 机械风版守卫的差异：商务风 tokens.css 是单主题、扁平 token 表，
// 不存在「四份复制需逐项同值」的结构；因此本版守卫改为——
//   1) 29 个配对的色值全部从 tokens.css **现场解析**（不是写死），
//      谁改了 token 色值导致对比度跌破门槛，这里立刻红；
//   2) 每个被引用的 token 必须真实存在（防误删/误改名）；
//   3) 机械风变量（--mech-*）不得回归。
// 判据来源不变：UI 规范第二章验收原文「全部文字对其实际底色 ≥4.5:1，
// 大字与图形 ≥3:1」。seam 计入图形（用户 2026-10-31 拍板的读法 A）。
const test = require('node:test');
const assert = require('node:assert');
const fs = require('node:fs');
const path = require('node:path');

const CSS_PATH = path.join(__dirname, 'src', 'tokens.css');
const css = fs.readFileSync(CSS_PATH, 'utf-8');

// ── 从 tokens.css 解析 --name: #hex ────────────────────────────────
const TOKENS = {};
for (const m of css.matchAll(/(--[a-z0-9-]+)\s*:\s*(#[0-9A-Fa-f]{6})/g)) {
  TOKENS[m[1]] = m[2].toUpperCase();
}
function resolve(x) {
  if (x.startsWith('--')) {
    assert.ok(TOKENS[x], `token ${x} 在 tokens.css 中不存在（被误删或误改名？）`);
    return TOKENS[x];
  }
  return x.toUpperCase();
}

// ── WCAG 2.1 相对亮度 / 对比度 ──────────────────────────────────────
function lin(c) {
  const v = c / 255;
  return v <= 0.04045 ? v / 12.92 : Math.pow((v + 0.055) / 1.055, 2.4);
}
function lum(hex) {
  hex = hex.slice(1);
  const r = parseInt(hex.slice(0, 2), 16), g = parseInt(hex.slice(2, 4), 16), b = parseInt(hex.slice(4, 6), 16);
  return 0.2126 * lin(r) + 0.7152 * lin(g) + 0.0722 * lin(b);
}
function ratio(fg, bg) {
  const lf = lum(fg), lb = lum(bg);
  const hi = Math.max(lf, lb), lo = Math.min(lf, lb);
  return (hi + 0.05) / (lo + 0.05);
}

// ── 29 个配对（沿用商务风收口验收的配对表，改为 token 引用）──────
// [说明, 前景(token或字面色), 背景(token或字面色), 门槛]
const PAIRS = [
  // 正文 / 次级 / 弱化文字
  ['正文 text-1 / 卡片',        '--text-1',    '--bg-card',   4.5],
  ['正文 text-1 / 画布',        '--text-1',    '--bg-canvas', 4.5],
  ['次级 text-2 / 卡片',        '--text-2',    '--bg-card',   4.5],
  ['弱化 text-3 / 卡片',        '--text-3',    '--bg-card',   4.5],
  ['弱化 text-3 / 画布',        '--text-3',    '--bg-canvas', 4.5],
  ['弱化 text-3 / 内衬输入底',  '--text-3',    '--bg-inset',  4.5],
  // 品牌文字 / 按钮
  ['brand-text 链接文案 / 卡片', '--brand-text', '--bg-card',    4.5],
  ['brand-text 链接文案 / 画布', '--brand-text', '--bg-canvas',  4.5],
  ['brand-text / 内衬选中底',    '--brand-text', '--brand-liner', 4.5],
  ['白字 / 主实心按钮',          '#FFFFFF',      '--brand-solid',  4.5],
  ['白字 / 悬停按钮',            '#FFFFFF',      '--brand-hover',  4.5],
  ['白字 / 按下按钮',            '#FFFFFF',      '--brand-pressed', 4.5],
  // 状态文字（浅底深字徽标 + 白底）
  ['amber-fg / amber-bg 临期徽标', '--amber-fg', '--amber-bg', 4.5],
  ['red-fg / red-bg 逾期徽标',     '--red-fg',   '--red-bg',   4.5],
  ['green-fg / green-bg 相关标记', '--green-fg', '--green-bg', 4.5],
  ['ok-text 成功文字 / 卡片',      '--ok-text',  '--bg-card',  4.5],
  ['red-fg 逾期提示文字 / 卡片',   '--red-fg',   '--bg-card',  4.5],
  // 图形（色条 / 灯 / 焦点环 / 板块）≥3.0
  ['brand 图形 / 卡片',     '--brand',      '--bg-card',   3.0],
  ['brand-deep 图形 / 卡片', '--brand-deep', '--bg-card',   3.0],
  ['brand 图形 / 画布',     '--brand',      '--bg-canvas', 3.0],
  ['ok 成功灯 / 卡片',      '--ok',         '--bg-card',   3.0],
  ['warn-amber 临期灯 / 卡片', '--warn-amber', '--bg-card', 3.0],
  ['warn-red 逾期灯 / 卡片',  '--warn-red',  '--bg-card',   3.0],
  ['板块条 b1 政策 / 卡片', '--board-b1', '--bg-card', 3.0],
  ['板块条 b2 项目 / 卡片', '--board-b2', '--bg-card', 3.0],
  ['板块条 b3 数据 / 卡片', '--board-b3', '--bg-card', 3.0],
  ['板块条 b4 渠道 / 卡片', '--board-b4', '--bg-card', 3.0],
  ['板块条 b5 客户 / 卡片', '--board-b5', '--bg-card', 3.0],
  ['板块条 b6 跟进 / 卡片', '--board-b6', '--bg-card', 3.0],
];

test('商务风 29 项配对：色值现场解析，对比度全部达标', () => {
  assert.strictEqual(PAIRS.length, 29, '配对表必须是 29 项（与商务风收口验收同数）');
  const fails = [];
  for (const [label, fg, bg, min] of PAIRS) {
    const r = ratio(resolve(fg), resolve(bg));
    if (r < min) fails.push(`${label}: ${r.toFixed(2)} < ${min}`);
  }
  assert.deepStrictEqual(fails, [], `对比度跌破门槛：\n${fails.join('\n')}`);
});

test('机械风变量不得回归（商务风已整体移除 --mech-*）', () => {
  const hits = [...css.matchAll(/--mech-[a-z0-9-]+/g)].map((m) => m[0]);
  assert.deepStrictEqual(hits, [], `tokens.css 出现机械风残留：${[...new Set(hits)].join(', ')}`);
});
