//! 落库与回读：把导入结果写进 `work_item` / `kpi_metric`，并按板块派生 KPI 当前值。
//!
//! 两条设计约束落在这里：
//! - **一命令一事务边界**（架构 §3.3-2）：导入的整批写入在同一事务里完成。
//! - **KPI 不落死值**（架构 §4.11-2）：表里只存目标值与统计口径，
//!   「当前完成」一律在查询时由 `work_item` 实时派生，绝不存一个手工数字。

use crate::import::{ImportOutcome, ImportReport};
use crate::model::{Board, ItemStatus, KpiRule, Priority, WorkItem};
use crate::store::{Store, StoreError};
use rusqlite::{params, Connection};

/// 落库结果，供自检记录实测值。
#[derive(Debug, Clone, serde::Serialize)]
pub struct PersistReport {
    pub board_rows: usize,
    pub items_written: usize,
    pub kpis_written: usize,
    /// 回读校验：从库里数出来的条数，必须与写入数一致
    pub items_readback: i64,
    pub kpis_readback: i64,
    /// 库里按板块分组的条数，用于核对 12/11/14/10/9/8
    pub per_board_readback: Vec<(String, i64)>,
}

/// 把导入结果写入主库。已存在同 `(fiscal_year, seq_no)` 时覆盖（幂等，可重复导入同一份存量表）。
pub fn persist_import(store: &Store, out: &ImportOutcome, year: i32) -> Result<PersistReport, StoreError> {
    let guard = store.begin_write("import")?; // 写前 L1 快照；失败即阻断写入
    let conn = guard.conn();

    let tx = conn.unchecked_transaction()?;

    // 1) 板块字典（外键前置）
    let mut board_rows = 0usize;
    for b in Board::ALL {
        tx.execute(
            "INSERT OR REPLACE INTO domain_board (code, zh_name, sheet_name) VALUES (?1, ?2, ?3)",
            params![b.code(), b.zh_name(), b.sheet_name()],
        )?;
        board_rows += 1;
    }

    // 2) 工作项
    let now = crate::store::now_iso_pub();
    for it in &out.items {
        tx.execute(
            "INSERT OR REPLACE INTO work_item (
                id, seq_no, board_code, title, category, priority, target_quant,
                planned_start_date, planned_start_precision, planned_start_raw,
                planned_end_date, planned_end_precision, planned_end_raw,
                progress_pct, status, milestone_stage, next_action, owner,
                source_sheet, fiscal_year, ext_json, created_at, updated_at, updated_by
             ) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20,?21,?22,?23,?24)",
            params![
                it.id,
                it.seq_no,
                it.board.code(),
                it.title,
                it.category,
                it.priority.map(|p| p.as_str()),
                it.target_quant,
                it.planned_start_date,
                it.planned_start_precision,
                it.planned_start_raw,
                it.planned_end_date,
                it.planned_end_precision,
                it.planned_end_raw,
                it.progress_pct,
                it.status.map(|s| s.as_str()),
                it.milestone_stage,
                it.next_action,
                it.owner,
                it.source_sheet,
                year,
                serde_json::to_string(&it.ext).unwrap_or_else(|_| "{}".into()),
                now,
                now,
                "import",
            ],
        )?;
    }

    // 3) KPI 指标（只存目标与口径）
    for k in &out.kpis {
        tx.execute(
            "INSERT OR REPLACE INTO kpi_metric (
                id, category, name, target_value, rule, source_sheet, note, fiscal_year, updated_at, updated_by
             ) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)",
            params![
                k.id,
                k.category,
                k.name,
                k.target_value,
                kpi_rule_code(k.rule),
                k.source_sheet,
                k.note,
                year,
                now,
                "import",
            ],
        )?;
    }

    tx.commit()?;

    // 4) 回读校验（不信任写入时的自增计数）
    let (items_readback, per_board_readback) = readback_items(conn, year)?;
    let kpis_readback: i64 =
        conn.query_row("SELECT COUNT(*) FROM kpi_metric WHERE fiscal_year = ?1", params![year], |r| r.get(0))?;

    Ok(PersistReport {
        board_rows,
        items_written: out.items.len(),
        kpis_written: out.kpis.len(),
        items_readback,
        kpis_readback,
        per_board_readback,
    })
}

fn kpi_rule_code(r: KpiRule) -> &'static str {
    match r {
        KpiRule::BoardTotal => "board_total",
        KpiRule::BoardStarted => "board_started",
        KpiRule::BoardDone => "board_done",
    }
}

fn readback_items(conn: &Connection, year: i32) -> Result<(i64, Vec<(String, i64)>), StoreError> {
    let total: i64 =
        conn.query_row("SELECT COUNT(*) FROM work_item WHERE fiscal_year = ?1", params![year], |r| r.get(0))?;
    let mut stmt = conn.prepare(
        "SELECT board_code, COUNT(*) FROM work_item WHERE fiscal_year = ?1 GROUP BY board_code ORDER BY board_code",
    )?;
    let rows = stmt
        .query_map(params![year], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?
        .collect::<Result<Vec<_>, _>>()?;
    Ok((total, rows))
}

/// 从库里重新构造工作项，供规则求值使用（规则只看这些字段）。
pub fn load_items(conn: &Connection, year: i32) -> Result<Vec<WorkItem>, StoreError> {
    let mut stmt = conn.prepare(
        "SELECT seq_no, board_code, title, category, priority, target_quant,
                planned_start_date, planned_start_precision, planned_start_raw,
                planned_end_date, planned_end_precision, planned_end_raw,
                progress_pct, status, milestone_stage, next_action, owner,
                source_sheet, fiscal_year, ext_json, id, created_at, updated_at
         FROM work_item
         WHERE fiscal_year = ?1 AND archived_at IS NULL AND deleted_at IS NULL
         ORDER BY seq_no",
    )?;
    let items = stmt
        .query_map(params![year], |r| {
            let board_code: String = r.get(1)?;
            let prio: Option<String> = r.get(4)?;
            let status: Option<String> = r.get(13)?;
            let ext_json: String = r.get(19)?;
            Ok(WorkItem {
                id: r.get(20)?,
                seq_no: r.get(0)?,
                board: Board::from_code(&board_code).unwrap_or(Board::Gov),
                title: r.get(2)?,
                category: r.get(3)?,
                priority: prio.as_deref().and_then(Priority::from_raw),
                target_quant: r.get(5)?,
                planned_start_date: r.get(6)?,
                planned_start_precision: r.get(7)?,
                planned_start_raw: r.get(8)?,
                planned_end_date: r.get(9)?,
                planned_end_precision: r.get(10)?,
                planned_end_raw: r.get(11)?,
                progress_pct: r.get(12)?,
                status: status.as_deref().and_then(ItemStatus::from_zh),
                milestone_stage: r.get(14)?,
                next_action: r.get(15)?,
                owner: r.get(16)?,
                source_sheet: r.get(17)?,
                fiscal_year: r.get(18)?,
                ext: serde_json::from_str(&ext_json).unwrap_or_default(),
                created_at: r.get(21)?,
                updated_at: r.get(22)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(items)
}

/// KPI 当前完成值（派生，不落库）。返回（指标名，目标，当前，口径）。
pub fn derive_kpi(conn: &Connection, year: i32) -> Result<Vec<(String, f64, Option<f64>, String)>, StoreError> {
    let mut stmt = conn.prepare(
        "SELECT name, target_value, rule, category FROM kpi_metric
         WHERE fiscal_year = ?1 AND deleted_at IS NULL ORDER BY id",
    )?;
    let rows = stmt
        .query_map(params![year], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, f64>(1)?, r.get::<_, String>(2)?, r.get::<_, String>(3)?))
        })?
        .collect::<Result<Vec<_>, _>>()?;

    let _ = count_all; // 保留全库口径备查，正常路径不用
    let mut out = Vec::new();
    for (name, target, rule, category) in rows {
        let board = Board::from_zh(&category).or_else(|| Board::from_code(&category));
        // 口径落不到板块时给 None，而**不是**悄悄退化成全库计数——
        // 那会把「政策项目总数」算成 64 而不是 8，是一个看起来正常的错误数字。
        let current = match (rule.as_str(), board) {
            ("board_total", Some(b)) => Some(count_board(conn, year, b, None)?),
            ("board_started", Some(b)) => Some(count_board(conn, year, b, Some("started"))?),
            ("board_done", Some(b)) => Some(count_board(conn, year, b, Some("done"))?),
            _ => None,
        };
        out.push((name, target, current, rule));
    }
    Ok(out)
}

fn count_board(conn: &Connection, year: i32, b: Board, mode: Option<&str>) -> Result<f64, StoreError> {
    let sql = match mode {
        None => "SELECT COUNT(*) FROM work_item WHERE fiscal_year=?1 AND board_code=?2 AND deleted_at IS NULL",
        Some("started") => {
            "SELECT COUNT(*) FROM work_item WHERE fiscal_year=?1 AND board_code=?2 AND deleted_at IS NULL
             AND (progress_pct > 0 OR status IN ('进行中','已完成'))"
        }
        _ => {
            "SELECT COUNT(*) FROM work_item WHERE fiscal_year=?1 AND board_code=?2 AND deleted_at IS NULL
             AND status IN ('已完成','已取消')"
        }
    };
    let n: i64 = conn.query_row(sql, params![year, b.code()], |r| r.get(0))?;
    Ok(n as f64)
}

fn count_all(conn: &Connection, year: i32, mode: Option<&str>) -> Result<f64, StoreError> {
    let sql = match mode {
        None => "SELECT COUNT(*) FROM work_item WHERE fiscal_year=?1 AND deleted_at IS NULL",
        Some("started") => {
            "SELECT COUNT(*) FROM work_item WHERE fiscal_year=?1 AND deleted_at IS NULL
             AND (progress_pct > 0 OR status IN ('进行中','已完成'))"
        }
        _ => {
            "SELECT COUNT(*) FROM work_item WHERE fiscal_year=?1 AND deleted_at IS NULL
             AND status IN ('已完成','已取消')"
        }
    };
    let n: i64 = conn.query_row(sql, params![year], |r| r.get(0))?;
    Ok(n as f64)
}

/// 把导入报告的关键字段写进一张自检台账表，便于事后回查（只增不改）。
pub fn record_import_report(conn: &Connection, rep: &ImportReport) -> Result<(), StoreError> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS import_run (
            id           INTEGER PRIMARY KEY AUTOINCREMENT,
            source_file  TEXT NOT NULL,
            fiscal_year  INTEGER NOT NULL,
            items_total  INTEGER NOT NULL,
            items_imported INTEGER NOT NULL,
            items_skipped  INTEGER NOT NULL,
            kpi_total    INTEGER NOT NULL,
            reconciled   INTEGER NOT NULL,
            note         TEXT,
            created_at   TEXT NOT NULL
         );",
    )?;
    conn.execute(
        "INSERT INTO import_run
         (source_file, fiscal_year, items_total, items_imported, items_skipped, kpi_total, reconciled, note, created_at)
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)",
        params![
            rep.source_file,
            rep.fiscal_year,
            rep.items_total as i64,
            rep.items_imported as i64,
            rep.items_skipped as i64,
            rep.kpi_total as i64,
            if rep.reconciled { 1 } else { 0 },
            rep.reconcile_note,
            crate::store::now_iso_pub(),
        ],
    )?;
    Ok(())
}
