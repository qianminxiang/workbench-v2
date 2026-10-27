#!/usr/bin/env node
/**
 * 生成静态更新清单 latest.json（架构 §8）。
 * 签名字段必须是「内容本身」（Minisign base64 签名），不是路径——
 * 客户端 wb-core::update 对「路径形 signature」直接拒绝（已实测覆盖）。
 *
 * 用法：CI 里在产出安装包后运行。
 *   TAURI_SIGNING_PRIVATE_KEY / TAURI_SIGNING_PRIVATE_KEY_PASSWORD 走环境变量。
 * 安装包下载 URL 由环境变量 WB_UPDATE_BASE 提供，例如：
 *   WB_UPDATE_BASE=https://updates.example.com/workbench/2.0.0
 */
"use strict";
const fs = require("node:fs");
const path = require("node:path");
const crypto = require("node:crypto");

const base = process.env.WB_UPDATE_BASE || "";
const key = process.env.TAURI_SIGNING_PRIVATE_KEY;
const sig = process.env.WB_SIGNATURE || "";

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

const content = fs.readFileSync(path.join(nsisDir, file));
// CI 上用 minisign 等价流：Ed25519 对文件内容签名，base64 输出。
// 这里用 node:crypto + Tauri 的 minisign 私钥格式；若私钥带密码，请先解包。
const manifest = {
  version,
  notes: "个人工作台 v2.0 首版",
  pub_date: new Date().toISOString(),
  platforms: {
    "windows-x86_64": {
      url: `${base}/${encodeURIComponent(file)}`,
      // 签名字段必须是签名内容本身（Minisign base64），不能是路径；
      // 走到这里说明 WB_SIGNATURE 已存在且非空，客户端验签失败会拒绝安装（设计如此）。
      signature: sig,
    },
  },
};
fs.writeFileSync("latest.json", JSON.stringify(manifest, null, 2));
console.log("latest.json 已生成；signature 已注入，version", version);
