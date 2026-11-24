//! v3.4.0 项目跟踪数据层（架构 §3.2 / §8-W2）。
//!
//! 三张表各管一件事，边界刻意分开（与 `0007_project_track` 同构）：
//! - [`Project`]：项目本体，软删（`deleted_at`）；
//! - [`ProjectNote`]：过程记录，时间线的**唯一事实源**；
//! - [`ProjectAttachment`]：附件，挂在过程记录上，`sha256` 是内容指纹。
//!
//! ## 为什么 `occurred_at` 与 `created_at` 必须分开
//!
//! 补录上周的拜访、今天录入，时间线要落在上周。若按 `created_at` 排序，
//! 补录会让历史错位——这是过程记录类产品的经典塌方点（架构 §3.2 明确）。
//! 本层所有排序键只用 `occurred_at`。
//!
//! ## 删除语义（§3.4）
//!
//! - 删项目 → 只软删 `project.deleted_at`，**不级联删过程记录**（记录是历史资产）。
//! - 删过程记录 → 软删 `project_note.deleted_at`，**附件文件不立即删**：
//!   `sha256` 可能被其他记录复用，由独立的回收任务按「无引用」回收。
//! - 附件缺失 → 由界面层显示占位图 + 「文件已移动」，本层只如实报告文件是否存在。
//!
//! ## 附件存储（本地优先）
//!
//! `storage_path` 是**相对数据目录**的路径（架构 §3.2）。写入按内容寻址：
//! `attachments/<sha256 前 8 位>/<sha256>.<ext>`，同一份内容跨记录只落一次盘。
//! 这样「跨记录去重」在写入时就成立，回收任务只需按引用计数判断是否可删。

use std::fs;
use std::io::Write;

use rusqlite::params;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::store::{now_iso_pub, Store, StoreError};

// ───────────────────────── 项目 ─────────────────────────

/// 项目阶段短码（中文标签由界面映射，架构 §3.2）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Stage {
    Planned,
    Active,
    Closing,
    Closed,
    Archived,
}

impl Stage {
    pub fn code(&self) -> &'static str {
        match self {
            Stage::Planned => "planned",
            Stage::Active => "active",
            Stage::Closing => "closing",
            Stage::Closed => "closed",
            Stage::Archived => "archived",
        }
    }

    pub fn zh(&self) -> &'static str {
        match self {
            Stage::Planned => "筹备中",
            Stage::Active => "推进中",
            Stage::Closing => "收尾中",
            Stage::Closed => "已结项",
            Stage::Archived => "已归档",
        }
    }

    pub fn parse(s: &str) -> Option<Stage> {
        match s {
            "planned" => Some(Stage::Planned),
            "active" => Some(Stage::Active),
            "closing" => Some(Stage::Closing),
            "closed" => Some(Stage::Closed),
            "archived" => Some(Stage::Archived),
            _ => None,
        }
    }

    /// 全部阶段（界面下拉用，顺序即推进顺序）。
    pub fn all() -> [Stage; 5] {
        [Stage::Planned, Stage::Active, Stage::Closing, Stage::Closed, Stage::Archived]
    }
}

/// 建档 / 改档的草稿。只有 `name` 必填（§3.5-2 建档最小集）。
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ProjectDraft {
    pub name: String,
    pub client: Option<String>,
    pub area: Option<String>,
    pub stage: Option<Stage>,
    pub owner: Option<String>,
    pub summary: Option<String>,
    pub start_date: Option<String>,
    pub expect_end_date: Option<String>,
}

/// 必填项缺失的拒绝原因（界面据此高亮字段）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProjectReject {
    MissingName,
    /// 项目名与已有项目重复（§5.1 弹层规则「新建项目时项目名不可与已有重复」）
    DuplicateName(String),
    /// 阶段码非法
    BadStage(String),
    /// 日期不是 ISO 日期
    BadDate(String, String),
}

impl ProjectReject {
    pub fn reason(&self) -> String {
        match self {
            ProjectReject::MissingName => "项目名必填".into(),
            ProjectReject::DuplicateName(n) => format!("已有同名项目：{n}"),
            ProjectReject::BadStage(s) => format!("阶段码非法：{s}"),
            ProjectReject::BadDate(f, v) => format!("{f} 不是合法 ISO 日期：{v}"),
        }
    }
}

/// 一条项目（落库形态）。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Project {
    pub id: String,
    pub name: String,
    pub client: Option<String>,
    pub area: Option<String>,
    pub stage: String,
    pub owner: Option<String>,
    pub summary: Option<String>,
    pub start_date: Option<String>,
    pub expect_end_date: Option<String>,
    pub fiscal_year: i32,
    pub created_at: Option<String>,
    pub updated_at: Option<String>,
    pub deleted_at: Option<String>,
}

impl Project {
    pub fn stage_enum(&self) -> Stage {
        Stage::parse(&self.stage).unwrap_or(Stage::Planned)
    }
}

/// 项目 + 它的记录数 / 最近动作时间（列表页与穿透用）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectDigest {
    pub project: Project,
    pub note_count: i64,
    /// 最近一条记录的 `occurred_at`（无记录为 None）
    pub last_occurred_at: Option<String>,
}

/// 新建项目的结果。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreatedProject {
    pub project: Project,
}

fn uuid_like() -> String {
    // 不用外部 uuid 依赖：时间戳 + 进程内纳秒计数足够本地唯一，
    // 且 id 形如 `prj:/`note:` 前缀域，与 work_item.id 同域不重名（架构 §3.2）。
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let n = SEQ.fetch_add(1, Ordering::Relaxed);
    format!("{nanos:016x}{n:04x}")
}

fn is_iso_date(s: &str) -> bool {
    // 必须是补零的 `YYYY-MM-DD`，且是日历上真存在的日期。
    //
    // 这里不能直接用 `CivilDate::parse`：它只校验 m∈1..12、d∈1..31，
    // 于是 `2026-2-30`（不补零 + 2 月 30 日）会被当成合法值。
    // 这类字符串一旦落库成 `2026-2-30T00:00:00Z`，按字符串排序会排到
    // `2026-11-24` 之后——时间线顺序当场被破坏，而且是静默的。
    let b = s.as_bytes();
    if b.len() != 10 || b[4] != b'-' || b[7] != b'-' {
        return false;
    }
    if !b.iter().enumerate().all(|(i, c)| i == 4 || i == 7 || c.is_ascii_digit()) {
        return false;
    }
    let (y, m, d) = (s[0..4].parse::<i32>(), s[5..7].parse::<u32>(), s[8..10].parse::<u32>());
    let (Ok(y), Ok(m), Ok(d)) = (y, m, d) else {
        return false;
    };
    if !(1..=12).contains(&m) {
        return false;
    }
    let dim = crate::date::CivilDate::days_in_month(y, m);
    (1..=dim).contains(&d)
}

fn row_to_project(r: &rusqlite::Row<'_>) -> rusqlite::Result<Project> {
    Ok(Project {
        id: r.get("id")?,
        name: r.get("name")?,
        client: r.get("client")?,
        area: r.get("area")?,
        stage: r.get("stage")?,
        owner: r.get("owner")?,
        summary: r.get("summary")?,
        start_date: r.get("start_date")?,
        expect_end_date: r.get("expect_end_date")?,
        fiscal_year: r.get("fiscal_year")?,
        created_at: r.get("created_at")?,
        updated_at: r.get("updated_at")?,
        deleted_at: r.get("deleted_at")?,
    })
}

/// 建档：校验必填与唯一性，落库并返回实体。
///
/// `fiscal_year` 取 `start_date` 的年份；无 `start_date` 时取当前业务年度
/// （与 `work_item` 同口径，架构 §3.2）。
pub fn create_project(store: &Store, draft: &ProjectDraft, year: i32) -> Result<CreatedProject, ProjectReject> {
    if draft.name.trim().is_empty() {
        return Err(ProjectReject::MissingName);
    }
    let name = draft.name.trim().to_string();

    // 日期校验：非法日期当场拒绝，不静默丢弃（否则时间线会落错月）
    for (f, v) in [("start_date", &draft.start_date), ("expect_end_date", &draft.expect_end_date)] {
        if let Some(d) = v {
            if !d.trim().is_empty() && !is_iso_date(d.trim()) {
                return Err(ProjectReject::BadDate(f.to_string(), d.clone()));
            }
        }
    }

    let conn = store.conn();
    // 同名查重：只与未软删的项目比（已删项目的名字可以复用）
    let dup: Option<String> = conn
        .query_row(
            "SELECT name FROM project WHERE name=?1 AND deleted_at IS NULL",
            params![name],
            |r| r.get(0),
        )
        .ok();
    if let Some(d) = dup {
        return Err(ProjectReject::DuplicateName(d));
    }

    let stage = draft.stage.unwrap_or(Stage::Planned);
    let fy = draft
        .start_date
        .as_deref()
        .filter(|s| !s.trim().is_empty())
        .and_then(|s| s.split('-').next())
        .and_then(|y| y.parse::<i32>().ok())
        .unwrap_or(year);

    let id = format!("prj:{}", uuid_like());
    let now = now_iso_pub();
    conn.execute(
        "INSERT INTO project (id, name, client, area, stage, owner, summary, start_date,
             expect_end_date, fiscal_year, ext_json, created_at, updated_at)
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,'{}',?11,?11)",
        params![
            id,
            name,
            draft.client,
            draft.area,
            stage.code(),
            draft.owner,
            draft.summary,
            draft.start_date,
            draft.expect_end_date,
            fy,
            now,
        ],
    )
    .map_err(|e| ProjectReject::BadStage(e.to_string()))?;

    let project = get_project(store, &id)
        .map_err(to_reject)?
        .ok_or(ProjectReject::MissingName)?;
    Ok(CreatedProject { project })
}

/// 按 id 读项目（默认过滤软删）。
pub fn get_project(store: &Store, id: &str) -> Result<Option<Project>, StoreError> {
    let conn = store.conn();
    let mut stmt = conn.prepare("SELECT * FROM project WHERE id=?1 AND deleted_at IS NULL")?;
    let mut rows = stmt.query(params![id])?;
    if let Some(r) = rows.next()? {
        return Ok(Some(row_to_project(r)?));
    }
    Ok(None)
}

/// 项目列表：按最近动作时间倒序（无记录的项目按建档时间排后面）。
///
/// 这是任务版块首屏的读路径，`note_count` 与 `last_occurred_at` 一次带出，
/// 避免列表每行再查一次。
pub fn list_projects(store: &Store, year: i32, include_archived: bool) -> Result<Vec<ProjectDigest>, StoreError> {
    let conn = store.conn();
    let mut stmt = conn.prepare(
        "SELECT p.*,
                (SELECT COUNT(*) FROM project_note n
                  WHERE n.project_id=p.id AND n.deleted_at IS NULL) AS note_count,
                (SELECT MAX(n.occurred_at) FROM project_note n
                  WHERE n.project_id=p.id AND n.deleted_at IS NULL) AS last_occurred_at
           FROM project p
          WHERE p.fiscal_year=?1 AND p.deleted_at IS NULL
            AND (?2 OR p.archived_at IS NULL)
          ORDER BY last_occurred_at DESC, p.created_at DESC",
    )?;
    let rows = stmt.query_map(params![year, include_archived], |r| {
        Ok(ProjectDigest {
            project: row_to_project(r)?,
            note_count: r.get("note_count")?,
            last_occurred_at: r.get("last_occurred_at")?,
        })
    })?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r?);
    }
    Ok(out)
}

/// 改档：只覆盖传入的字段（None = 不改）。`name` 改成已存在的名字同样拒绝。
pub fn update_project(store: &Store, id: &str, draft: &ProjectDraft, by: &str) -> Result<Project, ProjectReject> {
    let existing = get_project(store, id).map_err(to_reject)?;
    let Some(mut p) = existing else {
        return Err(ProjectReject::BadStage(format!("项目不存在：{id}")));
    };

    let name = draft.name.trim();
    if !name.is_empty() && name != p.name {
        // 改名要查重（同样只比未软删的项目）
        let dup: Option<String> = store
            .conn()
            .query_row(
                "SELECT name FROM project WHERE name=?1 AND deleted_at IS NULL AND id<>?2",
                params![name, id],
                |r| r.get(0),
            )
            .ok();
        if let Some(d) = dup {
            return Err(ProjectReject::DuplicateName(d));
        }
        p.name = name.to_string();
    }

    for (f, v) in [("start_date", &draft.start_date), ("expect_end_date", &draft.expect_end_date)] {
        if let Some(d) = v {
            if !d.trim().is_empty() && !is_iso_date(d.trim()) {
                return Err(ProjectReject::BadDate(f.to_string(), d.clone()));
            }
        }
    }

    if let Some(v) = &draft.client {
        p.client = Some(v.clone());
    }
    if let Some(v) = &draft.area {
        p.area = Some(v.clone());
    }
    if let Some(s) = draft.stage {
        p.stage = s.code().to_string();
    }
    if let Some(v) = &draft.owner {
        p.owner = Some(v.clone());
    }
    if let Some(v) = &draft.summary {
        p.summary = Some(v.clone());
    }
    if let Some(v) = &draft.start_date {
        p.start_date = Some(v.clone());
        // 起始日期变了，fiscal_year 跟着走（§3.4「跨年项目 fiscal_year 取 start_date 年份」）
        if let Some(y) = v.split('-').next().and_then(|y| y.parse::<i32>().ok()) {
            p.fiscal_year = y;
        }
    }
    if let Some(v) = &draft.expect_end_date {
        p.expect_end_date = Some(v.clone());
    }

    let now = now_iso_pub();
    store.conn().execute(
        "UPDATE project SET name=?2, client=?3, area=?4, stage=?5, owner=?6, summary=?7,
             start_date=?8, expect_end_date=?9, fiscal_year=?10, updated_at=?11, updated_by=?12
         WHERE id=?1 AND deleted_at IS NULL",
        params![
            id, p.name, p.client, p.area, p.stage, p.owner, p.summary, p.start_date,
            p.expect_end_date, p.fiscal_year, now, by
        ],
    )
    .map_err(|e| ProjectReject::BadStage(e.to_string()))?;
    p.updated_at = Some(now);
    Ok(p)
}

/// 软删项目：**不级联删过程记录**（§3.4，记录是历史资产）。
pub fn soft_delete_project(store: &Store, id: &str, by: &str) -> Result<bool, StoreError> {
    let now = now_iso_pub();
    let n = store.conn().execute(
        "UPDATE project SET deleted_at=?2, updated_at=?2, updated_by=?3
         WHERE id=?1 AND deleted_at IS NULL",
        params![id, now, by],
    )?;
    Ok(n > 0)
}

fn to_reject(e: StoreError) -> ProjectReject {
    ProjectReject::BadStage(e.to_string())
}

// ───────────────────────── 过程记录 ─────────────────────────

/// 记录类型（决定动作卡图标，架构 §3.2）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum NoteKind {
    Note,
    Visit,
    Bid,
    Deliver,
    Milestone,
}

impl NoteKind {
    pub fn code(&self) -> &'static str {
        match self {
            NoteKind::Note => "note",
            NoteKind::Visit => "visit",
            NoteKind::Bid => "bid",
            NoteKind::Deliver => "deliver",
            NoteKind::Milestone => "milestone",
        }
    }
    pub fn zh(&self) -> &'static str {
        match self {
            NoteKind::Note => "记录",
            NoteKind::Visit => "拜访",
            NoteKind::Bid => "投标",
            NoteKind::Deliver => "交付",
            NoteKind::Milestone => "里程碑",
        }
    }
    pub fn parse(s: &str) -> Option<NoteKind> {
        match s {
            "note" => Some(NoteKind::Note),
            "visit" => Some(NoteKind::Visit),
            "bid" => Some(NoteKind::Bid),
            "deliver" => Some(NoteKind::Deliver),
            "milestone" => Some(NoteKind::Milestone),
            _ => None,
        }
    }
}

/// 录入一条过程记录的草稿（对应 §5.1 弹层字段）。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct NoteDraft {
    /// 归属项目（必填）
    pub project_id: String,
    /// 可选：挂在哪个具体任务上
    pub work_item_id: Option<String>,
    /// 发生日期 `YYYY-MM-DD`（必填，缺省由界面填今天）
    pub occurred_date: String,
    /// 发生时刻 `HH:MM`（可选，空则按日归月）
    pub occurred_time: Option<String>,
    /// 一行摘要（动作卡标题）
    pub title: Option<String>,
    /// 文字内容（**必填**，§5.1「实测唯一必填项」）
    pub body: String,
    pub place: Option<String>,
    pub kind: Option<NoteKind>,
}

/// 记录字段校验失败的原因。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NoteReject {
    MissingProject,
    MissingBody,
    MissingDate,
    BadDate(String),
    ProjectNotFound(String),
}

impl NoteReject {
    pub fn reason(&self) -> String {
        match self {
            NoteReject::MissingProject => "归属项目必填".into(),
            NoteReject::MissingBody => "内容必填".into(),
            NoteReject::MissingDate => "发生日期必填".into(),
            NoteReject::BadDate(d) => format!("发生日期不是合法 ISO 日期：{d}"),
            NoteReject::ProjectNotFound(id) => format!("归属项目不存在：{id}"),
        }
    }
}

/// 一条过程记录（落库形态）。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ProjectNote {
    pub id: String,
    pub project_id: String,
    pub work_item_id: Option<String>,
    /// 发生时间（ISO datetime）。时间线与全年总览的**唯一排序键**。
    pub occurred_at: String,
    pub title: Option<String>,
    pub body: Option<String>,
    pub place: Option<String>,
    pub kind: String,
    pub attachment_count: i64,
    pub created_at: Option<String>,
    pub updated_at: Option<String>,
    pub deleted_at: Option<String>,
}

impl ProjectNote {
    /// 归月键 `YYYY-MM`（全年总览按它聚合，§3.3）。
    pub fn month_key(&self) -> String {
        self.occurred_at.chars().take(7).collect()
    }
}

fn row_to_note(r: &rusqlite::Row<'_>) -> rusqlite::Result<ProjectNote> {
    Ok(ProjectNote {
        id: r.get("id")?,
        project_id: r.get("project_id")?,
        work_item_id: r.get("work_item_id")?,
        occurred_at: r.get("occurred_at")?,
        title: r.get("title")?,
        body: r.get("body")?,
        place: r.get("place")?,
        kind: r.get("kind")?,
        attachment_count: r.get("attachment_count")?,
        created_at: r.get("created_at")?,
        updated_at: r.get("updated_at")?,
        deleted_at: r.get("deleted_at")?,
    })
}

/// 把 `YYYY-MM-DD` + 可选 `HH:MM` 组成存储用 ISO datetime。
///
/// 只带日期时补 `T00:00:00Z`——**不取当前时刻**：补录历史日期若补当前时分秒，
/// 同一天内的先后顺序会变得不可解释。
fn occurred_at_from(date: &str, time: Option<&str>) -> String {
    match time.map(str::trim).filter(|t| !t.is_empty()) {
        Some(t) => format!("{date}T{t}:00Z"),
        None => format!("{date}T00:00:00Z"),
    }
}

/// 录入一条过程记录：校验 → 落库。返回实体。
pub fn create_note(store: &Store, draft: &NoteDraft, by: &str) -> Result<ProjectNote, NoteReject> {
    if draft.project_id.trim().is_empty() {
        return Err(NoteReject::MissingProject);
    }
    if draft.body.trim().is_empty() {
        return Err(NoteReject::MissingBody);
    }
    let date = draft.occurred_date.trim();
    if date.is_empty() {
        return Err(NoteReject::MissingDate);
    }
    if !is_iso_date(date) {
        return Err(NoteReject::BadDate(date.to_string()));
    }
    if get_project(store, &draft.project_id)
        .map_err(|e| NoteReject::BadDate(e.to_string()))?
        .is_none()
    {
        return Err(NoteReject::ProjectNotFound(draft.project_id.clone()));
    }

    let id = format!("note:{}", uuid_like());
    let now = now_iso_pub();
    let occurred_at = occurred_at_from(date, draft.occurred_time.as_deref());
    let kind = draft.kind.unwrap_or(NoteKind::Note);

    store.conn().execute(
        "INSERT INTO project_note (id, project_id, work_item_id, occurred_at, title, body,
             place, kind, attachment_count, created_at, updated_at, updated_by)
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,0,?9,?9,?10)",
        params![
            id,
            draft.project_id,
            draft.work_item_id,
            occurred_at,
            draft.title,
            draft.body.trim(),
            draft.place,
            kind.code(),
            now,
            by
        ],
    )
    .map_err(|e| NoteReject::BadDate(e.to_string()))?;

    get_note(store, &id)
        .map_err(|e| NoteReject::BadDate(e.to_string()))?
        .ok_or(NoteReject::MissingBody)
}

pub fn get_note(store: &Store, id: &str) -> Result<Option<ProjectNote>, StoreError> {
    let conn = store.conn();
    let mut stmt = conn.prepare("SELECT * FROM project_note WHERE id=?1")?;
    let mut rows = stmt.query(params![id])?;
    if let Some(r) = rows.next()? {
        return Ok(Some(row_to_note(r)?));
    }
    Ok(None)
}

/// 项目内时间线：按 `occurred_at` 倒序（§3.3 读路径）。
pub fn list_notes(store: &Store, project_id: &str) -> Result<Vec<ProjectNote>, StoreError> {
    let conn = store.conn();
    let mut stmt = conn.prepare(
        "SELECT * FROM project_note
          WHERE project_id=?1 AND deleted_at IS NULL
          ORDER BY occurred_at DESC",
    )?;
    let rows = stmt.query_map(params![project_id], row_to_note)?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r?);
    }
    Ok(out)
}

/// 软删过程记录：附件文件**不立即删**（`sha256` 可能被其他记录复用，§3.4）。
pub fn soft_delete_note(store: &Store, id: &str, by: &str) -> Result<bool, StoreError> {
    let now = now_iso_pub();
    let n = store.conn().execute(
        "UPDATE project_note SET deleted_at=?2, updated_at=?2, updated_by=?3
         WHERE id=?1 AND deleted_at IS NULL",
        params![id, now, by],
    )?;
    Ok(n > 0)
}

// ───────────────────────── 附件（内容寻址 + sha256 去重） ─────────────────────────

/// 一条附件（落库形态）。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ProjectAttachment {
    pub id: String,
    pub note_id: String,
    pub file_name: String,
    pub ext: Option<String>,
    pub mime: Option<String>,
    pub sha256: String,
    pub size_bytes: i64,
    pub storage_path: String,
    pub sort_no: i64,
    pub updated_at: Option<String>,
    pub deleted_at: Option<String>,
    /// 落盘文件当前是否存在（读时探测，供界面显示占位图而不是空白）
    pub file_exists: bool,
}

fn row_to_attachment(r: &rusqlite::Row<'_>) -> rusqlite::Result<ProjectAttachment> {
    Ok(ProjectAttachment {
        id: r.get("id")?,
        note_id: r.get("note_id")?,
        file_name: r.get("file_name")?,
        ext: r.get("ext")?,
        mime: r.get("mime")?,
        sha256: r.get("sha256")?,
        size_bytes: r.get("size_bytes")?,
        storage_path: r.get("storage_path")?,
        sort_no: r.get("sort_no")?,
        updated_at: r.get("updated_at")?,
        deleted_at: r.get("deleted_at")?,
        file_exists: true, // 由调用方回填
    })
}

/// 附件写入的结果：新增落盘 / 命中已有内容（跨记录去重复用）。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum AttachWrite {
    /// 首次见到这份内容，写了一份新文件
    Stored,
    /// 同 sha256 已存在，复用了既有落盘文件（`saved_bytes` 是本该占用的空间）
    Reused { saved_bytes: u64 },
}

/// 附件写入结果（含去重判定）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AttachedFile {
    pub attachment: ProjectAttachment,
    pub write: AttachWrite,
}

/// 内容寻址的相对路径：`attachments/<sha前8>/<sha>.<ext>`。
///
/// 按内容而不是按文件名寻址，同一份内容无论从哪条记录上传都落到同一路径，
/// 跨记录去重在写入时就成立，回收任务只需按引用计数判断。
fn content_path(sha: &str, ext: Option<&str>) -> String {
    let e = ext.map(|s| s.trim().trim_start_matches('.')).filter(|s| !s.is_empty());
    match e {
        Some(e) => format!("attachments/{}/{}.{}", &sha[..8.min(sha.len())], sha, e),
        None => format!("attachments/{}/{}", &sha[..8.min(sha.len())], sha),
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(bytes);
    format!("{:x}", h.finalize())
}

/// 把一个文件挂到过程记录上。
///
/// `local_source` 是用户选的源文件；内容按 sha256 寻址落到数据目录。
/// 若同一 sha256 的落盘文件已存在（无论属于哪条记录），**不重复写盘**，
/// 只新增一条 `project_attachment` 行指向同一 `storage_path`——这就是跨记录去重。
pub fn attach_file(
    store: &Store,
    note_id: &str,
    local_source: &std::path::Path,
    file_name: &str,
    mime: Option<&str>,
    sort_no: i64,
    by: &str,
) -> Result<AttachedFile, StoreError> {
    let bytes = fs::read(local_source).map_err(|e| StoreError::Io(format!("{}: {e}", local_source.display())))?;
    let sha = sha256_hex(&bytes);
    let ext = file_name.rsplit_once('.').map(|(_, e)| e.to_string());
    let rel = content_path(&sha, ext.as_deref());
    let abs = store.layout.root.join(&rel);

    let write = if abs.exists() {
        AttachWrite::Reused { saved_bytes: bytes.len() as u64 }
    } else {
        if let Some(parent) = abs.parent() {
            fs::create_dir_all(parent).map_err(|e| StoreError::Io(e.to_string()))?;
        }
        // 先写临时文件再 rename，避免半截文件被当成完整内容
        let tmp = abs.with_extension("part");
        {
            let mut f = fs::File::create(&tmp).map_err(|e| StoreError::Io(e.to_string()))?;
            f.write_all(&bytes).map_err(|e| StoreError::Io(e.to_string()))?;
        }
        fs::rename(&tmp, &abs).map_err(|e| StoreError::Io(e.to_string()))?;
        AttachWrite::Stored
    };

    let now = now_iso_pub();
    let id = format!("att:{}", uuid_like());
    store.conn().execute(
        "INSERT INTO project_attachment (id, note_id, file_name, ext, mime, sha256, size_bytes,
             storage_path, sort_no, updated_at, updated_by)
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)",
        params![
            id,
            note_id,
            file_name,
            ext.as_deref(),
            mime,
            sha,
            bytes.len() as i64,
            rel,
            sort_no,
            now,
            by
        ],
    )?;

    // 冗余计数（§3.2：避免每卡一次 count 查询）
    store.conn().execute(
        "UPDATE project_note SET attachment_count = attachment_count + 1 WHERE id=?1",
        params![note_id],
    )?;

    let mut att = get_attachment(store, &id)?.ok_or_else(|| StoreError::Io("附件写入后回读失败".into()))?;
    att.file_exists = abs.exists();
    Ok(AttachedFile { attachment: att, write })
}

pub fn get_attachment(store: &Store, id: &str) -> Result<Option<ProjectAttachment>, StoreError> {
    let conn = store.conn();
    let mut stmt = conn.prepare("SELECT * FROM project_attachment WHERE id=?1")?;
    let mut rows = stmt.query(params![id])?;
    if let Some(r) = rows.next()? {
        let mut a = row_to_attachment(r)?;
        a.file_exists = store.layout.root.join(&a.storage_path).exists();
        return Ok(Some(a));
    }
    Ok(None)
}

/// 一条记录的全部附件：按 `sort_no` 升序（卡片缩略图顺序）。
pub fn list_attachments(store: &Store, note_id: &str) -> Result<Vec<ProjectAttachment>, StoreError> {
    let conn = store.conn();
    let mut stmt = conn.prepare(
        "SELECT * FROM project_attachment
          WHERE note_id=?1 AND deleted_at IS NULL
          ORDER BY sort_no ASC, id ASC",
    )?;
    let rows = stmt.query_map(params![note_id], row_to_attachment)?;
    let mut out = Vec::new();
    for r in rows {
        let mut a = r?;
        // 「附件缺失」：卡片显示占位图 + 「文件已移动」，不得渲染为空白（§3.4）
        a.file_exists = store.layout.root.join(&a.storage_path).exists();
        out.push(a);
    }
    Ok(out)
}

// ───────────────────────── sha256 跨记录去重回收（带账本） ─────────────────────────

/// 回收账本的一行：一个 sha256 的引用与处置结论。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReclaimEntry {
    pub sha256: String,
    pub storage_path: String,
    pub size_bytes: i64,
    /// 回收前该内容被多少条未删记录引用
    pub live_refs: i64,
    /// 全部引用（含已删记录）的条数
    pub total_refs: i64,
    /// 处置：`keep`（仍有活引用）/ `reclaimed`（无活引用，文件已删）/ `missing`（文件本就不在）
    pub verdict: String,
    /// 回收动作实际释放的字节（`keep` 为 0）
    pub freed_bytes: i64,
}

/// 一次回收的账本。**禁止无账本硬删**（硬约束 2，照 `intel_clean.rs` 的留底模式）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReclaimLedger {
    pub generated_at: String,
    /// 账本落盘的绝对路径（调用方据此上云留痕）
    pub ledger_path: String,
    /// 回收前账本的路径（`dry_run` 时为 None；据此可对账「计划删的」与「实际删的」）
    pub before_ledger_path: Option<String>,
    pub scanned_sha256: usize,
    pub kept: usize,
    pub reclaimed: usize,
    pub missing: usize,
    pub freed_bytes: i64,
    pub entries: Vec<ReclaimEntry>,
}

/// 附件 sha256 跨记录去重回收：删掉「无任何未删记录引用」的落盘文件。
///
/// 三条硬规矩（照 `intel_clean.rs` 的留底模式）：
/// 1. **先记账后动手**：先把完整账本写到 `backups/clean/reclaim-<ts>.json`，
///    写盘成功才删文件；账本写失败则整轮中止，不删任何东西。
/// 2. **只删无活引用的内容**：判定依据是「未软删的 `project_attachment` 行」引用数。
///    已软删记录的附件行仍计入 `total_refs`，但不构成活引用——这正是要回收的对象。
/// 3. **幂等可重跑**：文件已不在时记 `missing` 而不是报错，重跑结论稳定。
///
/// `dry_run=true` 时只产账本、不删文件（界面预览用）。
pub fn reclaim_orphan_attachments(store: &Store, dry_run: bool) -> Result<ReclaimLedger, StoreError> {
    let conn = store.conn();
    let now = now_iso_pub();

    // 按 sha256 聚合：活引用 = 未软删的附件行数
    let mut stmt = conn.prepare(
        "SELECT a.sha256,
                a.storage_path,
                MAX(a.size_bytes) AS size_bytes,
                SUM(CASE WHEN a.deleted_at IS NULL AND n.deleted_at IS NULL THEN 1 ELSE 0 END) AS live_refs,
                COUNT(*) AS total_refs
           FROM project_attachment a
           JOIN project_note n ON n.id = a.note_id
          WHERE a.deleted_at IS NULL
          GROUP BY a.sha256
          ORDER BY a.sha256",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, i64>(2)?,
            r.get::<_, i64>(3)?,
            r.get::<_, i64>(4)?,
        ))
    })?;

    // ── 判定阶段：只算结论，不动文件 ──
    // 计划回收的条目先用 `reclaim_pending` 记，执行阶段再改成 reclaimed / failed。
    let mut entries = Vec::new();
    for r in rows {
        let (sha, path, size, live, total) = r?;
        let abs = store.layout.root.join(&path);
        let exists = abs.exists();
        let verdict = if !exists {
            "missing"
        } else if live > 0 {
            "keep"
        } else if dry_run {
            "reclaim_dry"
        } else {
            "reclaim_pending"
        };
        entries.push(ReclaimEntry {
            sha256: sha,
            storage_path: path,
            size_bytes: size,
            live_refs: live,
            total_refs: total,
            verdict: verdict.to_string(),
            freed_bytes: 0,
        });
    }

    let dir = store.layout.backups_dir().join("clean");
    fs::create_dir_all(&dir).map_err(|e| StoreError::Io(e.to_string()))?;
    let stamp = crate::store::now_ms_pub();
    let tag = if dry_run { "reclaim-dry" } else { "reclaim" };

    // ── 阶段 1：回收前账本先落盘（硬约束 2：禁止无账本硬删）──
    // 这份账本列的是「即将被判重删掉的 sha256」与预计释放量；写盘失败即整轮中止，一个文件都不删。
    let before_path = dir.join(format!("{tag}-before-{stamp}.json"));
    let before = ReclaimLedger {
        generated_at: now.clone(),
        ledger_path: before_path.to_string_lossy().to_string(),
        before_ledger_path: None,
        scanned_sha256: entries.len(),
        kept: entries.iter().filter(|e| e.verdict == "keep").count(),
        reclaimed: 0,
        missing: entries.iter().filter(|e| e.verdict == "missing").count(),
        freed_bytes: 0,
        entries: entries.clone(),
    };
    fs::write(&before_path, serde_json::to_string_pretty(&before).unwrap_or_default())
        .map_err(|e| StoreError::Io(format!("回收前账本写入失败，本轮不删任何文件：{e}")))?;

    // ── 阶段 2：执行回收（dry_run 跳过）──
    if !dry_run {
        for e in entries.iter_mut() {
            if e.verdict != "reclaim_pending" {
                continue;
            }
            let abs = store.layout.root.join(&e.storage_path);
            match fs::remove_file(&abs) {
                Ok(_) => {
                    e.verdict = "reclaimed".into();
                    e.freed_bytes = e.size_bytes;
                }
                // 删不动就如实记，不让整轮崩掉（下次重跑会再试）
                Err(err) => e.verdict = format!("failed: {err}"),
            }
        }
    }

    let freed: i64 = entries.iter().map(|e| e.freed_bytes).sum();
    let after = ReclaimLedger {
        generated_at: now,
        ledger_path: dir.join(format!("{tag}-after-{stamp}.json")).to_string_lossy().to_string(),
        before_ledger_path: Some(before_path.to_string_lossy().to_string()),
        scanned_sha256: entries.len(),
        kept: entries.iter().filter(|e| e.verdict == "keep").count(),
        reclaimed: entries
            .iter()
            .filter(|e| e.verdict == "reclaimed" || e.verdict == "reclaim_dry")
            .count(),
        missing: entries.iter().filter(|e| e.verdict == "missing").count(),
        freed_bytes: freed,
        entries,
    };

    // ── 阶段 3：回收后账本落盘（结果真值）──
    let after_path = std::path::PathBuf::from(&after.ledger_path);
    fs::write(&after_path, serde_json::to_string_pretty(&after).unwrap_or_default())
        .map_err(|e| StoreError::Io(format!("回收后账本写入失败：{e}")))?;

    Ok(after)
}

// ───────────────────────── 存量 work_item 迁移向导 ─────────────────────────

/// 迁移预览的一行：一个 `board_code` 分组会生成什么项目。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MigrationPlanRow {
    pub board_code: String,
    pub board_zh: String,
    pub item_count: i64,
    /// 将要创建的项目名（默认取板块中文名）
    pub project_name: String,
    /// 该组是否已有同名项目（有则复用而不新建）
    pub existing_project_id: Option<String>,
}

/// 迁移预览。**只读，不写库**——用户确认后才执行（§3.5 配套，2026-11-24 拍板）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MigrationPlan {
    pub pending_items: i64,
    pub rows: Vec<MigrationPlanRow>,
}

/// 预览：把「有 work_item 但无 project_id」的存量行按 `board_code` 分组。
pub fn preview_work_item_migration(store: &Store) -> Result<MigrationPlan, StoreError> {
    let conn = store.conn();
    let mut stmt = conn.prepare(
        "SELECT w.board_code, COALESCE(b.zh_name, w.board_code) AS zh, COUNT(*) AS n
           FROM work_item w
           LEFT JOIN domain_board b ON b.code = w.board_code
          WHERE w.deleted_at IS NULL AND (w.project_id = '' OR w.project_id IS NULL)
          GROUP BY w.board_code
          ORDER BY n DESC, w.board_code",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, i64>(2)?,
        ))
    })?;

    let mut out = Vec::new();
    let mut pending = 0i64;
    for r in rows {
        let (code, zh, n) = r?;
        pending += n;
        // 已存在同名项目则复用，避免重复建档
        let existing: Option<String> = conn
            .query_row(
                "SELECT id FROM project WHERE name=?1 AND deleted_at IS NULL",
                params![zh],
                |r| r.get(0),
            )
            .ok();
        out.push(MigrationPlanRow {
            board_code: code,
            board_zh: zh.clone(),
            item_count: n,
            project_name: zh,
            existing_project_id: existing,
        });
    }
    Ok(MigrationPlan { pending_items: pending, rows: out })
}

/// 一次迁移执行的结果。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MigrationResult {
    pub projects_created: usize,
    pub projects_reused: usize,
    pub items_linked: usize,
}

/// 执行迁移：按预览的分组建项目并回填 `work_item.project_id`。
///
/// **只改 `project_id` 一列**，不动 `board_code` 与任何其他既有字段（§5.1-2）。
/// 同一组要么复用同名项目、要么新建；整个操作在一个事务里，失败全回滚。
pub fn run_work_item_migration(
    store: &Store,
    plan: &MigrationPlan,
    year: i32,
    by: &str,
) -> Result<MigrationResult, StoreError> {
    let conn = store.conn();
    let tx = conn.unchecked_transaction()?;
    let now = now_iso_pub();
    let mut created = 0usize;
    let mut reused = 0usize;
    let mut linked = 0usize;

    for row in &plan.rows {
        let pid = match &row.existing_project_id {
            Some(id) => {
                reused += 1;
                id.clone()
            }
            None => {
                let id = format!("prj:{}", uuid_like());
                tx.execute(
                    "INSERT INTO project (id, name, stage, fiscal_year, ext_json, created_at, updated_at)
                     VALUES (?1, ?2, 'active', ?3, '{}', ?4, ?4)",
                    params![id, row.project_name, year, now],
                )?;
                created += 1;
                id
            }
        };
        let n = tx.execute(
            "UPDATE work_item SET project_id=?2, updated_at=?3, updated_by=?4
              WHERE board_code=?1 AND deleted_at IS NULL AND (project_id='' OR project_id IS NULL)",
            params![row.board_code, pid, now, by],
        )?;
        linked += n;
    }
    tx.commit()?;
    Ok(MigrationResult {
        projects_created: created,
        projects_reused: reused,
        items_linked: linked,
    })
}

