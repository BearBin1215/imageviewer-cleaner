# -*- coding: utf-8 -*-
"""生成中文字体子集，供 GUI 嵌入。

字符集 = GB2312 全部汉字 + ASCII + UI 符号集。

为什么包含 UI 符号：`✓ ✗ ⚠` 与框线字符不在 GB2312 里。界面虽然保留
`default_fonts` 作回退，把符号显式纳入子集后就不依赖回退了。

用法：
    python tools/subset_font.py
    python tools/subset_font.py --font D:\\fonts\\NotoSansSC-VF.ttf

产物：
    crates/gui/assets/font_subset.ttf   （约 2.2 MB，提交进仓库）

字体来源优先级：
    1. --font 指定的路径
    2. 系统已装的 NotoSansSC-VF.ttf
    3. 环境变量 BCT_FONT_SRC

字体更新时才需要重跑本脚本，日常构建不需要。
"""

from __future__ import annotations

import argparse
import os
import sys
from pathlib import Path

try:
    from fontTools import subset
    from fontTools.ttLib import TTFont
    from fontTools.varLib import instancer
except ImportError:
    sys.exit("缺少 fonttools，请先安装：pip install fonttools brotli")

REPO = Path(__file__).resolve().parent.parent
OUT = REPO / "crates" / "gui" / "assets" / "font_subset.ttf"

FALLBACK_FONTS = [
    Path(r"C:\Windows\Fonts\NotoSansSC-VF.ttf"),
    Path(r"C:\Windows\Fonts\msyh.ttc"),
]

# 界面用到的符号。GB2312 不包含这些码位。
UI_SYMBOLS = (
    "✓✗⚠→←↑↓"
    "─│┌┐└┘├┤┬┴┼"
    "●○■□·"
    "…—、。，：；！？（）【】《》「」"
)


def gb2312_chars() -> list[str]:
    """GB2312 收录的全部汉字（6763 个）"""
    out: set[str] = set()
    for b1 in range(0xB0, 0xF8):
        for b2 in range(0xA1, 0xFF):
            try:
                out.add(bytes([b1, b2]).decode("gb2312"))
            except UnicodeDecodeError:
                continue
    return sorted(out)


def pick_source(explicit: str | None) -> Path:
    if explicit:
        p = Path(explicit)
        if not p.exists():
            sys.exit(f"指定的字体不存在：{p}")
        return p

    env = os.environ.get("BCT_FONT_SRC")
    if env and Path(env).exists():
        return Path(env)

    for p in FALLBACK_FONTS:
        if p.exists():
            return p

    sys.exit(
        "未找到可用字体。请用 --font 指定，"
        "或设置环境变量 BCT_FONT_SRC，或安装 Noto Sans SC。"
    )


def main() -> None:
    ap = argparse.ArgumentParser(description="生成中文字体子集")
    ap.add_argument("--font", help="源字体路径")
    ap.add_argument("--out", default=str(OUT), help="输出路径")
    args = ap.parse_args()

    src = pick_source(args.font)
    out = Path(args.out)
    out.parent.mkdir(parents=True, exist_ok=True)

    hanzi = gb2312_chars()
    ascii_chars = "".join(chr(c) for c in range(0x20, 0x7F))
    text = "".join(hanzi) + ascii_chars + UI_SYMBOLS

    print(f"源字体   : {src}")
    print(f"汉字     : {len(hanzi)}")
    print(f"UI 符号  : {len(UI_SYMBOLS)}")
    print(f"总字符数 : {len(set(text))}")

    font = TTFont(src)
    # 可变字体先固定到 Regular，得到体积更小的静态实例
    if "fvar" in font:
        instancer.instantiateVariableFont(
            font, {"wght": 400}, inplace=True, updateFontNames=False
        )

    opts = subset.Options()
    opts.layout_features = ["*"]
    opts.name_IDs = ["*"]
    opts.notdef_outline = True
    opts.recalc_bounds = True
    opts.drop_tables += ["DSIG"]

    s = subset.Subsetter(options=opts)
    s.populate(text=text)
    s.subset(font)
    subset.save_font(font, str(out), opts)
    font.close()

    size_mb = out.stat().st_size / 1024 / 1024
    print(f"\n已生成 {out}  ({size_mb:.2f} MB)")
    print("把它提交进仓库即可；仅当字体上游更新时才需要重跑本脚本。")


if __name__ == "__main__":
    main()
