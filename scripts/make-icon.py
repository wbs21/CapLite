#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""生成 CapLite 的应用图标（蓝色 "Cap"）——**可重复运行**，输出完全确定。

输出：
  * ``assets/caplite.ico``      多尺寸 ICO：16 / 24 / 32 / 48 / 64 / 128 / 256
  * ``assets/caplite-256.png``  256×256 PNG（预览 / 文档用）
  * ``assets/caplite-256.rgba`` 256×256×4 = 262 144 B 的**裸 RGBA8**（逐行、**非预乘**）
  * ``assets/caplite-64.rgba``  64×64×4 = 16 384 B 的同格式小尺寸（备选，见下）

**为什么还要导出裸 RGBA**：ICO 只能进 **PE 资源节**（exe 文件图标 = 资源管理器/文件属性里那个），
而**运行时的窗口图标**（标题栏左上角 + 任务栏）跟 PE 资源**没有任何关系** —— 它由窗口库
（winit/eframe）在创建窗口时用 ``WM_SETICON`` 现场设置。``src/main.rs`` 只能把图标字节
``include_bytes!`` 进二进制再交给 ``ViewportBuilder::with_icon(IconData)``，而 ``egui::IconData``
要的正好是 ``rgba: Vec<u8>`` + ``width`` + ``height``（``rgba`` 是 **非预乘** RGBA8，字段口径见
``egui-0.36.2/src/viewport.rs`` 的 ``IconData``）。裸字节没有解码器、没有额外依赖，
也不会像 PNG 那样要求 ``image`` crate 的 PNG feature —— 所以这里直接导出裸 RGBA。

设计口径（为什么长这样）：
  * **蓝色系**：底色用垂直渐变 ``#3B82F6 → #2563EB``（都是品牌蓝，``#2563EB`` 与界面
    强调色 ``theme::ACCENT`` 同值），白色文字压在上面保证对比度 > 4.5:1；
  * **"Cap" 是主体**：白色粗体、居中、左右各留约 8% 边距 —— 缩小到 16×16 时
    "Cap" 仍是三团可辨认的白色笔画；
  * **"Lite" 只在 ≥64px 时才画**（右下角，半透明白）：16px 上多画一行小字只会糊成一团，
    所以小尺寸**主动省略**，宁可少一个词也不牺牲可读性。这一判断写在 ``LITE_MIN_PX``。

不联网、不新增依赖：只用本机 Python + Pillow（``--deps`` 由 ``scripts/*.ps1`` 传入的
那个解释器即自带 Pillow）+ Windows 自带字体。

用法（在仓库根）：
    python scripts/make-icon.py
    python scripts/make-icon.py --force        # 目标文件已存在也重写
"""

from __future__ import annotations

import argparse
import os
import sys
from pathlib import Path

from PIL import Image, ImageDraw, ImageFilter, ImageFont


# --------------------------------------------------------------------------- #
# 设计参数（**改这里**，不要再在别处写第二份）
# --------------------------------------------------------------------------- #

S = 1024  # 主画布边长（先大后小，缩放到小尺寸时的抗锯齿更干净）
SUPERSAMPLE = 2  # 再放大 2 倍绘制，最后 LANCZOS 缩回，边缘更平滑

BLUE_TOP = (0x3B, 0x82, 0xF6)  # #3B82F6 渐变上端
BLUE_BOTTOM = (0x25, 0x63, 0xEB)  # #2563EB 渐变下端（= 界面强调色）
SHEEN = (0xFF, 0xFF, 0xFF)  # 顶部高光
SHEEN_ALPHA = 36  # 高光强度（0..255）

CORNER_RADIUS_RATIO = 0.215  # 圆角半径 / 边长
# 居中文字："Cap" 的**墨迹宽度**（含 C 的左边缘到 p 的右边缘）≈ 画布的这个比例。
# 为什么按"宽度"而不是"高度"定字号：这两个字母的外形决定了它天然是**扁宽**的，
# 按高度定会让 "Cap" 顶满左右两边（实测 0.42 高度时左右只剩 ~3% 边距，16px 上会糊到圆角里）；
# 按宽度定则左右各留 10%，缩小后四周仍有干净的蓝色留白。
CAP_BOX_W_RATIO = 0.80
LITE_BOX_H_RATIO = 0.115  # "Lite" 的墨迹高度比例
LITE_MARGIN_RATIO = 0.085  # "Lite" 距右边/下边的边距比例
LITE_ALPHA = 165  # "Lite" 的不透明度（低对比、不抢主体）
LITE_MIN_PX = 64  # **小于这个尺寸不画 "Lite"**（见模块 docstring）

ICO_SIZES = [16, 24, 32, 48, 64, 128, 256]

# 字体候选：先拉丁粗体，再中文粗体（两者都含 "Cap"/"Lite" 的拉丁字形）。
# 只**探测**系统字体，不下载。
FONT_CANDIDATES = [
    r"C:\Windows\Fonts\seguisb.ttf",  # Segoe UI Semibold（Windows 的 UI 字体）
    r"C:\Windows\Fonts\segoeuib.ttf",  # Segoe UI Bold
    r"C:\Windows\Fonts\arialbd.ttf",  # Arial Bold
    r"C:\Windows\Fonts\calibrib.ttf",  # Calibri Bold
    r"C:\Windows\Fonts\verdanab.ttf",  # Verdana Bold
    r"C:\Windows\Fonts\tahomabd.ttf",  # Tahoma Bold
    r"C:\Windows\Fonts\msyhbd.ttc",  # 微软雅黑 Bold
    r"C:\Windows\Fonts\msyh.ttc",  # 微软雅黑 Regular
]

REPO_ROOT = Path(__file__).resolve().parent.parent
ASSETS = REPO_ROOT / "assets"


# --------------------------------------------------------------------------- #
# 小工具
# --------------------------------------------------------------------------- #


def pick_font() -> Path:
    """挑一个存在的粗体字体；一个都没有就报错（不静默降级成方块）。"""
    for cand in FONT_CANDIDATES:
        p = Path(cand)
        if p.exists():
            return p
    raise SystemExit(
        "错误：找不到任何可用字体，试过：\n  " + "\n  ".join(FONT_CANDIDATES)
    )


def load_font(
    path: Path,
    box: int,
    *,
    by: str = "width",
    sample: str = "Cap",
) -> tuple[ImageFont.FreeTypeFont, tuple[int, int, int, int]]:
    """按"目标墨迹尺寸 box"取字号（``by="width"`` 或 ``"height"``）。

    用 ``bbox`` 的**墨迹尺寸**（而不是字号 / ascent）当口径：不同字体的字形大小差别很大，
    按字号取会让 "Cap" 在 Segoe 与雅黑下差一截 —— 这个脚本要的是"看起来一样大"。
    """
    box = max(box, 6)
    size = box
    for _ in range(40):
        font = ImageFont.truetype(str(path), size)
        bbox = font.getbbox(sample)
        cur = (bbox[2] - bbox[0]) if by == "width" else (bbox[3] - bbox[1])
        if cur <= 0:
            size += 2
            continue
        target = int(round(size * box / cur))
        if abs(target - size) <= 1:
            break
        size = target
    font = ImageFont.truetype(str(path), size)
    return font, font.getbbox(sample)


def draw_text_optical(
    canvas: Image.Image,
    text: str,
    font: ImageFont.FreeTypeFont,
    fill: tuple[int, int, int, int],
    *,
    center: tuple[float, float] | None = None,
    bottom_right: tuple[float, float] | None = None,
) -> None:
    """把文字**按墨迹（ink）范围**对齐到指定位置。

    ``draw.text(xy)`` 默认按字体原点对齐，不同字体留白不同；这里改成
    "先量出真实墨迹框，再让墨迹框居中 / 贴右下角"，这样换字体也不会跑偏。
    """
    bbox = font.getbbox(text)
    w = bbox[2] - bbox[0]
    h = bbox[3] - bbox[1]
    if center is not None:
        x = center[0] - w / 2.0 - bbox[0]
        y = center[1] - h / 2.0 - bbox[1]
    elif bottom_right is not None:
        x = bottom_right[0] - w - bbox[0]
        y = bottom_right[1] - h - bbox[1]
    else:  # pragma: no cover - 调用点一定给了一个
        raise ValueError("center 或 bottom_right 必须给一个")
    ImageDraw.Draw(canvas).text((x, y), text, font=font, fill=fill)


def rounded_mask(size: int, radius: int, scale: int) -> Image.Image:
    """圆角矩形蒙版（``scale`` 倍超采样后缩回，边缘才不锯齿）。"""
    m = Image.new("L", (size * scale, size * scale), 0)
    d = ImageDraw.Draw(m)
    d.rounded_rectangle(
        (0, 0, size * scale - 1, size * scale - 1),
        radius=radius * scale,
        fill=255,
    )
    return m.resize((size, size), Image.LANCZOS)


def vertical_gradient(size: int, top: tuple[int, int, int], bottom: tuple[int, int, int]) -> Image.Image:
    """垂直渐变（逐行填），返回 RGB。"""
    img = Image.new("RGB", (1, size))
    px = img.load()
    for y in range(size):
        t = y / max(size - 1, 1)
        px[0, y] = (
            int(round(top[0] + (bottom[0] - top[0]) * t)),
            int(round(top[1] + (bottom[1] - top[1]) * t)),
            int(round(top[2] + (bottom[2] - top[2]) * t)),
        )
    return img.resize((size, size), Image.NEAREST)


def top_sheen(size: int, radius: int) -> Image.Image:
    """顶部白色高光（自身带 alpha 的 RGBA），给平板一点"厚度"。"""
    layer = Image.new("RGBA", (size, size), (0, 0, 0, 0))
    d = ImageDraw.Draw(layer)
    d.rounded_rectangle(
        (int(size * 0.055), int(size * 0.045), int(size * 0.945), int(size * 0.44)),
        radius=max(int(radius * 0.55), 2),
        fill=SHEEN + (SHEEN_ALPHA,),
    )
    return layer.filter(ImageFilter.GaussianBlur(size * 0.02))


# --------------------------------------------------------------------------- #
# 主图
# --------------------------------------------------------------------------- #


def render_master(font_path: Path) -> Image.Image:
    """画一张 S×S 的 RGBA 主图（所有尺寸都由它缩放而来 → 各尺寸风格一致）。"""
    radius = int(round(S * CORNER_RADIUS_RATIO))

    # 1) 蓝色渐变底 + 圆角蒙版
    base = vertical_gradient(S, BLUE_TOP, BLUE_BOTTOM).convert("RGBA")
    base.putalpha(rounded_mask(S, radius, 1))

    # 2) 顶部高光（叠在圆角内部）
    sheen = top_sheen(S, radius)
    base.alpha_composite(Image.composite(sheen, Image.new("RGBA", (S, S), (0, 0, 0, 0)), base.getchannel("A")))

    # 3) 文字
    font, _ = load_font(font_path, int(round(S * CAP_BOX_W_RATIO)), by="width", sample="Cap")
    # 视觉重心：把 "Cap" 的墨迹中心放在 (50%, 46%)，给下方 "Lite" 留位置
    #（也避免"上方空、下方挤"）
    draw_text_optical(base, "Cap", font, (255, 255, 255, 255), center=(S / 2.0, S * 0.46))

    lite_font, _ = load_font(font_path, int(round(S * LITE_BOX_H_RATIO)), by="height", sample="Lite")
    margin = S * LITE_MARGIN_RATIO
    draw_text_optical(
        base,
        "Lite",
        lite_font,
        (255, 255, 255, LITE_ALPHA),
        bottom_right=(S - margin, S - margin),
    )
    return base


def downsample(master: Image.Image, px: int) -> Image.Image:
    """从一个尺寸得到"该尺寸真正会显示的样子"：缩到 px。"""
    return master.resize((px, px), Image.LANCZOS)


def maybe_with_lite(master_with: Image.Image, master_without: Image.Image, px: int) -> Image.Image:
    """小尺寸去掉 "Lite"（见模块 docstring 的 LITE_MIN_PX）。"""
    src = master_with if px >= LITE_MIN_PX else master_without
    return downsample(src, px)


def render_master_without_lite(font_path: Path) -> Image.Image:
    """同一套参数但不画 "Lite"（给小尺寸用）。"""
    radius = int(round(S * CORNER_RADIUS_RATIO))
    base = vertical_gradient(S, BLUE_TOP, BLUE_BOTTOM).convert("RGBA")
    base.putalpha(rounded_mask(S, radius, 1))
    sheen = top_sheen(S, radius)
    base.alpha_composite(Image.composite(sheen, Image.new("RGBA", (S, S), (0, 0, 0, 0)), base.getchannel("A")))
    font, _ = load_font(font_path, int(round(S * CAP_BOX_W_RATIO)), by="width", sample="Cap")
    # 没有 "Lite" 时把 "Cap" 放正中间
    draw_text_optical(base, "Cap", font, (255, 255, 255, 255), center=(S / 2.0, S / 2.0))
    return base


# --------------------------------------------------------------------------- #
# 裸 RGBA 导出（给运行时的窗口图标用，见模块 docstring）
# --------------------------------------------------------------------------- #

# `src/main.rs` 里 `include_bytes!` 的就是这两个文件；改尺寸必须同时改那边的
# `IconData { width, height }`（改错会让 winit 拿到对不上的缓冲区）。
RGBA_SIZES = [256, 64]


def rgba_bytes(img: Image.Image) -> bytes:
    """``Image`` → **逐行、非预乘** RGBA8 裸字节（``len == w*h*4``）。

    为什么显式 ``convert("RGBA")`` + ``tobytes()``：
      * Pillow 的 ``tobytes()`` 对 ``RGBA`` 模式**按存储顺序**逐像素输出，**不做** alpha 预乘，
        与 ``egui::IconData.rgba`` 的口径（``viewport.rs``：``RGBA pixels, with
        separate/unmultiplied alpha``）一致；
      * 不经过 PNG 编解码 ⇒ 不引入 `image` crate 的 PNG feature，也没有解码期不确定性。
    """
    raw = img.convert("RGBA")
    data = raw.tobytes()
    expected = raw.width * raw.height * 4
    if len(data) != expected:  # pragma: no cover - 防御性：Pillow 行为变了就立刻炸
        raise SystemExit(f"内部错误：RGBA 字节数 {len(data)} != 期望 {expected}")
    return data


def write_rgba(master: Image.Image, out_dir: Path) -> list[tuple[Path, int]]:
    """写出 ``RGBA_SIZES`` 里每个尺寸的裸 RGBA；返回 ``[(路径, 边长), …]``。"""
    written: list[tuple[Path, int]] = []
    for px in RGBA_SIZES:
        path = out_dir / f"caplite-{px}.rgba"
        # `maybe_with_lite` 的口径与 ICO 一致：< LITE_MIN_PX 的尺寸不画 "Lite"。
        # 64px 正好 >= LITE_MIN_PX(64)，所以 caplite-64.rgba **带** "Lite"，
        # 与 ICO 里 64×64 那一帧、以及 `caplite-256.rgba` 缩到 64 的样子都对得上。
        data = rgba_bytes(downsample(master, px))
        path.write_bytes(data)
        print(f"[icon] 写出 {path}（{len(data)} 字节，{px}×{px}×4 裸 RGBA8 非预乘）")
        written.append((path, px))
    return written


def ensure_rgba(out_dir: Path, *, force: bool) -> bool:
    """幂等：缺哪个补哪个。都在且非 ``force`` → 什么都不做，返回 ``False``。

    这是"脚本可重复运行"的关键：ICO/PNG 已存在时主流程会提前返回，但用户若删掉了
    ``*.rgba``（或以前从没生成过），单独跑一次脚本就应该把它们补回来 —— 且**只**补它们，
    不重画 ICO/PNG（不动已有产物的 mtime，构建缓存不会白失效）。
    """
    for px in RGBA_SIZES:
        path = out_dir / f"caplite-{px}.rgba"
        if force or not path.exists():
            return True
    return False


def main(argv: list[str] | None = None) -> int:
    ap = argparse.ArgumentParser(description="生成 CapLite 图标（蓝色 Cap）")
    ap.add_argument("--out-dir", default=str(ASSETS), help="输出目录（默认 <仓库>/assets）")
    ap.add_argument("--force", action="store_true", help="目标已存在时也重写")
    args = ap.parse_args(argv)

    out_dir = Path(args.out_dir).resolve()
    out_dir.mkdir(parents=True, exist_ok=True)
    ico_path = out_dir / "caplite.ico"
    png_path = out_dir / "caplite-256.png"

    have_ico_png = ico_path.exists() and png_path.exists()
    if not args.force and have_ico_png and not ensure_rgba(out_dir, force=False):
        print(f"已存在，跳过（加 --force 可重写）：{ico_path}")
        print(f"已存在，跳过（加 --force 可重写）：{png_path}")
        for px in RGBA_SIZES:
            print(f"已存在，跳过（加 --force 可重写）：{out_dir / f'caplite-{px}.rgba'}")
        return 0

    font_path = pick_font()
    print(f"[icon] 字体: {font_path}")
    # 只要缺少任何一份 RGBA（或 --force），就把主图重画一遍：同一套参数 ⇒ 同一份字节。
    master = render_master(font_path)
    master_nolite = render_master_without_lite(font_path)

    if args.force or not have_ico_png:
        # 256 PNG（预览用；发布包不带它）
        png = downsample(master, 256)
        png.save(png_path, format="PNG", optimize=True)

        # 多尺寸 ICO：每个尺寸单独画好再交给 Pillow 打包（append_images 的尺寸即条目尺寸）
        frames = [(px, maybe_with_lite(master, master_nolite, px)) for px in ICO_SIZES]
        biggest = frames[-1][1]
        biggest.save(
            ico_path,
            format="ICO",
            sizes=[(px, px) for px, _ in frames],
            append_images=[img for _, img in frames[:-1]],
        )

        print(f"[icon] 写出 {ico_path}（{ico_path.stat().st_size} 字节，尺寸 {ICO_SIZES}）")
        print(f"[icon] 写出 {png_path}（{png_path.stat().st_size} 字节，256×256）")
    else:  # pragma: no cover - 只在"ICO/PNG 在、RGBA 缺"的补写路径上走到
        print(f"[icon] 保留已有的 {ico_path} / {png_path}（只补写缺失的 *.rgba）")

    write_rgba(master, out_dir)
    print(f"[icon] 'Lite' 只在 >= {LITE_MIN_PX}px 的尺寸里绘制；主色 #3B82F6 -> #2563EB")
    return 0


if __name__ == "__main__":
    sys.exit(main())
