/**
 * B3 门禁一致性守卫（2026-11-25 · 质检单 7711670075803127088 B3/H3）
 *
 * B3 的收口方案是「预发 tag 跑 test-core 作为 PR 级 CI 的等效门禁」。这套方案
 * 有个天然弱点：**「等效」是两处 YAML 的约定，不是代码里的同一份实现**。
 * 预发门禁哪天被改宽（少跑一步、去掉 --no-fail-fast、改成 point-named 测试文件），
 * 或者正式链哪天被改窄，两边就会悄悄脱钩——门禁还在跑，但已经不是同一个门禁了。
 *
 * 本守卫把「等效」钉成可执行断言，任何一侧脱钩都会红：
 *  1. 正式 tag 仍走完整发布链（'v*' 保留），预发 tag 被排除出发布链（'!v*-rc*'）
 *  2. 预发门禁只响应 rc tag，且**只读**（结构上无发布能力）
 *  3. 三条命令两边逐字相同（这正是「等效」的全部内容）
 *  4. 正式链的两个 job 与发布步条件仍在（防止「为了让守卫绿而删掉发布步」）
 *
 * 反向自检（写这个文件时实跑过，不是推测）：
 *  · 去掉 windows-release.yml 的 '!v*-rc*' → 第 1 组用例红
 *  · 把预发门禁 permissions 改成 contents: write → 第 2 组用例红
 *  · 把预发门禁少写一步 npm --prefix server test → 第 3 组用例红
 */
const test = require("node:test");
const assert = require("node:assert");
const fs = require("node:fs");
const path = require("node:path");

const WF_DIR = path.join(__dirname, "..", "..", ".github", "workflows");
const release = fs.readFileSync(path.join(WF_DIR, "windows-release.yml"), "utf8");
const gate = fs.readFileSync(path.join(WF_DIR, "pre-release-gate.yml"), "utf8");

/** 取某个 workflow 的 on.push.tags 行（原文，供逐字符断言）。 */
function tagsLine(src, name) {
  const m = src.match(/^\s{4}tags:.*$/m);
  assert.ok(m, `${name} 必须有 on.push.tags 行`);
  return m[0].trim();
}

/** 去掉注释行后的有效指令文本：注释里写了一句命令不算「真的跑了这句」。 */
function codeOnly(src) {
  return src
    .split("\n")
    .filter((l) => !l.trim().startsWith("#"))
    .join("\n");
}

// ── 1. 两个 workflow 的 tag 面互斥且正式链未被削弱 ─────────────────────
test("B3·正式 tag 仍触发发布链，预发 tag 被排除（不然预发门禁会误烧一次发布）", () => {
  const line = tagsLine(release, "windows-release.yml");
  assert.ok(line.includes("'v*'"), "正式 tag 面 'v*' 不得删除，否则 v3.4.0 不再出包");
  assert.ok(
    line.includes('"!v*-rc*"') || line.includes("'!v*-rc*'"),
    `windows-release.yml 的 tags 必须排除预发 tag，实际：${line}`
  );
});

test("B3·预发门禁只响应 rc tag（正式 tag 不得被它抢跑）", () => {
  const line = tagsLine(gate, "pre-release-gate.yml");
  assert.ok(line.includes("'v*-rc*'") || line.includes('"v*-rc*"'),
    `pre-release-gate.yml 必须在 'v*-rc*' 上触发，实际：${line}`);
  // 预发门禁不得同时挂正式 tag 面：否则一个正式 tag 会同时跑两条流水线
  assert.ok(!line.includes("'v*'"), "预发门禁不得匹配裸 'v*'（会与发布链重复触发）");
});

// ── 2. 预发门禁是只读的（结构性无发布能力，不靠口头承诺） ─────────────
test("B3·预发门禁只读：不申请 contents: write，结构上无法建 Release", () => {
  const code = codeOnly(gate);
  assert.ok(/permissions:\s*\n\s*contents:\s*read/.test(code),
    "pre-release-gate.yml 必须声明 permissions.contents: read");
  assert.ok(!/contents:\s*write/.test(code),
    "pre-release-gate.yml 不得申请 contents: write（那会把门禁变成第二个发布口）");
  // 也不得调用发布类动作
  for (const forbidden of ["gh release create", "gh release upload", "softprops/action-gh-release"]) {
    assert.ok(!code.includes(forbidden), `预发门禁不得包含发布动作：${forbidden}`);
  }
});

// ── 3. 「等效」的全部内容：三条命令两边逐字相同 ──────────────────────────
const COMMANDS = [
  { label: "Rust 单测（H3 编译证据载体）", cmd: "cargo test -p wb-core -p wbd --locked --no-fail-fast" },
  { label: "前端守卫（目录通配）", cmd: "node --test --test-reporter=tap apps/desktop/*.test.cjs" },
  { label: "服务端 e2e", cmd: "npm --prefix server test" },
];

test("B3·等效门禁：三条命令在预发门禁与正式链中逐字相同", () => {
  const gateCode = codeOnly(gate);
  const releaseCode = codeOnly(release);
  for (const { label, cmd } of COMMANDS) {
    assert.ok(gateCode.includes(cmd), `预发门禁缺这一步或命令被改写：${label} → ${cmd}`);
    assert.ok(releaseCode.includes(cmd), `正式链这一步被改写（等效关系破裂）：${label} → ${cmd}`);
  }
  // 两个 job 都必须还在：拆 job 是为了让 cargo 日志不被前端赤侧淹没，
  // 但删掉任一 job 就等于门禁被削窄（这一步守卫必须拦住）。
  assert.ok(/^\s{2}gate-rust:/m.test(gateCode), "gate-rust job 必须仍在（H3/H4 的证据载体）");
  assert.ok(/^\s{2}gate-guards:/m.test(gateCode), "gate-guards job 必须仍在（test-core 其余两步）");
});

test("B3·--no-fail-fast 不得被摘掉（摘了以后多包只出一条 test result，红侧被削）", () => {
  // 这条单独钉，是因为它是「日志看起来全绿其实后一个包没跑」的唯一防线。
  const cmd = "cargo test -p wb-core -p wbd --locked --no-fail-fast";
  assert.ok(codeOnly(gate).includes(cmd), "预发门禁的 Rust 步必须带 --no-fail-fast");
  assert.ok(codeOnly(release).includes(cmd), "正式链的 Rust 步必须带 --no-fail-fast");
});

// ── 4. 正式链既有 job / 发布步条件未被我改动（只许加条件分支，不许删） ────
test("B3·正式链结构未削弱：test-core / build-windows 两 job 与 tag 发布步条件仍在", () => {
  const code = codeOnly(release);
  assert.ok(/^\s{2}test-core:/m.test(code), "test-core job 必须仍在");
  assert.ok(/^\s{2}build-windows:/m.test(code), "build-windows job 必须仍在");
  assert.ok(/needs:\s*test-core/.test(code), "build-windows 仍必须 needs test-core（门禁前置关系不变）");
  // 发布步的 tag 条件：手动触发时仍不得发布（这条是 v2.1 以来就有的保护）
  assert.ok(
    code.includes("if: startsWith(github.ref, 'refs/tags/')"),
    "tag 发布步的 if 条件必须仍在（否则手动触发会误建 Release）"
  );
  // 两条发布类步骤都要在（建 Release + 发布后核验），少一条就等于发布链被削弱
  const tagGuards = code.split("if: startsWith(github.ref, 'refs/tags/')").length - 1;
  assert.ok(tagGuards >= 2, `tag 条件应至少出现在 2 个发布类步骤上，实际 ${tagGuards} 处`);
});

test("B3·预发门禁把日志上传出来（H3/H4 的留痕载体必须能取回原文）", () => {
  const code = codeOnly(gate);
  assert.ok(code.includes("actions/upload-artifact@"),
    "预发门禁必须上传测试日志 artifact，否则「留痕」只剩会过期的网页日志");
  // 红侧也要传：只在成功时上传，等于把最该取证的那次跑丢了
  assert.ok(/if:\s*always\(\)/.test(code),
    "上传日志那一步必须 if: always()，否则门禁判红时恰恰没有日志");
  // 至少把 cargo 日志纳入上传路径，H3 的编译证据就在这份文件里
  assert.ok(code.includes("cargo-test.log"), "cargo-test.log 必须在上传路径里（H3 的编译证据）");
});
