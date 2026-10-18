#!/usr/bin/env python3
"""从 app/assets/rustshell.ico 派生出 macOS / Linux 打包所需的图标文件。

为什么必须派生、不能三平台共用一份：

  · Windows —— `.ico`。app/build.rs 经 embed-resource 把它嵌进 exe 的 RT_ICON，
    cargo-packager 的 `icons` / `installer-icon` 也直接读它。**这一份是源**。
  · macOS   —— `.icns`。`.app/Contents/Resources` 只认 ICNS 容器（里面装的是一组
    按尺寸命名的 PNG 块），塞 .ico 进去 Finder/Dock 直接显示成通用文档图标。
  · Linux   —— `.png`。.deb 的图标要按 freedesktop hicolor 规范落到
    `usr/share/icons/hicolor/<宽>x<高>/apps/<name>.png`，扩展名与目录名都必须对。

生成方式：
    python3 scripts/gen-app-icons.py          # 覆盖写 app/assets/ 下的两个文件
    python3 scripts/gen-app-icons.py --check  # 只校验现有文件是否与生成结果一致

产物（都已入库，CI 只读不生成 —— 打包阶段不该依赖 Python 环境）：
    app/assets/RustShell.icns    macOS .app 图标（ic07/08/09/10/11/12/13/14 共 8 个尺寸）
    app/assets/rustshell.png     Linux hicolor 图标（256×256）

⚠️ 源 .ico 的最大尺寸是 256×256（实测：16/32/48/64/128/256 六档）。ICNS 规范里
   512（ic09）与 1024（ic10）是 Retina 尺寸，只能从 256 放大 —— 放大用 LANCZOS，
   观感可接受；**不要**为了「看起来有 1024」去伪造，那只是更大的模糊图。
"""

import argparse
import io
import struct
import sys
from pathlib import Path

try:
    from PIL import Image
except ImportError:  # pragma: no cover - 只在没装 PIL 的环境触发
    sys.exit("需要 Pillow：pip install pillow")

REPO = Path(__file__).resolve().parent.parent
SRC_ICO = REPO / "app" / "assets" / "rustshell.ico"
OUT_ICNS = REPO / "app" / "assets" / "RustShell.icns"
OUT_PNG = REPO / "app" / "assets" / "rustshell.png"

# ICNS 里的 PNG 块类型 → 像素边长。
#
# 这些类型码不是随便定的，key 与尺寸的对应关系由 Apple 的 Icon Family 规范固定：
#   ic07 = 128×128        ic08 = 256×256        ic09 = 512×512
#   ic10 = 512×512@2x（1024）                     ic11 = 16×16@2x（32）
#   ic12 = 32×32@2x（64）  ic13 = 128×128@2x（256） ic14 = 256×256@2x（512）
# 只给「大尺寸 + Retina」这几档也行（现代 macOS 只读这几档），但补齐 16/32 的
# 好处是 Spotlight / 列表视图不会回落到系统默认图标。
ICNS_BLOCKS = [
    (b"ic07", 128),
    (b"ic08", 256),
    (b"ic09", 512),
    (b"ic10", 1024),
    (b"ic11", 32),
    (b"ic12", 64),
    (b"ic13", 256),
    (b"ic14", 512),
]


def load_source():
    """读源 .ico，返回可缩放的最大一帧（RGBA）。"""
    if not SRC_ICO.exists():
        sys.exit("找不到源图标 %s" % SRC_ICO)
    im = Image.open(SRC_ICO)
    # .ico 里有多帧，PIL 默认只加载第一帧（通常是最小尺寸），必须显式挑最大的。
    sizes = sorted(im.info.get("sizes") or [im.size])
    biggest = max(sizes)
    im.size = biggest
    im = im.convert("RGBA")
    if im.size != (biggest[0], biggest[1]):
        sys.exit("读取 .ico 的 %s 帧失败（实际 %s）" % (biggest, im.size))
    return im


def png_bytes(im, edge):
    """把源图缩放到 edge×edge，编码成 PNG 字节串。

    放大用 LANCZOS 而不是默认的 NEAREST：图标是线框+渐变，NEAREST 会把边缘
    放大成锯齿块，在 Retina 上非常明显。缩小同理（256 -> 32 走 BICUBIC 会糊边）。
    """
    frame = im if im.size == (edge, edge) else im.resize((edge, edge), Image.LANCZOS)
    buf = io.BytesIO()
    frame.save(buf, format="PNG", optimize=True)
    return buf.getvalue()


def build_icns(im):
    """拼 ICNS 容器：'icns' + 总长度（大端 u32）+ 若干 [4字节类型][4字节长度][PNG]。"""
    body = b""
    for code, edge in ICNS_BLOCKS:
        payload = png_bytes(im, edge)
        body += code + struct.pack(">I", 8 + len(payload)) + payload
    # 长度字段含 8 字节头（magic + length 自身）
    return b"icns" + struct.pack(">I", 8 + len(body)) + body


def build_png(im):
    return png_bytes(im, 256)


def verify_icns(data):
    """回读校验：magic / 总长 / 每个块的长度字段都必须自洽。

    ICNS 是「用长度字段串起来的块链」，只要有一个长度写错，后面全部错位 ——
    macOS 的表现是「图标不显示」，不报任何错。所以这里必须自己走一遍块链。
    """
    if data[:4] != b"icns":
        return "magic 不是 icns"
    total = struct.unpack(">I", data[4:8])[0]
    if total != len(data):
        return "头部总长 %d 与文件实际长度 %d 不一致" % (total, len(data))
    off, seen = 8, []
    while off < len(data):
        code, length = data[off : off + 4], struct.unpack(">I", data[off + 4 : off + 8])[0]
        if length < 8 or off + length > len(data):
            return "块 %r 长度 %d 越界（偏移 %d）" % (code, length, off)
        seen.append(code)
        off += length
    expect = [c for c, _ in ICNS_BLOCKS]
    if seen != expect:
        return "块序列不符：期望 %s，实际 %s" % (expect, seen)
    return None


def main():
    ap = argparse.ArgumentParser(description="生成 macOS .icns 与 Linux .png 图标")
    ap.add_argument(
        "--check",
        action="store_true",
        help="不写文件，只校验现有产物是否与生成结果一致（CI / 提交前用）",
    )
    args = ap.parse_args()

    im = load_source()
    icns = build_icns(im)
    png = build_png(im)

    problem = verify_icns(icns)
    if problem:
        sys.exit("生成的 ICNS 自检失败：%s" % problem)

    print("源图标 %s（最大帧 %d×%d）" % (SRC_ICO.relative_to(REPO), im.size[0], im.size[1]))
    print("  ICNS %d 字节，%d 个尺寸：%s"
          % (len(icns), len(ICNS_BLOCKS), ", ".join(str(e) for _, e in ICNS_BLOCKS)))
    print("  PNG  %d 字节，256×256" % len(png))

    if args.check:
        stale = [
            p.name
            for p, want in ((OUT_ICNS, icns), (OUT_PNG, png))
            if not p.exists() or p.read_bytes() != want
        ]
        if stale:
            sys.exit("以下图标与源 .ico 不一致，请重跑本脚本：" + ", ".join(stale))
        print("校验通过：两份图标都与源 .ico 一致")
        return 0

    OUT_ICNS.write_bytes(icns)
    OUT_PNG.write_bytes(png)
    print("已写入：%s、%s"
          % (OUT_ICNS.relative_to(REPO), OUT_PNG.relative_to(REPO)))
    return 0


if __name__ == "__main__":
    sys.exit(main())
