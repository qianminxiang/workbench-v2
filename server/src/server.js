'use strict';
/**
 * 个人工作台 v2.0 · 团队同步服务端（薄服务）
 *
 * 职责只有三件（架构 §4.12）：
 *   ① 存一份团队库镜像；
 *   ② 接收各端 push 的变更行并执行合并判定；
 *   ③ 向各端 pull 分发增量。
 * 明确不做：业务计算、提醒调度、外部数据抓取——那些永远留在客户端。
 *
 * 只依赖 Node 22 内置模块（http / crypto / node:sqlite），零 npm 依赖。
 * 这是刻意的：服务端越薄，越不容易成为第二个需要维护的应用。
 *
 * 启动：node src/server.js   （环境变量见 README）
 */

const http = require('node:http');
const crypto = require('node:crypto');
const fs = require('node:fs');
const path = require('node:path');
const { DatabaseSync } = require('node:sqlite');

const CONFIG = {
  port: Number(process.env.WB_PORT || 8443),
  bind: process.env.WB_BIND || '0.0.0.0',
  // 管理接口只绑回环（架构 §4.12 加固基线）
  adminBind: process.env.WB_ADMIN_BIND || '127.0.0.1',
  adminPort: Number(process.env.WB_ADMIN_PORT || 8444),
  dataDir: process.env.WB_DATA_DIR || path.join(process.cwd(), 'data'),
  // 服务端拒绝低于自身 schema 版本的连接（架构 §4.16）
  // v2（2026-10-27）：多设备令牌表 token；v1 是单 token(member.token) 时代。
  schemaVersion: Number(process.env.WB_SCHEMA_VERSION || 2),
  backupKeep: Number(process.env.WB_BACKUP_KEEP_DAYS || 14),
  inviteTtlHours: 48,
  // 登录失败限速（fail2ban 类的轻量前置，架构 §4.12）
  loginWindowMs: 10 * 60 * 1000,
  loginMaxFails: 10,
};

const DATA_DIR = CONFIG.dataDir;
const BACKUP_DIR = path.join(DATA_DIR, 'backups');
fs.mkdirSync(DATA_DIR, { recursive: true });
fs.mkdirSync(BACKUP_DIR, { recursive: true });

const db = new DatabaseSync(path.join(DATA_DIR, 'team.db'));
db.exec('PRAGMA journal_mode = WAL');
db.exec('PRAGMA foreign_keys = ON');

// ───────────────────────────── schema ─────────────────────────────

db.exec(`
CREATE TABLE IF NOT EXISTS meta (
  k TEXT PRIMARY KEY,
  v TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS member (
  member_id     TEXT PRIMARY KEY,
  display_name  TEXT NOT NULL,
  role          TEXT NOT NULL CHECK (role IN ('admin','member')),
  password_hash TEXT NOT NULL,
  token         TEXT,
  invited_by    TEXT,
  created_at    TEXT NOT NULL,
  status        TEXT NOT NULL DEFAULT 'active'
);
CREATE TABLE IF NOT EXISTS invite (
  code        TEXT PRIMARY KEY,
  created_by  TEXT NOT NULL,
  expires_at  INTEGER NOT NULL,
  used_by     TEXT,
  used_at     INTEGER
);
-- 中心行版本：整行 JSON + 版本戳，服务端只搬运不解释业务含义
CREATE TABLE IF NOT EXISTS row_version (
  table_name  TEXT NOT NULL,
  row_id      TEXT NOT NULL,
  payload     TEXT NOT NULL,
  updated_at  INTEGER NOT NULL,
  updated_by  TEXT NOT NULL,
  device_id   TEXT NOT NULL,
  deleted_at  INTEGER,
  seq         INTEGER NOT NULL,
  PRIMARY KEY (table_name, row_id)
);
CREATE INDEX IF NOT EXISTS idx_row_seq ON row_version(seq);
CREATE TABLE IF NOT EXISTS conflict_log (
  id          INTEGER PRIMARY KEY AUTOINCREMENT,
  table_name  TEXT NOT NULL,
  row_id      TEXT NOT NULL,
  losing_json TEXT NOT NULL,
  losing_by   TEXT,
  winning_by  TEXT,
  resolved_at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS audit (
  id       INTEGER PRIMARY KEY AUTOINCREMENT,
  at       INTEGER NOT NULL,
  actor    TEXT,
  action   TEXT NOT NULL,
  detail   TEXT
);
-- 多设备令牌（2026-10-27 裁决：每设备一 token、每成员上限 5、淘汰最旧）。
-- member.token 是单 token 时代的产物，保留列以兼容旧库，但不再读写：
-- 单 token 覆盖会让「新登录顶掉旧登录」，多设备并存后这条作废路径直接失效。
CREATE TABLE IF NOT EXISTS token (
  token          TEXT PRIMARY KEY,
  member_id      TEXT NOT NULL,
  device_id      TEXT,
  created_at     INTEGER NOT NULL,
  last_active_at INTEGER NOT NULL,
  revoked_at     INTEGER
);
CREATE INDEX IF NOT EXISTS idx_token_member ON token(member_id, revoked_at);
`);

const getMeta = (k) => {
  const r = db.prepare('SELECT v FROM meta WHERE k = ?').get(k);
  return r ? r.v : null;
};
const setMeta = (k, v) => {
  db.prepare('INSERT INTO meta (k,v) VALUES (?,?) ON CONFLICT(k) DO UPDATE SET v = excluded.v').run(k, String(v));
};

if (getMeta('schema_version') === null) setMeta('schema_version', CONFIG.schemaVersion);
if (getMeta('seq') === null) setMeta('seq', 0);

const nextSeq = () => {
  db.exec('BEGIN IMMEDIATE');
  try {
    const cur = Number(getMeta('seq')) || 0;
    const n = cur + 1;
    setMeta('seq', n);
    db.exec('COMMIT');
    return n;
  } catch (e) {
    db.exec('ROLLBACK');
    throw e;
  }
};

// ───────────────────────────── 工具 ─────────────────────────────

const nowMs = () => Date.now();
const nowIso = () => new Date().toISOString();

/** scrypt 哈希：不引第三方依赖，也不落明文 */
function hashPassword(password, salt = crypto.randomBytes(16).toString('hex')) {
  const dk = crypto.scryptSync(password, salt, 32).toString('hex');
  return `scrypt$${salt}$${dk}`;
}
function verifyPassword(password, stored) {
  const parts = String(stored).split('$');
  if (parts.length !== 3 || parts[0] !== 'scrypt') return false;
  const dk = crypto.scryptSync(password, parts[1], 32).toString('hex');
  return crypto.timingSafeEqual(Buffer.from(dk, 'hex'), Buffer.from(parts[2], 'hex'));
}
const newToken = () => crypto.randomBytes(32).toString('hex');
const newMemberId = () => 'm_' + crypto.randomBytes(8).toString('hex');
const newInviteCode = () => crypto.randomBytes(9).toString('base64url').toUpperCase().slice(0, 10);

// ── 迁移 v1 → v2：单 token(member.token) → 多设备 token 表 ──
// 可重入：靠 meta.schema_version 判断，重复执行不会重复搬迁。
// 旧库里的 member.token 搬进 token 表后清空该列——避免同一枚令牌存在两处、
// 也避免旧代码路径（若有）继续从 member.token 读出一个已被多设备模型弃用的值。
const curSchema = Number(getMeta('schema_version')) || 1;
if (curSchema < 2) {
  const legacy = db.prepare(
    "SELECT member_id, token, created_at FROM member WHERE token IS NOT NULL AND token <> ''"
  ).all();
  const ins = db.prepare(
    `INSERT INTO token (token, member_id, device_id, created_at, last_active_at, revoked_at)
     VALUES (?, ?, ?, ?, ?, NULL)
     ON CONFLICT(token) DO NOTHING`
  );
  const stamp = Date.parse(legacy[0]?.created_at || '') || nowMs();
  for (const m of legacy) {
    ins.run(m.token, m.member_id, 'legacy-unknown', stamp, stamp);
  }
  db.prepare("UPDATE member SET token = NULL").run();
  setMeta('schema_version', 2);
  db.prepare('INSERT INTO audit (at, actor, action, detail) VALUES (?, ?, ?, ?)').run(
    nowMs(), null, 'schema.migrate', `v${curSchema} -> v2；搬迁旧 token ${legacy.length} 枚`
  );
}
// 兜底：配置文件指定了更高版本号时，以配置为准（服务端只升不降）。
if (Number(getMeta('schema_version')) < CONFIG.schemaVersion) setMeta('schema_version', CONFIG.schemaVersion);

const MAX_TOKENS_PER_MEMBER = 5;

/// 登记一枚新令牌。每设备一 token：同一 device_id 重新登录时先撤销该设备的旧令牌，
/// 再插入新令牌（设备换了但同一台机器登录，不该占两个名额）。
/// 超过上限时淘汰最久未活跃的那枚（revoked_at 非空的不计入在用数）。
function issueToken(memberId, deviceId) {
  const dev = String(deviceId || '').trim() || `dev-${newToken().slice(0, 12)}`;
  const at = nowMs();
  db.exec('BEGIN IMMEDIATE');
  try {
    if (deviceId) {
      db.prepare(
        'UPDATE token SET revoked_at = ? WHERE member_id = ? AND device_id = ? AND revoked_at IS NULL'
      ).run(at, memberId, dev);
    }
    const active = db.prepare(
      'SELECT token FROM token WHERE member_id = ? AND revoked_at IS NULL ORDER BY last_active_at DESC'
    ).all(memberId);
    // 淘汰最旧：active 按 last_active_at 倒序，尾部的先撤，直到留出 1 个空位。
    const excess = active.length - (MAX_TOKENS_PER_MEMBER - 1);
    for (let i = 0; i < excess; i++) {
      const victim = active[active.length - 1 - i];
      db.prepare('UPDATE token SET revoked_at = ? WHERE token = ?').run(at, victim.token);
    }
    const token = newToken();
    db.prepare(
      `INSERT INTO token (token, member_id, device_id, created_at, last_active_at, revoked_at)
       VALUES (?, ?, ?, ?, ?, NULL)`
    ).run(token, memberId, dev, at, at);
    const left = db.prepare(
      'SELECT COUNT(*) AS n FROM token WHERE member_id = ? AND revoked_at IS NULL'
    ).get(memberId).n;
    db.exec('COMMIT');
    return { token, deviceId: dev, activeTokens: Number(left) };
  } catch (e) {
    db.exec('ROLLBACK');
    throw e;
  }
}


/**
 * 条目级 LWW 合并判定（与客户端 wb-core::sync 同一口径）。
 * 返回 { winner: 'local'|'remote', progressOverridden: bool }
 */
function compareVersion(a, b) {
  if (a.updated_at !== b.updated_at) return a.updated_at > b.updated_at ? 1 : -1;
  if (a.updated_by !== b.updated_by) return a.updated_by > b.updated_by ? 1 : -1;
  if (a.device_id !== b.device_id) return a.device_id > b.device_id ? 1 : -1;
  return 0;
}

function mergeDecision(local, remote) {
  if (local.payload === remote.payload && (local.deleted_at ?? null) === (remote.deleted_at ?? null)) {
    return { winner: 'identical', progressOverridden: false };
  }
  const cmp = compareVersion(local, remote);
  if (cmp === 0) return { winner: 'local', unclear: true }; // 无法自动判定，保留中心值并标记
  return { winner: cmp > 0 ? 'local' : 'remote', progressOverridden: false };
}

function progressOf(payloadStr) {
  try {
    const p = JSON.parse(payloadStr);
    return typeof p.progress_pct === 'number' ? p.progress_pct : null;
  } catch {
    return null;
  }
}

// 登录限速状态（内存即可，服务端重启清零不影响安全边界）
const loginFails = new Map();
function loginThrottled(key) {
  const rec = loginFails.get(key);
  if (!rec) return false;
  if (nowMs() - rec.first > CONFIG.loginWindowMs) {
    loginFails.delete(key);
    return false;
  }
  return rec.count >= CONFIG.loginMaxFails;
}
function noteLoginFail(key) {
  const rec = loginFails.get(key);
  if (!rec || nowMs() - rec.first > CONFIG.loginWindowMs) {
    loginFails.set(key, { first: nowMs(), count: 1 });
    return;
  }
  rec.count += 1;
}

// ───────────────────────────── HTTP ─────────────────────────────

const routes = new Map();
const route = (method, p, fn) => routes.set(`${method} ${p}`, fn);

function send(res, code, body) {
  const text = JSON.stringify(body);
  res.writeHead(code, {
    'content-type': 'application/json; charset=utf-8',
    'content-length': Buffer.byteLength(text),
    'cache-control': 'no-store',
  });
  res.end(text);
}

function readBody(req, limit = 8 * 1024 * 1024) {
  return new Promise((resolve, reject) => {
    let size = 0;
    const chunks = [];
    req.on('data', (c) => {
      size += c.length;
      if (size > limit) {
        reject(new Error('payload too large'));
        req.destroy();
        return;
      }
      chunks.push(c);
    });
    req.on('end', () => {
      if (!chunks.length) return resolve({});
      try {
        resolve(JSON.parse(Buffer.concat(chunks).toString('utf8')));
      } catch (e) {
        reject(new Error('invalid json'));
      }
    });
    req.on('error', reject);
  });
}

function authMember(req) {
  const h = req.headers.authorization || '';
  const m = h.match(/^Bearer\s+(\S+)$/i);
  if (!m) return null;
  const tok = m[1];
  // v2：令牌在独立 token 表，按 token 精确匹配，且必须未撤销。
  // 顺带刷新 last_active_at——「淘汰最旧」靠它排序，不刷新就永远是登录顺序。
  const row = db.prepare(
    `SELECT mem.* FROM token t JOIN member mem ON mem.member_id = t.member_id
      WHERE t.token = ? AND t.revoked_at IS NULL AND mem.status = 'active'`
  ).get(tok);
  if (!row) return null;
  db.prepare('UPDATE token SET last_active_at = ? WHERE token = ?').run(nowMs(), tok);
  return row;
}

/// 当前生效的令牌数（不含已撤销）。
function activeTokenCount(memberId) {
  return Number(
    db.prepare('SELECT COUNT(*) AS n FROM token WHERE member_id = ? AND revoked_at IS NULL').get(memberId).n
  );
}

// 处理器签名统一为 (req, res, params, member, params, url)。
// requireAuth/requireAdmin 必须把 url 原样透传下去——否则 /sync/pull 这类
// 带查询参数的 GET 会在 url.searchParams 上抛 400（首次实测抓到的缺陷）。
const requireAuth = (fn) => async (req, res, params, _ignoredMember, _params, url) => {
  const member = authMember(req);
  if (!member) return send(res, 401, { ok: false, error: 'unauthorized' });
  return fn(req, res, params, member, _params, url);
};

const requireAdmin = (fn) => requireAuth(async (req, res, params, member, _params, url) => {
  if (member.role !== 'admin') return send(res, 403, { ok: false, error: 'admin only' });
  return fn(req, res, params, member, _params, url);
});

// ── 健康检查与版本协商 ──
route('GET', '/health', async (req, res) => {
  send(res, 200, { ok: true, schema_version: Number(getMeta('schema_version')), seq: Number(getMeta('seq')), at: nowIso() });
});

// ── 首成员初始化（只允许一次）──
route('POST', '/setup', async (req, res) => {
  const body = await readBody(req);
  const count = db.prepare('SELECT COUNT(*) AS n FROM member').get().n;
  if (count > 0) return send(res, 409, { ok: false, error: 'already initialized' });
  if (!body.username || !body.password || String(body.password).length < 8) {
    return send(res, 400, { ok: false, error: 'username and password(>=8) required' });
  }
  const id = newMemberId();
  db.prepare(
    `INSERT INTO member (member_id, display_name, role, password_hash, token, created_at)
     VALUES (?, ?, 'admin', ?, NULL, ?)`
  ).run(id, String(body.username), hashPassword(body.password), nowIso());
  const issued = issueToken(id, body.device_id);
  db.prepare('INSERT INTO audit (at, actor, action, detail) VALUES (?, ?, ?, ?)').run(
    nowMs(), id, 'setup', '首成员初始化，成为 admin'
  );
  send(res, 200, {
    ok: true, member_id: id, role: 'admin',
    token: issued.token, device_id: issued.deviceId, active_tokens: issued.activeTokens,
  });
});

// ── 登录 ──
route('POST', '/login', async (req, res) => {
  const body = await readBody(req);
  const key = req.socket.remoteAddress || 'unknown';
  if (loginThrottled(key)) return send(res, 429, { ok: false, error: 'too many attempts, retry later' });
  const m = db.prepare("SELECT * FROM member WHERE display_name = ? AND status = 'active'").get(String(body.username || ''));
  if (!m || !verifyPassword(String(body.password || ''), m.password_hash)) {
    noteLoginFail(key);
    return send(res, 401, { ok: false, error: 'invalid credentials' });
  }
  const issued = issueToken(m.member_id, body.device_id);
  loginFails.delete(key);
  send(res, 200, {
    ok: true, member_id: m.member_id, role: m.role,
    token: issued.token, device_id: issued.deviceId, active_tokens: issued.activeTokens,
  });
});

// ── 退出登录：只撤销自己当前这枚令牌 ──
// 注意：这**不足以**兜住「设备丢失去作废」的场景——丢的那台机器没法自己来调这个接口。
// 那种情况要走 POST /tokens/revoke 指定令牌撤销（下方，本人或 admin）。
route('POST', '/logout', requireAuth(async (req, res, _p, member) => {
  const h = req.headers.authorization || '';
  const m = h.match(/^Bearer\s+(\S+)$/i);
  if (m) db.prepare('UPDATE token SET revoked_at = ? WHERE token = ? AND revoked_at IS NULL').run(nowMs(), m[1]);
  db.prepare('INSERT INTO audit (at, actor, action, detail) VALUES (?, ?, ?, ?)').run(
    nowMs(), member.member_id, 'logout', '撤销当前设备令牌'
  );
  send(res, 200, { ok: true, active_tokens: activeTokenCount(member.member_id) });
}));

// ── 本设备令牌列表（本人可见，令牌只给前缀）──
route('GET', '/tokens', requireAuth(async (req, res, _p, member) => {
  const rows = db.prepare(
    `SELECT token, device_id, created_at, last_active_at, revoked_at
       FROM token WHERE member_id = ? ORDER BY last_active_at DESC`
  ).all(member.member_id);
  const items = rows.map((r) => ({
    token_prefix: String(r.token).slice(0, 8),
    device_id: r.device_id,
    created_at: r.created_at,
    last_active_at: r.last_active_at,
    revoked: r.revoked_at !== null,
  }));
  send(res, 200, { ok: true, tokens: items, max: MAX_TOKENS_PER_MEMBER });
}));

// ── 撤销指定设备令牌（本人或 admin）──
// 存在的理由：多设备并存后，旧 token 不再被新登录覆盖，一旦某台设备丢失，
// 只能靠这条接口把它单独作废，否则那枚令牌会一直有效（且本服务端没有过期机制）。
// 身份校验：member_id + device_id 二选一，且必须属于调用者本人；admin 可撤任意成员。
route('POST', '/tokens/revoke', requireAuth(async (req, res, _p, member) => {
  const body = await readBody(req);
  const targetMember = body.member_id ? String(body.member_id) : member.member_id;
  if (targetMember !== member.member_id && member.role !== 'admin') {
    return send(res, 403, { ok: false, error: 'not your token' });
  }
  const at = nowMs();
  let changed = 0;
  if (body.device_id) {
    changed = db.prepare(
      'UPDATE token SET revoked_at = ? WHERE member_id = ? AND device_id = ? AND revoked_at IS NULL'
    ).run(at, targetMember, String(body.device_id)).changes;
  } else if (body.token_prefix) {
    // 前端只有前缀（完整令牌从不回传），按前缀在该成员名下唯一匹配。
    const cands = db.prepare(
      'SELECT token FROM token WHERE member_id = ? AND revoked_at IS NULL'
    ).all(targetMember).filter((r) => String(r.token).startsWith(String(body.token_prefix)));
    if (cands.length !== 1) {
      return send(res, 400, { ok: false, error: cands.length ? 'ambiguous token prefix' : 'token not found' });
    }
    changed = db.prepare('UPDATE token SET revoked_at = ? WHERE token = ?').run(at, cands[0].token).changes;
  } else {
    return send(res, 400, { ok: false, error: 'device_id or token_prefix required' });
  }
  db.prepare('INSERT INTO audit (at, actor, action, detail) VALUES (?, ?, ?, ?)').run(
    at, member.member_id, 'token.revoke',
    `${targetMember} / ${body.device_id || body.token_prefix} / changed=${changed}`
  );
  send(res, 200, { ok: true, changed, active_tokens: activeTokenCount(targetMember) });
}));

// ── 邀请（admin）──
route('POST', '/invite', requireAdmin(async (req, res, _p, member) => {
  const code = newInviteCode();
  const expires = nowMs() + CONFIG.inviteTtlHours * 3600 * 1000;
  db.prepare('INSERT INTO invite (code, created_by, expires_at) VALUES (?, ?, ?)').run(code, member.member_id, expires);
  db.prepare('INSERT INTO audit (at, actor, action, detail) VALUES (?, ?, ?, ?)').run(nowMs(), member.member_id, 'invite.create', code);
  send(res, 200, { ok: true, code, expires_at: expires, ttl_hours: CONFIG.inviteTtlHours });
}));

// ── 凭邀请码注册 ──
route('POST', '/join', async (req, res) => {
  const body = await readBody(req);
  const inv = db.prepare('SELECT * FROM invite WHERE code = ?').get(String(body.code || '').toUpperCase());
  if (!inv) return send(res, 404, { ok: false, error: 'invite not found' });
  if (inv.used_by) return send(res, 409, { ok: false, error: 'invite already used' });
  if (inv.expires_at < nowMs()) return send(res, 410, { ok: false, error: 'invite expired' });
  if (!body.username || String(body.password || '').length < 8) {
    return send(res, 400, { ok: false, error: 'username and password(>=8) required' });
  }
  const id = newMemberId();
  db.prepare(
    `INSERT INTO member (member_id, display_name, role, password_hash, token, invited_by, created_at)
     VALUES (?, ?, 'member', ?, NULL, ?, ?)`
  ).run(id, String(body.username), hashPassword(body.password), inv.created_by, nowIso());
  const issued = issueToken(id, body.device_id);
  db.prepare('UPDATE invite SET used_by = ?, used_at = ? WHERE code = ?').run(id, nowMs(), inv.code);
  db.prepare('INSERT INTO audit (at, actor, action, detail) VALUES (?, ?, ?, ?)').run(nowMs(), id, 'member.join', inv.code);
  send(res, 200, {
    ok: true, member_id: id, role: 'member',
    token: issued.token, device_id: issued.deviceId, active_tokens: issued.activeTokens,
  });
});

// ── 成员列表 / 移除（admin）──
route('GET', '/members', requireAdmin(async (req, res) => {
  const rows = db.prepare('SELECT member_id, display_name, role, created_at, status FROM member ORDER BY created_at').all();
  send(res, 200, { ok: true, members: rows });
}));
route('POST', '/members/remove', requireAdmin(async (req, res, _p, member) => {
  const body = await readBody(req);
  if (body.member_id === member.member_id) return send(res, 400, { ok: false, error: 'cannot remove yourself' });
  // 护栏：不许移除库里最后一个可用 admin。否则没人能登录、也无人能再发邀请，
  // 比「凭据泄露」更糟——这是 2026-10-27 复核明确裁决过的坑。
  const target = db.prepare('SELECT member_id, role, status FROM member WHERE member_id = ?').get(String(body.member_id));
  if (!target) return send(res, 404, { ok: false, error: 'member not found' });
  if (target.role === 'admin') {
    const admins = db.prepare(
      "SELECT COUNT(*) AS n FROM member WHERE role = 'admin' AND status = 'active'"
    ).get().n;
    if (Number(admins) <= 1) {
      return send(res, 409, {
        ok: false,
        error: 'last admin',
        hint: '唯一 admin 不可禁用。要作废外泄的库，请删除库文件、首启建空库后用新口令重注册。',
      });
    }
  }
  db.prepare("UPDATE member SET status = 'disabled' WHERE member_id = ?").run(String(body.member_id));
  // 禁用成员时连带撤销其全部令牌，否则 disabled 只是挡登录、挡不住已发出的令牌。
  db.prepare('UPDATE token SET revoked_at = ? WHERE member_id = ? AND revoked_at IS NULL').run(nowMs(), String(body.member_id));
  db.prepare('INSERT INTO audit (at, actor, action, detail) VALUES (?, ?, ?, ?)').run(nowMs(), member.member_id, 'member.remove', String(body.member_id));
  send(res, 200, { ok: true });
}));

// ── push：接收变更行并合并 ──
route('POST', '/sync/push', requireAuth(async (req, res, _p, member) => {
  const body = await readBody(req);
  if (Number(body.schema_version) !== Number(getMeta('schema_version'))) {
    // 新旧行结构混流会让合并结果不可预测（架构 §4.16）
    return send(res, 409, { ok: false, error: 'schema version mismatch', server: Number(getMeta('schema_version')) });
  }
  const rows = Array.isArray(body.rows) ? body.rows : [];
  const results = [];
  const conflicts = [];
  const stmtGet = db.prepare('SELECT * FROM row_version WHERE table_name = ? AND row_id = ?');
  const stmtPut = db.prepare(
    `INSERT INTO row_version (table_name,row_id,payload,updated_at,updated_by,device_id,deleted_at,seq)
     VALUES (?,?,?,?,?,?,?,?)
     ON CONFLICT(table_name,row_id) DO UPDATE SET
       payload=excluded.payload, updated_at=excluded.updated_at, updated_by=excluded.updated_by,
       device_id=excluded.device_id, deleted_at=excluded.deleted_at, seq=excluded.seq`
  );

  for (const r of rows) {
    if (!r) continue;
    // 客户端核心 SyncRow 用 table/id；同时容忍 table_name/row_id 别名，
    // 避免两套命名约定在集成期静默丢行（实测抓到过一次 applied:[]）。
    const tableName = r.table ?? r.table_name;
    const rowId = r.id ?? r.row_id;
    if (!tableName || !rowId) continue;
    const incoming = {
      table_name: String(tableName),
      row_id: String(rowId),
      payload: typeof r.payload === 'string' ? r.payload : JSON.stringify(r.payload ?? {}),
      updated_at: Number(r.updated_at || 0),
      updated_by: String(r.updated_by || member.member_id),
      device_id: String(r.device_id || 'unknown'),
      deleted_at: r.deleted_at == null ? null : Number(r.deleted_at),
    };
    const local = stmtGet.get(incoming.table_name, incoming.row_id);
    if (!local) {
      const seq = nextSeq();
      stmtPut.run(incoming.table_name, incoming.row_id, incoming.payload, incoming.updated_at,
        incoming.updated_by, incoming.device_id, incoming.deleted_at, seq);
      results.push({ table: incoming.table_name, id: incoming.row_id, decision: 'inserted', seq });
      continue;
    }
    const d = mergeDecision(local, incoming);
    if (d.winner === 'identical') {
      results.push({ table: incoming.table_name, id: incoming.row_id, decision: 'identical', seq: local.seq });
      continue;
    }
    if (d.unclear) {
      results.push({ table: incoming.table_name, id: incoming.row_id, decision: 'needs_manual_review', seq: local.seq });
      continue;
    }
    // 与 Rust 核心 merge_row 对齐：任一方向发生覆盖，都要——
    // ① progress_pct 取 max（定向例外，架构 §4.14），只有真的改写胜出方进度才算应用；
    // ② 被覆盖值进冲突日志（local_wins 时被覆盖的是刚推来的旧值，同样必须留痕）。
    const lp = progressOf(local.payload);
    const rp = progressOf(incoming.payload);
    if (d.winner === 'local') {
      // 本地胜出：行整体不动，但本地进度可能要被更旧的远端进度抬升。
      if (lp !== null && rp !== null && rp > lp) {
        const obj = JSON.parse(local.payload);
        obj.progress_pct = rp;
        const seq = nextSeq();
        stmtPut.run(local.table_name, local.row_id, JSON.stringify(obj), local.updated_at,
          local.updated_by, local.device_id, local.deleted_at, seq);
        db.prepare(
          `INSERT INTO conflict_log (table_name,row_id,losing_json,losing_by,winning_by,resolved_at)
           VALUES (?,?,?,?,?,?)`
        ).run(local.table_name, local.row_id, incoming.payload, incoming.updated_by, local.updated_by, nowMs());
        conflicts.push({ table: local.table_name, id: local.row_id });
        results.push({ table: incoming.table_name, id: incoming.row_id, decision: 'local_wins_progress_max', seq });
        continue;
      }
      db.prepare(
        `INSERT INTO conflict_log (table_name,row_id,losing_json,losing_by,winning_by,resolved_at)
         VALUES (?,?,?,?,?,?)`
      ).run(local.table_name, local.row_id, incoming.payload, incoming.updated_by, local.updated_by, nowMs());
      conflicts.push({ table: local.table_name, id: local.row_id });
      results.push({ table: incoming.table_name, id: incoming.row_id, decision: 'local_wins', seq: local.seq });
      continue;
    }
    // 远端胜出：整体采用远端行，但 progress_pct 取 max（定向例外，架构 §4.14）
    let payload = incoming.payload;
    let progressApplied = false;
    if (lp !== null && rp !== null && lp > rp) {
      const obj = JSON.parse(incoming.payload);
      obj.progress_pct = lp;
      payload = JSON.stringify(obj);
      progressApplied = true;
    }
    const seq = nextSeq();
    stmtPut.run(incoming.table_name, incoming.row_id, payload, incoming.updated_at,
      incoming.updated_by, incoming.device_id, incoming.deleted_at, seq);

    // 被覆盖值进冲突日志：永久可查，不做定期清理（架构 §4.14）
    db.prepare(
      `INSERT INTO conflict_log (table_name,row_id,losing_json,losing_by,winning_by,resolved_at)
       VALUES (?,?,?,?,?,?)`
    ).run(local.table_name, local.row_id, local.payload, local.updated_by, incoming.updated_by, nowMs());
    conflicts.push({ table: local.table_name, id: local.row_id });

    results.push({
      table: incoming.table_name, id: incoming.row_id,
      decision: progressApplied ? 'remote_wins_progress_max' : 'remote_wins', seq,
    });
  }

  send(res, 200, { ok: true, applied: results, conflicts, seq: Number(getMeta('seq')) });
}));

// ── pull：按游标分发增量 ──
route('GET', '/sync/pull', requireAuth(async (req, res, _p, _member, _params, url) => {
  const cursor = Number(url.searchParams.get('cursor') || 0);
  const limit = Math.min(Number(url.searchParams.get('limit') || 500), 2000);
  const rows = db.prepare(
    'SELECT table_name,row_id,payload,updated_at,updated_by,device_id,deleted_at,seq FROM row_version WHERE seq > ? ORDER BY seq ASC LIMIT ?'
  ).all(cursor, limit);
  send(res, 200, {
    ok: true,
    rows: rows.map((r) => ({
      table: r.table_name, id: r.row_id, payload: JSON.parse(r.payload),
      updated_at: r.updated_at, updated_by: r.updated_by, device_id: r.device_id,
      deleted_at: r.deleted_at, seq: r.seq,
    })),
    cursor: rows.length ? rows[rows.length - 1].seq : cursor,
    has_more: rows.length === limit,
    server_seq: Number(getMeta('seq')),
  });
}));

// ── 冲突回看（第二期 UI 用，接口先留）──
route('GET', '/sync/conflicts', requireAuth(async (req, res, _p, _m, _params, url) => {
  const limit = Math.min(Number(url.searchParams.get('limit') || 100), 500);
  const rows = db.prepare('SELECT * FROM conflict_log ORDER BY id DESC LIMIT ?').all(limit);
  send(res, 200, { ok: true, conflicts: rows });
}));

// ── 服务端备份（admin 手动触发）──
route('POST', '/admin/backup', requireAdmin(async (req, res, _p, member) => {
  const r = backupNow();
  db.prepare('INSERT INTO audit (at, actor, action, detail) VALUES (?, ?, ?, ?)').run(nowMs(), member.member_id, 'backup', r.file);
  send(res, 200, { ok: true, ...r });
}));

// ── 请求分发 ──
function makeHandler() {
  return async (req, res) => {
    try {
      const url = new URL(req.url, `http://${req.headers.host || 'localhost'}`);
      const fn = routes.get(`${req.method} ${url.pathname}`);
      if (!fn) return send(res, 404, { ok: false, error: 'not found' });
      await fn(req, res, {}, null, {}, url);
    } catch (e) {
      send(res, 400, { ok: false, error: String(e && e.message ? e.message : e) });
    }
  };
}

/**
 * 服务端数据目录每日备份，保留 14 天（架构 §9 风险行）。
 * 用 SQLite 在线备份接口（VACUUM INTO），不拷文件——与客户端同一纪律。
 */
function backupNow() {
  const stamp = new Date().toISOString().replace(/[:.]/g, '-');
  const file = path.join(BACKUP_DIR, `team_${stamp}.db`);
  db.exec(`VACUUM INTO '${file.replace(/'/g, "''")}'`);
  pruneBackups();
  const bytes = fs.statSync(file).size;
  return { file, bytes, kept: listBackups().length };
}

function listBackups() {
  return fs.readdirSync(BACKUP_DIR).filter((f) => f.startsWith('team_') && f.endsWith('.db')).sort();
}

function pruneBackups() {
  const files = listBackups();
  while (files.length > CONFIG.backupKeep) {
    const victim = files.shift();
    try {
      fs.unlinkSync(path.join(BACKUP_DIR, victim));
    } catch {
      /* 已不存在即视为已清理 */
    }
  }
}

// 每日备份定时器：低频任务，不与同步请求抢资源
setInterval(() => {
  try {
    backupNow();
  } catch (e) {
    // 备份失败只记审计，不打断服务
    db.prepare('INSERT INTO audit (at, actor, action, detail) VALUES (?, ?, ?, ?)').run(
      nowMs(), 'system', 'backup.failed', String(e && e.message)
    );
  }
}, 24 * 3600 * 1000).unref();

// ───────────────────────────── 启动 ─────────────────────────────

if (require.main === module) {
  const publicServer = http.createServer(makeHandler());
  publicServer.listen(CONFIG.port, CONFIG.bind, () => {
    console.log(`[wb-sync] 同步端口 ${CONFIG.bind}:${CONFIG.port}`);
    console.log(`[wb-sync] 数据目录 ${DATA_DIR}`);
    console.log(`[wb-sync] schema 版本 ${getMeta('schema_version')} · 当前 seq ${getMeta('seq')}`);
  });

  // 管理接口只绑回环（架构 §4.12），不与业务端口混跑公网
  const adminServer = http.createServer(makeHandler());
  adminServer.listen(CONFIG.adminPort, CONFIG.adminBind, () => {
    console.log(`[wb-sync] 管理端口 ${CONFIG.adminBind}:${CONFIG.adminPort}（仅回环）`);
  });

  const shutdown = () => {
    publicServer.close();
    adminServer.close();
    try {
      db.exec('PRAGMA wal_checkpoint(TRUNCATE)');
    } catch {
      /* 关闭阶段尽力而为 */
    }
    process.exit(0);
  };
  process.on('SIGINT', shutdown);
  process.on('SIGTERM', shutdown);
}

module.exports = {
  mergeDecision, compareVersion, hashPassword, verifyPassword, backupNow, makeHandler,
  db, CONFIG,
};
