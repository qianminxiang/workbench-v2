//! 真实 HTTP 传输层：桌面端与 wbd 驱动器共用的 `Transport` 实现（架构 §4.15）。
//!
//! v2.1 单机化时随传输调用一并下线；v3.0 随服务器中枢恢复（2026-11-17 重建）。
//! 契约以 `server/src/server.js` 为准。两处线格式 → 客户端结构的换算**必须**在这里完成：
//! ① push 响应的 `conflicts` 是数组（逐条留痕），客户端 `PushResult.conflicts` 是计数；
//! ② push 响应的全局序列号字段叫 `seq`，pull 叫 `server_seq`。
//!
//! 超时口径：连接 10 秒、整请求 30 秒——慢网下宁可进补推队列，不要吊死 UI。

use std::time::Duration;

use serde_json::{json, Value};

use super::{
    AppliedRow, LoginResult, PullResult, PushResult, SyncRow, Transport, TransportError,
    CLIENT_SCHEMA_VERSION,
};

/// 真实传输实现。`Agent` 内部带连接池，可长期复用（桌面端进程生命周期一份即可）。
pub struct HttpTransport {
    agent: ureq::Agent,
}

impl Default for HttpTransport {
    fn default() -> Self {
        Self::new()
    }
}

impl HttpTransport {
    pub fn new() -> Self {
        let agent = ureq::AgentBuilder::new()
            .timeout_connect(Duration::from_secs(10))
            .timeout(Duration::from_secs(30))
            .build();
        Self { agent }
    }

    /// POST JSON 并解析响应体。`token` 为 None 时匿名（login / setup）。
    fn post_json(&self, url: &str, token: Option<&str>, body: &Value) -> Result<Value, TransportError> {
        let mut req = self.agent.post(url).set("Content-Type", "application/json");
        if let Some(t) = token {
            req = req.set("Authorization", &format!("Bearer {t}"));
        }
        let resp = req.send_string(&body.to_string()).map_err(map_err)?;
        body_to_json(resp)
    }

    /// GET 并解析响应体（带令牌）。
    fn get_json(&self, url: &str, token: &str) -> Result<Value, TransportError> {
        let resp = self
            .agent
            .get(url)
            .set("Authorization", &format!("Bearer {token}"))
            .call()
            .map_err(map_err)?;
        body_to_json(resp)
    }
}

fn body_to_json(resp: ureq::Response) -> Result<Value, TransportError> {
    let text = resp
        .into_string()
        .map_err(|e| TransportError::Unreachable(format!("读取响应失败：{e}")))?;
    serde_json::from_str(&text)
        .map_err(|e| TransportError::Rejected(format!("响应不是合法 JSON：{e}")))
}

/// ureq 错误 → 传输错误分类。分类决定恢复动作（补推 / 重登录 / 停止），不能笼统。
fn map_err(e: ureq::Error) -> TransportError {
    match e {
        ureq::Error::Status(code, resp) => {
            let body = resp.into_string().unwrap_or_default();
            let parsed: Option<Value> = serde_json::from_str(&body).ok();
            if code == 401 || code == 403 {
                return TransportError::Unauthorized;
            }
            if code == 409 {
                let msg = parsed
                    .as_ref()
                    .and_then(|v| v.get("error").and_then(|e| e.as_str()))
                    .unwrap_or("");
                if msg.contains("schema version mismatch") {
                    let server = parsed
                        .and_then(|v| v.get("server").and_then(|s| s.as_i64()))
                        .unwrap_or(0);
                    return TransportError::SchemaMismatch { server };
                }
            }
            let msg = parsed
                .as_ref()
                .and_then(|v| v.get("error").and_then(|e| e.as_str()))
                .map(String::from)
                .unwrap_or_else(|| {
                    if body.is_empty() { format!("HTTP {code}") } else { body.clone() }
                });
            TransportError::Rejected(format!("HTTP {code}：{msg}"))
        }
        ureq::Error::Transport(t) => TransportError::Unreachable(t.to_string()),
    }
}

/// login / setup 共用的响应解析（两者返回同形）。
fn parse_login(v: &Value, what: &str) -> Result<LoginResult, TransportError> {
    if v.get("ok").and_then(|b| b.as_bool()) != Some(true) {
        return Err(TransportError::Rejected(format!("{what}被拒绝")));
    }
    let token = v.get("token").and_then(|t| t.as_str()).unwrap_or_default();
    if token.is_empty() {
        return Err(TransportError::Rejected(format!(
            "{what}成功但未返回令牌（服务端契约异常）"
        )));
    }
    Ok(LoginResult {
        member_id: v.get("member_id").and_then(|s| s.as_str()).unwrap_or_default().to_string(),
        role: v.get("role").and_then(|s| s.as_str()).unwrap_or("member").to_string(),
        token: token.to_string(),
        device_id: v.get("device_id").and_then(|s| s.as_str()).unwrap_or_default().to_string(),
        active_tokens: v.get("active_tokens").and_then(|n| n.as_i64()).unwrap_or(1),
    })
}

/// push 响应（线格式）→ `PushResult`。独立成函数以便离线单测穷举换算口径。
fn parse_push(v: &Value) -> Result<PushResult, TransportError> {
    if v.get("ok").and_then(|b| b.as_bool()) != Some(true) {
        return Err(TransportError::Rejected("push 被拒绝（响应 ok=false）".into()));
    }
    let mut applied = Vec::new();
    if let Some(arr) = v.get("applied").and_then(|a| a.as_array()) {
        for r in arr {
            applied.push(AppliedRow {
                table: r.get("table").and_then(|s| s.as_str()).unwrap_or_default().to_string(),
                id: r.get("id").and_then(|s| s.as_str()).unwrap_or_default().to_string(),
                decision: r.get("decision").and_then(|s| s.as_str()).unwrap_or_default().to_string(),
                seq: r.get("seq").and_then(|n| n.as_i64()).unwrap_or(0),
            });
        }
    }
    // 线格式：conflicts 是数组（逐条 {table,id}）；客户端结构要计数
    let conflicts = v
        .get("conflicts")
        .and_then(|c| c.as_array())
        .map(|a| a.len() as i64)
        .unwrap_or(0);
    // 线格式：push 的全局序列号字段叫 seq（pull 才叫 server_seq）
    let server_seq = v.get("seq").and_then(|n| n.as_i64()).unwrap_or(0);
    Ok(PushResult { applied, conflicts, server_seq })
}

/// pull 响应（线格式）→ `PullResult`。行结构不符要报错而不是静默丢行。
fn parse_pull(v: &Value, cursor: i64) -> Result<PullResult, TransportError> {
    if v.get("ok").and_then(|b| b.as_bool()) != Some(true) {
        return Err(TransportError::Rejected("pull 被拒绝（响应 ok=false）".into()));
    }
    let mut rows = Vec::new();
    if let Some(arr) = v.get("rows").and_then(|a| a.as_array()) {
        for r in arr {
            rows.push(serde_json::from_value(r.clone()).map_err(|e| {
                TransportError::Rejected(format!("pull 行结构与 SyncRow 不符：{e}"))
            })?);
        }
    }
    Ok(PullResult {
        rows,
        cursor: v.get("cursor").and_then(|n| n.as_i64()).unwrap_or(cursor),
        has_more: v.get("has_more").and_then(|b| b.as_bool()).unwrap_or(false),
        server_seq: v.get("server_seq").and_then(|n| n.as_i64()).unwrap_or(0),
    })
}

impl Transport for HttpTransport {
    fn login(
        &self,
        base_url: &str,
        username: &str,
        password: &str,
        device_id: &str,
    ) -> Result<LoginResult, TransportError> {
        let body = json!({ "username": username, "password": password, "device_id": device_id });
        let v = self.post_json(&format!("{base_url}/login"), None, &body)?;
        parse_login(&v, "登录")
    }

    fn setup(
        &self,
        base_url: &str,
        username: &str,
        password: &str,
        device_id: &str,
    ) -> Result<LoginResult, TransportError> {
        let body = json!({ "username": username, "password": password, "device_id": device_id });
        match self.post_json(&format!("{base_url}/setup"), None, &body) {
            Ok(v) => parse_login(&v, "首成员初始化"),
            // 409 already initialized：不是故障，是「应改走登录」的明确信号
            Err(TransportError::Rejected(msg)) if msg.contains("already initialized") => {
                Err(TransportError::Rejected(
                    "服务器已完成首成员初始化，请改用登录（login）".into(),
                ))
            }
            Err(e) => Err(e),
        }
    }

    fn push(&self, base_url: &str, token: &str, rows: &[SyncRow]) -> Result<PushResult, TransportError> {
        let body = json!({
            "schema_version": CLIENT_SCHEMA_VERSION,
            "rows": rows,
        });
        let v = self.post_json(&format!("{base_url}/sync/push"), Some(token), &body)?;
        parse_push(&v)
    }

    fn pull(
        &self,
        base_url: &str,
        token: &str,
        cursor: i64,
        limit: usize,
    ) -> Result<PullResult, TransportError> {
        let url = format!("{base_url}/sync/pull?cursor={cursor}&limit={limit}");
        let v = self.get_json(&url, token)?;
        parse_pull(&v, cursor)
    }

    fn logout(&self, base_url: &str, token: &str) -> Result<(), TransportError> {
        let body = json!({});
        let v = self.post_json(&format!("{base_url}/logout"), Some(token), &body)?;
        if v.get("ok").and_then(|b| b.as_bool()) != Some(true) {
            return Err(TransportError::Rejected("logout 被拒绝".into()));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 线格式样本逐字对照 server/src/server.js 的真实响应（2026-11-17）。
    #[test]
    fn push_响应的数组_conflicts_换算为计数() {
        let wire = json!({
            "ok": true,
            "applied": [
                { "table": "work_item", "id": "r1", "decision": "inserted", "seq": 1 },
                { "table": "work_item", "id": "r2", "decision": "local_wins_progress_max", "seq": 2 }
            ],
            "conflicts": [ { "table": "work_item", "id": "r2" } ],
            "seq": 2
        });
        let r = parse_push(&wire).unwrap();
        assert_eq!(r.applied.len(), 2);
        assert_eq!(r.applied[1].decision, "local_wins_progress_max");
        assert_eq!(r.conflicts, 1); // 数组 → 计数
        assert_eq!(r.server_seq, 2); // seq → server_seq
    }

    #[test]
    fn push_无_conflicts_字段时计数为零() {
        let wire = json!({
            "ok": true,
            "applied": [ { "table": "work_item", "id": "r1", "decision": "inserted", "seq": 1 } ],
            "seq": 1
        });
        let r = parse_push(&wire).unwrap();
        assert_eq!(r.conflicts, 0);
    }

    #[test]
    fn pull_行结构与游标解析() {
        let wire = json!({
            "ok": true,
            "rows": [
                {
                    "table": "work_item", "id": "r1",
                    "payload": { "title": "对接网点", "progress_pct": 40 },
                    "updated_at": 1795000000000i64, "updated_by": "m_1",
                    "device_id": "d_1", "deleted_at": null, "seq": 5
                }
            ],
            "cursor": 5, "has_more": false, "server_seq": 5
        });
        let r = parse_pull(&wire, 0).unwrap();
        assert_eq!(r.rows.len(), 1);
        assert_eq!(r.rows[0].id, "r1");
        assert_eq!(r.rows[0].payload.get("title").and_then(|t| t.as_str()), Some("对接网点"));
        assert!(r.rows[0].deleted_at.is_none());
        assert_eq!(r.cursor, 5);
        assert!(!r.has_more);
    }

    #[test]
    fn pull_行结构不符时报错不丢行() {
        let wire = json!({
            "ok": true,
            "rows": [ { "table": "work_item" } ], // 缺 id / payload 等必填字段
            "cursor": 0, "has_more": false, "server_seq": 0
        });
        assert!(parse_pull(&wire, 0).is_err());
    }

    #[test]
    fn login_响应解析() {
        let v = json!({
            "ok": true, "member_id": "m_1", "role": "admin",
            "token": "tok_abc", "device_id": "d_1", "active_tokens": 2
        });
        let r = parse_login(&v, "登录").unwrap();
        assert_eq!(r.member_id, "m_1");
        assert_eq!(r.role, "admin");
        assert_eq!(r.token, "tok_abc");
        assert_eq!(r.active_tokens, 2);
    }

    #[test]
    fn login_无令牌视为契约异常() {
        let v = json!({ "ok": true, "member_id": "m_1", "role": "admin", "token": "" });
        assert!(parse_login(&v, "登录").is_err());
    }
}
