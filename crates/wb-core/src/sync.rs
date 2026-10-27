//! 同步合并：条目级 LWW + 定向例外（架构 §4.14）。
//!
//! 这一层刻意只做**合并判定**，不碰网络与数据库——网络与游标在 Rust 传输层，
//! 合并规则是纯函数，才能被单测穷举（架构 §3.1 的判据：改合并规则要不要重新发版）。
//!
//! 三条不可妥协的口径：
//! 1. **主策略行级 LWW**，比较 `updated_at`；同刻并列按 `updated_by` + 设备 ID 确定性决胜，
//!    保证所有端收敛到同一结果。
//! 2. **`progress_pct` 取 max**：进度只会前进；确需回退时走人工确认（业务操作），不靠同步覆盖。
//! 3. **被覆盖的旧值进 `sync_conflict_log`，永久可查**——这是「我的修改消失了」的唯一救济入口。

use serde::{Deserialize, Serialize};

/// 参与同步的行。字段刻意与 `work_item` 主干对齐，便于直接映射。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyncRow {
    /// 表名，如 `work_item`
    pub table: String,
    /// 主键
    pub id: String,
    /// 整行数据（列名 → 值），LWW 的合并单元
    pub payload: serde_json::Map<String, serde_json::Value>,
    /// 毫秒级时间戳；LWW 比较依据
    pub updated_at: i64,
    pub updated_by: String,
    /// 设备 ID：同刻并列时的第二决胜键
    pub device_id: String,
    /// 服务端分配的全局递增序列号（pull 增量依据）
    pub seq: i64,
    /// 软删除标记（架构 §4.14：删除一律软删参与同步）
    pub deleted_at: Option<i64>,
}

/// 一次合并的判定结果，供冲突日志与自检记录。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MergeDecision {
    /// 本地行胜出（本地更新）
    LocalWins,
    /// 远端行胜出（远端更新）
    RemoteWins,
    /// 完全一致，无需动作
    Identical,
    /// `progress_pct` 取 max 导致混合结果：远端行胜出，但进度取双方较大值
    ProgressMaxApplied,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MergeOutcome {
    pub decision: MergeDecision,
    /// 合并后的行（调用方据此写库）
    pub merged: SyncRow,
    /// 被冲掉的旧值；`None` 表示没有值被覆盖
    pub overwritten: Option<SyncRow>,
    /// 冲突日志应写的内容（人可读，进 sync_conflict_log）
    pub conflict_note: Option<String>,
}

/// 确定性比较：先 `updated_at`，同刻比 `updated_by`，再比 `device_id`。
/// 返回值 >0 表示 `a` 胜出。
fn compare_version(a: &SyncRow, b: &SyncRow) -> std::cmp::Ordering {
    a.updated_at
        .cmp(&b.updated_at)
        .then_with(|| a.updated_by.cmp(&b.updated_by))
        .then_with(|| a.device_id.cmp(&b.device_id))
}

fn progress_of(row: &SyncRow) -> Option<i64> {
    row.payload.get("progress_pct").and_then(|v| v.as_i64())
}

/// 合并一行。`local` 是本地已有行，`remote` 是服务端/对端来的行。
pub fn merge_row(local: &SyncRow, remote: &SyncRow) -> MergeOutcome {
    // 表或主键不一致属于调用方错误；这里以 local 为准并说明，不静默合并
    if local.table != remote.table || local.id != remote.id {
        return MergeOutcome {
            decision: MergeDecision::Identical,
            merged: local.clone(),
            overwritten: None,
            conflict_note: Some(format!(
                "合并请求的行标识不一致（{}#{} vs {}#{}），已拒绝合并",
                local.table, local.id, remote.table, remote.id
            )),
        };
    }

    if local.payload == remote.payload && local.deleted_at == remote.deleted_at {
        return MergeOutcome {
            decision: MergeDecision::Identical,
            merged: local.clone(),
            overwritten: None,
            conflict_note: None,
        };
    }

    let (winner, loser, decision) = match compare_version(local, remote) {
        std::cmp::Ordering::Greater => (local, remote, MergeDecision::LocalWins),
        std::cmp::Ordering::Less => (remote, local, MergeDecision::RemoteWins),
        std::cmp::Ordering::Equal => {
            // 完全同刻同人同设备但内容不同：不可判定，保留本地并标记待人工确认
            return MergeOutcome {
                decision: MergeDecision::LocalWins,
                merged: local.clone(),
                overwritten: None,
                conflict_note: Some(format!(
                    "{}#{} 版本戳完全相同但内容不一致，无法自动判定，已保留本地值并标记人工确认",
                    local.table, local.id
                )),
            };
        }
    };

    let mut merged = winner.clone();
    let mut decision = decision;

    // 定向例外：progress_pct 取 max（架构 §4.14）
    //
    // 只有「取 max 真的改写了胜出方的进度」才算应用了例外。若 max 恰好等于胜出方原值，
    // 合并结果就是纯 LWW，决策标签不该被改写——否则每一次正常覆盖都会被误记成例外，
    // 冲突日志与自检数据都会失去分辨力。
    let (lp, rp) = (progress_of(local), progress_of(remote));
    if let (Some(a), Some(b)) = (lp, rp) {
        let m = a.max(b);
        if progress_of(&merged) != Some(m) {
            merged.payload.insert("progress_pct".into(), serde_json::json!(m));
            decision = MergeDecision::ProgressMaxApplied;
        }
    }

    let note = format!(
        "{}#{} 行级 LWW：胜出方 {}（{}，{}），被覆盖值来自 {}（{}，{}）",
        local.table,
        local.id,
        merged.updated_by,
        merged.updated_at,
        merged.device_id,
        loser.updated_by,
        loser.updated_at,
        loser.device_id
    );

    MergeOutcome {
        decision,
        merged,
        overwritten: Some(loser.clone()),
        conflict_note: Some(note),
    }
}

/// 冲突日志行（落 `sync_conflict_log`）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConflictLogEntry {
    pub table_name: String,
    pub row_id: String,
    pub losing_json: String,
    pub losing_by: String,
    pub winning_by: String,
    pub resolved_at: i64,
}

impl MergeOutcome {
    /// 需要写冲突日志时返回条目；无覆盖发生时返回 `None`。
    pub fn conflict_entry(&self, resolved_at: i64) -> Option<ConflictLogEntry> {
        let loser = self.overwritten.as_ref()?;
        Some(ConflictLogEntry {
            table_name: loser.table.clone(),
            row_id: loser.id.clone(),
            losing_json: serde_json::to_string(&loser.payload).unwrap_or_default(),
            losing_by: loser.updated_by.clone(),
            winning_by: self.merged.updated_by.clone(),
            resolved_at,
        })
    }
}

/// pull 增量筛选：只取 `seq` 大于游标的行。
pub fn select_delta(rows: &[SyncRow], cursor: i64) -> Vec<SyncRow> {
    let mut out: Vec<SyncRow> = rows.iter().filter(|r| r.seq > cursor).cloned().collect();
    out.sort_by_key(|r| r.seq);
    out
}

/// 服务端合并一批 push 行：按主键分组，逐组 LWW。
/// 返回（写入结果列表、需记录的冲突条目）。
pub fn merge_batch(
    existing: &[SyncRow],
    incoming: &[SyncRow],
    resolved_at: i64,
) -> (Vec<MergeOutcome>, Vec<ConflictLogEntry>) {
    let mut outcomes = Vec::new();
    let mut conflicts = Vec::new();
    for inc in incoming {
        match existing.iter().find(|e| e.table == inc.table && e.id == inc.id) {
            Some(local) => {
                let o = merge_row(local, inc);
                if let Some(c) = o.conflict_entry(resolved_at) {
                    conflicts.push(c);
                }
                outcomes.push(o);
            }
            None => {
                // 服务端首次见到的行：直接接受
                outcomes.push(MergeOutcome {
                    decision: MergeDecision::RemoteWins,
                    merged: inc.clone(),
                    overwritten: None,
                    conflict_note: None,
                });
            }
        }
    }
    (outcomes, conflicts)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(id: &str, ts: i64, by: &str, dev: &str, pct: i64) -> SyncRow {
        let mut payload = serde_json::Map::new();
        payload.insert("title".into(), serde_json::json!("某工作项"));
        payload.insert("progress_pct".into(), serde_json::json!(pct));
        SyncRow {
            table: "work_item".into(),
            id: id.into(),
            payload,
            updated_at: ts,
            updated_by: by.into(),
            device_id: dev.into(),
            seq: 0,
            deleted_at: None,
        }
    }

    #[test]
    fn newer_timestamp_wins() {
        let local = row("i1", 1000, "m1", "d1", 10);
        let remote = row("i1", 2000, "m2", "d2", 20);
        let o = merge_row(&local, &remote);
        assert_eq!(o.decision, MergeDecision::RemoteWins);
        assert_eq!(o.merged.updated_by, "m2");
        assert!(o.overwritten.is_some());
    }

    #[test]
    fn progress_takes_max_even_when_local_loses() {
        // 远端更新但进度更低：进度不得回退
        let local = row("i1", 2000, "m1", "d1", 60);
        let remote = row("i1", 3000, "m2", "d2", 40);
        let o = merge_row(&local, &remote);
        assert_eq!(o.decision, MergeDecision::ProgressMaxApplied);
        assert_eq!(progress_of(&o.merged), Some(60));
        // 其余字段仍是远端（后写）的值
        assert_eq!(o.merged.updated_by, "m2");
    }

    #[test]
    fn tie_breaks_deterministically_by_member_then_device() {
        let a = row("i1", 5000, "m1", "dA", 10);
        let b = row("i1", 5000, "m2", "dB", 20);
        // 无论调用顺序，胜出方一致
        let o1 = merge_row(&a, &b);
        let o2 = merge_row(&b, &a);
        assert_eq!(o1.merged.updated_by, o2.merged.updated_by);
        assert_eq!(o1.merged.updated_by, "m2");
    }

    #[test]
    fn identical_rows_produce_no_conflict_log() {
        let a = row("i1", 1000, "m1", "d1", 10);
        let o = merge_row(&a, &a.clone());
        assert_eq!(o.decision, MergeDecision::Identical);
        assert!(o.overwritten.is_none());
        assert!(o.conflict_entry(1).is_none());
    }

    #[test]
    fn overwritten_value_is_preserved_for_traceability() {
        // 两行内容必须真的不同（例如远端改了标题），否则本就没有"被覆盖的值"可存
        let local = row("i1", 1000, "m1", "d1", 10);
        let mut remote = row("i1", 9000, "m2", "d2", 10);
        remote
            .payload
            .insert("title".into(), serde_json::json!("某工作项（远端改过标题）"));
        let o = merge_row(&local, &remote);
        assert_eq!(o.decision, MergeDecision::RemoteWins);
        let e = o.conflict_entry(12_000).expect("必须有冲突记录");
        // 存档的是**被冲掉的本地值**，不是远端的值
        assert!(e.losing_json.contains("某工作项"), "被覆盖值应在日志里：{}", e.losing_json);
        assert!(!e.losing_json.contains("远端改过标题"), "日志不该存胜出值");
        assert_eq!(e.losing_by, "m1");
        assert_eq!(e.winning_by, "m2");
    }

    #[test]
    fn delta_selection_is_ordered_and_incremental() {
        let mut a = row("a", 1, "m", "d", 0);
        a.seq = 30;
        let mut b = row("b", 1, "m", "d", 0);
        b.seq = 10;
        let mut c = row("c", 1, "m", "d", 0);
        c.seq = 20;
        let d = select_delta(&[a, b, c], 15);
        assert_eq!(d.len(), 2);
        assert_eq!(d[0].seq, 20);
        assert_eq!(d[1].seq, 30);
    }
}
