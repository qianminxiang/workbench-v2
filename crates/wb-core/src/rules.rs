//! 主动辅助引擎：规则求值是**纯函数**（架构 §6.1），输入（今日、规则集、工作项集），
//! 输出候选提醒。纯函数才能让「跨月边界」「节假日顺延」这类边界被单测覆盖。
//!
//! 四条规则（架构 §6.2）：
//! - A 临期提醒：锚 planned_end_date，提前量按优先级分档，月粒度加 7 天缓冲
//! - B 里程碑预警：应然进度 − 实际进度 > 阈值，**且状态必须已进入进行中**
//! - C 周/月待办聚合：动态派生，不落存储
//! - D 每日晨报：四段结构，各端本地 9:00 触发

use crate::date::{CivilDate, Precision};
use crate::model::{ItemStatus, Priority, WorkItem};
use serde::{Deserialize, Serialize};

/// 提醒类型
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AlertKind {
    /// 逾期（最严重，排最前）
    Overdue,
    /// 临期
    DueSoon,
    /// 进度落后
    BehindSchedule,
    /// 滞留（同一里程碑阶段停留过久）
    Stalled,
}

impl AlertKind {
    /// 警示灯语义（UI 提案 P6：只有临期=琥珀、逾期=红，其余不借用警示色）
    pub fn severity(&self) -> u8 {
        match self {
            AlertKind::Overdue => 3,
            AlertKind::DueSoon => 2,
            AlertKind::BehindSchedule => 1,
            AlertKind::Stalled => 1,
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            AlertKind::Overdue => "逾期",
            AlertKind::DueSoon => "临期",
            AlertKind::BehindSchedule => "进度落后",
            AlertKind::Stalled => "阶段滞留",
        }
    }
}

/// 一条提醒
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Alert {
    pub kind: AlertKind,
    pub item_id: String,
    pub seq_no: i64,
    pub board: crate::model::Board,
    pub title: String,
    pub detail: String,
    /// 距离闭环锚点还有几天（负数=已逾期天数）
    pub days_left: Option<i64>,
    /// 去重键：规则 + 工作项 + 目标日期 + 触发窗口（架构 §4.6 的灵魂字段）
    pub dedup_key: String,
    /// 排序权重
    pub rank: u8,
}

/// 可配置规则集。阈值改动只在前端层，不进 Rust（架构 §3.1）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Ruleset {
    /// 临期提醒总开关
    pub due_soon_enabled: bool,
    /// 月粒度项目的额外缓冲天数（架构 §6.2 建议 7 天）
    pub month_precision_buffer_days: i64,
    /// 无优先级项的提前量
    pub unprioritized_lead_days: i64,
    /// 落后判定：A 级阈值（百分点）
    pub behind_threshold_a: f64,
    /// 落后判定：B/C/未分级阈值（百分点）
    pub behind_threshold_b: f64,
    /// 是否启用落后判定
    pub behind_enabled: bool,
    /// 滞留天数阈值（架构 §6.2 建议 21 天；PRD F2 写 14 天，取 PRD 口径）
    pub stall_days: i64,
    /// 是否启用滞留预警
    pub stall_enabled: bool,
    /// 单日主动提醒配额（架构 §6.3 建议 5 条）
    pub daily_quota: usize,
}

impl Default for Ruleset {
    fn default() -> Self {
        Ruleset {
            due_soon_enabled: true,
            month_precision_buffer_days: 7,
            unprioritized_lead_days: 7,
            behind_threshold_a: 20.0,
            behind_threshold_b: 30.0,
            behind_enabled: false, // P1，首版默认关闭
            stall_days: 14,        // PRD F2 口径（架构建议 21，取更严的 PRD 值）
            stall_enabled: false,  // P1，首版默认关闭
            daily_quota: 5,
        }
    }
}

impl Ruleset {
    /// 某项的临期提前量：优先级分档 + 月粒度缓冲
    fn lead_days(&self, item: &WorkItem) -> (i64, &'static str) {
        let base = match item.priority {
            Some(p) => p.lead_days(),
            None => self.unprioritized_lead_days,
        };
        // 月粒度项：区间末端为锚点，额外加缓冲（「6月完成」往往月中就要收口）
        if item.end_precision() == Precision::Month {
            return (base + self.month_precision_buffer_days, "含月粒度缓冲");
        }
        if item.end_precision() == Precision::Quarter {
            return (base + self.month_precision_buffer_days, "含季度粒度缓冲");
        }
        (base, "按优先级档位")
    }
}

/// 求值主入口。`items` 应只包含**未归档**的在办项。
///
/// 排序：逾期 → 临期 → 落后 → 滞留；同类型内按优先级（A 先）与剩余天数（少先）。
/// `daily_quota` 只约束「主动推送类」提醒，超出部分仍进应用内列表（架构 §6.3 日配额）。
pub fn evaluate(today: CivilDate, items: &[WorkItem], ruleset: &Ruleset) -> Vec<Alert> {
    let mut alerts: Vec<Alert> = Vec::new();

    for item in items {
        if item.status.map(|s| s.is_closed()).unwrap_or(false) {
            continue;
        }
        if item.start_precision() == Precision::Ongoing {
            // 常驻项不参与临期提醒（架构 §5.2）
            continue;
        }

        let end = item.end_date();
        let end_p = item.end_precision();

        // ---- A1 逾期：已过闭环且未闭环（不自动关闭）
        if let Some(end) = end {
            if end_p.is_known() && end_p != Precision::Ongoing {
                let left = CivilDate::diff_days(today, end);
                if left < 0 {
                    alerts.push(Alert {
                        kind: AlertKind::Overdue,
                        item_id: item.id.clone(),
                        seq_no: item.seq_no,
                        board: item.board,
                        title: item.title.clone(),
                        detail: format!(
                            "计划闭环 {} 已过 {} 天仍未闭环{}",
                            item.planned_end_raw.clone().unwrap_or_else(|| end.to_iso()),
                            -left,
                            precision_hint(end_p)
                        ),
                        days_left: Some(left),
                        dedup_key: format!("overdue:{}:{}", item.id, end.to_iso()),
                        rank: rank_of(item),
                    });
                } else if ruleset.due_soon_enabled {
                    // ---- A2 临期
                    let (lead, why) = ruleset.lead_days(item);
                    if left <= lead {
                        alerts.push(Alert {
                            kind: AlertKind::DueSoon,
                            item_id: item.id.clone(),
                            seq_no: item.seq_no,
                            board: item.board,
                            title: item.title.clone(),
                            detail: format!(
                                "距计划闭环 {} 还有 {} 天（提前 {} 天提醒，{}）",
                                item.planned_end_raw.clone().unwrap_or_else(|| end.to_iso()),
                                left,
                                lead,
                                why
                            ),
                            days_left: Some(left),
                            dedup_key: format!("due:{}:{}:{}", item.id, end.to_iso(), lead),
                            rank: rank_of(item),
                        });
                    }
                }
            }
        }

        // ---- B 里程碑/进度预警：**只对已进入进行中的项**判定
        if ruleset.behind_enabled {
            if let (Some(start), Some(end)) = (item.start_date(), end) {
                let is_active = item.status == Some(ItemStatus::Active) || item.progress_pct > 0;
                // 关键防线：待启动/未筹备项进度为 0 是初始态，不是落后
                let eligible = item.status == Some(ItemStatus::Active)
                    && !item.status.map(|s| s.is_not_started()).unwrap_or(true);
                let _ = is_active;
                if eligible && end > start {
                    let total = CivilDate::diff_days(start, end) as f64;
                    let elapsed = CivilDate::diff_days(start, today).max(0) as f64;
                    let should = (elapsed / total * 100.0).clamp(0.0, 100.0);
                    let actual = item.progress_pct as f64;
                    let gap = should - actual;
                    let threshold = if item.priority == Some(Priority::A) {
                        ruleset.behind_threshold_a
                    } else {
                        ruleset.behind_threshold_b
                    };
                    if gap > threshold {
                        alerts.push(Alert {
                            kind: AlertKind::BehindSchedule,
                            item_id: item.id.clone(),
                            seq_no: item.seq_no,
                            board: item.board,
                            title: item.title.clone(),
                            detail: format!(
                                "应然进度 {:.0}%，实际 {:.0}%，偏差 {:.0} 个百分点（阈值 {:.0}）",
                                should, actual, gap, threshold
                            ),
                            days_left: Some(CivilDate::diff_days(today, end)),
                            dedup_key: format!("behind:{}:{}:{:.0}", item.id, today.to_iso(), gap / 10.0),
                            rank: rank_of(item),
                        });
                    }
                }
            }
        }

        // ---- 滞留：同阶段停留过久（依赖 updated_at）
        if ruleset.stall_enabled {
            if let Some(updated) = item.updated_at.as_deref().and_then(|u| {
                u.get(0..10).and_then(CivilDate::parse)
            }) {
                let dwell = CivilDate::diff_days(updated, today);
                if item.status == Some(ItemStatus::Active) && dwell >= ruleset.stall_days {
                    alerts.push(Alert {
                        kind: AlertKind::Stalled,
                        item_id: item.id.clone(),
                        seq_no: item.seq_no,
                        board: item.board,
                        title: item.title.clone(),
                        detail: format!(
                            "已 {} 天无进度更新（阈值 {} 天），检查下一步动作：{}",
                            dwell,
                            ruleset.stall_days,
                            item.next_action.clone().unwrap_or_else(|| "未填写".into())
                        ),
                        days_left: None,
                        dedup_key: format!("stall:{}:{}", item.id, item.milestone_stage.clone().unwrap_or_default()),
                        rank: rank_of(item),
                    });
                }
            }
        }
    }

    alerts.sort_by(|a, b| {
        b.kind
            .severity()
            .cmp(&a.kind.severity())
            .then(a.rank.cmp(&b.rank))
            .then(
                a.days_left
                    .unwrap_or(i64::MAX)
                    .cmp(&b.days_left.unwrap_or(i64::MAX)),
            )
            .then(a.seq_no.cmp(&b.seq_no))
    });
    alerts
}

fn precision_hint(p: Precision) -> &'static str {
    match p {
        Precision::Month => "（月粒度，按整月区间计）",
        Precision::Quarter => "（季度粒度，按整季区间计）",
        Precision::Day => "",
        _ => "",
    }
}

fn rank_of(item: &WorkItem) -> u8 {
    match item.priority {
        Some(Priority::A) => 0,
        Some(Priority::B) => 1,
        Some(Priority::C) => 2,
        None => 3,
    }
}

/// 周区间：周一 00:00 至周日 23:59（PRD F9 与 F2 共用同一套周定义）。
pub fn week_bounds(today: CivilDate) -> (CivilDate, CivilDate) {
    // 2026-01-01 是周四；用余数对齐 ISO 周（周一为一周首日）
    let days = today.to_days();
    // 1970-01-01 是周四 → +3 天对齐到周一
    let offset = (days + 3).rem_euclid(7);
    let monday = today.add_days(-offset);
    let sunday = monday.add_days(6);
    (monday, sunday)
}

/// 月区间
pub fn month_bounds(today: CivilDate) -> (CivilDate, CivilDate) {
    let first = CivilDate::new(today.year, today.month, 1);
    let last = CivilDate::new(
        today.year,
        today.month,
        CivilDate::days_in_month(today.year, today.month),
    );
    (first, last)
}

/// 晨报四段结构（架构 §6.2 D）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MorningBrief {
    /// ① 当日到期 / 当日应启动
    pub due_today: Vec<Alert>,
    /// ② 临期与逾期汇总
    pub due_soon: Vec<Alert>,
    pub overdue: Vec<Alert>,
    /// ③ 今日行程（首版由 schedule_block 提供，此处只留结构）
    pub today_schedule: Vec<String>,
    /// ④ 周一时附加的周待办聚合
    pub weekly: Option<WeeklyAggregate>,
    /// 生成日
    pub brief_date: String,
}

/// 周待办聚合（动态派生，不落存储）
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct WeeklyAggregate {
    /// 本周应闭环
    pub should_close: Vec<String>,
    /// 本周应启动
    pub should_start: Vec<String>,
    pub week_start: String,
    pub week_end: String,
}

/// 生成晨报。`today` 由 Rust 时钟注入（前端领域层不读时钟，架构 §4.11-4）。
pub fn morning_brief(today: CivilDate, items: &[WorkItem], ruleset: &Ruleset) -> MorningBrief {
    let alerts = evaluate(today, items, ruleset);
    let (ws, we) = week_bounds(today);

    let due_today: Vec<Alert> = alerts
        .iter()
        .filter(|a| a.days_left == Some(0))
        .cloned()
        .collect();

    let overdue: Vec<Alert> = alerts
        .iter()
        .filter(|a| a.kind == AlertKind::Overdue)
        .cloned()
        .collect();

    let due_soon: Vec<Alert> = alerts
        .iter()
        .filter(|a| a.kind == AlertKind::DueSoon)
        .cloned()
        .collect();

    // 周一只出周聚合（ISO 周一 = 1）
    // 1970-01-01 是周四 → 距 1970-01-01 的天数模 7：0=周四 1=周五 2=周六 3=周日 4=周一 5=周二 6=周三
    let is_monday = today.to_days().rem_euclid(7) == 4;
    let weekly = if is_monday {
        let mut should_start = Vec::new();
        let mut should_close = Vec::new();
        for item in items {
            if item.status.map(|s| s.is_closed()).unwrap_or(false) {
                continue;
            }
            if let Some(s) = item.start_date() {
                if s >= ws && s <= we && item.status.map(|st| st.is_not_started()).unwrap_or(true) {
                    should_start.push(item.title.clone());
                }
            }
            if let Some(e) = item.end_date() {
                if e >= ws && e <= we {
                    should_close.push(item.title.clone());
                }
            }
        }
        Some(WeeklyAggregate {
            should_close,
            should_start,
            week_start: ws.to_iso(),
            week_end: we.to_iso(),
        })
    } else {
        None
    };

    MorningBrief {
        due_today,
        due_soon,
        overdue,
        today_schedule: Vec::new(),
        weekly,
        brief_date: today.to_iso(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Board;

    fn item(seq: i64, start: &str, end: &str, sp: &str, ep: &str, status: ItemStatus, prog: i64) -> WorkItem {
        WorkItem {
            id: format!("it{}", seq),
            seq_no: seq,
            board: Board::Gov,
            title: format!("测试项 {}", seq),
            category: None,
            priority: Some(Priority::A),
            target_quant: None,
            planned_start_date: (!start.is_empty()).then(|| start.to_string()),
            planned_start_precision: sp.to_string(),
            planned_start_raw: Some("2月".into()),
            planned_end_date: (!end.is_empty()).then(|| end.to_string()),
            planned_end_precision: ep.to_string(),
            planned_end_raw: Some("6月".into()),
            progress_pct: prog,
            status: Some(status),
            milestone_stage: Some("M1".into()),
            next_action: Some("启动对接".into()),
            owner: None,
            source_sheet: None,
            fiscal_year: 2026,
            ext: Default::default(),
            created_at: None,
            updated_at: Some("2026-06-01T00:00:00Z".into()),
        }
    }

    #[test]
    fn pending_items_never_count_as_behind() {
        // 首启 64 项全 0 进度：不做初始态排除会立刻弹几十条「严重落后」
        let mut r = Ruleset::default();
        r.behind_enabled = true;
        let it = item(1, "2026-02-01", "2026-06-30", "month", "month", ItemStatus::Pending, 0);
        let alerts = evaluate(CivilDate::new(2026, 9, 1), &[it], &r);
        assert!(alerts.iter().all(|a| a.kind != AlertKind::BehindSchedule));
    }

    #[test]
    fn active_item_can_be_behind() {
        let mut r = Ruleset::default();
        r.behind_enabled = true;
        let it = item(1, "2026-02-01", "2026-06-30", "month", "month", ItemStatus::Active, 10);
        let alerts = evaluate(CivilDate::new(2026, 5, 1), &[it], &r);
        assert!(alerts.iter().any(|a| a.kind == AlertKind::BehindSchedule));
    }

    #[test]
    fn overdue_detected_and_not_auto_closed() {
        let it = item(1, "2026-02-01", "2026-06-30", "month", "month", ItemStatus::Pending, 0);
        let alerts = evaluate(CivilDate::new(2026, 7, 15), &[it], &Ruleset::default());
        let od: Vec<_> = alerts.iter().filter(|a| a.kind == AlertKind::Overdue).collect();
        assert_eq!(od.len(), 1);
        assert_eq!(od[0].days_left, Some(-15));
    }

    #[test]
    fn due_soon_within_lead_window() {
        let it = item(1, "2026-02-01", "2026-06-30", "month", "month", ItemStatus::Pending, 0);
        // A 级 14 天 + 月粒度缓冲 7 = 21 天；6/30 往前 10 天 → 命中
        let alerts = evaluate(CivilDate::new(2026, 6, 20), &[it], &Ruleset::default());
        assert!(alerts.iter().any(|a| a.kind == AlertKind::DueSoon));
    }

    #[test]
    fn due_soon_respects_switch_off() {
        let mut r = Ruleset::default();
        r.due_soon_enabled = false;
        let it = item(1, "2026-02-01", "2026-06-30", "month", "month", ItemStatus::Pending, 0);
        let alerts = evaluate(CivilDate::new(2026, 6, 20), &[it], &r);
        assert!(alerts.iter().all(|a| a.kind != AlertKind::DueSoon));
    }

    #[test]
    fn ongoing_items_are_skipped() {
        let it = item(1, "2026-01-01", "", "ongoing", "ongoing", ItemStatus::Pending, 0);
        let alerts = evaluate(CivilDate::new(2026, 12, 31), &[it], &Ruleset::default());
        assert!(alerts.is_empty());
    }

    #[test]
    fn week_bounds_are_monday_to_sunday() {
        // 2026-10-26 是周一
        let (a, b) = week_bounds(CivilDate::new(2026, 10, 26));
        assert_eq!(a.to_iso(), "2026-10-26");
        assert_eq!(b.to_iso(), "2026-11-01");
    }

    #[test]
    fn week_bounds_mid_week() {
        // 2026-10-29 是周四 → 本周一 10-26
        let (a, b) = week_bounds(CivilDate::new(2026, 10, 29));
        assert_eq!(a.to_iso(), "2026-10-26");
        assert_eq!(b.to_iso(), "2026-11-01");
    }

    #[test]
    fn weekly_aggregate_only_on_monday() {
        let it = item(1, "2026-10-27", "2026-10-30", "day", "day", ItemStatus::Pending, 0);
        let brief_mon = morning_brief(CivilDate::new(2026, 10, 26), &[it.clone()], &Ruleset::default());
        assert!(brief_mon.weekly.is_some());
        let brief_tue = morning_brief(CivilDate::new(2026, 10, 27), &[it], &Ruleset::default());
        assert!(brief_tue.weekly.is_none());
    }
}
