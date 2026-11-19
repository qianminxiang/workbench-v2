//! v3.2.0 T3「按项目聚合查询」的命令层验收。
//!
//! 为什么要这一层：`wb-core` 里的 `list_projects` / `read_project_timeline` 已由
//! `tests/project_timeline.rs` 覆盖，但那证明的是「函数返回对不对」；本文件证明
//! **命令真的能跑通并输出可用 JSON**——真起子进程、真读 SQLite、真解析 stdout。
//! 只看函数返回值会漏掉「参数解析错、把非法值静默当不过滤、输出字段名对不上」这类
//! 只在命令层才犯的错。

use std::path::PathBuf;
use std::process::Command;

use wb_core::intel::Relevance;
use wb_core::intel_store::persist_fetch;
use wb_core::store::Store;

fn tmp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("wb-wbd-project-{}-{}", tag, std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("mk dir");
    dir
}

fn item(site: &str, key: &str, title: &str, date: &str, body: &str) -> wb_core::intel::IntelItem {
    wb_core::intel::IntelItem {
        key: key.into(),
        site_id: site.into(),
        site_name: format!("{site}-name"),
        title: title.into(),
        org: "普农集团".into(),
        area: "铜陵".into(),
        date: date.into(),
        amount: Some("100万元".into()),
        url: format!("http://example.invalid/{key}"),
        relevance: Relevance::Core,
        reasons: vec!["位置:标题仅含主题词，正文复核".into()],
        incomplete: false,
        manual_verdict: false,
        first_seen: date.into(),
        body_head: body.into(),
        native_code: None,
    }
}

/// 造一个真实库：同一项目（同一编号）的招标公告 + 中标结果公告 + 一条无编号公告。
fn seeded_store(tag: &str) -> PathBuf {
    let dir = tmp_dir(tag);
    let store = Store::open(&dir).expect("open store");
    let items = vec![
        item(
            "hfggfc",
            "hfggfc|tender",
            "某无人机采购项目招标公告",
            "2026-06-01",
            "项目编号：TLTRZB【2026】074",
        ),
        item(
            "hfggfc",
            "hfggfc|result",
            "某无人机采购项目中标（成交）结果公告",
            "2026-07-10",
            "项目编号：TLTRZB【2026】074",
        ),
        // 无编号公告：应落到 fp:/body: 键，独立成为一个项目，不能被并进上面那个
        item(
            "pjwnc",
            "pjwnc|nocode",
            "综合农事服务中心无人机作业服务",
            "2026-08-01",
            "无项目编号，只有正文描述",
        ),
    ];
    persist_fetch(&store, &items, Some("2026-08-02T00:00:00Z")).expect("persist");
    dir
}

/// 跑 `wbd project ...`，返回 (退出码, stdout, stderr)。
fn run_wbd(args: &[&str]) -> (i32, String, String) {
    let exe = env!("CARGO_BIN_EXE_wbd");
    let out = Command::new(exe).args(args).output().expect("run wbd");
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).to_string(),
        String::from_utf8_lossy(&out.stderr).to_string(),
    )
}

/// list 必须真从库里查出项目，且字段能反推出「同一项目两个阶段」这个核心事实。
#[test]
fn project_list_reads_sqlite_and_reports_stage_and_node_count() {
    let dir = seeded_store("list");
    let d = dir.to_string_lossy().to_string();
    let (code, stdout, stderr) = run_wbd(&["project", &d, "list"]);
    assert_eq!(code, 0, "list 应成功，实际退出码 {code}，stderr={stderr}");

    let v: serde_json::Value = serde_json::from_str(&stdout).expect("stdout 必须是合法 JSON");
    let total = v["total"].as_u64().expect("total 必须是数字");
    // 有编号的项目 1 个 + 无编号项目 1 个 = 2
    assert_eq!(total, 2, "项目数应为 2，实际输出：{stdout}");

    let projects = v["projects"].as_array().expect("projects 必须是数组");
    let with_code = projects
        .iter()
        .find(|p| p["project_code"].as_str().unwrap_or("").contains("tltrzb2026074"))
        .unwrap_or_else(|| panic!("未找到编号项目，实际输出：{stdout}"));

    assert_eq!(
        with_code["node_count"].as_u64(),
        Some(2),
        "同一项目的招标 + 中标应是 2 个节点，实际 {with_code}"
    );
    // 时间轴按进程序排，最后节点是签约（中标结果），所以当前阶段应是签约
    assert_eq!(
        with_code["current_stage"].as_str(),
        Some("contract"),
        "当前阶段应是 contract（中标结果公告），实际 {with_code}"
    );
    assert_eq!(with_code["current_stage_label"].as_str(), Some("签约"));
    assert_eq!(with_code["first_date"].as_str(), Some("2026-06-01"));
    assert_eq!(with_code["last_date"].as_str(), Some("2026-07-10"));
    // 跳段语义 = 「从 0 到 max_stage_order 之间有几个段没有任何节点」。
    // 本项目节点是 review(1) 与 contract(2)，max=2，缺 intent(0) 一段 ⇒ 1。
    // （这里先读实现确认语义再写断言：一开始按「相邻节点之间是否跳」想当然写成 0，被测试抓到。）
    assert_eq!(with_code["skipped_stages"].as_u64(), Some(1), "实际 {with_code}");
    assert_eq!(with_code["max_stage_order"].as_u64(), Some(2), "实际 {with_code}");
}

/// show 必须能取到一条按进程序排的时间轴，且节点标题/阶段逐项正确。
#[test]
fn project_show_returns_ordered_timeline() {
    let dir = seeded_store("show");
    let d = dir.to_string_lossy().to_string();
    let (code, stdout, stderr) = run_wbd(&["project", &d, "show", "code:tltrzb2026074"]);
    assert_eq!(code, 0, "show 应成功，退出码 {code}，stderr={stderr}");
    let v: serde_json::Value = serde_json::from_str(&stdout).expect("stdout 必须是合法 JSON");
    let nodes = v["nodes"].as_array().expect("nodes 必须是数组");
    assert_eq!(nodes.len(), 2, "时间轴应有 2 个节点，实际 {stdout}");
    // 第一个节点是招标公告（评审），第二个是中标结果（签约）
    assert_eq!(nodes[0]["stage"].as_str(), Some("review"), "实际 {}", nodes[0]);
    assert_eq!(nodes[1]["stage"].as_str(), Some("contract"), "实际 {}", nodes[1]);
    assert_eq!(nodes[0]["date"].as_str(), Some("2026-06-01"));
    assert_eq!(nodes[1]["date"].as_str(), Some("2026-07-10"));
}

/// 不存在的项目键必须明确失败（退出码非 0），不能返回空 JSON 冒充「查到了」。
#[test]
fn project_show_on_unknown_key_fails_loudly() {
    let dir = seeded_store("missing");
    let d = dir.to_string_lossy().to_string();
    let (code, _stdout, stderr) = run_wbd(&["project", &d, "show", "code:no-such-project"]);
    assert_ne!(code, 0, "查不到项目时退出码应非 0");
    assert!(
        stderr.contains("未找到项目"),
        "错误信息应说明未找到，实际 stderr={stderr}"
    );
}

/// 非法 --stage 必须报错退出，**不能静默当成「不过滤」**。
/// 这是命令层最容易犯的错：用户以为筛了「运维阶段」，实际拿到全部项目。
///
/// 断言不写死整条文案（文案会随共享校验函数演进），只要求三件事：
/// ① 退出码非 0；② 提到是 `--stage` 的问题；③ 把用户传的非法值回显出来。
/// 第 ③ 条很重要——只说「参数非法」而不回显值，用户不知道该改哪个参数。
#[test]
fn invalid_stage_is_rejected_not_silently_ignored() {
    let dir = seeded_store("badstage");
    let d = dir.to_string_lossy().to_string();
    let (code, stdout, stderr) = run_wbd(&["project", &d, "list", "--stage", "运维"]);
    assert_ne!(code, 0, "非法 --stage 应失败，实际退出码 {code}，stdout={stdout}");
    assert!(stdout.trim().is_empty(), "失败时不应再输出结果 JSON，实际 stdout={stdout}");
    assert!(
        stderr.contains("--stage"),
        "错误信息应指明是 --stage 参数，实际 stderr={stderr}"
    );
    assert!(
        stderr.contains("非法"),
        "错误信息应说明参数非法，实际 stderr={stderr}"
    );
    assert!(
        stderr.contains("运维"),
        "错误信息应回显用户传入的非法值，实际 stderr={stderr}"
    );
}

/// 合法 --stage 过滤必须真的过滤，而不是原样返回全部。
#[test]
fn valid_stage_filter_actually_filters() {
    let dir = seeded_store("filter");
    let d = dir.to_string_lossy().to_string();
    let (all_code, all_out, _e1) = run_wbd(&["project", &d, "list"]);
    assert_eq!(all_code, 0);
    let all: serde_json::Value = serde_json::from_str(&all_out).unwrap();
    assert_eq!(all["total"].as_u64(), Some(2), "不过滤应有 2 个项目");

    let (code, out, stderr) = run_wbd(&["project", &d, "list", "--stage", "contract"]);
    assert_eq!(code, 0, "合法 --stage 应成功，stderr={stderr}");
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(
        v["stage_filter"].as_str(),
        Some("contract"),
        "输出应回显生效的过滤条件，实际 {out}"
    );
    assert_eq!(
        v["total"].as_u64(),
        Some(1),
        "contract 阶段只有 1 个项目（招标那条是 review），实际 {out}"
    );
}

/// --limit 必须真截断（keyset 分页的前置）。
#[test]
fn limit_truncates_result() {
    let dir = seeded_store("limit");
    let d = dir.to_string_lossy().to_string();
    let (code, out, stderr) = run_wbd(&["project", &d, "list", "--limit", "1"]);
    assert_eq!(code, 0, "stderr={stderr}");
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["total"].as_u64(), Some(1), "limit=1 应只返回 1 个，实际 {out}");
    assert_eq!(v["limit"].as_u64(), Some(1));
}
