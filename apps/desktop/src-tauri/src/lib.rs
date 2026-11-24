//!
//! 架构 §3.1 分层判据：这个文件只做「系统层」——窗口、托盘、系统通知、
//! 把领域核心（wb-core）的结果递给前端。业务逻辑一律不写在这里。
//!
//! B7 最小形态在这里落地：
//! - 托盘常驻：应用关闭窗口不退出，晨报从托盘送达（系统通知）；
//! - 9:00 本地时钟调度：不依赖网络与服务端，断网照常；迟到补发；
//! - **状态必须落盘**：`last_brief.json` 是「完全退出后下次启动补推」的唯一依据，
//!   缺了写入方，补推链路就是空壳（2026-10-27 复核指出的洞）。
//!
//! v2.1 变更（用户 2026-10-27 版本目标）：
//! - 单机化：移除多端同步的传输层调用与设置页入口（server/ 与 wb-core sync
//!   纯函数保留不删，可逆决策）；
//! - B7① 修复：today()/now_minutes() 加本地时区偏移——now_iso() 本体不动
//!   （sync LWW 依赖 UTC 单调串），换算走 wb-core::local_time 纯函数；
//! - 免打扰可编辑 + 语音播报上线（F10）：偏好持久化走 sync_state 键值表；
//! - 桌面自动更新：tauri-plugin-updater 打通「检查 → 下载 → 验签 → 重启安装」。

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::Duration;

use tauri::menu::{Menu, MenuItem};
use tauri::tray::TrayIconBuilder;
use tauri::{Emitter, Manager, State};
use tauri_plugin_notification::NotificationExt;
use tauri_plugin_updater::UpdaterExt;
use wb_core::{
    brief::{self, DeliveryState, QuietHours},
    intel, intel_manual, intel_payload, intel_store,
    rules::{self, Ruleset},
    sync_client,
    CivilDate, Store, StoreError,
};

/// 自动更新 E2E 驱动（仅 `--features e2e-update` 编译，见 Cargo.toml 该 feature 注释）。
#[cfg(feature = "e2e-update")]
pub mod e2e;


/// 应用状态：数据库根目录（登录用户数据目录）在启动时确定，之后只读。
struct AppPaths {
    data_dir: PathBuf,
}

fn err_string(e: StoreError) -> String {
    e.to_string()
}

/// 本机时区相对 UTC 的偏移分钟（东八区 = +480）。时间只由系统层提供（架构 §4.11-4）。
/// P0 修复（v2.2.3）：`chrono` 的 `local_minus_utc()` 返回的是**秒**（UTC+8 = 28800），
/// 此前未换算直接当分钟传给 `civil_and_minutes`，28800 分钟 = 整 20 天，
/// 导致 UTC+8 机器上所有日期 +20 天（2026-10-29 显示成 2026-11-18）。
fn local_offset_minutes() -> i32 {
    offset_seconds_to_minutes(chrono::Local::now().offset().local_minus_utc())
}

/// 适配层换算：chrono 偏移秒 → 偏移分钟。独立纯函数，供注入非零偏移做回归单测。
fn offset_seconds_to_minutes(seconds: i32) -> i32 {
    seconds / 60
}

/// 现在（本地时区）：自然日 + 当日分钟数，一次性换算保持二者一致。
/// B7① 修复点：此前直接截 `now_iso()` 的 UTC 字段，UTC+8 下晨报 9:00 要拖到
/// 本地 17:00 才成立、免打扰窗口整体错 8 小时。换算是 wb-core 纯函数，
/// UTC+8 语义在 test-core 里被穷举单测（local_time.rs 八条回归）。
fn now_local() -> (CivilDate, u32) {
    let iso = wb_core::store::now_iso_pub();
    wb_core::local_time::civil_and_minutes(&iso, local_offset_minutes())
        .unwrap_or_else(|| (CivilDate::new(2026, 1, 1), 0))
}

/// 今天（本机时区的自然日）。
fn today() -> CivilDate {
    now_local().0
}

/// 当日分钟数（本地时间）。**从系统时钟取，不在领域层各读一遍**。
fn now_minutes() -> u32 {
    now_local().1
}

// ─────────────────────── 单机偏好（免打扰） ───────────────────────

/// 读偏好：免打扰窗口。持久化通道是 sync_state 键值表——
/// 多端模式已放弃，但键值表仍是官方通道（老 workbench.db 无需迁移、可逆）。
/// 免打扰缺省回 22:00–8:00（与首版默认一致）。
/// v3.0 集成：语音播报/音效/主题开关已整体移除，本区只剩免打扰一个偏好。
fn load_prefs(conn: &rusqlite::Connection) -> QuietHours {
    let hhmm = |key: &str| {
        sync_client::state_get(conn, key).and_then(|v| wb_core::local_time::parse_hhmm(&v))
    };
    QuietHours {
        start_minute: hhmm(sync_client::keys::QUIET_START).unwrap_or(22 * 60),
        end_minute: hhmm(sync_client::keys::QUIET_END).unwrap_or(8 * 60),
    }
}

/// 偏好的 JSON 形态（time input 的 value 格式就是 HH:MM）。
fn prefs_json(conn: &rusqlite::Connection) -> serde_json::Value {
    let quiet = load_prefs(conn);
    serde_json::json!({
        "quiet_start": wb_core::local_time::fmt_hhmm(quiet.start_minute),
        "quiet_end": wb_core::local_time::fmt_hhmm(quiet.end_minute),
    })
}

fn open_store(paths: &AppPaths) -> Result<Store, String> {
    Store::open(&paths.data_dir).map_err(err_string)
}

// ─────────────────────────── 数据命令 ───────────────────────────

#[tauri::command]
fn import_excel(
    paths: State<Mutex<AppPaths>>,
    xlsx_path: String,
    year: i32,
) -> Result<serde_json::Value, String> {
    let paths = paths.lock().unwrap();
    // ① 解析 Excel（领域层纯函数）
    let out = wb_core::import::import_workbook(&xlsx_path, year).map_err(|e| e.to_string())?;
    let report = out.report.clone();
    // ② 落库（写前备份在 store.begin_write 内部完成，失败即阻断）
    let store = open_store(&paths)?;
    let persist = wb_core::repo::persist_import(&store, &out, year).map_err(err_string)?;
    wb_core::repo::record_import_report(store.conn(), &report).map_err(err_string)?;
    Ok(serde_json::json!({
        "report": report,
        "persist": persist,
    }))
}

#[tauri::command]
fn list_items(paths: State<Mutex<AppPaths>>, year: i32) -> Result<Vec<wb_core::WorkItem>, String> {
    let paths = paths.lock().unwrap();
    let store = open_store(&paths)?;
    wb_core::repo::load_items(store.conn(), year).map_err(err_string)
}

/// KPI 总览（B2）：指标、目标值、当前完成（派生，None=口径落不到板块，UI 显示「—」）。
#[tauri::command]
fn kpi_overview(
    paths: State<Mutex<AppPaths>>,
    year: i32,
) -> Result<Vec<serde_json::Value>, String> {
    let paths = paths.lock().unwrap();
    let store = open_store(&paths)?;
    let rows = wb_core::repo::derive_kpi(store.conn(), year).map_err(err_string)?;
    Ok(rows
        .into_iter()
        .map(|(name, target, current, rule)| {
            serde_json::json!({
                "name": name,
                "target": target,
                "current": current,
                "rule": rule,
            })
        })
        .collect())
}

/// 主动辅助引擎输出：提醒候选 + 晨报（F2）。规则求值是纯函数（架构 §6.1）。
#[tauri::command]
fn alerts(
    paths: State<Mutex<AppPaths>>,
    year: i32,
) -> Result<serde_json::Value, String> {
    let paths = paths.lock().unwrap();
    let store = open_store(&paths)?;
    let items = wb_core::repo::load_items(store.conn(), year).map_err(err_string)?;
    let rs = Ruleset::default();
    let t = today();
    let alerts = rules::evaluate(t, &items, &rs);
    let brief = rules::morning_brief(t, &items, &rs);
    Ok(serde_json::json!({
        "today": t.to_iso(),
        "alerts": alerts,
        "brief": {
            "date": brief.brief_date,
            "due_today": brief.due_today,
            "due_soon": brief.due_soon,
            "overdue": brief.overdue,
        },
    }))
}

/// F8 骨架（首版 P0）：单位档案 + 单位页聚合。
#[tauri::command]
fn units(paths: State<Mutex<AppPaths>>, year: i32) -> Result<Vec<serde_json::Value>, String> {
    let paths = paths.lock().unwrap();
    let store = open_store(&paths)?;
    let items = wb_core::repo::load_items(store.conn(), year).map_err(err_string)?;
    // 单位名在各板块 ext 字段里的键不同（明细 join 的语义键）
    let key_of = |b: wb_core::Board| match b {
        wb_core::Board::Gov | wb_core::Board::Industry => "单位名称",
        wb_core::Board::Media => "媒体名称",
        wb_core::Board::Customer => "客户名称",
        _ => "",
    };
    let mut map: std::collections::BTreeMap<String, Vec<serde_json::Value>> = Default::default();
    for it in &items {
        let key = key_of(it.board);
        if key.is_empty() {
            continue;
        }
        let Some(name) = it.ext.get(key) else { continue };
        let trimmed = name.trim();
        if trimmed.is_empty() || trimmed == "1" {
            continue; // 架构 §2.3：存量表存在占位行（客户名称=1），骨架同样排除
        }
        map.entry(trimmed.to_string()).or_default().push(serde_json::json!({
            "id": it.id,
            "board": it.board.zh_name(),
            "title": it.title,
            "status": it.status,
            "progress_pct": it.progress_pct,
            "next_action": it.next_action,
        }));
    }
    Ok(map
        .into_iter()
        .map(|(name, items)| {
            let open_cnt = items.iter().filter(|i| i["status"] != "done").count();
            serde_json::json!({
                "name": name,
                "item_count": items.len(),
                "open_count": open_cnt,
                "items": items,
            })
        })
        .collect())
}

/// 手动触发一次 L2 全量备份（设置页的「立即备份」）。
#[tauri::command]
fn backup_now(paths: State<Mutex<AppPaths>>) -> Result<serde_json::Value, String> {
    let paths = paths.lock().unwrap();
    let store = open_store(&paths)?;
    let rec = store.full_backup("manual").map_err(err_string)?;
    Ok(serde_json::json!({
        "path": rec.path,
        "bytes": rec.bytes,
        "integrity": rec.integrity,
        "json_tables": rec.json_tables,
        "at": wb_core::store::now_iso_pub(),
    }))
}

/// 单机偏好读取（设置页回填 + 主界面开关初始化）。
#[tauri::command]
fn get_prefs(paths: State<Mutex<AppPaths>>) -> Result<serde_json::Value, String> {
    let paths = paths.lock().unwrap();
    let store = open_store(&paths)?;
    Ok(prefs_json(store.conn()))
}

/// 单机偏好写入：免打扰窗口（HH:MM）。v3.0 集成后语音/音效/主题已整体移除，
/// 契约只剩两个字段。时间格式在本地就拦下，不合法不落库；成功返回回读后的规范值。
#[tauri::command]
fn set_prefs(
    paths: State<Mutex<AppPaths>>,
    quiet_start: String,
    quiet_end: String,
) -> Result<serde_json::Value, String> {
    let s = wb_core::local_time::parse_hhmm(&quiet_start)
        .ok_or_else(|| "免打扰开始时间格式不合法（应为 HH:MM）".to_string())?;
    let e = wb_core::local_time::parse_hhmm(&quiet_end)
        .ok_or_else(|| "免打扰结束时间格式不合法（应为 HH:MM）".to_string())?;
    {
        let paths = paths.lock().unwrap();
        let store = open_store(&paths)?;
        sync_client::state_set(
            store.conn(),
            sync_client::keys::QUIET_START,
            &wb_core::local_time::fmt_hhmm(s),
        )
        .map_err(err_string)?;
        sync_client::state_set(
            store.conn(),
            sync_client::keys::QUIET_END,
            &wb_core::local_time::fmt_hhmm(e),
        )
        .map_err(err_string)?;
        Ok(prefs_json(store.conn()))
    }
}

/// 业务年度（v2.2.1）：前端启动时回填 YEAR，看板/明细/时间线/晨报同一年。
#[tauri::command]
fn fiscal_year() -> i32 {
    wb_core::FISCAL_YEAR
}

// ─────────────────────── v3.0 服务器中枢：真实同步命令 ───────────────────────
//
// 传输层 = wb_core::sync_client::http::HttpTransport（ureq + TLS），与服务端
// https://ahxbd.work 的链路已由 `wbd sync-live` 真实首推验证（2026-11-17，8/8）。
// 设计要点：
// - 口令只在登录/初始化请求里出现，**不落库、不进日志**；本机库只存服务端
//   发回的令牌（与 v2.1 之前同一通道 sync_state）。
// - sync_status 的返回里**永不包含令牌本身**（wb-core SyncStatus 结构就没有该字段）。
// - 登录/初始化成功后立刻补一轮同步，用户不用再手动点一次。

fn sync_status_value(conn: &rusqlite::Connection) -> serde_json::Value {
    let st = sync_client::read_status(conn);
    // 角色以成员表为准（成员表为空时回落登录返回的角色，再没有就是 member）：
    // 界面「成员」页要靠它决定哪些按钮可点，不能让两个来源各说一套。
    let ident = wb_core::collab::identity(conn);
    let role = if ident.known {
        ident.role
    } else {
        sync_client::state_get(conn, sync_client::keys::MEMBER_ROLE)
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| wb_core::collab::ROLE_MEMBER.to_string())
    };
    serde_json::json!({
        "state": st.state.label(),
        "base_url": st.base_url,
        "logged_in": st.logged_in,
        "member_name": st.member_name,
        "role": role,
        "role_known": ident.known,
        "cursor": st.cursor,
        "pending": st.pending,
        "last_ok_at": st.last_ok_at,
        "last_error": st.last_error,
    })
}

/// 同步状态（设置页回显；不含令牌）。
#[tauri::command]
fn sync_status(paths: State<Mutex<AppPaths>>) -> Result<serde_json::Value, String> {
    let paths = paths.lock().unwrap();
    let store = open_store(&paths)?;
    Ok(sync_status_value(store.conn()))
}

/// 首成员初始化（服务器只允许一次，成功即 admin），随后立刻同步一轮。
#[tauri::command]
fn sync_setup(
    paths: State<Mutex<AppPaths>>,
    base_url: String,
    username: String,
    password: String,
) -> Result<serde_json::Value, String> {
    let paths = paths.lock().unwrap();
    let store = open_store(&paths)?;
    let transport = sync_client::http::HttpTransport::new();
    let now = wb_core::store::now_iso_pub();
    let seed = format!("desktop:{}@{base_url}", username);
    sync_client::setup_first(store.conn(), &base_url, &username, &password, &seed, &transport, &now)
        .map_err(|e| e.describe())?;
    let report = run_sync_once(store.conn(), &transport);
    Ok(serde_json::json!({ "status": sync_status_value(store.conn()), "report": report }))
}

/// 登录（服务器已初始化后走这个），随后立刻同步一轮。
#[tauri::command]
fn sync_login(
    paths: State<Mutex<AppPaths>>,
    base_url: String,
    username: String,
    password: String,
) -> Result<serde_json::Value, String> {
    let paths = paths.lock().unwrap();
    let store = open_store(&paths)?;
    let transport = sync_client::http::HttpTransport::new();
    let now = wb_core::store::now_iso_pub();
    let seed = format!("desktop:{}@{base_url}", username);
    sync_client::login(store.conn(), &base_url, &username, &password, &seed, &transport, &now)
        .map_err(|e| e.describe())?;
    let report = run_sync_once(store.conn(), &transport);
    Ok(serde_json::json!({ "status": sync_status_value(store.conn()), "report": report }))
}

/// 登出：作废本机令牌，本地数据与游标保留（离线继续可用）。
#[tauri::command]
fn sync_logout(paths: State<Mutex<AppPaths>>) -> Result<serde_json::Value, String> {
    let paths = paths.lock().unwrap();
    let store = open_store(&paths)?;
    let transport = sync_client::http::HttpTransport::new();
    sync_client::logout(store.conn(), &transport).map_err(|e| e.describe())?;
    Ok(sync_status_value(store.conn()))
}

fn run_sync_once(conn: &rusqlite::Connection, transport: &sync_client::http::HttpTransport) -> serde_json::Value {
    let base_url = sync_client::state_get(conn, sync_client::keys::BASE_URL).unwrap_or_default();
    let token = sync_client::state_get(conn, sync_client::keys::TOKEN).unwrap_or_default();
    if base_url.is_empty() || token.is_empty() {
        return serde_json::json!({ "error": "尚未登录" });
    }
    let now_ms = wb_core::store::now_ms_pub();
    let now_iso = wb_core::store::now_iso_pub();
    let r = sync_client::sync_once(conn, &base_url, &token, transport, now_ms, &now_iso);
    serde_json::json!({
        "pushed": r.pushed, "pulled": r.pulled, "conflicts": r.conflicts,
        "pending": r.pending, "cursor": r.cursor, "state": r.state.label(),
        "error": r.error,
    })
}

/// 手动「立即同步」：推待发队列 → 拉增量。断网时改动留在队列，恢复后补推。
#[tauri::command]
fn sync_now(paths: State<Mutex<AppPaths>>) -> Result<serde_json::Value, String> {
    let paths = paths.lock().unwrap();
    let store = open_store(&paths)?;
    let transport = sync_client::http::HttpTransport::new();
    let report = run_sync_once(store.conn(), &transport);
    Ok(serde_json::json!({ "status": sync_status_value(store.conn()), "report": report }))
}


// ─────────────────────── 桌面自动更新（v2.1 目标①） ───────────────────────

/// 检查更新：拉 latest.json → 比版本 → **只报结果不落盘**。
/// 端点与公钥在 tauri.conf.json 的 plugins.updater 里配置；清单损坏 / 签名
/// 体系不可用 / 网络失败都如实冒泡给前端展示，绝不静默当「已是最新」。
#[tauri::command]
async fn check_update(app: tauri::AppHandle) -> Result<serde_json::Value, String> {
    let updater = app.updater().map_err(|e| e.to_string())?;
    match updater.check().await {
        Ok(Some(u)) => Ok(serde_json::json!({
            "available": true,
            "version": u.version,
            "notes": u.body,
            // OffsetDateTime 直接 to_string（serde 特性未必然开启，不赌）
            "date": u.date.map(|d| d.to_string()),
        })),
        Ok(None) => Ok(serde_json::json!({ "available": false })),
        Err(e) => Err(e.to_string()),
    }
}

/// 用户确认后执行「下载 → 验签 → 重启安装」。
/// 验签在 tauri-plugin-updater 内部：minisign/Ed25519 公钥不匹配直接报错，
/// 绝不安装未验签的包（与 wb-core::update 同一铁律）。
/// 构建期依赖 TAURI_SIGNING_PRIVATE_KEY 签出 .sig 文件，私钥不入仓库。
#[tauri::command]
async fn install_update(app: tauri::AppHandle) -> Result<(), String> {
    let updater = app.updater().map_err(|e| e.to_string())?;
    let Some(u) = updater.check().await.map_err(|e| e.to_string())? else {
        return Err("没有可安装的更新（清单已变更或已是最新）".into());
    };
    // 两个回调留给进度条用（二期），空闭包即「静默下载」。
    // Windows：安装器接管后本函数会直接退出进程，restart_after_install
    // 默认 true（安装完成后自动重启进新版）；走到下方 restart() 的只有
    // macOS / Linux——tauri-plugin-updater 2.14 download_and_install 文档口径。
    u.download_and_install(|_, _| {}, || {}).await.map_err(|e| e.to_string())?;
    app.restart();
}

// ─────────────────────── F1：手工快速录入 ───────────────────────

/// 快速录入（PRD F1「不定时输入计划和排期」的最小入口）。
/// 日期文本（如「下月3号」）由 wb-core 的规范化管道解析，前端不自己猜日期。
#[tauri::command]
fn add_item(
    paths: State<Mutex<AppPaths>>,
    title: String,
    board: String,
    plan_start_raw: String,
    plan_end_raw: String,
    year: i32,
) -> Result<serde_json::Value, String> {
    let paths = paths.lock().unwrap();
    let store = open_store(&paths)?;
    // 录入人：本机未接入团队时用 "local"，接入后由同步命令写入 member_id
    let who = wb_core::sync_client::state_get(store.conn(), wb_core::sync_client::keys::MEMBER_ID)
        .unwrap_or_else(|| "local".into());
    let req = wb_core::entry::QuickAdd {
        title,
        board,
        plan_start_raw,
        plan_end_raw,
        fiscal_year: year,
        base_date: today().to_iso(),
        updated_by: who,
    };
    let r = wb_core::entry::quick_add(&store, &req).map_err(err_string)?;
    // 负责人默认创建人（PRD §3.3.2「负责人单选、默认为创建人」）。初次指定不写
    // 变更历史，否则每建一条都往提醒区塞一条。失败不影响已落库的项——这只是补一个
    // 默认值，缺了它负责人视角就没有起点可比。
    let creator = wb_core::collab::identity(store.conn()).member_name;
    let _ = wb_core::collab::init_owner_on_create(store.conn(), &r.id, &creator);
    Ok(serde_json::json!({
        "id": r.id,
        "seq_no": r.seq_no,
        "plan_start_date": r.plan_start_date,
        "plan_start_precision": r.plan_start_precision,
        "plan_end_date": r.plan_end_date,
        "plan_end_precision": r.plan_end_precision,
        "needs_confirm": r.needs_confirm,
        "queued": r.queued,
        "owner": creator,
    }))
}

// ──────────────── 追加需求 C：进度 / 动作管理（update_item / log_action） ────────────────

/// 更新进度与状态（追加需求 C③ 的 `update_item` 命令）。
///
/// 参数用扁平形式而非直接收结构体：前端 JS 传的是字符串/数字字面量，
/// 结构体跨 IPC 时多一层序列化约定（camelCase ↔ snake_case）容易传错，
/// 而进度这种数字传错是静默的——宁可参数多几个也不留这个坑。
///
/// `progress_pct` / `status` 传 `null` 表示「本次不改」，与传 `0` 语义不同：
/// 传 0 是把进度真正归零，不能因为 JS 里 `0 || 不改` 的写法混为一谈。
#[tauri::command]
fn update_item(
    paths: State<Mutex<AppPaths>>,
    item_id: String,
    progress_pct: Option<i64>,
    status: Option<String>,
    note: Option<String>,
) -> Result<serde_json::Value, String> {
    let paths = paths.lock().unwrap();
    let store = open_store(&paths)?;
    // 操作人取**本机成员名**：负责人字段按姓名匹配，第一层闸也按姓名判，
    // 传 member_id 会让「负责人本人」判定对不上号（引擎层有归一，命令层不留这个歧义）。
    let who = wb_core::collab::identity(store.conn()).member_name;
    let patch = wb_core::pct::ItemPatch {
        item_id,
        progress_pct,
        status,
        note,
        by: Some(who),
    };
    let out = wb_core::pct::update_item(&store, &patch).map_err(err_string)?;
    serde_json::to_value(&out).map_err(|e| e.to_string())
}

/// 追加一条动作记录（追加需求 C③ 的 `log_action` 命令）。
/// 只写历史、不动数字——「今天跟甲方通了电话」不该把进度改成别的值。
#[tauri::command]
fn log_action(
    paths: State<Mutex<AppPaths>>,
    item_id: String,
    note: String,
) -> Result<serde_json::Value, String> {
    let paths = paths.lock().unwrap();
    let store = open_store(&paths)?;
    let who = wb_core::collab::identity(store.conn()).member_name;
    let a = wb_core::pct::log_action(&store, &item_id, &note, Some(&who)).map_err(err_string)?;
    serde_json::to_value(&a).map_err(|e| e.to_string())
}

/// 某条项目的动作历史（明细页展开用，最新在上）。
#[tauri::command]
fn item_actions(
    paths: State<Mutex<AppPaths>>,
    item_id: String,
) -> Result<serde_json::Value, String> {
    let paths = paths.lock().unwrap();
    let store = open_store(&paths)?;
    let logs = wb_core::pct::list_actions(store.conn(), &item_id).map_err(err_string)?;
    serde_json::to_value(&logs).map_err(|e| e.to_string())
}

// ─────────────────── F5 团队协作面：成员与负责人（v3.2.0） ───────────────────
//
// 边界：**不复活多用户客户端 / 多账号体系**。这一组命令只读写本机库里的
// `member` 表与 `work_item.owner`，不引入登录态、不切换账号。所谓「权限」
// 是单机态口径：同一条库里既有负责人也有成员表，能不能改由库里的事实判定。
//
// 三层口径：命令层判一次（这里）→ 引擎层再判一次（`pct::update_item`）→
// 两层都调 `collab::deny_reason`，不会判出两种结果。

/// 成员列表 + 本机身份。成员表为空时把本机身份补登为管理员（首位即管理员）。
#[tauri::command]
fn member_list(paths: State<Mutex<AppPaths>>) -> Result<serde_json::Value, String> {
    let paths = paths.lock().unwrap();
    let store = open_store(&paths)?;
    let (rows, ident) = wb_core::collab::list_members(&store).map_err(err_string)?;
    // 顺带打一次活跃时间：PRD 的「最近活跃时间」不要求实时在线状态，
    // 打开成员页就是一次活跃，不必再另起心跳。
    wb_core::collab::touch_active(store.conn(), &ident.member_name);
    Ok(serde_json::json!({ "members": rows, "me": ident }))
}

/// 新建成员（仅管理员）。重名直接拒绝——负责人按显示名匹配，重名会让判定失去唯一性。
#[tauri::command]
fn member_create(
    paths: State<Mutex<AppPaths>>,
    display_name: String,
    role: String,
) -> Result<serde_json::Value, String> {
    let paths = paths.lock().unwrap();
    let store = open_store(&paths)?;
    let by = wb_core::collab::identity(store.conn()).member_name;
    let m = wb_core::collab::create_member(&store, &display_name, &role, &by).map_err(err_string)?;
    serde_json::to_value(&m).map_err(|e| e.to_string())
}

/// 改角色（仅管理员）。最后一名启用中的管理员不能被降级，否则库里再没人能管成员。
#[tauri::command]
fn member_set_role(
    paths: State<Mutex<AppPaths>>,
    member_id: String,
    role: String,
) -> Result<serde_json::Value, String> {
    let paths = paths.lock().unwrap();
    let store = open_store(&paths)?;
    let by = wb_core::collab::identity(store.conn()).member_name;
    let m = wb_core::collab::set_member_role(&store, &member_id, &role, &by).map_err(err_string)?;
    serde_json::to_value(&m).map_err(|e| e.to_string())
}

/// 停用 / 恢复成员（仅管理员）。停用不做删除：数据与历史都留着，只是不再允许以该身份写。
#[tauri::command]
fn member_set_status(
    paths: State<Mutex<AppPaths>>,
    member_id: String,
    status: String,
) -> Result<serde_json::Value, String> {
    let paths = paths.lock().unwrap();
    let store = open_store(&paths)?;
    let by = wb_core::collab::identity(store.conn()).member_name;
    let m = wb_core::collab::set_member_status(&store, &member_id, &status, &by).map_err(err_string)?;
    serde_json::to_value(&m).map_err(|e| e.to_string())
}

/// 指定 / 变更负责人（管理员、当前负责人、或尚未分配时任何人）。
///
/// `to_owner` 传空串 = 取消分配。变更写 `owner_change_log`，下次打开时进提醒区。
/// 返回 `{ changed: bool, change: OwnerChange|null }`：没变不算错，但要如实告诉界面。
#[tauri::command]
fn assign_owner(
    paths: State<Mutex<AppPaths>>,
    item_id: String,
    to_owner: Option<String>,
) -> Result<serde_json::Value, String> {
    let paths = paths.lock().unwrap();
    let store = open_store(&paths)?;
    let by = wb_core::collab::identity(store.conn()).member_name;
    let change = wb_core::collab::set_item_owner(&store, &item_id, to_owner.as_deref(), &by)
        .map_err(err_string)?;
    Ok(serde_json::json!({
        "changed": change.is_some(),
        "change": change,
    }))
}

/// 待提醒的负责人变更（只返回与当前身份相关的：你被派了活 / 你的活被转走）。
#[tauri::command]
fn pending_owner_changes(paths: State<Mutex<AppPaths>>) -> Result<serde_json::Value, String> {
    let paths = paths.lock().unwrap();
    let store = open_store(&paths)?;
    let rows = wb_core::collab::pending_owner_changes(store.conn()).map_err(err_string)?;
    serde_json::to_value(&rows).map_err(|e| e.to_string())
}

/// 确认已提醒（下一次打开不再重复提醒）。只标传进来的那几条。
#[tauri::command]
fn ack_owner_changes(
    paths: State<Mutex<AppPaths>>,
    ids: Vec<String>,
) -> Result<serde_json::Value, String> {
    let paths = paths.lock().unwrap();
    let store = open_store(&paths)?;
    wb_core::collab::mark_owner_changes_notified(store.conn(), &ids).map_err(err_string)?;
    Ok(serde_json::json!({ "ok": true, "ack": ids.len() }))
}

// ─────────────────────── B7：晨报送达（落盘 + 补推） ───────────────────────

/// 待送达晨报状态文件（简单 JSON，落在数据目录）。
/// **必须有写入方**：完全退出且托盘未驻留时，靠它下次启动补推（B7 验收②）。
fn delivery_file(data_dir: &PathBuf) -> PathBuf {
    data_dir.join("last_brief.json")
}

fn read_delivery(paths: &AppPaths) -> DeliveryState {
    let text = std::fs::read_to_string(delivery_file(&paths.data_dir)).unwrap_or_default();
    serde_json::from_str(&text).unwrap_or_default()
}

fn write_delivery(paths: &AppPaths, state: &DeliveryState) {
    if let Ok(text) = serde_json::to_string_pretty(state) {
        let _ = std::fs::write(delivery_file(&paths.data_dir), text);
    }
}

/// 生成晨报并投递系统通知；成功返回通知正文，失败返回 Err 说明。
/// v2.1：免打扰窗口从 sync_state 读真实偏好（不再写死 default）；
/// 输出附带播报文本与语音开关，前端据此决定是否朗读（免打扰只通知不播报）。
/// v2.2 FR-6：`version_event` 非空时按「新版本」来源标注（标题+正文末行）；
/// 无论哪种来源，成功送达都把当前包版本写入 last_delivered_version
/// （AC-6-7：正常 9:00 送达也要更新版本记录，次日才不会误判版本事件）。
fn deliver_brief(
    app: &tauri::AppHandle,
    paths: &AppPaths,
    version_event: Option<brief::VersionBootEvent>,
) -> Result<serde_json::Value, String> {
    let store = open_store(paths)?;
    // 业务年度固定取 FISCAL_YEAR（v2.2.1）：此前用 today().year，
    // 到了年度切换点会出现「界面看 2027、晨报取 2026」的错年。
    let year = wb_core::FISCAL_YEAR;
    let items = wb_core::repo::load_items(store.conn(), year).map_err(err_string)?;
    let rs = Ruleset::default();
    let t = today();
    let decision = brief::decide(t, now_minutes(), &read_delivery(paths));
    let b = rules::morning_brief(t, &items, &rs);
    let (title, body) = match version_event.as_ref() {
        Some(ev) => brief::version_boot_notification(&b, ev),
        None => brief::notification_text(&b, &decision),
    };
    let quiet = load_prefs(store.conn());
    let speech = brief::speech_text(&b, &decision);

    let suppressed = brief::notification_suppressed(now_minutes(), quiet);
    if !suppressed {
        app.notification()
            .builder()
            .title(&title)
            .body(&body)
            .show()
            .map_err(|e| format!("系统通知发送失败：{e}"))?;
    }

    // 落盘：无论通知是否被免打扰抑制，晨报本身都已生成并送达（应用内可看），
    // 因此这里写「已送达」。抑制的只是弹窗，不是晨报内容——不静默吞掉。
    // AC-6-5：免打扰下版本事件同样要更新版本记录（不静默丢失）。
    let mut st = read_delivery(paths);
    st.last_delivered_date = Some(decision.brief_date.clone());
    st.last_delivered_version = Some(app.package_info().version.to_string());
    st.last_attempt_at = Some(wb_core::store::now_iso_pub());
    write_delivery(paths, &st);

    Ok(serde_json::json!({
        "brief_date": decision.brief_date,
        "verdict": format!("{:?}", decision.verdict),
        "late_minutes": decision.late_minutes,
        "notification_suppressed": suppressed,
        "speech": speech,
        "title": title,
        "body": body,
        "new_version": version_event.is_some(),
        "version_from": version_event.as_ref().map(|e| e.previous_version.clone()),
        "version_to": version_event.as_ref().map(|e| e.current_version.clone()),
        "counts": {
            "due_today": b.due_today.len(),
            "due_soon": b.due_soon.len(),
            "overdue": b.overdue.len(),
        },
    }))
}

#[tauri::command]
fn get_pending_brief(paths: State<Mutex<AppPaths>>) -> Result<serde_json::Value, String> {
    let paths = paths.lock().unwrap();
    let st = read_delivery(&paths);
    let today_iso = today().to_iso();
    let pending = st.last_delivered_date.as_deref() != Some(today_iso.as_str())
        && now_minutes() >= brief::BRIEF_HOUR * 60 + brief::BRIEF_MINUTE;
    Ok(serde_json::json!({
        "last_delivered_date": st.last_delivered_date,
        "last_delivered_version": st.last_delivered_version,
        "last_attempt_at": st.last_attempt_at,
        "pending": pending,
        "today": today_iso,
    }))
}

/// 手动触发一次晨报（托盘「查看晨报」/ 应用内按钮），补发时会标明。
#[tauri::command]
fn trigger_brief(app: tauri::AppHandle, paths: State<Mutex<AppPaths>>) -> Result<serde_json::Value, String> {
    let paths = paths.lock().unwrap();
    // 手动触发不标「新版本」：来源标注只属于版本事件（FR-6）
    deliver_brief(&app, &paths, None)
}

/// 兼容旧前端：清除待送达标记（清空已送达日期，下次判为待补发）。
#[tauri::command]
fn mark_brief_delivered(paths: State<Mutex<AppPaths>>) -> Result<(), String> {
    let paths = paths.lock().unwrap();
    let _ = std::fs::remove_file(delivery_file(&paths.data_dir));
    Ok(())
}

/// 9:00 调度线程：每 30 秒醒一次判一次，断网无关、与系统时区同步。
///
/// 用轮询而不是「睡到明天 9:00」：后者遇到休眠/唤醒、系统时钟跳变会整段错过。
/// 30 秒粒度对晨报足够，且代价可忽略（一次内存判定，不碰数据库）。
fn spawn_brief_scheduler(app: tauri::AppHandle) {
    std::thread::spawn(move || loop {
        std::thread::sleep(Duration::from_secs(30));
        let Some(paths) = app.try_state::<Mutex<AppPaths>>() else { continue };
        let decision = {
            let p = paths.lock().unwrap();
            brief::decide(today(), now_minutes(), &read_delivery(&p))
        };
        if !decision.verdict.is_due() {
            continue;
        }
        // 到期才开库、生成、通知；失败不 panic，只把原因发给前端提示
        let result = {
            let p = paths.lock().unwrap();
            deliver_brief(&app, &p, None)
        };
        if let Err(e) = &result {
            let _ = app.emit("brief-error", e.clone());
        } else if let Ok(v) = &result {
            let _ = app.emit("brief-delivered", v.clone());
        }
    });
}

// ─────────────────────────────────────────────────────────────────────
// v2.2 FR-2 项目情报 + FR-1 极飞网点（应用内直连，用户 2026-10-28 拍板）
// 红线：抓取失败只影响本视图，不阻断启动/看板/晨报/导入（AC-2-11）
// ─────────────────────────────────────────────────────────────────────

fn intel_state_file(data_dir: &PathBuf) -> PathBuf {
    data_dir.join("intel_state.json")
}

/// 诊断记录用的本地时刻（`YYYY-MM-DDTHH:MM` 本地）。用户拿它对照自己机器上的时间，
/// 所以必须是本机时区，不能是 UTC——UTC 记录会让用户对不上"到底几点崩的"。
fn now_stamp_local() -> String {
    let (d, m) = now_local();
    format!("{}T{}", d.to_iso(), wb_core::local_time::fmt_hhmm(m))
}

fn load_intel(paths: &AppPaths) -> intel::IntelState {
    intel::IntelState::load(&paths.data_dir)
}

/// 采集进行中标志：整轮绝对上限 77.6 分钟（4654s）。算式逐项来自源码常量：
/// 单请求最坏 140s（40s×3 次 + 5s/15s 退避）；Epoint 单站最坏 300s（前 4 页各拖满
/// 40s 后成功、第 5 页失败且重试用尽，失败即中止）；真正发请求的 Epoint 站是 14 个
/// （芜湖/池州 epoint=None，解析期即返回 ParseError，不发请求），14×300s；
/// 省级主站双请求 2×140s；徽采云 140s；18 站间 17×2s。
/// 没有它时，用户连点「立即刷新」（或调度线程正好同时到点）会并发跑两轮，
/// 两轮各自 `IntelState::load` 一份旧快照、各自 `save`，后落盘的一方把前一轮
/// 刚写好的站点状态与诊断整段覆盖掉——表现为「刷新了一次，另一批结果不见了」。
/// 用 `swap` 做占用判定，天然互斥，不需要给整个采集加锁。
static INTEL_RUNNING: AtomicBool = AtomicBool::new(false);

/// 占用一次采集名额：成功返回 `true`，已有采集在跑返回 `false`。
/// 抽成独立函数是为了让这条语义能被单测直接验证（`spawn_intel_run` 需要
/// `AppHandle`，单测里造不出来，只能靠肉眼看代码——那不算验证）。
fn try_begin_intel_run() -> bool {
    !INTEL_RUNNING.swap(true, Ordering::SeqCst)
}

/// 释放采集名额。由线程内的 guard 在退出时调用，panic 展开也会触发。
fn end_intel_run() {
    INTEL_RUNNING.store(false, Ordering::SeqCst);
}

/// 日频抓取一轮（调度线程与手动「立即刷新」共用）。
/// 抓取在独立线程执行不阻塞 UI；同站间隔 ≥2s（预研口径）；失败只标记站点。
fn spawn_intel_run(app: tauri::AppHandle, manual: bool) {
    // 已有采集在跑：直接返回，让调用方（前端）保持原有进度显示。
    if !try_begin_intel_run() {
        let _ = app.emit("intel-busy", serde_json::json!({ "manual": manual }));
        return;
    }
    std::thread::spawn(move || {
        // 无论正常结束还是 panic 退出，都必须把标志放回去，否则一次异常
        // 会让后续所有采集被永久挡住。用一个 guard 结构承载。
        struct RunGuard;
        impl Drop for RunGuard {
            fn drop(&mut self) {
                end_intel_run();
            }
        }
        let _guard = RunGuard;
        let (data_dir, today_iso, start_iso, end_iso) = {
            let Some(paths) = app.try_state::<Mutex<AppPaths>>() else { return };
            let p = paths.lock().unwrap();
            let t = today();
            (
                p.data_dir.clone(),
                t.to_iso(),
                t.add_days(-90).to_iso(),
                t.to_iso(),
            )
        };
        let fetcher = intel::UreqFetcher;
        let mut state = intel::IntelState::load(&data_dir);
        // 改判一（AC-3）：线索落库。壳层采集此前只写 intel_state.json，线索（lead 表）
        // 永远是空的——「线索转待办」入口就成了摆设。这里与 wbd intel 同一条引擎通路：
        // 每源抓取成功后 persist_fetch 进 Store（三层去重 + 拉链）。
        // 打不开库不阻断采集（与诊断写入同口径：降级为「本轮只更新 JSON 态」）。
        let lead_store = Store::open(&data_dir).ok();
        let mut results: Vec<intel::SiteResult> = Vec::new();
        let mut new_total = 0usize;
        let mut failed = 0usize;
        let mut first = true;
        // 逐站进度：整轮绝对上限 77.6 分钟（4654s），前端需要知道「跑到第几站」而不是干等一行字。
        // 首发闸门（v2.2.6）：只跑实测通过的源，与 run_daily_at / wbd intel 同一判定——
        // 壳层此前漏了这一层过滤，9 个未实测源会被静默尝试。
        let total_sites = intel::SITE_REGISTRY
            .iter()
            .filter(|s| s.tier.is_scheduled() && intel::is_enabled_for_release(s.id))
            .count();
        let mut done_sites = 0usize;
        for spec in intel::SITE_REGISTRY
            .iter()
            .filter(|s| s.tier.is_scheduled() && intel::is_enabled_for_release(s.id))
        {
            if !first {
                // 同站请求间隔 ≥2s：跨站也保持，避免整体突发
                std::thread::sleep(Duration::from_secs(2));
            }
            first = false;
            let r = intel::fetch_site_with_retry(
                spec, &fetcher, &intel::RealSleeper, intel::KEYWORD, &start_iso, &end_iso, &today_iso,
            );
            let mut fail: Option<intel::FetchFail> = None;
            let sr = match r {
                Ok(items) => {
                    let (new_n, _) = intel::merge_items(&mut state, items.clone(), &today_iso);
                    new_total += new_n;
                    // 改判一（AC-3）：同批条目进线索库（三层去重 + 拉链，与 wbd intel 同通路）。
                    // 落库失败不阻断采集——本轮该源线索维持旧态，采集结果照常进 IntelState。
                    if let Some(store) = lead_store.as_ref() {
                        let _ = intel_store::persist_fetch(store, &items, None);
                    }
                    let n = state.items.values().filter(|i| i.site_id == spec.id).count();
                    intel::SiteResult {
                        site_id: spec.id.to_string(),
                        status: intel::FetchStatus::Ok,
                        detail: String::new(),
                        items: n,
                        new_items: new_n,
                    }
                }
                Err(f) => {
                    failed += 1;
                    let sr = intel::SiteResult {
                        site_id: spec.id.to_string(),
                        status: f.status.clone(),
                        detail: f.detail.clone(),
                        items: 0,
                        new_items: 0,
                    };
                    fail = Some(f);
                    sr
                }
            };
            // 站点状态行（含失败原因，界面可见 AC-2-5）
            let row = state.sites.entry(spec.id.to_string()).or_default();
            row.last_attempt = Some(today_iso.clone());
            match sr.status {
                intel::FetchStatus::Ok => {
                    row.last_success = Some(today_iso.clone());
                    row.last_status = Some(intel::FetchStatus::Ok);
                    row.last_detail = String::new();
                    row.fail_streak = 0;
                }
                ref st => {
                    row.last_status = Some(st.clone());
                    row.last_detail = sr.detail.clone();
                    row.fail_streak += 1;
                }
            }
            // 诊断滚动落库（每源最近 3 条）。纯内存操作，落盘在 IntelState::save 里,
            // 写失败不会中断采集（需求：诊断写入失败不得中断采集流程）。
            row.push_diag(intel::DiagEntry {
                ts: now_stamp_local(),
                url: fail.as_ref().map(|f| f.url.clone()).unwrap_or_default(),
                http_status: fail.as_ref().and_then(|f| f.http_status),
                kind: sr.status.label().to_string(),
                detail: if matches!(sr.status, intel::FetchStatus::Ok) {
                    if sr.items == 0 { "请求成功，结果 0 条".into() } else { format!("请求成功，本站在库 {} 条", sr.items) }
                } else {
                    sr.detail.clone()
                },
                body_head: fail.as_ref().map(|f| f.body_head.clone()).unwrap_or_default(),
                attempts: fail.as_ref().map(|f| f.attempts).unwrap_or(1),
            });
            results.push(sr);
            // 进度事件只带计数与站名，不带业务数据——被丢掉的监听不会丢任何结果。
            done_sites += 1;
            let _ = app.emit(
                "intel-progress",
                serde_json::json!({
                    "done": done_sites, "total": total_sites,
                    "site_name": spec.name, "manual": manual,
                    "failed": failed, "new_total": new_total,
                }),
            );
        }
        state.last_run_date = Some(today_iso.clone());
        let (connected, planned) = intel::coverage_counts();
        let summary = format!(
            "今日新增相关项目 {} 条，{} 个源采集失败；已接入 {} / 计划覆盖 {} 源",
            new_total, failed, connected, planned
        );
        state.save(&data_dir);
        let _ = app.emit(
            "intel-updated",
            serde_json::json!({ "summary": summary, "manual": manual, "new_total": new_total, "failed": failed }),
        );
    });
}

/// FR-1 极飞网点周比对（周频：距上次 ≥7 天才自动跑；手动可强制）
fn spawn_dealer_run(app: tauri::AppHandle, manual: bool) {
    std::thread::spawn(move || {
        let Some(paths) = app.try_state::<Mutex<AppPaths>>() else { return };
        let data_dir = {
            let p = paths.lock().unwrap();
            p.data_dir.clone()
        };
        let fetcher = intel::UreqFetcher;
        let mut state = intel::IntelState::load(&data_dir);
        match intel::fetch_dealers(&fetcher) {
            Ok(nodes) => {
                // 先对旧快照算 diff，再把快照更新为本次全量
                let diff = intel::diff_dealers(&state.dealer_snapshot, &nodes);
                state.dealer_snapshot = nodes;
                state.last_dealer_run_date = Some(today().to_iso());
                state.dealer_last_diff = Some(diff);
                state.save(&data_dir);
                let _ = app.emit("dealer-updated", serde_json::json!({ "manual": manual, "ok": true }));
            }
            Err(f) => {
                // 失败不改快照，只发事件让界面可见（AC-1-4 口径）
                let _ = app.emit(
                    "dealer-updated",
                    serde_json::json!({
                        "manual": manual, "ok": false,
                        "status": format!("{:?}", f.status), "detail": f.detail,
                        "url": f.url, "http_status": f.http_status, "attempts": f.attempts,
                    }),
                );
            }
        }
    });
}

/// 情报视图数据：条目按发布日期倒序（无关级不进列表），站点状态 + 覆盖计数
#[tauri::command]
fn intel_get(paths: State<Mutex<AppPaths>>) -> Result<serde_json::Value, String> {
    let paths = paths.lock().unwrap();
    let state = load_intel(&paths);
    let mut items: Vec<&intel::IntelItem> = state
        .items
        .values()
        .filter(|i| i.relevance != intel::Relevance::Excluded)
        .collect();
    items.sort_by(|a, b| b.date.cmp(&a.date).then(b.first_seen.cmp(&a.first_seen)));
    let (connected, planned) = intel::coverage_counts();
    Ok(serde_json::json!({
        "items": items,
        "sites": state.sites,
        "registry": intel::SITE_REGISTRY.iter().map(|s| serde_json::json!({
            "id": s.id, "name": s.name, "city": s.city,
            "tier": format!("{:?}", s.tier), "scheduled": s.tier.is_scheduled(),
            "note": s.note,
        })).collect::<Vec<_>>(),
        "coverage": { "connected": connected, "planned": planned },
        "last_run_date": state.last_run_date,
        "total_stored": state.items.len(),
    }))
}

/// 手动「立即刷新」（AC-2-5：失败源可一键重试）
#[tauri::command]
fn intel_refresh(app: tauri::AppHandle) -> Result<(), String> {
    spawn_intel_run(app, true);
    Ok(())
}

/// 人工改判（AC-2-7：不被自动规则覆盖）
#[tauri::command]
fn intel_set_verdict(
    paths: State<Mutex<AppPaths>>,
    key: String,
    verdict: String,
) -> Result<(), String> {
    let paths = paths.lock().unwrap();
    let mut state = load_intel(&paths);
    let v = match verdict.as_str() {
        "core" => intel::Relevance::Core,
        "suspect" => intel::Relevance::Suspect,
        "excluded" => intel::Relevance::Excluded,
        _ => return Err(format!("非法判定值: {}", verdict)),
    };
    if !intel::set_verdict(&mut state, &key, v) {
        return Err("条目不存在".to_string());
    }
    state.save(&paths.data_dir);
    Ok(())
}

// ─────────────────── 改判一（AC-3）：补贴导出 / 线索转待办 的壳层接线 ───────────────────
// v2.2.6 最小界面入口：引擎层（subsidy_export / intel_store / lead_todo）此前只有 wbd CLI
// 通路；这里补上桌面壳命令，前端情报页两个入口直接调引擎，不再经过 CLI。

/// 前端传来的单条筛选条件（补贴导出）。op: "eq"（精确）| "contains"（包含）。
#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct SubsidyFilterArg {
    field: String,
    op: String,
    value: String,
}

/// 补贴明细一键导出：**按当前筛选条件全量导出，不是只导当前页**（本功能的定义）。
/// 前端把视图当前筛选序列化成 filters 传入；引擎对整个 CSV 应用筛选后写出 xlsx / csv。
// ─────────── v3.4.0 项目跟踪（W2 数据层命令） ───────────
//
// 壳层只做参数整形与错误转字符串，全部逻辑在 `wb_core::project`，
// 与 wbd CLI / 测试走同一批函数（避免三处各写一套校验）。

#[tauri::command]
fn project_list(paths: State<Mutex<AppPaths>>, year: i32, include_archived: Option<bool>) -> Result<serde_json::Value, String> {
    let paths = paths.lock().unwrap();
    let store = open_store(&paths)?;
    let rows = wb_core::project::list_projects(&store, year, include_archived.unwrap_or(false))
        .map_err(err_string)?;
    Ok(serde_json::json!({ "projects": rows }))
}

#[tauri::command]
fn project_get(paths: State<Mutex<AppPaths>>, id: String) -> Result<serde_json::Value, String> {
    let paths = paths.lock().unwrap();
    let store = open_store(&paths)?;
    let p = wb_core::project::get_project(&store, id.trim()).map_err(err_string)?;
    Ok(serde_json::json!({ "project": p }))
}

#[tauri::command]
fn project_create(
    paths: State<Mutex<AppPaths>>,
    draft: wb_core::project::ProjectDraft,
    year: i32,
) -> Result<serde_json::Value, String> {
    let paths = paths.lock().unwrap();
    let store = open_store(&paths)?;
    let out = wb_core::project::create_project(&store, &draft, year)
        .map_err(|e| e.reason())?;
    Ok(serde_json::json!({ "project": out.project }))
}

#[tauri::command]
fn project_update(
    paths: State<Mutex<AppPaths>>,
    id: String,
    draft: wb_core::project::ProjectDraft,
    actor: Option<String>,
) -> Result<serde_json::Value, String> {
    let paths = paths.lock().unwrap();
    let store = open_store(&paths)?;
    let p = wb_core::project::update_project(&store, id.trim(), &draft, actor.as_deref().unwrap_or("ui"))
        .map_err(|e| e.reason())?;
    Ok(serde_json::json!({ "project": p }))
}

#[tauri::command]
fn project_delete(paths: State<Mutex<AppPaths>>, id: String, actor: Option<String>) -> Result<serde_json::Value, String> {
    let paths = paths.lock().unwrap();
    let store = open_store(&paths)?;
    let changed = wb_core::project::soft_delete_project(&store, id.trim(), actor.as_deref().unwrap_or("ui"))
        .map_err(err_string)?;
    Ok(serde_json::json!({ "deleted": changed }))
}

// ── 过程记录 ──

#[tauri::command]
fn project_notes(paths: State<Mutex<AppPaths>>, project_id: String) -> Result<serde_json::Value, String> {
    let paths = paths.lock().unwrap();
    let store = open_store(&paths)?;
    let notes = wb_core::project::list_notes(&store, project_id.trim()).map_err(err_string)?;
    Ok(serde_json::json!({ "notes": notes }))
}

#[tauri::command]
fn project_note_create(
    paths: State<Mutex<AppPaths>>,
    draft: wb_core::project::NoteDraft,
    actor: Option<String>,
) -> Result<serde_json::Value, String> {
    let paths = paths.lock().unwrap();
    let store = open_store(&paths)?;
    let note = wb_core::project::create_note(&store, &draft, actor.as_deref().unwrap_or("ui"))
        .map_err(|e| e.reason())?;
    Ok(serde_json::json!({ "note": note }))
}

#[tauri::command]
fn project_note_delete(paths: State<Mutex<AppPaths>>, id: String, actor: Option<String>) -> Result<serde_json::Value, String> {
    let paths = paths.lock().unwrap();
    let store = open_store(&paths)?;
    let changed = wb_core::project::soft_delete_note(&store, id.trim(), actor.as_deref().unwrap_or("ui"))
        .map_err(err_string)?;
    Ok(serde_json::json!({ "deleted": changed }))
}

// ── 附件（内容寻址 + sha256 跨记录去重） ──

#[tauri::command]
fn project_attachments(paths: State<Mutex<AppPaths>>, note_id: String) -> Result<serde_json::Value, String> {
    let paths = paths.lock().unwrap();
    let store = open_store(&paths)?;
    let list = wb_core::project::list_attachments(&store, note_id.trim()).map_err(err_string)?;
    Ok(serde_json::json!({ "attachments": list }))
}

#[tauri::command]
fn project_attach(
    paths: State<Mutex<AppPaths>>,
    note_id: String,
    source_path: String,
    file_name: Option<String>,
    mime: Option<String>,
    sort_no: Option<i64>,
    actor: Option<String>,
) -> Result<serde_json::Value, String> {
    let paths = paths.lock().unwrap();
    let store = open_store(&paths)?;
    let src = std::path::Path::new(source_path.trim());
    // 未指定展示名时取源文件文件名（保留扩展名，卡片要按扩展名选图标）
    let name = file_name
        .filter(|s| !s.trim().is_empty())
        .or_else(|| src.file_name().map(|s| s.to_string_lossy().to_string()))
        .ok_or_else(|| "无法确定附件文件名".to_string())?;
    let out = wb_core::project::attach_file(
        &store,
        note_id.trim(),
        src,
        &name,
        mime.as_deref().filter(|m| !m.trim().is_empty()),
        sort_no.unwrap_or(0),
        actor.as_deref().unwrap_or("ui"),
    )
    .map_err(err_string)?;
    Ok(serde_json::json!({ "attachment": out.attachment, "write": out.write }))
}

#[tauri::command]
fn project_reclaim(paths: State<Mutex<AppPaths>>, dry_run: Option<bool>) -> Result<serde_json::Value, String> {
    let paths = paths.lock().unwrap();
    let store = open_store(&paths)?;
    let ledger = wb_core::project::reclaim_orphan_attachments(&store, dry_run.unwrap_or(true))
        .map_err(err_string)?;
    Ok(serde_json::json!({ "ledger": ledger }))
}

// ─────────── v3.4.0 待办版块（W5：generate_todo → work_item 落库的读命令） ───────────
//
// 写路径在 `wb_core::intel_store::generate_todo / backfill_todos`（同库同表），
// 闭环复用既有 `update_item`；壳层只做读取与参数整形。

/// 待办版块列表（§3.2）：work_item 全量（未归档未删），LEFT JOIN project 供穿透。
/// `project_alive = false` 表示待办挂着的 project 已被软删——前端显示「原项目已不存在」。
#[tauri::command]
fn todo_list(paths: State<Mutex<AppPaths>>, year: i32) -> Result<serde_json::Value, String> {
    let paths = paths.lock().unwrap();
    let store = open_store(&paths)?;
    let conn = store.conn();
    let mut stmt = conn
        .prepare(
            "SELECT w.id, w.title, w.priority, w.status, w.planned_end_date,
                    w.next_action, w.owner, w.project_id, w.ext_json,
                    w.created_at, w.updated_at,
                    p.name, p.stage,
                    CASE WHEN p.id IS NULL THEN 0 ELSE 1 END
             FROM work_item w
             LEFT JOIN project p
               ON p.id = w.project_id AND p.deleted_at IS NULL
             WHERE w.fiscal_year = ?1 AND w.archived_at IS NULL AND w.deleted_at IS NULL
             ORDER BY (w.planned_end_date IS NULL), w.planned_end_date, w.seq_no",
        )
        .map_err(err_string)?;
    let rows = stmt
        .query_map(rusqlite::params![year], |r| {
            let ext_json: String = r.get(8)?;
            Ok(serde_json::json!({
                "id": r.get::<_, String>(0)?,
                "title": r.get::<_, String>(1)?,
                "priority": r.get::<_, Option<String>>(2)?,
                "status": r.get::<_, Option<String>>(3)?,
                "dueDate": r.get::<_, Option<String>>(4)?,
                "nextAction": r.get::<_, Option<String>>(5)?,
                "owner": r.get::<_, Option<String>>(6)?,
                "projectId": r.get::<_, String>(7)?,
                "ext": serde_json::from_str::<serde_json::Value>(&ext_json).unwrap_or_default(),
                "createdAt": r.get::<_, Option<String>>(9)?,
                "updatedAt": r.get::<_, Option<String>>(10)?,
                "projectName": r.get::<_, Option<String>>(11)?,
                "projectStage": r.get::<_, Option<String>>(12)?,
                "projectAlive": r.get::<_, i64>(13)? == 1,
            }))
        })
        .map_err(err_string)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| e.to_string())?;
    Ok(serde_json::json!({ "todos": rows }))
}

/// 新建待办（§3.2 标题行「新建待办」/ 空态「手动建档」后的自动待办，§5.4）。
/// 人工新建不写 `v34_source` —— 前端据此不显示来源徽标（§3.2 口径）。
#[tauri::command]
fn todo_create(
    paths: State<Mutex<AppPaths>>,
    title: String,
    due_date: Option<String>,
    project_id: Option<String>,
    owner: Option<String>,
) -> Result<serde_json::Value, String> {
    let paths = paths.lock().unwrap();
    let store = open_store(&paths)?;
    let conn = store.conn();
    let title = title.trim().to_string();
    if title.is_empty() {
        return Err("待办标题不能为空".into());
    }
    let ts = wb_core::store::now_iso_pub();
    let fiscal_year: i64 = due_date
        .as_deref()
        .and_then(|d| d.get(0..4))
        .and_then(|y| y.parse::<i64>().ok())
        .or_else(|| ts.get(0..4).and_then(|y| y.parse::<i64>().ok()))
        .unwrap_or(0);
    let id = format!("todo:{}", &ts.replace(['-', ':', '.', 'T'], "")[..14]);
    let seq_no: i64 = conn
        .query_row(
            "SELECT COALESCE(MAX(seq_no), 0) + 1 FROM work_item WHERE fiscal_year = ?1",
            rusqlite::params![fiscal_year],
            |r| r.get(0),
        )
        .map_err(err_string)?;
    conn.execute(
        "INSERT INTO work_item
           (id, seq_no, board_code, title,
            planned_start_date, planned_start_precision,
            planned_end_date, planned_end_precision,
            progress_pct, status, next_action, owner, project_id,
            fiscal_year, ext_json, created_at, updated_at)
         VALUES (?1, ?2, 'prj', ?3,
                 NULL, 'unknown',
                 ?4, 'day',
                 0, '待启动', NULL, ?5, ?6,
                 ?7, '{}', ?8, ?8)",
        rusqlite::params![id, seq_no, title, due_date, owner,
                          project_id.unwrap_or_default(), fiscal_year, ts],
    )
    .map_err(err_string)?;
    Ok(serde_json::json!({ "id": id }))
}

/// 历史待办回填（W5，架构 §4.1：按 todo_json 重放，幂等可重跑）。
#[tauri::command]
fn todo_backfill(paths: State<Mutex<AppPaths>>) -> Result<serde_json::Value, String> {
    let paths = paths.lock().unwrap();
    let store = open_store(&paths)?;
    let report = wb_core::intel_store::backfill_todos(&store).map_err(err_string)?;
    serde_json::to_value(&report).map_err(|e| e.to_string())
}

/// v3.4.0 · W3：数据目录绝对路径。附件缩略/预览走 asset 协议时必须用绝对路径拼接
/// （`storage_path` 是相对数据目录的路径，架构 §3.2）；前端不从 path 插件取，
/// 与 Store 根目录单一来源同源，避免两处目录不一致。
#[tauri::command]
fn data_dir_path(paths: State<Mutex<AppPaths>>) -> String {
    paths.lock().unwrap().data_dir.to_string_lossy().into_owned()
}

// ── 存量 work_item 一次性迁移向导（预览 → 确认执行） ──

#[tauri::command]
fn project_migration_preview(paths: State<Mutex<AppPaths>>) -> Result<serde_json::Value, String> {
    let paths = paths.lock().unwrap();
    let store = open_store(&paths)?;
    let plan = wb_core::project::preview_work_item_migration(&store).map_err(err_string)?;
    Ok(serde_json::json!({ "plan": plan }))
}

#[tauri::command]
fn project_migration_run(
    paths: State<Mutex<AppPaths>>,
    plan: wb_core::project::MigrationPlan,
    year: i32,
    actor: Option<String>,
) -> Result<serde_json::Value, String> {
    let paths = paths.lock().unwrap();
    let store = open_store(&paths)?;
    let res = wb_core::project::run_work_item_migration(&store, &plan, year, actor.as_deref().unwrap_or("ui"))
        .map_err(err_string)?;
    Ok(serde_json::json!({ "result": res }))
}

#[tauri::command]
fn subsidy_export(
    from: String,
    out: String,
    filters: Vec<SubsidyFilterArg>,
    sheet: Option<String>,
) -> Result<serde_json::Value, String> {
    use wb_core::subsidy_export as se;
    let parsed: Vec<se::ExportFilter> = filters
        .into_iter()
        .map(|f| match f.op.as_str() {
            "eq" => Ok(se::ExportFilter::eq(&f.field, &f.value)),
            "contains" => Ok(se::ExportFilter::contains(&f.field, &f.value)),
            other => Err(format!("非法筛选操作符：{other}（仅支持 eq / contains）")),
        })
        .collect::<Result<Vec<_>, String>>()?;
    let (headers, rows) =
        se::read_csv(std::path::Path::new(&from)).map_err(|e| format!("读数据失败：{e}"))?;
    let total = rows.len();
    let filtered = se::apply_filters(&headers, &rows, &parsed)?;
    let filtered_rows: Vec<Vec<String>> = filtered.into_iter().cloned().collect();
    let out_path = std::path::Path::new(&out);
    let is_csv = out_path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.eq_ignore_ascii_case("csv"))
        .unwrap_or(false);
    let n = if is_csv {
        se::export_csv(out_path, &headers, &filtered_rows)
    } else {
        se::export_xlsx(out_path, &headers, &filtered_rows, sheet.as_deref().unwrap_or("补贴明细"))
    }
    .map_err(|e| format!("写出失败：{e}"))?;
    Ok(serde_json::json!({
        "out": out,
        "format": if is_csv { "csv" } else { "xlsx" },
        "source_rows_total": total,
        "filters_applied": parsed.len(),
        "rows_exported": n,
        "note": "按筛选条件全量导出（非当前页）",
    }))
}

/// 线索清单（改判一 AC-3 入口数据）。采集落库后按三层去重产出，状态机：
/// 待确认 → 已确认 → 已转待办；已忽略为终态。
#[tauri::command]
fn intel_leads_get(paths: State<Mutex<AppPaths>>) -> Result<serde_json::Value, String> {
    let paths = paths.lock().unwrap();
    let store = open_store(&paths)?;
    let mut leads = intel_store::read_leads(&store, None).map_err(err_string)?;
    // 与情报条目同序：发布日期倒序，再按首次入库时间倒序
    leads.sort_by(|a, b| b.date.cmp(&a.date).then(b.first_seen.cmp(&a.first_seen)));
    Ok(serde_json::json!({ "leads": leads }))
}

/// 把一张 payload 聚合卡折成**界面兼容**的 JSON 对象：载荷字段全保留，
/// 另注入界面层已在读的别名（`project_key` / `org` / `last_date` / `manual` 等）。
///
/// 为什么在这里注入而不是让界面自己适配：界面层（`intel-projects.js` 的
/// `kcard` / `projectView` / `headHtml`）已按 `p.nodes` + 别名字段渲染整条进程轴，
/// 而且节点里**五段 `stage` 与六段 `segment` 同时带**（见 `wb_core::intel_payload::CardNode`），
/// 界面 `stage6Of` 走路径 A 即可拿到无歧义的六段码。这里只补别名，不改渲染主干。
fn card_with_ui_aliases(mut v: serde_json::Value) -> serde_json::Value {
    let Some(obj) = v.as_object_mut() else { return v; };
    let agg = obj.get("agg_key").cloned().unwrap_or_default();
    // 转待办把 `project_key` 直接当线索键用（项目键与线索键同口径 code:/fp:/body:），
    // 因此别名取第一个引擎键；无引擎键（手动建档）回退聚合键。
    let engine_key = obj
        .get("engine_keys")
        .and_then(|e| e.as_array())
        .and_then(|a| a.first())
        .cloned()
        .unwrap_or_else(|| agg.clone());
    let nodes_len = obj
        .get("nodes")
        .and_then(|n| n.as_array())
        .map(|a| a.len())
        .unwrap_or(0);
    let first_date = obj
        .get("nodes")
        .and_then(|n| n.as_array())
        .and_then(|a| a.first())
        .and_then(|n| n.get("date"))
        .cloned()
        .unwrap_or_default();
    let is_manual = obj.get("source_kind").and_then(|s| s.as_str()) == Some("manual");
    // 跳段展示：当前段之前、没有公告的实段（headHtml 的「跳段」提示用）
    let cur_idx = obj.get("current_stage_index").and_then(|i| i.as_u64());
    let skipped: Vec<String> = obj
        .get("stages")
        .and_then(|s| s.as_array())
        .map(|arr| {
            arr.iter()
                .filter(|b| {
                    let present = b.get("present").and_then(|p| p.as_bool()).unwrap_or(false);
                    let idx = b.get("display_index").and_then(|i| i.as_u64());
                    let adv = b.get("advances").and_then(|a| a.as_bool()).unwrap_or(true);
                    !present && adv && idx.map(|i| Some(i) <= cur_idx).unwrap_or(false)
                })
                .filter_map(|b| b.get("code").and_then(|c| c.as_str()).map(String::from))
                .collect()
        })
        .unwrap_or_default();
    obj.insert("project_key".into(), engine_key);
    obj.insert("org".into(), obj.get("buyer").cloned().unwrap_or_default());
    obj.insert("last_date".into(), obj.get("latest_announcement_at").cloned().unwrap_or_default());
    obj.insert("first_date".into(), first_date);
    obj.insert("node_count".into(), serde_json::json!(nodes_len));
    obj.insert("manual".into(), serde_json::json!(is_manual));
    obj.insert("skipped_stages".into(), serde_json::json!(skipped.join("、")));
    // obj 是 as_object_mut 借来的 &mut Map，返回值需要 Value
    serde_json::Value::Object(obj.clone())
}

/// 项目聚合列表（v3.2.0 接线）：走六段 payload 适配层
/// （`wb_core::intel_payload::project_list_payload`，与 `wbd project payload` 同一份实现），
/// 每张卡**同时带六段 `current_stage`/`stages` 与五段节点 `nodes[].stage`**，
/// 界面按需取用；旧五段消费者读 `nodes[].stage` 仍是落库口径。
///
/// `stage` 传空串表示不过滤；非法值报错（不静默当「不过滤」）。
/// 注意过滤口径已从旧五段（intent|review|…）切到六段（intent|tender|…），
/// 与 `wbd project payload --stage` 完全一致。
#[tauri::command]
fn intel_projects_get(
    paths: State<Mutex<AppPaths>>,
    stage: Option<String>,
    limit: Option<usize>,
) -> Result<serde_json::Value, String> {
    let paths = paths.lock().unwrap();
    let store = open_store(&paths)?;
    let payload =
        intel_payload::project_list_payload(&store, stage.as_deref(), limit.unwrap_or(0))
            .map_err(err_string)?;
    let mut v = serde_json::to_value(&payload).map_err(|e| e.to_string())?;
    if let Some(arr) = v.get_mut("projects").and_then(|p| p.as_array_mut()) {
        for card in arr.iter_mut() {
            *card = card_with_ui_aliases(card.clone());
        }
    }
    Ok(v)
}

/// 单项目明细（v3.2.0 接线）：走 `wb_core::intel_payload::project_detail_payload`
/// （与 `wbd project detail` 同一份实现）。入参既可以是聚合键 `k:...`，也可以是
/// 引擎项目键 `code:`/`fp:`/`body:`（payload 层内部兼容两种）。
#[tauri::command]
fn intel_project_get(
    paths: State<Mutex<AppPaths>>,
    key: String,
) -> Result<serde_json::Value, String> {
    let paths = paths.lock().unwrap();
    let store = open_store(&paths)?;
    match intel_payload::project_detail_payload(&store, key.trim()).map_err(err_string)? {
        Some(p) => {
            let mut v = serde_json::to_value(&p).map_err(|e| e.to_string())?;
            if let Some(obj) = v.as_object_mut() {
                // 明细也注入同一组界面别名，保持列表/明细同构
                let mut aliased = card_with_ui_aliases(serde_json::Value::Object(obj.clone()));
                if let Some(o2) = aliased.as_object_mut() {
                    for (k2, v2) in o2.iter() {
                        obj.insert(k2.clone(), v2.clone());
                    }
                }
            }
            Ok(serde_json::json!({ "project": v }))
        }
        None => Ok(serde_json::json!({ "project": null })),
    }
}

// ─────────────── 手动建档六命令（v3.2.0 数据接线轨 A-7） ───────────────
// 全部只做「壳」：参数校验给可读中文错误，业务一律调 `wb_core::intel_manual`
// （与 `wbd manual` CLI 同一份实现，桌面/命令行不各写一套）。

/// 公告侧聚合卡 → 查重/疑似关联用的最小候选信息（引擎键 + 名称 + 采购人 + 编号 + 公告数）。
fn announcement_candidates(store: &Store) -> Result<Vec<intel_manual::AnnouncementCandidate>, StoreError> {
    let payload = intel_payload::project_list_payload(store, None, 0)?;
    Ok(payload
        .projects
        .iter()
        .map(|c| intel_manual::AnnouncementCandidate {
            // 引擎键才是合并目标（merged_into 落的是引擎项目键）
            project_key: c
                .engine_keys
                .first()
                .cloned()
                .unwrap_or_else(|| c.agg_key.clone()),
            project_name: c.project_name.clone(),
            buyer: c.buyer.clone(),
            project_code: c
                .engine_keys
                .first()
                .and_then(|k| k.strip_prefix("code:").map(String::from))
                .unwrap_or_default(),
            announcement_count: c.announcement_count,
        })
        .collect())
}

/// 手动建档：建档即查重（`force=true` 仅在用户明示「仍单独建档」后传）。
/// 返回 `outcome=created` 或 `outcome=duplicate`（重复再分 `dup_kind=manual|announcement`，
/// 界面据此区分「重复」与「疑似关联」并给出两个出口）。
#[tauri::command]
fn manual_project_create(
    paths: State<Mutex<AppPaths>>,
    project_name: String,
    buyer: String,
    area: Option<String>,
    budget: Option<String>,
    note: Option<String>,
    lead_source: Option<String>,
    expect_method: Option<String>,
    force: Option<bool>,
) -> Result<serde_json::Value, String> {
    // 必填校验放最前面：给字段级的中文错误，而不是让深层报「数据库错误」
    if project_name.trim().is_empty() {
        return Err("建档被拒：项目名必填".into());
    }
    if buyer.trim().is_empty() {
        return Err("建档被拒：采购人必填".into());
    }
    let paths = paths.lock().unwrap();
    let store = open_store(&paths)?;
    let draft = intel_manual::ManualDraft {
        project_name,
        buyer,
        area,
        budget,
        note,
        lead_source,
        expect_method,
    };
    // 查重范围 = 引擎已聚合的全部公告项目（与 CLI 不同：CLI 只传手动档案库本身）
    let payload = intel_payload::project_list_payload(&store, None, 0).map_err(err_string)?;
    let keys: Vec<(String, String)> = payload
        .projects
        .iter()
        .map(|c| (c.agg_key.clone(), c.project_name.clone()))
        .collect();
    match intel_manual::create_manual(&store, &draft, force.unwrap_or(false), &keys, None) {
        Ok(intel_manual::CreateOutcome::Created(m)) => Ok(serde_json::json!({
            "outcome": "created",
            "manual_id": m.manual_id,
            "agg_key": m.agg_key,
            "agg_basis": m.agg_basis,
        })),
        Ok(intel_manual::CreateOutcome::Duplicate(d)) => Ok(serde_json::json!({
            "outcome": "duplicate",
            "prompt": d.prompt(),
            "agg_key": d.key.key,
            "dup_kind": if d.manual_hit.is_some() { "manual" } else { "announcement" },
            "manual_hit": d.manual_hit.as_ref().map(|m| serde_json::json!({
                "manual_id": m.manual_id, "project_name": m.project_name, "agg_key": m.agg_key,
            })),
            "announcement_hit": d.announcement_hit,
        })),
        Err(e) => Err(err_string(e)),
    }
}

/// 手动档案列表（界面左栏并卡用；返回对象已注入界面别名，与引擎卡同构）。
#[tauri::command]
fn manual_project_list(
    paths: State<Mutex<AppPaths>>,
    merged_only: Option<bool>,
) -> Result<serde_json::Value, String> {
    let paths = paths.lock().unwrap();
    let store = open_store(&paths)?;
    let manuals = intel_manual::list_manual(&store, merged_only.unwrap_or(false)).map_err(err_string)?;
    let mut out = Vec::with_capacity(manuals.len());
    for m in manuals.iter() {
        let card = serde_json::to_value(intel_payload::manual_card(m)).map_err(|e| e.to_string())?;
        let mut obj = match card_with_ui_aliases(card) {
            v if v.is_object() => v,
            _ => continue,
        };
        if let Some(o) = obj.as_object_mut() {
            // 手动档案本体字段保留（manual_id 是合并/忽略/重开的定位键）
            o.insert("manual_id".into(), serde_json::json!(m.manual_id));
            o.insert("note".into(), serde_json::json!(m.note.clone().unwrap_or_default()));
            o.insert("created_at".into(), serde_json::json!(m.created_at));
            o.insert("updated_at".into(), serde_json::json!(m.updated_at));
            o.insert("merged_into".into(), serde_json::json!(m.merged_into));
        }
        out.push(obj);
    }
    Ok(serde_json::json!({ "manuals": out }))
}

/// 疑似关联：先按当前公告侧候选**检测**（落 pending 提示，从不自动合并），
/// 再取该档案的关联列表（默认只取 pending）。
#[tauri::command]
fn manual_project_links(
    paths: State<Mutex<AppPaths>>,
    manual_id: String,
    pending_only: Option<bool>,
) -> Result<serde_json::Value, String> {
    let paths = paths.lock().unwrap();
    let store = open_store(&paths)?;
    let candidates = announcement_candidates(&store).map_err(err_string)?;
    intel_manual::detect_suspected_links(&store, &candidates, None).map_err(err_string)?;
    let links = intel_manual::links_of(&store, manual_id.trim(), pending_only.unwrap_or(true))
        .map_err(err_string)?;
    Ok(serde_json::json!({ "links": links }))
}

/// 确认合并：手动档案并进公告聚合项目。采购人/项目名以公告为准，
/// 手动补充字段保留并标注；写「手动合并」审计。
#[tauri::command]
fn manual_project_merge(
    paths: State<Mutex<AppPaths>>,
    manual_id: String,
    target_project: String,
    name: String,
    buyer: String,
    actor: Option<String>,
) -> Result<serde_json::Value, String> {
    if manual_id.trim().is_empty() || target_project.trim().is_empty() {
        return Err("合并被拒：manual_id 与 target_project 均必填".into());
    }
    let paths = paths.lock().unwrap();
    let store = open_store(&paths)?;
    match intel_manual::merge_manual_into_project(
        &store,
        manual_id.trim(),
        target_project.trim(),
        &name,
        &buyer,
        actor.as_deref().unwrap_or("ui"),
        None,
    ) {
        Ok(o) => Ok(serde_json::to_value(&o).map_err(|e| e.to_string())?),
        Err(e) => Err(err_string(e)),
    }
}

/// 忽略（「不是同一项目」）：写审计、置 ignored，之后同一对不再提示。
#[tauri::command]
fn manual_project_ignore(
    paths: State<Mutex<AppPaths>>,
    manual_id: String,
    target_project: String,
    actor: Option<String>,
) -> Result<serde_json::Value, String> {
    let paths = paths.lock().unwrap();
    let store = open_store(&paths)?;
    intel_manual::ignore_link(
        &store,
        manual_id.trim(),
        target_project.trim(),
        actor.as_deref().unwrap_or("ui"),
        None,
    )
    .map_err(err_string)?;
    Ok(serde_json::json!({ "outcome": "ignored", "manual_id": manual_id, "target_project": target_project }))
}

/// 重开对照：ignored 改回 pending，写审计（规格 A-7「可手动重开对照」）。
#[tauri::command]
fn manual_project_reopen(
    paths: State<Mutex<AppPaths>>,
    manual_id: String,
    target_project: String,
    actor: Option<String>,
) -> Result<serde_json::Value, String> {
    let paths = paths.lock().unwrap();
    let store = open_store(&paths)?;
    intel_manual::reopen_link(
        &store,
        manual_id.trim(),
        target_project.trim(),
        actor.as_deref().unwrap_or("ui"),
        None,
    )
    .map_err(err_string)?;
    Ok(serde_json::json!({ "outcome": "reopened", "manual_id": manual_id, "target_project": target_project }))
}

/// 线索动作：confirm（确认）/ reject（忽略，终态）/ todo（转待办，落 work_item 进晨报看板）。
#[tauri::command]
fn intel_lead_act(
    paths: State<Mutex<AppPaths>>,
    action: String,
    key: String,
    owner: Option<String>,
) -> Result<serde_json::Value, String> {
    let paths = paths.lock().unwrap();
    let store = open_store(&paths)?;
    match action.as_str() {
        "confirm" => {
            let ok = intel_store::confirm_lead(&store, &key, None).map_err(err_string)?;
            if !ok {
                return Err("线索不存在，或状态已流转（刷新后重试）".into());
            }
            Ok(serde_json::json!({ "action": "confirm", "key": key }))
        }
        "reject" => {
            let ok = intel_store::reject_lead(&store, &key, None).map_err(err_string)?;
            if !ok {
                return Err("线索不存在，或已忽略".into());
            }
            Ok(serde_json::json!({ "action": "reject", "key": key }))
        }
        "todo" => {
            let today_iso = today().to_iso();
            let owner = owner.unwrap_or_else(|| "我".to_string());
            match intel_store::generate_todo(&store, &key, &today_iso, &owner) {
                Ok(Ok(t)) => Ok(serde_json::json!({ "action": "todo", "todo": t })),
                Ok(Err(r)) => Err(format!("未生成待办：{}", r.reason())),
                Err(e) => Err(err_string(e)),
            }
        }
        other => Err(format!("非法动作：{other}（仅支持 confirm / reject / todo）")),
    }
}

/// FR-1：网点视图数据 + 手动刷新
#[tauri::command]
fn dealer_get(paths: State<Mutex<AppPaths>>) -> Result<serde_json::Value, String> {
    let paths = paths.lock().unwrap();
    let state = load_intel(&paths);
    Ok(serde_json::json!({
        "snapshot": state.dealer_snapshot,
        "last_diff": state.dealer_last_diff,
        "last_run_date": state.last_dealer_run_date,
        // v2.2.1：快照树（安徽省 → 16 地市 → 区县），区县从公司名/地址/区域解析，
        // 解析不到的归「待归类」。映射表在 wb_core::anhui（带单测），这里只做分组。
        "tree": dealer_tree(&state.dealer_snapshot),
        // 本周新增/消失按地市分组（与树同一套归属口径）
        "added_by_city": diff_by_city(state.dealer_last_diff.as_ref().map(|d| &d.added)),
        "removed_by_city": diff_by_city(state.dealer_last_diff.as_ref().map(|d| &d.removed)),
    }))
}

/// 网点 → 地市归属：区县名优先（公司名+区域+地址三处找），找不到再试
/// 文本里是否直接出现 16 市市名；都不命中归「待归类」。
fn dealer_city(node: &wb_core::intel::DealerNode) -> &'static str {
    let hay = format!("{}{}{}", node.company_name, node.area, node.address);
    if let Some((_, city)) = wb_core::anhui::locate_in_text(&hay) {
        return city;
    }
    for c in wb_core::anhui::CITIES {
        if hay.contains(c) || hay.contains(c.trim_end_matches('市')) {
            return c;
        }
    }
    "待归类"
}

/// 快照树：[{city, districts: [{district, dealers: [公司名…]}], total}]，
/// 地市按 CITIES 固定顺序、区县按拼音序（BTreeMap 自然序）、待归类排最后。
fn dealer_tree(snapshot: &[wb_core::intel::DealerNode]) -> serde_json::Value {
    use std::collections::BTreeMap;
    let mut tree: BTreeMap<&str, BTreeMap<&str, Vec<&str>>> = BTreeMap::new();
    for n in snapshot {
        let city = dealer_city(n);
        let hay = format!("{}{}{}", n.company_name, n.area, n.address);
        let district = wb_core::anhui::locate_in_text(&hay)
            .map(|(d, _)| d)
            .unwrap_or("待归类");
        tree.entry(city).or_default().entry(district).or_default().push(&n.company_name);
    }
    let mut cities: Vec<serde_json::Value> = Vec::new();
    // 地市按 CITIES 固定顺序（皖北→皖中→皖南的既定排列），「待归类」固定排最后
    let mut ordered: Vec<&str> = wb_core::anhui::CITIES.iter().copied().filter(|c| tree.contains_key(c)).collect();
    if tree.contains_key("待归类") {
        ordered.push("待归类");
    }
    for city in ordered {
        let districts = &tree[city];
        let total: usize = districts.values().map(|v| v.len()).sum();
        cities.push(serde_json::json!({
            "city": city,
            "total": total,
            "districts": districts.iter().map(|(d, names)| serde_json::json!({
                "district": d, "count": names.len(), "dealers": names,
            })).collect::<Vec<_>>(),
        }));
    }
    serde_json::json!(cities)
}

/// 增减明细按地市分组：[{city, nodes: [{name, area}]}]
fn diff_by_city(nodes: Option<&Vec<wb_core::intel::DealerNode>>) -> serde_json::Value {
    use std::collections::BTreeMap;
    let mut by: BTreeMap<&str, Vec<serde_json::Value>> = BTreeMap::new();
    for n in nodes.into_iter().flatten() {
        by.entry(dealer_city(n)).or_default().push(serde_json::json!({
            "name": n.company_name, "area": n.area,
        }));
    }
    by.into_iter()
        .map(|(city, nodes)| serde_json::json!({ "city": city, "nodes": nodes }))
        .collect::<Vec<_>>()
        .into()
}

#[tauri::command]
fn dealer_refresh(app: tauri::AppHandle) -> Result<(), String> {
    spawn_dealer_run(app, true);
    Ok(())
}

/// 情报调度线程：每 10 分钟醒一次；当日未采集且已过 9:30（晨报之后）→ 日频采集；
/// 距上次网点比对 ≥7 天 → 周频比对。轮询而非定点，理由同晨报调度（休眠/时钟跳变）。
fn spawn_intel_scheduler(app: tauri::AppHandle) {
    std::thread::spawn(move || loop {
        std::thread::sleep(Duration::from_secs(600));
        let Some(paths) = app.try_state::<Mutex<AppPaths>>() else { continue };
        let (today_iso, minutes) = (today().to_iso(), now_minutes());
        let state = {
            let p = paths.lock().unwrap();
            load_intel(&p)
        };
        if state.last_run_date.as_deref() != Some(today_iso.as_str()) && minutes >= 9 * 60 + 30 {
            spawn_intel_run(app.clone(), false);
        }
        // 周频：距上次 ≥7 天
        let dealer_due = state
            .last_dealer_run_date
            .as_deref()
            .and_then(|d| CivilDate::parse(d))
            .map(|d| CivilDate::diff_days(d, today()) >= 7)
            .unwrap_or(true);
        if dealer_due {
            spawn_dealer_run(app.clone(), false);
        }
    });
}

/// v2.1 防多开兜底所需的全部 Win32 声明（仅 CreateMutexW / GetLastError /
/// FindWindowW）。不引 windows-sys 依赖（会动 Cargo.lock 触发全量依赖重解析）；
/// kernel32/user32 本就是 Rust Windows 目标的默认链接库，显式声明链接属性求稳。
#[cfg(target_os = "windows")]
mod win_si_ffi {
    use std::os::raw::{c_int, c_void};

    #[link(name = "kernel32")]
    extern "system" {
        pub fn CreateMutexW(
            attrs: *mut c_void,
            initial_owner: c_int,
            name: *const u16,
        ) -> *mut c_void;
        pub fn GetLastError() -> u32;
    }
    #[link(name = "user32")]
    extern "system" {
        pub fn FindWindowW(class: *const u16, window: *const u16) -> *mut c_void;
    }
}

/// v2.1 防多开·应用侧兜底仲裁（任务书要求①：仲裁失败也不许双进程常驻）。
/// 插件的二次启动退出路径**仅在 FindWindowW 命中仲裁窗口时生效**（插件
/// windows.rs:82-131：互斥体已存在但窗口找不到时整个退出块被跳过，二次
/// 启动方会完整启动成双进程——run 40597450315 / 40604956721 场景2 的
/// 实测形态）。本兜底在构建 Builder 之前自判，补上这条缺口：
///   · 自持一把独立命名互斥体（与插件的 {id}-sim 不同名；进程终止由 OS
///     回收句柄，崩溃也不会残留）；
///   · 互斥体已存在（确有首实例）且插件仲裁窗口（{id}-sic/-siw，见插件
///     windows.rs:73-74）找不到 → 通道已断、插件救不了 → 本进程立即退出，
///     保住「不双开」红线（通道已断时首实例窗口可能仍藏于托盘，恢复能力
///     以通道健在为前提，此边界如实写入自检文档）；
///   · 仲裁窗口健在 → 不介入，让插件 setup 走正常路径（回调恢复+置前+退出），
///     兜底绝不抢在插件前面，正常路径行为零变化。
/// 类名/窗口名与插件保持一致的约定：identifier 变更时此处需同步（插件侧
/// 读 app.config().identifier，此处在 Builder 构建前拿不到 config，故沿用
/// 同一常量；两处均已注释标明）。
#[cfg(target_os = "windows")]
fn single_instance_fallback_guard() {
    const ERROR_ALREADY_EXISTS: u32 = 183;
    // 与 tauri.conf.json 的 identifier 保持同步（同上注释）
    const APP_ID: &str = "com.anhui.pr.workbench";

    let wide = |s: &str| -> Vec<u16> { s.encode_utf16().chain(std::iter::once(0)).collect() };

    unsafe {
        // 「已存在」时 CreateMutexW 同样返回有效句柄，判定依据是 GetLastError；
        // 返回的句柄由本进程持有到退出（不 CloseHandle），与首实例生命周期绑定。
        let mutex = wide(&format!("{APP_ID}-wb-fallback-sim"));
        let _h = win_si_ffi::CreateMutexW(std::ptr::null_mut(), 1, mutex.as_ptr());
        if win_si_ffi::GetLastError() != ERROR_ALREADY_EXISTS {
            return; // 首实例：仅占住兜底互斥体，其余交给插件正常机制
        }
        let class = wide(&format!("{APP_ID}-sic"));
        let window = wide(&format!("{APP_ID}-siw"));
        if win_si_ffi::FindWindowW(class.as_ptr(), window.as_ptr()).is_null() {
            // 有首实例 + 通道断：插件不会退出本进程，这里补位退出。
            std::process::exit(0);
        }
        // 通道健在：正常路径交给插件（回调恢复+置前+二次启动方退出）。
    }
}

/// 应用启动入口。
pub fn run() {
    // 防多开兜底必须先于一切初始化执行（见函数注释）；
    // 非 Windows 平台为空操作。
    #[cfg(target_os = "windows")]
    single_instance_fallback_guard();

    tauri::Builder::default()
        // v2.1 防多开：必须注册在插件链最前面——
        // 二次启动（任务栏最小化态 / 托盘态 / 快速双击竞态）把已有主窗口置前，
        // 随后本次进程立即退出，用户视角始终只有一个窗口。
        .plugin(tauri_plugin_single_instance::init(|app: &tauri::AppHandle, _args, _cwd| {
            // 回调在首实例主线程的 WM_COPYDATA（跨进程同步 sent 消息）WndProc 里执行，
            // 二次启动方正阻塞等它返回才退出。若在这里内联做窗口操作——尤其托盘
            // 隐藏态下 show() 会触发 WebView2 controller 的跨线程握手，在 sent 消息
            // 上下文内无法完成——主线程一旦挂起，二次启动方的 SendMessageTimeoutW
            // 在消息已被接收方开始处理后便不再受超时保护，双方一起挂死
            // （run 40597450315 场景2：25s 后双进程常驻、启动方不退出）。
            // 故回调内零窗口操作、立即返回（二次启动方随之 exit(0)，无论后续恢复
            // 动作成败——防多开红线由「先退出」结构性保证）；恢复窗口的动作经
            // 事件循环以队列任务执行，与托盘菜单同一上下文，安全。
            let handle = app.clone();
            std::thread::spawn(move || {
                let handle2 = handle.clone();
                // 从非主线程调用 run_on_main_thread → event proxy 异步入队（fire-and-forget）
                let _ = handle.run_on_main_thread(move || {
                    if let Some(w) = handle2.get_webview_window("main") {
                        let _ = w.show(); // 托盘隐藏态（B7：点×仅 hide 不退出）：先恢复可见
                        let _ = w.unminimize(); // 任务栏最小化态
                        let _ = w.set_focus(); // 最后置前
                    }
                });
            });
        }))
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_shell::init())
        // v2.2.1 设置页导入：原生文件选择器（此前手填路径，空路径点击静默无反应）
        .plugin(tauri_plugin_dialog::init())
        // tauri-plugin-updater 2.14 起移除了 init()，改用 Builder；
        // pubkey/endpoints 从 tauri.conf.json plugins.updater 读取。
        .plugin(tauri_plugin_updater::Builder::new().build())
        .invoke_handler(tauri::generate_handler![
            import_excel,
            list_items,
            kpi_overview,
            alerts,
            units,
            backup_now,
            add_item,
            update_item,
            log_action,
            item_actions,
            get_prefs,
            set_prefs,
            sync_status,
            sync_setup,
            sync_login,
            sync_logout,
            sync_now,
            fiscal_year,
            check_update,
            install_update,
            get_pending_brief,
            trigger_brief,
            mark_brief_delivered,
            intel_get,
            intel_refresh,
            intel_set_verdict,
            intel_leads_get,
            intel_projects_get,
            intel_project_get,
            intel_lead_act,
            // 六个手动档案命令：壳层只做必填校验与别名注入，
            // 实现全部走 wb_core::intel_manual（与 wbd CLI 同一批函数）。
            manual_project_create,
            manual_project_list,
            manual_project_links,
            manual_project_merge,
            manual_project_ignore,
            manual_project_reopen,
            subsidy_export,
            dealer_get,
            dealer_refresh,
            member_list,
            member_create,
            member_set_role,
            member_set_status,
            assign_owner,
            pending_owner_changes,
            ack_owner_changes,
            // v3.4.0 项目跟踪（W2）
            project_list,
            project_get,
            project_create,
            project_update,
            project_delete,
            project_notes,
            project_note_create,
            project_note_delete,
            project_attachments,
            project_attach,
            project_reclaim,
            project_migration_preview,
            project_migration_run,
            // v3.4.0 待办版块（W5）
            todo_list,
            todo_create,
            todo_backfill,
            data_dir_path,
        ])
        // B7：点关闭 = 最小化到托盘，晨报仍从托盘送达；
        // 只有托盘菜单「退出」才真正退出（退出后的补推靠 last_brief.json）。
        .on_window_event(|window, event| {
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                window.hide().ok();
                api.prevent_close();
            }
        })
        .setup(|app| {
            let data_dir = app
                .path()
                .app_data_dir()
                .expect("无法确定用户数据目录");
            std::fs::create_dir_all(&data_dir).expect("无法创建用户数据目录");
            app.manage(Mutex::new(AppPaths { data_dir: data_dir.clone() }));

            // ── v3.3.1 · 启动期清洗（宿州源历史苏州误抓数据，幂等一次性）──
            // 详见 wb_core::intel_clean：先备份 + 账本（backups/clean/），后删除。
            // 任何一步失败都不阻断启动，但必须可见——清洗失败意味着苏州数据还在库里。
            match open_store(&AppPaths { data_dir }) {
                Ok(store) => {
                    for r in wb_core::intel_clean::run_v331_startup(&store) {
                        match r {
                            Ok(rep) if rep.executed => eprintln!(
                                "[v3.3.1 清洗] {} 已清 {} 行（current {} / 线索 {} / 洞察 {}），备份 {}，账本 {}",
                                rep.source_id, rep.rows_total, rep.rows_current,
                                rep.leads_deleted, rep.insights_deleted,
                                rep.backup_file, rep.ledger_file
                            ),
                            Ok(rep) if rep.skipped_read_only => eprintln!(
                                "[v3.3.1 清洗] {} 只读库，本轮跳过", rep.source_id
                            ),
                            Ok(_) => {} // 幂等：上一轮已清过 / 该源无数据，无需动作
                            Err(e) => eprintln!("[v3.3.1 清洗] 失败：{e}"),
                        }
                    }
                }
                Err(e) => eprintln!("[v3.3.1 清洗] 打开库失败：{e}"),
            }

            // ── 托盘菜单（P0-2 缺失项：此前完全没有构建） ──
            let handle = app.handle().clone();
            let open_item = MenuItem::with_id(app, "open", "打开主界面", true, None::<&str>)?;
            let brief_item = MenuItem::with_id(app, "brief", "查看晨报", true, None::<&str>)?;
            let quit_item = MenuItem::with_id(app, "quit", "退出", true, None::<&str>)?;
            let menu = Menu::with_items(app, &[&open_item, &brief_item, &quit_item])?;

            TrayIconBuilder::with_id("main-tray")
                .icon(app.default_window_icon().unwrap().clone())
                .tooltip("综合工作台")
                .menu(&menu)
                .show_menu_on_left_click(false)
                .on_menu_event(move |app, event| match event.id.as_ref() {
                    "open" => {
                        if let Some(w) = app.get_webview_window("main") {
                            let _ = w.show();
                            let _ = w.set_focus();
                        }
                    }
                    "brief" => {
                        if let Some(w) = app.get_webview_window("main") {
                            let _ = w.show();
                            let _ = w.set_focus();
                            let _ = w.emit("brief-open", ());
                        }
                    }
                    "quit" => app.exit(0),
                    _ => {}
                })
                .build(app)?;

            // FR-6 晨报随新版本即时启动：版本号变化 + 已过 9:00 + 本版本未送过
            // → 立即送达一次并标注「新版本」。放在补推判定**之前**：送达成功后
            // 下面的 pending 判定会看到 AlreadyDelivered，不再发 brief-pending，
            // 保证版本事件与补发只送达一次（R6-2）。判定失败（如开库失败）
            // 不阻塞启动，交给 30 秒调度线程按 DueLate 兜底。
            {
                let p = app.state::<Mutex<AppPaths>>();
                let paths = p.lock().unwrap();
                let st = read_delivery(&paths);
                // R6-1：版本号只认 tauri.conf.json 构建期注入的包版本
                let cur = handle.package_info().version.to_string();
                if let brief::VersionBootDecision::Deliver(ev) =
                    brief::decide_version_boot(now_minutes(), &cur, &st)
                {
                    match deliver_brief(&handle, &paths, Some(ev)) {
                        Ok(v) => {
                            let _ = handle.emit("brief-delivered", v);
                        }
                        Err(e) => {
                            let _ = handle.emit("brief-error", e);
                        }
                    }
                }
            }

            // 启动即补推：应用可能昨天没开，今天一开机就该看到晨报（B7 验收②）
            {
                let p = app.state::<Mutex<AppPaths>>();
                let paths = p.lock().unwrap();
                let d = brief::decide(today(), now_minutes(), &read_delivery(&paths));
                if d.verdict.is_due() {
                    let _ = handle.emit("brief-pending", serde_json::json!({
                        "brief_date": d.brief_date,
                        "late_minutes": d.late_minutes,
                        "verdict": format!("{:?}", d.verdict),
                    }));
                }
            }

            // 9:00 调度线程
            spawn_brief_scheduler(handle.clone());
            // v2.2 FR-2/FR-1：情报日频 + 网点周频调度（独立线程，失败不影响本地功能）
            spawn_intel_scheduler(handle.clone());

            // ── 启动静默检查更新（v2.1 默认策略）──
            // 只检查不安装；发现新版 emit 给前端，由用户确认后才走 install_update。
            // 失败静默（离线/端点未就绪是常态），设置页仍保留手动检查入口。
            tauri::async_runtime::spawn(async move {
                // E2E 构建（--features e2e-update）：不走向上 emit，直接驱动
                // check → download → verify → install 全链路，把结果写进 WB_E2E_RESULT。
                // 发布构建不含这段（feature 默认关闭），走下面原本的静默检查。
                #[cfg(feature = "e2e-update")]
                {
                    if std::env::var("WB_E2E_RESULT").map(|v| !v.trim().is_empty()).unwrap_or(false) {
                        crate::e2e::run_once(handle.clone()).await;
                        return;
                    }
                }
                if let Ok(updater) = handle.updater() {
                    if let Ok(Some(u)) = updater.check().await {
                        let _ = handle.emit("update-available", serde_json::json!({
                            "version": u.version,
                            "notes": u.body,
                        }));
                    }
                }
            });
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("综合工作台启动失败");
}


#[cfg(test)]
mod tz_tests {
    use super::*;

    /// P0 回归（v2.2.3）：chrono 偏移是**秒**，适配层必须换算成分钟再进 `civil_and_minutes`。
    /// 换算缺失时 28800 分钟 = 整 20 天，2026-10-29 会被算成 2026-11-18。
    #[test]
    fn offset_seconds_converted_to_minutes() {
        assert_eq!(offset_seconds_to_minutes(28800), 480); // UTC+8
        assert_eq!(offset_seconds_to_minutes(0), 0); // UTC
        assert_eq!(offset_seconds_to_minutes(-25200), -420); // UTC-7
        // 半小时时区（UTC+5:30，印度）：19800 秒 → 330 分钟（整除截断正确）
        assert_eq!(offset_seconds_to_minutes(19800), 330);
    }

    /// 注入非零偏移走完整链路：`civil_and_minutes` 必须拿到分钟口径的偏移，
    /// UTC 2026-10-29 01:00 + 480 分钟 = 本地 2026-10-29 09:00。
    #[test]
    fn civil_and_minutes_with_utc_plus_8_minute_offset() {
        let (d, m) = wb_core::local_time::civil_and_minutes("2026-10-29T01:00:00Z", 480)
            .expect("valid iso");
        assert_eq!((d.year, d.month, d.day), (2026, 10, 29));
        assert_eq!(m, 9 * 60);
    }

    /// 防重入（v2.2.4 追加）：连点「立即刷新」或调度线程同时到点时，
    /// 两轮采集会各自 load 一份旧快照再各自 save，后落盘者覆盖前一轮的站点状态。
    /// 这条断言锁死「同一时刻只允许一轮采集」这一契约。
    #[test]
    fn intel_run_is_exclusive_until_released() {
        end_intel_run(); // 清场：测试可能与其他用例共享同一静态量
        assert!(try_begin_intel_run(), "首次应能占用");
        assert!(!try_begin_intel_run(), "占用期间第二次必须被拒");
        assert!(!try_begin_intel_run(), "连点第三次同样必须被拒");
        end_intel_run();
        assert!(try_begin_intel_run(), "释放后应可再次占用");
        end_intel_run();
    }

    /// 反证用例：若把秒当分钟传进去（修复前的实际行为），日期会 +20 天——
    /// 该断言锁死"偏移必须是分钟"这一契约，防止回归。
    #[test]
    fn seconds_fed_as_minutes_shifts_date_by_20_days() {
        let (d, _) = wb_core::local_time::civil_and_minutes("2026-10-29T01:00:00Z", 28800)
            .expect("valid iso");
        assert_eq!((d.year, d.month, d.day), (2026, 11, 18)); // 2026-10-29 + 20 天
    }
}

/// v3.0 同步胶水层：只测**不发网络请求**的部分（传输链路已由 wbd sync-live
/// 对生产服务器 8/8 验证，见档D）。锁死的契约：
/// ① 状态 JSON 永不包含令牌；② 未登录时「立即同步」返回明确语义而非报错崩溃。
#[cfg(test)]
mod sync_glue_tests {
    use super::*;

    fn tmp_store() -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().expect("tmpdir");
        let store = Store::open(dir.path()).expect("open store");
        (dir, store)
    }

    #[test]
    fn status_json_never_leaks_token() {
        let (_keep, store) = tmp_store();
        sync_client::state_set(store.conn(), sync_client::keys::BASE_URL, "https://ahxbd.work").unwrap();
        sync_client::state_set(store.conn(), sync_client::keys::TOKEN, "tok_secret_should_not_appear").unwrap();
        sync_client::state_set(store.conn(), sync_client::keys::MEMBER_NAME, "admin").unwrap();
        sync_client::state_set(store.conn(), sync_client::keys::CURSOR, "7").unwrap();
        let v = sync_status_value(store.conn());
        let s = v.to_string();
        assert!(!s.contains("tok_secret_should_not_appear"), "令牌绝不能出现在状态 JSON 里");
        assert_eq!(v["base_url"], "https://ahxbd.work");
        assert_eq!(v["member_name"], "admin");
        assert_eq!(v["cursor"], 7);
        assert_eq!(v["logged_in"], true);
        assert_eq!(v["state"], "已连接");
    }

    #[test]
    fn sync_now_without_login_reports_not_logged_in() {
        let (_keep, store) = tmp_store();
        let transport = sync_client::http::HttpTransport::new();
        let v = run_sync_once(store.conn(), &transport);
        assert_eq!(v["error"], "尚未登录", "未登录时应给出明确语义，而不是空指针或崩溃");
    }
}
