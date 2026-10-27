//! 更新通道：静态 JSON 清单 + Ed25519 验签 + SemVer 比对（架构 §8）。
//!
//! 这条链路里**不存在任何平台凭据**——只有 `HTTPS GET 静态 latest.json`，
//! 因此 v1.x 的 21001（飞书 OpenAPI token 被策略拦截）在架构上不可能复现。
//!
//! 三个高频踩坑点，本模块用代码挡住（架构 §8.2 明列）：
//! 1. `signature` 必须是**签名内容本身**，填路径或 URL 一律判非法（很常见的错法）。
//! 2. 整份清单先校验、再读 version；任一平台键不完整 = 整份清单无效 → 拒绝。
//! 3. 验签失败**必须终止安装**，不回退到「忽略签名」。
//!
//! 密钥纪律（§8.3）：公钥可公开并写入客户端配置；私钥只存在于 CI secret 与离线备份，
//! 任何情况下不进仓库。本模块只出现环境变量**名**，不出现任何密钥**值**。

use ed25519_dalek::{Signature, VerifyingKey};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::fmt;

/// 构建时取用私钥的环境变量名。`.env` 文件不生效，必须走环境变量（§8.3）。
pub const SIGNING_KEY_ENV: &str = "TAURI_SIGNING_PRIVATE_KEY";
/// 私钥密码的环境变量名。
pub const SIGNING_KEY_PASSWORD_ENV: &str = "TAURI_SIGNING_PRIVATE_KEY_PASSWORD";

/// 清单中必须齐全的平台键。缺任何一个都会让整份清单失效（§8.2）。
pub const REQUIRED_PLATFORMS: [&str; 4] =
    ["windows-x86_64", "darwin-x86_64", "darwin-aarch64", "linux-x86_64"];

// ---------------------------------------------------------------------------
// 清单结构
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
struct RawPlatform {
    url: Option<String>,
    signature: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct RawManifest {
    version: Option<String>,
    notes: Option<String>,
    pub_date: Option<String>,
    platforms: Option<BTreeMap<String, RawPlatform>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlatformArtifact {
    pub url: String,
    pub signature: String,
}

#[derive(Debug, Clone)]
pub struct Manifest {
    pub version: Version,
    pub notes: Option<String>,
    pub pub_date: Option<String>,
    pub platforms: BTreeMap<String, PlatformArtifact>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ManifestError {
    Json(String),
    MissingVersion,
    InvalidVersion(String),
    MissingPlatforms,
    /// 平台键不完整——只要有一个平台缺 url/signature，整份清单无效。
    PlatformIncomplete { platform: String, field: &'static str },
    /// `signature` 填成了路径或 URL，这是最常见的一种错法。
    SignatureIsNotContent { platform: String, got: String },
    InvalidPubDate(String),
}

impl fmt::Display for ManifestError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ManifestError::Json(e) => write!(f, "清单不是合法 JSON：{e}"),
            ManifestError::MissingVersion => write!(f, "清单缺 version"),
            ManifestError::InvalidVersion(v) => write!(f, "version 不是合法 SemVer：{v}"),
            ManifestError::MissingPlatforms => write!(f, "清单缺 platforms"),
            ManifestError::PlatformIncomplete { platform, field } => {
                write!(f, "平台 {platform} 缺 {field}，整份清单无效（不能只写当前平台）")
            }
            ManifestError::SignatureIsNotContent { platform, got } => write!(
                f,
                "平台 {platform} 的 signature 填的是路径/URL（{got}）而不是签名内容"
            ),
            ManifestError::InvalidPubDate(p) => write!(f, "pub_date 不符合 RFC 3339：{p}"),
        }
    }
}

/// 解析并**严格校验**整份清单。
///
/// 校验顺序刻意与 Tauri 一致：**先校验整份文件，再读 version**。这样任何一个平台键
/// 不完整都不会被静默放过——留空平台键导致全盘失更是官方文档明写的坑。
pub fn parse_manifest(text: &str) -> Result<Manifest, ManifestError> {
    let raw: RawManifest = serde_json::from_str(text).map_err(|e| ManifestError::Json(e.to_string()))?;

    let platforms_raw = raw.platforms.ok_or(ManifestError::MissingPlatforms)?;

    // 1) 平台完整性：url 与 signature 都必须存在且非空
    let mut platforms = BTreeMap::new();
    for (key, p) in platforms_raw {
        let url = p.url.unwrap_or_default();
        if url.trim().is_empty() {
            return Err(ManifestError::PlatformIncomplete { platform: key, field: "url" });
        }
        let sig = p.signature.unwrap_or_default();
        let sig_trim = sig.trim();
        if sig_trim.is_empty() {
            return Err(ManifestError::PlatformIncomplete { platform: key, field: "signature" });
        }
        // signature 必须是内容：路径 / URL 一律判非法
        if sig_trim.contains("://")
            || sig_trim.ends_with(".sig")
            || sig_trim.starts_with('/')
            || sig_trim.starts_with("http")
        {
            return Err(ManifestError::SignatureIsNotContent { platform: key, got: sig_trim.to_string() });
        }
        platforms.insert(key, PlatformArtifact { url, signature: sig_trim.to_string() });
    }

    if platforms.is_empty() {
        return Err(ManifestError::MissingPlatforms);
    }
    for req in REQUIRED_PLATFORMS {
        if !platforms.contains_key(req) {
            return Err(ManifestError::PlatformIncomplete { platform: req.to_string(), field: "url/signature" });
        }
    }

    // 2) pub_date（存在时须符合 RFC 3339）
    if let Some(pd) = raw.pub_date.as_deref() {
        if !is_rfc3339(pd) {
            return Err(ManifestError::InvalidPubDate(pd.to_string()));
        }
    }

    // 3) 最后才读 version
    let version_raw = raw.version.ok_or(ManifestError::MissingVersion)?;
    let version = Version::parse(&version_raw).ok_or_else(|| ManifestError::InvalidVersion(version_raw.clone()))?;

    Ok(Manifest { version, notes: raw.notes, pub_date: raw.pub_date, platforms })
}

/// 轻量 RFC 3339 检查（够用即可，不引第三方依赖）。
fn is_rfc3339(s: &str) -> bool {
    let b = s.as_bytes();
    if b.len() < 20 {
        return false;
    }
    let digits = |i: usize| b.get(i).map(|c| c.is_ascii_digit()).unwrap_or(false);
    // YYYY-MM-DDTHH:MM:SS
    digits(0) && digits(1) && digits(2) && digits(3) && b[4] == b'-'
        && digits(5) && digits(6) && b[7] == b'-' && digits(8) && digits(9)
        && (b[10] == b'T' || b[10] == b't' || b[10] == b' ')
        && digits(11) && digits(12) && b[13] == b':'
        && digits(14) && digits(15) && b[16] == b':'
        && digits(17) && digits(18)
        && matches!(b[19], b'Z' | b'z' | b'+' | b'-')
}

// ---------------------------------------------------------------------------
// SemVer
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Version {
    pub major: u64,
    pub minor: u64,
    pub patch: u64,
    pub pre: Option<String>,
}

impl Version {
    /// 允许带 `v` 前缀（架构 §8.2：version 可带或不带 v 前缀）。
    pub fn parse(s: &str) -> Option<Version> {
        let t = s.trim().trim_start_matches('v').trim_start_matches('V');
        let (core, pre) = match t.split_once('-') {
            Some((c, p)) => (c, Some(p.to_string())),
            None => (t, None),
        };
        // 构建元数据 +xxx 不影响比较，丢弃
        let core = core.split('+').next().unwrap_or(core);
        let mut it = core.split('.');
        let major = it.next()?.parse().ok()?;
        let minor = it.next().unwrap_or("0").parse().ok()?;
        let patch = it.next().unwrap_or("0").parse().ok()?;
        if it.next().is_some() {
            return None;
        }
        Some(Version { major, minor, patch, pre })
    }

    fn core(&self) -> (u64, u64, u64) {
        (self.major, self.minor, self.patch)
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)?;
        if let Some(p) = &self.pre {
            write!(f, "-{p}")?;
        }
        Ok(())
    }
}

/// SemVer 优先级比较：无 pre 版本 > 带 pre 版本；pre 内逐段比较，纯数字段按数值。
pub fn compare_versions(a: &Version, b: &Version) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    match a.core().cmp(&b.core()) {
        Ordering::Equal => {}
        other => return other,
    }
    match (&a.pre, &b.pre) {
        (None, None) => Ordering::Equal,
        (None, Some(_)) => Ordering::Greater,
        (Some(_), None) => Ordering::Less,
        (Some(x), Some(y)) => {
            let xs: Vec<&str> = x.split('.').collect();
            let ys: Vec<&str> = y.split('.').collect();
            for i in 0..xs.len().max(ys.len()) {
                match (xs.get(i), ys.get(i)) {
                    (None, None) => return Ordering::Equal,
                    (None, Some(_)) => return Ordering::Less,
                    (Some(_), None) => return Ordering::Greater,
                    (Some(p), Some(q)) => {
                        let pn: Option<u64> = p.parse().ok();
                        let qn: Option<u64> = q.parse().ok();
                        let ord = match (pn, qn) {
                            (Some(a), Some(b)) => a.cmp(&b),
                            // 数字段优先级低于字母段
                            (Some(_), None) => Ordering::Less,
                            (None, Some(_)) => Ordering::Greater,
                            (None, None) => p.cmp(q),
                        };
                        if ord != Ordering::Equal {
                            return ord;
                        }
                    }
                }
            }
            Ordering::Equal
        }
    }
}

// ---------------------------------------------------------------------------
// 更新决策
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpdateDecision {
    UpToDate,
    Available {
        version: Version,
        notes: Option<String>,
        artifact: PlatformArtifact,
    },
    /// 拒绝更新。任何拒绝都必须终止流程，绝不「忽略签名继续装」。
    Refused {
        reason: String,
    },
}

/// 决定要不要装这次的更新。
///
/// `target` 形如 `windows-x86_64`。清单里没有该平台 → 拒绝（当前机器装不了）。
pub fn decide(current_version: &str, manifest: &Manifest, target: &str) -> UpdateDecision {
    let cur = match Version::parse(current_version) {
        Some(v) => v,
        None => {
            return UpdateDecision::Refused {
                reason: format!("本机版本号不是合法 SemVer：{current_version}"),
            }
        }
    };
    if compare_versions(&manifest.version, &cur) != std::cmp::Ordering::Greater {
        return UpdateDecision::UpToDate;
    }
    // 更新安装前必须能拿到本平台的包
    match manifest.platforms.get(target) {
        Some(a) => UpdateDecision::Available { version: manifest.version.clone(), notes: manifest.notes.clone(), artifact: a.clone() },
        None => UpdateDecision::Refused { reason: format!("清单中无本平台 {target} 的更新包") },
    }
}

// ---------------------------------------------------------------------------
// Ed25519 验签（Minisign 外层格式）
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VerifyError {
    BadBase64(String),
    BadPubKeyLength(usize),
    BadSignatureLength(usize),
    /// Minisign 大写算法标记（`ED`）= 预哈希模式。
    /// 生产链路由 tauri-plugin-updater 原生支持；本模块**拒绝**而不猜，失败即关闭。
    PrehashedNotSupported,
    UnknownAlgorithm(String),
    KeyIdMismatch { expected: u64, actual: u64 },
    InvalidSignature,
    BadKey(String),
}

impl fmt::Display for VerifyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            VerifyError::BadBase64(e) => write!(f, "base64 解码失败：{e}"),
            VerifyError::BadPubKeyLength(n) => write!(f, "公钥长度异常：{n} 字节"),
            VerifyError::BadSignatureLength(n) => write!(f, "签名长度异常：{n} 字节"),
            VerifyError::PrehashedNotSupported => {
                write!(f, "预哈希模式签名（ED）本模块不支持，拒绝安装")
            }
            VerifyError::UnknownAlgorithm(a) => write!(f, "未知签名算法标记：{a}"),
            VerifyError::KeyIdMismatch { expected, actual } => {
                write!(f, "签名 key id 与本机公钥不符（{actual:016X} ≠ {expected:016X}）")
            }
            VerifyError::InvalidSignature => write!(f, "签名校验不通过"),
            VerifyError::BadKey(e) => write!(f, "公钥无法加载：{e}"),
        }
    }
}

/// 标准 base64（带 padding）。自己实现体量极小，避免给可移植核心再加依赖。
pub fn b64_decode(s: &str) -> Result<Vec<u8>, String> {
    let mut out = Vec::with_capacity(s.len() / 4 * 3);
    let mut buf: u32 = 0;
    let mut bits = 0u32;
    for c in s.bytes() {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            b'=' => break,
            b'\n' | b'\r' | b' ' | b'\t' => continue,
            _ => return Err(format!("非法 base64 字符 {c:#x}")),
        };
        buf = (buf << 6) | v as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buf >> bits) as u8);
        }
    }
    Ok(out)
}

pub fn b64_encode(data: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut s = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        s.push(ALPHABET[((n >> 18) & 63) as usize] as char);
        s.push(ALPHABET[((n >> 12) & 63) as usize] as char);
        s.push(if chunk.len() > 1 { ALPHABET[((n >> 6) & 63) as usize] as char } else { '=' });
        s.push(if chunk.len() > 2 { ALPHABET[(n & 63) as usize] as char } else { '=' });
    }
    s
}

/// 加载公钥。接受三种写法：裸 base64、minisign 公钥文件（含 `untrusted comment:` 行）、
/// 多行文本中的第一个合法 base64 块。
///
/// 公钥是公开物，写进客户端配置即可——它必须**是内容本身**，不能是文件路径（§8.3）。
pub fn load_pubkey(text: &str) -> Result<(VerifyingKey, u64), VerifyError> {
    let b64 = extract_b64_line(text);
    let raw = b64_decode(&b64).map_err(VerifyError::BadBase64)?;
    // minisign 公钥：2 字节算法 + 8 字节 key id + 32 字节公钥
    let (alg, key_id, key_bytes) = if raw.len() == 42 {
        (String::from_utf8_lossy(&raw[0..2]).to_string(), read_u64(&raw[2..10]), &raw[10..42])
    } else if raw.len() == 32 {
        (String::from("Ed"), 0u64, &raw[..32])
    } else {
        return Err(VerifyError::BadPubKeyLength(raw.len()));
    };
    if alg != "Ed" {
        return Err(VerifyError::UnknownAlgorithm(alg));
    }
    let arr: [u8; 32] = key_bytes.try_into().map_err(|_| VerifyError::BadPubKeyLength(key_bytes.len()))?;
    let vk = VerifyingKey::from_bytes(&arr).map_err(|e| VerifyError::BadKey(e.to_string()))?;
    Ok((vk, key_id))
}

/// 验签。`signature` 可以是裸 base64 的 64 字节签名，也可以是 minisign 签名文件全文。
pub fn verify_signature(pubkey_text: &str, signature: &str, content: &[u8]) -> Result<(), VerifyError> {
    let (vk, key_id) = load_pubkey(pubkey_text)?;
    let sig_b64 = extract_b64_line(signature);
    let raw = b64_decode(&sig_b64).map_err(VerifyError::BadBase64)?;

    let (sig_bytes, sig_key_id, alg) = match raw.len() {
        64 => (&raw[..], key_id, "Ed".to_string()),
        74 => {
            let alg = String::from_utf8_lossy(&raw[0..2]).to_string();
            (raw[10..74].as_ref(), read_u64(&raw[2..10]), alg)
        }
        n => return Err(VerifyError::BadSignatureLength(n)),
    };

    if alg == "ED" {
        // 预哈希（Ed25519ph / Blake2b）——本模块不猜，直接拒绝；生产由 tauri-plugin-updater 处理。
        return Err(VerifyError::PrehashedNotSupported);
    }
    if alg != "Ed" {
        return Err(VerifyError::UnknownAlgorithm(alg));
    }
    if sig_key_id != 0 && sig_key_id != key_id {
        return Err(VerifyError::KeyIdMismatch { expected: key_id, actual: sig_key_id });
    }

    let arr: [u8; 64] = sig_bytes.try_into().map_err(|_| VerifyError::BadSignatureLength(sig_bytes.len()))?;
    let sig = Signature::from_bytes(&arr);
    vk.verify_strict(content, &sig).map_err(|_| VerifyError::InvalidSignature)
}

fn read_u64(b: &[u8]) -> u64 {
    let mut a = [0u8; 8];
    a.copy_from_slice(&b[..8]);
    u64::from_le_bytes(a)
}

/// 取文本中第一个"看起来是 base64 载荷"的行（跳过 `untrusted comment:` 一类前缀行）。
fn extract_b64_line(text: &str) -> String {
    for line in text.lines() {
        let t = line.trim();
        if t.is_empty() {
            continue;
        }
        if t.contains(':') || t.starts_with('#') {
            continue;
        }
        return t.to_string();
    }
    text.trim().replace(['\n', '\r'], "")
}

// ---------------------------------------------------------------------------
// 测试
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::{Signer, SigningKey};

    const GOOD_MANIFEST: &str = r#"{
      "version": "2.0.1",
      "notes": "修复备份轮转边界",
      "pub_date": "2026-10-26T10:00:00Z",
      "platforms": {
        "windows-x86_64": { "url": "https://dl.example.com/wb_2.0.1_x64-setup.exe", "signature": "AAA" },
        "darwin-x86_64":  { "url": "https://dl.example.com/wb_2.0.1_x64.app.tar.gz", "signature": "AAA" },
        "darwin-aarch64": { "url": "https://dl.example.com/wb_2.0.1_aarch64.app.tar.gz", "signature": "AAA" },
        "linux-x86_64":   { "url": "https://dl.example.com/wb_2.0.1_amd64.AppImage", "signature": "AAA" }
      }
    }"#;

    fn manifest_with(mut f: impl FnMut(&mut serde_json::Value)) -> String {
        let mut v: serde_json::Value = serde_json::from_str(GOOD_MANIFEST).unwrap();
        f(&mut v);
        serde_json::to_string(&v).unwrap()
    }

    #[test]
    fn complete_manifest_parses() {
        let m = parse_manifest(GOOD_MANIFEST).unwrap();
        assert_eq!(m.version.to_string(), "2.0.1");
        assert_eq!(m.platforms.len(), 4);
    }

    #[test]
    fn missing_platform_key_invalidates_whole_manifest() {
        let text = manifest_with(|v| {
            v["platforms"].as_object_mut().unwrap().remove("linux-x86_64");
        });
        match parse_manifest(&text) {
            Err(ManifestError::PlatformIncomplete { platform, .. }) => assert_eq!(platform, "linux-x86_64"),
            other => panic!("应判整份清单无效，实际 {other:?}"),
        }
    }

    #[test]
    fn signature_as_path_is_rejected() {
        let text = manifest_with(|v| {
            v["platforms"]["windows-x86_64"]["signature"] =
                serde_json::json!("https://dl.example.com/wb_2.0.1.sig");
        });
        assert!(matches!(parse_manifest(&text), Err(ManifestError::SignatureIsNotContent { .. })));
    }

    #[test]
    fn bad_pub_date_rejected() {
        let text = manifest_with(|v| v["pub_date"] = serde_json::json!("2026/10/26 10:00"));
        assert!(matches!(parse_manifest(&text), Err(ManifestError::InvalidPubDate(_))));
    }

    #[test]
    fn semver_ordering() {
        let p = |s: &str| Version::parse(s).unwrap();
        assert_eq!(compare_versions(&p("2.0.1"), &p("v2.0.0")), std::cmp::Ordering::Greater);
        assert_eq!(compare_versions(&p("2.0.0"), &p("2.0.0")), std::cmp::Ordering::Equal);
        assert_eq!(compare_versions(&p("2.0.0"), &p("2.0.0-rc.1")), std::cmp::Ordering::Greater);
        assert_eq!(compare_versions(&p("2.0.0-rc.2"), &p("2.0.0-rc.1")), std::cmp::Ordering::Greater);
        assert_eq!(compare_versions(&p("2.0.0-beta"), &p("2.0.0-rc.1")), std::cmp::Ordering::Less);
    }

    #[test]
    fn decision_available_up_to_date_and_refused() {
        let m = parse_manifest(GOOD_MANIFEST).unwrap();
        assert!(matches!(decide("2.0.0", &m, "windows-x86_64"), UpdateDecision::Available { .. }));
        assert_eq!(decide("2.0.1", &m, "windows-x86_64"), UpdateDecision::UpToDate);
        assert_eq!(decide("2.0.2", &m, "windows-x86_64"), UpdateDecision::UpToDate);
        assert!(matches!(decide("2.0.0", &m, "android-arm64"), UpdateDecision::Refused { .. }));
    }

    #[test]
    fn real_ed25519_roundtrip_verifies_and_tamper_fails() {
        // 真实密钥对：私钥仅存在于测试内，模拟「签名 → 分发 → 客户端验签」全链路
        let sk = SigningKey::from_bytes(&[7u8; 32]);
        let vk = sk.verifying_key();
        let mut pubkey_raw = Vec::new();
        pubkey_raw.extend_from_slice(b"Ed");
        pubkey_raw.extend_from_slice(&0x1234_5678_9abc_def0u64.to_le_bytes());
        pubkey_raw.extend_from_slice(&vk.to_bytes());
        let pubkey_text = format!("untrusted comment: minisign public key 1234\n{}", b64_encode(&pubkey_raw));

        let content = b"fake-updater-archive-bytes".as_slice();
        let sig = sk.sign(content);
        let mut sig_raw = Vec::new();
        sig_raw.extend_from_slice(b"Ed");
        sig_raw.extend_from_slice(&0x1234_5678_9abc_def0u64.to_le_bytes());
        sig_raw.extend_from_slice(&sig.to_bytes());
        let sig_text = format!("untrusted comment: signature\n{}", b64_encode(&sig_raw));

        assert_eq!(verify_signature(&pubkey_text, &sig_text, content), Ok(()));

        // 内容被改一个字节 → 必须失败
        let mut tampered = content.to_vec();
        tampered[0] ^= 0x01;
        assert_eq!(verify_signature(&pubkey_text, &sig_text, &tampered), Err(VerifyError::InvalidSignature));

        // key id 不符 → 明确拒绝（不是含糊的"校验不通过"）
        let mut other_key_id = Vec::new();
        other_key_id.extend_from_slice(b"Ed");
        other_key_id.extend_from_slice(&1u64.to_le_bytes());
        other_key_id.extend_from_slice(&sig.to_bytes());
        assert!(matches!(
            verify_signature(&pubkey_text, &b64_encode(&other_key_id), content),
            Err(VerifyError::KeyIdMismatch { .. })
        ));
    }

    #[test]
    fn prehashed_signature_is_refused_not_guessed() {
        let sk = SigningKey::from_bytes(&[9u8; 32]);
        let vk = sk.verifying_key();
        let mut pk = Vec::new();
        pk.extend_from_slice(b"Ed");
        pk.extend_from_slice(&0u64.to_le_bytes());
        pk.extend_from_slice(&vk.to_bytes());
        let content = b"x";
        let sig = sk.sign(content);
        let mut sr = Vec::new();
        sr.extend_from_slice(b"ED"); // 大写 = 预哈希
        sr.extend_from_slice(&0u64.to_le_bytes());
        sr.extend_from_slice(&sig.to_bytes());
        assert_eq!(
            verify_signature(&b64_encode(&pk), &b64_encode(&sr), content),
            Err(VerifyError::PrehashedNotSupported)
        );
    }

    #[test]
    fn base64_roundtrip() {
        for n in 0..40usize {
            let data: Vec<u8> = (0..n).map(|i| (i * 37 % 256) as u8).collect();
            let enc = b64_encode(&data);
            assert_eq!(b64_decode(&enc).unwrap(), data, "长度 {n}");
        }
    }

    #[test]
    fn pubkey_as_path_is_not_a_valid_key() {
        // 公钥必须是内容本身，不能是文件路径——路径解不出 32/42 字节
        assert!(load_pubkey("/etc/keys/workbench.pub").is_err());
    }
}
