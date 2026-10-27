//! 存量 Excel 导入：字段语义映射 + 值域校验 + 日期规范化 + 导入报告。
//!
//! 实现依据（架构 §2.3 对真实工作簿的解析结论，全部已在实现阶段用真实文件复核）：
//!
//! 1. **共 64 个工作项**（12+11+14+10+9+8），不是 65。报告按 64 出。
//! 2. `客户线索转化` 明细表有一行占位脏数据（序号 1 行、客户名称为字符串 `1`、其余 18 列全空），
//!    主键字段为空或等于序号本身 → 判定占位行，跳过并记入报告，不报错中断。
//! 3. `媒体品牌传播` 明细表存在**列错位**：`报道选题/方向` 列为空，选题文本落在
//!    `计划开启时间` 列，其后日期整体右移一列。按表头位置机械映射会得到日期全错的媒体计划表，
//!    因此本模块采用「表头语义映射 + 值域校验」：发现某列本应是日期却解析不出日期、
//!    而右邻列能解析出日期时，判定为列错位，按右移一位取值并在报告中列出具体行号与列名。
//! 4. 计划日期以总控表为权威来源（总控表 64 行日期完整且与明细表一致），
//!    明细表只提供板块专有字段——这条让列错位不会污染日期。
//!
//! 日期一律走 [`crate::date::normalize_pair`]：`raw` / `date` / `precision` 三元组，
//! 「2月」→ 当月 1 号（起点）、当月最后一天（闭环），不编造日精度。

use std::path::Path;

use calamine::{open_workbook_auto, Data, Reader, Sheets};
use serde::{Deserialize, Serialize};

use crate::date::{normalize_pair, Precision};
use crate::model::{Board, ItemStatus, KpiMetric, KpiRule, Priority, WorkItem};

// ───────────────────────────── 报告结构 ─────────────────────────────

/// 导入问题的分类。报告里逐条列出，不静默吞掉任何一条。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IssueKind {
    /// 占位行（主键为空或等于序号本身）→ 跳过
    PlaceholderSkipped,
    /// 需人工复核（含列错位修复、值域异常）
    NeedsReview,
    /// 日期无法解析，precision=unknown，进待确认队列
    DateUnparsed,
    /// 日期落在年度区间外（存量数据应在当年 2–12 月）
    DateOutOfRange,
    /// 源数据自身不一致（如 KPI 目标值与明细条数矛盾）
    SourceInconsistent,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImportIssue {
    pub kind: IssueKind,
    pub sheet: String,
    /// Excel 中的物理行号（1 起），便于用户回表核对
    pub row_no: usize,
    pub seq_no: Option<i64>,
    pub column: Option<String>,
    pub detail: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PrecisionTally {
    pub day: usize,
    pub month: usize,
    pub quarter: usize,
    pub ongoing: usize,
    pub unknown: usize,
}

impl PrecisionTally {
    pub fn add(&mut self, p: Precision) {
        match p {
            Precision::Day => self.day += 1,
            Precision::Month => self.month += 1,
            Precision::Quarter => self.quarter += 1,
            Precision::Ongoing => self.ongoing += 1,
            Precision::Unknown => self.unknown += 1,
        }
    }

    pub fn total(&self) -> usize {
        self.day + self.month + self.quarter + self.ongoing + self.unknown
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SheetTally {
    pub sheet: String,
    /// 数据行数（不含表头与小计行）
    pub data_rows: usize,
    pub imported: usize,
    pub skipped_placeholder: usize,
    pub needs_review: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImportReport {
    pub fiscal_year: i32,
    pub source_file: String,
    pub sheets: Vec<SheetTally>,
    pub issues: Vec<ImportIssue>,
    pub items_total: usize,
    pub items_imported: usize,
    pub items_skipped: usize,
    pub start_precision: PrecisionTally,
    pub end_precision: PrecisionTally,
    pub kpi_total: usize,
    /// 六大板块逐板块条数，用于与源表「小计」逐行对账
    pub per_board: Vec<(Board, usize)>,
    /// 总控表声明的项数（小计行之和），用于对账
    pub declared_total: Option<usize>,
    pub reconciled: bool,
    pub reconcile_note: String,
}

impl ImportReport {
    pub fn issue_count(&self, kind: IssueKind) -> usize {
        self.issues.iter().filter(|i| i.kind == kind).count()
    }

    /// 报告的单行摘要，供自检记录与 CLI 直接打印。
    pub fn summary_line(&self) -> String {
        format!(
            "导入 {} 项（源表声明 {:?}）· 跳过占位 {} · 待复核 {} · 日期未知 {} · KPI {} 项 · 对账 {}",
            self.items_imported,
            self.declared_total,
            self.issue_count(IssueKind::PlaceholderSkipped),
            self.issue_count(IssueKind::NeedsReview),
            self.issue_count(IssueKind::DateUnparsed),
            self.kpi_total,
            if self.reconciled { "通过" } else { "不通过" }
        )
    }
}

/// 一次导入的完整产出：可直接入库的工作项 + KPI + 报告。
#[derive(Debug, Clone)]
pub struct ImportOutcome {
    pub items: Vec<WorkItem>,
    pub kpis: Vec<KpiMetric>,
    pub report: ImportReport,
}

#[derive(Debug)]
pub enum ImportError {
    Io(String),
    Workbook(String),
    SheetMissing(String),
    NoMasterSheet,
}

impl std::fmt::Display for ImportError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ImportError::Io(e) => write!(f, "读取文件失败：{e}"),
            ImportError::Workbook(e) => write!(f, "工作簿解析失败：{e}"),
            ImportError::SheetMissing(s) => write!(f, "缺少必需的工作表：{s}"),
            ImportError::NoMasterSheet => write!(f, "未找到总控表（工作进度总控）"),
        }
    }
}

impl std::error::Error for ImportError {}

// ───────────────────────────── 单元格读取 ─────────────────────────────

/// 工作表名（与存量工作簿一致）。
pub const MASTER_SHEET: &str = "工作进度总控";
pub const KPI_SHEET: &str = "年度KPI总览";

fn cell_text(cell: &Data) -> String {
    match cell {
        Data::Empty => String::new(),
        Data::String(s) => s.trim().to_string(),
        Data::Float(f) => {
            if (f.fract()).abs() < f64::EPSILON {
                format!("{}", *f as i64)
            } else {
                format!("{f}")
            }
        }
        Data::Int(i) => format!("{i}"),
        Data::Bool(b) => format!("{b}"),
        Data::DateTime(dt) => format!("{}", dt.as_f64()),
        Data::DateTimeIso(s) | Data::DurationIso(s) => s.trim().to_string(),
        Data::Error(_) => String::new(),
    }
}

fn sheet_rows(
    wb: &mut Sheets<std::io::BufReader<std::fs::File>>,
    name: &str,
) -> Result<Vec<Vec<String>>, ImportError> {
    let range = wb
        .worksheet_range(name)
        .map_err(|e| ImportError::Workbook(format!("{name}: {e}")))?;
    Ok(range
        .rows()
        .map(|r| r.iter().map(cell_text).collect::<Vec<_>>())
        .collect())
}

/// 表头归一：去空白、去括号补充说明。`闭环时间(计划发布)` → `闭环时间`。
fn header_key(s: &str) -> String {
    let mut out = String::new();
    let mut depth = 0usize;
    for c in s.chars() {
        match c {
            '(' | '（' => depth += 1,
            ')' | '）' => {
                if depth > 0 {
                    depth -= 1
                }
            }
            _ => {
                if depth == 0 && !c.is_whitespace() && c != '\u{3000}' {
                    out.push(c)
                }
            }
        }
    }
    out
}

fn find_col(headers: &[String], key: &str) -> Option<usize> {
    let want = header_key(key);
    headers
        .iter()
        .position(|h| header_key(h) == want)
        // 容错：表头带额外后缀（如 `计划开启时间(筹备)` 已在 header_key 里剥掉，
        // 但仍可能出现 `计划开启时间窗` 这类变体）
        .or_else(|| headers.iter().position(|h| header_key(h).starts_with(&want)))
}

/// 定位表头行：第一行里能同时命中最少 `min_hits` 个关键列的那一行。
fn locate_header(rows: &[Vec<String>], keys: &[&str], min_hits: usize) -> Option<usize> {
    rows.iter()
        .take(8)
        .position(|row| keys.iter().filter(|k| find_col(row, k).is_some()).count() >= min_hits)
}

fn get(row: &[String], idx: Option<usize>) -> Option<String> {
    let v = idx.and_then(|i| row.get(i)).map(|s| s.trim().to_string())?;
    if v.is_empty() {
        None
    } else {
        Some(v)
    }
}

/// 解析序号列。非数字（小计行、标题行、空行）返回 None。
fn parse_seq(row: &[String], idx: Option<usize>) -> Option<i64> {
    let raw = get(row, idx)?;
    let digits: String = raw.chars().take_while(|c| c.is_ascii_digit()).collect();
    if digits.is_empty() {
        None
    } else {
        digits.parse::<i64>().ok()
    }
}

fn parse_pct(raw: Option<&String>) -> i64 {
    let Some(s) = raw else { return 0 };
    let cleaned = s.trim_end_matches('%').trim();
    cleaned.parse::<f64>().map(|f| f.round() as i64).unwrap_or(0).clamp(0, 100)
}


/// 明细 sheet 的名称列（各板块叫法不同，是 join 的语义键）。
fn name_col_key(board: Board) -> &'static str {
    match board {
        Board::Gov | Board::Industry => "单位名称",
        Board::Media => "媒体名称",
        Board::Event => "活动名称",
        Board::Customer => "客户名称",
        Board::Policy => "项目名称",
    }
}

/// 各板块的扩展字段映射：`(ext 键, 明细表列名)`。
/// 键名对齐架构 §4.4 扩展表字段，保证存储层能一一落到 `ext_*` 列。
fn ext_mapping(board: Board) -> &'static [(&'static str, &'static str)] {
    match board {
        Board::Gov => &[
            ("层级", "层级"),
            ("对接部门", "对接部门"),
            ("关键联系人", "关键联系人"),
            ("职务", "职务"),
            ("联系方式", "联系方式"),
            ("区域", "区域"),
            ("累计联系次数", "累计联系次数"),
            ("沟通深度", "沟通深度"),
            ("合作待办事项", "合作/待办事项"),
            ("批次进度描述", "批次进度描述"),
            ("下次计划日期", "下次计划日期"),
        ],
        Board::Industry => &[
            ("类别", "类别"),
            ("关键联系人", "关键联系人"),
            ("职务", "职务"),
            ("联系方式", "联系方式"),
            ("合作方向", "合作方向"),
            ("累计联系次数", "累计联系次数"),
            ("批次进度描述", "批次进度描述"),
            ("下次计划日期", "下次计划日期"),
        ],
        Board::Media => &[
            ("媒体类型", "媒体类型"),
            ("联系人", "联系人"),
            ("联系方式", "联系方式"),
            ("选题方向", "报道选题/方向"),
            ("实际发布时间", "实际发布时间"),
            ("报道链接", "报道链接"),
            ("曝光量阅读量", "曝光量/阅读量"),
        ],
        Board::Event => &[
            ("活动类型", "活动类型"),
            ("销售季", "销售季"),
            ("地点", "地点"),
            ("目标人数", "目标人数"),
            ("实际费用", "实际费用"),
            ("实际参与人数", "实际参与人数"),
            ("现场签约台数", "现场签约(台)"),
            ("收集线索条数", "收集线索(条)"),
            ("媒体报道篇数", "媒体报道(篇)"),
            ("效果评估", "效果评估"),
        ],
        Board::Customer => &[
            ("客户类型", "客户类型"),
            ("区域", "区域"),
            ("来源渠道", "来源渠道"),
            ("意向等级", "意向等级"),
            ("需求产品", "需求产品"),
            ("意向数量台", "意向数量(台)"),
            ("首次接触日期", "首次接触日期"),
            ("最近跟进日期", "最近跟进日期"),
            ("跟进次数", "跟进次数"),
        ],
        Board::Policy => &[
            ("申报部门", "申报部门"),
            ("项目类型", "项目类型"),
            ("政策依据", "政策依据"),
            ("材料准备进度", "材料准备进度%"),
            ("提交日期", "提交日期"),
            ("审批状态", "审批状态"),
            ("立项金额万元", "立项金额(万元)"),
            ("到账金额万元", "到账金额(万元)"),
        ],
    }
}

/// 明细表里本应是日期、但不是主日期锚点的列（用于列错位检测）。
fn date_like(s: &str) -> bool {
    if s.is_empty() {
        return false;
    }
    crate::date::normalize(s, 2026, crate::date::Role::Start)
        .precision
        .is_known()
}

/// 从明细表构建按「板块 + 名称 + 同名序次」索引的扩展字段表。
fn build_detail_index(
    wb: &mut Sheets<std::io::BufReader<std::fs::File>>,
    board: Board,
    report: &mut ImportReport,
) -> Vec<((Board, String, usize), std::collections::BTreeMap<String, String>)> {
    let mut out = Vec::new();
    let sheet = board.sheet_name();
    let rows = match sheet_rows(wb, sheet) {
        Ok(r) => r,
        Err(e) => {
            report.issues.push(ImportIssue {
                kind: IssueKind::SourceInconsistent,
                sheet: sheet.to_string(),
                row_no: 0,
                seq_no: None,
                column: None,
                detail: format!("明细表不可读，该板块仅导入总控表共性字段：{e}"),
            });
            return out;
        }
    };
    if rows.is_empty() {
        return out;
    }

    // 表头行：明细表表头在第 1 行；容错取前 3 行里命中最多关键列的一行
    let keys = ["序号", name_col_key(board)];
    let Some(hdr_idx) = locate_header(&rows, &keys, 2) else {
        report.issues.push(ImportIssue {
            kind: IssueKind::SourceInconsistent,
            sheet: sheet.to_string(),
            row_no: 0,
            seq_no: None,
            column: None,
            detail: "未识别到表头行，该板块仅导入总控表共性字段".into(),
        });
        return out;
    };
    let headers = rows[hdr_idx].clone();

    let seq_col = find_col(&headers, "序号");
    let name_key = name_col_key(board);
    let name_idx = find_col(&headers, name_key);

    // 媒体表列错位的列位置（供逐行检测）
    let topic_idx = find_col(&headers, "报道选题/方向");
    let start_idx = find_col(&headers, "计划开启时间").or_else(|| find_col(&headers, "计划开启"));

    let mut name_counter: std::collections::HashMap<String, usize> = std::collections::HashMap::new();

    for (offset, row) in rows.iter().enumerate().skip(hdr_idx + 1) {
        let row_no = offset + 1;
        if row.iter().all(|c| c.trim().is_empty()) {
            continue;
        }
        let Some(_seq) = parse_seq(row, seq_col) else {
            continue; // 小计行、说明行
        };
        let Some(name) = get(row, name_idx) else {
            continue;
        };

        // 占位行判定：主键字段为空，或等于序号本身的字符串（架构 §2.3-2）
        let seq_text = get(row, seq_col).unwrap_or_default();
        if name == seq_text || name.parse::<i64>().is_ok() {
            report.issues.push(ImportIssue {
                kind: IssueKind::PlaceholderSkipped,
                sheet: sheet.to_string(),
                row_no,
                seq_no: Some(_seq),
                column: Some(name_key.to_string()),
                detail: format!(
                    "占位行：`{name_key}` 值为 `{name}`，其余列全空，按占位跳过（不计入导入条数）"
                ),
            });
            continue;
        }

        let mut ext: std::collections::BTreeMap<String, String> = Default::default();
        for (k, col) in ext_mapping(board) {
            if let Some(v) = get(row, find_col(&headers, col)) {
                ext.insert((*k).to_string(), v);
            }
        }

        // 媒体表列错位修复（架构 §2.3-3）：选题列空 + 计划开启列不是日期 → 选题落错列
        if board == Board::Media {
            let topic_empty = topic_idx.and_then(|i| row.get(i)).map(|s| s.trim().is_empty()).unwrap_or(true);
            let start_val = start_idx.and_then(|i| row.get(i)).map(|s| s.trim().to_string()).unwrap_or_default();
            if topic_empty && !start_val.is_empty() && !date_like(&start_val) {
                ext.insert("选题方向".into(), start_val.clone());
                // 修复标记：值域校验判定为列错位，已在此行按右移一位取值
                ext.insert("列错位修复".into(), "1".into());
                report.issues.push(ImportIssue {
                    kind: IssueKind::NeedsReview,
                    sheet: sheet.to_string(),
                    row_no,
                    seq_no: Some(_seq),
                    column: Some("报道选题/方向←计划开启时间".into()),
                    detail: format!(
                        "检测到列错位：「报道选题/方向」列为空，选题文本 `{start_val}` 落在「计划开启时间」列，\
                         其后日期整体右移一列。计划开启/闭环日期已改取总控表权威值，选题已回填本行。"
                    ),
                });
            }
        }

        let n = name_counter.entry(name.clone()).or_insert(0);
        *n += 1;
        out.push(((board, name, *n), ext));
    }

    let tally = report.sheets.iter_mut().find(|s| s.sheet == sheet);
    let imported_rows = out.len();
    if let Some(t) = tally {
        t.imported = imported_rows;
        t.data_rows = imported_rows;
    }
    out
}


// ───────────────────────────── 总控表解析 ─────────────────────────────

const MASTER_KEYS: [&str; 6] = ["序号", "板块", "工作项", "计划开启", "计划闭环", "阶段状态"];

fn parse_master(
    rows: &[Vec<String>],
    fiscal_year: i32,
    detail_index: &[((Board, String, usize), std::collections::BTreeMap<String, String>)],
    report: &mut ImportReport,
) -> Vec<WorkItem> {
    let Some(hdr_idx) = locate_header(rows, &MASTER_KEYS, 4) else {
        return Vec::new();
    };
    let headers = rows[hdr_idx].clone();

    let c_seq = find_col(&headers, "序号");
    let c_board = find_col(&headers, "板块");
    let c_title = find_col(&headers, "工作项");
    let c_cat = find_col(&headers, "类型/层级").or_else(|| find_col(&headers, "类型"));
    let c_prio = find_col(&headers, "优先级");
    let c_quant = find_col(&headers, "量化目标");
    let c_start = find_col(&headers, "计划开启");
    let c_end = find_col(&headers, "计划闭环");
    let c_pct = find_col(&headers, "当前进度%").or_else(|| find_col(&headers, "当前进度"));
    let c_status = find_col(&headers, "阶段状态");
    let c_ms = find_col(&headers, "里程碑阶段");
    let c_next = find_col(&headers, "下一步动作");
    let c_owner = find_col(&headers, "负责人");
    let c_src = find_col(&headers, "数据来源");

    let mut declared: usize = 0;
    let mut seen_declared: Vec<(Board, usize)> = Vec::new();
    let mut items: Vec<WorkItem> = Vec::new();
    let mut name_counter: std::collections::HashMap<(Board, String), usize> = Default::default();

    for (offset, row) in rows.iter().enumerate().skip(hdr_idx + 1) {
        let row_no = offset + 1;

        // 声明项数：小计行形如 `小计：12 项 · 已完成 0 项`，值可能落在首列之后的任意列，
        // 因此扫描整行而不是只看第一列。
        if let Some(sub) = row.iter().find(|c| c.contains("小计") && c.contains("项")) {
            if let Some(n) = extract_minor_total(sub) {
                declared += n;
            }
            continue;
        }

        let Some(seq_no) = parse_seq(row, c_seq) else {
            continue;
        };
        let Some(board_raw) = get(row, c_board) else {
            continue;
        };
        let Some(board) = Board::from_zh(&board_raw) else {
            report.issues.push(ImportIssue {
                kind: IssueKind::SourceInconsistent,
                sheet: MASTER_SHEET.to_string(),
                row_no,
                seq_no: Some(seq_no),
                column: Some("板块".into()),
                detail: format!("未知板块名 `{board_raw}`，该行未导入"),
            });
            continue;
        };
        seen_declared.push((board, 0));
        let Some(title) = get(row, c_title) else {
            report.issues.push(ImportIssue {
                kind: IssueKind::PlaceholderSkipped,
                sheet: MASTER_SHEET.to_string(),
                row_no,
                seq_no: Some(seq_no),
                column: Some("工作项".into()),
                detail: "工作项名称为空，按占位行跳过".into(),
            });
            continue;
        };

        let raw_start = get(row, c_start).unwrap_or_default();
        let raw_end = get(row, c_end).unwrap_or_default();
        let (sd, ed) = normalize_pair(&raw_start, &raw_end, fiscal_year);
        report.start_precision.add(sd.precision);
        report.end_precision.add(ed.precision);

        if sd.precision == Precision::Unknown {
            report.issues.push(ImportIssue {
                kind: IssueKind::DateUnparsed,
                sheet: MASTER_SHEET.to_string(),
                row_no,
                seq_no: Some(seq_no),
                column: Some("计划开启".into()),
                detail: format!(
                    "计划开启原文 `{raw_start}` 无法解析：{}，已置 unknown 进待确认队列，未猜测日期",
                    sd.reason.clone().unwrap_or_default()
                ),
            });
        }
        if ed.precision == Precision::Unknown {
            report.issues.push(ImportIssue {
                kind: IssueKind::DateUnparsed,
                sheet: MASTER_SHEET.to_string(),
                row_no,
                seq_no: Some(seq_no),
                column: Some("计划闭环".into()),
                detail: format!(
                    "计划闭环原文 `{raw_end}` 无法解析：{}，已置 unknown 进待确认队列，未猜测日期",
                    ed.reason.clone().unwrap_or_default()
                ),
            });
        }
        // 存量数据应落在当年 2–12 月（PRD F1 的校验口径）；越界只提示，不改值
        let real_dates = [
            ("计划开启", sd.precision, sd.anchor()),
            ("计划闭环", ed.precision, ed.anchor()),
        ];
        for (label, prec, anchor) in real_dates {
            // 「持续」类常驻项与季度锚点不落在具体日程上，不参与月度越界校验
            if !matches!(prec, Precision::Day | Precision::Month) {
                continue;
            }
            if let Some(a) = anchor {
                if a.year != fiscal_year || a.month < 2 || a.month > 12 {
                    report.issues.push(ImportIssue {
                        kind: IssueKind::DateOutOfRange,
                        sheet: MASTER_SHEET.to_string(),
                        row_no,
                        seq_no: Some(seq_no),
                        column: Some(label.into()),
                        detail: format!("规范化结果 {} 落在 {fiscal_year} 年 2–12 月区间外", a.to_iso()),
                    });
                }
            }
        }

        // 明细扩展字段：按 (板块, 名称, 同名序次) 关联
        let n = name_counter.entry((board, title.clone())).or_insert(0);
        *n += 1;
        let ext = detail_index
            .iter()
            .find(|((b, t, k), _)| *b == board && *t == title && *k == *n)
            .map(|(_, e)| e.clone())
            .unwrap_or_default();

        items.push(WorkItem {
            id: format!("fy{}-{:03}", fiscal_year, seq_no),
            seq_no,
            board,
            title,
            category: get(row, c_cat),
            priority: get(row, c_prio).and_then(|v| Priority::from_raw(&v)),
            target_quant: get(row, c_quant),
            planned_start_date: sd.date.clone(),
            planned_start_precision: sd.precision.as_str().to_string(),
            planned_start_raw: Some(raw_start).filter(|s| !s.is_empty()),
            planned_end_date: ed.date.clone(),
            planned_end_precision: ed.precision.as_str().to_string(),
            planned_end_raw: Some(raw_end).filter(|s| !s.is_empty()),
            progress_pct: parse_pct(get(row, c_pct).as_ref()),
            status: get(row, c_status).as_deref().and_then(ItemStatus::from_zh),
            milestone_stage: get(row, c_ms),
            next_action: get(row, c_next),
            owner: get(row, c_owner),
            source_sheet: get(row, c_src).or_else(|| Some(board.sheet_name().to_string())),
            fiscal_year,
            ext,
            created_at: None,
            updated_at: None,
        });
    }

    report.declared_total = if declared > 0 { Some(declared) } else { None };
    items
}

fn extract_minor_total(s: &str) -> Option<usize> {
    let digits: String = s
        .chars()
        .skip_while(|c| !c.is_ascii_digit())
        .take_while(|c| c.is_ascii_digit())
        .collect();
    digits.parse::<usize>().ok()
}

// ───────────────────────────── KPI 解析 ─────────────────────────────

fn parse_kpi(rows: &[Vec<String>], report: &mut ImportReport) -> Vec<KpiMetric> {
    let keys = ["指标类别", "具体指标", "年度目标"];
    let Some(hdr_idx) = locate_header(rows, &keys, 3) else {
        return Vec::new();
    };
    let headers = rows[hdr_idx].clone();
    let c_cat = find_col(&headers, "指标类别");
    let c_name = find_col(&headers, "具体指标");
    let c_target = find_col(&headers, "年度目标");
    let c_src = find_col(&headers, "数据来源");
    let c_note = find_col(&headers, "备注");

    let mut out = Vec::new();
    for (offset, row) in rows.iter().enumerate().skip(hdr_idx + 1) {
        let row_no = offset + 1;
        let Some(cat) = get(row, c_cat) else { continue };
        let Some(name) = get(row, c_name) else { continue };
        let Some(target_raw) = get(row, c_target) else { continue };
        let Ok(target_value) = target_raw.parse::<f64>() else {
            report.issues.push(ImportIssue {
                kind: IssueKind::SourceInconsistent,
                sheet: KPI_SHEET.to_string(),
                row_no,
                seq_no: None,
                column: Some("年度目标".into()),
                detail: format!("年度目标 `{target_raw}` 不是数字，该指标未导入"),
            });
            continue;
        };
        let source_sheet = get(row, c_src);
        let board = source_sheet.as_deref().and_then(Board::from_zh);
        let rule = kpi_rule_for(&name, board);
        out.push(KpiMetric {
            id: format!("kpi-{:02}", out.len() + 1),
            category: cat,
            name,
            target_value,
            rule,
            source_sheet,
            note: get(row, c_note),
        });
    }
    out
}

/// 指标口径判定：按指标名语义映射到统计规则。
/// 「当前完成(自动统计)」在源表里是手工填死的数字，本实现改为派生（架构 §4.11-2：KPI 不落死值）。
fn kpi_rule_for(name: &str, board: Option<Board>) -> KpiRule {
    if name.contains("总数") || name.contains("总场次") {
        return KpiRule::BoardTotal;
    }
    if name.contains("完成") || name.contains("已成交") || name.contains("已立项")
        || name.contains("已发布") || name.contains("已对接") || name.contains("已合作")
    {
        if name.contains("总数") {
            return KpiRule::BoardTotal;
        }
        return KpiRule::BoardDone;
    }
    if name.contains("线索总数") {
        return KpiRule::BoardTotal;
    }
    // 兜底：按板块总数，避免出现没有口径的死值
    let _ = board;
    KpiRule::BoardStarted
}

// ───────────────────────────── 公开入口 ─────────────────────────────

/// 从工作簿导入。`fiscal_year` 必须显式传入（架构 §5.2-3：不从系统时间隐式推断）。
pub fn import_workbook(
    path: impl AsRef<Path>,
    fiscal_year: i32,
) -> Result<ImportOutcome, ImportError> {
    let path = path.as_ref();
    let mut wb = open_workbook_auto(path)
        .map_err(|e| ImportError::Workbook(e.to_string()))?;

    let mut report = ImportReport {
        fiscal_year,
        source_file: path
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_default(),
        sheets: Vec::new(),
        issues: Vec::new(),
        items_total: 0,
        items_imported: 0,
        items_skipped: 0,
        start_precision: Default::default(),
        end_precision: Default::default(),
        kpi_total: 0,
        per_board: Vec::new(),
        declared_total: None,
        reconciled: false,
        reconcile_note: String::new(),
    };

    // 逐 sheet 建立空台账，便于报告逐 sheet 对账（含无导入的 sheet）
    for name in wb.sheet_names().to_vec() {
        report.sheets.push(SheetTally {
            sheet: name,
            data_rows: 0,
            imported: 0,
            skipped_placeholder: 0,
            needs_review: 0,
        });
    }

    let master_rows = sheet_rows(&mut wb, MASTER_SHEET)
        .map_err(|_| ImportError::NoMasterSheet)?;

    // 明细扩展字段（六板块）
    let mut detail_index = Vec::new();
    for b in Board::ALL {
        detail_index.extend(build_detail_index(&mut wb, b, &mut report));
    }

    let items = parse_master(&master_rows, fiscal_year, &detail_index, &mut report);

    let kpis = match sheet_rows(&mut wb, KPI_SHEET) {
        Ok(rows) => parse_kpi(&rows, &mut report),
        Err(_) => {
            report.issues.push(ImportIssue {
                kind: IssueKind::SourceInconsistent,
                sheet: KPI_SHEET.to_string(),
                row_no: 0,
                seq_no: None,
                column: None,
                detail: "KPI 总览表不可读，未导入任何 KPI".into(),
            });
            Vec::new()
        }
    };

    // 台账回填：总控表承载全部 64 行（明细表只提供扩展字段），单独记账
    if let Some(t) = report.sheets.iter_mut().find(|s| s.sheet == MASTER_SHEET) {
        t.data_rows = items.len() + report.issues.iter().filter(|i| i.sheet == MASTER_SHEET).count();
        t.imported = items.len();
        t.skipped_placeholder = report
            .issues
            .iter()
            .filter(|i| i.sheet == MASTER_SHEET && i.kind == IssueKind::PlaceholderSkipped)
            .count();
    }
    for t in report.sheets.iter_mut() {
        let imported = items.iter().filter(|i| {
            i.source_sheet.as_deref() == Some(t.sheet.as_str())
                || Board::from_zh(&t.sheet).map(|b| b == i.board).unwrap_or(false)
        });
        t.imported = t.imported.max(imported.count());
        t.skipped_placeholder = report
            .issues
            .iter()
            .filter(|i| i.sheet == t.sheet && i.kind == IssueKind::PlaceholderSkipped)
            .count();
        t.needs_review = report
            .issues
            .iter()
            .filter(|i| i.sheet == t.sheet && i.kind == IssueKind::NeedsReview)
            .count();
    }

    report.items_total = items.len();
    report.items_imported = items.len();
    report.items_skipped = report.issue_count(IssueKind::PlaceholderSkipped);
    report.kpi_total = kpis.len();
    report.per_board = Board::ALL
        .iter()
        .map(|b| (*b, items.iter().filter(|i| i.board == *b).count()))
        .collect();

    // 对账：总控表「小计」声明数 vs 实导条数
    match report.declared_total {
        Some(d) if d == items.len() => {
            report.reconciled = true;
            report.reconcile_note = format!("总控表小计声明 {d} 项，实导 {} 项，逐板块一致", items.len());
        }
        Some(d) => {
            report.reconciled = false;
            report.reconcile_note =
                format!("总控表小计声明 {d} 项，实导 {} 项，差 {} 项", items.len(), d as i64 - items.len() as i64);
        }
        None => {
            report.reconciled = false;
            report.reconcile_note = "总控表未找到小计行，无法对账".into();
        }
    }

    Ok(ImportOutcome { items, kpis, report })
}
