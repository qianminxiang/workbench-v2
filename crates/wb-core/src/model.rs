//! 领域模型：六大板块、工作项、优先级、状态、KPI 指标。
//!
//! 与架构 §4.3 主干 + §4.4 扩展表一致：共性与差异分开，差异字段放在 `ext: HashMap`，
//! 避免把 49 个板块专有列塞进一张宽表（架构 §4.2 的明确否决项）。

use serde::{Deserialize, Serialize};

/// 六大板块。`code` 与存量 Excel 的板块名一一对应，是导入映射的稳定键。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Board {
    /// 政府关系建立
    Gov,
    /// 行业资源开发
    Industry,
    /// 媒体品牌传播
    Media,
    /// 活动执行管理
    Event,
    /// 客户线索转化
    Customer,
    /// 政策项目申报
    Policy,
}

impl Board {
    pub const ALL: [Board; 6] = [
        Board::Gov,
        Board::Industry,
        Board::Media,
        Board::Event,
        Board::Customer,
        Board::Policy,
    ];

    pub fn code(&self) -> &'static str {
        match self {
            Board::Gov => "gov",
            Board::Industry => "industry",
            Board::Media => "media",
            Board::Event => "event",
            Board::Customer => "customer",
            Board::Policy => "policy",
        }
    }

    /// 存量 Excel「板块」列的显示名
    pub fn zh_name(&self) -> &'static str {
        match self {
            Board::Gov => "政府关系",
            Board::Industry => "行业资源",
            Board::Media => "媒体传播",
            Board::Event => "活动执行",
            Board::Customer => "客户转化",
            Board::Policy => "政策申报",
        }
    }

    /// 明细 sheet 名（导入时按 sheet 定位扩展字段）
    pub fn sheet_name(&self) -> &'static str {
        match self {
            Board::Gov => "政府关系建立",
            Board::Industry => "行业资源开发",
            Board::Media => "媒体品牌传播",
            Board::Event => "活动执行管理",
            Board::Customer => "客户线索转化",
            Board::Policy => "政策项目申报",
        }
    }

    /// 存量表里同一个板块有多种写法：总控表用「政府关系」，明细 sheet 用「政府关系建立」，
    /// KPI 表用更短的「政策项目」。别名映射是为了让 KPI 的统计口径能正确落到板块上——
    /// 落不上会退化成全库口径，把「政策项目总数」算成 64 而不是 8。
    pub fn from_zh(s: &str) -> Option<Board> {
        let t = s.trim();
        if let Some(b) = Board::ALL.into_iter().find(|b| {
            b.zh_name() == t || b.sheet_name() == t || b.code() == t
        }) {
            return Some(b);
        }
        match t {
            "政策项目" | "政策项目申报" => Some(Board::Policy),
            "政府关系建立" | "政府关系对象" => Some(Board::Gov),
            "行业资源开发" | "行业科研" => Some(Board::Industry),
            "媒体品牌" | "媒体报道" => Some(Board::Media),
            "活动管理" | "活动" => Some(Board::Event),
            "客户线索" | "客户" => Some(Board::Customer),
            _ => None,
        }
    }

    pub fn from_code(s: &str) -> Option<Board> {
        Board::ALL.into_iter().find(|b| b.code() == s)
    }

    /// 是否属于「对外关系」板块——决定是否必须挂单位档案（PRD F8）。
    pub fn is_external(&self) -> bool {
        matches!(self, Board::Gov | Board::Industry | Board::Media | Board::Customer)
    }
}

/// 优先级。存量表有 `—` 18 项，导入时映射为 `None` 而非臆造等级（架构 §4.3）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Priority {
    A,
    B,
    C,
}

impl Priority {
    pub fn as_str(&self) -> &'static str {
        match self {
            Priority::A => "A",
            Priority::B => "B",
            Priority::C => "C",
        }
    }

    pub fn from_raw(s: &str) -> Option<Priority> {
        match s.trim().trim_end_matches('级') {
            "A" | "a" => Some(Priority::A),
            "B" | "b" => Some(Priority::B),
            "C" | "c" => Some(Priority::C),
            _ => None,
        }
    }

    /// 提前量（天）——架构 §6.2 A 节分档
    pub fn lead_days(self) -> i64 {
        match self {
            Priority::A => 14,
            Priority::B => 7,
            Priority::C => 3,
        }
    }
}

/// 工作项状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ItemStatus {
    /// 待启动（存量表「待启动」）
    Pending,
    /// 未筹备（活动板块的存量用词，语义等同待启动）
    Unprepared,
    /// 进行中
    Active,
    /// 已完成
    Done,
    /// 已暂停
    Paused,
    /// 已取消
    Cancelled,
}

impl ItemStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            ItemStatus::Pending => "待启动",
            ItemStatus::Unprepared => "未筹备",
            ItemStatus::Active => "进行中",
            ItemStatus::Done => "已完成",
            ItemStatus::Paused => "已暂停",
            ItemStatus::Cancelled => "已取消",
        }
    }

    pub fn from_zh(s: &str) -> Option<ItemStatus> {
        match s.trim() {
            "待启动" => Some(ItemStatus::Pending),
            "未筹备" => Some(ItemStatus::Unprepared),
            "进行中" => Some(ItemStatus::Active),
            "已完成" | "已闭环" => Some(ItemStatus::Done),
            "已暂停" => Some(ItemStatus::Paused),
            "已取消" => Some(ItemStatus::Cancelled),
            _ => None,
        }
    }

    /// 尚未启动（含活动的「未筹备」）。**这类项只参与临期提醒，不参与落后判定**——
    /// 不做这条排除，首启 64 项全 0 进度会立刻弹出几十条「严重落后」（架构 §6.2 B）。
    pub fn is_not_started(&self) -> bool {
        matches!(self, ItemStatus::Pending | ItemStatus::Unprepared)
    }

    pub fn is_closed(&self) -> bool {
        matches!(self, ItemStatus::Done | ItemStatus::Cancelled)
    }
}

/// 工作项主干（对应架构 §4.3 `work_item`）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkItem {
    pub id: String,
    /// 原 Excel 序号 1–64
    pub seq_no: i64,
    pub board: Board,
    pub title: String,
    /// 类型/层级：省级、地市级、行业协会、演示会……
    pub category: Option<String>,
    pub priority: Option<Priority>,
    pub target_quant: Option<String>,
    pub planned_start_date: Option<String>,
    pub planned_start_precision: String,
    pub planned_start_raw: Option<String>,
    pub planned_end_date: Option<String>,
    pub planned_end_precision: String,
    pub planned_end_raw: Option<String>,
    pub progress_pct: i64,
    pub status: Option<ItemStatus>,
    pub milestone_stage: Option<String>,
    pub next_action: Option<String>,
    pub owner: Option<String>,
    pub source_sheet: Option<String>,
    pub fiscal_year: i32,
    /// 板块专有字段（对应 ext_* 扩展表）
    pub ext: std::collections::BTreeMap<String, String>,
    pub created_at: Option<String>,
    pub updated_at: Option<String>,
}

impl WorkItem {
    pub fn start_date(&self) -> Option<crate::date::CivilDate> {
        self.planned_start_date.as_deref().and_then(crate::date::CivilDate::parse)
    }

    pub fn end_date(&self) -> Option<crate::date::CivilDate> {
        self.planned_end_date.as_deref().and_then(crate::date::CivilDate::parse)
    }

    pub fn start_precision(&self) -> crate::date::Precision {
        crate::date::Precision::parse(&self.planned_start_precision)
    }

    pub fn end_precision(&self) -> crate::date::Precision {
        crate::date::Precision::parse(&self.planned_end_precision)
    }
}

/// KPI 指标（对应架构 §4.3 的 `kpi_link`：当前完成是派生值，不落死值）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KpiMetric {
    pub id: String,
    pub category: String,
    pub name: String,
    pub target_value: f64,
    /// 自动统计口径（哪个板块 / 哪个状态条件）
    pub rule: KpiRule,
    pub source_sheet: Option<String>,
    pub note: Option<String>,
}

/// KPI 自动统计口径。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KpiRule {
    /// 该板块工作项总数
    BoardTotal,
    /// 该板块已启动数（进度 > 0 或状态为进行中/已完成）
    BoardStarted,
    /// 该板块已闭环数
    BoardDone,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn board_mapping_is_bijective() {
        for b in Board::ALL {
            assert_eq!(Board::from_zh(b.zh_name()), Some(b));
            assert_eq!(Board::from_zh(b.sheet_name()), Some(b));
            assert_eq!(Board::from_code(b.code()), Some(b));
        }
    }

    #[test]
    fn dash_priority_maps_to_none_not_guess() {
        assert_eq!(Priority::from_raw("—"), None);
        assert_eq!(Priority::from_raw("A级"), Some(Priority::A));
    }

    #[test]
    fn unprepared_status_is_not_started() {
        assert!(ItemStatus::Unprepared.is_not_started());
        assert!(ItemStatus::Pending.is_not_started());
        assert!(!ItemStatus::Active.is_not_started());
    }
}
