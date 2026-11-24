//! W2 项目数据层验收：CRUD、occurred_at 排序、附件 sha256 跨记录去重与带账本回收、
//! 存量 work_item 迁移向导（预览只读 + 确认执行只改 project_id）。
//!
//! 每条用例对应架构文档 §3.4 的边界行为或硬约束 2 的账本要求。

use std::path::Path;

use wb_core::project::{
    attach_file, create_note, create_project, get_project, list_attachments, list_notes, list_projects,
    preview_work_item_migration, reclaim_orphan_attachments, run_work_item_migration, soft_delete_note,
    soft_delete_project, update_project, AttachWrite, NoteDraft, NoteKind, NoteReject, ProjectDraft,
    ProjectReject, Stage,
};
use wb_core::store::Store;

fn tmp_store(tag: &str) -> Store {
    let dir = std::env::temp_dir().join(format!("wb-pdata-{}-{}", tag, std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    Store::open(&dir).expect("open store")
}

fn draft(name: &str) -> ProjectDraft {
    ProjectDraft {
        name: name.into(),
        client: Some("合肥某某医院".into()),
        area: Some("安徽".into()),
        stage: None,
        owner: Some("大角子".into()),
        summary: None,
        start_date: Some("2026-11-10".into()),
        expect_end_date: None,
    }
}

fn note(project_id: &str, date: &str, time: Option<&str>, body: &str) -> NoteDraft {
    NoteDraft {
        project_id: project_id.into(),
        work_item_id: None,
        occurred_date: date.into(),
        occurred_time: time.map(|s| s.to_string()),
        title: Some(body.chars().take(12).collect()),
        body: body.into(),
        place: Some("现场".into()),
        kind: Some(NoteKind::Visit),
    }
}

// ───────────────────────── 项目 CRUD ─────────────────────────

#[test]
fn project_create_read_update_and_stage_enum() {
    let store = tmp_store("crud");
    let c = create_project(&store, &draft("低空经济产业规划"), 2026).expect("建档");
    let p = &c.project;

    assert_eq!(p.name, "低空经济产业规划");
    assert_eq!(p.stage, "planned", "未指定阶段默认 planned");
    assert_eq!(p.fiscal_year, 2026, "fiscal_year 跟随 start_date 年份");
    assert_eq!(p.stage_enum(), Stage::Planned);

    // 读回
    let got = get_project(&store, &p.id).expect("读").expect("在库");
    assert_eq!(got, *p);

    // 改档：只覆盖传入字段
    let upd = update_project(
        &store,
        &p.id,
        &ProjectDraft {
            name: "低空经济产业规划（二期）".into(),
            client: None,
            area: None,
            stage: Some(Stage::Active),
            owner: None,
            summary: None,
            start_date: Some("2027-03-01".into()),
            expect_end_date: Some("2027-12-31".into()),
        },
        "大角子",
    )
    .expect("改档");

    assert_eq!(upd.name, "低空经济产业规划（二期）");
    assert_eq!(upd.stage, "active");
    assert_eq!(upd.fiscal_year, 2027, "start_date 跨年则 fiscal_year 跟走（§3.4）");
    assert_eq!(upd.client.as_deref(), Some("合肥某某医院"), "未传的字段保持原值");
    assert_eq!(upd.owner.as_deref(), Some("大角子"));
}

#[test]
fn duplicate_name_is_rejected_but_released_after_soft_delete() {
    let store = tmp_store("dup");
    let a = create_project(&store, &draft("同一项目名"), 2026).expect("建档");

    // 同名在建 → 拒绝
    let dup = create_project(&store, &draft("同一项目名"), 2026).unwrap_err();
    assert!(
        matches!(dup, ProjectReject::DuplicateName(ref n) if n == "同一项目名"),
        "应当拒绝同名，实际：{dup:?}"
    );

    // 改名撞已有名字 → 也拒绝
    let b = create_project(&store, &draft("另一个项目"), 2026).expect("建档 B");
    let clash = update_project(
        &store,
        &b.project.id,
        &ProjectDraft {
            name: "同一项目名".into(),
            ..Default::default()
        },
        "大角子",
    )
    .unwrap_err();
    assert!(matches!(clash, ProjectReject::DuplicateName(_)), "改名撞名应拒绝：{clash:?}");

    // A 软删后，名字可以复用（只与未软删项目比）
    assert!(soft_delete_project(&store, &a.project.id, "大角子").expect("软删"));
    assert!(get_project(&store, &a.project.id).expect("读").is_none(), "软删后读不到");
    create_project(&store, &draft("同一项目名"), 2026).expect("软删后同名可复用");
}

#[test]
fn missing_name_and_bad_date_are_rejected() {
    let store = tmp_store("valid");
    let e = create_project(&store, &draft("   "), 2026).unwrap_err();
    assert_eq!(e, ProjectReject::MissingName, "空白项目名应拒绝");

    let mut d = draft("日期非法");
    d.start_date = Some("2026-13-45".into());
    let e = create_project(&store, &d, 2026).unwrap_err();
    assert!(
        matches!(e, ProjectReject::BadDate(ref f, _) if f == "start_date"),
        "非法日期应拒绝而不是静默丢弃：{e:?}"
    );
}

#[test]
fn list_orders_by_last_note_time_then_newest_project() {
    let store = tmp_store("order");
    let a = create_project(&store, &draft("A 项目"), 2026).expect("A").project;
    let b = create_project(&store, &draft("B 项目"), 2026).expect("B").project;

    // 给 A 补一条更早的历史记录，给 B 一条更近的
    create_note(&store, &note(&a.id, "2026-11-02", None, "更早的动作"), "大角子").expect("A note");
    create_note(&store, &note(&b.id, "2026-11-20", Some("10:30"), "更近的动作"), "大角子").expect("B note");

    let list = list_projects(&store, 2026, false).expect("列表");
    assert_eq!(list.len(), 2);
    assert_eq!(list[0].project.name, "B 项目", "最近有动作的排前");
    assert_eq!(list[0].note_count, 1);
    assert_eq!(list[0].last_occurred_at.as_deref(), Some("2026-11-20T10:30:00Z"));
    assert_eq!(list[1].project.name, "A 项目");
}

#[test]
fn soft_delete_project_keeps_notes() {
    let store = tmp_store("keepnotes");
    let p = create_project(&store, &draft("有记录的项目"), 2026).expect("建档").project;
    create_note(&store, &note(&p.id, "2026-11-15", None, "一条记录"), "大角子").expect("note");

    assert!(soft_delete_project(&store, &p.id, "大角子").expect("软删"));
    assert!(list_projects(&store, 2026, false).expect("列表").is_empty(), "软删项目不进列表");

    // 记录是历史资产，不级联删（§3.4）
    let notes = list_notes(&store, &p.id).expect("按项目读记录");
    assert_eq!(notes.len(), 1, "软删项目不得连带删除过程记录");
}

// ───────────────────────── 过程记录 ─────────────────────────

#[test]
fn notes_sort_by_occurred_at_not_created_at() {
    let store = tmp_store("occurred");
    let p = create_project(&store, &draft("时间线项目"), 2026).expect("建档").project;

    // 先录入「今天」的，再补录一条「上周」的——补录不该排在最前面
    create_note(&store, &note(&p.id, "2026-11-24", Some("09:00"), "今天录的"), "大角子").expect("n1");
    create_note(&store, &note(&p.id, "2026-11-18", None, "补录上周的"), "大角子").expect("n2");
    create_note(&store, &note(&p.id, "2026-11-18", Some("14:20"), "同一天下午"), "大角子").expect("n3");

    let notes = list_notes(&store, &p.id).expect("时间线");
    let times: Vec<&str> = notes.iter().map(|n| n.occurred_at.as_str()).collect();
    assert_eq!(
        times,
        vec!["2026-11-24T09:00:00Z", "2026-11-18T14:20:00Z", "2026-11-18T00:00:00Z"],
        "排序键必须是 occurred_at，补录历史日期不得靠 created_at 顶到最前"
    );
    // 无时刻的补录归零时，不补当前时刻（否则同一天内的先后不可解释）
    assert_eq!(notes[2].occurred_at, "2026-11-18T00:00:00Z");
    assert_eq!(notes[2].month_key(), "2026-11");
}

#[test]
fn note_rejects_missing_body_date_and_unknown_project() {
    let store = tmp_store("notevalid");
    let p = create_project(&store, &draft("校验项目"), 2026).expect("建档").project;

    let mut d = note(&p.id, "2026-11-20", None, "  ");
    d.body = "   ".into();
    assert_eq!(create_note(&store, &d, "大角子").unwrap_err(), NoteReject::MissingBody);

    let mut d = note(&p.id, "", None, "内容有");
    d.occurred_date = "".into();
    assert_eq!(create_note(&store, &d, "大角子").unwrap_err(), NoteReject::MissingDate);

    // 不补零 → 落库后会破坏字符串排序，必须在入口拒绝
    let d = note(&p.id, "2026-2-30", None, "内容有");
    assert!(matches!(create_note(&store, &d, "大角子").unwrap_err(), NoteReject::BadDate(_)));
    // 补零但日历上不存在（2 月 30 日）同样拒绝
    let d = note(&p.id, "2026-02-30", None, "内容有");
    assert!(matches!(create_note(&store, &d, "大角子").unwrap_err(), NoteReject::BadDate(_)));

    let mut d = note("prj:不存在", "2026-11-20", None, "内容有");
    d.project_id = "prj:不存在".into();
    assert!(matches!(
        create_note(&store, &d, "大角子").unwrap_err(),
        NoteReject::ProjectNotFound(_)
    ));
}

#[test]
fn soft_delete_note_hides_from_timeline() {
    let store = tmp_store("noteDel");
    let p = create_project(&store, &draft("删记录项目"), 2026).expect("建档").project;
    let n = create_note(&store, &note(&p.id, "2026-11-19", None, "会被删的记录"), "大角子").expect("note");

    assert!(soft_delete_note(&store, &n.id, "大角子").expect("软删"));
    assert!(list_notes(&store, &p.id).expect("时间线").is_empty());
    assert!(list_projects(&store, 2026, false).expect("列表")[0].last_occurred_at.is_none());
}

// ───────────────────────── 附件：sha256 跨记录去重 ─────────────────────────

fn write_src(dir: &Path, name: &str, bytes: &[u8]) -> std::path::PathBuf {
    let p = dir.join(name);
    std::fs::write(&p, bytes).expect("写源文件");
    p
}

#[test]
fn identical_content_is_stored_once_across_records() {
    let store = tmp_store("dedup");
    let root = store.layout.root.clone();
    let src_dir = root.join("src");
    std::fs::create_dir_all(&src_dir).expect("src dir");

    let p1 = create_project(&store, &draft("项目一"), 2026).expect("p1").project;
    let p2 = create_project(&store, &draft("项目二"), 2026).expect("p2").project;
    let n1 = create_note(&store, &note(&p1.id, "2026-11-10", None, "现场照片 A"), "大角子").expect("n1");
    let n2 = create_note(&store, &note(&p2.id, "2026-11-12", None, "现场照片 B"), "大角子").expect("n2");

    let bytes = b"same photo bytes";
    let f1 = write_src(&src_dir, "site-a.jpg", bytes);
    let f2 = write_src(&src_dir, "site-b.jpg", bytes); // 同内容不同文件名

    let a1 = attach_file(&store, &n1.id, &f1, "site-a.jpg", Some("image/jpeg"), 0, "大角子").expect("挂 a");
    assert_eq!(a1.write, AttachWrite::Stored, "首份内容需要落盘");
    assert!(a1.attachment.file_exists);

    let a2 = attach_file(&store, &n2.id, &f2, "site-b.jpg", Some("image/jpeg"), 0, "大角子").expect("挂 b");
    match a2.write {
        AttachWrite::Reused { saved_bytes } => assert_eq!(saved_bytes, bytes.len() as u64),
        other => panic!("同内容第二次挂载应复用落盘文件，实际：{other:?}"),
    }
    assert_eq!(a1.attachment.sha256, a2.attachment.sha256);
    assert_eq!(
        a1.attachment.storage_path, a2.attachment.storage_path,
        "内容寻址：同 sha256 必落到同一路径"
    );

    // 落盘一份（不是两份）
    let abs = root.join(&a1.attachment.storage_path);
    assert!(abs.exists());
    let files: Vec<_> = std::fs::read_dir(abs.parent().unwrap())
        .expect("列目录")
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_string_lossy() == abs.file_name().unwrap().to_string_lossy())
        .collect();
    assert_eq!(files.len(), 1, "同内容只应有一份落盘文件");

    // 记录侧冗余计数
    assert_eq!(list_attachments(&store, &n1.id).expect("n1 附件").len(), 1);
    assert_eq!(list_attachments(&store, &n2.id).expect("n2 附件").len(), 1);
}

#[test]
fn attachment_list_orders_by_sort_no_and_flags_missing_file() {
    let store = tmp_store("attl");
    let root = store.layout.root.clone();
    let src_dir = root.join("src");
    std::fs::create_dir_all(&src_dir).expect("src dir");

    let p = create_project(&store, &draft("附件项目"), 2026).expect("p").project;
    let n = create_note(&store, &note(&p.id, "2026-11-22", None, "多附件"), "大角子").expect("n");

    let f2 = write_src(&src_dir, "b.png", b"bbbb");
    let f1 = write_src(&src_dir, "a.png", b"aaaa");
    let b = attach_file(&store, &n.id, &f2, "b.png", Some("image/png"), 1, "大角子").expect("b");
    let a = attach_file(&store, &n.id, &f1, "a.png", Some("image/png"), 0, "大角子").expect("a");

    let list = list_attachments(&store, &n.id).expect("列表");
    assert_eq!(list[0].file_name, "a.png", "sort_no 升序");
    assert_eq!(list[1].file_name, "b.png");

    // 落盘文件被外部移走 → file_exists=false（界面显示占位，不渲染空白）
    let abs = root.join(&a.attachment.storage_path);
    std::fs::remove_file(&abs).expect("移走");
    let again = list_attachments(&store, &n.id).expect("再列");
    let moved = again.iter().find(|x| x.id == a.attachment.id).expect("找得到");
    assert!(!moved.file_exists);
    assert!(b.attachment.file_exists);
}

// ───────────────────────── 附件回收：账本先落盘 ─────────────────────────

#[test]
fn reclaim_writes_ledger_before_deleting_and_frees_only_orphans() {
    let store = tmp_store("reclaim");
    let root = store.layout.root.clone();
    let src_dir = root.join("src");
    std::fs::create_dir_all(&src_dir).expect("src dir");

    // 三条记录：两条已软删（其附件应被回收）、一条存活（其附件要留下）
    let p = create_project(&store, &draft("回收项目"), 2026).expect("p").project;
    let n_live = create_note(&store, &note(&p.id, "2026-11-24", None, "活记录"), "大角子").expect("live");
    let n_del1 = create_note(&store, &note(&p.id, "2026-11-01", None, "删记录一"), "大角子").expect("d1");
    let n_del2 = create_note(&store, &note(&p.id, "2026-11-02", None, "删记录二"), "大角子").expect("d2");

    let live_bytes = b"live content";
    let orphan_bytes = b"orphan content 0123456789";
    let f_live = write_src(&src_dir, "live.jpg", live_bytes);
    let f_orph = write_src(&src_dir, "orphan1.jpg", orphan_bytes);
    let f_orph2 = write_src(&src_dir, "orphan2.jpg", orphan_bytes); // 同内容两份记录

    let a_live = attach_file(&store, &n_live.id, &f_live, "live.jpg", None, 0, "大角子").expect("live att");
    let a_d1 = attach_file(&store, &n_del1.id, &f_orph, "orphan1.jpg", None, 0, "大角子").expect("d1 att");
    let a_d2 = attach_file(&store, &n_del2.id, &f_orph2, "orphan2.jpg", None, 0, "大角子").expect("d2 att");
    assert_eq!(a_d1.attachment.storage_path, a_d2.attachment.storage_path, "同内容同路径");

    // 软删这些记录（附件行保留，引用变为非活）
    soft_delete_note(&store, &n_del1.id, "大角子").expect("del1");
    soft_delete_note(&store, &n_del2.id, "大角子").expect("del2");

    // ── dry_run：只记账，文件必须还在 ──
    let dry = reclaim_orphan_attachments(&store, true).expect("dry");
    assert_eq!(dry.reclaimed, 1, "dry 判定 1 个 sha256 可回收");
    assert_eq!(dry.freed_bytes, 0, "dry_run 不释放空间");
    let ledger_json: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&dry.ledger_path).expect("读 dry 账本")).expect("parse");
    assert_eq!(ledger_json["scanned_sha256"], 2, "扫到 2 个 sha256（活 1 + 孤儿 1）");
    assert!(root.join(&a_d1.attachment.storage_path).exists(), "dry_run 不得删文件");

    // ── 真跑：先有回收前账本，再删 ──
    let real = reclaim_orphan_attachments(&store, false).expect("reclaim");
    assert_eq!(real.reclaimed, 1, "恰好回收 1 个 sha256");
    assert_eq!(real.kept, 1, "活引用那份保留");
    assert_eq!(real.freed_bytes, orphan_bytes.len() as i64, "释放字节数应等于孤儿内容体积");

    // 回收前账本必须存在且早于回收后账本，且内容里写明了「打算删哪个 sha256」
    let before_path = real.before_ledger_path.as_deref().expect("必须有回收前账本");
    let before: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(before_path).expect("读回收前账本")).expect("parse");
    let before_entries = before["entries"].as_array().expect("entries");
    let planned = before_entries
        .iter()
        .find(|e| e["verdict"] == "reclaim_pending")
        .expect("回收前账本必须写出计划回收的 sha256");
    assert_eq!(planned["sha256"], a_d1.attachment.sha256);
    assert_eq!(planned["live_refs"], 0, "判定依据是无活引用");
    assert_eq!(planned["total_refs"], 2, "总引用 2 条（都来自已删记录）");

    // 落盘：孤儿文件没了，活文件还在
    assert!(!root.join(&a_d1.attachment.storage_path).exists(), "孤儿内容已删除");
    assert!(root.join(&a_live.attachment.storage_path).exists(), "活引用内容不得被删");

    // 回收后账本的 entries 要能逐个 sha256 交代处置结论
    let after: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&real.ledger_path).expect("读回收后账本")).expect("parse");
    let verdicts: Vec<String> = after["entries"]
        .as_array()
        .expect("entries")
        .iter()
        .map(|e| e["verdict"].as_str().unwrap_or_default().to_string())
        .collect();
    assert!(verdicts.contains(&"keep".to_string()));
    assert!(verdicts.contains(&"reclaimed".to_string()));

    // 账本落在 backups/clean/ 下，可查
    assert!(
        Path::new(&real.ledger_path).starts_with(store.layout.backups_dir().join("clean")),
        "账本必须落在 backups/clean/"
    );

    // ── 幂等：再跑一次不应报错，也不该再释放任何空间 ──
    let again = reclaim_orphan_attachments(&store, false).expect("重跑");
    assert_eq!(again.freed_bytes, 0, "已回收的内容重跑不再释放空间");
}

// ───────────────────────── 存量迁移向导 ─────────────────────────

/// 造 3 条存量 work_item（v3.3.1 形态：有 board_code、project_id 为空）。
fn seed_legacy_items(store: &Store) -> i64 {
    let conn = store.conn();
    conn.execute(
        "INSERT OR IGNORE INTO domain_board (code, zh_name, sheet_name) VALUES ('gov_rel','政企关系','政企关系')",
        [],
    )
    .expect("board");
    for (id, seq, board) in [("wi:1", 1, "gov_rel"), ("wi:2", 2, "gov_rel"), ("wi:3", 3, "gov_rel")] {
        conn.execute(
            "INSERT INTO work_item (id, seq_no, board_code, title, fiscal_year, status,
                 planned_start_precision, planned_end_precision, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, 2026, 'open', 'unknown', 'unknown',
                 '2026-11-01T00:00:00Z', '2026-11-01T00:00:00Z')",
            rusqlite::params![id, seq, board, format!("存量任务 {seq}")],
        )
        .expect("insert legacy");
    }
    3
}

#[test]
fn migration_preview_is_read_only() {
    let store = tmp_store("migprev");
    let seeded = seed_legacy_items(&store);

    let before_items = count(&store, "SELECT COUNT(*) FROM work_item");
    let before_projects = count(&store, "SELECT COUNT(*) FROM project");
    let before_board: String = store
        .conn()
        .query_row("SELECT board_code FROM work_item WHERE id='wi:1'", [], |r| r.get(0))
        .expect("board_code");

    let plan = preview_work_item_migration(&store).expect("预览");
    assert_eq!(plan.pending_items, seeded, "待迁移条数 = 全部无 project_id 的行");
    assert_eq!(plan.rows.len(), 1, "按 board_code 分组，只有一组");
    assert_eq!(plan.rows[0].board_code, "gov_rel");
    assert_eq!(plan.rows[0].project_name, "政企关系", "默认项目名取板块中文名");
    assert!(plan.rows[0].existing_project_id.is_none());

    // 预览不得写库
    assert_eq!(count(&store, "SELECT COUNT(*) FROM work_item"), before_items, "预览不改行数");
    assert_eq!(count(&store, "SELECT COUNT(*) FROM project"), before_projects, "预览不建项目");
    let after_board: String = store
        .conn()
        .query_row("SELECT board_code FROM work_item WHERE id='wi:1'", [], |r| r.get(0))
        .expect("board_code");
    assert_eq!(after_board, before_board, "预览不动 board_code");
}

#[test]
fn migration_run_only_sets_project_id_and_keeps_board_code() {
    let store = tmp_store("migrun");
    seed_legacy_items(&store);
    let plan = preview_work_item_migration(&store).expect("预览");

    let res = run_work_item_migration(&store, &plan, 2026, "大角子").expect("执行");
    assert_eq!(res.projects_created, 1);
    assert_eq!(res.projects_reused, 0);
    assert_eq!(res.items_linked, 3);

    // project_id 都填上了，board_code 原值保留（§5.1-2「不改写存量行」）
    let conn = store.conn();
    let mut stmt = conn.prepare("SELECT board_code, project_id FROM work_item ORDER BY seq_no").expect("stmt");
    let rows: Vec<(String, String)> = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .expect("q")
        .map(|r| r.expect("row"))
        .collect();
    assert_eq!(rows.len(), 3);
    for (_board, pid) in &rows {
        assert_eq!(rows[0].0, "gov_rel", "board_code 保持原值");
        assert!(!pid.is_empty(), "project_id 必须回填");
    }
    assert!(rows.iter().all(|(_, pid)| pid == &rows[0].1), "同组共用一个项目");

    // 项目确实建出来了，可读
    let projects = list_projects(&store, 2026, false).expect("列表");
    assert_eq!(projects.len(), 1);
    assert_eq!(projects[0].project.name, "政企关系");

    // 再预览 → 0 条待迁移（幂等）
    let again = preview_work_item_migration(&store).expect("再预览");
    assert_eq!(again.pending_items, 0);
}

#[test]
fn migration_reuses_existing_project_with_same_name() {
    let store = tmp_store("migreuse");
    // 先手工建一个同名项目
    create_project(&store, &draft("政企关系"), 2026).expect("已建");
    seed_legacy_items(&store);

    let plan = preview_work_item_migration(&store).expect("预览");
    assert!(
        plan.rows[0].existing_project_id.is_some(),
        "已有同名项目时必须复用，预览里要标出来"
    );

    let res = run_work_item_migration(&store, &plan, 2026, "大角子").expect("执行");
    assert_eq!(res.projects_created, 0, "不重复建档");
    assert_eq!(res.projects_reused, 1);
    assert_eq!(res.items_linked, 3);
    assert_eq!(list_projects(&store, 2026, false).expect("列表").len(), 1, "仍然只有一个项目");
}

fn count(store: &Store, sql: &str) -> i64 {
    store.conn().query_row(sql, [], |r| r.get(0)).expect("count")
}
