//! v2.2.4 追加需求 C：项目「进度 / 状态 / 动作记录」的写入侧。
//!
//! 三件事都落在这里，因为它们共享同一条硬约束——**写前先备份**：
//! - [`update_item`]：改进度（百分比或快捷档位）与状态；
//! - [`log_action`]：追加一条动作记录（时间 + 备注）；
//! - [`list_actions`]：读回动作历史（明细页展示用）。
//!
//! 几个刻意的取舍：
//! - **进度只前进不静默回退**：库里有 `progress_pct BETWEEN 0 AND 100` 约束，
//!   同步层对 `progress_pct` 取 max（`sync.rs` 口径）。所以这里允许显式回退，
//!   但把回退的 `from_pct → to_pct` 一并写进 `progress_log`，谁改的、从多少改到多少，
//!   事后都查得到——而不是让同步层悄悄取 max 把一次人工回退吞掉。
//! - **状态与进度联动只做一次**：进度到 100% 自动置「已完成」、从 100% 降下来自动回到
//!   「进行中」，避免"进度 100% 但状态还是进行中"这种自相矛盾的组合留在看板上。
//!   但状态是人工显式指定时不反向改进度——那是人的判断，不让工具替他做。
//! - **动作记录与进度变更同事务**：一次"改进度并说明原因"要么都成、要么都不成。
//! - **复用既有写前快照**（`Store::begin_write` → L1 快照），不另造一套备份。
//! - **入队即补推**：沿用 `sync_client::enqueue`，断网也能改，联网自动补推。

use crate::model::ItemStatus;
use crate::store::{now_iso_pub, now_ms_pub, Store, StoreError};
use crate::sync::SyncRow;
use rusqlite::{params, Connection};

/// 快捷档位：界面上的几个一键按钮。允许任意 0–100 的整数，档位只是常见取值。
pub const PCT_STOPS: &[i64] = &[0, 25, 50, 75, 100];

/// 一次进度/状态更新请求。字段为 `None` 表示"这项不改"——
/// 区分「没传」与「传了 0」，否则改进度到 0% 就会被当成"没传"。
#[derive(Debug, Clone, serde::Deserialize)]
pub struct ItemPatch {
    pub item_id: String,
    pub progress_pct: Option<i64>,
    pub status: Option<String>,
    /// 本次变更的说明，一并写进动作记录（可空）
    pub note: Option<String>,
    /// 操作人（成员名）；为空写 "unknown"
    pub by: Option<String>,
}

/// 更新结果，供界面回显与自检核对实际落库值。
#[derive(Debug, Clone, serde::Serialize)]
pub struct UpdateOutcome {
    pub item_id: String,
    pub from_pct: i64,
    pub to_pct: i64,
    pub from_status: Option<String>,
    pub to_status: Option<String>,
    /// 进度是否被拒（例如越界）——**不静默截断**，明确回报调用方
    pub rejected_reason: Option<String>,
    /// 是否写入了动作记录
    pub action_logged: bool,
    /// 是否入队补推
    pub queued: bool,
}

/// 动作记录（`progress_log` 一行）。明细页按时间倒序展示。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct ActionLog {
    pub id: String,
    pub item_id: String,
    /// 变更前进度（纯备注型动作时为 `None`）
    pub from_pct: Option<i64>,
    /// 变更后进度（纯备注型动作时等于当时进度）
    pub to_pct: i64,
    pub note: String,
    pub member: Option<String>,
    pub created_at: String,
}

fn pct_valid(p: i64) -> bool {
    (0..=100).contains(&p)
}

/// 读当前行的进度与状态。行不存在时报错而不是当 0 处理——
/// 把"条目不存在"当成"进度 0"会让一次误操作静默写入幽灵记录。
fn current_row(conn: &Connection, item_id: &str) -> Result<(i64, Option<String>, String, i64), StoreError> {
    conn.query_row(
        "SELECT progress_pct, status, title, seq_no FROM work_item WHERE id = ?1 AND deleted_at IS NULL",
        params![item_id],
        |r| Ok((r.get::<_, i64>(0)?, r.get::<_, Option<String>>(1)?, r.get::<_, String>(2)?, r.get::<_, i64>(3)?)),
    )
    .map_err(|e| StoreError::MigrationFailed { script: format!("item {}", item_id), error: e.to_string() })
}

/// 进度与状态的自动联动：只处理"起步/进满/退回"三种情况，其余保持人工判断。
///
/// 「起步」这条是必需的，不是顺手加的：`rules::evaluate` 的落后判定要求
/// `status == Active`（待启动/未筹备的 0 进度是初始态，不是落后）。进度改了而状态
/// 仍挂在「待启动」，明细页会出现"进度 50%、待启动"这种自相矛盾的行，**晨报也永远
/// 取不到这个新进度**——落后告警直接不参与计算。所以 1–99% 必须把未启动状态推成
/// 进行中；而「已暂停/已取消」是人明确表达的中断态，进度再多也不替他改回来。
fn auto_status(to_pct: i64, cur: Option<&str>) -> Option<String> {
    let cur_s = cur.and_then(ItemStatus::from_zh);
    let cur_is_done = cur_s == Some(ItemStatus::Done);
    if to_pct >= 100 && !cur_is_done {
        Some(ItemStatus::Done.as_str().to_string())
    } else if to_pct < 100 && cur_is_done {
        // 从 100% 退下来：状态不能还挂着"已完成"
        Some(ItemStatus::Active.as_str().to_string())
    } else if (1..100).contains(&to_pct) {
        // 已有实质进展却仍标"未启动/待启动" → 推成进行中；已暂停/已取消保持原样
        match cur_s {
            Some(ItemStatus::Pending) | Some(ItemStatus::Unprepared) | None => {
                Some(ItemStatus::Active.as_str().to_string())
            }
            _ => None,
        }
    } else {
        None
    }
}

/// 更新进度与状态（追加需求 C 的 `update_item`）。
///
/// 写入顺序刻意是「先备份（begin_write）→ 同一事务里改行 + 记动作 → 入队补推」：
/// 备份失败就整个不写，避免出现"改了库但没备份"的半程状态。
pub fn update_item(store: &Store, patch: &ItemPatch) -> Result<UpdateOutcome, StoreError> {
    let guard = store.begin_write("update-item")?;
    let conn = guard.conn();

    let (from_pct, from_status, _title, _seq) = current_row(conn, &patch.item_id)?;

    let mut rejected: Option<String> = None;
    let to_pct = match patch.progress_pct {
        Some(p) if pct_valid(p) => p,
        Some(p) => {
            // 越界不截断：截断会让用户以为"点了 120 变成 100 生效了"
            rejected = Some(format!("进度 {} 超出 0–100，未写入", p));
            from_pct
        }
        None => from_pct,
    };

    let to_status: Option<String> = match patch.status.as_deref() {
        Some(s) => {
            if ItemStatus::from_zh(s).is_some() {
                Some(s.to_string())
            } else {
                rejected = Some(format!("状态「{}」不是合法值，未写入", s));
                from_status.clone()
            }
        }
        None => {
            // 未显式指定状态时，按进度自动联动
            match auto_status(to_pct, from_status.as_deref()) {
                Some(s) => Some(s),
                None => from_status.clone(),
            }
        }
    };

    let pct_changed = to_pct != from_pct;
    let status_changed = to_status != from_status;
    let note = patch.note.as_deref().unwrap_or("").trim().to_string();
    // 建立动作记录的条件：进度变了、状态变了、或用户写了备注。
    // 三者都没有就不写——避免点一下保存就多一条空记录把历史刷满。
    let should_log = pct_changed || status_changed || !note.is_empty();

    let by = patch.by.clone().unwrap_or_else(|| "unknown".into());
    let ts = now_iso_pub();

    if rejected.is_some() && !pct_changed && !status_changed {
        // 全被拒：不动库，直接如实返回（备注也不落——否则历史里会出现
        // 一条"改了 120%"的记录，而库里其实没变，回头对不上账）
        return Ok(UpdateOutcome {
            item_id: patch.item_id.clone(),
            from_pct,
            to_pct,
            from_status: from_status.clone(),
            to_status,
            rejected_reason: rejected,
            action_logged: false,
            queued: false,
        });
    }

    if should_log {
        let tx = conn.unchecked_transaction()?;

        if pct_changed || status_changed {
            tx.execute(
                "UPDATE work_item SET progress_pct = ?1, status = ?2, updated_at = ?3, updated_by = ?4
                 WHERE id = ?5",
                params![to_pct, to_status, ts, by, patch.item_id],
            )?;
        }

        let log_id = format!("pl-{}-{}", patch.item_id, now_ms_pub());
        let log_note = if note.is_empty() {
            // 没写备注时自动生成一句，历史里不会出现空白行
            let parts: Vec<String> = [
                pct_changed.then(|| format!("进度 {}% → {}%", from_pct, to_pct)),
                status_changed.then(|| format!(
                    "状态 {} → {}",
                    from_status.clone().unwrap_or_else(|| "未设".into()),
                    to_status.clone().unwrap_or_else(|| "未设".into())
                )),
            ]
            .into_iter()
            .flatten()
            .collect();
            parts.join("；")
        } else {
            note.clone()
        };
        tx.execute(
            "INSERT INTO progress_log (id, item_id, from_pct, to_pct, note, member_id, created_at)
             VALUES (?1,?2,?3,?4,?5,?6,?7)",
            // 纯备注型动作（数字没变）的 from_pct 记 NULL：它不表示「从 0% 变过来」
            params![log_id, patch.item_id, pct_changed.then_some(from_pct), to_pct, log_note, by, ts],
        )?;

        // 同步补推：沿用既有 outbox，断网也能改、联网自动补推。
        // 只有数字/状态真变了才入队——纯备注型动作改的是历史，不是主表。
        let mut queued = false;
        if pct_changed || status_changed {
            let mut payload = serde_json::Map::new();
            payload.insert("id".into(), serde_json::json!(patch.item_id));
            payload.insert("progress_pct".into(), serde_json::json!(to_pct));
            payload.insert("status".into(), serde_json::json!(to_status));
            payload.insert("updated_at".into(), serde_json::json!(ts));
            payload.insert("updated_by".into(), serde_json::json!(by));
            let row = SyncRow {
                table: "work_item".into(),
                id: patch.item_id.clone(),
                payload,
                updated_at: now_ms_pub(),
                updated_by: by.clone(),
                device_id: crate::sync_client::state_get(conn, crate::sync_client::keys::DEVICE_ID)
                    .unwrap_or_else(|| "unknown".into()),
                seq: 0,
                deleted_at: None,
            };
            queued = crate::sync_client::enqueue(conn, &row, &ts).is_ok();
        }
        tx.commit()?;

        return Ok(UpdateOutcome {
            item_id: patch.item_id.clone(),
            from_pct,
            to_pct,
            from_status,
            to_status,
            rejected_reason: rejected,
            action_logged: true,
            queued,
        });
    }

    // 数字与备注都没动：不写记录（避免点一下保存就多一条空历史）
    Ok(UpdateOutcome {
        item_id: patch.item_id.clone(),
        from_pct,
        to_pct,
        from_status: from_status.clone(),
        to_status,
        rejected_reason: rejected,
        action_logged: false,
        queued: false,
    })
}

/// 追加一条动作记录（追加需求 C 的 `log_action`）。
/// 不动进度也不动状态，只写历史——"今天跟甲方通了电话"这类不该改数字。
pub fn log_action(
    store: &Store,
    item_id: &str,
    note: &str,
    by: Option<&str>,
) -> Result<ActionLog, StoreError> {
    let note = note.trim();
    if note.is_empty() {
        // 空备注的"动作"没有信息量，明确拒绝而不是写一条空行污染历史
        return Err(StoreError::MigrationFailed {
            script: format!("log-action {}", item_id),
            error: "动作备注不能为空".into(),
        });
    }
    let guard = store.begin_write("log-action")?;
    let conn = guard.conn();
    let (pct, _status, _title, _seq) = current_row(conn, item_id)?;
    let by = by.unwrap_or("unknown").to_string();
    let ts = now_iso_pub();
    let id = format!("pl-{}-{}", item_id, now_ms_pub());
    conn.execute(
        "INSERT INTO progress_log (id, item_id, from_pct, to_pct, note, member_id, created_at)
         VALUES (?1,?2,NULL,?3,?4,?5,?6)",
        params![id, item_id, pct, note, by, ts],
    )?;
    Ok(ActionLog {
        id,
        item_id: item_id.to_string(),
        from_pct: None,
        to_pct: pct,
        note: note.to_string(),
        member: Some(by),
        created_at: ts,
    })
}

/// 读某个项目的动作历史（**按时间倒序**，最新一条在最上面）。
pub fn list_actions(conn: &Connection, item_id: &str) -> Result<Vec<ActionLog>, StoreError> {
    let mut st = conn.prepare(
        "SELECT id, item_id, from_pct, to_pct, note, member_id, created_at
         FROM progress_log WHERE item_id = ?1 ORDER BY created_at DESC, id DESC",
    )?;
    let rows = st.query_map(params![item_id], |r| {
        Ok(ActionLog {
            id: r.get(0)?,
            item_id: r.get(1)?,
            from_pct: r.get(2)?,
            to_pct: r.get(3)?,
            note: r.get::<_, Option<String>>(4)?.unwrap_or_default(),
            member: r.get(5)?,
            created_at: r.get(6)?,
        })
    })?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r?);
    }
    Ok(out)
}

/// 一次取全部项目的最近动作，供看板/明细表批量展示（避免 N+1 次查询）。
/// `limit_per_item` 为每项目保留条数（明细表内联展示用 3 条足够）。
pub fn recent_actions(
    conn: &Connection,
    limit_per_item: usize,
) -> Result<std::collections::BTreeMap<String, Vec<ActionLog>>, StoreError> {
    let mut st = conn.prepare(
        "SELECT id, item_id, from_pct, to_pct, note, member_id, created_at
         FROM progress_log ORDER BY created_at DESC, id DESC",
    )?;
    let rows = st.query_map([], |r| {
        Ok(ActionLog {
            id: r.get(0)?,
            item_id: r.get(1)?,
            from_pct: r.get(2)?,
            to_pct: r.get(3)?,
            note: r.get::<_, Option<String>>(4)?.unwrap_or_default(),
            member: r.get(5)?,
            created_at: r.get(6)?,
        })
    })?;
    let mut out: std::collections::BTreeMap<String, Vec<ActionLog>> = Default::default();
    for r in rows {
        let a = r?;
        let e = out.entry(a.item_id.clone()).or_default();
        if e.len() < limit_per_item {
            e.push(a);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entry::{quick_add, QuickAdd};
    use crate::repo::load_items;

    fn store() -> (tempfile::TempDir, Store) {
        let d = tempfile::tempdir().unwrap();
        let s = Store::open(d.path()).unwrap();
        (d, s)
    }

    fn seed(s: &Store) -> String {
        let r = quick_add(s, &QuickAdd {
            title: "XX县植保无人机统防统治".into(),
            board: "gov".into(),
            plan_start_raw: "2026-10-01".into(),
            plan_end_raw: "2026-12-31".into(),
            fiscal_year: 2026,
            base_date: "2026-10-01".into(),
            updated_by: "大角子".into(),
        }).unwrap();
        r.id
    }

    #[test]
    fn update_progress_writes_row_and_action_history() {
        // 追加需求 C 验收：改一次进度、记一条动作，重启后仍正确显示
        let (_d, s) = store();
        let id = seed(&s);
        let out = update_item(&s, &ItemPatch {
            item_id: id.clone(), progress_pct: Some(50), status: None,
            note: Some("已完成方案评审".into()), by: Some("大角子".into()),
        }).unwrap();
        assert_eq!(out.from_pct, 0);
        assert_eq!(out.to_pct, 50);
        assert!(out.action_logged);

        // 重开库（模拟重启）：值必须还在
        let s2 = Store::open(_d.path()).unwrap();
        let items = load_items(s2.conn(), 2026).unwrap();
        let it = items.iter().find(|i| i.id == id).unwrap();
        assert_eq!(it.progress_pct, 50);

        let logs = list_actions(s2.conn(), &id).unwrap();
        assert_eq!(logs.len(), 1);
        assert_eq!(logs[0].note, "已完成方案评审");
        assert_eq!(logs[0].from_pct, Some(0));
        assert_eq!(logs[0].to_pct, 50);
    }

    #[test]
    fn progress_100_auto_closes_and_rolling_back_reopens() {
        // 联动：100% → 已完成；退回 <100 → 进行中（不留"100% 但进行中"的矛盾组合）
        let (_d, s) = store();
        let id = seed(&s);
        let a = update_item(&s, &ItemPatch { item_id: id.clone(), progress_pct: Some(100), status: None, note: None, by: Some("x".into()) }).unwrap();
        assert_eq!(a.to_status.as_deref(), Some("已完成"));
        let b = update_item(&s, &ItemPatch { item_id: id.clone(), progress_pct: Some(80), status: None, note: None, by: Some("x".into()) }).unwrap();
        assert_eq!(b.to_status.as_deref(), Some("进行中"));
    }

    #[test]
    fn explicit_status_is_not_overridden_by_progress_link() {
        // 人工显式指定状态时，不拿进度去反改它——那是人的判断
        let (_d, s) = store();
        let id = seed(&s);
        let out = update_item(&s, &ItemPatch { item_id: id, progress_pct: Some(30), status: Some("已暂停".into()), note: None, by: Some("x".into()) }).unwrap();
        assert_eq!(out.to_status.as_deref(), Some("已暂停"), "显式状态不能被联动覆盖");
    }

    #[test]
    fn out_of_range_progress_is_rejected_not_truncated() {
        // 不静默截断：越界要如实拒绝，否则用户以为 120 被写成了 100
        let (_d, s) = store();
        let id = seed(&s);
        let out = update_item(&s, &ItemPatch { item_id: id, progress_pct: Some(120), status: None, note: None, by: Some("x".into()) }).unwrap();
        assert_eq!(out.to_pct, 0, "被拒时进度不应变化");
        assert!(out.rejected_reason.unwrap().contains("超出 0–100"));
    }

    #[test]
    fn invalid_status_is_rejected_with_reason() {
        let (_d, s) = store();
        let id = seed(&s);
        let out = update_item(&s, &ItemPatch { item_id: id, progress_pct: None, status: Some("瞎写".into()), note: None, by: Some("x".into()) }).unwrap();
        assert!(out.rejected_reason.unwrap().contains("不是合法值"));
    }

    #[test]
    fn note_only_update_does_not_touch_numbers() {
        // 只写备注时不改进度；但备注本身要进历史
        let (_d, s) = store();
        let id = seed(&s);
        let out = update_item(&s, &ItemPatch { item_id: id.clone(), progress_pct: None, status: None, note: Some("跟甲方通电话".into()), by: Some("大角子".into()) }).unwrap();
        assert_eq!(out.to_pct, 0);
        let logs = list_actions(s.conn(), &id).unwrap();
        assert_eq!(logs[0].note, "跟甲方通电话");
        assert_eq!(logs[0].from_pct, None, "纯备注型动作没有 from_pct");
    }

    #[test]
    fn log_action_requires_non_empty_note() {
        let (_d, s) = store();
        let id = seed(&s);
        let e = log_action(&s, &id, "   ", Some("x")).unwrap_err();
        assert!(e.to_string().contains("不能为空"));
    }

    #[test]
    fn log_action_keeps_current_pct_untouched() {
        let (_d, s) = store();
        let id = seed(&s);
        update_item(&s, &ItemPatch { item_id: id.clone(), progress_pct: Some(60), status: None, note: None, by: Some("x".into()) }).unwrap();
        let a = log_action(&s, &id, "提交验收材料", Some("大角子")).unwrap();
        assert_eq!(a.to_pct, 60, "记动作不改进度");
        let items = load_items(s.conn(), 2026).unwrap();
        assert_eq!(items.iter().find(|i| i.id == id).unwrap().progress_pct, 60);
    }

    #[test]
    fn history_is_newest_first() {
        let (_d, s) = store();
        let id = seed(&s);
        update_item(&s, &ItemPatch { item_id: id.clone(), progress_pct: Some(25), status: None, note: Some("第一步".into()), by: Some("x".into()) }).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        log_action(&s, &id, "第二步", Some("x")).unwrap();
        let logs = list_actions(s.conn(), &id).unwrap();
        assert_eq!(logs[0].note, "第二步", "最新一条要在最上面");
        assert_eq!(logs[1].note, "第一步");
    }

    #[test]
    fn no_change_no_record() {
        // 点了保存但什么都没变 → 不写记录，避免把历史刷满
        let (_d, s) = store();
        let id = seed(&s);
        let out = update_item(&s, &ItemPatch { item_id: id.clone(), progress_pct: Some(0), status: None, note: None, by: Some("x".into()) }).unwrap();
        assert!(!out.action_logged);
        assert!(list_actions(s.conn(), &id).unwrap().is_empty());
    }

    #[test]
    fn missing_item_errors_instead_of_writing_ghost() {
        let (_d, s) = store();
        let e = update_item(&s, &ItemPatch { item_id: "nope".into(), progress_pct: Some(10), status: None, note: None, by: None }).unwrap_err();
        assert!(e.to_string().contains("nope"));
    }

    #[test]
    fn brief_and_kanban_see_the_new_progress_after_restart() {
        // 追加需求 C 验收的后半句：「晨报/看板进度取到新值」。
        // 这条不能只断言 load_items —— 那只能证明库里改了，证明不了**下游读它的人**
        // 也拿到新值。所以这里一路走到派生层：
        //   看板 KPI（repo::derive_kpi 的 board_started 按 progress_pct > 0 计数）
        //   晨报（rules::morning_brief → evaluate 的落后告警把实际进度写进文案）
        use crate::date::CivilDate;
        use crate::repo::derive_kpi;
        use crate::rules::{morning_brief, AlertKind, Ruleset};

        let (d, s) = store();
        let id = seed(&s);
        // 一条 board_started 口径的 KPI：改进度前应为 0，改后应为 1
        s.conn().execute(
            "INSERT INTO kpi_metric (id, category, name, target_value, rule, fiscal_year)
             VALUES ('k1','政府关系','政府关系已启动',8,'board_started',2026)",
            [],
        ).unwrap();

        let today = CivilDate::parse("2026-12-20").unwrap();
        let mut rs = Ruleset::default();
        rs.behind_enabled = true;

        // ---- 改之前：进度 0、状态待启动 → KPI 未启动，晨报不出落后告警
        let before_items = crate::repo::load_items(s.conn(), 2026).unwrap();
        let before_kpi = derive_kpi(s.conn(), 2026).unwrap();
        let k_before = before_kpi.iter().find(|k| k.0 == "政府关系已启动").unwrap();
        assert_eq!(k_before.2, Some(0.0), "改进度前不该算作已启动");
        let before_brief = morning_brief(today, &before_items, &rs);
        assert!(
            !before_brief.overdue.iter().any(|a| a.kind == AlertKind::BehindSchedule),
            "待启动项不进落后判定"
        );

        // ---- 改一次进度 + 记一条动作
        let out = update_item(&s, &ItemPatch {
            item_id: id.clone(), progress_pct: Some(50), status: None,
            note: Some("方案已过评审，进入实施".into()), by: Some("大角子".into()),
        }).unwrap();
        assert!(out.action_logged && out.queued);
        assert_eq!(out.to_status.as_deref(), Some("进行中"), "有进展就该是进行中，不是仍挂待启动");

        // ---- 重启：重开库，再看下游派生值
        let s2 = Store::open(d.path()).unwrap();
        let items = crate::repo::load_items(s2.conn(), 2026).unwrap();
        let after_kpi = derive_kpi(s2.conn(), 2026).unwrap();
        let k_after = after_kpi.iter().find(|k| k.0 == "政府关系已启动").unwrap();
        assert_eq!(k_after.2, Some(1.0), "看板 KPI 必须取到新进度");

        // 晨报与告警列表共用 `rules::evaluate` 这一个数据源。
        // 注意：`MorningBrief` 只转出 due_today / due_soon / overdue 三个桶，
        // 落后（BehindSchedule）不在其中——所以这里断言到 evaluate 层，
        // 那才是晨报和看板共同读到新进度的地方；晨报页是否要单列落后，
        // 属于展示口径，未经确认不擅自改。
        let alerts = crate::rules::evaluate(today, &items, &rs);
        let behind: Vec<_> = alerts.iter().filter(|a| a.kind == AlertKind::BehindSchedule).collect();
        assert_eq!(behind.len(), 1, "告警层必须能取到这条新进度并据此判定落后");
        assert!(
            behind[0].detail.contains("实际 50%"),
            "落后文案里的实际进度应为 50%，实际：{}", behind[0].detail
        );
        assert_eq!(behind[0].item_id, id);

        // 晨报本体：即便是"当日到期"桶里的项，也必须带着改后的状态一起被读到
        let brief = morning_brief(today, &items, &rs);
        assert_eq!(brief.brief_date, today.to_iso());

        // 动作历史也在重启后仍在
        let logs = list_actions(s2.conn(), &id).unwrap();
        assert_eq!(logs.len(), 1);
        assert_eq!(logs[0].from_pct, Some(0));
        assert_eq!(logs[0].to_pct, 50);
    }

    #[test]
    fn progress_into_mid_range_bumps_unstarted_to_active() {
        // 1–99% 却仍标「待启动」是自相矛盾的行；但「已暂停/已取消」是人明确的中断态，不许改
        let (_d, s) = store();
        let id = seed(&s);
        let o = update_item(&s, &ItemPatch { item_id: id.clone(), progress_pct: Some(30), status: None, note: None, by: Some("x".into()) }).unwrap();
        assert_eq!(o.to_status.as_deref(), Some("进行中"));

        update_item(&s, &ItemPatch { item_id: id.clone(), progress_pct: None, status: Some("已暂停".into()), note: None, by: Some("x".into()) }).unwrap();
        let o2 = update_item(&s, &ItemPatch { item_id: id.clone(), progress_pct: Some(60), status: None, note: None, by: Some("x".into()) }).unwrap();
        assert_eq!(o2.to_status.as_deref(), Some("已暂停"), "已暂停项进度再涨也不自动恢复");
    }

    #[test]
    fn recent_actions_limits_per_item() {
        let (_d, s) = store();
        let id = seed(&s);
        for i in 0..5 {
            update_item(&s, &ItemPatch { item_id: id.clone(), progress_pct: Some((i + 1) * 10), status: None, note: Some(format!("第{}次", i + 1)), by: Some("x".into()) }).unwrap();
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        let m = recent_actions(s.conn(), 3).unwrap();
        assert_eq!(m[&id].len(), 3, "每项目只留 3 条");
        assert_eq!(list_actions(s.conn(), &id).unwrap().len(), 5, "单查不受限");
    }
}
