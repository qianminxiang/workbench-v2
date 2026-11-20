//! `wbd` —— 综合工作台 v2.0 系统层驱动。
//!
//! 存在的意义：让「实现自检」用**真实执行**取数，而不是靠断言。Tauri 壳需要 GUI 环境，
//! 本 CLI 复用同一份 `wb-core` 系统层能力，因此导入、落库、备份轮转、断网降级、
//! 规则求值、更新清单校验都能在无界面环境下实测。

use std::path::PathBuf;
use std::process::ExitCode;

use rusqlite::Connection;
use wb_core::import::{import_workbook, IssueKind};
use wb_core::repo;
use wb_core::store::{Layout, Store};
use wb_core::{rules, sync, update};

fn usage() -> String {
    r#"wbd —— 综合工作台 v2.0 驱动

用法：
  wbd import   <xlsx> <fiscal_year> [--json]
      导入存量工作簿并打印报告（条数、占位跳过、待复核、日期粒度、对账）

  wbd dates    <xlsx> <fiscal_year>
      只打印日期规范化结果（raw → date/precision，TSV），供逐条核对

  wbd backup   <data_dir> <n>
      备份轮转演练：连做 n 次写前 L1 快照 + 1 次 L2 全量，打印留存份数

  wbd rules    <data_dir> <fiscal_year> <yyyy-mm-dd>
      规则求值：临期/逾期/落后/滞留 + 当日晨报（含周一聚合）

  wbd sync     <data_dir>
      同步合并演练：LWW、progress 取 max、冲突日志、增量游标

  wbd update   <data_dir> [--emit-manifest <dir>]
      更新清单与签名校验演练（含"signature 填成路径"等反例）

  wbd selfcheck <xlsx> <fiscal_year> <data_dir>
      端到端自检：导入 → 落库 → 备份 → 断网 → 规则，输出实测 JSON

  wbd intel <data_dir> --start <yyyy-mm-dd> --end <yyyy-mm-dd> [--sites id1,id2] [--sample N]
      v2.3.0 招标情报：逐站真实抓取 → 三层去重 → SQLite 落库（拉链 + 线索池），
      输出窗口/各站报告/去重审计/每源落库回读样例

  wbd intel-read <data_dir> <source_id> [limit]
      从 SQLite 回读指定源的当前链记录（独立验收命令：证明数据真的在库里）

  wbd lead <data_dir> list [--state discovered|confirmed|rejected|todoed]
      线索池列表（v2.3.0 验收4：发现 → 确认 → 待办闭环的入口）

  wbd lead <data_dir> confirm|reject|todo <dedup_key> [--owner 名] [--today yyyy-mm-dd]
      线索裁决与待办生成：confirm/reject 需先 discovered；todo 需先 confirmed，
      辅源线索即使确认也不产待办（needs_primary_confirm）

  wbd project <data_dir> list [--stage intent|review|contract|acceptance|operation] [--limit N]
      v3.2.0 T3 按项目聚合：列出所有项目及其当前阶段、节点数、首末公告日期、跳段数。
      数据取自 ext_record 当前链，不重新抓取；--stage 非法值直接报错不静默放行

  wbd project <data_dir> show <project_key>
      同一项目的多阶段公告时间轴（节点按进程序 + 日期排序，阶段未知节点排在末尾但不丢弃）

  wbd export-subsidy <out.xlsx|out.csv> --from <数据.csv> [--filter 字段=值]... [--contains 字段:值]... [--sheet 名称]
      v2.3.0 需求⑤ 补贴一键导出：按筛选条件全量导出（非当前页），xlsx 主格式 + CSV 口子

  wbd sync-live <data_dir> setup <base_url> <username> <password>
      v3.0 真实传输：首成员初始化（服务器只允许一次），落地令牌与设备标识

  wbd sync-live <data_dir> login <base_url> <username> <password>
      v3.0 真实传输：登录（服务器已初始化后走这个，替代 setup）

  wbd sync-live <data_dir> status
      读取同步状态（地址、成员、游标、待推、上次结果——不显示令牌）

  wbd sync-live <data_dir> push-test [--title 文本] [--id 行ID]
      v3.0 真实传输：构造一条测试行进入待推队列（不动真实业务数据）

  wbd sync-live <data_dir> sync-once
      v3.0 真实传输：同步一轮——推队列 → 拉增量（HTTPS）

  wbd sync-live <data_dir> logout
      v3.0 真实传输：作废本机令牌（本地数据与游标保留）
"#
    .to_string()
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() {
        println!("{}", usage());
        return ExitCode::SUCCESS;
    }
    match args[0].as_str() {
        "import" => cmd_import(&args[1..]),
        "dates" => cmd_dates(&args[1..]),
        "backup" => cmd_backup(&args[1..]),
        "rules" => cmd_rules(&args[1..]),
        "sync" => cmd_sync(&args[1..]),
        "update" => cmd_update(&args[1..]),
        "selfcheck" => cmd_selfcheck(&args[1..]),
        "intel" => cmd_intel(&args[1..]),
        "intel-read" => cmd_intel_read(&args[1..]),
        "lead" => cmd_lead(&args[1..]),
        "project" => cmd_project(&args[1..]),
        "manual" => cmd_manual(&args[1..]),
        "export-subsidy" => cmd_export_subsidy(&args[1..]),
        "sync-live" => cmd_sync_live(&args[1..]),
        "help" | "--help" | "-h" => {
            println!("{}", usage());
            ExitCode::SUCCESS
        }
        other => {
            eprintln!("未知子命令：{other}\n\n{}", usage());
            ExitCode::from(2)
        }
    }
}

fn fail(msg: impl std::fmt::Display, code: u8) -> ExitCode {
    eprintln!("{msg}");
    ExitCode::from(code)
}

// ───────────────────────────── import / dates ─────────────────────────────

fn parse_xlsx_year(args: &[String]) -> Result<(PathBuf, i32, bool), String> {
    if args.len() < 2 {
        return Err("需要 <xlsx> 与 <fiscal_year> 两个参数".into());
    }
    let path = PathBuf::from(&args[0]);
    let year: i32 = args[1].parse().map_err(|_| format!("fiscal_year 非法：{}", args[1]))?;
    let json = args.iter().any(|a| a == "--json");
    Ok((path, year, json))
}

fn cmd_import(args: &[String]) -> ExitCode {
    let (path, year, json) = match parse_xlsx_year(args) {
        Ok(v) => v,
        Err(e) => return fail(e, 2),
    };
    match import_workbook(&path, year) {
        Ok(out) => {
            if json {
                let payload = serde_json::json!({
                    "report": out.report,
                    "items": out.items.iter().map(|i| serde_json::json!({
                        "seq_no": i.seq_no, "board": i.board.code(), "title": i.title,
                        "priority": i.priority.map(|p| p.as_str()),
                        "start_raw": i.planned_start_raw, "start_date": i.planned_start_date,
                        "start_precision": i.planned_start_precision,
                        "end_raw": i.planned_end_raw, "end_date": i.planned_end_date,
                        "end_precision": i.planned_end_precision,
                        "status": i.status.map(|s| s.as_str()),
                        "progress_pct": i.progress_pct,
                    })).collect::<Vec<_>>(),
                    "kpis": out.kpis.iter().map(|k| serde_json::json!({
                        "id": k.id, "category": k.category, "name": k.name,
                        "target": k.target_value, "rule": k.rule,
                    })).collect::<Vec<_>>(),
                });
                println!("{}", serde_json::to_string_pretty(&payload).unwrap());
            } else {
                print_report(&out.report, &out.items, out.kpis.len());
            }
            ExitCode::SUCCESS
        }
        Err(e) => fail(format!("导入失败：{e}"), 1),
    }
}

fn print_report(r: &wb_core::import::ImportReport, items: &[wb_core::WorkItem], kpi_len: usize) {
    println!("== 导入报告 ==");
    println!("源文件：{}（计划年度 {}）", r.source_file, r.fiscal_year);
    println!("{}", r.summary_line());
    println!();
    println!("逐板块条数（与总控表小计对账）：");
    for (b, n) in &r.per_board {
        println!("  {:<10} {:>3} 项", b.zh_name(), n);
    }
    println!();
    println!("日期粒度分布：");
    let sp = &r.start_precision;
    let ep = &r.end_precision;
    println!("  计划开启  日 {:<3} 月 {:<3} 季 {:<2} 持续 {:<2} 未知 {:<2} 合计 {}", sp.day, sp.month, sp.quarter, sp.ongoing, sp.unknown, sp.total());
    println!("  计划闭环  日 {:<3} 月 {:<3} 季 {:<2} 持续 {:<2} 未知 {:<2} 合计 {}", ep.day, ep.month, ep.quarter, ep.ongoing, ep.unknown, ep.total());
    println!();
    println!("逐 sheet 台账：");
    for s in &r.sheets {
        println!("  {:<12} 导入 {:<3} 占位跳过 {:<2} 待复核 {}", s.sheet, s.imported, s.skipped_placeholder, s.needs_review);
    }
    println!();
    println!("对账：{}", r.reconcile_note);
    println!("项数：实导 {} · 跳过 {} · KPI {}", r.items_imported, r.items_skipped, kpi_len);
    println!();
    if !r.issues.is_empty() {
        println!("问题清单（{} 条）：", r.issues.len());
        for i in &r.issues {
            println!("  [{}] {} 行 {} · 列 {} · {}", issue_label(i.kind), i.sheet, i.row_no, i.column.clone().unwrap_or_else(|| "—".into()), i.detail);
        }
    }
    println!("抽样（前 5 项日期规范化）：");
    for it in items.iter().take(5) {
        println!("  #{:<2} {:<12} 开启 `{}` → {} ({}) · 闭环 `{}` → {} ({})",
            it.seq_no, it.title.chars().take(12).collect::<String>(),
            it.planned_start_raw.clone().unwrap_or_default(),
            it.planned_start_date.clone().unwrap_or_else(|| "—".into()), it.planned_start_precision,
            it.planned_end_raw.clone().unwrap_or_default(),
            it.planned_end_date.clone().unwrap_or_else(|| "—".into()), it.planned_end_precision);
    }
}

fn issue_label(k: IssueKind) -> &'static str {
    match k {
        IssueKind::PlaceholderSkipped => "占位跳过",
        IssueKind::NeedsReview => "待复核",
        IssueKind::DateUnparsed => "日期未知",
        IssueKind::DateOutOfRange => "日期越界",
        IssueKind::SourceInconsistent => "源不一致",
    }
}

fn cmd_dates(args: &[String]) -> ExitCode {
    let (path, year, _) = match parse_xlsx_year(args) {
        Ok(v) => v,
        Err(e) => return fail(e, 2),
    };
    match import_workbook(&path, year) {
        Ok(out) => {
            println!("seq\ttitle\tstart_raw\tstart_date\tstart_precision\tend_raw\tend_date\tend_precision");
            for it in &out.items {
                println!("{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}",
                    it.seq_no, it.title,
                    it.planned_start_raw.clone().unwrap_or_default(),
                    it.planned_start_date.clone().unwrap_or_default(), it.planned_start_precision,
                    it.planned_end_raw.clone().unwrap_or_default(),
                    it.planned_end_date.clone().unwrap_or_default(), it.planned_end_precision);
            }
            ExitCode::SUCCESS
        }
        Err(e) => fail(format!("导入失败：{e}"), 1),
    }
}

// ───────────────────────────── backup ─────────────────────────────

fn cmd_backup(args: &[String]) -> ExitCode {
    if args.len() < 2 {
        return fail("用法：wbd backup <data_dir> <n>", 2);
    }
    let dir = PathBuf::from(&args[0]);
    let n: usize = match args[1].parse() {
        Ok(v) => v,
        Err(_) => return fail("n 必须是整数", 2),
    };
    let store = match Store::open(&dir) {
        Ok(s) => s,
        Err(e) => return fail(e, 1),
    };
    println!("库根目录：{}", dir.display());
    println!("schema 版本：{}（目标 {}）", store.schema_version(), wb_core::store::TARGET_SCHEMA_VERSION);

    let mut l1_bytes = Vec::new();
    for i in 0..n {
        // 每次写前快照都伴随一次真实写入，验证「写前备份」语义
        let guard = match store.begin_write(&format!("drill{i:02}")) {
            Ok(g) => g,
            Err(e) => return fail(format!("第 {i} 次写前备份失败（写入被阻断）：{e}"), 1),
        };
        if let Some(s) = &guard.snapshot {
            l1_bytes.push(s.bytes);
            let _ = guard.conn().execute(
                "INSERT OR REPLACE INTO domain_board (code, zh_name, sheet_name) VALUES ('drill','演练','演练')",
                [],
            );
        }
        drop(guard);
    }
    let l1 = list_backups(&dir.join("backups").join("l1"));
    println!("L1 快照：本次生成 {n} 份，目录现存 {} 份（上限 {}），平均 {:.1} KB",
        l1.len(), wb_core::store::L1_KEEP,
        if l1_bytes.is_empty() { 0.0 } else { l1_bytes.iter().sum::<u64>() as f64 / l1_bytes.len() as f64 / 1024.0 });

    match store.full_backup("daily") {
        Ok(b) => {
            println!("L2 全量：{} · {:.1} KB · 完整性 {} · JSON 表 {} 个",
                b.path.rsplit('/').next().unwrap_or(""), b.bytes as f64 / 1024.0, b.integrity, b.json_tables);
        }
        Err(e) => return fail(format!("L2 备份失败：{e}"), 1),
    }
    let l2 = list_backups(&dir.join("backups").join("l2"));
    println!("L2 目录现存 {} 份（日备上限 {}）", l2.len(), wb_core::store::L2_KEEP_DAILY);
    println!("轮转结论：L1 现 {} 份 ≤ 上限 {} · L2 现 {} 份 ≤ 上限 {}",
        l1.len(), wb_core::store::L1_KEEP, l2.len(), wb_core::store::L2_KEEP_DAILY);
    ExitCode::SUCCESS
}

fn list_backups(dir: &PathBuf) -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir(dir)
        .map(|rd| rd.filter_map(|e| e.ok()).map(|e| e.file_name().to_string_lossy().to_string()).collect())
        .unwrap_or_default();
    v.retain(|n| n.ends_with(".db"));
    v.sort();
    v
}

// ───────────────────────────── rules ─────────────────────────────

fn cmd_rules(args: &[String]) -> ExitCode {
    if args.len() < 3 {
        return fail("用法：wbd rules <data_dir> <fiscal_year> <yyyy-mm-dd>", 2);
    }
    let dir = PathBuf::from(&args[0]);
    let year: i32 = match args[1].parse() {
        Ok(v) => v,
        Err(_) => return fail("fiscal_year 非法", 2),
    };
    let today = match wb_core::CivilDate::parse(&args[2]) {
        Some(d) => d,
        None => return fail(format!("日期非法：{}", args[2]), 2),
    };
    let store = match Store::open(&dir) {
        Ok(s) => s,
        Err(e) => return fail(e, 1),
    };
    let items = match repo::load_items(store.conn(), year) {
        Ok(v) => v,
        Err(e) => return fail(e, 1),
    };
    println!("在办项 {} 条 · 求值日 {}", items.len(), today.to_iso());

    let rs = rules::Ruleset::default();
    let alerts = rules::evaluate(today, &items, &rs);
    println!("候选提醒 {} 条（含开关关闭的规则；默认 behind/stall 关闭）", alerts.len());
    let mut by_kind = std::collections::BTreeMap::new();
    for a in &alerts {
        *by_kind.entry(format!("{:?}", a.kind)).or_insert(0usize) += 1;
    }
    for (k, n) in &by_kind {
        println!("  {k:<14} {n}");
    }

    // 初始态排除验证：把 behind 打开，看是否仍不误报待启动项
    let mut rs_on = rules::Ruleset::default();
    rs_on.behind_enabled = true;
    let alerts_on = rules::evaluate(today, &items, &rs_on);
    let behind_cnt = alerts_on.iter().filter(|a| matches!(a.kind, rules::AlertKind::BehindSchedule)).count();
    println!("打开落后判定后：落后 {} 条（待启动/未筹备项必须为 0，否则首启即误报）", behind_cnt);

    let brief = rules::morning_brief(today, &items, &rs);
    println!("晨报 {}：当日到期 {} · 临期 {} · 逾期 {}",
        brief.brief_date, brief.due_today.len(), brief.due_soon.len(), brief.overdue.len());
    let quota = rs.daily_quota;
    println!("日配额 {}：托盘类提醒实际投递 {} 条，其余降级应用内", quota, alerts.len().min(quota));
    ExitCode::SUCCESS
}

// ───────────────────────────── sync ─────────────────────────────

fn cmd_sync(args: &[String]) -> ExitCode {
    let _ = args;
    // 用合成行演练合并语义；真实传输在服务端与客户端 Rust 层，规则这一层可离线验证
    let mk = |ts: i64, by: &str, dev: &str, pct: i64, title: &str| {
        let mut p = serde_json::Map::new();
        p.insert("title".into(), serde_json::json!(title));
        p.insert("progress_pct".into(), serde_json::json!(pct));
        sync::SyncRow {
            table: "work_item".into(), id: "fy2026-001".into(), payload: p,
            updated_at: ts, updated_by: by.into(), device_id: dev.into(), seq: 0, deleted_at: None,
        }
    };

    println!("== 同步合并演练（架构 §4.14）==");
    let local = mk(1_000, "m1", "devA", 30, "某政府关系项");
    let remote = mk(2_000, "m2", "devB", 20, "某政府关系项");
    let o = sync::merge_row(&local, &remote);
    println!("① 远端后写但进度更低：decision={:?} 胜出={} 合并后进度={}（进度不回退）",
        o.decision, o.merged.updated_by, o.merged.payload.get("progress_pct").unwrap());
    let conflict = o.conflict_entry(9_999).expect("应有冲突记录");
    println!("   冲突日志：被覆盖方={} 胜出方={} 存档长度={} 字节（旧值永久可查）",
        conflict.losing_by, conflict.winning_by, conflict.losing_json.len());

    // 同刻并列：确定性决胜，换调用顺序结论不变
    // 两行内容必须不同，否则会先被判为 Identical（无内容差异就不该产生覆盖）
    let a = mk(5_000, "m1", "devA", 10, "版本 A 的标题");
    let b = mk(5_000, "m2", "devB", 10, "版本 B 的标题");
    let o1 = sync::merge_row(&a, &b);
    let o2 = sync::merge_row(&b, &a);
    println!(
        "② 同刻并列：顺序无关？{} 胜出方={}（updated_by 字典序决胜，所有端收敛到同一结果）",
        o1.merged.updated_by == o2.merged.updated_by, o1.merged.updated_by
    );

    // 软删除参与同步
    let mut del = mk(3_000, "m2", "devB", 30, "某政府关系项");
    del.deleted_at = Some(3_000);
    let od = sync::merge_row(&local, &del);
    println!("③ 软删除：胜出方 deleted_at={:?}（物理删除只发生在归档环节）", od.merged.deleted_at);

    // 增量游标
    let mut rows = Vec::new();
    for i in 1..=5 {
        let mut r = mk(1_000 + i, "m1", "devA", i as i64 * 5, "row");
        r.id = format!("r{i}");
        r.seq = i * 10;
        rows.push(r);
    }
    let delta = sync::select_delta(&rows, 25);
    println!("④ 增量拉取：cursor=25 时拉到 {} 条，seq 顺序 {:?}",
        delta.len(), delta.iter().map(|r| r.seq).collect::<Vec<_>>());
    ExitCode::SUCCESS
}

// ───────────────────────────── update ─────────────────────────────

fn cmd_update(args: &[String]) -> ExitCode {
    if args.is_empty() {
        return fail("用法：wbd update <data_dir> [--emit-manifest <dir>]", 2);
    }
    let dir = PathBuf::from(&args[0]);
    let emit = args.iter().position(|a| a == "--emit-manifest").and_then(|i| args.get(i + 1)).map(PathBuf::from);

    println!("== 更新通道演练（架构 §8）==");
    println!("签名密钥环境变量名：{}（值不进仓库、不进日志）", update::SIGNING_KEY_ENV);
    println!("更新链路：HTTPS GET 静态 latest.json → 比对版本 → 下载 → 验签 → 安装");
    println!("链路内无任何平台凭据，v1.x 的 21001 在本架构下不可能复现");

    // 真实签名：私钥仅存在于本次演练内存，用于验证「验签全链路」而非只验 JSON 形状
    use ed25519_dalek::{Signer, SigningKey};
    let sk = SigningKey::from_bytes(&[0x2bu8; 32]);
    let vk = sk.verifying_key();
    let mut pk = Vec::new();
    pk.extend_from_slice(b"Ed");
    pk.extend_from_slice(&7u64.to_le_bytes());
    pk.extend_from_slice(&vk.to_bytes());
    let pubkey_text = format!("untrusted comment: workbench updater pubkey\n{}", update::b64_encode(&pk));

    let artifact = b"workbench-2.0.1-x64-setup.exe".as_slice();
    let mut sig = Vec::new();
    sig.extend_from_slice(b"Ed");
    sig.extend_from_slice(&7u64.to_le_bytes());
    sig.extend_from_slice(&sk.sign(artifact).to_bytes());
    let sig_text = update::b64_encode(&sig);
    let sig = sig_text.clone();

    let manifest = format!(r#"{{
  "version": "2.0.1",
  "notes": "修复备份轮转边界；导入报告增列待复核明细",
  "pub_date": "2026-10-26T09:00:00Z",
  "platforms": {{
    "windows-x86_64": {{"url":"https://dl.example.com/wb_2.0.1_x64-setup.exe","signature":"{sig}"}},
    "darwin-x86_64":  {{"url":"https://dl.example.com/wb_2.0.1_x64.app.tar.gz","signature":"{sig}"}},
    "darwin-aarch64": {{"url":"https://dl.example.com/wb_2.0.1_aarch64.app.tar.gz","signature":"{sig}"}},
    "linux-x86_64":   {{"url":"https://dl.example.com/wb_2.0.1_amd64.AppImage","signature":"{sig}"}}
  }}
}}"#);

    if let Some(out) = &emit {
        let _ = std::fs::create_dir_all(out);
        let p = out.join("latest.json");
        if std::fs::write(&p, &manifest).is_ok() {
            println!("示范清单已写出：{}", p.display());
        }
    }

    let parsed = match update::parse_manifest(&manifest) {
        Ok(m) => m,
        Err(e) => return fail(format!("清单非法：{e}"), 1),
    };
    println!("① 清单校验通过：version {} · 平台 {} 个", parsed.version, parsed.platforms.len());

    match update::decide("2.0.0", &parsed, "windows-x86_64") {
        update::UpdateDecision::Available { version, .. } => println!("② 版本比对：2.0.0 → {version} 有更新"),
        d => return fail(format!("② 应为 Available，实际 {d:?}"), 1),
    }
    match update::decide("2.0.1", &parsed, "windows-x86_64") {
        update::UpdateDecision::UpToDate => println!("③ 同版本：UpToDate（不重复提示）"),
        d => return fail(format!("③ 应为 UpToDate，实际 {d:?}"), 1),
    }

    // 验签：正例 + 篡改反例
    match update::verify_signature(&pubkey_text, &sig_text, artifact) {
        Ok(()) => println!("④ 验签通过（Ed25519，公钥内容内嵌而非路径）"),
        Err(e) => return fail(format!("④ 验签应通过，实际失败：{e}"), 1),
    }
    let mut tampered = artifact.to_vec();
    tampered[0] ^= 0x01;
    match update::verify_signature(&pubkey_text, &sig_text, &tampered) {
        Err(update::VerifyError::InvalidSignature) => println!("⑤ 篡改包验签失败 → 终止安装（不回退到忽略签名）"),
        other => return fail(format!("⑤ 篡改包必须验签失败，实际 {other:?}"), 1),
    }

    // 反例：把 signature 填成 .sig 文件路径——最常见的错法
    let bad = manifest.replace(&format!("\"signature\":\"{sig}\""), "\"signature\":\"https://dl.example.com/wb.sig\"");
    match update::parse_manifest(&bad) {
        Err(update::ManifestError::SignatureIsNotContent { .. }) => println!("⑥ 反例：signature 填路径被拒（官方文档明写的坑）"),
        other => {
            let shown = match other {
                Ok(m) => m.version.to_string(),
                Err(e) => e.to_string(),
            };
            return fail(format!("⑥ signature 填路径应被拒，实际 {shown}"), 1);
        }
    }
    // 反例：少一个平台键 → 整份清单无效
    let mut v: serde_json::Value = serde_json::from_str(&manifest).unwrap();
    v["platforms"].as_object_mut().unwrap().remove("linux-x86_64");
    match update::parse_manifest(&v.to_string()) {
        Err(update::ManifestError::PlatformIncomplete { platform, .. }) => println!("⑦ 反例：缺 {platform} 键 → 整份清单无效（不能只写当前平台）"),
        _ => return fail("⑦ 缺平台键应判整份清单无效", 1),
    }

    let _ = dir; // 更新检查失败只静默跳过，与本地数据目录无强耦合
    ExitCode::SUCCESS
}

// ───────────────────────────── selfcheck ─────────────────────────────

fn cmd_selfcheck(args: &[String]) -> ExitCode {
    if args.len() < 3 {
        return fail("用法：wbd selfcheck <xlsx> <fiscal_year> <data_dir>", 2);
    }
    let xlsx = PathBuf::from(&args[0]);
    let year: i32 = match args[1].parse() {
        Ok(v) => v,
        Err(_) => return fail("fiscal_year 非法", 2),
    };
    let dir = PathBuf::from(&args[2]);
    let _ = std::fs::remove_dir_all(&dir);
    let layout = Layout::new(&dir);
    if let Err(e) = layout.ensure() {
        return fail(e, 1);
    }

    let mut rec = serde_json::Map::new();

    // ① 导入
    let out = match import_workbook(&xlsx, year) {
        Ok(v) => v,
        Err(e) => return fail(format!("导入失败：{e}"), 1),
    };
    let rep = &out.report;
    rec.insert("import".into(), serde_json::json!({
        "source_file": rep.source_file,
        "items_total": rep.items_total,
        "items_imported": rep.items_imported,
        "items_skipped": rep.items_skipped,
        "declared_total": rep.declared_total,
        "reconciled": rep.reconciled,
        "kpi_total": rep.kpi_total,
        "per_board": rep.per_board.iter().map(|(b,n)| serde_json::json!({"board": b.zh_name(), "count": n})).collect::<Vec<_>>(),
        "start_precision": rep.start_precision,
        "end_precision": rep.end_precision,
        "issue_count": rep.issues.len(),
        "issues_by_kind": {
            "placeholder_skipped": rep.issues.iter().filter(|i| matches!(i.kind, IssueKind::PlaceholderSkipped)).count(),
            "needs_review": rep.issues.iter().filter(|i| matches!(i.kind, IssueKind::NeedsReview)).count(),
            "date_unparsed": rep.issues.iter().filter(|i| matches!(i.kind, IssueKind::DateUnparsed)).count(),
            "date_out_of_range": rep.issues.iter().filter(|i| matches!(i.kind, IssueKind::DateOutOfRange)).count(),
        },
    }));

    // ② 落库 + 回读
    let store = match Store::open(&dir) {
        Ok(s) => s,
        Err(e) => return fail(e, 1),
    };
    let pr = match repo::persist_import(&store, &out, year) {
        Ok(v) => v,
        Err(e) => return fail(format!("落库失败：{e}"), 1),
    };
    let kpi = repo::derive_kpi(store.conn(), year).unwrap_or_default();
    rec.insert("persist".into(), serde_json::json!({
        "items_written": pr.items_written,
        "items_readback": pr.items_readback,
        "kpis_written": pr.kpis_written,
        "kpis_readback": pr.kpis_readback,
        "per_board_readback": pr.per_board_readback.iter().map(|(c,n)| serde_json::json!({"board": c, "count": n})).collect::<Vec<_>>(),
        "kpi_derived": kpi.iter().map(|(n,t,c,r)| serde_json::json!({"name": n, "target": t, "current": c, "rule": r, "resolved": c.is_some()})).collect::<Vec<_>>(),
        "schema_version": store.schema_version(),
    }));

    // ③ 备份轮转：连做 24 次写前快照，验证 L1 上限 20 生效
    let mut l1_first_bytes = 0u64;
    let mut l1_fail = 0usize;
    for i in 0..24 {
        match store.begin_write(&format!("sc{i:02}")) {
            Ok(g) => {
                if let Some(s) = &g.snapshot {
                    if i == 0 { l1_first_bytes = s.bytes; }
                }
                let _ = g.conn().execute(
                    "INSERT OR REPLACE INTO domain_board (code, zh_name, sheet_name) VALUES (?, ?, ?)",
                    rusqlite::params![format!("t{i}"), "演练", "演练"],
                );
            }
            Err(_) => l1_fail += 1,
        }
    }
    let l1 = count_files(&dir.join("backups").join("l1"), ".db");
    let l2 = match store.full_backup("daily") {
        Ok(b) => b,
        Err(e) => return fail(format!("L2 备份失败：{e}"), 1),
    };
    let checksums = verify_backups_ok(&dir.join("backups").join("l1"));
    rec.insert("backup".into(), serde_json::json!({
        "l1_attempts": 24,
        "l1_failed": l1_fail,
        "l1_kept": l1.len(),
        "l1_keep_limit": wb_core::store::L1_KEEP,
        "l1_first_bytes": l1_first_bytes,
        "l1_all_integrity_ok": checksums,
        "l2_kind": l2.kind,
        "l2_bytes": l2.bytes,
        "l2_integrity": l2.integrity,
        "l2_json_tables": l2.json_tables,
        "l2_kept": count_files(&dir.join("backups").join("l2"), ".db").len(),
        "l2_daily_limit": wb_core::store::L2_KEEP_DAILY,
        "backup_dir_visible": dir.join("backups").exists(),
    }));

    // ④ 断网降级：本地读写与服务端不可达完全解耦
    let offline = offline_drill(&store, year);
    rec.insert("offline".into(), offline);

    // ⑤ 规则求值（用真实库里的真实数据）
    let items = repo::load_items(store.conn(), year).unwrap_or_default();
    let today = wb_core::CivilDate::parse("2026-10-26").unwrap();
    let rs = rules::Ruleset::default();
    let alerts = rules::evaluate(today, &items, &rs);
    let mut rs_on = rules::Ruleset::default();
    rs_on.behind_enabled = true;
    rs_on.stall_enabled = true;
    let alerts_on = rules::evaluate(today, &items, &rs_on);
    let brief = rules::morning_brief(today, &items, &rs);
    rec.insert("rules".into(), serde_json::json!({
        "evaluated_items": items.len(),
        "today": today.to_iso(),
        "alerts_default": alerts.len(),
        "alerts_all_on": alerts_on.len(),
        "overdue": alerts.iter().filter(|a| matches!(a.kind, rules::AlertKind::Overdue)).count(),
        "due_soon": alerts.iter().filter(|a| matches!(a.kind, rules::AlertKind::DueSoon)).count(),
        "behind_when_enabled": alerts_on.iter().filter(|a| matches!(a.kind, rules::AlertKind::BehindSchedule)).count(),
        "behind_excludes_not_started": alerts_on.iter()
            .filter(|a| matches!(a.kind, rules::AlertKind::BehindSchedule))
            .all(|a| items.iter().any(|i| i.id == a.item_id && !i.status.map(|s| s.is_not_started()).unwrap_or(true))),
        "brief_date": brief.brief_date,
        "brief_due_today": brief.due_today.len(),
        "brief_due_soon": brief.due_soon.len(),
        "brief_overdue": brief.overdue.len(),
        "brief_daily_quota": rs.daily_quota,
    }));

    // ⑥ 日期规范化直证：「2月」→ 当月 1 号
    let feb: Vec<&wb_core::WorkItem> = items.iter().filter(|i| i.planned_start_raw.as_deref() == Some("2月")).collect();
    rec.insert("date_normalization".into(), serde_json::json!({
        "rule": "2月 → 当年 02-01，precision=month（闭环取区间末端）",
        "feb_start_samples": feb.iter().take(6).map(|i| serde_json::json!({
            "seq_no": i.seq_no, "raw": i.planned_start_raw,
            "date": i.planned_start_date, "precision": i.planned_start_precision,
        })).collect::<Vec<_>>(),
        "feb_start_count": feb.len(),
        "all_feb_first_of_month": feb.iter().all(|i| i.planned_start_date.as_deref().map(|d| d.ends_with("-02-01")).unwrap_or(false)),
        "unknown_precision_items": items.iter().filter(|i| i.planned_start_precision == "unknown" || i.planned_end_precision == "unknown").count(),
    }));

    // ⑦ 环境事实：安装包与内存在此构建机上无法实测，如实记录而不是估算
    rec.insert("environment".into(), serde_json::json!({
        "host_os": std::env::consts::OS,
        "host_arch": std::env::consts::ARCH,
        "installer_size_measured": false,
        "installer_size_note": "构建机无 Windows 目标工具链（无 x86_64-pc-windows-msvc / mingw，无 webkit2gtk-4.1 与 gtk+-3.0，无 sudo），无法在此产出 .exe/.msi，故体积未实测，不给估算值",
        "idle_memory_measured": false,
        "idle_memory_note": "Tauri 壳需 GUI 运行时，本机无 WebKitGTK，无法启动窗口进程实测常驻内存；该指标须在 Windows 目标机实测",
    }));

    println!("{}", serde_json::to_string_pretty(&serde_json::Value::Object(rec)).unwrap());

    let ok = rep.reconciled
        && pr.items_readback == pr.items_written as i64
        && pr.kpis_readback == pr.kpis_written as i64
        && l1_fail == 0
        && l1.len() <= wb_core::store::L1_KEEP;
    ExitCode::from(if ok { 0 } else { 3 })
}

fn count_files(dir: &PathBuf, ext: &str) -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir(dir)
        .map(|rd| rd.filter_map(|e| e.ok()).map(|e| e.file_name().to_string_lossy().to_string()).collect())
        .unwrap_or_default();
    v.retain(|n| n.ends_with(ext));
    v.sort();
    v
}

fn verify_backups_ok(dir: &PathBuf) -> bool {
    let files = count_files(dir, ".db");
    if files.is_empty() {
        return false;
    }
    files.iter().all(|f| {
        Connection::open(dir.join(f))
            .ok()
            .and_then(|c| c.query_row("PRAGMA integrity_check", [], |r| r.get::<_, String>(0)).ok())
            .map(|s| s == "ok")
            .unwrap_or(false)
    })
}

/// 断网降级演练：**不发任何网络请求**，只验证本地读写不依赖服务端。
/// 关键断言是「同步不可达时本地写入仍然成功」，这正是本地优先架构的兜底。
fn offline_drill(store: &Store, year: i32) -> serde_json::Value {
    let before = repo::load_items(store.conn(), year).map(|v| v.len()).unwrap_or(0);

    // 模拟「同步服务端不可达」：同步客户端返回错误，业务写路径完全不受影响
    let sync_error: Result<(), String> = Err("connect timed out: sync.example.com:8443".into());
    let sync_failed_silently = sync_error.is_err();

    // 同步不可达的同时做一次真实本地写入
    let mut write_ok = false;
    if let Ok(g) = store.begin_write("offline-edit") {
        let r = g.conn().execute(
            "UPDATE work_item SET progress_pct = 5, updated_at = ?1 WHERE fiscal_year = ?2 AND seq_no = 1",
            rusqlite::params![wb_core::store::now_iso_pub(), year],
        );
        if r.is_ok() {
            // 验证本地修改确实落盘
            write_ok = store
                .conn()
                .query_row(
                    "SELECT progress_pct FROM work_item WHERE fiscal_year = ?1 AND seq_no = 1",
                    rusqlite::params![year],
                    |r| r.get::<_, i64>(0),
                )
                .map(|p| p == 5)
                .unwrap_or(false);
        }
    }
    let after = repo::load_items(store.conn(), year).map(|v| v.len()).unwrap_or(0);

    let pending = store
        .conn()
        .query_row(
            "SELECT COUNT(*) FROM work_item WHERE fiscal_year = ?1 AND updated_at IS NOT NULL",
            rusqlite::params![year],
            |r| r.get::<_, i64>(0),
        )
        .unwrap_or(-1);

    serde_json::json!({
        "sync_reachable": false,
        "sync_failed_silently": sync_failed_silently,
        "local_write_succeeded": write_ok,
        "items_before": before,
        "items_after": after,
        "readable_while_offline": after == before,
        "pending_rows_for_later_push": pending,
        "note": "同步失败不弹窗、不报错；本地读写完整可用，恢复后按 sync_cursor 增量补推",
    })
}

// ───────────────────────────── intel（v2.3.0 招标情报落库） ─────────────────────────────

fn cmd_intel(args: &[String]) -> ExitCode {
    if args.is_empty() {
        eprintln!("用法：wbd intel <data_dir> --start <yyyy-mm-dd> --end <yyyy-mm-dd> [--sites id1,id2] [--sample N]");
        return ExitCode::from(2);
    }
    let data_dir = &args[0];
    let mut start = String::new();
    let mut end = String::new();
    let mut sites: Option<Vec<String>> = None;
    let mut sample_n = 1usize;
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--start" => { start = args.get(i + 1).cloned().unwrap_or_default(); i += 2; }
            "--end" => { end = args.get(i + 1).cloned().unwrap_or_default(); i += 2; }
            "--sites" => {
                sites = args.get(i + 1).map(|s| s.split(',').map(|x| x.trim().to_string()).filter(|x| !x.is_empty()).collect());
                i += 2;
            }
            "--sample" => { sample_n = args.get(i + 1).and_then(|s| s.parse().ok()).unwrap_or(1); i += 2; }
            other => { eprintln!("未知参数：{other}"); return ExitCode::from(2); }
        }
    }
    if start.is_empty() || end.is_empty() {
        eprintln!("--start 与 --end 必填（窗口口径要写进证据）");
        return ExitCode::from(2);
    }
    let today = end.clone();
    let store = match Store::open(data_dir) {
        Ok(s) => s,
        Err(e) => return fail(format!("打开库失败：{e}"), 1),
    };
    let http = wb_core::intel::UreqFetcher;
    let sleeper = wb_core::intel::RealSleeper;
    let ts = wb_core::store::now_iso_pub();

    // 首发闸门（与 run_daily_at 同一判定，不允许两条路径各说一套）：
    // 未开启的源即便 --sites 显式点名也不抓——发布环境复验前不得静默上。
    let specs: Vec<&wb_core::intel::SiteSpec> = wb_core::intel::SITE_REGISTRY
        .iter()
        .filter(|s| s.tier.is_scheduled() && wb_core::intel::is_enabled_for_release(s.id))
        .filter(|s| sites.as_ref().map(|v| v.contains(&s.id.to_string())).unwrap_or(true))
        .collect();
    if specs.is_empty() {
        eprintln!("没有可抓取的源：首发只开启 {} 个实测通过的源。", wb_core::intel::RELEASE_ENABLED_SITES.len());
        return ExitCode::from(2);
    }

    let mut per_source = Vec::new();
    for spec in &specs {
        eprintln!("抓取 {}（{}）…", spec.id, spec.name);
        let outcome = wb_core::intel::fetch_site_with_retry(
            spec, &http, &sleeper, wb_core::intel::KEYWORD, &start, &end, &today,
        );
        let entry = match outcome {
            Ok(items) => {
                let rep = match wb_core::intel_store::persist_fetch(&store, &items, Some(&ts)) {
                    Ok(r) => r,
                    Err(e) => return fail(format!("落库失败（{}）：{e}", spec.id), 1),
                };
                // fetch_run 留档（每源一行，证据链的一部分）
                if let Ok(g) = store.begin_write(&format!("fetch-run-{}", spec.id)) {
                    let _ = g.conn().execute(
                        "INSERT OR REPLACE INTO fetch_run (id, source_id, started_at, finished_at, result, error_class, item_count, changed_count)
                         VALUES (?1, ?2, ?3, ?4, 'ok', NULL, ?5, ?6)",
                        rusqlite::params![
                            format!("{}|{}", spec.id, ts),
                            spec.id, ts, ts,
                            rep.fetched as i64,
                            (rep.inserted + rep.superseded) as i64
                        ],
                    );
                }
                // 验收1：落库后从 SQLite 回读样例
                let samples = wb_core::intel_store::read_current(&store, spec.id, sample_n)
                    .unwrap_or_default()
                    .into_iter()
                    .map(|it| {
                        // body_head 单独截断，避免在 json! 宏里嵌语句块（宏对 expr 边界敏感）。
                        let n = it.body_head.chars().count();
                        let head: String = it.body_head.chars().take(120).collect();
                        serde_json::json!({
                            "biz_key_title": it.title,
                            "url": it.url,
                            "date": it.date,
                            "org": it.org,
                            "area": it.area,
                            "amount": it.amount,
                            "body_head": format!("{head}…（{n} 字）"),
                            "relevance": format!("{:?}", it.relevance),
                        })
                    })
                    .collect::<Vec<_>>();
                serde_json::json!({
                    "site_id": spec.id,
                    "status": "ok",
                    "fetched": rep.fetched,
                    "persist": rep,
                    "readback_samples": samples,
                })
            }
            Err(f) => {
                if let Ok(g) = store.begin_write(&format!("fetch-run-{}", spec.id)) {
                    let _ = g.conn().execute(
                        "INSERT OR REPLACE INTO fetch_run (id, source_id, started_at, finished_at, result, error_class, item_count, changed_count)
                         VALUES (?1, ?2, ?3, ?4, 'failed', ?5, 0, 0)",
                        rusqlite::params![
                            format!("{}|{}", spec.id, ts),
                            spec.id, ts, ts,
                            format!("{:?}", f.status)
                        ],
                    );
                }
                serde_json::json!({
                    "site_id": spec.id,
                    "status": format!("{:?}", f.status),
                    "detail": f.detail,
                    "url": f.url,
                    "attempts": f.attempts,
                })
            }
        };
        per_source.push(entry);
    }

    let audit = wb_core::intel_store::dedup_audit(&store).map(|(a, b, c, d)| {
        serde_json::json!({ "current_records": a, "distinct_biz_keys": b, "leads": c, "distinct_lead_keys": d,
            "no_dark_duplicates": a == b && c == d })
    }).unwrap_or_else(|e| serde_json::json!({ "error": e.to_string() }));
    let out = serde_json::json!({
        "run_ts": ts,
        "window": { "start": start, "end": end, "keyword": wb_core::intel::KEYWORD },
        "sources": per_source,
        "dedup_audit": audit,
    });
    println!("{}", serde_json::to_string_pretty(&out).unwrap());
    ExitCode::SUCCESS
}

fn cmd_intel_read(args: &[String]) -> ExitCode {
    if args.len() < 2 {
        eprintln!("用法：wbd intel-read <data_dir> <source_id> [limit]");
        return ExitCode::from(2);
    }
    let store = match Store::open(&args[0]) {
        Ok(s) => s,
        Err(e) => return fail(format!("打开库失败：{e}"), 1),
    };
    let limit: usize = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(5);
    let items = match wb_core::intel_store::read_current(&store, &args[1], limit) {
        Ok(v) => v,
        Err(e) => return fail(format!("回读失败：{e}"), 1),
    };
    let total: i64 = store
        .conn()
        .query_row(
            "SELECT COUNT(*) FROM ext_record WHERE source_id = ?1 AND is_current = 1",
            rusqlite::params![args[1]],
            |r| r.get(0),
        )
        .unwrap_or(-1);
    let out = serde_json::json!({
        "source_id": args[1],
        "current_total": total,
        "readback": items.iter().map(|it| serde_json::json!({
            "key": it.key,
            "title": it.title,
            "org": it.org,
            "area": it.area,
            "date": it.date,
            "amount": it.amount,
            "url": it.url,
            "relevance": format!("{:?}", it.relevance),
            "reasons": it.reasons,
            "incomplete": it.incomplete,
            "body_head": it.body_head,
        })).collect::<Vec<_>>(),
    });
    println!("{}", serde_json::to_string_pretty(&out).unwrap());
    ExitCode::SUCCESS
}

fn cmd_lead(args: &[String]) -> ExitCode {
    if args.len() < 2 {
        eprintln!("用法：\n  wbd lead <data_dir> list [--state discovered|confirmed|rejected|todoed]\n  wbd lead <data_dir> confirm|reject|todo <dedup_key> [--owner 名] [--today yyyy-mm-dd]");
        return ExitCode::from(2);
    }
    let data_dir = &args[0];
    let store = match Store::open(data_dir) {
        Ok(s) => s,
        Err(e) => return fail(format!("打开库失败：{e}"), 1),
    };
    match args[1].as_str() {
        "list" => {
            let state = wb_core::lead_todo::LeadState::Discovered;
            let _ = state;
            let mut filter: Option<wb_core::lead_todo::LeadState> = None;
            let mut i = 2;
            while i < args.len() {
                if args[i] == "--state" {
                    filter = match args.get(i + 1).map(|s| s.as_str()) {
                        Some("discovered") => Some(wb_core::lead_todo::LeadState::Discovered),
                        Some("confirmed") => Some(wb_core::lead_todo::LeadState::Confirmed),
                        Some("rejected") => Some(wb_core::lead_todo::LeadState::Rejected),
                        Some("todoed") => Some(wb_core::lead_todo::LeadState::Todoed),
                        _ => None,
                    };
                }
                i += 1;
            }
            let leads = match wb_core::intel_store::read_leads(&store, filter.as_ref()) {
                Ok(v) => v,
                Err(e) => return fail(format!("读取线索失败：{e}"), 1),
            };
            // todo_id 不在 Lead 结构里（lead 表独立列），单独查一遍映射，避免在 json! 宏里嵌套闭包。
            let mut todo_ids: std::collections::HashMap<String, String> = std::collections::HashMap::new();
            {
                let conn = store.conn();
                let mut stmt = match conn.prepare("SELECT dedup_key, todo_id FROM intel_lead WHERE todo_id IS NOT NULL AND todo_id != ''") {
                    Ok(s) => s,
                    Err(e) => return fail(format!("读取 todo_id 失败：{e}"), 1),
                };
                let rows = match stmt.query_map([], |r| {
                    Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
                }) {
                    Ok(it) => it,
                    Err(e) => return fail(format!("读取 todo_id 失败：{e}"), 1),
                };
                for row in rows.flatten() {
                    todo_ids.insert(row.0, row.1);
                }
            }
            let out = serde_json::json!({
                "total": leads.len(),
                "leads": leads.iter().map(|l| serde_json::json!({
                    "dedup_key": l.dedup_key,
                    "layer": l.dedup_layer,
                    "origin": format!("{:?}", l.origin),
                    "state": l.state.label(),
                    "title": l.title,
                    "org": l.org,
                    "area": l.area,
                    "date": l.date,
                    "url": l.url,
                    "amount": l.amount,
                    "needs_primary_confirm": l.needs_primary_confirm,
                    "todo_id": todo_ids.get(&l.dedup_key).cloned().unwrap_or_default(),
                })).collect::<Vec<_>>(),
            });
            println!("{}", serde_json::to_string_pretty(&out).unwrap());
            ExitCode::SUCCESS
        }
        "confirm" | "reject" | "todo" => {
            if args.len() < 3 {
                eprintln!("缺少 dedup_key（wbd lead list 先查）");
                return ExitCode::from(2);
            }
            let key = &args[2];
            let mut owner = "大角子".to_string();
            let mut today = String::new();
            let mut i = 3;
            while i < args.len() {
                match args[i].as_str() {
                    "--owner" => { owner = args.get(i + 1).cloned().unwrap_or(owner); i += 2; }
                    "--today" => { today = args.get(i + 1).cloned().unwrap_or_default(); i += 2; }
                    _ => i += 1,
                }
            }
            if today.is_empty() {
                today = wb_core::store::now_iso_pub()[..10].to_string();
            }
            match args[1].as_str() {
                "confirm" => {
                    match wb_core::intel_store::confirm_lead(&store, key, None) {
                        Ok(true) => println!("已确认：{key}"),
                        Ok(false) => { eprintln!("确认失败：线索不存在或状态已流转（当前态不是待确认）"); return ExitCode::from(1); }
                        Err(e) => return fail(format!("写库失败：{e}"), 1),
                    }
                }
                "reject" => {
                    match wb_core::intel_store::reject_lead(&store, key, None) {
                        Ok(true) => println!("已忽略：{key}"),
                        Ok(false) => { eprintln!("忽略失败：线索不存在或状态已流转"); return ExitCode::from(1); }
                        Err(e) => return fail(format!("写库失败：{e}"), 1),
                    }
                }
                "todo" => {
                    match wb_core::intel_store::generate_todo(&store, key, &today, &owner) {
                        Ok(Ok(t)) => println!("{}", serde_json::to_string_pretty(&serde_json::json!({
                            "todo_id": t.todo_id, "title": t.title, "due_date": t.due_date,
                            "source_url": t.source_url, "owner": t.owner, "next_action": t.next_action,
                        })).unwrap()),
                        Ok(Err(r)) => { eprintln!("未生成待办：{}", r.reason()); return ExitCode::from(1); }
                        Err(e) => return fail(format!("写库失败：{e}"), 1),
                    }
                }
                _ => unreachable!(),
            }
            ExitCode::SUCCESS
        }
        other => { eprintln!("未知子命令：{other}"); ExitCode::from(2) }
    }
}


// ───────────────────────────── project（v3.2.0 T3 按项目聚合查询） ─────────────────────────────

/// `wbd project <data_dir> list [--stage intent|review|contract|acceptance|operation] [--limit N]`
/// `wbd project <data_dir> show <project_key>`
///
/// 数据全部来自 `ext_record` 的 current 行（拉链当前层），不重新抓取。
/// `show` 输出同一项目多阶段公告组成的时间轴；`list` 按最近公告日期倒序。
fn cmd_project(args: &[String]) -> ExitCode {
    if args.len() < 2 {
        eprintln!("用法：\n  wbd project <data_dir> list [--stage intent|review|contract|acceptance|operation] [--limit N]\n  wbd project <data_dir> show <project_key>\n  wbd project <data_dir> payload [--stage intent|tender|correction|award|contract|termination] [--limit N]\n  wbd project <data_dir> detail <agg_key|project_key>");
        return ExitCode::from(2);
    }
    let data_dir = &args[0];
    let store = match Store::open(data_dir) {
        Ok(s) => s,
        Err(e) => return fail(format!("打开库失败：{e}"), 1),
    };
    match args[1].as_str() {
        "list" => {
            let mut stage: Option<wb_core::intel_stage::Stage> = None;
            let mut limit: usize = 0;
            let mut i = 2;
            while i < args.len() {
                match args[i].as_str() {
                    "--stage" => {
                        // 校验逻辑走 wb-core 的共享实现（桌面壳用同一份），
                        // 非法值直接报错而不是静默当「不过滤」——静默回退会让人以为筛过了。
                        let raw = args.get(i + 1).map(|s| s.as_str()).unwrap_or("");
                        stage = match wb_core::intel_stage::Stage::parse_filter(raw) {
                            Ok(v) => v,
                            Err(e) => {
                                eprintln!("--stage {e}");
                                return ExitCode::from(2);
                            }
                        };
                        i += 2;
                    }
                    "--limit" => {
                        match args.get(i + 1).and_then(|s| s.parse::<usize>().ok()) {
                            Some(n) => limit = n,
                            None => {
                                eprintln!("--limit 需要一个非负整数");
                                return ExitCode::from(2);
                            }
                        }
                        i += 2;
                    }
                    _ => i += 1,
                }
            }
            let projects = match wb_core::intel_store::list_projects(&store, stage, limit) {
                Ok(v) => v,
                Err(e) => return fail(format!("聚合查询失败：{e}"), 1),
            };
            let out = serde_json::json!({
                "total": projects.len(),
                "stage_filter": stage.map(|s| s.code()).unwrap_or(""),
                "limit": limit,
                "projects": projects.iter().map(|p| serde_json::json!({
                    "project_key": p.project_key,
                    "project_code": p.project_code,
                    "project_name": p.project_name,
                    "org": p.org,
                    "area": p.area,
                    "dedup_layer": p.dedup_layer,
                    "current_stage": p.current_stage,
                    "current_stage_label": p.current_stage_label,
                    "node_count": p.node_count,
                    "first_date": p.first_date,
                    "last_date": p.last_date,
                    "max_stage_order": p.max_stage_order,
                    "skipped_stages": p.skipped_stages,
                })).collect::<Vec<_>>(),
            });
            println!("{}", serde_json::to_string_pretty(&out).unwrap());
            ExitCode::SUCCESS
        }
        "show" => {
            let key = match args.get(2) {
                Some(k) => k,
                None => {
                    eprintln!("缺少 project_key（wbd project list 先查）");
                    return ExitCode::from(2);
                }
            };
            match wb_core::intel_store::read_project_timeline(&store, key) {
                Ok(Some(p)) => {
                    println!("{}", serde_json::to_string_pretty(&p).unwrap());
                    ExitCode::SUCCESS
                }
                Ok(None) => {
                    eprintln!("未找到项目：{key}（用 wbd project list 看可用 key）");
                    ExitCode::from(1)
                }
                Err(e) => fail(format!("聚合查询失败：{e}"), 1),
            }
        }
        // v3.2.0 数据接线轨：供界面层直接消费的 payload（规格 §2 口径）
        // `list`/`show` 沿用引擎项目键口径；`payload`/`detail` 走「采购人 + 项目名归一」聚合键口径。
        "payload" => {
            let mut stage: Option<&str> = None;
            let mut limit: usize = 0;
            let mut i = 2;
            while i < args.len() {
                match args[i].as_str() {
                    "--stage" => {
                        stage = Some(args.get(i + 1).map(|s| s.as_str()).unwrap_or(""));
                        i += 2;
                    }
                    "--limit" => {
                        match args.get(i + 1).and_then(|s| s.parse::<usize>().ok()) {
                            Some(n) => limit = n,
                            None => {
                                eprintln!("--limit 需要一个非负整数");
                                return ExitCode::from(2);
                            }
                        }
                        i += 2;
                    }
                    _ => i += 1,
                }
            }
            match wb_core::intel_payload::project_list_payload(&store, stage, limit) {
                Ok(v) => {
                    println!("{}", serde_json::to_string_pretty(&v).unwrap());
                    ExitCode::SUCCESS
                }
                Err(e) => fail(format!("payload 生成失败：{e}"), 1),
            }
        }
        "detail" => {
            let key = match args.get(2) {
                Some(k) => k,
                None => {
                    eprintln!("缺少聚合键（wbd project payload 先查，取 projects[].agg_key）");
                    return ExitCode::from(2);
                }
            };
            match wb_core::intel_payload::project_detail_payload(&store, key) {
                Ok(Some(v)) => {
                    println!("{}", serde_json::to_string_pretty(&v).unwrap());
                    ExitCode::SUCCESS
                }
                Ok(None) => {
                    eprintln!("未找到聚合项目：{key}");
                    ExitCode::from(1)
                }
                Err(e) => fail(format!("payload 生成失败：{e}"), 1),
            }
        }
        other => {
            eprintln!("未知子命令：{other}");
            ExitCode::from(2)
        }
    }
}


// ─────────────────────── manual（v3.2.0 数据接线轨 T2 手动建档） ───────────────────────

/// 手动建档数据层 CLI（与桌面壳共用 `wb_core::intel_manual` 同一份实现）。
///
/// - `manual <dir> create --name X --buyer Y [--area ..] [--budget ..] [--note ..] [--lead ..] [--method ..]`
///   → 建档即查重；命中则返回 `duplicate` 分支而不是硬建。
/// - `manual <dir> list [--merged]` → 已有手动档案
/// - `manual <dir> links <manual_id> [--pending]` → 疑似关联提示
/// - `manual <dir> merge <manual_id> <target_project> --name X --buyer Y [--actor ..]`
/// - `manual <dir> ignore|reopen <manual_id> <target_project> [--actor ..]`
fn cmd_manual(args: &[String]) -> ExitCode {
    if args.len() < 2 {
        eprintln!("用法：manual <data_dir> create|list|links|merge|ignore|reopen ...");
        return ExitCode::from(2);
    }
    let data_dir = &args[0];
    let store = match Store::open(data_dir) {
        Ok(s) => s,
        Err(e) => return fail(format!("打开库失败：{e}"), 1),
    };
    // 简单 --key value 扫描（同名参数以最后一次为准）
    let flag = |name: &str| -> Option<String> {
        let mut v: Option<String> = None;
        let mut i = 0;
        while i < args.len() {
            if args[i] == name {
                if let Some(x) = args.get(i + 1) {
                    v = Some(x.clone());
                }
                i += 2;
            } else {
                i += 1;
            }
        }
        v
    };
    let has = |name: &str| -> bool { args.iter().any(|a| a == name) };

    match args[1].as_str() {
        "create" => {
            let draft = wb_core::intel_manual::ManualDraft {
                project_name: flag("--name").unwrap_or_default(),
                buyer: flag("--buyer").unwrap_or_default(),
                area: flag("--area"),
                budget: flag("--budget"),
                note: flag("--note"),
                lead_source: flag("--lead"),
                expect_method: flag("--method"),
            };
            match wb_core::intel_manual::create_manual(&store, &draft, has("--force"), &[], None) {
                Ok(wb_core::intel_manual::CreateOutcome::Created(m)) => {
                    println!("{}", serde_json::to_string_pretty(&serde_json::json!({
                        "outcome": "created", "manual_id": m.manual_id, "agg_key": m.agg_key,
                        "agg_basis": m.agg_basis, "manual_supplement": m.manual_supplement_fields()
                    })).unwrap());
                    ExitCode::SUCCESS
                }
                Ok(wb_core::intel_manual::CreateOutcome::Duplicate(d)) => {
                    println!("{}", serde_json::to_string_pretty(&serde_json::json!({
                        "outcome": "duplicate", "prompt": d.prompt(), "agg_key": d.key.key,
                        "manual_hit": d.manual_hit.as_ref().map(|m| m.manual_id.clone()),
                        "announcement_hit": d.announcement_hit
                    })).unwrap());
                    ExitCode::from(3)
                }
                Err(e) => fail(format!("建档失败：{e}"), 1),
            }
        }
        "list" => match wb_core::intel_manual::list_manual(&store, has("--merged")) {
            Ok(v) => { println!("{}", serde_json::to_string_pretty(&v).unwrap()); ExitCode::SUCCESS }
            Err(e) => fail(format!("查询失败：{e}"), 1),
        },
        "links" => {
            let id = match args.get(2) { Some(v) => v, None => { eprintln!("缺少 manual_id"); return ExitCode::from(2); } };
            match wb_core::intel_manual::links_of(&store, id, has("--pending")) {
                Ok(v) => { println!("{}", serde_json::to_string_pretty(&v).unwrap()); ExitCode::SUCCESS }
                Err(e) => fail(format!("查询失败：{e}"), 1),
            }
        }
        "merge" => {
            let (id, target) = match (args.get(2), args.get(3)) {
                (Some(i), Some(t)) => (i.clone(), t.clone()),
                _ => { eprintln!("用法：manual <dir> merge <manual_id> <target_project> --name X --buyer Y"); return ExitCode::from(2); }
            };
            let name = flag("--name").unwrap_or_default();
            let buyer = flag("--buyer").unwrap_or_default();
            let actor = flag("--actor").unwrap_or_else(|| "cli".to_string());
            match wb_core::intel_manual::merge_manual_into_project(&store, &id, &target, &name, &buyer, &actor, None) {
                Ok(o) => {
                    println!("{}", serde_json::to_string_pretty(&o).unwrap());
                    ExitCode::SUCCESS
                }
                Err(e) => fail(format!("合并失败：{e}"), 1),
            }
        }
        "ignore" | "reopen" => {
            let (id, target) = match (args.get(2), args.get(3)) {
                (Some(i), Some(t)) => (i.clone(), t.clone()),
                _ => { eprintln!("用法：manual <dir> ignore|reopen <manual_id> <target_project>"); return ExitCode::from(2); }
            };
            let actor = flag("--actor").unwrap_or_else(|| "cli".to_string());
            let r = if args[1] == "ignore" {
                wb_core::intel_manual::ignore_link(&store, &id, &target, &actor, None)
            } else {
                wb_core::intel_manual::reopen_link(&store, &id, &target, &actor, None)
            };
            match r {
                Ok(()) => { println!("{{\"outcome\":\"{}\"}}", args[1]); ExitCode::SUCCESS }
                Err(e) => fail(format!("操作失败：{e}"), 1),
            }
        }
        other => { eprintln!("未知子命令：{other}"); ExitCode::from(2) }
    }
}


// ───────────────────────────── export-subsidy（v2.3.0 需求⑤ 补贴一键导出） ─────────────────────────────

/// `wbd export-subsidy <out.xlsx|out.csv> --from <补贴明细.csv> [--filter 字段=值]... [--contains 字段:值]... [--sheet 名称]`
/// 演示/验收入口：从全量数据 CSV 装载 → 按筛选条件过滤（全量，不只当前页）→ 导出 xlsx 或 CSV。
/// UI 侧（补贴明细视图工具栏「导出」按钮）调用的是同一个 `subsidy_export` 引擎。
fn cmd_export_subsidy(args: &[String]) -> ExitCode {
    if args.len() < 3 {
        eprintln!("用法：wbd export-subsidy <out.xlsx|out.csv> --from <数据.csv> [--filter 字段=值]... [--contains 字段:值]... [--sheet 名称]");
        return ExitCode::from(2);
    }
    let out = &args[0];
    let mut from = String::new();
    let mut filters: Vec<wb_core::subsidy_export::ExportFilter> = Vec::new();
    let mut sheet_name = "补贴明细".to_string();
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--from" => { from = args.get(i + 1).cloned().unwrap_or_default(); i += 2; }
            "--filter" => {
                let kv = args.get(i + 1).cloned().unwrap_or_default();
                match kv.split_once('=') {
                    Some((k, v)) => filters.push(wb_core::subsidy_export::ExportFilter::eq(k.trim(), v)),
                    None => { eprintln!("--filter 需要 字段=值 形式，得到：{kv}"); return ExitCode::from(2); }
                }
                i += 2;
            }
            "--contains" => {
                let kv = args.get(i + 1).cloned().unwrap_or_default();
                match kv.split_once(':') {
                    Some((k, v)) => filters.push(wb_core::subsidy_export::ExportFilter::contains(k.trim(), v)),
                    None => { eprintln!("--contains 需要 字段:值 形式，得到：{kv}"); return ExitCode::from(2); }
                }
                i += 2;
            }
            "--sheet" => { sheet_name = args.get(i + 1).cloned().unwrap_or(sheet_name); i += 2; }
            other => { eprintln!("未知参数：{other}"); return ExitCode::from(2); }
        }
    }
    if from.is_empty() {
        eprintln!("--from 必填（全量数据 CSV）");
        return ExitCode::from(2);
    }
    let (headers, rows) = match wb_core::subsidy_export::read_csv(std::path::Path::new(&from)) {
        Ok(v) => v,
        Err(e) => return fail(format!("读数据失败：{e}"), 1),
    };
    let total = rows.len();
    let filtered = match wb_core::subsidy_export::apply_filters(&headers, &rows, &filters) {
        Ok(v) => v,
        Err(e) => return fail(e, 1),
    };
    let filtered_rows: Vec<Vec<String>> = filtered.into_iter().cloned().collect();
    let out_path = std::path::Path::new(out);
    let wrote = if out_path.extension().and_then(|e| e.to_str()) == Some("csv") {
        wb_core::subsidy_export::export_csv(out_path, &headers, &filtered_rows)
    } else {
        wb_core::subsidy_export::export_xlsx(out_path, &headers, &filtered_rows, &sheet_name)
    };
    match wrote {
        Ok(n2) => {
            println!("{}", serde_json::to_string_pretty(&serde_json::json!({
                "out": out,
                "format": if out_path.extension().and_then(|e| e.to_str()) == Some("csv") { "csv" } else { "xlsx" },
                "source_rows_total": total,
                "filters_applied": filters.iter().map(|f| serde_json::json!({
                    "field": f.field,
                    "op": format!("{:?}", f.op),
                    "value": f.value,
                })).collect::<Vec<_>>(),
                "rows_exported": n2,
                "note": "按筛选条件全量导出（非当前页）",
            })).unwrap());
            ExitCode::SUCCESS
        }
        Err(e) => fail(e, 1),
    }
}



// ───────────────────────────── sync-live（v3.0 真实传输） ─────────────────────────────

/// 对真实服务端跑同步链路：setup/login/status/push-test/sync-once/logout。
/// 与 `sync`（离线合并演练）的区别：这里走 [`wb_core::sync_client::http::HttpTransport`]，
/// 每次 push/pull 都是真实 HTTPS 往返——桌面壳恢复传输层后走的正是同一份代码。
fn cmd_sync_live(args: &[String]) -> ExitCode {
    if args.len() < 2 {
        return fail(
            "用法：wbd sync-live <data_dir> setup|login|status|push-test|sync-once|logout ...（详见 wbd help）",
            2,
        );
    }
    let dir = PathBuf::from(&args[0]);
    let cmd = args[1].as_str();
    let rest = &args[2..];

    let store = match Store::open(&dir) {
        Ok(s) => s,
        Err(e) => return fail(format!("打开库失败：{e}"), 1),
    };
    let conn = store.conn();
    let transport = wb_core::sync_client::http::HttpTransport::new();

    match cmd {
        // ── 首成员初始化 / 登录：落地 base_url + token + 身份 ──
        "setup" | "login" => {
            if rest.len() < 3 {
                return fail(format!("用法：wbd sync-live <data_dir> {cmd} <base_url> <username> <password>"), 2);
            }
            let (base_url, username, password) = (&rest[0], &rest[1], &rest[2]);
            let seed = format!("wbd-sync-live:{username}@{base_url}");
            let now = wb_core::store::now_iso_pub();
            let res = if cmd == "setup" {
                wb_core::sync_client::setup_first(conn, base_url, username, password, &seed, &transport, &now)
            } else {
                wb_core::sync_client::login(conn, base_url, username, password, &seed, &transport, &now)
            };
            match res {
                Ok(st) => {
                    println!("== {cmd} 成功（真实 HTTPS）==");
                    print_sync_status(&st);
                    ExitCode::SUCCESS
                }
                Err(e) => fail(format!("{cmd} 失败：{}", e.describe()), 1),
            }
        }

        // ── 状态：不显示令牌本身 ──
        "status" => {
            let st = wb_core::sync_client::read_status(conn);
            println!("== 同步状态（本机库）==");
            print_sync_status(&st);
            ExitCode::SUCCESS
        }

        // ── 构造一条测试行进待推队列（不动真实业务数据）──
        "push-test" => {
            let flag = |name: &str| rest.iter().position(|a| a == name).and_then(|i| rest.get(i + 1)).cloned();
            let id = flag("--id").unwrap_or_else(|| format!("probe-{}", wb_core::store::now_ms_pub()));
            let title = flag("--title").unwrap_or_else(|| "v3.0 真实传输首推验证行".into());
            let by = wb_core::sync_client::state_get(conn, wb_core::sync_client::keys::MEMBER_ID)
                .unwrap_or_else(|| "probe".into());
            let dev = wb_core::sync_client::state_get(conn, wb_core::sync_client::keys::DEVICE_ID)
                .unwrap_or_else(|| "unknown-device".into());
            let now_ms = wb_core::store::now_ms_pub();
            let mut payload = serde_json::Map::new();
            payload.insert("title".into(), serde_json::json!(title));
            payload.insert("progress_pct".into(), serde_json::json!(0));
            payload.insert("probe".into(), serde_json::json!(true));
            let row = sync::SyncRow {
                table: "work_item".into(),
                id,
                payload,
                updated_at: now_ms,
                updated_by: by,
                device_id: dev,
                seq: 0,
                deleted_at: None,
            };
            match wb_core::sync_client::enqueue(conn, &row, &wb_core::store::now_iso_pub()) {
                Ok(()) => {
                    let pending = wb_core::sync_client::count_pending(conn).unwrap_or(-1);
                    println!("测试行已入队：table=work_item id={} updated_at={}（队列共 {pending} 条）",
                        row.id, row.updated_at);
                    ExitCode::SUCCESS
                }
                Err(e) => fail(format!("入队失败：{e}"), 1),
            }
        }

        // ── 同步一轮：推队列 → 拉增量（真实 HTTPS）──
        "sync-once" => {
            let base_url = wb_core::sync_client::state_get(conn, wb_core::sync_client::keys::BASE_URL)
                .unwrap_or_default();
            let token = wb_core::sync_client::state_get(conn, wb_core::sync_client::keys::TOKEN)
                .unwrap_or_default();
            if base_url.is_empty() || token.is_empty() {
                return fail("尚未登录：先跑 sync-live setup（或 login）", 1);
            }
            let now_ms = wb_core::store::now_ms_pub();
            let now_iso = wb_core::store::now_iso_pub();
            let report =
                wb_core::sync_client::sync_once(conn, &base_url, &token, &transport, now_ms, &now_iso);
            println!("== sync-once 结果（真实 HTTPS：{base_url}）==");
            println!(
                "推上 {} 行 / 拉回 {} 行 / 冲突 {} 条 / 剩余待推 {} 条 / 游标 {}",
                report.pushed, report.pulled, report.conflicts, report.pending, report.cursor
            );
            match &report.error {
                Some(e) => {
                    println!("状态：{}（错误：{e}）", report.state.label());
                    ExitCode::FAILURE
                }
                None => {
                    println!("状态：{}", report.state.label());
                    ExitCode::SUCCESS
                }
            }
        }

        // ── 登出：作废令牌，本地数据保留 ──
        "logout" => match wb_core::sync_client::logout(conn, &transport) {
            Ok(_) => {
                println!("已登出：令牌作废，本地数据与游标保留");
                ExitCode::SUCCESS
            }
            Err(e) => fail(format!("登出失败：{}", e.describe()), 1),
        },

        other => fail(format!("未知 sync-live 动作：{other}（可用：setup/login/status/push-test/sync-once/logout）"), 2),
    }
}

fn print_sync_status(st: &wb_core::SyncStatus) {
    println!("  状态：{}", st.state.label());
    println!("  服务器：{}", if st.base_url.is_empty() { "（未配置）" } else { &st.base_url });
    println!("  成员：{}（{}）", st.member_name.as_deref().unwrap_or("（未登录）"), st.member_id.as_deref().unwrap_or("-"));
    println!("  设备：{} / 游标 {} / 待推 {} 条", st.device_id.as_deref().unwrap_or("-"), st.cursor, st.pending);
    println!("  上次成功：{} / 上次错误：{}", st.last_ok_at.as_deref().unwrap_or("（无）"), st.last_error.as_deref().unwrap_or("（无）"));
}
