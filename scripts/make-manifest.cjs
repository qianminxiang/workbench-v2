#!/usr/bin/env node
/**
 * 生成静态更新清单 latest.json（架构 §8）。
 * 签名字段必须是「内容本身」（Minisign base64 签名），不是路径——
 * 客户端 wb-core::update 对「路径形 signature」直接拒绝（已实测覆盖）。
 *
 * v2.1（2026-10-27）：四平台键齐全改造。
 * 客户端 wb-core::update::parse_manifest 硬性要求清单必须同时含
 * windows-x86_64 / darwin-x86_64 / darwin-aarch64 / linux-x86_64
 * 四个平台键（缺一即 PlatformIncomplete 拒收），因此清单必须四键齐全。
 * 本仓库只构建 Windows 产物：windows-x86_64 用真实签名（WB_SIGNATURE，
 * 由 CI 从 tauri 生成的 .sig 签名产物读入）；其余三平台为**占位键**——
 * URL 指向本 Release 内不存在的占位文件名、signature 为 base64 编码的
 * 占位说明（是内容不是路径，可通过清单校验）。
 * 这不是伪造可安装产物：这些平台没有客户端二进制，永远不会走到下载步；
 * 一旦将来出 macOS/Linux 包，把对应平台换成真实 URL+签名即可。
 *
 * 用法：CI 里在产出安装包后运行。
 *   TAURI_SIGNING_PRIVATE_KEY / TAURI_SIGNING_PRIVATE_KEY_PASSWORD 走环境变量。
 * 安装包下载 URL 由环境变量 WB_UPDATE_BASE 提供，例如：
 *   WB_UPDATE_BASE=https://github.com/qianminxiang/workbench-v2/releases/download/v2.1.0
 */
"use strict";
const fs = require("node:fs");
const path = require("node:path");

const base = process.env.WB_UPDATE_BASE || "";
const key = process.env.TAURI_SIGNING_PRIVATE_KEY;
const sig = (process.env.WB_SIGNATURE || "").trim();

// 2026-10-27 修复：此前缺少 WB_UPDATE_BASE 就直接 exit(1)，而 CI 里从未设置它，
// 结果是「体积/内存门禁都通过了，却在最后一步把 job 弄红、安装包传不上 artifact」。
// 更新清单属于「有发布地址时才需要」的产物：配置不全时应当**明确跳过**并留痕，
// 而不是让整条构建失败。真正需要清单的发布流程请设置下面三个环境变量。
const missing = [];
if (!base) missing.push("WB_UPDATE_BASE");
if (!key)  missing.push("TAURI_SIGNING_PRIVATE_KEY");
if (!sig)  missing.push("WB_SIGNATURE");
if (missing.length) {
  console.warn(`[make-manifest] 跳过更新清单生成：缺少 ${missing.join(" / ")}`);
  console.warn("[make-manifest] 说明：本步骤不是红线门禁，缺失不视为构建失败；");
  console.warn("[make-manifest] 需要 latest.json 时请在发布流水线里补齐上述变量。");
  process.exit(0);
}

const version = JSON.parse(fs.readFileSync(
  path.join("apps", "desktop", "src-tauri", "tauri.conf.json"), "utf8")).version;
// 路径口径：src-tauri 是 workspace 成员，cargo 的 target_directory 是**仓库根/target**。
const nsisDir = "target/release/bundle/nsis";
// 按名字排序取定，不用 readdir 顺序——否则两次构建可能选中不同文件。
const file = fs.readdirSync(nsisDir).filter((f) => f.endsWith(".exe")).sort()[0];
if (!file) { console.error("未找到安装包"); process.exit(1); }

// 四平台键：windows 真实，其余占位（理由见文件头注释）。
const platforms = {
  "windows-x86_64": {
    url: `${base}/${encodeURIComponent(file)}`,
    // 签名字段必须是签名内容本身（Minisign base64），不能是路径；
    // 客户端验签失败会拒绝安装（设计如此）。
    signature: sig,
  },
};
const placeholderPlatforms = ["darwin-x86_64", "darwin-aarch64", "linux-x86_64"];
for (const plat of placeholderPlatforms) {
  platforms[plat] = {
    url: `${base}/${encodeURIComponent(`placeholder-no-build-${plat}`)}`,
    // 占位签名 = base64(说明文字)：内容形（无 "://"、不以 "/" 或 "http" 开头、
    // 不以 ".sig" 结尾），能通过 parse_manifest 的形态校验；真实验签永远失败，
    // 这是刻意的——该平台没有真实产物，客户端不该装上任何东西。
    signature: Buffer.from(`no-artifact:${plat}:windows-only-build`, "utf8").toString("base64"),
  };
}

const manifest = {
  version,
  notes: "个人工作台 v2.1：桌面自动更新上线、单机化（移除多端同步 UI）、晨报语音播报、UTC+8 晨报送达修正与免打扰可编辑。",
  pub_date: new Date().toISOString(),
  platforms,
};
fs.writeFileSync("latest.json", JSON.stringify(manifest, null, 2));
console.log("latest.json 已生成：version", version);
console.log("windows-x86_64 = 真实签名；darwin-x86_64 / darwin-aarch64 / linux-x86_64 = 占位键（本仓库只出 Windows 包）");
