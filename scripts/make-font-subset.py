#!/usr/bin/env python3
"""生成中文子集字体（`scripts/make-font-subset.ps1` 的干活部分）。

字形集合 = **语言包全部字符** ∪ **源码里的字面量字符** ∪ **固定补充集**
（ASCII 可见字符、常用标点/数字/单位/箭头/几何符号、常见环境中文用字）。

为什么要"源码里的字面量"：界面文案有一部分是硬编码在 `.rs` 里没进语言包的
（`Display {}`、`主显示器 `、`（默认）`、`音频设备 {i}`、`显示器 {index}` 等）。
漏掉它们 → 子集缺字 → 首帧到"完整字体追加完成"之间会看到方框。

只依赖 `fontTools`（子集化）——`brotli` 只在源字体带 WOFF2 时才需要，
本脚本只写 TTF/OTF，所以 brotli 是可选的。
"""

from __future__ import annotations

import argparse
import json
import re
import sys
from pathlib import Path

# ---------------------------------------------------------------------------
# 固定补充集
# ---------------------------------------------------------------------------

# ASCII 可见字符（含空格）：拉丁字母/数字/常见符号。汉字子集里**保留它们**是刻意的：
# 万一某个控件回落到中文字体，至少不会出现"数字变成方框"。
ASCII = "".join(chr(c) for c in range(0x20, 0x7F))

# 界面里真的会出现的非 ASCII 符号与标点（见 theme.rs / main_page.rs / editor.rs 的字面量）。
# 说明：这里**不追求穷举 Unicode 标点**，只覆盖本程序实际用到的那些；
# 另外加上"常见环境中文用字"（设备名/显示器名/用户名里高频出现的字），
# 让首帧之后到完整字体加载完成之间的窗口期几乎不可能出现方框。
EXTRA_SYMBOLS = (
    "×÷±≈≠≤≥°℃µΩ"
    "→←↑↓↔"
    "·•…—–―‐"
    "、。，；：？！“”‘’（）【】《》〈〉「」『』〔〕"
    "　"
    "｜／＼［］｛｝％＃＆＊＋－＝＜＞"
    "①②③④⑤⑥⑦⑧⑨⑩"
    "ⅠⅡⅢⅣⅤ"
    "√∞§¶†‡"
    "●○◆◇■□▲△▼▽★☆"
    "✔✓✗✘"
)

# 常见环境中文用字补充（显示器/设备/用户目录名里高频）：
# 这些不是"界面文案"，但会出现在**运行期拼出来的字符串**里（设备友好名、显示器名、路径）。
# 只挑真正的硬件/显示/录制词：每多 100 个字约多 60~120 KB，**不要**为了"万一"把整个字库带上
#（实测：GB2312 一级字库 3755 字 → 子集 2.9 MB，光解析就又是几十毫秒，等于没优化）。
EXTRA_HANZI = (
    "显示设备音响麦克风耳机扬声器默认主次内外上下左右大小新旧多少长短宽窄高低"
    "屏幕分辨率刷新频率赫兹像素帧率码率音频视频画面录制停止开始暂停继续保存取消确定"
    "设置选项语言中文英文目录文件路径文件夹磁盘本地网络缓存临时导出导入剪辑预览播放"
    "秒分钟时天日期年份星期月日号数字错误失败成功完成处理中等待加载请稍候重试退出"
    "通用高级自定义常规推荐自动手动开启关闭启用禁用允许拒绝"
    "英特尔英伟达超微半导体集成独立核心显卡处理器内存硬盘固态机械键盘鼠标触控板"
    "笔记本台式一体机服务器工作站虚拟远程桌面窗口全屏区域选择框选矩形圆角边框阴影"
    "输入输出采集编码解码压缩解压封装格式容器质量速度平衡性能稳定兼容版本更新日志"
    "计算机用户文档图片音乐下载桌面上网音量静音混音声道立体环绕"
)


# ---------------------------------------------------------------------------
# 从仓库里收集字符
# ---------------------------------------------------------------------------

def chars_from_locales(root: Path) -> set[str]:
    chars: set[str] = set()
    for path in sorted((root / "locales").glob("*.json")):
        data = json.loads(path.read_text(encoding="utf-8"))
        for value in data.get("texts", {}).values():
            chars.update(str(value))
    return chars


# Rust 源码里的**字符串字面量**（含中文的）。正则只做粗筛，多抓一点无害
# （多抓的字符只会让子集略大，不会让界面出错）。
RS_STRING = re.compile(r'"((?:[^"\\]|\\.)*)"')


def chars_from_sources(root: Path) -> set[str]:
    chars: set[str] = set()
    for path in sorted((root / "src").rglob("*.rs")):
        text = path.read_text(encoding="utf-8", errors="replace")
        for m in RS_STRING.finditer(text):
            chars.update(m.group(1))
    return chars


def build_charset(root: Path) -> tuple[set[str], set[str], dict[str, int]]:
    """返回 `(必需字符, 可选字符, 统计)`。

    * **必需**：语言包与源码字面量里的每一个字符 —— 缺一个就会在界面看到方框；
    * **可选**：固定补充集（ASCII/符号/常见环境用字）—— 源字体里没有的字形直接丢掉。
    """
    locales = chars_from_locales(root)
    sources = chars_from_sources(root)
    extra = set(ASCII) | set(EXTRA_SYMBOLS) | set(EXTRA_HANZI)
    # **必需** = 语言包/源码字面量里"不是纯 ASCII"的字符：那些是中文与中文标点，
    # 正常都由中文字体提供（ASCII 全在补充集里，且本来就由内置拉丁字体负责）。
    required = {
        c for c in (locales | sources) if not c.isascii() and (c.isprintable() or c == " ")
    }
    optional = {c for c in extra if c.isprintable() or c == " "}
    stats = {
        "locales": len(locales),
        "sources": len(sources),
        "extra": len(optional),
        "required": len(required),
        "total": len(required | optional),
    }
    return required, optional, stats


# ---------------------------------------------------------------------------
# 子集化
# ---------------------------------------------------------------------------

def load_face(source: Path, face_index: int):
    from fontTools.ttLib import TTFont, TTCollection

    suffix = source.suffix.lower()
    if suffix in (".ttc", ".otc"):
        collection = TTCollection(str(source), lazy=True)
        if face_index >= len(collection.fonts):
            raise SystemExit(
                f"源字体 {source} 只有 {len(collection.fonts)} 个字面，请求的是 #{face_index}"
            )
        return collection.fonts[face_index]
    return TTFont(str(source), lazy=True)


def available_codepoints(font) -> set[int]:
    cmap: set[int] = set()
    for table in font["cmap"].tables:
        cmap.update(table.cmap.keys())
    return cmap


def subset(source: Path, face_index: int, required: set[str], optional: set[str], out: Path) -> dict:
    """按 `required ∪ optional` 子集化，并**列出源字体没有的字形**。

    * **补充集**里没有的字形（`↔ ✓ ✘` 这类）直接剔除 —— 它们本来就由 egui 内置的
      emoji 字体提供，中文字体里没有是正常的；
    * **必需集**里没有的字形会被明确报出来（脚本退出码 2）。它们来自语言包或源码字面量，
      中文字体缺了就只能靠内置字体兜底 —— 界面会不会显示方框取决于那个字符是不是 emoji。
      脚本**不**替人决定，只如实列出，让人看一眼再决定要不要改文案。
    """
    from fontTools import subset as ft_subset

    font = load_face(source, face_index)
    cmap = available_codepoints(font)

    required_missing = sorted(c for c in required if ord(c) not in cmap)
    optional_present = {c for c in optional if ord(c) in cmap}
    optional_dropped = sorted(c for c in optional if ord(c) not in cmap)

    keep = (required - set(required_missing)) | optional_present
    options = ft_subset.Options()
    options.layout_features = ["*"]        # 保留 kerning 等特性（体积代价很小）
    options.name_IDs = ["*"]
    options.name_legacy = True
    options.notdef_outline = True
    options.recalc_bounds = True
    options.drop_tables += ["EBDT", "EBLC", "SVG ", "COLR", "CPAL", "sbix", "CBDT", "CBLC"]
    options.glyph_names = False
    subsetter = ft_subset.Subsetter(options=options)
    subsetter.populate(text="".join(sorted(keep)))
    subsetter.subset(font)

    out.parent.mkdir(parents=True, exist_ok=True)
    font.flavor = None
    font.save(str(out), reorderTables=True)
    return {
        "kept": len(keep),
        "keep": keep,
        "optional_present": len(optional_present),
        "optional_dropped": "".join(optional_dropped),
        "required_missing": "".join(required_missing),
    }


def verify(out: Path, keep: set[str]) -> tuple[int, list[str]]:
    """复核产物：读回子集字体的 cmap，列出**缺字**（必须为空才允许发布）。"""
    from fontTools.ttLib import TTFont

    font = TTFont(str(out), lazy=True)
    cmap = available_codepoints(font)
    missing = sorted(c for c in keep if ord(c) not in cmap)
    return len(cmap), missing


def safe(text: str) -> str:
    """把诊断文本压成"任何控制台都能打出来"的形式。

    本机 Windows 控制台是 GBK，Python 默认 stdout 编码也是 GBK：`↔ ⚠ ✓` 这类字符
    直接 print 会抛 `UnicodeEncodeError`（已经踩过一次）。诊断信息不值得为它崩掉，
    所以非 ASCII 的**符号**统一转成 `\\uXXXX`；汉字照常显示。
    """
    return "".join(
        ch
        if (ch.isascii() or "\u4e00" <= ch <= "\u9fff" or ch in "，。：（）/、")
        else f"\\u{ord(ch):04x}"
        for ch in text
    )


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--source", required=True)
    ap.add_argument("--face-index", type=int, default=0)
    ap.add_argument("--root", required=True)
    ap.add_argument("--out", required=True)
    ap.add_argument("--dry-run", action="store_true")
    args = ap.parse_args()

    root = Path(args.root)
    out = Path(args.out)
    required, optional, stats = build_charset(root)
    print(
        "字形集合  : 语言包 {locales} + 源码字面量 {sources} + 补充集 {extra}"
        " → 必需 {required} / 合计 {total} 个字符".format(**stats)
    )

    if args.dry_run:
        print(f"  源码字面量样例: {safe(''.join(sorted(chars_from_sources(root))[:80]))}")
        print(f"  补充集样例    : {safe(''.join(sorted(optional)[:80]))}")
        return 0

    source = Path(args.source)
    info = subset(source, args.face_index, required, optional, out)
    size = out.stat().st_size
    glyphs, missing = verify(out, info["keep"])
    print(f"产物      : {out} ({size:,} 字节 / {size / 1024:.1f} KiB, {glyphs} 个码位)")
    print(f"保留字形  : 必需 {len(required)} + 补充命中 {info['optional_present']} = {info['kept']}")
    if info["optional_dropped"]:
        print(
            "补充集未命中（源字体没有这些字形，属正常，会由内置 emoji/其它字体兜底）: "
            f"{safe(info['optional_dropped'])}"
        )
    if info["required_missing"]:
        # 不判失败：源码字面量里可能混着日志装饰字符（例如 `⚠`），中文字体没有很正常。
        # 但**必须打印出来**，让人判断"这是不是界面真的会显示的字"。
        print(
            "提示：以下字符来自语言包/源码字面量，但源字体没有对应字形"
            "（界面会回落到 egui 内置字体；若是界面文案请换成中文字体有的字）: "
            f"{safe(info['required_missing'])}"
        )
    if missing:
        print(
            f"复核缺字 {len(missing)} 个（不应该发生）: {safe(''.join(missing[:80]))}",
            file=sys.stderr,
        )
        return 1
    print("复核      : 产物覆盖全部候选字形，无缺字")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
