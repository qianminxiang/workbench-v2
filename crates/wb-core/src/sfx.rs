//! 交互音效采样白名单（v2.2.2 需求15，v2.2.4 起对齐冻结版 9 条音效）。
//!
//! 「点击音不是红警」的根因是音频写死在 WebAudio 合成里，风格无法替换。改法是：
//! 采样文件放在前端 `sfx/` 目录，**采样 id 落到数据库偏好**，播放层只认白的 id。
//! 归一规则集中在这里（可单测），Tauri 层只负责取键、写键。
//!
//! v2.2.4（UI 规范第三部分路线 c+a 定稿 v1 冻结版）：音效从「3 个可换点击样本」
//! 升级为**用户逐条确认的 9 条冻结音效**（ID 即文件名）。旧偏好里的 tick-a/b/c
//! 等值按别名回落 ui-click，不报错、不丢偏好。

/// 点击音可选采样 id（对应 apps/desktop/src/sfx/{id}.wav）。
/// 冻结版只保留 ui-click 一条（用户确认的「沉重金属按键咔哒 + 机械咬合 + 短促余音」），
/// 数组形态保持不变以免调用方大改。
pub const TICK_SAMPLES: [&str; 1] = ["ui-click"];

/// 全部冻结音效 id（9 条，规范第三部分用户确认版 v1）。
pub const SFX_IDS: [&str; 9] = [
    "ui-gate-close",
    "ui-gate-open",
    "ui-click",
    "ui-confirm",
    "ui-cancel",
    "ui-alert-overdue",
    "ui-alert-due",
    "ui-import-done",
    "ui-report-in",
];

/// 默认点击音采样。
pub const DEFAULT_SAMPLE: &str = "ui-click";

/// 旧版本采样 id 的别名（v2.2.2 的 tick-a/b/c 与 airlock-close/open、alert-1/2）。
/// 这些 id 在 v2.2.4 已不存在；读到旧偏好时映射到最接近的冻结音效，避免「偏好丢失」。
pub const LEGACY_ALIASES: [(&str, &str); 7] = [
    ("tick-a", "ui-click"),
    ("tick-b", "ui-click"),
    ("tick-c", "ui-click"),
    ("airlock-close", "ui-gate-close"),
    ("airlock-open", "ui-gate-open"),
    ("alert-1", "ui-alert-due"),
    ("alert-2", "ui-alert-overdue"),
];

/// 判断 id 是否为已知采样（冻结版 9 条 + 旧别名）。
pub fn is_known_sample(id: &str) -> bool {
    TICK_SAMPLES.contains(&id) || SFX_IDS.contains(&id) || LEGACY_ALIASES.iter().any(|(k, _)| *k == id)
}

/// 归一化采样 id：新 id 原样返回、旧 id 走别名、其余回落默认。
pub fn normalize_sample(raw: Option<&str>) -> String {
    match raw {
        Some(v) if SFX_IDS.contains(&v) => v.to_string(),
        Some(v) => match LEGACY_ALIASES.iter().find(|(k, _)| *k == v) {
            Some((_, to)) => to.to_string(),
            None => DEFAULT_SAMPLE.to_string(),
        },
        None => DEFAULT_SAMPLE.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_sample_is_known() {
        assert!(is_known_sample(DEFAULT_SAMPLE));
        assert!(SFX_IDS.contains(&DEFAULT_SAMPLE));
    }

    #[test]
    fn known_samples_pass_through() {
        for s in TICK_SAMPLES {
            assert_eq!(normalize_sample(Some(s)), s);
        }
        // 9 条冻结音效全部可归一（不回落默认）
        for s in SFX_IDS {
            assert_eq!(normalize_sample(Some(s)), s);
        }
    }

    #[test]
    fn unknown_and_empty_fall_back_to_default() {
        assert_eq!(normalize_sample(None), DEFAULT_SAMPLE);
        assert_eq!(normalize_sample(Some("")), DEFAULT_SAMPLE);
        assert_eq!(normalize_sample(Some("tick-z")), DEFAULT_SAMPLE);
        assert_eq!(normalize_sample(Some("../tick-a")), DEFAULT_SAMPLE);
    }

    #[test]
    fn legacy_ids_map_to_frozen_sounds() {
        // v2.2.2 的三样本与气闸/提示旧 id 必须映射到冻结版，不丢用户偏好
        assert_eq!(normalize_sample(Some("tick-a")), "ui-click");
        assert_eq!(normalize_sample(Some("tick-b")), "ui-click");
        assert_eq!(normalize_sample(Some("tick-c")), "ui-click");
        assert_eq!(normalize_sample(Some("airlock-close")), "ui-gate-close");
        assert_eq!(normalize_sample(Some("airlock-open")), "ui-gate-open");
        assert_eq!(normalize_sample(Some("alert-1")), "ui-alert-due");
        assert_eq!(normalize_sample(Some("alert-2")), "ui-alert-overdue");
    }

    #[test]
    fn frozen_id_set_is_nine_and_unique() {
        assert_eq!(SFX_IDS.len(), 9);
        let mut seen = std::collections::HashSet::new();
        for s in SFX_IDS {
            assert!(seen.insert(s), "冻结音效 id 重复：{s}");
        }
    }

    #[test]
    fn sample_ids_have_no_path_separators() {
        // 采样 id 会被拼进 `./sfx/{id}.wav`，必须挡住路径穿越字符。
        for s in SFX_IDS {
            assert!(!s.contains('/') && !s.contains('\\') && !s.contains('.'));
        }
    }
}
