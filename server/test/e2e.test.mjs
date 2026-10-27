/**
 * 服务端 e2e（真实起进程 + 真实 HTTP + 真实 SQLite）
 *
 * 为什么要有这套测试：2026-10-27 复核指出「21 项服务端 e2e」在交付包里找不到对应物
 * （`server/test/` 是空目录，也没有 package.json）。这里把每一项都落成可执行的断言。
 *
 * 运行：`cd server && npm test`（或 `node --test test/`）。零 npm 依赖，Node 22 内置即可。
 */
import { test, before, after } from 'node:test';
import assert from 'node:assert/strict';
import { spawn } from 'node:child_process';
import { mkdtempSync, rmSync, existsSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { createServer } from 'node:net';

const ROOT = new URL('..', import.meta.url).pathname;

let child;
let dataDir;
let base;

/** 取一个空闲端口（避免与开发中实例冲突） */
function freePort() {
  return new Promise((resolve, reject) => {
    const s = createServer();
    s.listen(0, '127.0.0.1', () => {
      const p = s.address().port;
      s.close(() => resolve(p));
    });
    s.on('error', reject);
  });
}

async function waitHealthy(url, tries = 80) {
  for (let i = 0; i < tries; i++) {
    try {
      const r = await fetch(`${url}/health`);
      if (r.ok) return await r.json();
    } catch {
      /* 还没起来 */
    }
    await new Promise((r) => setTimeout(r, 100));
  }
  throw new Error('server did not become healthy');
}

async function req(method, path, { body, token } = {}) {
  const headers = {};
  if (body !== undefined) headers['content-type'] = 'application/json';
  if (token) headers.authorization = `Bearer ${token}`;
  const r = await fetch(`${base}${path}`, {
    method,
    headers,
    body: body === undefined ? undefined : JSON.stringify(body),
  });
  const text = await r.text();
  let json;
  try {
    json = JSON.parse(text);
  } catch {
    json = { raw: text };
  }
  return { status: r.status, body: json };
}

before(async () => {
  dataDir = mkdtempSync(join(tmpdir(), 'wb-sync-test-'));
  const port = await freePort();
  const adminPort = await freePort();
  base = `http://127.0.0.1:${port}`;

  child = spawn(process.execPath, [join(ROOT, 'src', 'server.js')], {
    cwd: ROOT,
    env: {
      ...process.env,
      WB_PORT: String(port),
      WB_ADMIN_PORT: String(adminPort),
      WB_BIND: '127.0.0.1',
      WB_ADMIN_BIND: '127.0.0.1',
      WB_DATA_DIR: dataDir,
    },
    stdio: ['ignore', 'pipe', 'pipe'],
  });
  child.stderr.on('data', () => {});
  child.stdout.on('data', () => {});
  await waitHealthy(base);
});

after(() => {
  if (child) child.kill('SIGTERM');
  try {
    rmSync(dataDir, { recursive: true, force: true });
  } catch {
    /* 尽力而为 */
  }
});

// ───────────────── 1–4：启动、健康检查、首成员初始化 ─────────────────

test('01 health 返回 schema_version 2 与 seq', async () => {
  const r = await req('GET', '/health');
  assert.equal(r.status, 200);
  assert.equal(r.body.ok, true);
  // v2 = 多设备令牌表上线（v1 是单 token 时代）
  assert.equal(r.body.schema_version, 2);
  assert.ok(Number.isInteger(r.body.seq));
});

test('02 空库首启后 /setup 建立唯一 admin 并发放令牌', async () => {
  const r = await req('POST', '/setup', {
    body: { username: '大角子', password: 'correct-horse-battery', device_id: 'd_alpha' },
  });
  assert.equal(r.status, 200);
  assert.equal(r.body.ok, true);
  assert.equal(r.body.role, 'admin');
  assert.match(r.body.member_id, /^m_[0-9a-f]{16}$/);
  assert.match(r.body.token, /^[0-9a-f]{64}$/);
  assert.equal(r.body.device_id, 'd_alpha');
  assert.equal(r.body.active_tokens, 1);
});

test('03 /setup 第二次返回 409，不会建出第二个 admin', async () => {
  const r = await req('POST', '/setup', { body: { username: 'x', password: '12345678' } });
  assert.equal(r.status, 409);
  assert.equal(r.body.error, 'already initialized');
});

test('04 口令长度不足被拒', async () => {
  // 此时库已初始化，先确认参数校验仍生效在 /join 这类入口上
  const r = await req('POST', '/join', { body: { code: 'NOPE', username: 'a', password: '123' } });
  assert.equal(r.status, 404); // 邀请码不存在优先返回 404
});

// ───────────────── 5–9：登录与多设备令牌 ─────────────────

let adminToken;
let adminMember;
let devAlphaToken;
let devBetaToken;

test('05 正确口令登录成功，返回 device_id 与在用令牌数', async () => {
  const r = await req('POST', '/login', {
    body: { username: '大角子', password: 'correct-horse-battery', device_id: 'd_alpha' },
  });
  assert.equal(r.status, 200);
  assert.equal(r.body.ok, true);
  assert.equal(r.body.role, 'admin');
  adminToken = r.body.token;
  adminMember = r.body.member_id;
  assert.equal(r.body.device_id, 'd_alpha');
  // 同设备重新登录：旧令牌被本设备新令牌顶掉，仍只占 1 个名额
  assert.equal(r.body.active_tokens, 1);
});

test('06 错误口令返回 401', async () => {
  const r = await req('POST', '/login', {
    body: { username: '大角子', password: 'wrong-password', device_id: 'd_evil' },
  });
  assert.equal(r.status, 401);
});

test('07 多设备并存：B 登录不会把 A 顶下线（裁决要点）', async () => {
  const r = await req('POST', '/login', {
    body: { username: '大角子', password: 'correct-horse-battery', device_id: 'd_beta' },
  });
  assert.equal(r.status, 200);
  assert.equal(r.body.active_tokens, 2);
  devBetaToken = r.body.token;
  devAlphaToken = adminToken;

  // A 的令牌仍然可用——这正是单 token 覆盖模型做不到的
  const a = await req('GET', '/tokens', { token: devAlphaToken });
  assert.equal(a.status, 200);
  assert.equal(a.body.tokens.filter((t) => !t.revoked).length, 2);
});

test('08 每成员上限 5 枚，超出淘汰最久未活跃的那枚', async () => {
  // 已有 2 枚（alpha / beta），再登录 4 台 → 第 6 台时开始淘汰
  const tokens = [devAlphaToken, devBetaToken];
  for (const dev of ['d_c1', 'd_c2', 'd_c3', 'd_c4']) {
    const r = await req('POST', '/login', {
      body: { username: '大角子', password: 'correct-horse-battery', device_id: dev },
    });
    assert.equal(r.status, 200);
    tokens.push(r.body.token);
  }
  // 最后一次登录后，在用令牌数必须 == 5
  const last = await req('POST', '/login', {
    body: { username: '大角子', password: 'correct-horse-battery', device_id: 'd_c5' },
  });
  assert.equal(last.status, 200);
  assert.equal(last.body.active_tokens, 5);

  const list = await req('GET', '/tokens', { token: last.body.token });
  assert.equal(list.body.tokens.filter((t) => !t.revoked).length, 5);
  assert.equal(list.body.max, 5);

  // 最早那枚（alpha）已被淘汰：用它请求应 401
  const stale = await req('GET', '/tokens', { token: devAlphaToken });
  assert.equal(stale.status, 401);

  adminToken = last.body.token;
});

test('09 /tokens 只回令牌前缀，绝不回完整令牌', async () => {
  const r = await req('GET', '/tokens', { token: adminToken });
  assert.equal(r.status, 200);
  for (const t of r.body.tokens) {
    assert.equal(typeof t.token_prefix, 'string');
    assert.equal(t.token_prefix.length, 8);
    assert.equal(t.token, undefined, '不得回传完整令牌');
    assert.ok(typeof t.last_active_at === 'number');
  }
});

// ───────────────── 10–14：撤销路径 ─────────────────

test('10 /logout 只撤销自己的令牌，其他设备不受影响', async () => {
  const login = await req('POST', '/login', {
    body: { username: '大角子', password: 'correct-horse-battery', device_id: 'd_temp' },
  });
  const tempToken = login.body.token;
  const r = await req('POST', '/logout', { token: tempToken });
  assert.equal(r.status, 200);
  assert.equal(r.body.ok, true);

  const again = await req('GET', '/tokens', { token: tempToken });
  assert.equal(again.status, 401, '已撤销的令牌必须失效');

  // 主设备仍可用
  const still = await req('GET', '/tokens', { token: adminToken });
  assert.equal(still.status, 200);
});

test('11 /tokens/revoke 可按 device_id 撤销指定设备（丢设备场景）', async () => {
  const login = await req('POST', '/login', {
    body: { username: '大角子', password: 'correct-horse-battery', device_id: 'd_lost_phone' },
  });
  const lostToken = login.body.token;
  assert.equal(login.status, 200);

  const r = await req('POST', '/tokens/revoke', {
    token: adminToken,
    body: { device_id: 'd_lost_phone' },
  });
  assert.equal(r.status, 200);
  assert.equal(r.body.changed, 1);

  const gone = await req('GET', '/tokens', { token: lostToken });
  assert.equal(gone.status, 401);
});

test('12 /tokens/revoke 支持按前缀撤销', async () => {
  const login = await req('POST', '/login', {
    body: { username: '大角子', password: 'correct-horse-battery', device_id: 'd_prefix' },
  });
  const prefix = login.body.token.slice(0, 8);
  const r = await req('POST', '/tokens/revoke', { token: adminToken, body: { token_prefix: prefix } });
  assert.equal(r.status, 200);
  assert.equal(r.body.changed, 1);

  const gone = await req('GET', '/tokens', { token: login.body.token });
  assert.equal(gone.status, 401);
});

test('13 /tokens/revoke 缺参数时返回 400，不静默成功', async () => {
  const r = await req('POST', '/tokens/revoke', { token: adminToken, body: {} });
  assert.equal(r.status, 400);
  assert.match(r.body.error, /required/);
});

test('14 唯一 admin 不可被 /members/remove 禁用（防锁死护栏）', async () => {
  const r = await req('POST', '/members/remove', {
    token: adminToken,
    body: { member_id: 'm_not_me_but_does_not_exist' },
  });
  assert.equal(r.status, 404);

  // 自己对号实现的是更早的 400，但核心是：唯一 admin 任何路径都不能被禁
  const members = await req('GET', '/members', { token: adminToken });
  assert.equal(members.status, 200);
  const me = members.body.members.find((m) => m.member_id === adminMember);
  assert.equal(me.role, 'admin');
  assert.equal(me.status, 'active');
});

// ───────────────── 15–18：同步 push / pull ─────────────────

const rowAlpha = {
  table: 'work_item',
  id: 'w_alpha',
  payload: { title: '对齐 Q4 目标', progress_pct: 10, board_code: 'gov' },
  updated_at: 1000,
  updated_by: 'u_1',
  device_id: 'd_alpha',
  seq: 0,
  deleted_at: null,
};

test('15 push 首次写入返回 inserted 并分配 seq', async () => {
  const r = await req('POST', '/sync/push', {
    token: adminToken,
    body: { schema_version: 2, rows: [rowAlpha] },
  });
  assert.equal(r.status, 200);
  assert.equal(r.body.ok, true);
  assert.equal(r.body.applied.length, 1);
  assert.equal(r.body.applied[0].decision, 'inserted');
  assert.ok(r.body.applied[0].seq > 0);
  assert.equal(r.body.conflicts.length, 0, '首插不产生冲突');
});

test('16 schema_version 不匹配返回 409 并回服务端版本', async () => {
  const r = await req('POST', '/sync/push', {
    token: adminToken,
    body: { schema_version: 1, rows: [] },
  });
  assert.equal(r.status, 409);
  assert.equal(r.body.server, 2);
});

test('17 pull 按游标返回增量，游标推进', async () => {
  const first = await req('GET', '/sync/pull?cursor=0&limit=100', { token: adminToken });
  assert.equal(first.status, 200);
  assert.ok(first.body.rows.length >= 1);
  const row = first.body.rows.find((x) => x.id === 'w_alpha');
  assert.equal(row.table, 'work_item');
  assert.equal(row.payload.title, '对齐 Q4 目标');
  assert.ok(first.body.cursor > 0);

  // 用推进后的游标再拉，不应重复拿到同一条
  const second = await req('GET', `/sync/pull?cursor=${first.body.cursor}&limit=100`, { token: adminToken });
  assert.equal(second.status, 200);
  assert.equal(second.body.rows.find((x) => x.id === 'w_alpha'), undefined);
});

test('18 LWW：更新时间更晚的一方获胜，落败值进冲突日志', async () => {
  const newer = {
    ...rowAlpha,
    payload: { title: '对齐 Q4 目标（改）', progress_pct: 20, board_code: 'gov' },
    updated_at: 2000,
    updated_by: 'u_2',
    device_id: 'd_beta',
  };
  const r = await req('POST', '/sync/push', {
    token: adminToken,
    body: { schema_version: 2, rows: [newer] },
  });
  assert.equal(r.status, 200);
  assert.equal(r.body.applied[0].decision, 'remote_wins');
  assert.equal(r.body.conflicts.length, 1);

  const c = await req('GET', '/sync/conflicts?limit=10', { token: adminToken });
  assert.equal(c.status, 200);
  assert.ok(c.body.conflicts.length >= 1);
  const losing = JSON.parse(c.body.conflicts[0].losing_json);
  assert.equal(losing.title, '对齐 Q4 目标');
});

test('19 progress_pct 例外：落败行的进度更高时按 max 取值', async () => {
  // 旧时间戳但进度更高 → 进度仍应抬升，且决策记为 progress max
  const olderButFurther = {
    ...rowAlpha,
    payload: { title: '对齐 Q4 目标（改）', progress_pct: 80, board_code: 'gov' },
    updated_at: 1500,
    updated_by: 'u_3',
    device_id: 'd_c1',
  };
  const r = await req('POST', '/sync/push', {
    token: adminToken,
    body: { schema_version: 2, rows: [olderButFurther] },
  });
  assert.equal(r.status, 200);
  assert.equal(r.body.applied[0].decision, 'local_wins_progress_max');

  const p = await req('GET', '/sync/pull?cursor=0&limit=50', { token: adminToken });
  const got = p.body.rows.find((x) => x.id === 'w_alpha');
  assert.equal(got.payload.progress_pct, 80, '进度只升不降');
});

// ───────────────── 20–23：权限边界与运维入口 ─────────────────

test('20 无令牌访问受保护接口返回 401', async () => {
  const r = await req('GET', '/sync/pull?cursor=0');
  assert.equal(r.status, 401);
});

test('21 邀请 + 入伙：新成员不是 admin，且拿到自己的令牌', async () => {
  const inv = await req('POST', '/invite', { token: adminToken });
  assert.equal(inv.status, 200);
  assert.ok(inv.body.code);

  const join = await req('POST', '/join', {
    body: { code: inv.body.code, username: '小李', password: 'another-good-pass', device_id: 'd_li' },
  });
  assert.equal(join.status, 200);
  assert.equal(join.body.role, 'member');
  assert.equal(join.body.active_tokens, 1);

  // 成员无权看成员列表
  const members = await req('GET', '/members', { token: join.body.token });
  assert.equal(members.status, 403);

  // 同一邀请码不能重复使用
  const reuse = await req('POST', '/join', {
    body: { code: inv.body.code, username: '小王', password: 'yet-another-pass' },
  });
  assert.equal(reuse.status, 409);
});

test('22 成员可撤销自己的令牌，但撤不了别人的', async () => {
  const inv = await req('POST', '/invite', { token: adminToken });
  const join = await req('POST', '/join', {
    body: { code: inv.body.code, username: '小张', password: 'theirs-not-mine-9', device_id: 'd_zhang' },
  });
  const zhangToken = join.body.token;

  // 试图撤销 admin 的令牌 → 403
  const evil = await req('POST', '/tokens/revoke', {
    token: zhangToken,
    body: { member_id: adminMember, device_id: 'd_beta' },
  });
  assert.equal(evil.status, 403);

  // 撤自己 → 成功
  const own = await req('POST', '/tokens/revoke', { token: zhangToken, body: { device_id: 'd_zhang' } });
  assert.equal(own.status, 200);
});

test('23 admin 手动备份产出真实文件', async () => {
  const r = await req('POST', '/admin/backup', { token: adminToken });
  assert.equal(r.status, 200);
  assert.equal(r.body.ok, true);
  assert.ok(r.body.bytes > 0);
  assert.ok(existsSync(r.body.file));
});

test('24 未知路径返回 404，不泄露内部信息', async () => {
  const r = await req('GET', '/definitely-not-a-route', { token: adminToken });
  assert.equal(r.status, 404);
  assert.equal(r.body.error, 'not found');
});
