//! 「综合工作台 v2.0」可移植核心。
//!
//! 分层依据架构文档 §3.1 的判据：**这段逻辑改了，要不要重新发版**。
//! 本 crate 承载业务规则与系统层能力（导入、存储、备份、迁移、合并、更新校验），
//! 以纯函数与明确事务边界实现，可脱离 GUI 单测——这也是「实现自检」能给出实测数据的唯一路径。

pub mod anhui;
pub mod brief;
pub mod date;
pub mod entry;
pub mod import;
pub mod import_generic;
pub mod intel;
pub mod local_time;
pub mod model;
pub mod pct;
pub mod repo;
pub mod rules;
pub mod sfx;
pub mod store;
pub mod sync;
pub mod sync_client;
pub mod update;
pub mod voice;

/// 业务年度（工作台口径的「今年」）。v2.2.1 起固定为 **2027**：
/// 看板 / 明细 / 年度时间线 / 晨报的筛选年度统一取这一处，前端 app.js 的 `YEAR`
/// 由后端 `fiscal_year` 命令回填，两端不再各写一份常量而相互漂移。
pub const FISCAL_YEAR: i32 = 2027;

pub use date::{normalize, normalize_with_base, normalize_pair, CivilDate, NormalizedDate, Precision, RelativeBase, Role};
pub use entry::{quick_add, QuickAdd, QuickAddResult};
pub use model::{Board, ItemStatus, Priority, WorkItem};
pub use brief::{
    decide as brief_decide, minutes_until_next_fire, notification_suppressed, notification_text,
    BriefDecision, BriefVerdict, DeliveryState, QuietHours,
};
pub use sync_client::{SyncReport, SyncState, SyncStatus, Transport, TransportError};
pub use rules::{evaluate, morning_brief, week_bounds, Alert, AlertKind, Ruleset};
pub use repo::{derive_kpi, load_items, persist_import, PersistReport};
pub use store::{Layout, Store, StoreError};
pub use sync::{merge_batch, merge_row, select_delta, MergeDecision, MergeOutcome, SyncRow};
pub use update::{decide, parse_manifest, verify_signature, Manifest, UpdateDecision};
