//! 自动更新端到端验证驱动（E2E-only，`--features e2e-update` 才编译）。
//!
//! 发布包（v2.1.0 Release 资产）不含本模块：`Cargo.toml` 里它是可选 feature，
//! 默认构建不打开，所以发布流水线 `cargo tauri build` 的产物与本文件无关。
//!
//! 职责：按环境变量驱动**真实的** tauri-plugin-updater 链路
//! （check → download → verify → install），把每一步的机器可读结果落成 JSON 后退出。
//! 不复制插件任何逻辑，全部调用插件公开 API，因此「验签 / 私钥内版本绑定 /
//! ShellExecuteW 拉起 NSIS 安装器」这些行为与线上完全一致。
//!
//! 环境变量：
//! - `WB_E2E_RESULT`：结果 JSON 路径（必填；缺省则本模块不介入，走原静默检查）
//! - `WB_E2E_ENDPOINT`：覆盖清单地址；留空 = 走 tauri.conf.json 里配置的真实
//!   GitHub 地址（正例即如此，用来证明普通用户匿名环境可完成）

use std::io::Write as _;

use serde_json::{json, Value};
use tauri::Manager;
use tauri_plugin_updater::UpdaterExt;

fn result_path() -> Option<String> {
    std::env::var("WB_E2E_RESULT")
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

fn write_json(path: &str, v: &Value) {
    let p = std::path::Path::new(path);
    if let Some(dir) = p.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(mut f) = std::fs::File::create(p) {
        let _ = f.write_all(serde_json::to_string_pretty(v).unwrap_or_default().as_bytes());
        let _ = f.flush();
        let _ = f.sync_all();
    }
}

fn finish(stage: &str, ok: bool, error: Option<String>, extra: &Value) -> ! {
    let mut v = json!({ "stage": stage, "ok": ok });
    if let Some(e) = error {
        v["error"] = Value::String(e);
    }
    if let Some(obj) = extra.as_object() {
        for (k, val) in obj {
            v[k.clone()] = val.clone();
        }
    }
    if let Some(path) = result_path() {
        write_json(&path, &v);
    }
    eprintln!("[e2e] {v}");
    // 退出码恒为 0：判定交给 CI 读 JSON，避免把「预期拒绝」与「程序崩溃」混为一谈。
    std::process::exit(0);
}

pub async fn run_once(app: tauri::AppHandle) {
    // ── P0 时区回归（v2.2.3）：独立模式，不进更新链路 ──
    // WB_E2E_EXPECT_LOCAL_DATE=yyyy-MM-dd 时：计算本机时区下的「今天」（brief 同源），
    // 与外部独立推算的期望值比对后退出。修复前 `local_minus_utc()` 的**秒**被当
    // **分钟**用，UTC+8 下 28800 分钟 = 整 20 天，所有日期 +20 天；CI runner 默认
    // UTC（偏移 0）不会显形，因此驱动侧只报值，非 UTC 时区由 workflow 先 Set-TimeZone。
    if let Ok(expected) = std::env::var("WB_E2E_EXPECT_LOCAL_DATE") {
        let expected = expected.trim().to_string();
        let (d, mins) = super::now_local();
        let got = format!("{:04}-{:02}-{:02}", d.year, d.month, d.day);
        let ok = !expected.is_empty() && got == expected;
        let extra = json!({
            "local_date": got,
            "expected_local_date": expected,
            "local_minutes": mins,
            "utc_iso": wb_core::store::now_iso_pub(),
            "offset_minutes": super::offset_seconds_to_minutes(
                chrono::Local::now().offset().local_minus_utc()),
        });
        let err = if ok {
            None
        } else {
            Some(format!("本地日期({got}) != 期望日期({expected})：偏移换算疑似回归（修复前 UTC+8 会 +20 天）"))
        };
        finish("tz_check", ok, err, &extra);
    }

    // ── 重入保护（防御性）──
    // install() 在 Windows 上会用 NSIS `/R /ARGS` 重新拉起本程序，环境变量随之继承。
    // 若新拉起的仍是本 E2E 二进制，它会再跑一遍更新。判据：结果文件里已有
    // installer_launched=true，说明这是「安装完成后的那次启动」，只记录版本后退出。
    // 正例里最终装上的是**线上 2.1.0 正式包**（不含本模块），所以这条只作兜底。
    if let Some(path) = result_path() {
        if let Ok(text) = std::fs::read_to_string(&path) {
            if let Ok(mut prev) = serde_json::from_str::<Value>(&text) {
                if prev["installer_launched"] == Value::Bool(true) {
                    prev["relaunch_version"] = json!(app.package_info().version.to_string());
                    prev["stage"] = json!("relaunched");
                    write_json(&path, &prev);
                    eprintln!("[e2e] relaunched as {}", app.package_info().version);
                    std::process::exit(0);
                }
            }
        }
    }

    let mut builder = app.updater_builder();

    if let Ok(raw) = std::env::var("WB_E2E_ENDPOINT") {
        let raw = raw.trim().to_string();
        if !raw.is_empty() {
            let url = match tauri::Url::parse(&raw) {
                Ok(u) => u,
                Err(e) => finish("endpoint", false, Some(format!("parse {raw}: {e}")), &Value::Null),
            };
            match builder.endpoints(vec![url]) {
                Ok(b) => builder = b,
                Err(e) => finish("endpoint", false, Some(e.to_string()), &Value::Null),
            }
        }
    }

    let updater = match builder.build() {
        Ok(u) => u,
        Err(e) => finish("build", false, Some(e.to_string()), &Value::Null),
    };

    // ── 1. 检查清单 ──
    let update = match updater.check().await {
        Ok(Some(u)) => u,
        Ok(None) => finish("check", false, Some("no-update-available".into()), &Value::Null),
        Err(e) => finish("check", false, Some(e.to_string()), &Value::Null),
    };

    let ctx = json!({
        "current_version": update.current_version,
        "announced_version": update.version,
        "download_url": update.download_url.to_string(),
    });

    // ── 2. 下载（插件在 download 内部就验签，篡改安装包/签名在此暴露）──
    let bytes = match update.download(|_, _| {}, || {}).await {
        Ok(b) => b,
        Err(e) => finish("download", false, Some(e.to_string()), &ctx),
    };

    // ── 3. 对同一份字节再显式验签一次（独立断言「验签通过」）──
    if let Err(e) = update.verify(&bytes) {
        finish("verify", false, Some(e.to_string()), &ctx);
    }

    // Windows 上 install() 成功会直接 std::process::exit(0)（插件用 ShellExecuteW
    // 拉起安装器后退出本进程），所以「安装器已拉起」必须在调用前落盘。
    let mut pre = ctx.clone();
    pre["downloaded_bytes"] = json!(bytes.len());
    pre["verified"] = json!(true);
    pre["installer_launched"] = json!(true);
    if let Some(path) = result_path() {
        write_json(&path, &pre);
    }

    match update.install(&bytes) {
        Ok(()) => finish("install", true, None, &pre),
        Err(e) => {
            let mut bad = pre.clone();
            bad["installer_launched"] = json!(false);
            finish("install", false, Some(e.to_string()), &bad)
        }
    }
}
