#!/usr/bin/env python3
"""WAV 头完整性门禁：拦「流式写入器把长度字段留成 0xFFFFFFFF」。

## 为什么需要这道门

入包的 18 个 WAV（9 条 UI 音效 + 9 条语音播报）当初由流式写入器产出，
RIFF 头里的两个长度字段没被回填，留成了哨兵值 0xFFFFFFFF：

- `RIFF` size（offset 4..7）应为 `filelen - 8`，实际是 `0xFFFFFFFF`
- `data` chunk size（`data` 块头 offset+4..+7）应为 payload 字节数，实际是 `0xFFFFFFFF`

结果是严格的 RIFF 解析器（Python `wave`、Windows Media Foundation）会把这个
文件读成 `0xFFFFFFFF / block_align` 帧。16bit 立体声 block_align = 4，
于是得到 1,073,741,823 帧 ≈ 26,843.5 秒 ≈ 7.46 小时的时长；
Chromium / ffmpeg 这类宽容解析器不报错，所以**播放听起来是正常的，缺陷只在
严格解析器上暴露**——这正是它能一路进包、没人发现的原因。

`data` 只改 8 个字节就能修好，payload 一个字节不用动（本项目实测：9 条语音
全部修完 payload 逐字节一致，时长从「26843.5s」恢复为真实的 3.28~13.00s）。

## 门禁口径

对 `apps/desktop/src/sfx/*.wav` 与 `apps/desktop/src/voice/*.wav` 逐个走 chunk：

1. 文件头必须是 `RIFF` + `WAVE`
2. `RIFF` size 必须等于 `filelen - 8`（不允许哨兵值）
3. 每个 chunk 的 `size` 不得超过其后可用字节数（不允许越界声明）
4. chunk 链必须正好走到文件末尾（不允许多余尾字节 / 提前中断）
5. 必须同时存在 `fmt ` 与 `data`

任一条不满足即非 0 退出，并把文件、字段、期望值与实际值打印出来，
方便直接照着手工回填。纯标准库，不需要装任何依赖。
"""

import os
import struct
import sys

PLACEHOLDER = 0xFFFFFFFF
ROOT = os.path.join("apps", "desktop", "src")
DIRS = ("sfx", "voice")


def check_file(path):
    problems = []
    with open(path, "rb") as f:
        data = f.read()
    n = len(data)

    if data[0:4] != b"RIFF":
        return [f"文件头不是 'RIFF'（实际 {data[0:4]!r}）"]
    if data[8:12] != b"WAVE":
        return [f"form 不是 'WAVE'（实际 {data[8:12]!r}）"]

    riff = struct.unpack("<I", data[4:8])[0]
    if riff == PLACEHOLDER:
        problems.append(
            f"RIFF size 是哨兵值 0xFFFFFFFF，应为 {n - 8}（流式写入器忘了回填）")
    elif riff != n - 8:
        problems.append(f"RIFF size = {riff}，应为 {n - 8}")

    off, seen = 12, set()
    while off + 8 <= n:
        cid = data[off:off + 4]
        csz = struct.unpack("<I", data[off + 4:off + 8])[0]
        avail = n - (off + 8)
        seen.add(cid.decode("latin1", "replace"))
        if csz == PLACEHOLDER:
            problems.append(
                f"chunk {cid.decode('latin1', 'replace')!r} @{off} 的 size 是哨兵值 "
                f"0xFFFFFFFF，应为 {avail}")
            break
        if csz > avail:
            problems.append(
                f"chunk {cid.decode('latin1', 'replace')!r} @{off} 声明 {csz} 字节，"
                f"实际只剩 {avail}")
            break
        off += 8 + csz + (csz & 1)

    missing = {"fmt ", "data"} - seen
    if missing:
        problems.append(f"缺少必需 chunk: {sorted(missing)}")
    if not problems and off != n:
        problems.append(f"chunk 链在 {off} 结束，文件长度 {n}（尾部有 {n - off} 字节无法解释）")
    return problems


def main():
    bad = 0
    total = 0
    for d in DIRS:
        base = os.path.join(ROOT, d)
        if not os.path.isdir(base):
            print(f"跳过不存在的目录: {base}")
            continue
        for name in sorted(os.listdir(base)):
            if not name.lower().endswith(".wav"):
                continue
            total += 1
            p = os.path.join(base, name)
            problems = check_file(p)
            if problems:
                bad += 1
                print(f"FAIL {p}")
                for msg in problems:
                    print(f"       {msg}")
            else:
                print(f"ok   {p}")

    print(f"\n共 {total} 个 WAV，{bad} 个头部不合规")
    if bad:
        print("修复方法：RIFF size 回填 filelen-8，data chunk size 回填 payload 字节数；"
              "payload 不用动（8 字节/文件）。")
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
