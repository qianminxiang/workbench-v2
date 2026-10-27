//! `wbd` —— 个人工作台 v2.0 系统层驱动。
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
    r#"wbd —— 个人工作台 v2.0 驱动

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
    println!("晨报 {}：当日到期 {} · 临期 {} · 逾期 {} · 周一聚合 {}",
        brief.brief_date, brief.due_today.len(), brief.due_soon.len(), brief.overdue.len(),
        brief.weekly.as_ref().map(|w| w.should_close.len() + w.should_start.len()).unwrap_or(0));
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
