//! v2.3.0 线索 → 待办闭环（需求④）。
//!
//! 闭环只有一条通路：**发现（`Discovered`）→ 用户确认（`Confirmed`）→ 待办（`todo`）**。
//! 未确认的线索永不生成待办，因此不会打扰用户；辅源（聚合站）线索即使被确认也不直接派发，
//! 必须等主源落地确认（`needs_primary_confirm=true` 时拒绝派发）——这是去重口径里
//! 「辅源只进 pre_screen」的落地点。
//!
//! 幂等：同一线索反复点确认只产生一条待办，靠 `todo_id = lead:<dedup_key>` 派生，不是自增。

use serde::{Deserialize, Serialize};

/// 线索生命周期。`Rejected` 是终态：用户明确说不要，之后同 key 再进也不得复活。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LeadState {
    /// 已抓取、待用户判断
    Discovered,
    /// 用户确认相关
    Confirmed,
    /// 用户否决
    Rejected,
    /// 已生成待办（确认之后的落地态）
    Todoed,
}

impl LeadState {
    pub fn label(&self) -> &'static str {
        match self {
            LeadState::Discovered => "待确认",
            LeadState::Confirmed => "已确认",
            LeadState::Rejected => "已忽略",
            LeadState::Todoed => "已转待办",
        }
    }
}

/// 线索记录（落 `lead` 表）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Lead {
    pub dedup_key: String,
    pub dedup_layer: String,
    pub origin: Origin,
    pub title: String,
    pub org: String,
    pub area: String,
    pub date: String,
    pub url: String,
    pub amount: Option<String>,
    pub state: LeadState,
    /// 辅源线索为 true：必须等主源落地才能派待办
    pub needs_primary_confirm: bool,
    pub first_seen: String,
}

/// 源的性质。决定「能不能独立成线索 / 能不能生成待办」。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Origin {
    /// 16 地市分站 / 省监管网 / 企业官网（首发方）
    Primary,
    /// 聚合站（转载方，只作初筛）
    Auxiliary,
    /// 用户手动添加
    Manual,
}

impl Origin {
    /// 辅源永不成待办（口径 §3）。
    pub fn can_produce_todo(&self) -> bool {
        !matches!(self, Origin::Auxiliary)
    }
}

/// 待办条目（落 `work_item`，与既有工作项同一张表，因此能进晨报/看板）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LeadTodo {
    pub todo_id: String,
    pub lead_key: String,
    pub title: String,
    /// 截止日：从正文抽取，抽不到按 +3 工作日兜底
    pub due_date: String,
    pub source_url: String,
    pub owner: String,
    pub next_action: String,
}

/// 派发失败的原因（不是 panic，而是可展示的拒绝理由）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TodoReject {
    /// 状态不对：未确认 / 已忽略
    NotConfirmed(LeadState),
    /// 辅源线索，须先由主源确认
    NeedsPrimaryConfirm,
    /// 已忽略的线索不复活
    AlreadyRejected,
}

impl TodoReject {
    pub fn reason(&self) -> String {
        match self {
            TodoReject::NotConfirmed(s) => format!("线索状态为「{}」，未经用户确认，不进待办", s.label()),
            TodoReject::NeedsPrimaryConfirm => "辅源（聚合站）线索须先由主源确认，不单独派发待办".into(),
            TodoReject::AlreadyRejected => "线索已被用户忽略，不再生成待办".into(),
        }
    }
}

/// 用户确认动作：`Discovered → Confirmed`。已忽略的保持忽略（不复活）。
pub fn confirm(lead: &mut Lead) -> bool {
    match lead.state {
        LeadState::Discovered => {
            lead.state = LeadState::Confirmed;
            true
        }
        _ => false,
    }
}

/// 用户忽略动作：`Discovered/Confirmed → Rejected`。
pub fn reject(lead: &mut Lead) -> bool {
    match lead.state {
        LeadState::Discovered | LeadState::Confirmed => {
            lead.state = LeadState::Rejected;
            true
        }
        _ => false,
    }
}

/// 从线索生成待办。**只有 Confirmed + 非辅源** 才通过，其余一律拒绝并给理由。
pub fn to_todo(lead: &Lead, today: &str, owner: &str) -> Result<LeadTodo, TodoReject> {
    if lead.state == LeadState::Rejected {
        return Err(TodoReject::AlreadyRejected);
    }
    if lead.state != LeadState::Confirmed && lead.state != LeadState::Todoed {
        return Err(TodoReject::NotConfirmed(lead.state));
    }
    if !lead.origin.can_produce_todo() || lead.needs_primary_confirm {
        return Err(TodoReject::NeedsPrimaryConfirm);
    }
    Ok(LeadTodo {
        // 幂等键：同一条线索无论如何重复确认，都只有这一条待办
        todo_id: format!("lead:{}", lead.dedup_key),
        lead_key: lead.dedup_key.clone(),
        title: lead.title.clone(),
        due_date: due_date_from(&lead.title, today),
        source_url: lead.url.clone(),
        owner: owner.to_string(),
        next_action: format!("跟进「{}」投标事宜", lead.title),
    })
}

/// 截止日：正文/标题里若有「开标时间/投标截止」取之；否则 **+3 工作日**（跳过周末）。
///
/// 为什么是 3 个工作日：招标公告从发布到开标通常 ≥5 个日历日，但用户需要的是
/// 「我什么时候该动手」；3 个工作日给出可执行的缓冲，且不会因为跨周末而指向同一天。
pub fn due_date_from(text: &str, today: &str) -> String {
    if let Some(d) = extract_deadline(text) {
        return d;
    }
    add_workdays(today, 3)
}

/// 从文本抽「YYYY年M月D日」或「YYYY-MM-DD」。取第一个出现的合法日期。
pub fn extract_deadline(text: &str) -> Option<String> {
    let chars: Vec<char> = text.chars().collect();
    let mut i = 0usize;
    while i + 4 <= chars.len() {
        // 形态一：YYYY年M月D日
        if chars[i].is_ascii_digit()
            && chars[i + 1].is_ascii_digit()
            && chars[i + 2].is_ascii_digit()
            && chars[i + 3].is_ascii_digit()
        {
            let y: String = chars[i..i + 4].iter().collect();
            let yv: i32 = y.parse().ok()?;
            if (2000..=2100).contains(&yv) {
                // 年
                let mut j = i + 4;
                if j < chars.len() && (chars[j] == '年') {
                    j += 1;
                    let mut m = String::new();
                    while j < chars.len() && chars[j].is_ascii_digit() && m.len() < 2 {
                        m.push(chars[j]);
                        j += 1;
                    }
                    if j < chars.len() && chars[j] == '月' {
                        j += 1;
                        let mut d = String::new();
                        while j < chars.len() && chars[j].is_ascii_digit() && d.len() < 2 {
                            d.push(chars[j]);
                            j += 1;
                        }
                        if j < chars.len() && chars[j] == '日' {
                            let (mv, dv) = (m.parse::<u32>().ok()?, d.parse::<u32>().ok()?);
                            if (1..=12).contains(&mv) && (1..=31).contains(&dv) {
                                return Some(format!("{:04}-{:02}-{:02}", yv, mv, dv));
                            }
                        }
                    }
                } else if j < chars.len() && chars[j] == '-' {
                    // 形态二：YYYY-MM-DD
                    let seg: String = chars[i..(i + 10).min(chars.len())].iter().collect();
                    if let Some(d) = parse_iso(&seg) {
                        return Some(d);
                    }
                }
            }
        }
        i += 1;
    }
    None
}

fn parse_iso(s: &str) -> Option<String> {
    let parts: Vec<&str> = s.split('-').collect();
    if parts.len() != 3 {
        return None;
    }
    let y: i32 = parts[0].parse().ok()?;
    let m: u32 = parts[1].parse().ok()?;
    let d: u32 = parts[2].parse().ok()?;
    if !(2000..=2100).contains(&y) || !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return None;
    }
    Some(format!("{:04}-{:02}-{:02}", y, m, d))
}

/// 加 N 个工作日（跳过周六周日）。不处理法定节假日——节假日由上层日程模块统一处理，
/// 这里只保证「不会把截止日落在周末」这一条最常被用户感知的错误。
pub fn add_workdays(today: &str, n: u32) -> String {
    let mut days = match crate::lead_dedup::date_to_days(today) {
        Some(d) => d,
        None => return today.to_string(),
    };
    let mut left = n;
    while left > 0 {
        days += 1;
        // 1970-01-01 是周四；days % 7: 0→周四, 3→周日, 4→周一... 用 mod 7 判定
        let dow = (days + 4).rem_euclid(7); // 0=周日 … 6=周六
        if dow != 0 && dow != 6 {
            left -= 1;
        }
    }
    days_to_date(days)
}

fn days_to_date(z: i64) -> String {
    // 与 intel::civil_from_days 同算法（Howard Hinnant 的 civil_from_days）
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{:04}-{:02}-{:02}", y, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lead(state: LeadState, origin: Origin, npc: bool) -> Lead {
        Lead {
            dedup_key: "code:tltrzb[2026]074".into(),
            dedup_layer: "L3".into(),
            origin,
            title: "综合农事服务中心无人机采购项目招标公告".into(),
            org: "安徽省普济圩现代农业集团".into(),
            area: "铜陵市".into(),
            date: "2026-10-28".into(),
            url: "http://www.pjwnc.com/content/detail/x.html".into(),
            amount: Some("22万".into()),
            state,
            needs_primary_confirm: npc,
            first_seen: "2026-10-28".into(),
        }
    }

    #[test]
    fn discovered_lead_cannot_produce_todo() {
        // 需求④核心：未确认不进待办、不打扰
        let l = lead(LeadState::Discovered, Origin::Primary, false);
        let e = to_todo(&l, "2026-11-05", "owner").unwrap_err();
        assert!(matches!(e, TodoReject::NotConfirmed(LeadState::Discovered)));
        assert!(e.reason().contains("未经用户确认"));
    }

    #[test]
    fn confirmed_primary_lead_produces_todo() {
        let mut l = lead(LeadState::Discovered, Origin::Primary, false);
        assert!(confirm(&mut l));
        let t = to_todo(&l, "2026-11-05", "owner").expect("确认后应生成待办");
        assert_eq!(t.todo_id, "lead:code:tltrzb[2026]074");
        assert_eq!(t.owner, "owner");
        assert!(!t.source_url.is_empty());
    }

    #[test]
    fn auxiliary_lead_never_produces_todo_even_if_confirmed() {
        // 口径 §3：辅源不单独派待办，须主源确认
        let mut l = lead(LeadState::Discovered, Origin::Auxiliary, true);
        assert!(confirm(&mut l));
        let e = to_todo(&l, "2026-11-05", "o").unwrap_err();
        assert_eq!(e, TodoReject::NeedsPrimaryConfirm);
    }

    #[test]
    fn rejected_lead_stays_rejected_and_never_revives() {
        let mut l = lead(LeadState::Discovered, Origin::Primary, false);
        assert!(reject(&mut l));
        assert_eq!(l.state, LeadState::Rejected);
        assert!(!confirm(&mut l), "已忽略的线索不得被确认复活");
        assert!(matches!(to_todo(&l, "2026-11-05", "o"), Err(TodoReject::AlreadyRejected)));
    }

    #[test]
    fn todo_is_idempotent_by_dedup_key() {
        let mut l = lead(LeadState::Discovered, Origin::Primary, false);
        confirm(&mut l);
        let a = to_todo(&l, "2026-11-05", "o").unwrap();
        let b = to_todo(&l, "2026-11-06", "o").unwrap();
        assert_eq!(a.todo_id, b.todo_id, "重复确认只能产生同一条待办");
    }

    #[test]
    fn due_date_prefers_deadline_in_text() {
        let t = "投标截止时间：2026年11月20日9时30分";
        assert_eq!(extract_deadline(t), Some("2026-11-20".into()));
    }

    #[test]
    fn due_date_fallback_adds_three_workdays_skipping_weekend() {
        // 2026-11-05 是周四 → +3 工作日 = 11-10（跳过 11-07/08 周末）
        assert_eq!(add_workdays("2026-11-05", 3), "2026-11-10");
        // 2026-10-30 是周四 → +3 工作日 = 11-04
        assert_eq!(add_workdays("2026-10-30", 3), "2026-11-04");
        // 2026-11-06 是周五 → +3 工作日 = 11-11
        assert_eq!(add_workdays("2026-11-06", 3), "2026-11-11");
    }
}
