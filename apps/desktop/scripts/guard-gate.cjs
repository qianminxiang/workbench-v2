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
//   ④ skipped 判据（2026-11-25 追加授权修订，归修复单①边界；与 W9 的
//      intel-canary.test.cjs SKIP_LIVE 门控联合对齐）：
//      默认（SKIP_LIVE 未设）：skipped === 0 —— 被跳过的守卫等于没有验证，
//      但它会让整轮仍显示 pass，是最典型的假绿来源；跳过必须可见且必须红。
//      仅当显式 SKIP_LIVE=1 时，且仅容忍唯一一种组合：跳过恰好 3 条、
//      且 TAP 里 3 条 skip 行全部是 W9 活站实测（境外 runner 物理不可达境内站，
//      活站断言由境内环境验证——已知取舍，非验证通过）。
//      其余任何情况（跳过数≠3 / 混入非活站跳过 / 未设 SKIP_LIVE 却有跳过）一律红，
//      不得放宽成无条件放行。
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

// 仅统计「TAP skip 行且测试名是 W9 活站实测」的条数——与 intel-canary.test.cjs
// 的三例名逐一对应。用它防洗绿：SKIP_LIVE=1 时若有人把别的测试也悄悄 skip 掉，
// 活站行数就对不上 skipped 总数，判红。
const liveSkipLines = text.match(/^ok\s+\d+\s+-.*活站实测.*#\s*SKIP/mg) || [];
const allowLiveSkip = process.env.SKIP_LIVE === '1';
// 唯一被容忍的跳过组合（追加授权裁决原文：只容忍这一种，不得放宽）。
const LIVE_SKIP_ALLOWED = 3;

const problems = [];
if (tests === null || fail === null || skipped === null) {
  problems.push('TAP 里没有 `# tests` / `# fail` / `# skipped` 汇总行（node 可能先崩了，不能当全绿）');
} else {
  if (tests <= 0) problems.push(`tests=${tests}（不接受空集通过）`);
  if (fail !== 0) problems.push(`fail=${fail}（存在失败用例）`);
  if (skipped !== 0) {
    const tolerated = allowLiveSkip && skipped === LIVE_SKIP_ALLOWED && liveSkipLines.length === LIVE_SKIP_ALLOWED;
    if (!tolerated) {
      if (allowLiveSkip) {
        problems.push(`skipped=${skipped}（SKIP_LIVE=1 只容忍恰 ${LIVE_SKIP_ALLOWED} 条活站实测跳过，实际活站 skip 行 ${liveSkipLines.length} 条）`);
      } else {
        problems.push(`skipped=${skipped}（被跳过的守卫等于没有验证，按失败处理）`);
      }
    }
  }
}

if (problems.length) {
  for (const p of problems) console.error(`✗ ${p}`);
  process.exit(1);
}
if (skipped === LIVE_SKIP_ALLOWED && allowLiveSkip) {
  console.log(`✓ 守卫通过：tests=${tests} fail=${fail} skipped=${skipped}（SKIP_LIVE=1：恰 ${LIVE_SKIP_ALLOWED} 条活站实测跳过，活站断言由境内环境验证）`);
} else {
  console.log(`✓ 守卫通过：tests=${tests} fail=${fail} skipped=${skipped}`);
}
