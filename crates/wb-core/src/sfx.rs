//! 交互音效采样白名单（v2.2.2 需求15）。
//!
//! 「点击音不是红警」的根因是音频写死在 WebAudio 合成里，风格无法替换。改法是：
//! 采样文件放在前端 `sfx/` 目录，**采样 id 落到数据库偏好**，播放层只认白的 id。
//! 归一规则集中在这里（可单测），Tauri 层只负责取键、写键。
//!
//! 不校验具体风格——红警采样由用户逐条确认后替换同名文件即可，无需改后端。

/// 点击音可选采样 id（对应 apps/desktop/src/sfx/{id}.wav）。
pub const TICK_SAMPLES: [&str; 3] = ["tick-a", "tick-b", "tick-c"];

/// 默认点击音采样。
pub const DEFAULT_SAMPLE: &str = "tick-a";

/// 判断 id 是否为已知采样。未知值（旧版脏值、前端乱传）由调用方回落默认。
pub fn is_known_sample(id: &str) -> bool {
    TICK_SAMPLES.contains(&id)
}

/// 归一化采样 id：已知值原样返回，其余回落默认。
pub fn normalize_sample(raw: Option<&str>) -> String {
    match raw {
        Some(v) if is_known_sample(v) => v.to_string(),
        _ => DEFAULT_SAMPLE.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_sample_is_known() {
        assert!(is_known_sample(DEFAULT_SAMPLE));
        assert!(TICK_SAMPLES.contains(&DEFAULT_SAMPLE));
    }

    #[test]
    fn known_samples_pass_through() {
        for s in TICK_SAMPLES {
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
    fn sample_ids_have_no_path_separators() {
        // 采样 id 会被拼进 `./sfx/{id}.wav`，必须挡住路径穿越字符。
        for s in TICK_SAMPLES {
            assert!(!s.contains('/') && !s.contains('\\') && !s.contains('.'));
        }
    }
}
