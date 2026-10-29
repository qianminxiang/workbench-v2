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


def bump_patch(version: str) -> str:
    """把 x.y.z 的最后一段 +1；无法解析时抛错（宁可失败，不静默造一个假版本）。"""
    parts = version.strip().split(".")
    if len(parts) < 3 or not all(p.isdigit() for p in parts):
        raise SystemExit(f"基线版本 {version!r} 不是 x.y.z 形式，无法派生 n3 版本")
    parts[-1] = str(int(parts[-1]) + 1)
    return ".".join(parts)


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
    # n3 的「清单版本」不再写死：基线资产可能是任意已发布版本，断言串由本脚本
    # 按实际写出的 n3_version 派生，避免版本前进后断言腐坏。
    #
    # ⚠ 结构性坑（v2.2.2 修复）：此前工作流传 `--n3-version $WB_BASE_VERSION`，
    # 也就是把清单版本篡改成**基线自己的版本**——签名里的版本与清单宣告版本相等，
    # 插件根本不会走「版本不符」分支，期望串永远不可能出现，n3 必然拿不到终态结果。
    # 现在改为可选：缺省自动取基线 patch+1，并在下面硬校验「必须 != 基线」。
    ap.add_argument("--n3-version", default=None,
                    help="n3 清单里篡改成的版本号；缺省 = 基线 patch+1（必须 != 基线）")
    args = ap.parse_args()

    out = Path(args.out)
    if out.exists():
        shutil.rmtree(out)
    out.mkdir(parents=True)

    real = json.loads(Path(args.manifest).read_text(encoding="utf-8"))

    # n3 只翻清单的 version 字段，url/signature 仍是真实的。
    # 若伪造值与清单真实版本相同，更新插件不会发出
    # 「signed for X but announced X」判定，既不报错也不终止，负例永远等不到终态
    # （2026-10-29 run 40829502144 实测：n3 卡死 → 未产出终态结果文件）。
    # 在夹具侧直接拦下，避免调用方再传成基线版本。
    if args.n3_version == real["version"]:
        raise SystemExit(
            "n3 伪造版本与清单真实版本相同（{}），负例不成立："
            "n3 必须给出与真实签名版本不同的伪造版本".format(real["version"])
        )
    win_real = real["platforms"]["windows-x86_64"]
    base = f"http://127.0.0.1:{args.port}"

    # 自检：清单签名 == .sig 文件原文（实测口径，防止夹具建在错误假设上）
    sig_file = Path(args.sig).read_text(encoding="utf-8").strip()
    if sig_file != win_real["signature"].strip():
        raise SystemExit("清单 signature 与 .sig 文件不一致：夹具口径不成立，停止")

    (out / "real").mkdir()
    shutil.copyfile(args.installer, out / "real" / "win.exe")
    # 正例清单：原样复刻本次运行开始时钉死的那份最新清单（版本号与资产 URL 是同一 tag 的一对）。
    # 存在的理由（2026-10-29 run 40836620836）：正例此前打真实 GitHub 端点，
    # 而 WB_BASE_VERSION 是 step6 按 tag 取到的；若两者之间又发了新版，
    # 插件公告的是新版本、env 里仍是旧版本，断言「未检测到基线版本 X：Y」必炸。
    # 这是夹具时序问题，不是升级链路坏——断言口径不变，只是让被宣告的版本
    # 回到本次运行钉死的那一版。
    write_manifest(out / "real" / "latest.json", real)

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

    # ── n3：篡改清单版本号（必须与签名内绑定的基线版本不同，否则这条红线不成立）──
    base_version = real["version"]
    n3_version = args.n3_version
    if not n3_version:
        n3_version = bump_patch(base_version)
    if n3_version == base_version:
        raise SystemExit(
            f"n3 版本不能等于基线版本 {base_version}：清单宣告与签名绑定一致时，"
            "插件不会进入「版本不符」分支，n3 用例失去意义"
        )
    (out / "n3").mkdir()
    m3 = json.loads(json.dumps(real))
    m3["version"] = n3_version
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
        "base_version": base_version,
        "pinned_manifest_url": f"{base}/real/latest.json",
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
