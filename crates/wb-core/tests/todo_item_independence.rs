//! H4 运行时验证：`pct.rs` 的 `auto_status` 与「线索→待办」闭环的状态字段互不污染。
//!
//! 质检单 7711670075803127088 的 H4 指出：W5 交付的待办版块让「情报线索转待办」和
//! 既有「工作项改进度」共用同一张 `work_item` 表，两者对 `status` / `progress_pct`
//! 的读写是否互相踩踏，此前只有静态读码结论、**没有运行时验证**。
//!
//! 本文件用公开接口跑到真实行为，覆盖 100% 边界两侧的三条路径：
//! - `60 → 100`：进度推满，`auto_status` 应把状态推到「已完成」
//! - `100 → 40`：从满进度退回，`auto_status` 应把「已完成」拉回「进行中」
//! - `1 → 50`：起步（已有实质进展但状态还挂在「待启动」）
//!
//! 每条路径之后都同时断言两侧事实：
//! 1. 待办身份 `todo_id = lead:<dedup_key>` 不因工作项状态变化而改变（不读 `work_item.status`）
//! 2. 待办的生命周期（`intel_lead.state = todoed`）与工作项的状态各自演进
//! 3. `work_item` 不因反复改进度而多出行（幂等键未被进度写入破坏）

use wb_core::intel::{IntelItem, Relevance};
use wb_core::intel_store::{confirm_lead, generate_todo, persist_fetch, read_leads};
use wb_core::lead_todo::LeadState;
use wb_core::model::{ItemStatus, WorkItem};
use wb_core::pct::{update_item, ItemPatch};
use wb_core::repo::load_items;
use wb_core::store::Store;

fn tmp_store(tag: &str) -> Store {
    let dir = std::env::temp_dir().join(format!(
        "wb-h4-indep-{}-{}-{}",
        tag,
        std::process::id(),
        // 同一进程内多条用例共用一个 tag 时也不会互相踩库
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    Store::open(&dir).expect("open store")
}

fn item(key: &str) -> IntelItem {
    IntelItem {
        key: key.to_string(),
        site_id: "pjwnc".into(),
        site_name: "普农集团".into(),
        title: "综合农事服务中心无人机采购项目招标公告".into(),
        org: "安徽省普济圩现代农业集团".into(),
        area: "铜陵".into(),
        date: "2026-10-28".into(),
        amount: Some("22万元".into()),
        // 日期字段只用于落库展示；due_date 由 `lead_todo::due_date_from(&lead.title, today)`
        // 决定——title 里没有日期，故走「+3 工作日」兜底。用例固定传 today='2026-11-06'
        // （周五），顺延到 2026-11-11，是确定性结果，不随实际运行日漂移。
        // 这一点是读 `to_todo` 实现确认的：它只读 title，不读 body_head。
        url: format!("http://www.pjwnc.com/content/detail/{}.html", key.replace('|', "_")),
        relevance: Relevance::Core,
        reasons: vec!["正文复核加分词:植保".into()],
        incomplete: false,
        manual_verdict: false,
        first_seen: "2026-11-06".into(),
        body_head: "TLTRZB【2026】074 预算22万元，采购植保无人机2台；开标时间2026-11-13 09:30".into(),
        native_code: None,
    }
}

/// 造一条已确认、已转待办的情报线索，返回（dedup_key, todo_id, work_item 行 id 的快照）。
fn seed_todo(store: &Store, tag: &str) -> (String, String) {
    let mut lead_item = item(&format!("pjwnc|http://www.pjwnc.com/content/detail/{}.html", tag));
    lead_item.key = format!("pjwnc|http://www.pjwnc.com/content/detail/{}.html", tag);
    persist_fetch(store, &[lead_item], Some("2026-11-06T08:00:00Z")).expect("persist");

    let dedup_key = read_leads(store, None)
        .expect("read leads")
        .first()
        .expect("线索应已入池")
        .dedup_key
        .clone();

    assert!(
        confirm_lead(store, &dedup_key, Some("2026-11-06T09:00:00Z")).expect("confirm"),
        "首次确认应成功"
    );

    let todo = generate_todo(store, &dedup_key, "2026-11-06", "大角子")
        .expect("generate_todo 不应报库错")
        .expect("确认后的主源线索应能转待办");

    assert_eq!(
        todo.todo_id,
        format!("lead:{}", dedup_key),
        "待办身份必须由 dedup_key 派生，而不是自增或时间戳"
    );
    (dedup_key, todo.todo_id)
}

/// 读回该待办对应的工作项（按 id 精确定位，避免该年其它行干扰断言）。
fn read_item(store: &Store, todo_id: &str) -> WorkItem {
    load_items(store.conn(), 2026)
        .expect("load items")
        .into_iter()
        .find(|i| i.id == todo_id)
        .unwrap_or_else(|| panic!("待办 {todo_id} 应对应 work_item 里的一行"))
}

/// 断言待办侧没被工作项的进度写入改动过。
fn assert_todo_untouched(store: &Store, dedup_key: &str, todo_id: &str) {
    let leads = read_leads(store, Some(&LeadState::Todoed)).expect("read todoed leads");
    assert_eq!(
        leads.iter().filter(|l| l.dedup_key == *dedup_key).count(),
        1,
        "线索应仍只有一条处于「已转待办」"
    );
    // 再转一次待办：能得到同一个 todo_id，说明「待办身份」不依赖 work_item.status
    let again = generate_todo(store, dedup_key, "2026-11-07", "大角子")
        .expect("re-generate")
        .expect("已转待办的线索应可重复确认并保持幂等");
    assert_eq!(again.todo_id, todo_id, "重复确认必须回到同一条待办");
}

fn set_pct(store: &Store, todo_id: &str, from_expected: i64, to: i64) -> wb_core::pct::UpdateOutcome {
    let out = update_item(
        store,
        &ItemPatch {
            item_id: todo_id.to_string(),
            progress_pct: Some(to),
            status: None,
            note: None,
            by: Some("大角子".into()),
        },
    )
    .expect("update_item 不应报库错");
    assert_eq!(out.from_pct, from_expected, "起点进度应符合前一步的实际结果");
    assert_eq!(out.to_pct, to, "进度应写成本次提交值");
    assert!(out.rejected_reason.is_none(), "合法进度不应被拒: {:?}", out.rejected_reason);
    out
}

#[test]
fn todo_survives_progress_crossing_60_to_100_and_back() {
    let store = tmp_store("cross");
    let (key, todo_id) = seed_todo(&store, "cross");

    // 初始：转待办落库时 status='待启动'、progress=0
    let it0 = read_item(&store, &todo_id);
    assert_eq!(it0.progress_pct, 0);
    assert_eq!(it0.status, Some(ItemStatus::Pending), "转待办入库状态应为「待启动」");

    // 0 → 60：起步，auto_status 把「待启动」推成「进行中」
    let out1 = set_pct(&store, &todo_id, 0, 60);
    assert_eq!(out1.to_status.as_deref(), Some("进行中"), "1–99% 应把待启动推成进行中");
    let it1 = read_item(&store, &todo_id);
    assert_eq!(it1.progress_pct, 60);
    assert_eq!(it1.status, Some(ItemStatus::Active));
    assert_todo_untouched(&store, &key, &todo_id);

    // 60 → 100：推满，auto_status 把状态推到「已完成」
    let out2 = set_pct(&store, &todo_id, 60, 100);
    assert_eq!(out2.to_status.as_deref(), Some("已完成"), "100% 应把非完成态推成已完成");
    let it2 = read_item(&store, &todo_id);
    assert_eq!(it2.progress_pct, 100);
    assert_eq!(it2.status, Some(ItemStatus::Done));
    // 关键断言：工作项已「已完成」，但待办侧的线索生命周期没有跟着变成完成/关闭
    assert_todo_untouched(&store, &key, &todo_id);

    // 100 → 40：从满进度退回，auto_status 把「已完成」拉回「进行中」
    let out3 = set_pct(&store, &todo_id, 100, 40);
    assert_eq!(out3.to_status.as_deref(), Some("进行中"), "从 100% 退回应把已完成拉回进行中");
    let it3 = read_item(&store, &todo_id);
    assert_eq!(it3.progress_pct, 40);
    assert_eq!(it3.status, Some(ItemStatus::Active));
    assert_todo_untouched(&store, &key, &todo_id);

    // 全程只该有这一行 work_item：反复改进度不能破坏幂等键、多出并行待办
    let rows = load_items(store.conn(), 2026).expect("load items");
    let same_id = rows.iter().filter(|i| i.id == todo_id).count();
    assert_eq!(same_id, 1, "同一待办只能对应一行 work_item，实际 {same_id} 行");
}

#[test]
fn todo_survives_progress_from_1_to_50_after_explicit_pause() {
    let store = tmp_store("start");
    let (key, todo_id) = seed_todo(&store, "start");

    // 0 → 1：最小实质进展，也属于 (1..100) 区间之外的边界，先确认 1% 的行为
    let out1 = set_pct(&store, &todo_id, 0, 1);
    assert_eq!(out1.to_status.as_deref(), Some("进行中"), "1% 已算实质进展，应推成进行中");
    assert_todo_untouched(&store, &key, &todo_id);

    // 人工显式置「已暂停」：auto_status 不得回改人工判断
    let paused = update_item(
        &store,
        &ItemPatch {
            item_id: todo_id.clone(),
            progress_pct: None,
            status: Some("已暂停".into()),
            note: Some("甲方要求暂缓".into()),
            by: Some("大角子".into()),
        },
    )
    .expect("explicit pause");
    assert_eq!(paused.to_status.as_deref(), Some("已暂停"));
    assert!(paused.action_logged, "显式改状态应落一条动作记录");

    // 1 → 50：进度继续走，但「已暂停」是人的中断态，不该被自动联动改回来
    let out2 = set_pct(&store, &todo_id, 1, 50);
    assert_eq!(
        out2.to_status.as_deref(),
        Some("已暂停"),
        "已暂停是人明确表达的中断态，进度再多也不替人改回进行中"
    );
    let it = read_item(&store, &todo_id);
    assert_eq!(it.progress_pct, 50);
    assert_eq!(it.status, Some(ItemStatus::Paused));

    // 待办侧仍独立：线索生命周期没被「已暂停」影响，仍可幂等重复确认
    assert_todo_untouched(&store, &key, &todo_id);
}

#[test]
fn deleting_item_status_backing_does_not_resurrect_or_drop_todo() {
    // 反向验证：「待办存在与否」不依赖 work_item.status 的取值。
    // 先把工作项状态显式置「已取消」，再重复转待办 —— 待办必须仍在且幂等。
    let store = tmp_store("cancel");
    let (key, todo_id) = seed_todo(&store, "cancel");

    let cancelled = update_item(
        &store,
        &ItemPatch {
            item_id: todo_id.clone(),
            progress_pct: None,
            status: Some("已取消".into()),
            note: None,
            by: Some("大角子".into()),
        },
    )
    .expect("explicit cancel");
    assert_eq!(cancelled.to_status.as_deref(), Some("已取消"));

    // 待办侧完全不受影响
    assert_todo_untouched(&store, &key, &todo_id);
    let it = read_item(&store, &todo_id);
    assert_eq!(it.status, Some(ItemStatus::Cancelled));
    assert_eq!(it.progress_pct, 0, "只改状态不改进度，进度应保持 0");
}
