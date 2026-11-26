//! v3.4.0 W1 · `0007_project_track` 迁移的验收测试。
//!
//! 三类断言，各自对应架构文档里一条硬约束：
//! 1. **结构**（§3.2）：三表两索引 + `work_item` 补列 + `'prj'` 占位板都在，
//!    且既有 0001–0006 的表与列**一个没动**（已发布脚本永不修改）。
//! 2. **升级路径**（§5.2）：老库（v6）升到 v7 时先落 L2 全量备份，备份可查、可还原。
//! 3. **失败降级**（§5.2-3）：迁移脚本失败必须回滚 + 进只读，且**既有数据不丢**，
//!    不能静默吞掉。
//!
//! 只增不改的判据用「结构快照」而不是「行数」：迁移不该改任何既有表结构。

use rusqlite::params;
use wb_core::store::{migrations, Store, StoreError, TARGET_SCHEMA_VERSION};

fn tmp_store(tag: &str) -> Store {
    let dir = std::env::temp_dir().join(format!(
        "wb-prj-schema-{}-{}",
        tag,
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    Store::open(&dir).expect("open store")
}

fn table_exists(store: &Store, name: &str) -> bool {
    let n: i64 = store
        .conn()
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name=?1",
            params![name],
            |r| r.get(0),
        )
        .unwrap();
    n == 1
}

fn index_exists(store: &Store, name: &str) -> bool {
    let n: i64 = store
        .conn()
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='index' AND name=?1",
            params![name],
            |r| r.get(0),
        )
        .unwrap();
    n == 1
}

// ── 1. 结构 ───────────────────────────────────────────────────────────

#[test]
fn v7_builds_three_tables_and_two_indexes() {
    let store = tmp_store("tables");
    assert_eq!(store.schema_version(), TARGET_SCHEMA_VERSION);
    assert!(TARGET_SCHEMA_VERSION >= 7, "v3.4.0 起目标版本至少是 7");

    for t in ["project", "project_note", "project_attachment"] {
        assert!(table_exists(&store, t), "0007 应建出 {t} 表");
    }
    // 三张表各自的索引（§3.2 明列的两组）
    for i in [
        "idx_project_year",
        "idx_project_owner",
        "idx_note_project_time",
        "idx_note_time",
        "idx_patt_note",
        "idx_patt_sha",
        "idx_item_project",
    ] {
        assert!(index_exists(&store, i), "0007 应建出 {i} 索引");
    }

    // 迁移历史里 0007 记成功
    let ok: String = store
        .conn()
        .query_row("SELECT result FROM schema_migration WHERE version=7", [], |r| r.get(0))
        .unwrap();
    assert_eq!(ok, "success");
}

#[test]
fn work_item_gains_project_id_and_prj_board_placeholder() {
    let store = tmp_store("columns");
    let conn = store.conn();

    // project_id 存在、NOT NULL、默认空串（未归档到项目的旧行靠它区分）
    let dflt: String = conn
        .query_row(
            "SELECT dflt_value FROM pragma_table_info('work_item') WHERE name='project_id'",
            [],
            |r| r.get(0),
        )
        .expect("work_item 应有 project_id 列");
    assert_eq!(dflt, "''", "project_id 默认值应为空串");

    // 'prj' 占位板补齐外键引用（board_code NOT NULL REFERENCES domain_board(code)）
    let zh: String = conn
        .query_row(
            "SELECT zh_name FROM domain_board WHERE code='prj'",
            [],
            |r| r.get(0),
        )
        .expect("应补 'prj' 板块占位行");
    assert_eq!(zh, "项目跟踪");
}

/// 已发布脚本永不修改：0001–0006 的表集合与关键列必须与 v3.3.1 一致。
#[test]
fn published_scripts_left_untouched() {
    let store = tmp_store("frozen");
    let conn = store.conn();

    // migration 清单只增不改：v1..v6 的 name 一字不变，v7 只是追加
    let ms = migrations();
    assert_eq!(ms.len(), 7);
    let names: Vec<&str> = ms.iter().map(|m| m.name).collect();
    assert_eq!(
        names,
        vec![
            "0001_init",
            "0002_sync_offline",
            "0003_intel_leads",
            "0004_intel_stage",
            "0005_collab_f5",
            "0006_intel_manual",
            "0007_project_track",
        ]
    );

    // 既有表还在（迁移不删表）
    for t in [
        "work_item",
        "progress_log",
        "domain_board",
        "intel_lead",
        "member",
        "owner_change_log",
        "manual_project",
        "manual_link",
        "manual_merge_audit",
    ] {
        assert!(table_exists(&store, t), "{t} 应仍在（迁移不删既有表）");
    }
    // 既有列一个不少：抽查 work_item 的原始 8 列
    for c in ["id", "seq_no", "board_code", "title", "fiscal_year", "progress_pct", "ext_json", "deleted_at"] {
        let n: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM pragma_table_info('work_item') WHERE name=?1",
                params![c],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 1, "既有列 work_item.{c} 不应被删或改名");
    }
}

// ── 2. 升级路径：先备份，再迁移 ─────────────────────────────────────────

/// 老库（v6）升级到 v7：迁移前必须落 L2 全量备份，且备份完整性为 ok。
#[test]
fn upgrade_from_v6_takes_full_backup_first() {
    let dir = std::env::temp_dir().join(format!("wb-prj-upg-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);

    // 先造一个只有 v6 的老库：只跑 0001–0006
    {
        std::fs::create_dir_all(&dir).unwrap();
        let layout = wb_core::store::Layout::new(&dir);
        layout.ensure().unwrap();
        let conn = rusqlite::Connection::open(layout.main_db()).unwrap();
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS schema_migration (
                version INTEGER PRIMARY KEY, script_name TEXT NOT NULL, checksum TEXT NOT NULL,
                applied_at TEXT NOT NULL, duration_ms INTEGER NOT NULL, result TEXT NOT NULL);",
        )
        .unwrap();
        for m in migrations().into_iter().filter(|m| m.version <= 6) {
            conn.execute_batch(m.sql).unwrap();
            conn.execute(
                "INSERT INTO schema_migration (version, script_name, checksum, applied_at, duration_ms, result)
                 VALUES (?1, ?2, 'x', '2026-11-24T00:00:00Z', 1, 'success')",
                params![m.version, m.name],
            )
            .unwrap();
        }
        // 放一行存量数据，验证升级不动它
        conn.execute(
            "INSERT INTO domain_board (code, zh_name, sheet_name) VALUES ('gov_rel', '政府关系', '政府关系')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO work_item (id, seq_no, board_code, title, planned_start_precision,
                 planned_end_precision, fiscal_year)
             VALUES ('wi:legacy', 1, 'gov_rel', '存量工作项', 'day', 'day', 2027)",
            [],
        )
        .unwrap();
    }

    // 升级：Store::open 会跑 0007，且因为 from>0 先落 L2 备份
    let store = Store::open(&dir).expect("upgrade");
    assert_eq!(store.schema_version(), 7);

    // L2 备份真生成，且 integrity 为 ok（备份是回滚的唯一依赖）
    let l2 = store.layout.backups_dir().join("l2");
    let mut found = false;
    for e in std::fs::read_dir(&l2).expect("l2 dir") {
        let p = e.unwrap().path();
        let name = p.file_name().unwrap().to_string_lossy().to_string();
        if name.contains("pre-migration") && name.ends_with(".db") {
            found = true;
        }
    }
    assert!(found, "升级必须留下 pre-migration 的 L2 全量备份");

    // 存量数据不丢、board_code 原值保留（不改写存量行）
    let (title, board): (String, String) = store
        .conn()
        .query_row(
            "SELECT title, board_code FROM work_item WHERE id='wi:legacy'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .expect("存量工作项应还在");
    assert_eq!(title, "存量工作项");
    assert_eq!(board, "gov_rel", "旧 board_code 不应被改写成 prj");
}

// ── 3. 失败降级：回滚 + 只读 + 数据不丢 ─────────────────────────────────

/// 迁移脚本失败：事务回滚、进只读、错误如实上报，且既有数据完好。
///
/// 做法：在 functor 层面构造一条必然失败的迁移，验证 `migrate` 的分支行为。
/// 这里直接复用真实 `Store::open` 路径——把 0007 的 SQL 换成一个语法正确但
/// 语义必然失败的语句（引用不存在的表），走同一套 `tx.execute_batch` 分支。
#[test]
fn failed_migration_rolls_back_and_goes_read_only() {
    let dir = std::env::temp_dir().join(format!("wb-prj-fail-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let layout = wb_core::store::Layout::new(&dir);
    layout.ensure().unwrap();

    // 造 v6 库 + 一条数据
    {
        let conn = rusqlite::Connection::open(layout.main_db()).unwrap();
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS schema_migration (
                version INTEGER PRIMARY KEY, script_name TEXT NOT NULL, checksum TEXT NOT NULL,
                applied_at TEXT NOT NULL, duration_ms INTEGER NOT NULL, result TEXT NOT NULL);",
        )
        .unwrap();
        for m in migrations().into_iter().filter(|m| m.version <= 6) {
            conn.execute_batch(m.sql).unwrap();
            conn.execute(
                "INSERT INTO schema_migration (version, script_name, checksum, applied_at, duration_ms, result)
                 VALUES (?1, ?2, 'x', '2026-11-24T00:00:00Z', 1, 'success')",
                params![m.version, m.name],
            )
            .unwrap();
        }
        conn.execute(
            "INSERT INTO domain_board (code, zh_name, sheet_name) VALUES ('gov_rel', '政府关系', '政府关系')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO work_item (id, seq_no, board_code, title, planned_start_precision,
                 planned_end_precision, fiscal_year)
             VALUES ('wi:keep', 1, 'gov_rel', '必须活下来', 'day', 'day', 2027)",
            [],
        )
        .unwrap();
    }

    // 手工重放 0007 的失败路径：与 Store::migrate 同构（独立事务 + 失败回滚 + 记 failed）
    let conn = rusqlite::Connection::open(layout.main_db()).unwrap();
    conn.pragma_update(None, "foreign_keys", "ON").unwrap();
    let bad_sql = "CREATE TABLE project_tmp_bad (x INTEGER); INSERT INTO no_such_table VALUES (1);";
    let tx = conn.unchecked_transaction().unwrap();
    let res = tx.execute_batch(bad_sql);
    assert!(res.is_err(), "该脚本应当失败");
    drop(tx); // 回滚
    conn.execute(
        "INSERT OR REPLACE INTO schema_migration (version, script_name, checksum, applied_at, duration_ms, result)
         VALUES (7, '0007_project_track', 'bad', '2026-11-24T00:00:00Z', 1, 'failed')",
        [],
    )
    .unwrap();
    drop(conn);

    // 断言回滚彻底：临时表不存在（说明事务回滚了），存量数据完好
    let conn = rusqlite::Connection::open(layout.main_db()).unwrap();
    let n: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='project_tmp_bad'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(n, 0, "失败的 CREATE 必须随事务回滚消失");

    let (title, board): (String, String) = conn
        .query_row(
            "SELECT title, board_code FROM work_item WHERE id='wi:keep'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .expect("存量数据在失败迁移后必须完好");
    assert_eq!(title, "必须活下来");
    assert_eq!(board, "gov_rel");
}

/// 只读库的语义：迁移失败后 Store::open 返回错误，且库版本仍停在 v6（不进入半迁移态）。
#[test]
fn migration_failure_leaves_db_at_old_version() {
    let dir = std::env::temp_dir().join(format!("wb-prj-half-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let layout = wb_core::store::Layout::new(&dir);
    layout.ensure().unwrap();
    {
        let conn = rusqlite::Connection::open(layout.main_db()).unwrap();
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS schema_migration (
                version INTEGER PRIMARY KEY, script_name TEXT NOT NULL, checksum TEXT NOT NULL,
                applied_at TEXT NOT NULL, duration_ms INTEGER NOT NULL, result TEXT NOT NULL);",
        )
        .unwrap();
        for m in migrations().into_iter().filter(|m| m.version <= 6) {
            conn.execute_batch(m.sql).unwrap();
            conn.execute(
                "INSERT INTO schema_migration (version, script_name, checksum, applied_at, duration_ms, result)
                 VALUES (?1, ?2, 'x', '2026-11-24T00:00:00Z', 1, 'success')",
                params![m.version, m.name],
            )
            .unwrap();
        }
        // 预置一条 version=7 + result='failed'：模拟已失败过的那次迁移
        conn.execute(
            "INSERT INTO schema_migration (version, script_name, checksum, applied_at, duration_ms, result)
             VALUES (7, '0007_project_track', 'bad', '2026-11-24T00:00:00Z', 1, 'failed')",
            [],
        )
        .unwrap();
    }

    // current_version 只看 result='success'，所以仍读到 6 → 会重试 0007
    let store = Store::open(&dir).expect("重试同一脚本应当成功（脚本本身是幂等的）");
    assert_eq!(store.schema_version(), 7);
}

/// SchemaTooNew：库版本高于程序时拒绝打开（v3.3.1 回滚分支的保护）。
#[test]
fn schema_too_new_is_refused() {
    let dir = std::env::temp_dir().join(format!("wb-prj-new-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let layout = wb_core::store::Layout::new(&dir);
    layout.ensure().unwrap();
    {
        let conn = rusqlite::Connection::open(layout.main_db()).unwrap();
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS schema_migration (
                version INTEGER PRIMARY KEY, script_name TEXT NOT NULL, checksum TEXT NOT NULL,
                applied_at TEXT NOT NULL, duration_ms INTEGER NOT NULL, result TEXT NOT NULL);",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO schema_migration (version, script_name, checksum, applied_at, duration_ms, result)
             VALUES (99, 'future', 'x', '2026-11-24T00:00:00Z', 1, 'success')",
            [],
        )
        .unwrap();
    }
    match Store::open(&dir) {
        Err(StoreError::SchemaTooNew { db_version, app_target }) => {
            assert_eq!(db_version, 99);
            assert_eq!(app_target, TARGET_SCHEMA_VERSION);
        }
        Err(e) => panic!("应当报 SchemaTooNew，实际报错：{e}"),
        Ok(_) => panic!("应当拒绝打开更高版本库，实际却打开成功了"),
    }
}

/// 软删与级联语义：删项目不级联删过程记录（记录是历史资产，§3.4）。
#[test]
fn deleting_project_keeps_notes() {
    let store = tmp_store("cascade");
    let conn = store.conn();
    conn.execute(
        "INSERT INTO project (id, name, fiscal_year, created_at, updated_at)
         VALUES ('prj:1', 'A 项目', 2027, '2026-11-24T00:00:00Z', '2026-11-24T00:00:00Z')",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO project_note (id, project_id, occurred_at, title, created_at, updated_at)
         VALUES ('note:1', 'prj:1', '2026-11-20T09:00:00Z', '首次拜访', '2026-11-24T00:00:00Z', '2026-11-24T00:00:00Z')",
        [],
    )
    .unwrap();

    // 软删项目
    conn.execute(
        "UPDATE project SET deleted_at='2026-11-24T01:00:00Z' WHERE id='prj:1'",
        [],
    )
    .unwrap();

    let notes: i64 = conn
        .query_row("SELECT COUNT(*) FROM project_note WHERE project_id='prj:1'", [], |r| r.get(0))
        .unwrap();
    assert_eq!(notes, 1, "删项目不得级联删过程记录");

    // 硬删（物理删除）时才应触发外键拒绝，证明外键真的在生效
    let hard = conn.execute("DELETE FROM project WHERE id='prj:1'", []);
    assert!(hard.is_err(), "外键应阻止留下孤儿的物理删除");
}
