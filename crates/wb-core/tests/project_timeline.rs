//! v3.2.0 情报项目阶段聚合：端到端验收（T3 轨「阶段规则 + 归并查询」）。

use wb_core::intel::Relevance;
use wb_core::intel_stage::{judge, stage_from_text, Stage, StageSource};
use wb_core::intel_store::{list_projects, persist_fetch, read_project_timeline};
use wb_core::store::Store;

fn tmp_store(tag: &str) -> Store {
    let dir = std::env::temp_dir().join(format!("wb-timeline-{}-{}", tag, std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    Store::open(&dir).expect("open store")
}

fn item(site: &str, key: &str, title: &str, url: &str, date: &str, body: &str) -> wb_core::intel::IntelItem {
    wb_core::intel::IntelItem {
        key: key.into(),
        site_id: site.into(),
        site_name: format!("{}-name", site),
        title: title.into(),
        org: "普农集团".into(),
        area: "铜陵".into(),
        date: date.into(),
        amount: Some("100万元".into()),
        url: url.into(),
        relevance: Relevance::Core,
        reasons: vec!["位置:标题仅含主题词，正文复核".into()],
        incomplete: false,
        manual_verdict: false,
        first_seen: date.into(),
        body_head: body.into(),
        native_code: None,
    }
}

/// 核心验收：同一项目的招标公告 + 中标结果公告，必须各成时间轴上的一个节点。
/// 这条同时是既有缺陷的回归——v3.1.0 会把后者合并进前者，只剩 1 个节点。
#[test]
fn same_project_two_announcements_give_two_timeline_nodes() {
    let store = tmp_store("two_nodes");
    let tender = item(
        "pjwnc",
        "pjwnc|http://www.pjwnc.com/a1.html",
        "综合农事服务中心无人机采购项目招标公告",
        "http://www.pjwnc.com/a1.html",
        "2026-10-20",
        "项目编号：TLTRZB【2026】074 预算100万元，采购植保无人机2台",
    );
    let result = item(
        "pjwnc",
        "pjwnc|http://www.pjwnc.com/a2.html",
        "综合农事服务中心无人机采购项目中标（成交）结果公告",
        "http://www.pjwnc.com/a2.html",
        "2026-11-10",
        "项目编号：TLTRZB【2026】074 中标供应商：某某航空科技有限公司",
    );

    let r1 = persist_fetch(&store, &[tender], Some("2026-10-20T00:00:00Z")).unwrap();
    let r2 = persist_fetch(&store, &[result], Some("2026-11-10T00:00:00Z")).unwrap();
    assert_eq!(r1.inserted, 1, "招标公告应新建节点: {:?}", r1);
    assert_eq!(r2.inserted, 1, "中标公告必须新建节点，不能被并入招标公告: {:?}", r2);
    assert_eq!(r2.merged, 0, "同项目不同阶段公告不是跨源副本: {:?}", r2);

    let agg = read_project_timeline(&store, "code:tltrzb2026074")
        .unwrap()
        .expect("应查到这个项目");
    assert_eq!(agg.node_count, 2, "时间轴必须有两个节点，实际 {}: {:?}", agg.node_count,
        agg.nodes.iter().map(|n| (&n.title, &n.stage)).collect::<Vec<_>>());
    assert_eq!(agg.nodes[0].stage, "review", "首节点是评审（招标）");
    assert_eq!(agg.nodes[1].stage, "contract", "次节点是签约（中标结果）");
    assert_eq!(agg.current_stage, "contract", "当前阶段取最大进程序");
    assert_eq!(agg.skipped_stages, 1, "评审→签约之间跳过 1 段（意向）");
    assert_eq!(agg.first_date, "2026-10-20");
    assert_eq!(agg.last_date, "2026-11-10");
    // 中标公告的金额/正文必须还在——这是旧实现里被 alias_urls 顶掉的内容
    assert!(agg.nodes[1].body_head.contains("中标供应商"), "中标公告内容不能丢");
}

/// 跨源转载副本仍必须合并：同项目同阶段同日期，不同链接。
#[test]
fn cross_source_reprint_merges_into_one_node() {
    let store = tmp_store("reprint");
    let a = item("pjwnc", "pjwnc|http://a/1", "XX无人机采购项目招标公告", "http://a/1", "2026-10-20",
        "项目编号：TLTRZB【2026】074");
    let mut b = item("yibao", "yibao|http://y/9", "【转载】XX无人机采购项目招标公告", "http://y/9", "2026-10-20",
        "项目编号：TLTRZB【2026】074");
    b.site_id = "yibao".into();
    persist_fetch(&store, &[a], Some("2026-10-20T00:00:00Z")).unwrap();
    let r2 = persist_fetch(&store, &[b], Some("2026-10-20T01:00:00Z")).unwrap();
    assert_eq!(r2.merged, 1, "同阶段同日期的转载应并入: {:?}", r2);
    assert_eq!(r2.inserted, 0);
    let agg = read_project_timeline(&store, "code:tltrzb2026074").unwrap().unwrap();
    assert_eq!(agg.node_count, 1, "转载副本不新增节点");
    assert_eq!(agg.nodes[0].alias_urls.len(), 1, "转载链接记为 alias");
}

/// 同一公告重抓幂等：不新增节点、不产生暗重复。
#[test]
fn refetch_same_announcement_is_idempotent() {
    let store = tmp_store("idem");
    let a = item("pjwnc", "pjwnc|http://a/1", "XX无人机采购项目招标公告", "http://a/1", "2026-10-20",
        "项目编号：TLTRZB【2026】074");
    let r1 = persist_fetch(&store, &[a.clone()], Some("2026-10-20T00:00:00Z")).unwrap();
    assert_eq!(r1.inserted, 1);
    let r2 = persist_fetch(&store, &[a], Some("2026-10-21T00:00:00Z")).unwrap();
    assert_eq!(r2.inserted, 0);
    assert_eq!(r2.unchanged, 1, "同内容重抓应 unchanged: {:?}", r2);
    let agg = read_project_timeline(&store, "code:tltrzb2026074").unwrap().unwrap();
    assert_eq!(agg.node_count, 1);
}

/// 线索池仍按项目一条：后续阶段公告不重复造线索。
#[test]
fn lead_pool_stays_one_per_project() {
    let store = tmp_store("pool");
    let a = item("pjwnc", "pjwnc|http://a/1", "XX无人机采购项目招标公告", "http://a/1", "2026-10-20",
        "项目编号：TLTRZB【2026】074");
    let b = item("pjwnc", "pjwnc|http://a/2", "XX无人机采购项目中标结果公告", "http://a/2", "2026-11-10",
        "项目编号：TLTRZB【2026】074");
    persist_fetch(&store, &[a], Some("2026-10-20T00:00:00Z")).unwrap();
    persist_fetch(&store, &[b], Some("2026-11-10T00:00:00Z")).unwrap();
    let (recs, keys, leads, lead_keys) = wb_core::intel_store::dedup_audit(&store).unwrap();
    assert_eq!(recs, 2, "两个公告节点");
    assert_eq!(keys, 2, "两个 biz_key");
    assert_eq!(leads, 1, "线索池只有 1 条（按项目）");
    assert_eq!(lead_keys, 1);
}

/// 阶段未知的节点不被丢弃，落在末尾并保留待确认来源。
#[test]
fn unknown_stage_node_is_kept_not_dropped() {
    let store = tmp_store("unknown");
    let a = item("pjwnc", "pjwnc|http://a/1", "XX无人机采购项目招标公告", "http://a/1", "2026-10-20",
        "项目编号：TLTRZB【2026】074");
    let b = item("pjwnc", "pjwnc|http://a/3", "XX项目情况说明", "http://a/3", "2026-10-25",
        "项目编号：TLTRZB【2026】074 补充说明");
    persist_fetch(&store, &[a], Some("2026-10-20T00:00:00Z")).unwrap();
    persist_fetch(&store, &[b], Some("2026-10-25T00:00:00Z")).unwrap();
    let agg = read_project_timeline(&store, "code:tltrzb2026074").unwrap().unwrap();
    assert_eq!(agg.node_count, 2, "未知阶段节点不得被丢弃");
    assert_eq!(agg.nodes.last().unwrap().stage, "unknown", "未知阶段排在末尾");
    assert_eq!(agg.nodes.last().unwrap().stage_source, "unknown");
}

/// 无项目编号的公告：退回指纹键，仍能按项目聚合（且不因缺编号而崩）。
#[test]
fn project_without_code_still_aggregates() {
    let store = tmp_store("nocode");
    let a = item("pjwnc", "pjwnc|http://n/1", "某单位办公家具采购招标公告", "http://n/1", "2026-10-20", "无编号正文");
    let b = item("pjwnc", "pjwnc|http://n/2", "某单位办公家具采购中标结果公告", "http://n/2", "2026-11-01", "无编号正文");
    persist_fetch(&store, &[a], Some("2026-10-20T00:00:00Z")).unwrap();
    persist_fetch(&store, &[b], Some("2026-11-01T00:00:00Z")).unwrap();
    let projects = list_projects(&store, None, 0).unwrap();
    // 无编号项目按标题+日期成 fp 键 → 两条公告分属两个项目，这里只断言不崩、能列出
    assert!(!projects.is_empty(), "无编号记录必须仍可列出");
    for p in &projects {
        assert_eq!(p.dedup_layer, "L2");
    }
}

/// 按阶段筛选项目。
#[test]
fn list_projects_filters_by_current_stage() {
    let store = tmp_store("filter");
    let a = item("pjwnc", "pjwnc|http://f/1", "XX无人机采购项目招标公告", "http://f/1", "2026-10-20",
        "项目编号：TLTRZB【2026】074");
    persist_fetch(&store, &[a], Some("2026-10-20T00:00:00Z")).unwrap();
    let all = list_projects(&store, None, 0).unwrap();
    assert!(!all.is_empty());
    let review_only = list_projects(&store, Some(Stage::Review), 0).unwrap();
    assert_eq!(review_only.len(), all.len(), "当前阶段就是评审");
    let contract_only = list_projects(&store, Some(Stage::Contract), 0).unwrap();
    assert!(contract_only.is_empty(), "没有项目处于签约阶段");
}

/// 判据来源要能区分 A（rule）与 B（heuristic），界面据此挂待确认。
#[test]
fn stage_source_distinguishes_rule_from_heuristic() {
    let v = judge("XX无人机采购项目招标公告", "", 0, 2);
    assert_eq!(v.source, StageSource::Rule);
    let v2 = judge("XX项目情况说明", "", 1, 3);
    assert_eq!(v2.source, StageSource::Unknown, "中间位置 A/B 都判不出");
    let t = stage_from_text("中标通知书", "");
    assert!(t.is_some(), "「中标通知书」含『中标』应命中签约族");
}
