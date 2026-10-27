//! 手工快速录入（PRD F1：「不定时输入计划和排期」）。
//!
//! 质检把 F1 从 P2 升为 P1：没有桌面入口，用户最核心的诉求就是空的。这一层只做
//! **最小形态**——标题 + 板块 + 计划时间文本（如「下月3号」）→ 复用 `date` 的规范化管道入库。
//!
//! 三条口径：
//! 1. **不做静默猜测**：解析不出就落 `unknown` 并进待确认队列，宁可标记也不替用户编日期；
//! 2. **入口只写一处**：录入与导入走同一张 `work_item`、同一套 `updated_at/updated_by`，
//!    否则同步的 LWW 会拿不到版本戳；
//! 3. **入队同步**：录入即入 `sync_outbox`，断网也照常交付给用户，联网后自动补推。

use rusqlite::{params, Connection};

use crate::date::{self, NormalizedDate, Precision, RelativeBase, Role};
use crate::model::Board;
use crate::store::StoreError;
use crate::sync::SyncRow;
use crate::sync_client;

/// 一条快速录入请求。字段刻意少：首版只要「能记下来且时间不丢」。
#[derive(Debug, Clone)]
pub struct QuickAdd {
    pub title: String,
    /// 板块代码（`gov`/`industry`/`media`/`event`/`customer`/`policy`）
    pub board: String,
    /// 计划时间原文，如「下月3号」「3月中旬」「Q4」「持续」
    pub plan_start_raw: String,
    /// 可选闭环原文；留空表示只记一个时间点
    pub plan_end_raw: String,
    /// 计划年度。**必须显式传**，不从系统时钟推断（与 `date::normalize` 同一条纪律）
    pub fiscal_year: i32,
    /// 录入基准日（`YYYY-MM-DD`）：解析「下月3号」这类相对写法用。
    /// **显式传入而非读系统时钟**，否则单测不可复现、跨零点会出两个结果。
    pub base_date: String,
    /// 录入人（成员 ID），进 `updated_by`，参与 LWW 决胜
    pub updated_by: String,
}

/// 录入结果：一部分是落库后的事实，一部分是给界面看的提示。
#[derive(Debug, Clone)]
pub struct QuickAddResult {
    pub id: String,
    pub seq_no: i64,
    /// 规范化后的起点（`YYYY-MM-DD`），无法解析时为 `None`
    pub plan_start_date: Option<String>,
    pub plan_start_precision: String,
    pub plan_end_date: Option<String>,
    pub plan_end_precision: String,
    /// 进入待确认队列的原因（日期解析不出时非空）——界面必须显示，不许静默吞掉
    pub needs_confirm: Vec<String>,
    /// 本次录入是否已进同步队列
    pub queued: bool,
}

fn now_iso() -> String {
    crate::store::now_iso_pub()
}

/// 生成条目 ID。带年度与序号，便于人工排查；冲突时由调用方重试（概率极低）。
fn make_id(fiscal_year: i32, seq_no: i64) -> String {
    format!("fy{}-m{:04}", fiscal_year, seq_no)
}

/// 下一条目的 `seq_no`：按年度取 max+1（`work_item` 上有 UNIQUE(fiscal_year, seq_no)）。
fn next_seq_no(conn: &Connection, fiscal_year: i32) -> Result<i64, StoreError> {
    let n: Option<i64> = conn
        .query_row(
            "SELECT MAX(seq_no) FROM work_item WHERE fiscal_year = ?1",
            params![fiscal_year],
            |r| r.get(0),
        )
        .ok()
        .flatten();
    Ok(n.unwrap_or(0) + 1)
}

fn precision_str(p: Precision) -> &'static str {
    p.as_str()
}

/// 把自然语言时间文本规范化为（date, precision, raw）。
fn normalize_one(raw: &str, fiscal_year: i32, role: Role, base: RelativeBase) -> NormalizedDate {
    date::normalize_with_base(raw, fiscal_year, role, base)
}

/// 快速录入一条工作项。
///
/// 校验顺序刻意如此：先看必填与板块（用户能自己改），再看日期（能进待确认队列），
/// 最后才动数据库——不要在数据不合规时先备份一次库。
pub fn quick_add(store: &crate::store::Store, req: &QuickAdd) -> Result<QuickAddResult, StoreError> {
    let title = req.title.trim();
    if title.is_empty() {
        return Err(StoreError::Io("标题不能为空".into()));
    }
    let board = Board::from_code(&req.board)
        .or_else(|| Board::from_zh(&req.board))
        .ok_or_else(|| StoreError::Io(format!("未知板块：{}", req.board)))?;

    let base = RelativeBase::from_iso(&req.base_date).ok_or_else(|| {
        StoreError::Io(format!("录入基准日不合法：{}（需 YYYY-MM-DD）", req.base_date))
    })?;

    let (start, end) = if req.plan_end_raw.trim().is_empty() {
        (normalize_one(&req.plan_start_raw, req.fiscal_year, Role::Start, base), None)
    } else {
        let (s, e) = date::normalize_pair_with_base(
            &req.plan_start_raw,
            &req.plan_end_raw,
            req.fiscal_year,
            base,
        );
        (s, Some(e))
    };

    let mut needs_confirm = Vec::new();
    if start.precision == Precision::Unknown {
        needs_confirm.push(format!(
            "计划时间「{}」未能识别，已进入待确认队列（不替你做猜测）",
            req.plan_start_raw.trim()
        ));
    }
    if let Some(e) = end.as_ref() {
        if e.precision == Precision::Unknown {
            needs_confirm.push(format!(
                "闭环时间「{}」未能识别，已进入待确认队列",
                req.plan_end_raw.trim()
            ));
        }
        if let Some(reason) = e.reason.as_deref() {
            if e.precision != Precision::Unknown {
                needs_confirm.push(reason.to_string());
            }
        }
    }

    let guard = store.begin_write("quick-add")?;
    let conn = guard.conn();

    let seq_no = next_seq_no(conn, req.fiscal_year)?;
    let id = make_id(req.fiscal_year, seq_no);
    let ts = now_iso();

    let end_date = end.as_ref().and_then(|e| e.date.clone());
    let end_precision = end
        .as_ref()
        .map(|e| precision_str(e.precision))
        .unwrap_or_else(|| precision_str(Precision::Unknown));
    let end_raw = end.as_ref().map(|e| e.raw.clone()).filter(|s| !s.trim().is_empty());

    conn.execute(
        "INSERT INTO work_item (
            id, seq_no, board_code, title, category, priority, target_quant,
            planned_start_date, planned_start_precision, planned_start_raw,
            planned_end_date, planned_end_precision, planned_end_raw,
            progress_pct, status, milestone_stage, next_action, owner, source_sheet,
            fiscal_year, ext_json, created_at, updated_at, updated_by
         ) VALUES (?1,?2,?3,?4,NULL,NULL,NULL,?5,?6,?7,?8,?9,?10,0,NULL,NULL,NULL,NULL,'手工录入',?11,?12,?13,?13,?14)",
        params![
            id,
            seq_no,
            board.code(),
            title,
            start.date,
            precision_str(start.precision),
            if start.raw.trim().is_empty() { None } else { Some(start.raw.clone()) },
            end_date,
            end_precision,
            end_raw,
            req.fiscal_year,
            "{}",
            ts,
            req.updated_by,
        ],
    )?;

    // 录入即入队：断网也能用，联网自动补推（P0-1 的补推队列在这里被真正用起来）
    let mut payload = serde_json::Map::new();
    payload.insert("id".into(), serde_json::json!(id));
    payload.insert("seq_no".into(), serde_json::json!(seq_no));
    payload.insert("board_code".into(), serde_json::json!(board.code()));
    payload.insert("title".into(), serde_json::json!(title));
    payload.insert("planned_start_date".into(), serde_json::json!(start.date));
    payload.insert(
        "planned_start_precision".into(),
        serde_json::json!(precision_str(start.precision)),
    );
    payload.insert("planned_start_raw".into(), serde_json::json!(start.raw));
    payload.insert("planned_end_date".into(), serde_json::json!(end_date));
    payload.insert("planned_end_precision".into(), serde_json::json!(end_precision));
    payload.insert("planned_end_raw".into(), serde_json::json!(end_raw));
    payload.insert("progress_pct".into(), serde_json::json!(0));
    payload.insert("fiscal_year".into(), serde_json::json!(req.fiscal_year));
    payload.insert("ext_json".into(), serde_json::json!("{}"));
    payload.insert("source_sheet".into(), serde_json::json!("手工录入"));
    payload.insert("created_at".into(), serde_json::json!(ts));
    payload.insert("updated_at".into(), serde_json::json!(ts));

    let ms = crate::store::now_ms_pub();
    let sync_row = SyncRow {
        table: "work_item".into(),
        id: id.clone(),
        payload,
        updated_at: ms,
        updated_by: req.updated_by.clone(),
        device_id: sync_client::state_get(conn, sync_client::keys::DEVICE_ID)
            .unwrap_or_else(|| "unknown".into()),
        seq: 0,
        deleted_at: None,
    };
    let queued = sync_client::enqueue(conn, &sync_row, &ts).is_ok();

    Ok(QuickAddResult {
        id,
        seq_no,
        plan_start_date: start.date,
        plan_start_precision: precision_str(start.precision).to_string(),
        plan_end_date: end_date,
        plan_end_precision: end_precision.to_string(),
        needs_confirm,
        queued,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Store;

    fn store() -> (tempfile::TempDir, Store) {
        let d = tempfile::tempdir().unwrap();
        let s = Store::open(d.path()).unwrap();
        (d, s)
    }

    fn req(title: &str, start: &str) -> QuickAdd {
        QuickAdd {
            title: title.into(),
            board: "gov".into(),
            plan_start_raw: start.into(),
            plan_end_raw: String::new(),
            fiscal_year: 2026,
            // 基准日固定为 2026-10-27（与任务环境时间一致），单测才可复现
            base_date: "2026-10-27".into(),
            updated_by: "m_test".into(),
        }
    }

    #[test]
    fn quick_add_with_natural_language_relative_date() {
        // PRD 原话是「下月3号」：规范化管道必须能吃下这种写法
        let (_d, s) = store();
        let r = quick_add(&s, &req("拜访某局", "下月3号")).unwrap();
        assert_eq!(r.plan_start_precision, "day");
        assert_eq!(r.plan_start_date.as_deref(), Some("2026-11-03"));
        assert!(r.needs_confirm.is_empty(), "能解析的日期不该进待确认队列");
        assert!(r.queued, "录入必须同时进同步队列，断网才能照常交付");
    }

    #[test]
    fn quick_add_month_precision_anchors_first_day() {
        let (_d, s) = store();
        let r = quick_add(&s, &req("季度复盘", "12月")).unwrap();
        assert_eq!(r.plan_start_date.as_deref(), Some("2026-12-01"));
        assert_eq!(r.plan_start_precision, "month");
    }

    #[test]
    fn quick_add_unparseable_goes_to_pending_confirmation() {
        let (_d, s) = store();
        let r = quick_add(&s, &req("等通知", "下周找个时间聊聊")).unwrap();
        assert_eq!(r.plan_start_precision, "unknown");
        assert_eq!(r.plan_start_date, None);
        assert_eq!(r.needs_confirm.len(), 1, "解析不出必须显式告诉用户，不许静默猜测");
    }

    #[test]
    fn quick_add_row_is_readable_by_kpi_and_repo_queries() {
        let (_d, s) = store();
        let r = quick_add(&s, &req("政策申报材料", "11月5日")).unwrap();
        let n: i64 = s
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM work_item WHERE id=?1 AND deleted_at IS NULL",
                params![r.id],
                |x| x.get(0),
            )
            .unwrap();
        assert_eq!(n, 1);
        let items = crate::repo::load_items(s.conn(), 2026).unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].title, "政策申报材料");
        assert_eq!(items[0].board, Board::Gov);
    }

    #[test]
    fn seq_no_increments_and_stays_unique() {
        let (_d, s) = store();
        let a = quick_add(&s, &req("第一条", "11月1日")).unwrap();
        let b = quick_add(&s, &req("第二条", "11月2日")).unwrap();
        assert_eq!(a.seq_no, 1);
        assert_eq!(b.seq_no, 2, "seq_no 必须按年度递增，否则 UNIQUE 会撞");
    }

    #[test]
    fn empty_title_and_unknown_board_are_rejected_before_touching_db() {
        let (_d, s) = store();
        assert!(quick_add(&s, &req("   ", "11月1日")).is_err());
        let mut bad = req("随便", "11月1日");
        bad.board = "不存在的板块".into();
        assert!(quick_add(&s, &bad).is_err());
        let n: i64 = s
            .conn()
            .query_row("SELECT COUNT(*) FROM work_item", [], |x| x.get(0))
            .unwrap();
        assert_eq!(n, 0, "校验失败不该留下半条数据");
    }

    #[test]
    fn range_uses_end_of_month_for_closing_side() {
        let (_d, s) = store();
        let mut r = req("六月活动", "6月");
        r.plan_end_raw = "6月".into();
        let out = quick_add(&s, &r).unwrap();
        assert_eq!(out.plan_start_date.as_deref(), Some("2026-06-01"));
        assert_eq!(out.plan_end_date.as_deref(), Some("2026-06-30"), "闭环取月末，否则时间窗缩一个月");
    }

    #[test]
    fn quick_add_enqueues_only_once_per_row() {
        let (_d, s) = store();
        let r = quick_add(&s, &req("同步候选", "11月8日")).unwrap();
        let n = sync_client::count_pending(s.conn()).unwrap();
        assert_eq!(n, 1);
        let stored: String = s
            .conn()
            .query_row("SELECT row_id FROM sync_outbox", [], |x| x.get(0))
            .unwrap();
        assert_eq!(stored, r.id);
    }
}
