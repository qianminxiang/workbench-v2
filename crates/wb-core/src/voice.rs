//! v2.2 语音包偏好（voice_pack 键）归一。
//!
//! 口径：`sync_state` 键值表是唯一事实源，键 `voice_pack`，取值两档：
//! - `system`（默认）：系统 TTS（WebView2 speechSynthesis）朗读实时文案；
//! - `pack`：语音包模式，播 2026-10-28 定稿样音（C 档破阵·浑厚版金属音
//!   系统 TTS 无法合成，必须预录 WAV 随包分发）。
//!
//! 未知值 / 空值一律回落 `system`：宁可退回可用路径，不让脏值把播报打哑。

/// 系统播报（默认档）。
pub const PACK_SYSTEM: &str = "system";
/// 语音包档。
pub const PACK_PACK: &str = "pack";

/// 把偏好原值归一成两档之一；`None`（未设置）与任何未知值都归到 `system`。
pub fn normalize_pack(raw: Option<&str>) -> &'static str {
    match raw {
        Some(PACK_PACK) => PACK_PACK,
        _ => PACK_SYSTEM,
    }
}

/// 是否处于语音包模式。
pub fn is_pack(raw: Option<&str>) -> bool {
    normalize_pack(raw) == PACK_PACK
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 未设置回落系统播报() {
        assert_eq!(normalize_pack(None), PACK_SYSTEM);
        assert!(!is_pack(None));
    }

    #[test]
    fn pack_识别为语音包模式() {
        assert_eq!(normalize_pack(Some("pack")), PACK_PACK);
        assert!(is_pack(Some("pack")));
    }

    #[test]
    fn system_保持系统播报() {
        assert_eq!(normalize_pack(Some("system")), PACK_SYSTEM);
        assert!(!is_pack(Some("system")));
    }

    #[test]
    fn 未知值一律回落系统播报() {
        for raw in ["", "PACK", "Pack", "yes", "1", "  "] {
            assert_eq!(normalize_pack(Some(raw)), PACK_SYSTEM, "raw={raw}");
            assert!(!is_pack(Some(raw)), "raw={raw}");
        }
    }
}
