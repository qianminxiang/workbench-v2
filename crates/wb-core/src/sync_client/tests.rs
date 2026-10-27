//! 同步编排的确定性测试：断网补推、游标推进、冲突去重、进度不回退。
//!
//! 这些用例存在的理由只有一个：**没有网络也能证明补推链路是对的**。
//! 实机验证只能证明「某一次连上了」，穷举不了断网恢复、重复推、游标半途这些坏路径。

use super::*;
use crate::store::Store;
use serde_json::json;
use std::cell::RefCell;
use std::collections::HashMap;

fn tmp_store() -> (tempfile::TempDir, Store) {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    (dir, store)
}

fn row(id: &str, progress: i64, updated_at: i64, by: &str) -> SyncRow {
    let mut payload = serde_json::Map::new();
    payload.insert("id".into(), json!(id));
    payload.insert("seq_no".into(), json!(1));
    payload.insert("board_code".into(), json!("gov"));
    payload.insert("title".into(), json!(format!("条目 {id}")));
    payload.insert("planned_start_precision".into(), json!("day"));
    payload.insert("planned_end_precision".into(), json!("day"));
    payload.insert("progress_pct".into(), json!(progress));
    payload.insert("fiscal_year".into(), json!(2026));
    payload.insert("ext_json".into(), json!("{}"));
    payload.insert("updated_at".into(), json!(iso_from_ms(updated_at)));
    payload.insert("updated_by".into(), json!(by));
    SyncRow {
        table: "work_item".into(),
        id: id.into(),
        payload,
        updated_at,
        updated_by: by.into(),
        device_id: format!("d_{by}"),
        seq: 0,
        deleted_at: None,
    }
}

/// 内存假服务端：`online` 一关就模拟断网，并记录收到的推送。
#[derive(Default)]
struct FakeTransport {
    online: bool,
    pushed: RefCell<Vec<Vec<SyncRow>>>,
    server_rows: RefCell<HashMap<String, SyncRow>>,
    seq: RefCell<i64>,
    logged_out: RefCell<Vec<String>>,
    login_calls: RefCell<usize>,
}

impl FakeTransport {
    fn online() -> Self {
        FakeTransport { online: true, ..Default::default() }
    }
    fn offline() -> Self {
        FakeTransport { online: false, ..Default::default() }
    }
    fn seed(&self, r: SyncRow) {
        let mut s = self.seq.borrow_mut();
        *s += 1;
        let mut rr = r.clone();
        rr.seq = *s;
        self.server_rows.borrow_mut().insert(rr.id.clone(), rr);
    }
}

impl Transport for FakeTransport {
    fn login(
        &self,
        _base: &str,
        username: &str,
        _password: &str,
        device_id: &str,
    ) -> Result<LoginResult, TransportError> {
        if !self.online {
            return Err(TransportError::Unreachable("网络不可达".into()));
        }
        if username == "wrong" {
            return Err(TransportError::Unauthorized);
        }
        *self.login_calls.borrow_mut() += 1;
        Ok(LoginResult {
            member_id: "m_test".into(),
            role: "admin".into(),
            token: format!("tok_{device_id}"),
            device_id: device_id.to_string(),
            active_tokens: 1,
        })
    }

    fn push(&self, _base: &str, token: &str, rows: &[SyncRow]) -> Result<PushResult, TransportError> {
        if !self.online {
            return Err(TransportError::Unreachable("网络不可达".into()));
        }
        if token.is_empty() {
            return Err(TransportError::Unauthorized);
        }
        self.pushed.borrow_mut().push(rows.to_vec());
        let mut applied = Vec::new();
        for r in rows {
            let mut s = self.seq.borrow_mut();
            *s += 1;
            let seq = *s;
            let mut rr = r.clone();
            rr.seq = seq;
            self.server_rows.borrow_mut().insert(r.id.clone(), rr);
            applied.push(AppliedRow {
                table: r.table.clone(),
                id: r.id.clone(),
                decision: "inserted".into(),
                seq,
            });
        }
        Ok(PushResult { applied, conflicts: 0, server_seq: *self.seq.borrow() })
    }

    fn pull(
        &self,
        _base: &str,
        token: &str,
        cursor: i64,
        limit: usize,
    ) -> Result<PullResult, TransportError> {
        if !self.online {
            return Err(TransportError::Unreachable("网络不可达".into()));
        }
        if token.is_empty() {
            return Err(TransportError::Unauthorized);
        }
        let mut all: Vec<SyncRow> = self.server_rows.borrow().values().cloned().collect();
        all.sort_by_key(|r| r.seq);
        let page: Vec<SyncRow> = all.into_iter().filter(|r| r.seq > cursor).take(limit).collect();
        let next = page.last().map(|r| r.seq).unwrap_or(cursor);
        Ok(PullResult {
            has_more: page.len() == limit,
            rows: page,
            cursor: next,
            server_seq: *self.seq.borrow(),
        })
    }

    fn logout(&self, _base: &str, token: &str) -> Result<(), TransportError> {
        self.logged_out.borrow_mut().push(token.to_string());
        Ok(())
    }
}

// ───────────── 地址校验 ─────────────

#[test]
fn rejects_plaintext_http_on_public_host() {
    // 令牌与工作项内容都过这条连接，公网明文 http 等于公开
    assert!(validate_base_url("http://sync.example.com:8443").is_err());
    assert!(validate_base_url("https://sync.example.com:8443").is_ok());
}

#[test]
fn allows_plaintext_http_on_lan_and_loopback() {
    assert!(validate_base_url("http://localhost:8443").is_ok());
    assert!(validate_base_url("http://127.0.0.1:8443").is_ok());
    assert!(validate_base_url("http://192.168.1.9:8443").is_ok());
    assert!(validate_base_url("http://10.0.0.4:8443").is_ok());
    assert!(validate_base_url("http://172.20.0.9:8443").is_ok());
}

#[test]
fn rejects_bad_scheme_and_empty() {
    assert!(validate_base_url("").is_err());
    assert!(validate_base_url("sync.example.com").is_err());
    assert!(validate_base_url("ftp://sync.example.com").is_err());
    assert!(validate_base_url("https://").is_err());
}

#[test]
fn trims_trailing_slash() {
    assert_eq!(validate_base_url(" https://a.example.com/ ").unwrap(), "https://a.example.com");
}

// ───────────── 断网 → 补推 ─────────────

#[test]
fn offline_push_keeps_rows_and_reports_pending() {
    let (_d, store) = tmp_store();
    let conn = store.conn();
    let t = FakeTransport::offline();
    enqueue(conn, &row("w1", 10, 1_700_000_000_000, "u1"), "2026-10-27T01:00:00Z").unwrap();
    enqueue(conn, &row("w2", 20, 1_700_000_000_000, "u1"), "2026-10-27T01:00:01Z").unwrap();

    let rep = sync_once(conn, "http://localhost:8443", "tok", &t, 1_700_000_000_000, "2026-10-27T01:01:00Z");
    assert_eq!(rep.pushed, 0);
    assert_eq!(rep.pending, 2, "断网时改动必须留在队列，不许丢");
    assert_eq!(rep.state, SyncState::Pending);

    // 失败次数要记下来，排查断网时长靠它
    let attempts: i64 = conn
        .query_row("SELECT MAX(attempts) FROM sync_outbox", [], |r| r.get(0))
        .unwrap();
    assert_eq!(attempts, 1);
}

#[test]
fn reconnect_pushes_queued_rows_automatically() {
    let (_d, store) = tmp_store();
    let conn = store.conn();
    let t = FakeTransport::online();
    // 断网期间产生的两条改动
    enqueue(conn, &row("w1", 10, 1_700_000_000_000, "u1"), "2026-10-27T01:00:00Z").unwrap();
    enqueue(conn, &row("w2", 20, 1_700_000_000_000, "u1"), "2026-10-27T01:00:01Z").unwrap();

    let rep = sync_once(conn, "http://localhost:8443", "tok", &t, 1_700_000_000_000, "2026-10-27T02:00:00Z");
    assert_eq!(rep.pushed, 2, "恢复后必须自动补推");
    assert_eq!(rep.pending, 0, "补推成功后队列必须清空");
    assert_eq!(rep.state, SyncState::Connected);
    assert_eq!(t.pushed.borrow().len(), 1, "两条改动应合成一批");
}

#[test]
fn enqueue_is_upsert_and_keeps_attempts() {
    let (_d, store) = tmp_store();
    let conn = store.conn();
    enqueue(conn, &row("w1", 10, 1, "u1"), "2026-10-27T01:00:00Z").unwrap();
    conn.execute("UPDATE sync_outbox SET attempts = 3", []).unwrap();
    // 同一行又改了一次：队列里应该只有一行，但断网计数要保留
    enqueue(conn, &row("w1", 40, 2, "u1"), "2026-10-27T02:00:00Z").unwrap();
    let n = count_pending(conn).unwrap();
    assert_eq!(n, 1, "同一主键不得重复入队");
    let (attempts, progress) : (i64, i64) = conn
        .query_row("SELECT attempts, json_extract(payload,'$.progress_pct') FROM sync_outbox", [], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .unwrap();
    assert_eq!(attempts, 3, "重入队不得抹掉断网累计次数");
    assert_eq!(progress, 40, "重入队必须带上最新内容");
}

// ───────────── pull 落库 ─────────────

#[test]
fn pull_inserts_remote_rows_and_advances_cursor() {
    let (_d, store) = tmp_store();
    let conn = store.conn();
    let t = FakeTransport::online();
    t.seed(row("r1", 30, 1_700_000_000_000, "u2"));

    let rep = sync_once(conn, "http://localhost:8443", "tok", &t, 1_700_000_000_000, "2026-10-27T02:00:00Z");
    assert_eq!(rep.pulled, 1);
    assert_eq!(rep.cursor, 1);

    let title: String = conn
        .query_row("SELECT title FROM work_item WHERE id='r1'", [], |r| r.get(0))
        .unwrap();
    assert_eq!(title, "条目 r1");

    // 再同步一次：游标已推进，不该重复拉同一行
    let rep2 = sync_once(conn, "http://localhost:8443", "tok", &t, 1_700_000_000_000, "2026-10-27T02:01:00Z");
    assert_eq!(rep2.pulled, 0, "游标推进后不得重复应用同一增量");
    assert_eq!(rep2.cursor, 1);
}

#[test]
fn remote_newer_wins_and_progress_takes_max() {
    let (_d, store) = tmp_store();
    let conn = store.conn();
    let t = FakeTransport::online();
    // 本地：旧但进度高
    write_remote_row(conn, &row("w1", 80, 1_700_000_000_000, "u_local")).unwrap();
    // 远端：新但进度低 —— LWW 让远端胜，progress 例外取 max 保住 80
    t.seed(row("w1", 30, 1_700_000_100_000, "u_remote"));

    let rep = sync_once(conn, "http://localhost:8443", "tok", &t, 1_700_000_200_000, "2026-10-27T02:00:00Z");
    assert_eq!(rep.pulled, 1);

    let (progress, by): (i64, String) = conn
        .query_row("SELECT progress_pct, updated_by FROM work_item WHERE id='w1'", [], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .unwrap();
    assert_eq!(by, "u_remote", "LWW 应按 updated_at 让远端胜出");
    assert_eq!(progress, 80, "progress_pct 取 max：进度不许因同步回退");
    assert_eq!(rep.conflicts, 1, "被覆盖的本地值必须进冲突日志");
}

#[test]
fn local_newer_wins_and_remote_progress_still_raises() {
    let (_d, store) = tmp_store();
    let conn = store.conn();
    let t = FakeTransport::online();
    write_remote_row(conn, &row("w1", 40, 1_700_000_500_000, "u_local")).unwrap();
    t.seed(row("w1", 95, 1_700_000_000_000, "u_old"));

    let rep = sync_once(conn, "http://localhost:8443", "tok", &t, 1_700_000_600_000, "2026-10-27T02:00:00Z");
    let (progress, by): (i64, String) = conn
        .query_row("SELECT progress_pct, updated_by FROM work_item WHERE id='w1'", [], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .unwrap();
    assert_eq!(by, "u_local", "本地更新更晚，应保留本地内容");
    assert_eq!(progress, 95, "进度取 max：更旧的远端进度更高也要抬上来");
    assert_eq!(rep.conflicts, 1);
}

#[test]
fn conflict_log_is_deduped_on_repeat_pull() {
    let (_d, store) = tmp_store();
    let conn = store.conn();
    let t = FakeTransport::online();
    write_remote_row(conn, &row("w1", 80, 1_700_000_000_000, "u_local")).unwrap();
    t.seed(row("w1", 30, 1_700_000_100_000, "u_remote"));

    let r1 = sync_once(conn, "http://localhost:8443", "tok", &t, 1_700_000_200_000, "2026-10-27T02:00:00Z");
    assert_eq!(r1.conflicts, 1);
    // 把游标退回 0 强制重拉同一行，验证不重复记账
    state_set(conn, keys::CURSOR, "0").unwrap();
    let r2 = sync_once(conn, "http://localhost:8443", "tok", &t, 1_700_000_200_000, "2026-10-27T02:05:00Z");
    assert_eq!(r2.conflicts, 0, "同一条冲突重复拉取不得刷屏冲突日志");

    let n: i64 = conn
        .query_row("SELECT COUNT(*) FROM sync_conflict_log", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n, 1);
}

// ───────────── 登录 / 登出 ─────────────

#[test]
fn login_stores_token_but_not_in_status_payload() {
    let (_d, store) = tmp_store();
    let conn = store.conn();
    let t = FakeTransport::online();

    let st = login(conn, "http://localhost:8443", "大角子", "pw12345678", "seed", &t, "2026-10-27T02:00:00Z").unwrap();
    assert!(st.logged_in);
    assert_eq!(st.state, SyncState::Connected);
    assert_eq!(st.member_name.as_deref(), Some("大角子"));
    assert!(st.device_id.as_deref().unwrap_or("").starts_with("d_"));

    let json = serde_json::to_string(&st).unwrap();
    assert!(!json.contains("tok_"), "状态下发前端不得带令牌");
}

#[test]
fn login_rejects_invalid_address_before_hitting_network() {
    let (_d, store) = tmp_store();
    let conn = store.conn();
    let t = FakeTransport::online();
    let err = login(conn, "http://sync.example.com", "u", "pw12345678", "seed", &t, "2026-10-27T02:00:00Z")
        .unwrap_err();
    assert!(matches!(err, TransportError::Rejected(_)));
    assert_eq!(*t.login_calls.borrow(), 0, "地址不合法时不该发出登录请求");
}

#[test]
fn login_failure_keeps_status_actionable() {
    let (_d, store) = tmp_store();
    let conn = store.conn();
    let t = FakeTransport::online();
    let err = login(conn, "http://localhost:8443", "wrong", "pw12345678", "seed", &t, "2026-10-27T02:00:00Z")
        .unwrap_err();
    assert_eq!(err, TransportError::Unauthorized);
}

#[test]
fn logout_revokes_token_but_keeps_local_data() {
    let (_d, store) = tmp_store();
    let conn = store.conn();
    let t = FakeTransport::online();
    write_remote_row(conn, &row("w1", 20, 1_700_000_000_000, "u1")).unwrap();
    login(conn, "http://localhost:8443", "大角子", "pw12345678", "seed", &t, "2026-10-27T02:00:00Z").unwrap();

    let st = logout(conn, &t).unwrap();
    assert!(!st.logged_in);
    assert_eq!(st.state, SyncState::AuthRequired);
    assert_eq!(t.logged_out.borrow().len(), 1, "登出应通知服务端作废本机令牌");

    let n: i64 = conn.query_row("SELECT COUNT(*) FROM work_item", [], |r| r.get(0)).unwrap();
    assert_eq!(n, 1, "登出不得删本地数据：离线仍要能用");
}

#[test]
fn unreachable_server_still_allows_local_logout() {
    let (_d, store) = tmp_store();
    let conn = store.conn();
    let t = FakeTransport::offline();
    state_set(conn, keys::BASE_URL, "http://localhost:8443").unwrap();
    state_set(conn, keys::TOKEN, "tok_x").unwrap();
    let st = logout(conn, &t).unwrap();
    assert!(!st.logged_in, "服务端不可达也必须能本地登出");
}

// ───────────── 设备标识与全量入队 ─────────────

#[test]
fn device_id_is_stable_across_calls() {
    let (_d, store) = tmp_store();
    let conn = store.conn();
    let a = ensure_device_id(conn, "seed").unwrap();
    let b = ensure_device_id(conn, "seed").unwrap();
    assert_eq!(a, b, "设备标识必须稳定，否则 LWW 同刻决胜会退化成两台设备");
}

#[test]
fn seed_outbox_is_idempotent() {
    let (_d, store) = tmp_store();
    let conn = store.conn();
    write_remote_row(conn, &row("w1", 10, 1_700_000_000_000, "u1")).unwrap();
    write_remote_row(conn, &row("w2", 20, 1_700_000_000_000, "u1")).unwrap();
    let first = seed_outbox(conn, "2026-10-27T02:00:00Z").unwrap();
    let second = seed_outbox(conn, "2026-10-27T02:01:00Z").unwrap();
    assert_eq!(first, 2);
    assert_eq!(second, 2, "全量入队每次调用都会覆盖，但队列行数不变");
    assert_eq!(count_pending(conn).unwrap(), 2);
}

#[test]
fn unauthorized_push_switches_to_auth_required() {
    let (_d, store) = tmp_store();
    let conn = store.conn();
    let t = FakeTransport::online();
    enqueue(conn, &row("w1", 10, 1_700_000_000_000, "u1"), "2026-10-27T01:00:00Z").unwrap();
    // 空令牌 = 401
    let rep = sync_once(conn, "http://localhost:8443", "", &t, 1_700_000_000_000, "2026-10-27T02:00:00Z");
    assert_eq!(rep.state, SyncState::AuthRequired);
    assert_eq!(rep.pending, 1, "令牌失效时改动仍留在队列，重新登录后补推");
}

#[test]
fn iso_roundtrip() {
    let ms = 1_700_000_000_000i64;
    let iso = iso_from_ms(ms);
    assert_eq!(parse_iso_to_ms(&iso), Some(ms));
    assert_eq!(parse_iso_to_ms("not-a-time"), None);
}
