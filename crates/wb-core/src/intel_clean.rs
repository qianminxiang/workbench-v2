//! v3.3.1 · 误配源数据清洗（一次性、幂等、先留底后删除）。
//!
//! 背景（用户 2026-11-22 19:01 原话「苏州区域的不要加进来，应该是拼音搜索的问题」）：
//! 宿州源 `suzhou_ah` 自接入起域名配错为江苏苏州站 `ggzy.suzhou.gov.cn`
//! （「suzhou」既是苏州也是宿州的拼音），历史抓到的**全部**记录都是苏州区域公告。
//! v3.3.1 已把域名修正为宿州真实站 `ggzyjy.ahsz.gov.cn`；修正只管新数据，
//! 库里的苏州误抓数据必须清掉——否则新旧混存，用户在界面看到的仍是「苏州区域数据」。
//!
//! 工作伙伴条款（随单带入，必须执行）：
//! 1. **清洗留底**：删除前把被删行**全量**导出 JSONL 备份，另写一份「按源、按公告日期」
//!    的条数分布账本（ledger JSON），两份都落在数据目录 `backups/clean/` 下；
//!    验收对账以账本为准，不是一句总数。
//! 2. **幂等**：`sync_state` 记一次性标记，重复启动不再删、不再覆盖备份。
//! 3. **只删该源**：不动其他源的任何行；只读库（迁移失败降级）直接跳过。
//!
//! 线索池联动：`intel_lead` 的 `dedup_key` 是项目键。删除某源全部记录后，
//! 「只剩这一个源支撑的项目」其线索一并删除；跨源项目（其他源仍有 current 行）
//! 的线索保留——该线索的 `record_id` 指向被删行时由下一轮抓取自然补齐，不在这里硬改。

use crate::store::{now_iso_pub, Store, StoreError};
use rusqlite::params;
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;

/// 清洗账本：按源、按公告日期的条数分布 + 备份文件路径。
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct CleanReport {
    pub source_id: String,
    pub reason: String,
    /// 本次是否实际执行（false = 幂等标记已存在 / 只读库 / 该源无数据）
    pub executed: bool,
    /// 是否因只读库跳过
    pub skipped_read_only: bool,
    pub rows_total: usize,
    pub rows_current: usize,
    /// 公告日期 → 条数（current 行；解析不出的日期记 "unknown"）
    pub by_date: BTreeMap<String, usize>,
    pub leads_deleted: usize,
    pub insights_deleted: usize,
    /// 全量备份 JSONL 的文件名（backups/clean/ 下）
    pub backup_file: String,
    /// 账本 JSON 的文件名（backups/clean/ 下）
    pub ledger_file: String,
    pub cleaned_at: String,
}

/// `sync_state` 的一次性标记键。
fn flag_key(source_id: &str) -> String {
    format!("intel_clean:{source_id}")
}

fn state_get(store: &Store, k: &str) -> Result<Option<String>, StoreError> {
    let conn = store.conn();
    let mut stmt = conn.prepare("SELECT v FROM sync_state WHERE k = ?1")?;
    let mut rows = stmt.query(params![k])?;
    Ok(match rows.next()? {
        Some(r) => Some(r.get::<_, String>(0)?),
        None => None,
    })
}

fn state_set(store: &Store, k: &str, v: &str) -> Result<(), StoreError> {
    let guard = store.begin_write("sync-state")?;
    guard.conn().execute(
        "INSERT INTO sync_state (k, v) VALUES (?1, ?2)
         ON CONFLICT(k) DO UPDATE SET v = excluded.v",
        params![k, v],
    )?;
    Ok(())
}

/// 清洗一个误配源的全部已入库数据（先备份 + 账本，后删除；幂等）。
///
/// 返回的 `CleanReport` 无论是否实际执行都带账本语义：
/// - 标记已存在 → `executed=false`，其余字段为 0 / 空文件名；
/// - 该源本就无数据 → `executed=false`（`rows_total=0`），**不**写标记也不写备份
///   （无数据可清，落标记反而掩盖「这轮到底有没有清」）；
/// - 只读库 → `skipped_read_only=true`，不动任何数据。
pub fn clean_source(store: &Store, source_id: &str, reason: &str) -> Result<CleanReport, StoreError> {
    let mut rep = CleanReport {
        source_id: source_id.to_string(),
        reason: reason.to_string(),
        executed: false,
        skipped_read_only: false,
        rows_total: 0,
        rows_current: 0,
        by_date: BTreeMap::new(),
        leads_deleted: 0,
        insights_deleted: 0,
        backup_file: String::new(),
        ledger_file: String::new(),
        cleaned_at: now_iso_pub(),
    };

    if state_get(store, &flag_key(source_id))?.is_some() {
        return Ok(rep); // 幂等：上一轮已清过，不重复删、不覆盖备份
    }
    if store.read_only {
        rep.skipped_read_only = true;
        return Ok(rep);
    }

    // 1) 读取该源全部行（含拉链历史），解析公告日期做账本
    let conn = store.conn();
    let rows: Vec<(String, String, String, i64, String)> = {
        let mut stmt = conn.prepare(
            "SELECT id, biz_key, record_json, is_current, valid_from
               FROM ext_record WHERE source_id = ?1 ORDER BY valid_from ASC, id ASC",
        )?;
        let it = stmt
            .query_map(params![source_id], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        it
    };
    rep.rows_total = rows.len();
    if rows.is_empty() {
        return Ok(rep); // 该源无数据：不落标记、不写备份（见函数文档）
    }

    for (_id, _biz, json, is_cur, _vf) in rows.iter() {
        if *is_cur != 1 {
            continue;
        }
        rep.rows_current += 1;
        let date = serde_json::from_str::<serde_json::Value>(json)
            .ok()
            .and_then(|v| v.get("date").and_then(|d| d.as_str()).map(|s| s.to_string()))
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "unknown".to_string());
        *rep.by_date.entry(date).or_insert(0) += 1;
    }

    // 2) 备份：全量行 JSONL（先写盘成功，才允许删）
    let dir = store.layout.backups_dir().join("clean");
    std::fs::create_dir_all(&dir).map_err(|e| StoreError::Io(e.to_string()))?;
    let stamp = rep.cleaned_at.replace([':', '-'], "");
    let backup_name = format!("clean-{source_id}-{stamp}.jsonl");
    let ledger_name = format!("clean-{source_id}-{stamp}.ledger.json");
    let backup_path = dir.join(&backup_name);
    {
        let mut f = std::fs::File::create(&backup_path).map_err(|e| StoreError::Io(e.to_string()))?;
        for (id, biz, json, is_cur, vf) in rows.iter() {
            let line = serde_json::json!({
                "id": id, "biz_key": biz, "is_current": is_cur, "valid_from": vf,
                "record_json": json,
            });
            writeln!(f, "{line}").map_err(|e| StoreError::Io(e.to_string()))?;
        }
        f.flush().map_err(|e| StoreError::Io(e.to_string()))?;
    }

    // 3) 账本（先落盘；之后删除就算中途失败，对账依据已留存）
    rep.backup_file = backup_name.clone();
    rep.ledger_file = ledger_name.clone();
    {
        let ledger_path = dir.join(&ledger_name);
        std::fs::write(&ledger_path, serde_json::to_string_pretty(&rep).unwrap_or_default())
            .map_err(|e| StoreError::Io(e.to_string()))?;
    }

    // 4) 删除（单事务）：ext_record / 仅剩该源支撑的线索 / 该源的洞察
    let guard = store.begin_write("intel-clean")?;
    let tx_conn = guard.conn();
    let tx = tx_conn.unchecked_transaction()?;

    // 4a) 被删行支撑的项目键（项目键 = biz_key 去掉节点后缀）
    let deleted_projects: BTreeSet<String> = rows
        .iter()
        .map(|(_id, biz, ..)| crate::intel_store::split_project_key(biz).0.to_string())
        .filter(|k| !k.is_empty())
        .collect();
    tx.execute("DELETE FROM ext_record WHERE source_id = ?1", params![source_id])?;

    // 4b) 幸存项目键：删除后仍存在的 current 行（任何源）所归属的项目
    let surviving: BTreeSet<String> = {
        let mut stmt = tx.prepare("SELECT biz_key FROM ext_record WHERE is_current = 1")?;
        let keys = stmt
            .query_map([], |r| r.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        keys.iter()
            .map(|k| crate::intel_store::split_project_key(k).0.to_string())
            .filter(|k| !k.is_empty())
            .collect()
    };
    for k in deleted_projects.difference(&surviving) {
        rep.leads_deleted += tx.execute("DELETE FROM intel_lead WHERE dedup_key = ?1", params![k])?;
    }
    rep.insights_deleted =
        tx.execute("DELETE FROM ext_insight WHERE source_id = ?1", params![source_id])?;
    tx.commit()?;
    drop(guard);

    // 5) 幂等标记（数据已删、备份已留之后才落标记）
    rep.executed = true;
    let summary = serde_json::json!({
        "cleaned_at": rep.cleaned_at,
        "rows_total": rep.rows_total,
        "rows_current": rep.rows_current,
        "leads_deleted": rep.leads_deleted,
    });
    state_set(store, &flag_key(source_id), &summary.to_string())?;

    // 账本补上执行结果重写一遍（executed / 删除数在初稿时还没有）
    let ledger_path = dir.join(&ledger_name);
    let _ = std::fs::write(&ledger_path, serde_json::to_string_pretty(&rep).unwrap_or_default());
    Ok(rep)
}

/// v3.3.1 启动期清洗入口（应用打开库之后、任何采集之前调用；失败不阻断启动）。
///
/// 返回每步的执行情况供启动日志 / 诊断输出。**任何一步失败都如实返回，
/// 不吞错**——清洗失败意味着苏州数据还在库里，必须让上层看见。
pub fn run_v331_startup(store: &Store) -> Vec<Result<CleanReport, StoreError>> {
    // 宿州源域名修正（拼音撞名）后的历史数据清洗。
    // 理由与证据链见模块文档；reason 写进账本，验收对账以它为口径。
    vec![clean_source(
        store,
        "suzhou_ah",
        "v3.3.1：宿州源域名配错（ggzy.suzhou.gov.cn 实为江苏苏州站，拼音撞名），历史数据全部为苏州区域误抓，整体清除",
    )]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::intel::IntelItem;
    use crate::intel_store::persist_fetch;
    use crate::store::Store;
    use serde_json::Value;

    fn tmp_store(tag: &str) -> Store {
        let dir = std::env::temp_dir().join(format!("wb-clean-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        Store::open(&dir).expect("open store")
    }

    fn item(site: &str, title: &str, date: &str) -> IntelItem {
        IntelItem {
            key: format!("{site}|http://x/{title}"),
            site_id: site.into(),
            site_name: format!("{site}-name"),
            title: title.into(),
            org: "某采购人".into(),
            area: "苏州".into(),
            date: date.into(),
            amount: None,
            url: format!("http://x/{title}"),
            relevance: crate::intel::Relevance::Core,
            reasons: vec!["命中主题词:无人机".into()],
            incomplete: false,
            manual_verdict: false,
            first_seen: date.into(),
            body_head: "正文内容样本".into(),
            native_code: None,
        }
    }

    fn count(store: &Store, sql: &str, p: &str) -> i64 {
        store
            .conn()
            .query_row(sql, params![p], |r| r.get(0))
            .unwrap()
    }

    #[test]
    fn cleans_only_target_source_with_backup_and_ledger() {
        let store = tmp_store("basic");
        // suzhou_ah（误抓源）两条 + chuzhou 一条 + 跨源共用项目键一条（pjwnc 与 suzhou_ah 同键）
        persist_fetch(&store, &[item("suzhou_ah", "苏州某无人机项目A", "2026-11-01")], None).unwrap();
        persist_fetch(&store, &[item("suzhou_ah", "苏州某无人机项目B", "2026-11-02")], None).unwrap();
        persist_fetch(&store, &[item("chuzhou", "滁州某无人机项目", "2026-11-03")], None).unwrap();
        // 跨源同项目：pjwnc 的公告与 suzhou_ah 一条公告同编号（L3 code: 相同项目键）。
        // 注意标题必须与 A/B 都不同——同标题同日期会被 L2 指纹拦下并入，
        // suzhou_ah 就凑不出 3 条 current 行（首轮红正是这个原因）。
        let mut shared = item("pjwnc", "跨源同编号公告", "2026-11-04");
        shared.native_code = Some("X-Shared-001".into());
        let mut sz_c = item("suzhou_ah", "苏州某无人机项目C", "2026-11-01");
        sz_c.native_code = Some("X-Shared-001".into());
        persist_fetch(&store, &[sz_c], None).unwrap();
        persist_fetch(&store, &[shared], None).unwrap();

        let before_sz = count(&store, "SELECT COUNT(*) FROM ext_record WHERE source_id = ?1", "suzhou_ah");
        assert!(before_sz >= 2, "夹具应至少有两条 suzhou_ah 行，实际 {before_sz}");

        let rep = clean_source(
            &store,
            "suzhou_ah",
            "测试清洗：拼音撞名误抓",
        )
        .unwrap();
        assert!(rep.executed);
        assert_eq!(rep.rows_current, 3, "三条 current 行进账本: {:?}", rep.by_date);
        assert_eq!(rep.by_date.get("2026-11-01"), Some(&2));
        assert_eq!(rep.by_date.get("2026-11-02"), Some(&1));

        // 该源行清零，其他源不动
        assert_eq!(count(&store, "SELECT COUNT(*) FROM ext_record WHERE source_id = ?1", "suzhou_ah"), 0);
        assert_eq!(count(&store, "SELECT COUNT(*) FROM ext_record WHERE source_id = ?1", "chuzhou"), 1);
        assert_eq!(count(&store, "SELECT COUNT(*) FROM ext_record WHERE source_id = ?1", "pjwnc"), 1);

        // 备份与账本真实存在且内容可读
        let dir = store.layout.backups_dir().join("clean");
        let backup = std::fs::read_to_string(dir.join(&rep.backup_file)).unwrap();
        let lines: Vec<&str> = backup.lines().filter(|l| !l.trim().is_empty()).collect();
        assert_eq!(lines.len(), rep.rows_total, "备份行数 = 删除总行数（含拉链）");
        let v: Value = serde_json::from_str(lines[0]).unwrap();
        assert!(v.get("record_json").is_some(), "备份必须带全量 record_json");
        let ledger = std::fs::read_to_string(dir.join(&rep.ledger_file)).unwrap();
        let lv: Value = serde_json::from_str(&ledger).unwrap();
        assert_eq!(lv["executed"], true);
        assert_eq!(lv["source_id"], "suzhou_ah");
        assert_eq!(lv["by_date"]["2026-11-01"], 2, "账本按日期分布: {ledger}");

        // 幂等：第二次调用不执行、不覆盖
        let rep2 = clean_source(&store, "suzhou_ah", "再跑一遍").unwrap();
        assert!(!rep2.executed);
        assert_eq!(rep2.rows_total, 0);
        assert_eq!(rep2.backup_file, "", "幂等轮不得写备份文件");
    }

    #[test]
    fn empty_source_writes_nothing_and_no_flag() {
        let store = tmp_store("empty");
        persist_fetch(&store, &[item("chuzhou", "滁州某无人机项目", "2026-11-03")], None).unwrap();
        let rep = clean_source(&store, "suzhou_ah", "空源").unwrap();
        assert!(!rep.executed);
        assert_eq!(rep.rows_total, 0);
        assert!(state_get(&store, &flag_key("suzhou_ah")).unwrap().is_none(), "无数据不得落幂等标记");
        let dir = store.layout.backups_dir().join("clean");
        assert!(!dir.exists(), "无数据不得产生备份目录");
    }

    #[test]
    fn leads_of_suzhou_only_projects_are_deleted_but_shared_kept() {
        let store = tmp_store("leads");
        // 项目键 code:xonly 只被 suzhou_ah 支撑 ⇒ 线索应删
        let mut solo = item("suzhou_ah", "苏州独占项目公告", "2026-11-05");
        solo.native_code = Some("X-Only-001".into());
        persist_fetch(&store, &[solo], None).unwrap();
        // 项目键 code:xshared 由 suzhou_ah 与 pjwnc 共同支撑 ⇒ 线索保留
        let mut sz = item("suzhou_ah", "苏州共享项目公告", "2026-11-05");
        sz.native_code = Some("X-Shared-B".into());
        let mut pj = item("pjwnc", "普农共享项目公告", "2026-11-06");
        pj.native_code = Some("X-Shared-B".into());
        persist_fetch(&store, &[sz, pj], None).unwrap();

        let rep = clean_source(&store, "suzhou_ah", "线索联动").unwrap();
        assert!(rep.executed);
        assert!(rep.leads_deleted >= 1, "独占项目的线索应被删: {rep:?}");
        let kept: i64 = store
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM intel_lead WHERE dedup_key = 'code:xsharedb'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(kept, 1, "跨源项目的线索必须保留");
        let gone: i64 = store
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM intel_lead WHERE dedup_key = 'code:xonly001'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(gone, 0, "只剩误抓源支撑的线索必须删除");
    }
}
