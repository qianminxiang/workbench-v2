#!/usr/bin/env node
// 守卫 TAP 汇总闸门（v3.4.0 质检 B1）
//
// 为什么要有这个文件：CI 上「跳过数 ≠ 0」和「测试失败」都必须让流水线红，
// 但把这段解析分别写在 bash 与 PowerShell 里，会有两份实现、两套转义规则，
// 而且沙箱里没有 pwsh，PowerShell 那份根本没法本地验证。所以统一到一个
// node 脚本：两个工作流都生成 TAP 文件后调用本脚本判胜负，判据只有一份。
//
// 判据（逐条都要求成立，缺一条即失败）：
//   ① 文件里能看到 `# tests` 汇总行 —— 没有汇总说明 node 自己先崩了，
//      此时「fail 0 / skipped 0」都不成立为真（空文件最容易被读成全绿）。
//   ② tests > 0 —— 不接受空集通过。
//   ③ fail === 0。
//   ④ skipped === 0 —— 被跳过的守卫等于没有验证，但它会让整轮仍显示 pass，
//      是最典型的假绿来源；跳过必须可见且必须红。
//
// 用法：node apps/desktop/scripts/guard-gate.cjs <tap-file>
// 退出码：0 = 通过；1 = 任一判据不成立（原因打到 stderr）。

const fs = require('node:fs');

const file = process.argv[2];
if (!file) {
  console.error('用法: node apps/desktop/scripts/guard-gate.cjs <tap-file>');
  process.exit(1);
}

let text;
try {
  text = fs.readFileSync(file, 'utf8');
} catch (e) {
  console.error(`读不到 TAP 文件 ${file}：${e.message}`);
  process.exit(1);
}

/** 取 `# <key> <n>` 的计数（同键只取第一次出现，node 输出里也没重复）。 */
function count(key) {
  const m = text.match(new RegExp(`^#\\s*${key}\\s+(\\d+)\\s*$`, 'm'));
  return m ? Number(m[1]) : null;
}

const summaryLines = text.split('\n').filter((l) => /^#\s*(tests|pass|fail|skipped|cancelled|todo)\b/.test(l));
console.log('── 守卫汇总 ──');
for (const l of summaryLines) console.log(l);

const tests = count('tests');
const fail = count('fail');
const skipped = count('skipped');

const problems = [];
if (tests === null || fail === null || skipped === null) {
  problems.push('TAP 里没有 `# tests` / `# fail` / `# skipped` 汇总行（node 可能先崩了，不能当全绿）');
} else {
  if (tests <= 0) problems.push(`tests=${tests}（不接受空集通过）`);
  if (fail !== 0) problems.push(`fail=${fail}（存在失败用例）`);
  if (skipped !== 0) problems.push(`skipped=${skipped}（被跳过的守卫等于没有验证，按失败处理）`);
}

if (problems.length) {
  for (const p of problems) console.error(`✗ ${p}`);
  process.exit(1);
}
console.log(`✓ 守卫通过：tests=${tests} fail=${fail} skipped=${skipped}`);
