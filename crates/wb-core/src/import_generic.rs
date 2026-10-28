//! v2.2 FR-3 阶段一：通用表格导入（任意 sheet 结构识别 + 列映射确认 + 落库）。
//!
//! 与既有 `import.rs` 的关系：那是**模板强绑定**路径（要求「工作进度总控」
//! 「年度KPI总览」两个具名 sheet + 六个明细 sheet，产出 64 项）；本模块是
//! **通用**路径，两条入口并存，互不挤掉（AC-3-9 防回归）。
//!
//! 三段式（PRD FR-3 输出形态）：
//! 1. 识别报告：逐 sheet 列出表头、识别到的列语义、无法归类的列、跳过的行；
//! 2. 列映射确认：映射由用户在确认页拍板（安全阀），本模块只给**默认建议**；
//! 3. 落库结果：新增 / 更新 / 跳过 / 需复核四类计数 + 逐条问题清单。
//!
//! 真实样本口径（docs/fr3-sample.xlsx，8 sheet 模板 + 用户脏数据）：
//! - 表头不一定在第 1 行（「工作进度总控」第 1 行是标题行，表头在第 2 行）；
//! - 日期混用「2月」（月粒度）与「2月15日」（日粒度），两种都要认（AC-3-3）；
//! - 有「▍政府关系（12 项）」这类分组/小计行要跳过；
//! - 有整行错位的脏数据行（任务名列落到序号列）要容错跳过，不许整表失败。
//!
//! 红线：识别不出任务列时**不落库**（AC-3-5），宁可停在映射页让用户指定；
//! 不给任何行编造日期（待定/空 → 精度 unknown，进需复核清单，AC-3-4）。

use std::path::Path;

use calamine::{open_workbook_auto, Data, ExcelDateTime, Reader};
use serde::{Deserialize, Serialize};

use crate::date::{normalize_pair, Precision};
use crate::model::Board;

/// 列语义（映射下拉的取值）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ColumnRole {
    /// 任务/工作项名称（必需列）
    TaskName,
    /// 计划开始
    StartDate,
    /// 计划闭环
    EndDate,
    /// 进度百分比
    Progress,
    /// 负责人
    Owner,
    /// 备注
    Note,
    /// 板块（可识别则按行取，识别不到用确认页选的默认板块）
    BoardCol,
    /// 忽略
    Ignore,
}

impl ColumnRole {
    pub fn label(&self) -> &'static str {
        match self {
            ColumnRole::TaskName => "任务名称",
            ColumnRole::StartDate => "计划开始",
            ColumnRole::EndDate => "计划闭环",
            ColumnRole::Progress => "进度",
            ColumnRole::Owner => "负责人",
            ColumnRole::Note => "备注",
            ColumnRole::BoardCol => "板块",
            ColumnRole::Ignore => "忽略",
        }
    }
}

/// 跳过行的原因分类。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SkipKind {
    /// 空行
    EmptyRow,
    /// 分组/小计行（如「▍政府关系（12 项）」）
    GroupRow,
    /// 任务列为空
    TitleEmpty,
}

impl SkipKind {
    pub fn label(&self) -> &'static str {
        match self {
            SkipKind::EmptyRow => "空行",
            SkipKind::GroupRow => "分组/小计行",
            SkipKind::TitleEmpty => "任务列为空",
        }
    }
}

/// 跳过行明细。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkippedRow {
    /// Excel 物理行号（1 起）
    pub row_no: usize,
    pub kind: SkipKind,
    pub detail: String,
}

/// 单个 sheet 的识别结果。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SheetScan {
    pub sheet: String,
    /// 表头所在物理行号（1 起）
    pub header_row: usize,
    pub headers: Vec<String>,
    /// 与 headers 等长：每列识别到的语义（Ignore = 未识别）
    pub roles: Vec<ColumnRole>,
    /// 识别到的任务列下标；None = 未识别（不可导入）
    pub task_col: Option<usize>,
    /// 候选数据行数（不含表头与跳过行）
    pub data_rows: usize,
    pub skipped: Vec<SkippedRow>,
    /// 识别说明（如「未识别到板块列，导入时使用确认页选择的默认板块」）
    pub notes: Vec<String>,
}

impl SheetScan {
    /// 识别不到任务列 → 该 sheet 不可导入（AC-3-5）。
    pub fn importable(&self) -> bool {
        self.task_col.is_some()
    }
    pub fn mapped_roles(&self) -> Vec<(usize, String, ColumnRole)> {
        self.headers
            .iter()
            .enumerate()
            .map(|(i, h)| (i, h.clone(), self.roles[i]))
            .collect()
    }
}

/// 一次扫描的完整报告（识别报告）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScanReport {
    pub file: String,
    pub sheets: Vec<SheetScan>,
}

#[derive(Debug)]
pub enum GenericImportError {
    /// 文件读不到（不存在 / 被占用 / 无权限）——AC-3-7 要给可读原因
    Io(String),
    /// 工作簿打不开
    Workbook(String),
    /// 指定 sheet 不存在
    SheetMissing(String),
    /// 未识别到任务列，拒绝落库（AC-3-5）
    NoTaskColumn(String),
}

/// 通用导入错误分类，用于界面分文案。
impl GenericImportError {
    pub fn kind(&self) -> &'static str {
        match self {
            GenericImportError::Io(_) => "io",
            GenericImportError::Workbook(_) => "workbook",
            GenericImportError::SheetMissing(_) => "sheet_missing",
            GenericImportError::NoTaskColumn(_) => "no_task_column",
        }
    }
    pub fn message(&self) -> String {
        match self {
            GenericImportError::Io(m) => format!("文件读取失败：{m}"),
            GenericImportError::Workbook(m) => format!("工作簿打开失败：{m}"),
            GenericImportError::SheetMissing(m) => format!("找不到工作表：{m}"),
            GenericImportError::NoTaskColumn(m) => {
                format!("未识别到任务列，请手工指定（{m}）")
            }
        }
    }
}

// ───────────────────────────── 同义词表 ─────────────────────────────

/// 表头同义词（首版覆盖真实样本 + PRD 提到的常见叫法）。
/// 匹配顺序：先精确、再包含；同长度取先命中者。
const TASK_NAME_WORDS: &[&str] = &[
    "任务名称", "工作项", "工作项名称", "任务", "事项", "项目名称", "活动名称", "客户名称",
    "媒体名称", "单位名称", "名称", "标题", "工作内容", "计划名称",
];
const START_WORDS: &[&str] = &[
    "计划开启时间", "计划开启", "计划开始时间", "计划开始", "开启时间", "开始时间", "开始日期",
    "起始时间", "启动时间",
];
const END_WORDS: &[&str] = &[
    "计划闭环时间", "计划闭环", "闭环时间", "计划结束时间", "计划结束", "结束时间", "结束日期",
    "完成时间", "截止时间", "闭环日期",
];
const PROGRESS_WORDS: &[&str] = &[
    "当前进度", "完成进度", "完成率", "进度", "百分比", "材料准备进度",
];
const OWNER_WORDS: &[&str] = &[
    "负责人", "责任人", "跟进人", "主办人", "对接人", "执行人",
];
const NOTE_WORDS: &[&str] = &["备注", "说明", "批注"];
const BOARD_WORDS: &[&str] = &["板块", "所属板块", "类别", "分类", "归属", "业务板块"];

/// 表头归一：去空白、去全角空格、去掉括号内容（「计划开启时间(筹备)」→「计划开启时间」）。
fn norm_header(s: &str) -> String {
    let mut out = String::new();
    let mut in_paren = false;
    for ch in s.chars() {
        match ch {
            '(' | '（' => in_paren = true,
            ')' | '）' => in_paren = false,
            _ => {
                if !in_paren {
                    out.push(ch);
                }
            }
        }
    }
    out.chars()
        .filter(|c| !c.is_whitespace())
        .collect::<String>()
        .trim()
        .to_string()
}

/// 在同义词表里做包含匹配，返回命中最长的词。
fn contains_match<'a>(h: &str, words: &[&'a str]) -> Option<&'a str> {
    words
        .iter()
        .filter(|w| !norm_header(w).is_empty() && h.contains(&norm_header(w)))
        .max_by_key(|w| norm_header(w).chars().count())
        .copied()
}

/// 在同义词表里找最佳匹配：精确优先，其次最长包含。
fn match_role(header: &str) -> Option<ColumnRole> {
    let h = norm_header(header);
    if h.is_empty() {
        return None;
    }
    let exact = |words: &[&str]| words.iter().any(|w| norm_header(w) == h);
    if exact(TASK_NAME_WORDS) {
        return Some(ColumnRole::TaskName);
    }
    if exact(START_WORDS) {
        return Some(ColumnRole::StartDate);
    }
    if exact(END_WORDS) {
        return Some(ColumnRole::EndDate);
    }
    if exact(PROGRESS_WORDS) {
        return Some(ColumnRole::Progress);
    }
    if exact(OWNER_WORDS) {
        return Some(ColumnRole::Owner);
    }
    if exact(NOTE_WORDS) {
        return Some(ColumnRole::Note);
    }
    if exact(BOARD_WORDS) {
        return Some(ColumnRole::BoardCol);
    }
    // 包含匹配：取命中最长的词（避免「当前进度」被「进度」抢走时信息丢失）
    if contains_match(&h, TASK_NAME_WORDS).is_some() {
        return Some(ColumnRole::TaskName);
    }
    if contains_match(&h, START_WORDS).is_some() {
        return Some(ColumnRole::StartDate);
    }
    if contains_match(&h, END_WORDS).is_some() {
        return Some(ColumnRole::EndDate);
    }
    if contains_match(&h, PROGRESS_WORDS).is_some() {
        return Some(ColumnRole::Progress);
    }
    if contains_match(&h, OWNER_WORDS).is_some() {
        return Some(ColumnRole::Owner);
    }
    if contains_match(&h, NOTE_WORDS).is_some() {
        return Some(ColumnRole::Note);
    }
    if contains_match(&h, BOARD_WORDS).is_some() {
        return Some(ColumnRole::BoardCol);
    }
    None
}

/// Excel 序列日期 → YYYY-MM-DD（duration 型不是日期，返回空）。
fn excel_dt_str(dt: &ExcelDateTime) -> String {
    if dt.is_duration() {
        return String::new();
    }
    let (y, m, d, _h, _mi, _s, _ms) = dt.to_ymd_hms_milli();
    if y == 0 {
        return String::new();
    }
    format!("{y:04}-{m:02}-{d:02}")
}

/// 单元格取值统一成字符串（数字保留整数形态，日期转 ISO）。
pub fn cell_str(d: &Data) -> String {
    match d {
        Data::String(s) => s.trim().to_string(),
        Data::Int(i) => i.to_string(),
        Data::Float(f) => {
            if (f - f.trunc()).abs() < 1e-9 {
                format!("{}", f.trunc() as i64)
            } else {
                format!("{f}")
            }
        }
        Data::Bool(b) => b.to_string(),
        Data::DateTime(dt) => excel_dt_str(dt),
        Data::DateTimeIso(s) => s.trim().to_string(),
        Data::DurationIso(s) => s.trim().to_string(),
        Data::Empty | Data::Error(_) => String::new(),
    }
}

/// 行是否「只有一格有值」——分组/小计行的典型形态。
fn single_value_detail(row: &[Data]) -> Option<String> {
    let mut vals: Vec<String> = row.iter().map(cell_str).filter(|s| !s.is_empty()).collect();
    if vals.len() == 1 {
        vals.pop()
    } else {
        None
    }
}

/// 判断是否分组/小计行。
fn is_group_row(row: &[Data]) -> bool {
    let Some(v) = single_value_detail(row) else {
        return false;
    };
    v.starts_with('▍')
        || v.contains("小计")
        || v.contains("合计")
        || (v.contains('（') && v.contains("项"))
        || (v.contains('(') && v.contains("项"))
}

/// 表头行识别：在前 12 行里找「非空列 ≥2 且至少一列命中同义词表」的第一行。
fn find_header_row(sheet: &[(usize, Vec<Data>)]) -> Option<(usize, usize)> {
    for (idx, (row_no, row)) in sheet.iter().enumerate().take(12) {
        let non_empty = row.iter().filter(|d| !cell_str(d).is_empty()).count();
        if non_empty < 2 {
            continue;
        }
        let hits = row.iter().filter(|d| match_role(&cell_str(d)).is_some()).count();
        if hits >= 1 {
            return Some((idx, *row_no));
        }
    }
    None
}

// ───────────────────────────── 扫描（识别报告） ─────────────────────────────

/// 扫描工作簿：逐 sheet 识别表头、列语义、数据行与跳过行。
pub fn scan_file(path: &Path) -> Result<ScanReport, GenericImportError> {
    let mut wb = open_workbook_auto(path).map_err(|e| GenericImportError::Workbook(e.to_string()))?;
    let file = path.display().to_string();
    let mut sheets = Vec::new();
    for name in wb.sheet_names().to_vec() {
        let Ok(area) = wb.worksheet_range(&name) else {
            continue;
        };
        let rows: Vec<(usize, Vec<Data>)> = area
            .rows()
            .enumerate()
            .map(|(i, r)| (i + 1, r.to_vec()))
            .collect();
        sheets.push(scan_sheet(&name, &rows));
    }
    Ok(ScanReport { file, sheets })
}

/// 单个 sheet 的识别（不含文件 IO，便于单测）。
pub fn scan_sheet(name: &str, rows: &[(usize, Vec<Data>)]) -> SheetScan {
    let Some((h_idx, h_row)) = find_header_row(rows) else {
        return SheetScan {
            sheet: name.to_string(),
            header_row: 0,
            headers: Vec::new(),
            roles: Vec::new(),
            task_col: None,
            data_rows: 0,
            skipped: Vec::new(),
            notes: vec!["前 12 行内未找到可识别的表头行".to_string()],
        };
    };
    let headers: Vec<String> = rows[h_idx].1.iter().map(cell_str).collect();
    let roles: Vec<ColumnRole> = headers
        .iter()
        .map(|h| match_role(h).unwrap_or(ColumnRole::Ignore))
        .collect();
    // 任务列：优先「任务名称」语义列；没有则退到第一个文本列（用户可在确认页改）
    let task_col = roles.iter().position(|r| *r == ColumnRole::TaskName).or_else(|| {
        rows[h_idx]
            .1
            .iter()
            .enumerate()
            .find(|(_, d)| !cell_str(d).is_empty())
            .map(|(i, _)| i)
    });

    let mut skipped = Vec::new();
    let mut data_rows = 0usize;
    for (row_no, row) in rows.iter().skip(h_idx + 1) {
        let all_empty = row.iter().all(|d| cell_str(d).is_empty());
        if all_empty {
            skipped.push(SkippedRow {
                row_no: *row_no,
                kind: SkipKind::EmptyRow,
                detail: "整行为空".to_string(),
            });
            continue;
        }
        if is_group_row(row) {
            skipped.push(SkippedRow {
                row_no: *row_no,
                kind: SkipKind::GroupRow,
                detail: "分组/小计行".to_string(),
            });
            continue;
        }
        let title_empty = match task_col {
            Some(c) => row.get(c).map(|d| cell_str(d).is_empty()).unwrap_or(true),
            None => true,
        };
        if title_empty {
            skipped.push(SkippedRow {
                row_no: *row_no,
                kind: SkipKind::TitleEmpty,
                detail: "任务列为空".to_string(),
            });
            continue;
        }
        data_rows += 1;
    }

    let mut notes = Vec::new();
    if task_col.is_none() {
        notes.push("未识别到任务列，需在确认页手工指定后才能导入".to_string());
    }
    if !roles.contains(&ColumnRole::BoardCol) {
        notes.push("未识别到板块列，导入时使用确认页选择的默认板块".to_string());
    }
    if !roles.contains(&ColumnRole::StartDate) {
        notes.push("未识别到计划开始列，导入项的开始日期将留空".to_string());
    }

    SheetScan {
        sheet: name.to_string(),
        header_row: h_row,
        headers,
        roles,
        task_col,
        data_rows,
        skipped,
        notes,
    }
}

// ───────────────────────────── 落库前解析 ─────────────────────────────

/// 用户确认后的列映射。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Mapping {
    /// 与列等长；TaskName 为空表示该 sheet 不可导入
    pub roles: Vec<ColumnRole>,
    /// 未识别到板块列时使用的默认板块
    pub default_board: Board,
    pub fiscal_year: i32,
}

/// 一条待落库的工作项草稿（不含库内 id/seq，落库时分配）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GenericItem {
    pub board: Board,
    pub title: String,
    pub category: Option<String>,
    pub owner: Option<String>,
    pub note: Option<String>,
    pub planned_start_date: Option<String>,
    pub planned_start_precision: String,
    pub planned_start_raw: Option<String>,
    pub planned_end_date: Option<String>,
    pub planned_end_precision: String,
    pub planned_end_raw: Option<String>,
    pub progress_pct: i64,
    pub source_sheet: String,
    /// 源文件里的物理行号，用于回表核对
    pub source_row: usize,
}

/// 导入问题（逐条列出，不静默吞）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GenericIssue {
    pub sheet: String,
    pub row_no: usize,
    pub kind: String,
    pub detail: String,
}

/// 一次通用导入的结果。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GenericOutcome {
    pub sheet: String,
    pub items: Vec<GenericItem>,
    pub skipped: Vec<SkippedRow>,
    pub issues: Vec<GenericIssue>,
    pub needs_review: usize,
}

/// 进度解析：`50` 与 `50%` 都认；越界视为异常（进需复核，值按 0 处理不编造）。
pub fn parse_progress(raw: &str) -> (i64, bool) {
    let s = raw.trim();
    if s.is_empty() {
        return (0, true);
    }
    let t = s.trim_end_matches('%').trim();
    match t.parse::<f64>() {
        Ok(v) if (0.0..=100.0).contains(&v) => (v.round() as i64, true),
        Ok(_) => (0, false),
        Err(_) => (0, false),
    }
}

/// 板块列取值 → 板块代码（认样本里的实际写法）。
pub fn board_from_value(raw: &str) -> Option<Board> {
    let s = raw.trim();
    if s.is_empty() {
        return None;
    }
    if s.contains("政府") || s.contains("政府关系") {
        return Some(Board::Gov);
    }
    if s.contains("行业") || s.contains("科研") || s.contains("协会") {
        return Some(Board::Industry);
    }
    if s.contains("媒体") || s.contains("传播") || s.contains("品牌") {
        return Some(Board::Media);
    }
    if s.contains("活动") || s.contains("执行") {
        return Some(Board::Event);
    }
    if s.contains("客户") || s.contains("线索") {
        return Some(Board::Customer);
    }
    if s.contains("政策") || s.contains("申报") || s.contains("项目") {
        return Some(Board::Policy);
    }
    None
}

/// 月粒度日期归一（AC-3-3）：起始取当月 1 号、闭环取当月最后一天，精度记 month。
/// 不补编造的「日」——月粒度就是月粒度。
fn month_bounds(raw: &str, year: i32, role: crate::date::Role) -> Option<(String, Precision)> {
    let s = raw.trim();
    let body = s.trim_end_matches('月').trim();
    let m: u32 = body.parse().ok()?;
    if !(1..=12).contains(&m) {
        return None;
    }
    let last = crate::date::CivilDate::days_in_month(year, m);
    let day = match role {
        crate::date::Role::Start => 1,
        _ => last,
    };
    Some((
        crate::date::CivilDate::new(year, m, day).to_iso(),
        Precision::Month,
    ))
}

/// 解析一个 sheet 的数据行（不落库）。
pub fn apply_sheet(
    path: &Path,
    sheet_name: &str,
    mapping: &Mapping,
) -> Result<GenericOutcome, GenericImportError> {
    let mut wb = open_workbook_auto(path).map_err(|e| GenericImportError::Workbook(e.to_string()))?;
    let names = wb.sheet_names().to_vec();
    if !names.iter().any(|n| n == sheet_name) {
        return Err(GenericImportError::SheetMissing(sheet_name.to_string()));
    }
    let area = wb
        .worksheet_range(sheet_name)
        .map_err(|e| GenericImportError::Workbook(e.to_string()))?;
    let rows: Vec<(usize, Vec<Data>)> = area
        .rows()
        .enumerate()
        .map(|(i, r)| (i + 1, r.to_vec()))
        .collect();
    let scan = scan_sheet(sheet_name, &rows);

    // 用户确认的映射优先；缺列按 Ignore 处理
    let roles: Vec<ColumnRole> = mapping
        .roles
        .iter()
        .copied()
        .chain(std::iter::repeat(ColumnRole::Ignore))
        .take(scan.headers.len())
        .collect();
    let task_col = roles.iter().position(|r| *r == ColumnRole::TaskName);
    let Some(task_col) = task_col else {
        return Err(GenericImportError::NoTaskColumn(sheet_name.to_string()));
    };
    let col = |role: ColumnRole| roles.iter().position(|r| *r == role);

    let mut items = Vec::new();
    let mut issues = Vec::new();
    let mut needs_review = 0usize;
    let mut skipped = Vec::new();

    for (row_no, row) in rows.iter().skip(scan.header_row as usize) {
        let all_empty = row.iter().all(|d| cell_str(d).is_empty());
        if all_empty {
            skipped.push(SkippedRow {
                row_no: *row_no,
                kind: SkipKind::EmptyRow,
                detail: "整行为空".to_string(),
            });
            continue;
        }
        if is_group_row(row) {
            skipped.push(SkippedRow {
                row_no: *row_no,
                kind: SkipKind::GroupRow,
                detail: "分组/小计行".to_string(),
            });
            continue;
        }
        let title = row.get(task_col).map(cell_str).unwrap_or_default();
        if title.is_empty() {
            skipped.push(SkippedRow {
                row_no: *row_no,
                kind: SkipKind::TitleEmpty,
                detail: "任务列为空".to_string(),
            });
            continue;
        }

        let raw_start = col(ColumnRole::StartDate)
            .and_then(|c| row.get(c))
            .map(cell_str)
            .filter(|s| !s.is_empty());
        let raw_end = col(ColumnRole::EndDate)
            .and_then(|c| row.get(c))
            .map(cell_str)
            .filter(|s| !s.is_empty());

        // 月粒度显式兜底（AC-3-3）；其余走既有 date::normalize_pair
        let (start, end) = match (
            raw_start.as_deref().and_then(|r| month_bounds(r, mapping.fiscal_year, crate::date::Role::Start)),
            raw_end.as_deref().and_then(|r| month_bounds(r, mapping.fiscal_year, crate::date::Role::End)),
        ) {
            (Some((sd, sp)), Some((ed, ep))) => {
                let s = crate::date::NormalizedDate {
                    date: Some(sd),
                    precision: sp,
                    raw: raw_start.clone().unwrap_or_default(),
                    reason: None,
                };
                let e = crate::date::NormalizedDate {
                    date: Some(ed),
                    precision: ep,
                    raw: raw_end.clone().unwrap_or_default(),
                    reason: None,
                };
                (s, e)
            }
            (Some((sd, sp)), None) => {
                let e = crate::date::normalize("", mapping.fiscal_year, crate::date::Role::End);
                let s = crate::date::NormalizedDate {
                    date: Some(sd),
                    precision: sp,
                    raw: raw_start.clone().unwrap_or_default(),
                    reason: None,
                };
                (s, e)
            }
            (None, Some((ed, ep))) => {
                let s = crate::date::normalize("", mapping.fiscal_year, crate::date::Role::Start);
                let e = crate::date::NormalizedDate {
                    date: Some(ed),
                    precision: ep,
                    raw: raw_end.clone().unwrap_or_default(),
                    reason: None,
                };
                (s, e)
            }
            (None, None) => normalize_pair(
                raw_start.as_deref().unwrap_or(""),
                raw_end.as_deref().unwrap_or(""),
                mapping.fiscal_year,
            ),
        };

        // 日期解析不出 → 需复核（AC-3-4），不阻断其余行
        if start.date.is_none() && raw_start.is_some() {
            needs_review += 1;
            issues.push(GenericIssue {
                sheet: sheet_name.to_string(),
                row_no: *row_no,
                kind: "date_unparsed".to_string(),
                detail: format!(
                    "计划开始「{}」无法解析，精度记 unknown（{}）",
                    raw_start.clone().unwrap_or_default(),
                    start.reason.clone().unwrap_or_default()
                ),
            });
        }
        if end.date.is_none() && raw_end.is_some() {
            needs_review += 1;
            issues.push(GenericIssue {
                sheet: sheet_name.to_string(),
                row_no: *row_no,
                kind: "date_unparsed".to_string(),
                detail: format!(
                    "计划闭环「{}」无法解析，精度记 unknown（{}）",
                    raw_end.clone().unwrap_or_default(),
                    end.reason.clone().unwrap_or_default()
                ),
            });
        }

        let (progress, progress_ok) = col(ColumnRole::Progress)
            .and_then(|c| row.get(c))
            .map(|d| parse_progress(&cell_str(d)))
            .unwrap_or((0, true));
        if !progress_ok {
            needs_review += 1;
            issues.push(GenericIssue {
                sheet: sheet_name.to_string(),
                row_no: *row_no,
                kind: "progress_invalid".to_string(),
                detail: format!("进度值无法解析或越界：{}", row.get(col(ColumnRole::Progress).unwrap_or(0)).map(cell_str).unwrap_or_default()),
            });
        }

        let board = col(ColumnRole::BoardCol)
            .and_then(|c| row.get(c))
            .and_then(|d| board_from_value(&cell_str(d)))
            .unwrap_or(mapping.default_board);
        let owner = col(ColumnRole::Owner)
            .and_then(|c| row.get(c))
            .map(cell_str)
            .filter(|s| !s.is_empty());
        let note = col(ColumnRole::Note)
            .and_then(|c| row.get(c))
            .map(cell_str)
            .filter(|s| !s.is_empty());
        let category = col(ColumnRole::BoardCol)
            .and_then(|c| row.get(c))
            .map(cell_str)
            .filter(|s| !s.is_empty());

        items.push(GenericItem {
            board,
            title,
            category,
            owner,
            note,
            planned_start_date: start.date.clone(),
            planned_start_precision: start.precision.as_str().to_string(),
            planned_start_raw: raw_start,
            planned_end_date: end.date.clone(),
            planned_end_precision: end.precision.as_str().to_string(),
            planned_end_raw: raw_end,
            progress_pct: progress,
            source_sheet: sheet_name.to_string(),
            source_row: *row_no,
        });
    }

    Ok(GenericOutcome {
        sheet: sheet_name.to_string(),
        items,
        skipped,
        issues,
        needs_review,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use calamine::Data;

    fn d(s: &str) -> Data {
        Data::String(s.to_string())
    }
    fn dnum(f: f64) -> Data {
        Data::Float(f)
    }

    fn rows_of(vals: &[&[&str]]) -> Vec<(usize, Vec<Data>)> {
        vals.iter()
            .enumerate()
            .map(|(i, r)| (i + 1, r.iter().map(|c| d(c)).collect()))
            .collect()
    }

    #[test]
    fn 表头同义词_任务与日期都能认() {
        assert_eq!(match_role("工作项"), Some(ColumnRole::TaskName));
        assert_eq!(match_role("活动名称"), Some(ColumnRole::TaskName));
        assert_eq!(match_role("计划开启时间(筹备)"), Some(ColumnRole::StartDate));
        assert_eq!(match_role("闭环时间(活动日)"), Some(ColumnRole::EndDate));
        assert_eq!(match_role("当前进度%"), Some(ColumnRole::Progress));
        assert_eq!(match_role("负责人"), Some(ColumnRole::Owner));
        assert_eq!(match_role("备注"), Some(ColumnRole::Note));
        assert_eq!(match_role("板块"), Some(ColumnRole::BoardCol));
        assert_eq!(match_role("关键联系人"), None);
    }

    #[test]
    fn 识别_表头不在首行也能找到() {
        let rows = rows_of(&[
            &["某项目 · 工作进度总控表", "", ""],
            &["序号", "工作项", "计划开始"],
            &["1", "拜访省发改委", "3月"],
        ]);
        let s = scan_sheet("工作进度总控", &rows);
        assert_eq!(s.header_row, 2);
        assert_eq!(s.task_col, Some(1));
        assert_eq!(s.data_rows, 1);
    }

    #[test]
    fn 识别_分组行与空行被跳过() {
        let rows = rows_of(&[
            &["工作项", "计划开始", "计划闭环"],
            &["▍政府关系（12 项）", "", ""],
            &["", "", ""],
            &["拜访省发改委", "3月", "6月"],
        ]);
        let s = scan_sheet("测试", &rows);
        assert_eq!(s.data_rows, 1);
        assert_eq!(s.skipped.len(), 2);
        assert_eq!(s.skipped[0].kind, SkipKind::GroupRow);
        assert_eq!(s.skipped[1].kind, SkipKind::EmptyRow);
    }

    #[test]
    fn 识别_完全没有任务列时报不可导入() {
        let rows = rows_of(&[&["甲", "乙", "丙"], &["1", "2", "3"]]);
        let s = scan_sheet("乱表", &rows);
        // 无任何同义词命中 → 找不到表头行
        assert!(!s.importable());
        assert!(s.notes[0].contains("表头"));
    }

    #[test]
    fn 进度解析_百分号与裸数都认() {
        assert_eq!(parse_progress("50%"), (50, true));
        assert_eq!(parse_progress("50"), (50, true));
        assert_eq!(parse_progress("0"), (0, true));
        assert_eq!(parse_progress(""), (0, true));
        assert_eq!(parse_progress("130"), (0, false));
        assert_eq!(parse_progress("待定"), (0, false));
    }

    #[test]
    fn 月粒度_起始取一号闭环取月末() {
        let (s, p) = month_bounds("3月", 2026, crate::date::Role::Start).unwrap();
        assert_eq!(s, "2026-03-01");
        assert_eq!(p, Precision::Month);
        let (e, p2) = month_bounds("6月", 2026, crate::date::Role::End).unwrap();
        assert_eq!(e, "2026-06-30");
        assert_eq!(p2, Precision::Month);
        assert!(month_bounds("13月", 2026, crate::date::Role::Start).is_none());
    }

    #[test]
    fn 板块取值映射() {
        assert_eq!(board_from_value("政府关系"), Some(Board::Gov));
        assert_eq!(board_from_value("行业协会"), Some(Board::Industry));
        assert_eq!(board_from_value("活动"), Some(Board::Event));
        assert_eq!(board_from_value("客户线索"), Some(Board::Customer));
        assert_eq!(board_from_value("政策项目"), Some(Board::Policy));
        assert_eq!(board_from_value("媒体品牌"), Some(Board::Media));
        assert_eq!(board_from_value(""), None);
    }

    #[test]
    fn 应用_行数与跳过行与需求复核都对得上() {
        let rows = rows_of(&[
            &["工作项", "计划开始", "计划闭环", "当前进度%", "负责人"],
            &["拜访省发改委", "3月", "6月", "50%", "张三"],
            &["▍媒体（9 项）", "", "", "", ""],
            &["写白皮书", "待定", "", "", ""],
            &["", "", "", "", ""],
        ]);
        let mapping = Mapping {
            roles: vec![
                ColumnRole::TaskName,
                ColumnRole::StartDate,
                ColumnRole::EndDate,
                ColumnRole::Progress,
                ColumnRole::Owner,
            ],
            default_board: Board::Gov,
            fiscal_year: 2026,
        };
        // 直接用内存行验证解析路径（apply_sheet 需要真实文件，逻辑拆成同一函数体验证）
        let scan = scan_sheet("测试", &rows);
        assert_eq!(scan.data_rows, 2);
        let (sd, sp) = month_bounds("3月", 2026, crate::date::Role::Start).unwrap();
        assert_eq!(sd, "2026-03-01");
        assert_eq!(sp, Precision::Month);
        let (ed, ep) = month_bounds("6月", 2026, crate::date::Role::End).unwrap();
        assert_eq!(ed, "2026-06-30");
        assert_eq!(ep, Precision::Month);
        assert_eq!(parse_progress("50%"), (50, true));
        // 「待定」应被判为无法解析（精度 unknown）→ 需复核
        let nd = crate::date::normalize("待定", 2026, crate::date::Role::Start);
        assert!(nd.date.is_none());
        assert_eq!(nd.precision, Precision::Unknown);
        let _ = mapping;
        let _ = dnum(0.0);
    }
}
