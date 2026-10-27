#!/usr/bin/env python3
# check-icon.py —— 图标质量 CI 门禁（纯标准库，不依赖 Pillow / numpy）
#
# 为什么需要：工作流第 4 步只校验「.ico 存在」，水印、透明底、图形被抹掉都没有回归保护。
# 这次是人工发现并修的，下次换图很容易又退回去，而且不会红。
#
# ── 本脚本判什么、不判什么（边界四条，全部经实测，任一条被省略都会把门禁吹过头）──
#
#  1) 判「与已签署基线逐像素一致」——这是最强的断言，也是水印唯一能落地的地方。
#     水印有没有，无法用亮度类数值代理判定：实测 r>200 与 r>=250 两种阈值在带印/无印
#     两版上给出完全相同的计数（徽章金属高光被算了进去）。故水印不按亮度判，而按
#     「整档像素摘要必须等于已签署基线」。**代价：改图必须重签基线（见 --resign）。**
#  2) 不判「基线本身对不对」。白名单只保证「和签署时一模一样」。基线自己带不带水印，
#     由人签署基线那一刻用眼睛保证，不由本脚本保证。
#     **「本门禁通过」= 「与已签署基线一致」，不等于「图标已正确」，两者不可互相替代。**
#  3) 率带断言只判「不透明像素占比落在区间内」，**不代表图形完整**。
#     实测反例：50% / 60% 见方的纯色方块（里面一个图形像素都没有）占比落在带内，
#     旧版会全 PASS。图形完整性改由下面第 4 条的掩膜交并比承担——当初那句
#     「图形在且背景真溶掉」是错的，本轮删除，不再声称。
#  4) 掩膜交并比判「哪些像素还活着」，对几何类缺陷（裁边 / 挖孔 / 纯色块 / 近似满幅
#     替换）有效；对「活着的像素里画了什么」（水印 / 灰度 / 反相）无感。所以它不单独
#     成立，必须与第 1 条的摘要白名单成对使用。
#
# ── 判据为什么是「两条腿」而不是一条（每条都实测过盲区）──
#   率带      ：挡不住定额裁切（2px 侵蚀在 256 档只掉 5% 量级），也挡不住内容替换。
#   bbox      ：只补裁切，对「高光被吃」「纯色方块」无效（包围盒没变）。已弃用。
#   透明连通域：只补内部挖孔，对裁切与纯色方块无效。
#   掩膜 IoU  ：五类几何缺陷全拦，但对水印/灰度/反相全部 100.0% 放行。
#   摘要白名单：任何像素改动都拦，但零鉴别力（它只回答「是不是同一张图」）。
#   两条腿合起来才闭环：IoU 说「形状还在」，白名单说「画的就是那张」。
#
# 不透明率上下限的取值依据（全部实测，非拍定）：
#   本包基线 7 档 alpha>=128 占比 = 62.20 / 61.77 / 62.16 / 62.89 / 63.96 / 61.63 / 60.16（%）
#   下限 25%：挡住「中央抹透明」（该变体实测 3.99 / 3.56 / 1.93 / 1.26 / 0 / 0 / 0 %）
#   上限 90%：挡住「实心方块只留四角透明」（该变体实测 99.99 / 99.98 / 99.90 / 99.83 / 99.61 / 99.31 / 98.44 %）
#   基线最低 60.16% 对下限有 35 个百分点余量，最高 63.96% 对上限有 26 个百分点余量。
#
# 用法：
#   python3 check-icon.py <icon.ico> <icon.png>            判门禁，退出码 0/1
#   python3 check-icon.py <icon.ico> <icon.png> --resign   重采基线，打印新常量块（改图后用）
#
import sys, struct, zlib, hashlib, os, base64

PNG_SIG = b'\x89PNG\r\n\x1a\n'
SOLID_LO = 25.0    # alpha>=128 像素占比下限（%）
SOLID_HI = 90.0    # alpha>=128 像素占比上限（%）
IOU_MIN = 99.5     # 与已签署基线掩膜的交并比下限（%）
ALPHA_CUT = 128    # 掩膜二值化阈值：alpha>=128 记为「不透明」
fail = []
npass = 0

# ── 已签署基线：7 档 alpha>=128 位掩膜，按 SHAPES 顺序位打包后 zlib 压缩再 base64 ──
# 11272 B 原始位图 → 395 B 压缩 → 528 B base64。重建：zlib.decompress(base64.b64decode(...))
SHAPES = ((256, 256), (128, 128), (64, 64), (48, 48), (32, 32), (24, 24), (16, 16))
MASK_B64 = (
    'eNrtmUmShCAQRSVY0DuOQN+Eo+HRPIpHqCULQlotcUgBceiIHn5WqZXxLMFv+IW0qhCIvx6N34Vc'
    '827P/QrXEez5wl8xrhZuY1znm193IIo9y3bPexF4G+cy2/3VCdg41wfcBO7i3OdPf+EJPAuQ4jwr'
    'z8ybFBc3uczKX8xfKa6+mevs5QUHBwcHBwcHBwcHBwcHBwcHB/8X/HWVP1S/a6/ywvrj3froUf21'
    'uspZYf3YXeOmtP59VD8/qr83WfmSAvLi9wcue3rH7y+abPcSHWBVtgGFt1IIBOJ0fIYfH/TxwIhZ'
    'CeI9kri5JuZoqJfRpw/NGTFnmnPinTQX5/P25v+RI0eO/Ef4Dd/7JfVT6rc7f+6Ifzvi75aMOVvy'
    'PKjpiLqLjKARm8mQmmQ2Qa1wmYKaLLHl2BZtZ92q7XbS20z662l4I5+/zm5sZWx7vGX5ez3ePeI3'
    'rptl/VaZDVqaE5L0xxG93LJflH14GY47HL/eTpzn+bO0w376zLfffz3Jtsrp+GegX/+HO/I='
)

GOLDEN = {
    256: '4a0464dc9ec484410cae3ae3d3653d96',
    128: '921b5ce8bf64d9c4c9701ef247938de4',
     64: '40524712bff6d01ae8a55d18309ef72e',
     48: 'bca95035633705dbcb726eb5aac51cf9',
     32: 'ef93012b7fcc83174f7c9d69868153d7',
     24: '3c1fa9049153b8f2e647330120610e90',
     16: '9c80d495fc37d3aca77d0a5933a03e69',
}


def check(cond, msg):
    global npass
    if cond:
        npass += 1
        print('  PASS  %s' % msg)
    else:
        print('  FAIL  %s' % msg)
        fail.append(msg)


def png_meta(blob):
    """读 PNG 头信息 + 解 IDAT 取 alpha 与 RGBA（纯 stdlib，无第三方依赖）。"""
    assert blob[:8] == PNG_SIG, '不是 PNG'
    pos, idat, ihdr = 8, b'', None
    while pos < len(blob):
        ln = struct.unpack('>I', blob[pos:pos + 4])[0]
        typ = blob[pos + 4:pos + 8]
        body = blob[pos + 8:pos + 8 + ln]
        if typ == b'IHDR':
            ihdr = struct.unpack('>IIBBBBB', body)
        elif typ == b'IDAT':
            idat += body
        elif typ == b'IEND':
            break
        pos += 12 + ln
    w, h, bitd, ctype, comp, filt, inter = ihdr
    res = dict(w=w, h=h, bitd=bitd, ctype=ctype, interlace=inter, alpha=None, rgba=None)
    if bitd != 8 or inter != 0 or ctype not in (0, 2, 4, 6):
        return res

    ch = {0: 1, 2: 3, 4: 2, 6: 4}[ctype]
    raw = zlib.decompress(idat)
    stride = w * ch
    prev = bytearray(stride)
    alphas = []
    rgba = bytearray()
    p = 0
    for y in range(h):
        ft = raw[p]; p += 1
        line = bytearray(raw[p:p + stride]); p += stride
        if ft == 1:
            for i in range(ch, stride):
                line[i] = (line[i] + line[i - ch]) & 0xff
        elif ft == 2:
            for i in range(stride):
                line[i] = (line[i] + prev[i]) & 0xff
        elif ft == 3:
            for i in range(stride):
                a = line[i - ch] if i >= ch else 0
                line[i] = (line[i] + ((a + prev[i]) >> 1)) & 0xff
        elif ft == 4:
            for i in range(stride):
                a = line[i - ch] if i >= ch else 0
                b = prev[i]
                c = prev[i - ch] if i >= ch else 0
                pa, pb, pc = abs(b - c), abs(a - c), abs(a + b - 2 * c)
                pr = a if (pa <= pb and pa <= pc) else (b if pb <= pc else c)
                line[i] = (line[i] + pr) & 0xff
        prev = line
        if ctype == 6:
            alphas.extend(line[3::4]); rgba += line
        elif ctype == 4:
            alphas.extend(line[1::2])
            for i in range(0, len(line), 2):
                g = line[i]; rgba += bytes((g, g, g, line[i + 1]))
        elif ctype == 2:
            alphas.extend([255] * w); rgba += line
        else:
            alphas.extend([255] * w)
            for g in line:
                rgba += bytes((g, g, g, 255))
    res['alpha'] = bytes(alphas)
    res['rgba'] = bytes(rgba)
    return res


def tier_digest(m):
    """逐档像素摘要：md5(LE32(w) + LE32(h) + 解码后的 RGBA 字节)。

    不用 PNG 文件 md5：同一个图形换一个 zlib 压缩级别重新编码，文件 md5 会变而像素
    一字节未动。按解码后的 RGBA 算才是「画的是什么」，不会假红。
    """
    return hashlib.md5(struct.pack('<II', m['w'], m['h']) + m['rgba']).hexdigest()


def golden_masks():
    """还原已签署基线的位掩膜：{尺寸: bytearray(每像素 0/1)}。"""
    raw = zlib.decompress(base64.b64decode(''.join(MASK_B64)))
    assert len(raw) == sum((w * h + 7) // 8 for w, h in SHAPES), '基线的形状常量与掩膜字节数不符'
    out, pos = {}, 0
    for w, h in SHAPES:
        nb = (w * h + 7) // 8
        seg = raw[pos:pos + nb]; pos += nb
        bits = bytearray(w * h)
        for i in range(w * h):
            bits[i] = (seg[i >> 3] >> (i & 7)) & 1
        out[w] = bits
    return out


def main(ico_path, png_path, resign=False):
    print('=' * 70)
    print('图标质量门禁：%s%s' % (os.path.basename(ico_path), '（重采基线模式）' if resign else ''))
    ico = open(ico_path, 'rb').read()
    icon_png = open(png_path, 'rb').read()

    # --- 1. ICO 容器结构 ---
    n = struct.unpack('<H', ico[4:6])[0]
    entries = []
    for i in range(n):
        off = 6 + i * 16
        w, h, cc, res, pl, bpp, sz, ofs = struct.unpack('<BBBBHHII', ico[off:off + 16])
        entries.append(dict(w=256 if w == 0 else w, h=256 if h == 0 else h,
                            bpp=bpp, sz=sz, ofs=ofs))
    check(n == 7, 'ICO 目录项 = 7（实际 %d）' % n)

    sizes = [e['w'] for e in entries]
    # 顺序不判：Windows 对 ICO 目录项顺序并无强制要求（升序、降序、乱序都能正常取用），
    # 把「存储顺序」当合法性会误伤一个图形一像素没动的合法图标。这里只判齐全 + 无重复。
    check(sorted(sizes) == [16, 24, 32, 48, 64, 128, 256],
          '标准 7 档齐全且无重复：%s' % sizes)
    check(all(e['bpp'] == 32 for e in entries), '全部 32bpp')

    # --- 2. 算术闭合 ---
    payload = sum(e['sz'] for e in entries)
    overhead = 6 + n * 16
    check(payload + overhead == len(ico),
          '载荷 %d + 开销 %d = %d 与实际 %d 零差' % (payload, overhead, payload + overhead, len(ico)))
    check(max(e['ofs'] + e['sz'] for e in entries) == len(ico), '末端顶到文件尾，无尾部垃圾')

    GM = golden_masks()
    new_digests = {}
    new_bits = bytearray()
    new_shapes = []

    # --- 3. 逐档：透明底 + 率带 + 掩膜交并比 + 逐档摘要 ---
    print('  ---- 逐档判据（掩膜阈值 alpha>=%d；百分比是占该档总像素）----' % ALPHA_CUT)
    for e in entries:
        blob = ico[e['ofs']:e['ofs'] + e['sz']]
        if blob[:8] != PNG_SIG:
            check(False, '%dx%d 帧不是 PNG（DIB 帧无法验 alpha）' % (e['w'], e['h']))
            continue
        m = png_meta(blob)
        a = m['alpha']
        if a is None:
            check(False, '%dx%d 没有 alpha 通道或位深不受支持（color type %d / bitdepth %d）'
                  % (e['w'], e['h'], m['ctype'], m['bitd']))
            continue
        tot = len(a)
        full = sum(1 for v in a if v == 0)
        solid = sum(1 for v in a if v == 255)
        semi = tot - full - solid
        gated = sum(1 for v in a if v >= ALPHA_CUT)
        corners = [a[0], a[m['w'] - 1], a[(m['h'] - 1) * m['w']], a[m['h'] * m['w'] - 1]]

        check(full > 0 and min(a) == 0 and solid < tot,
              '%dx%d 有透明区：全透明 %d / 半透明 %d / 不透明 %d' % (e['w'], e['h'], full, semi, solid))
        check(all(c == 0 for c in corners),
              '%dx%d 四角 alpha = %s' % (e['w'], e['h'], corners))
        # 率带：只判占比，不判图形完整（纯色方块也能过这一条，故不能单独成立）
        pct = gated / tot * 100.0
        check(SOLID_LO <= pct <= SOLID_HI,
              '%dx%d 不透明率 %.2f%% 落在 [%.0f%%, %.0f%%]（alpha>=%d %d/%d px）'
              % (e['w'], e['h'], pct, SOLID_LO, SOLID_HI, ALPHA_CUT, gated, tot))

        # 腿 1：掩膜交并比 —— 判「哪些像素还活着」，锁几何
        cur = bytearray(tot)
        nb = (tot + 7) // 8
        curseg = bytearray(nb)
        for i, v in enumerate(a):
            if v >= ALPHA_CUT:
                cur[i] = 1
                curseg[i >> 3] |= 1 << (i & 7)
        ref = GM.get(e['w'])
        if ref is None:
            check(False, '%dx%d 基线里没有这一档的掩膜（改过档位组合？需 --resign 重签）' % (e['w'], e['h']))
        else:
            inter = sum(1 for i in range(tot) if cur[i] and ref[i])
            union = sum(1 for i in range(tot) if cur[i] or ref[i])
            iou = (100.0 * inter / union) if union else 100.0
            diff = sum(1 for i in range(tot) if cur[i] != ref[i])
            check(iou >= IOU_MIN,
                  '%dx%d 与已签署基线掩膜交并比 %.3f%% (>= %.1f%%，交集 %d / 并集 %d，差异像素 %d)'
                  % (e['w'], e['h'], iou, IOU_MIN, inter, union, diff))

        # 腿 2：逐档摘要白名单 —— 判「画的就是那张」，锁内容（水印/灰度/反相只此一条能拦）
        dg = tier_digest(m)
        new_digests[e['w']] = dg
        got = GOLDEN.get(e['w'])
        if got is None:
            check(False, '%dx%d 基线摘要表里没有这一档（改过档位组合？需 --resign 重签）' % (e['w'], e['h']))
        elif dg == got:
            check(True, '%dx%d 逐像素与基线一致（md5 %s…）' % (e['w'], e['h'], dg[:16]))
        else:
            # 摘要不符可能是内容变了（RGB），也可能是掩膜变了（alpha）。两者用不同的量说明，
            # 别把 alpha 掩膜差异数（diff）当成「像素内容差异」——只改 RGB 时 diff 会是 0。
            check(False, '%dx%d 与基线像素不符：基线 %s… / 实际 %s…（其中 alpha 掩膜差异 %d px%s）'
                  % (e['w'], e['h'], got[:16], dg[:16], diff,
                     '，即纯 RGB 内容改动' if diff == 0 else ''))

        new_bits += curseg
        new_shapes.append((m['w'], m['h']))

    # --- 4. 托盘图标一致性 ---
    m256 = png_meta(icon_png)
    check(m256['ctype'] == 6, 'icon.png 是 RGBA（color type %d）' % m256['ctype'])
    check(m256['alpha'] is not None and m256['alpha'].count(0) > 0,
          'icon.png 含全透明像素（托盘图标必须能融进浅色任务栏）')
    e256 = [e for e in entries if e['w'] == 256]
    if e256:
        blob256 = ico[e256[0]['ofs']:e256[0]['ofs'] + e256[0]['sz']]
        check(blob256 == icon_png,
              'ICO 256 档与 icon.png 字节级相同（md5 %s）' % hashlib.md5(icon_png).hexdigest()[:12])
    else:
        check(False, 'ICO 里没有 256 档，无法与 icon.png 对比')

    print('=' * 70)

    if resign:
        comp = zlib.compress(bytes(new_bits), 9)
        b64 = base64.b64encode(comp).decode()
        print('重采基线：把下面两段常量替换进本脚本（SHAPES 一并核对），然后提交并由人签署。')
        print('本次基线来源：%s / %s' % (os.path.basename(ico_path), os.path.basename(png_path)))
        print()
        print('SHAPES = %r' % (tuple(new_shapes),))
        print('MASK_B64 = (')
        for i in range(0, len(b64), 76):
            print("    '%s'" % b64[i:i + 76])
        print(')')
        print('GOLDEN = {')
        for w, _h in new_shapes:
            print("    %3d: '%s'," % (w, new_digests[w]))
        print('}')
        print()
        print('重签前的自查（不代替人眼）：改图后必须确认 —— 无水印、图形完整、透明底干净。')
        print('本脚本判「与基线一致」，判不了「基线本身正确」。')
        return 0

    if fail:
        print('结论：FAIL —— %d 项不通过' % len(fail))
        for f in fail:
            print('   - %s' % f)
        print('注：本门禁判「与已签署基线一致」，不判「基线本身正确」；'
              '基线是否无水印由人签署那一刻用眼睛保证。改图请用 --resign 重采并重签。')
        return 1
    print('结论：PASS —— 全部 %d 项断言通过' % npass)
    print('注：本门禁判「与已签署基线一致」，不判「基线本身正确」；'
          '基线是否无水印由人签署那一刻用眼睛保证。改图请用 --resign 重采并重签。')
    return 0


if __name__ == '__main__':
    args = [a for a in sys.argv[1:] if not a.startswith('--')]
    resign = '--resign' in sys.argv[1:]
    if len(args) != 2:
        print('用法: python3 check-icon.py <icon.ico> <icon.png> [--resign]')
        sys.exit(2)
    sys.exit(main(args[0], args[1], resign))
