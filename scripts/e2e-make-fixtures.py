#!/usr/bin/env python3
"""构造自动更新 E2E 的负例夹具（只在 CI 里跑，产物是本地副本，绝不改动发布物）。

输入（全部由 CI 匿名下载得到的真实发布物）：
  --manifest   真实 latest.json（v2.1.0 Release 资产）
  --installer  真实 NSIS 安装包 workbench_2.1.0_x64-setup.exe
  --sig        真实签名 workbench_2.1.0_x64-setup.exe.sig
  --out        输出目录（http.server 直接对外提供）
  --port       本地 http 端口（写进清单 url）

关于签名编码（实测，非推测）：
  `.sig` 文件 ≠ minisign 明文。它整段是 base64，解出来才是 4 行 minisign 文本：
    untrusted comment / base64(74B: 算法+key_id+签名) / trusted comment(tab 分隔) / base64(64B 全局签名)
  清单的 `signature` 字段就是 `.sig` 文件的原文（实测逐字节相等）。
  插件 verify_signature() 先 base64_to_string(签名) 再 Signature::decode()，所以篡改必须
  在「解码后的文本」这一层做，再做整体 base64 —— 否则会落到 InvalidEncoding，
  失败原因就不再是「签名被改」了。

输出（每个负例只改动一个维度，失败原因可唯一归因）：
  real/win.exe        真实安装包副本（未改动）
  n1/tampered.exe     安装包内容篡改（中间 1 字节翻转）
  n1/latest.json      url → tampered.exe，signature 真实
  n2/latest.json      signature 篡改（全局签名最后一字节翻转；长度与 key_id 不变）
  n3/latest.json      version 改成 --n3-version（url/signature 仍是真实基线版本）
  n4/latest.json      url 指向不存在的资产（清单 URL 篡改）
"""
import argparse
import base64
import json
import shutil
from pathlib import Path


def write_manifest(path: Path, data: dict) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(data, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")


def corrupt_signature(sig_field: str) -> str:
    """把「全局签名」的最后一个字节翻转，其余完全不动。

    sig_field 是清单里的 signature：整段 base64(4 行 minisign 文本)。
    解出文本后只改第 4 行（对 trusted comment 的全局签名）的最后一字节：
      · base64 长度仍是 64 字节 → Signature::decode 通过（不会落成 InvalidEncoding）
      · 第 2 行的 key_id 原封不动 → 不会落成 UnexpectedKeyId
      · 于是必然落在 InvalidSignature —— 原因唯一可归因到「签名被改」
    """
    text = base64.b64decode(sig_field.strip()).decode("utf-8")
    lines = [ln for ln in text.replace("\r\n", "\n").split("\n") if ln.strip()]
    if len(lines) != 4:
        raise SystemExit(f"minisign 文本应为 4 行，实际 {len(lines)} 行")
    raw = bytearray(base64.b64decode(lines[3]))
    if len(raw) != 64:
        raise SystemExit(f"全局签名应为 64 字节，实际 {len(raw)}")
    raw[-1] ^= 0xFF
    lines[3] = base64.b64encode(bytes(raw)).decode("ascii")
    mutated = "\n".join(lines) + "\n"
    return base64.b64encode(mutated.encode("utf-8")).decode("ascii")


def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("--manifest", required=True)
    ap.add_argument("--installer", required=True)
    ap.add_argument("--sig", required=True)
    ap.add_argument("--out", required=True)
    ap.add_argument("--port", required=True, type=int)
    # n3 的「清单版本」不再写死：基线资产可能是任意已发布版本，
    # 断言串由工作流按 fixtures.json 里的 n3_version 拼，避免版本前进后断言腐坏。
    ap.add_argument("--n3-version", required=True,
                    help="n3 清单里篡改成的版本号（应 != 基线版本，通常取当前 latest）")
    args = ap.parse_args()

    out = Path(args.out)
    if out.exists():
        shutil.rmtree(out)
    out.mkdir(parents=True)

    real = json.loads(Path(args.manifest).read_text(encoding="utf-8"))
    win_real = real["platforms"]["windows-x86_64"]
    base = f"http://127.0.0.1:{args.port}"

    # 自检：清单签名 == .sig 文件原文（实测口径，防止夹具建在错误假设上）
    sig_file = Path(args.sig).read_text(encoding="utf-8").strip()
    if sig_file != win_real["signature"].strip():
        raise SystemExit("清单 signature 与 .sig 文件不一致：夹具口径不成立，停止")

    (out / "real").mkdir()
    shutil.copyfile(args.installer, out / "real" / "win.exe")

    # ── n1：篡改安装包内容 ──
    blob = bytearray(Path(args.installer).read_bytes())
    mid = len(blob) // 2
    original = blob[mid]
    blob[mid] ^= 0xFF
    (out / "n1").mkdir()
    (out / "n1" / "tampered.exe").write_bytes(bytes(blob))
    m1 = json.loads(json.dumps(real))
    m1["platforms"]["windows-x86_64"]["url"] = f"{base}/n1/tampered.exe"
    write_manifest(out / "n1" / "latest.json", m1)

    # ── n2：篡改签名 ──
    (out / "n2").mkdir()
    m2 = json.loads(json.dumps(real))
    m2["platforms"]["windows-x86_64"]["url"] = f"{base}/real/win.exe"
    m2["platforms"]["windows-x86_64"]["signature"] = corrupt_signature(win_real["signature"])
    write_manifest(out / "n2" / "latest.json", m2)
    if m2["platforms"]["windows-x86_64"]["signature"] == win_real["signature"]:
        raise SystemExit("n2 篡改未生效")

    # ── n3：篡改清单版本号 ──
    (out / "n3").mkdir()
    m3 = json.loads(json.dumps(real))
    m3["version"] = args.n3_version
    m3["platforms"]["windows-x86_64"]["url"] = f"{base}/real/win.exe"
    write_manifest(out / "n3" / "latest.json", m3)

    # ── n4：篡改清单 URL（指向不存在的资产）──
    (out / "n4").mkdir()
    m4 = json.loads(json.dumps(real))
    m4["platforms"]["windows-x86_64"]["url"] = f"{base}/n4/does-not-exist.exe"
    write_manifest(out / "n4" / "latest.json", m4)

    report = {
        "installer_bytes": len(blob),
        "tampered_offset": mid,
        "tampered_original_byte": f"0x{original:02x}",
        "tampered_new_byte": "0x{:02x}".format(blob[mid]),
        "sig_is_base64_of_minisign_text": True,
        "base_version": win_real["version"] if "version" in win_real else real["version"],
        "n3_version": m3["version"],
        # 断言串在脚本侧拼好（唯一来源），工作流不再自己写版本字面量
        "n3_expect": (
            "signed for version {signed} but the update endpoint announced version {announced}"
        ).format(signed=real["version"], announced=m3["version"]),
        "n4_url": m4["platforms"]["windows-x86_64"]["url"],
    }
    (out / "fixtures.json").write_text(
        json.dumps(report, ensure_ascii=False, indent=2) + "\n", encoding="utf-8"
    )
    print(json.dumps(report, ensure_ascii=False))


if __name__ == "__main__":
    main()
