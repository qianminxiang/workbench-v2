//!
//! 架构 §3.1 分层判据：这个文件只做「系统层」——窗口、托盘、系统通知、
//! 把领域核心（wb-core）的结果递给前端。业务逻辑一律不写在这里。
//!
//! B7 最小形态在这里落地：
//! - 托盘常驻：应用关闭窗口不退出，晨报从托盘送达（系统通知）；
//! - 9:00 本地时钟调度：不依赖网络与服务端，断网照常；迟到补发；
//! - **状态必须落盘**：`last_brief.json` 是「完全退出后下次启动补推」的唯一依据，
//!   缺了写入方，补推链路就是空壳（2026-10-27 复核指出的洞）。

use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Duration;

use tauri::menu::{Menu, MenuItem};
use tauri::tray::TrayIconBuilder;
use tauri::{Emitter, Manager, State};
use tauri_plugin_notification::NotificationExt;
use wb_core::{
    brief::{self, BriefVerdict, DeliveryState, QuietHours},
    rules::{self, Ruleset},
    CivilDate, Store, StoreError,
};

/// 应用状态：数据库根目录（登录用户数据目录）在启动时确定，之后只读。
struct AppPaths {
    data_dir: PathBuf,
}

fn err_string(e: StoreError) -> String {
    e.to_string()
}

/// 今天（本机时区的自然日）。时间只由系统层提供（架构 §4.11-4）。
fn today() -> CivilDate {
    let iso = wb_core::store::now_iso_pub();
    CivilDate::parse(&iso[..10]).unwrap_or_else(|| CivilDate::new(2026, 1, 1))
}

/// 当日分钟数（本地时间）。**从系统时钟取，不在领域层各读一遍**。
fn now_minutes() -> u32 {
    let iso = wb_core::store::now_iso_pub();
    let hh: u32 = iso[11..13].parse().unwrap_or(0);
    let mm: u32 = iso[14..16].parse().unwrap_or(0);
    hh * 60 + mm
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

/// 更新检查（架构 §8）：静态 JSON 清单 + Ed25519 验签，零鉴权。
#[tauri::command]
fn verify_update(
    manifest_text: String,
    current_version: String,
    platform: String,
) -> Result<serde_json::Value, String> {
    let manifest = wb_core::update::parse_manifest(&manifest_text).map_err(|e| e.to_string())?;
    let decision = wb_core::update::decide(&current_version, &manifest, &platform);
    let (decision_name, latest, notes) = match decision {
        wb_core::UpdateDecision::UpToDate => ("up_to_date", current_version, None),
        wb_core::UpdateDecision::Available { version, notes, .. } => {
            ("available", version.to_string(), notes)
        }
    };
    Ok(serde_json::json!({
        "decision": decision_name,
        "latest": latest,
        "notes": notes,
        "pub_date": manifest.pub_date,
    }))
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

// ─────────────────────── P0-1：同步命令 ───────────────────────

/// HTTP 传输实现。壳里只放「怎么连」，合并与编排全在 wb-core::sync_client。
struct HttpTransport;

fn http_err(e: ureq::Error) -> wb_core::sync_client::TransportError {
    use wb_core::sync_client::TransportError;
    // ureq 2.x 的错误只有两个变体：Status(u16, Response) 与 Transport(Transport)。
    match e {
        ureq::Error::Status(code, _) => match code {
            401 | 403 => TransportError::Unauthorized,
            409 => TransportError::SchemaMismatch { server: 0 },
            _ => TransportError::Rejected(format!("HTTP {code}")),
        },
        ureq::Error::Transport(t) => {
            use ureq::ErrorKind;
            match t.kind() {
                // 连不上、DNS 失败、超时、TLS 握手失败——一律按「离线」处理，
                // 这样壳会走 push 队列等恢复，而不是把用户踢成「需重新登录」。
                ErrorKind::Dns
                | ErrorKind::ConnectionFailed
                | ErrorKind::Io
                | ErrorKind::TooManyRedirects
                | ErrorKind::ProxyConnect => TransportError::Unreachable(t.to_string()),
                _ => TransportError::Rejected(t.to_string()),
            }
        }
    }
}

impl wb_core::sync_client::Transport for HttpTransport {
    fn login(
        &self,
        base_url: &str,
        username: &str,
        password: &str,
        device_id: &str,
    ) -> Result<wb_core::sync_client::LoginResult, wb_core::sync_client::TransportError> {
        use wb_core::sync_client::TransportError;
        let url = format!("{base_url}/login");
        let resp = ureq::post(&url)
            .timeout(Duration::from_secs(15))
            .send_json(serde_json::json!({
                "username": username,
                "password": password,
                "device_id": device_id,
            }))
            .map_err(http_err)?;
        let v: serde_json::Value = resp.into_json().map_err(|e| TransportError::Rejected(e.to_string()))?;
        if v["ok"].as_bool() != Some(true) {
            return Err(TransportError::Unauthorized);
        }
        Ok(wb_core::sync_client::LoginResult {
            member_id: v["member_id"].as_str().unwrap_or_default().to_string(),
            role: v["role"].as_str().unwrap_or("member").to_string(),
            token: v["token"].as_str().unwrap_or_default().to_string(),
            device_id: v["device_id"].as_str().unwrap_or(device_id).to_string(),
            active_tokens: v["active_tokens"].as_i64().unwrap_or(1),
        })
    }

    fn push(
        &self,
        base_url: &str,
        token: &str,
        rows: &[wb_core::sync::SyncRow],
    ) -> Result<wb_core::sync_client::PushResult, wb_core::sync_client::TransportError> {
        use wb_core::sync_client::TransportError;
        let url = format!("{base_url}/sync/push");
        let resp = ureq::post(&url)
            .timeout(Duration::from_secs(30))
            .set("authorization", &format!("Bearer {token}"))
            .send_json(serde_json::json!({
                "schema_version": wb_core::sync_client::CLIENT_SCHEMA_VERSION,
                "rows": rows,
            }))
            .map_err(http_err)?;
        let v: serde_json::Value = resp.into_json().map_err(|e| TransportError::Rejected(e.to_string()))?;
        let applied = v["applied"]
            .as_array()
            .map(|a| {
                a.iter()
                    .map(|x| wb_core::sync_client::AppliedRow {
                        table: x["table"].as_str().unwrap_or("work_item").to_string(),
                        id: x["id"].as_str().unwrap_or_default().to_string(),
                        decision: x["decision"].as_str().unwrap_or("inserted").to_string(),
                        seq: x["seq"].as_i64().unwrap_or(0),
                    })
                    .collect()
            })
            .unwrap_or_default();
        Ok(wb_core::sync_client::PushResult {
            applied,
            conflicts: v["conflicts"].as_array().map(|a| a.len()).unwrap_or(0) as i64,
            server_seq: v["seq"].as_i64().unwrap_or(0),
        })
    }

    fn pull(
        &self,
        base_url: &str,
        token: &str,
        cursor: i64,
        limit: usize,
    ) -> Result<wb_core::sync_client::PullResult, wb_core::sync_client::TransportError> {
        use wb_core::sync_client::TransportError;
        let url = format!("{base_url}/sync/pull?cursor={cursor}&limit={limit}");
        let resp = ureq::get(&url)
            .timeout(Duration::from_secs(30))
            .set("authorization", &format!("Bearer {token}"))
            .call()
            .map_err(http_err)?;
        let v: serde_json::Value = resp.into_json().map_err(|e| TransportError::Rejected(e.to_string()))?;
        let rows = v["rows"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|x| serde_json::from_value::<wb_core::sync::SyncRow>(x.clone()).ok())
                    .collect()
            })
            .unwrap_or_default();
        Ok(wb_core::sync_client::PullResult {
            rows,
            cursor: v["cursor"].as_i64().unwrap_or(cursor),
            has_more: v["has_more"].as_bool().unwrap_or(false),
            server_seq: v["server_seq"].as_i64().unwrap_or(0),
        })
    }

    fn logout(&self, base_url: &str, token: &str) -> Result<(), wb_core::sync_client::TransportError> {
        let url = format!("{base_url}/logout");
        let _ = ureq::post(&url)
            .timeout(Duration::from_secs(10))
            .set("authorization", &format!("Bearer {token}"))
            .send_json(serde_json::json!({}));
        Ok(())
    }
}

/// 设置页读取同步状态（不含令牌本身）。
#[tauri::command]
fn sync_status(paths: State<Mutex<AppPaths>>) -> Result<wb_core::sync_client::SyncStatus, String> {
    let paths = paths.lock().unwrap();
    let store = open_store(&paths)?;
    Ok(wb_core::sync_client::read_status(store.conn()))
}

/// 保存同步地址（只校验 + 保存，不发起登录）。
#[tauri::command]
fn set_sync_url(paths: State<Mutex<AppPaths>>, base_url: String) -> Result<String, String> {
    let paths = paths.lock().unwrap();
    let store = open_store(&paths)?;
    let url = wb_core::sync_client::validate_base_url(&base_url)?;
    wb_core::sync_client::state_set(store.conn(), wb_core::sync_client::keys::BASE_URL, &url)
        .map_err(err_string)?;
    Ok(url)
}

/// 登录（地址 + 口令 → 设备令牌）。地址不合法在本地就拦下，不发出请求。
#[tauri::command]
fn sync_login(
    paths: State<Mutex<AppPaths>>,
    base_url: String,
    username: String,
    password: String,
) -> Result<wb_core::sync_client::SyncStatus, String> {
    let paths = paths.lock().unwrap();
    let store = open_store(&paths)?;
    let seed = paths.data_dir.to_string_lossy().to_string();
    wb_core::sync_client::login(
        store.conn(),
        &base_url,
        &username,
        &password,
        &seed,
        &HttpTransport,
        &wb_core::store::now_iso_pub(),
    )
    .map_err(|e| e.describe())
}

#[tauri::command]
fn sync_logout(paths: State<Mutex<AppPaths>>) -> Result<wb_core::sync_client::SyncStatus, String> {
    let paths = paths.lock().unwrap();
    let store = open_store(&paths)?;
    wb_core::sync_client::logout(store.conn(), &HttpTransport).map_err(|e| e.describe())
}

/// 立即同步：推空队列 → 拉增量。断网时改动留在队列，恢复后（下次调用或定时）自动补推。
#[tauri::command]
fn sync_now(paths: State<Mutex<AppPaths>>) -> Result<wb_core::sync_client::SyncReport, String> {
    let paths = paths.lock().unwrap();
    let store = open_store(&paths)?;
    let base_url = wb_core::sync_client::state_get(store.conn(), wb_core::sync_client::keys::BASE_URL)
        .unwrap_or_default();
    let token = wb_core::sync_client::state_get(store.conn(), wb_core::sync_client::keys::TOKEN)
        .unwrap_or_default();
    if base_url.is_empty() || token.is_empty() {
        return Err("尚未配置同步地址或未登录".into());
    }
    Ok(wb_core::sync_client::sync_once(
        store.conn(),
        &base_url,
        &token,
        &HttpTransport,
        wb_core::store::now_ms_pub(),
        &wb_core::store::now_iso_pub(),
    ))
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
fn deliver_brief(app: &tauri::AppHandle, paths: &AppPaths) -> Result<serde_json::Value, String> {
    let store = open_store(paths)?;
    let year = today().year;
    let items = wb_core::repo::load_items(store.conn(), year).map_err(err_string)?;
    let rs = Ruleset::default();
    let t = today();
    let decision = brief::decide(t, now_minutes(), &read_delivery(paths));
    let b = rules::morning_brief(t, &items, &rs);
    let (title, body) = brief::notification_text(&b, &decision);

    let suppressed = brief::notification_suppressed(now_minutes(), QuietHours::default());
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
    let mut st = read_delivery(paths);
    st.last_delivered_date = Some(decision.brief_date.clone());
    st.last_attempt_at = Some(wb_core::store::now_iso_pub());
    write_delivery(paths, &st);

    Ok(serde_json::json!({
        "brief_date": decision.brief_date,
        "verdict": format!("{:?}", decision.verdict),
        "late_minutes": decision.late_minutes,
        "notification_suppressed": suppressed,
        "title": title,
        "body": body,
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
        "last_attempt_at": st.last_attempt_at,
        "pending": pending,
        "today": today_iso,
    }))
}

/// 手动触发一次晨报（托盘「查看晨报」/ 应用内按钮），补发时会标明。
#[tauri::command]
fn trigger_brief(app: tauri::AppHandle, paths: State<Mutex<AppPaths>>) -> Result<serde_json::Value, String> {
    let paths = paths.lock().unwrap();
    deliver_brief(&app, &paths)
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
            deliver_brief(&app, &p)
        };
        if let Err(e) = &result {
            let _ = app.emit("brief-error", e.clone());
        } else if let Ok(v) = &result {
            let _ = app.emit("brief-delivered", v.clone());
        }
    });
}

/// 应用启动入口。
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_shell::init())
        .invoke_handler(tauri::generate_handler![
            import_excel,
            list_items,
            kpi_overview,
            alerts,
            units,
            backup_now,
            verify_update,
            add_item,
            sync_status,
            set_sync_url,
            sync_login,
            sync_logout,
            sync_now,
            get_pending_brief,
            trigger_brief,
            mark_brief_delivered,
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
            let sync_item = MenuItem::with_id(app, "sync", "立即同步", true, None::<&str>)?;
            let quit_item = MenuItem::with_id(app, "quit", "退出", true, None::<&str>)?;
            let menu = Menu::with_items(app, &[&open_item, &brief_item, &sync_item, &quit_item])?;

            TrayIconBuilder::with_id("main-tray")
                .icon(app.default_window_icon().unwrap().clone())
                .tooltip("个人工作台")
                .menu(&menu)
                .menu_on_left_click(false)
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
                    "sync" => {
                        if let Some(w) = app.get_webview_window("main") {
                            let _ = w.emit("sync-requested", ());
                        }
                    }
                    "quit" => app.exit(0),
                    _ => {}
                })
                .build(app)?;

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
            spawn_brief_scheduler(handle);
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("个人工作台启动失败");
}
