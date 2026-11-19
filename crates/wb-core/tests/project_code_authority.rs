// v3.2.0 · 项目编号来源优先级：接口权威字段 projectno > 正文文本抽取
//
// 背景（真实样本实测，合肥公共资源交易中心 2026-08~11「无人机」50 条）：
// 正文里编号与后文之间没有分隔符时，抽取器会把后续数字吞进编号——
//   `招标项目编号：2026BFBAZ51418` 紧跟 `2.3 标段划分` → 抽成 `2026bfbaz514182`
//   `项目编号：2026BFFHZ01387-1`（`-1` 是分包号）      → 抽成 `2026bffhz013871`
// 同一项目因此得到两个不同的 L3 键，聚合视图把同一项目拆成两张卡。
//
// 接口本身在每个记录上返回 `projectno` 字段（权威值）。本测试锁定「有权威字段时
// 以它为准、没有时才退回文本抽取」这条口径。

use std::collections::BTreeMap;

use wb_core::lead_dedup::{dedup_batch, DedupAction, DedupInput, DedupKey, DedupLayer};

fn inp(site: &str, nid: &str, title: &str, body: &str, code: Option<&str>) -> DedupInput {
    DedupInput {
        site_id: site.into(),
        native_id: nid.into(),
        title: title.into(),
        body: body.into(),
        date: "2026-09-15".into(),
        native_code: code.map(|c| c.to_string()),
    }
}

#[test]
fn native_code_beats_text_extraction_when_body_has_no_separator() {
    // 同一项目的两条公告：正文形态一条干净、一条把章节号吞进编号。
    // 权威字段都是 2026BFBAZ51418。
    let a = inp(
        "hefei",
        "hefei|u1",
        "紫云山AI机器人科技文化基地设备采购及配套安装项目公开招标公告（评定分离）",
        // 真实形态：编号与后续章节号之间**没有分隔符**（合肥源原文即如此，
        // `2026BFBAZ514182.3 标段划分`），这正是粘连的来源。
        "招标项目编号：2026BFBAZ514182.3 标段划分：本招标项目共划分1个标段。",
        Some("2026BFBAZ51418"),
    );
    let b = inp(
        "hefei",
        "hefei|u2",
        "紫云山AI机器人科技文化基地设备采购及配套安装项目（二次）公开招标公告（评定分离）",
        "招标项目编号：2026BFBAZ51418",
        Some("2026BFBAZ51418"),
    );
    let inputs = vec![a, b];
    let out = dedup_batch(&BTreeMap::<String, DedupKey>::new(), inputs.iter());

    // 第一条落新键
    assert!(matches!(out[0].action, DedupAction::Insert { .. }), "首条应新建");
    // 第二条必须并入第一条（同一权威编号），而不是因正文粘连另起一链
    match &out[1].action {
        DedupAction::Merge { with, layer } => {
            assert_eq!(*layer, DedupLayer::ProjectCode, "应命中 L3 项目编号层");
            assert!(with.contains("2026bfbaz51418"), "合并键应是权威编号归一值，实际 {with}");
            assert!(!with.contains("2026bfbaz514182"), "不得使用吞了章节号的粘连编号，实际 {with}");
        }
        DedupAction::Insert { key } => panic!("同一项目的第二条被当成了新项目：{}", key.key),
    }
}

#[test]
fn native_code_ignores_subpackage_suffix() {
    // `项目编号：2026BFFHZ01387-1` 中的 `-1` 是分包号，不属于项目编号。
    // 有了权威字段，两条不同分包的公告应该归到同一项目。
    let a = inp("hefei", "hefei|x1", "合肥市疾病预防控制中心2026年专项资产购置（二次）",
                "项目编号：2026BFFHZ01387 二、项目名称", Some("2026BFFHZ01387"));
    let b = inp("hefei", "hefei|x2", "合肥市疾病预防控制中心2026年专项资产购置第1包（二次）",
                "项目编号：     2026BFFHZ01387-1 四、项目名称", Some("2026BFFHZ01387"));
    let inputs = vec![a, b];
    let out = dedup_batch(&BTreeMap::<String, DedupKey>::new(), inputs.iter());

    assert!(matches!(out[0].action, DedupAction::Insert { .. }));
    match &out[1].action {
        DedupAction::Merge { with, layer } => {
            assert_eq!(*layer, DedupLayer::ProjectCode);
            assert!(with.contains("2026bffhz01387"), "分包号不应改变项目键，实际 {with}");
        }
        DedupAction::Insert { key } => panic!("分包公告被拆成新项目：{}", key.key),
    }
}

#[test]
fn falls_back_to_text_extraction_without_native_code() {
    // 无权威字段的源（企业官网 / 聚合站）保持原行为：正文抽取仍生效。
    let a = inp("some_site", "s|1", "某项目招标公告", "项目编号：TLTRZB【2026】074", None);
    let inputs = vec![a];
    let out = dedup_batch(&BTreeMap::<String, DedupKey>::new(), inputs.iter());
    match &out[0].action {
        DedupAction::Insert { key } => {
            assert!(key.key.contains("tltrzb2026074"), "退回文本抽取时应得到归一编号，实际 {}", key.key);
        }
        other => panic!("单条应新建，实际 {other:?}"),
    }
}

#[test]
fn native_code_wins_even_if_body_says_something_else() {
    // 权威字段与正文冲突时以字段为准（正文可能是往期公告的复制粘贴残留）。
    let a = inp("hefei", "hefei|y1", "某项目中标结果公告", "项目编号：2026AAAAA00001", Some("2026BBBBB00002"));
    let out = dedup_batch(&BTreeMap::<String, DedupKey>::new(), vec![&a]);
    match &out[0].action {
        DedupAction::Insert { key } => {
            assert!(key.key.contains("2026bbbbb00002"), "应以接口字段为准，实际 {}", key.key);
            assert!(!key.key.contains("2026aaaaa00001"), "不应采用正文里的旧编号，实际 {}", key.key);
        }
        other => panic!("单条应新建，实际 {other:?}"),
    }
}
