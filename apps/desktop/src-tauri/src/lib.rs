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
use std::sync::Mutex;
use std::time::Duration;

use tauri::menu::{Menu, MenuItem};
use tauri::tray::TrayIconBuilder;
use tauri::{Emitter, Manager, State};
use tauri_plugin_notification::NotificationExt;
use tauri_plugin_updater::UpdaterExt;
use wb_core::{
    brief::{self, DeliveryState, QuietHours},
    intel,
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

// ─────────────────────── 单机偏好（免打扰 / 语音播报） ───────────────────────

/// 读偏好：免打扰窗口 + 语音播报开关。持久化通道是 sync_state 键值表——
/// 多端模式已放弃，但键值表仍是官方通道（老 workbench.db 无需迁移、可逆）。
/// 免打扰缺省回 22:00–8:00（与首版默认一致）；语音开关**缺省开启**
/// （2026-10-27 统筹者裁决：PRD 的「默认关闭」是功能未实现时代的口径）。
fn load_prefs(conn: &rusqlite::Connection) -> (QuietHours, bool) {
    let hhmm = |key: &str| {
        sync_client::state_get(conn, key).and_then(|v| wb_core::local_time::parse_hhmm(&v))
    };
    let quiet = QuietHours {
        start_minute: hhmm(sync_client::keys::QUIET_START).unwrap_or(22 * 60),
        end_minute: hhmm(sync_client::keys::QUIET_END).unwrap_or(8 * 60),
    };
    let voice_enabled = sync_client::state_get(conn, sync_client::keys::VOICE_ENABLED)
        .map(|v| v != "0")
        .unwrap_or(true);
    (quiet, voice_enabled)
}

/// v2.2 语音包偏好：读取 voice_pack 键；仅认 "pack" 为语音包模式，
/// 其余值（含未设置）一律回落 "system"（系统 TTS 朗读实时文案）。
/// 归一规则集中在 wb_core::voice（可单测），这里只负责取键。
fn load_voice_pack(conn: &rusqlite::Connection) -> String {
    let raw = sync_client::state_get(conn, sync_client::keys::VOICE_PACK);
    wb_core::voice::normalize_pack(raw.as_deref()).to_string()
}

/// v2.2.1 主题档位：读取 theme 键。仅认 "light"（办公态）与 "system"（跟随系统），
/// 其余值（含未设置、旧版脏值）一律回落 **"dark"（机甲态）**。
///
/// 为什么默认锁暗色：机甲风只画在暗色主题里，原来的默认值是「跟随系统」——
/// 用户实机（亮色系统环境）打开看到的是素净办公界面，机甲视觉完全没生效，
/// 实测反馈「UI 的效果基本未实现」。默认锁暗色是对症直改，亮色档保留在设置里可切。
fn load_theme(conn: &rusqlite::Connection) -> String {
    match sync_client::state_get(conn, sync_client::keys::THEME).as_deref() {
        Some("light") => "light".to_string(),
        Some("system") => "system".to_string(),
        _ => "dark".to_string(),
    }
}

/// v2.2.2（需求15）：交互音效独立开关 + 点击音采样 id。
/// 与 voice_enabled **互不相干**——这是「关掉语音播报连点击音一起消失」的对症修复：
/// 两个开关各存各的键，任一关闭都不影响另一个。
/// 缺省开启（用户主动点击产生的反馈音属于操作确认，不是主动推送，不受免打扰约束）。
fn load_sfx(conn: &rusqlite::Connection) -> (bool, String) {
    let enabled = sync_client::state_get(conn, sync_client::keys::SFX_ENABLED)
        .map(|v| v != "0")
        .unwrap_or(true);
    // v2.2.4：旧偏好（tick-a/b/c 等）经 normalize_sample 映射到冻结版 id，不丢设置
    let sample = sync_client::state_get(conn, sync_client::keys::SFX_SAMPLE)
        .map(|v| wb_core::sfx::normalize_sample(Some(&v)))
        .unwrap_or_else(|| wb_core::sfx::DEFAULT_SAMPLE.to_string());
    (enabled, sample)
}

/// 音量档位（v2.2.4，规范第六节①两档）：静音/安静/标准。非法值回落标准。
fn load_sfx_volume(conn: &rusqlite::Connection) -> String {
    sync_client::state_get(conn, sync_client::keys::SFX_VOLUME)
        .filter(|v| matches!(v.as_str(), "quiet" | "standard"))
        .unwrap_or_else(|| "standard".to_string())
}

/// 偏好的 JSON 形态（time input 的 value 格式就是 HH:MM）。
fn prefs_json(conn: &rusqlite::Connection) -> serde_json::Value {
    let (quiet, voice_enabled) = load_prefs(conn);
    let (sfx_enabled, sfx_sample) = load_sfx(conn);
    serde_json::json!({
        "quiet_start": wb_core::local_time::fmt_hhmm(quiet.start_minute),
        "quiet_end": wb_core::local_time::fmt_hhmm(quiet.end_minute),
        "voice_enabled": voice_enabled,
        "voice_pack": load_voice_pack(conn),
        "theme": load_theme(conn),
        "sfx_enabled": sfx_enabled,
        "sfx_sample": sfx_sample,
        "sfx_volume": load_sfx_volume(conn),
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
            "today_schedule": brief.today_schedule,
            "weekly": brief.weekly,
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

/// 单机偏好写入：免打扰窗口（HH:MM）+ 语音播报开关 + 语音包模式（v2.2）+ 主题档位（v2.2.1）
/// + 交互音效独立开关与采样（v2.2.2）。
/// 时间格式在本地就拦下，不合法不落库；voice_pack 仅认 system/pack 两值；
/// theme 仅认 dark/light/system 三值（未设置默认 dark）；sfx_sample 仅认已知采样 id；
/// 成功返回回读后的规范值。
#[tauri::command]
fn set_prefs(
    paths: State<Mutex<AppPaths>>,
    quiet_start: String,
    quiet_end: String,
    voice_enabled: bool,
    voice_pack: String,
    theme: String,
    sfx_enabled: bool,
    sfx_sample: String,
    sfx_volume: Option<String>,
) -> Result<serde_json::Value, String> {
    let s = wb_core::local_time::parse_hhmm(&quiet_start)
        .ok_or_else(|| "免打扰开始时间格式不合法（应为 HH:MM）".to_string())?;
    let e = wb_core::local_time::parse_hhmm(&quiet_end)
        .ok_or_else(|| "免打扰结束时间格式不合法（应为 HH:MM）".to_string())?;
    if wb_core::voice::normalize_pack(Some(&voice_pack)) != voice_pack {
        return Err("voice_pack 仅支持 system / pack".to_string());
    }
    if !matches!(theme.as_str(), "dark" | "light" | "system") {
        return Err("theme 仅支持 dark / light / system".to_string());
    }
    if !wb_core::sfx::is_known_sample(&sfx_sample) {
        return Err(format!(
            "sfx_sample 仅支持 {}",
            wb_core::sfx::SFX_IDS.join(" / ")
        ));
    }
    // v2.2.4：音量档位仅认两档（规范第六节①），未传则保持旧值（缺省标准）
    if let Some(v) = sfx_volume.as_deref() {
        if !matches!(v, "quiet" | "standard") {
            return Err("sfx_volume 仅支持 quiet / standard".to_string());
        }
    }
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
        sync_client::state_set(
            store.conn(),
            sync_client::keys::VOICE_ENABLED,
            if voice_enabled { "1" } else { "0" },
        )
        .map_err(err_string)?;
        sync_client::state_set(
            store.conn(),
            sync_client::keys::VOICE_PACK,
            &voice_pack,
        )
        .map_err(err_string)?;
        sync_client::state_set(
            store.conn(),
            sync_client::keys::THEME,
            &theme,
        )
        .map_err(err_string)?;
        // v2.2.2（需求15）：音效开关独立落库，与 VOICE_ENABLED 无任何联动。
        sync_client::state_set(
            store.conn(),
            sync_client::keys::SFX_ENABLED,
            if sfx_enabled { "1" } else { "0" },
        )
        .map_err(err_string)?;
        sync_client::state_set(
            store.conn(),
            sync_client::keys::SFX_SAMPLE,
            &sfx_sample,
        )
        .map_err(err_string)?;
        // v2.2.4：两档音量落库（未传则保持旧值，不覆盖）
        if let Some(v) = sfx_volume.as_deref() {
            sync_client::state_set(
                store.conn(),
                sync_client::keys::SFX_VOLUME,
                v,
            )
            .map_err(err_string)?;
        }
        Ok(prefs_json(store.conn()))
    }
}

/// 业务年度（v2.2.1）：前端启动时回填 YEAR，看板/明细/时间线/晨报同一年。
#[tauri::command]
fn fiscal_year() -> i32 {
    wb_core::FISCAL_YEAR
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
    Ok(serde_json::json!({
        "id": r.id,
        "seq_no": r.seq_no,
        "plan_start_date": r.plan_start_date,
        "plan_start_precision": r.plan_start_precision,
        "plan_end_date": r.plan_end_date,
        "plan_end_precision": r.plan_end_precision,
        "needs_confirm": r.needs_confirm,
        "queued": r.queued,
    }))
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
    let (quiet, voice_enabled) = load_prefs(store.conn());
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
        "voice_enabled": voice_enabled,
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
            "weekly": b.weekly.is_some(),
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

fn load_intel(paths: &AppPaths) -> intel::IntelState {
    intel::IntelState::load(&paths.data_dir)
}

/// 日频抓取一轮（调度线程与手动「立即刷新」共用）。
/// 抓取在独立线程执行不阻塞 UI；同站间隔 ≥2s（预研口径）；失败只标记站点。
fn spawn_intel_run(app: tauri::AppHandle, manual: bool) {
    std::thread::spawn(move || {
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
        let mut results: Vec<intel::SiteResult> = Vec::new();
        let mut new_total = 0usize;
        let mut failed = 0usize;
        let mut first = true;
        for spec in intel::SITE_REGISTRY.iter().filter(|s| s.tier.is_scheduled()) {
            if !first {
                // 同站请求间隔 ≥2s：跨站也保持，避免整体突发
                std::thread::sleep(Duration::from_secs(2));
            }
            first = false;
            let r = intel::fetch_site(spec, &fetcher, intel::KEYWORD, &start_iso, &end_iso, &today_iso);
            let sr = match r {
                Ok(items) => {
                    let (new_n, _) = intel::merge_items(&mut state, items, &today_iso);
                    new_total += new_n;
                    let n = state.items.values().filter(|i| i.site_id == spec.id).count();
                    intel::SiteResult {
                        site_id: spec.id.to_string(),
                        status: intel::FetchStatus::Ok,
                        detail: String::new(),
                        items: n,
                        new_items: new_n,
                    }
                }
                Err((st, detail)) => {
                    failed += 1;
                    intel::SiteResult { site_id: spec.id.to_string(), status: st, detail, items: 0, new_items: 0 }
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
            results.push(sr);
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
            Err((st, detail)) => {
                // 失败不改快照，只发事件让界面可见（AC-1-4 口径）
                let _ = app.emit(
                    "dealer-updated",
                    serde_json::json!({ "manual": manual, "ok": false, "status": format!("{:?}", st), "detail": detail }),
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
            get_prefs,
            set_prefs,
            fiscal_year,
            check_update,
            install_update,
            get_pending_brief,
            trigger_brief,
            mark_brief_delivered,
            intel_get,
            intel_refresh,
            intel_set_verdict,
            dealer_get,
            dealer_refresh,
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
            app.manage(Mutex::new(AppPaths { data_dir }));

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

    /// 反证用例：若把秒当分钟传进去（修复前的实际行为），日期会 +20 天——
    /// 该断言锁死"偏移必须是分钟"这一契约，防止回归。
    #[test]
    fn seconds_fed_as_minutes_shifts_date_by_20_days() {
        let (d, _) = wb_core::local_time::civil_and_minutes("2026-10-29T01:00:00Z", 28800)
            .expect("valid iso");
        assert_eq!((d.year, d.month, d.day), (2026, 11, 18)); // 2026-10-29 + 20 天
    }
}
