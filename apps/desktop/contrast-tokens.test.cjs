// 对比度守卫（v2.3.0）：把「17 项全消」从一次性复测变成可持续的回归门。
// 三条用例守三件事：
//   1) 每个 token 在它真实落到的 8 个底色上是否达到各自判据（文字 4.5 / 图形 3.0）；
//   2) 这套 token 在 tokens.css 里被复制成四份，四份必须逐项同值（最容易漏改的地方）；
//   3) 拆 token 后最可能出的错——角色串用：把文字色拿去当实心底、把实底色拿去当文字。
//
// 判据来源：UI 规范第二章验收原文「全部文字对其实际底色（含渐变取最暗档）≥4.5:1，
// 大字与图形 ≥3:1」。seam 计入图形（用户 2026-10-31 拍板的读法 A）。
const test = require('node:test');
const assert = require('node:assert');
const fs = require('node:fs');
const path = require('node:path');

const CSS_PATH = path.join(__dirname, 'src', 'tokens.css');

// ── WCAG 2.1 相对亮度 / 对比度 ────────────────────────────────────────
function lin(c) {
  const v = c / 255;
  return v <= 0.04045 ? v / 12.92 : Math.pow((v + 0.055) / 1.055, 2.4);
}
function lum(hex) {
  let h = hex.trim().replace('#', '');
  if (h.length === 3) h = h.split('').map((c) => c + c).join('');
  return 0.2126 * lin(parseInt(h.slice(0, 2), 16))
    + 0.7152 * lin(parseInt(h.slice(2, 4), 16))
    + 0.0722 * lin(parseInt(h.slice(4, 6), 16));
}
function ratio(a, b) {
  const la = lum(a);
  const lb = lum(b);
  const hi = Math.max(la, lb);
  const lo = Math.min(la, lb);
  return (hi + 0.05) / (lo + 0.05);
}

// ── 解析 tokens.css 的四份副本 ────────────────────────────────────────
const SRC = fs.readFileSync(CSS_PATH, 'utf8');

function bodyAfter(idx) {
  const open = SRC.indexOf('{', idx);
  let depth = 0;
  for (let i = open; i < SRC.length; i += 1) {
    if (SRC[i] === '{') depth += 1;
    else if (SRC[i] === '}') {
      depth -= 1;
      if (depth === 0) return SRC.slice(open + 1, i);
    }
  }
  throw new Error('花括号未配平');
}
function scopeBody(re) {
  const out = [];
  const g = new RegExp(re.source, re.flags.includes('g') ? re.flags : re.flags + 'g');
  let m;
  while ((m = g.exec(SRC)) !== null) out.push(bodyAfter(m.index));
  return out.join('\n');
}
function decls(body) {
  const d = {};
  const g = /--([a-z0-9-]+)\s*:\s*([^;{}]+);/g;
  let m;
  while ((m = g.exec(body)) !== null) d[m[1].trim()] = m[2].trim();
  return d;
}

const BASE = decls(scopeBody(/:root\s*\{/));
const DARK_MEDIA = decls(scopeBody(/:root:not\(\[data-theme="light"\]\)\s*\{/));
const LOCK_LIGHT = decls(scopeBody(/:root\[data-theme="light"\]\s*\{/));
const LOCK_DARK = decls(scopeBody(/:root\[data-theme="dark"\]\s*\{/));

// 四个真实生效环境，不是两个：
//   sys-light  = 系统浅色且从未点过主题开关（默认安装路径，只有 base 块生效）
//   sys-dark   = 系统深色且从未点过开关（base + 媒体查询块）
//   lock-light = 手动选浅色（base + [data-theme=light]）
//   lock-dark  = 手动选深色（base + 媒体查询块 + [data-theme=dark]）
const THEMES = {
  'sys-light': { ...BASE },
  'sys-dark': { ...BASE, ...DARK_MEDIA },
  'lock-light': { ...BASE, ...LOCK_LIGHT },
  'lock-dark': { ...BASE, ...DARK_MEDIA, ...LOCK_DARK },
};

function resolveHex(v, env, depth = 0) {
  if (v == null || depth > 6) return null;
  const m = /^var\(\s*--([a-z0-9-]+)\s*(?:,[^)]*)?\)$/.exec(v.trim());
  if (m) return resolveHex(env[m[1]], env, depth + 1);
  const h = v.trim();
  return /^#[0-9a-fA-F]{6}$/.test(h) ? h : null;
}

// 8 个真实底色：4 纯色面 + 装甲渐变 4 档（card 背景就是这条渐变，最暗档也算）
function surfaces(env) {
  const out = {};
  const map = { canvas: 'mech-canvas', panel: 'mech-panel', 'cabin-bg': 'cabin-bg', inset: 'mech-inset' };
  for (const [name, key] of Object.entries(map)) {
    const h = resolveHex(env[key], env);
    if (h) out[name] = h;
  }
  const stops = (env['armor-plate-grad'] || '').match(/#[0-9a-fA-F]{6}/g) || [];
  ['grad-0', 'grad-35', 'grad-75', 'grad-100'].forEach((n, i) => {
    if (stops[i]) out[n] = stops[i];
  });
  return out;
}

// 被测角色与判据
const GRAPHIC = ['mech-seam', 'mech-seam-hover', 'mech-ctl', 'mech-brand', 'icon-fill'];
const TEXT = ['mech-brand-text', 'mech-text-1', 'mech-text-2'];
const SOLID = ['mech-brand-solid', 'mech-brand-hover', 'mech-brand-pressed'];

// ── 用例 1：逐格达标 ─────────────────────────────────────────────────
test('每个 token 在 8 个真实底色上都达标（图形 3:1 / 文字 4.5:1）', () => {
  const fails = [];
  for (const theme of Object.keys(THEMES)) {
    const env = THEMES[theme];
    const sur = surfaces(env);
    assert.ok(Object.keys(sur).length === 8, `${theme} 底色数应为 8，实为 ${Object.keys(sur).length}`);

    for (const tok of GRAPHIC) {
      const fg = resolveHex(env[tok], env);
      assert.ok(fg, `${theme} 缺少 --${tok}`);
      for (const [name, bg] of Object.entries(sur)) {
        const r = ratio(fg, bg);
        if (r < 3.0) fails.push(`${theme} --${tok} ${fg} 落在 ${name} ${bg} = ${r.toFixed(4)} < 3.0`);
      }
    }
    for (const tok of TEXT) {
      const fg = resolveHex(env[tok], env);
      assert.ok(fg, `${theme} 缺少 --${tok}`);
      for (const [name, bg] of Object.entries(sur)) {
        const r = ratio(fg, bg);
        if (r < 4.5) fails.push(`${theme} --${tok} ${fg} 落在 ${name} ${bg} = ${r.toFixed(4)} < 4.5`);
      }
    }
    for (const tok of SOLID) {
      const bg = resolveHex(env[tok], env);
      assert.ok(bg, `${theme} 缺少 --${tok}`);
      const r = ratio('#FFFFFF', bg);
      if (r < 4.5) fails.push(`${theme} --${tok} ${bg} 配白字 = ${r.toFixed(4)} < 4.5`);
    }
  }
  assert.deepStrictEqual(fails, [], `对比度未达标：\n${fails.join('\n')}`);
});

// ── 用例 2：四份副本一致性 ───────────────────────────────────────────
test('四份主题副本的结构/控件/brand 类 token 逐项同值', () => {
  const keys = [
    'mech-seam', 'mech-seam-hover', 'mech-ctl',
    'mech-brand', 'mech-brand-solid', 'mech-brand-text',
    'mech-brand-hover', 'mech-brand-pressed', 'icon-fill',
  ];
  const fails = [];
  for (const k of keys) {
    const lightPair = [resolveHex(BASE[k], BASE), resolveHex(LOCK_LIGHT[k], LOCK_LIGHT)];
    const darkPair = [resolveHex(DARK_MEDIA[k], DARK_MEDIA), resolveHex(LOCK_DARK[k], LOCK_DARK)];
    if (lightPair[0] !== lightPair[1]) {
      fails.push(`--${k} 亮色两处不一致：:root=${lightPair[0]} [data-theme=light]=${lightPair[1]}（只改了一处）`);
    }
    if (darkPair[0] !== darkPair[1]) {
      fails.push(`--${k} 暗色两处不一致：media=${darkPair[0]} [data-theme=dark]=${darkPair[1]}（只改了一处）`);
    }
    if (lightPair[0] == null || darkPair[0] == null) {
      fails.push(`--${k} 在某个主题块里缺失`);
    }
  }
  assert.deepStrictEqual(fails, [], `主题副本不一致：\n${fails.join('\n')}`);
});

// ── 用例 3：角色不串用 ──────────────────────────────────────────────
test('拆 token 后无角色串用：文字色不作实心底、实底色不作文字', () => {
  const body = SRC;
  const bad = [];

  // 文字色出现在 background / background-color 上 = 串用
  for (const m of body.matchAll(/background(?:-color)?\s*:\s*[^;{}]*var\(--mech-brand-text\)/g)) {
    bad.push(`--mech-brand-text 被用作背景：${m[0].trim()}`);
  }
  // 实底色出现在 color（文字色）上 = 串用
  for (const m of body.matchAll(/\bcolor\s*:\s*var\(--mech-brand-solid\)/g)) {
    bad.push(`--mech-brand-solid 被用作文字色：${m[0].trim()}`);
  }
  // 实心底那段必须同处配白字：#FFFFFF 与 var(--mech-brand-solid) 同一规则内成对出现
  const solidUses = [...body.matchAll(/background\s*:\s*var\(--mech-brand-solid\)/g)];
  assert.ok(solidUses.length >= 1, '没有找到任何 --mech-brand-solid 的实底用法，改动未落地');
  for (const m of solidUses) {
    const tail = body.slice(m.index, m.index + 160);
    if (!/#FFFFFF/i.test(tail)) bad.push(`实底色未配白字：${tail.split('\n')[0].trim()}`);
  }
  // 灯带是图形，必须留在 --mech-brand（不能被误改成实底色）
  if (!/background:\s*var\(--panel-strip,\s*var\(--mech-brand\)\)/.test(body)) {
    bad.push('--panel-strip 灯带没有留在 --mech-brand（它属图形，视觉应不变）');
  }
  assert.deepStrictEqual(bad, [], `角色串用：\n${bad.join('\n')}`);
});
