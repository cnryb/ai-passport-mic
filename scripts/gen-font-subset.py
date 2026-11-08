#!/usr/bin/env python3
"""生成设备屏幕用的 16x16 点阵字库 device/main/font16_data.h。

字库来源
--------
GNU Unifont 16.0.01 的原生 16x16 位图（`.hex` 格式）。选它而不是把某个 TTF 光栅化：

- CJK 是**原生点阵**，不是缩放出来的——16 px 小字下笔画清晰度差别很大；
- 许可清晰：Unifont 自 13.0.04 起双许可 GPLv2+（含字体嵌入例外）与 SIL OFL 1.1；
- 只取子集，仓库里不会出现几 MB 的字体文件；生成物是位图，不引入字体软件的许可链。

为什么要有脚本
--------------
字库必须与代码里的文案一一对应。**加了新文案却忘了重新生成字库，屏幕上就是一个方框**
（`screen.c` 会把缺字形的字符画成 `?`，但那时已经在真机上了）。所以：

- 文案集中在 `device/main/screen_texts.h`，脚本从这个文件里**提取所有字符串字面量**
  自动算字符集，不手抄一遍；
- `./scripts/verify.sh` 会把生成的 `font16_data.h` 与"重新生成的结果"逐字节比对，
  文案改了没重生成就是 FAIL（与 `gen-protocol-vectors.sh --check` 同一套路）。

用法
----
    ./scripts/gen-font-subset.py            # 生成/更新 device/main/font16_data.h
    ./scripts/gen-font-subset.py --check    # 只校验是否最新（verify.sh 调用）

首次使用需要下载 Unifont（约 900 KB，gzip 后），缓存到 ~/.cache/ai-passport-mic/。
"""
from __future__ import annotations

import argparse
import gzip
import hashlib
import re
import sys
import urllib.request
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
TEXTS_H = ROOT / "device" / "main" / "screen_texts.h"
OUT_H = ROOT / "device" / "main" / "font16_data.h"

UNIFONT_VERSION = "16.0.01"
UNIFONT_URL = (
    "https://unifoundry.com/pub/unifont/unifont-%s/font-builds/unifont-%s.hex.gz"
    % (UNIFONT_VERSION, UNIFONT_VERSION)
)
# 钉死 SHA-256：字形换了会让屏幕上的字静默变样，这和"代码没变但行为变了"一样危险。
UNIFONT_SHA256 = "2ce5ba84af1b4606391d06255073b5f14e146f3e1fc0c781f2f18d4c0a6c8f13"
CACHE = Path.home() / ".cache" / "ai-passport-mic" / ("unifont-%s.hex.gz" % UNIFONT_VERSION)

# 固定要带的字符：ASCII 可见区给了「以后文案里出现英文/数字就不必立刻重生成」的余量，
# 代价只有 95 个字形 × 32 字节 ≈ 3 KB。
ALWAYS = set(chr(c) for c in range(0x20, 0x7F))
ALWAYS |= {
    "\u00d7",  # × 连发次数
    "\u00b7",  # · 分隔符
    "\u2026",  # … 截断标记
    "\u3000",  # 全角空格
    "\u2014",  # — 破折号
}


def extract_charset() -> set[str]:
    """从 screen_texts.h 里抠出所有字符串字面量的字符。

    刻意不做 C 语法解析：这个文件是我们自己写的、只放字面量与宏，正则足够，
    而且**宁可多收几个字**（多几个字形只占几十字节）也不能漏——漏了就是屏幕上的方框。
    """
    if not TEXTS_H.exists():
        sys.exit("找不到 %s" % TEXTS_H)
    text = TEXTS_H.read_text(encoding="utf-8")
    # 去掉 // 与 /* */ 注释：注释里的中文不该进字库，否则字库会随注释漂移。
    text = re.sub(r"/\*.*?\*/", "", text, flags=re.S)
    text = re.sub(r"//[^\n]*", "", text)
    chars: set[str] = set(ALWAYS)
    for literal in re.findall(r'"((?:[^"\\]|\\.)*)"', text):
        # 只处理常见的转义；\xNN 之类的字节序列不参与（屏幕文案里没有）。
        literal = literal.replace("\\n", "").replace("\\t", "").replace('\\"', '"')
        chars.update(literal)
    chars.discard("\n")
    chars.discard("\t")
    return chars


def load_unifont() -> dict[int, list[int]]:
    if not CACHE.exists():
        CACHE.parent.mkdir(parents=True, exist_ok=True)
        print("下载 %s ..." % UNIFONT_URL, file=sys.stderr)
        urllib.request.urlretrieve(UNIFONT_URL, CACHE)
    raw = CACHE.read_bytes()
    digest = hashlib.sha256(raw).hexdigest()
    if UNIFONT_SHA256 != "skip" and digest != UNIFONT_SHA256:
        # 允许覆盖（上游重打包过），但必须显式说明；默认直接失败，避免静默换字形。
        sys.exit(
            "Unifont 校验和不符：\n  期望 %s\n  实际 %s\n"
            "上游若重打包过，请确认后把 UNIFONT_SHA256 改成实际值并记进本次提交说明。"
            % (UNIFONT_SHA256, digest)
        )
    glyphs: dict[int, list[int]] = {}
    with gzip.open(CACHE, "rt", encoding="ascii") as f:
        for line in f:
            line = line.strip()
            if not line or ":" not in line:
                continue
            cp_s, bits_s = line.split(":", 1)
            cp = int(cp_s, 16)
            row_bytes = len(bits_s) // 32  # 16 行，每行 row_bytes 字节
            if len(bits_s) not in (32, 64):
                continue
            rows: list[int] = []
            for r in range(16):
                if row_bytes == 1:  # 8 宽：左边补 8 个 0 位，靠左对齐
                    rows.append(int(bits_s[r * 2 : (r + 1) * 2], 16) << 8)
                else:
                    rows.append(int(bits_s[r * 4 : (r + 1) * 4], 16))
            glyphs[cp] = rows
    if not glyphs:
        sys.exit("Unifont 解析出 0 个字形，文件可能损坏")
    return glyphs


def render(charset: set[str], glyphs: dict[int, list[int]]) -> str:
    cps = sorted(ord(c) for c in charset)
    missing = [cp for cp in cps if cp not in glyphs]
    if missing:
        pretty = ", ".join("%s(U+%04X)" % (chr(cp), cp) for cp in missing)
        sys.exit("以下字符在 Unifont 里没有字形，屏幕会显示成 '?':\n  " + pretty)

    out: list[str] = []
    out.append("// device/main/font16_data.h —— 16x16 点阵字库（自动生成，勿手改）")
    out.append("//")
    out.append("// 生成: ./scripts/gen-font-subset.py（字符集来自 device/main/screen_texts.h）")
    out.append("// 来源: GNU Unifont %s 的原生 16x16 位图。" % UNIFONT_VERSION)
    out.append("// Unifont 自 13.0.04 起双许可：GPLv2+（含字体嵌入例外）与 SIL OFL 1.1。")
    out.append("// 这里存的是**渲染位图**，且只含本固件文案用到的子集。")
    out.append("//")
    out.append("// 字形数据 16 行 × 2 字节，行优先自顶向下，每字节高位在左（bit7 = 最左像素）。")
    out.append("// 改文案后必须重跑脚本，否则 ./scripts/verify.sh 会 FAIL。")
    out.append("#pragma once")
    out.append("")
    out.append("#include <stdint.h>")
    out.append("")
    out.append("#define APM_FONT16_W 16")
    out.append("#define APM_FONT16_H 16")
    out.append("#define APM_FONT16_BYTES 32")
    out.append("")
    out.append("typedef struct {")
    out.append("    uint32_t cp;                        // Unicode 码位")
    out.append("    uint8_t  bits[APM_FONT16_BYTES];    // 16 行 × 2 字节")
    out.append("} apm_glyph16_t;")
    out.append("")
    out.append("static const apm_glyph16_t s_font16[] = {")
    for cp in cps:
        rows = glyphs[cp]
        bytes_ = []
        for v in rows:
            bytes_.append((v >> 8) & 0xFF)
            bytes_.append(v & 0xFF)
        hexes = ", ".join("0x%02X" % b for b in bytes_)
        out.append("    { 0x%04X, { %s } }," % (cp, hexes))
    out.append("};")
    out.append("")
    out.append("static const unsigned s_font16_count = sizeof(s_font16) / sizeof(s_font16[0]);")
    out.append("")
    return "\n".join(out)


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--check", action="store_true", help="只校验是否最新")
    ap.add_argument(
        "--skip-checksum",
        action="store_true",
        help="跳过 Unifont SHA-256 校验（上游重打包时用，需在提交说明里写清）",
    )
    args = ap.parse_args()

    global UNIFONT_SHA256
    if args.skip_checksum:
        UNIFONT_SHA256 = "skip"

    charset = extract_charset()
    glyphs = load_unifont()
    text = render(charset, glyphs)

    if args.check:
        current = OUT_H.read_text(encoding="utf-8") if OUT_H.exists() else ""
        if current == text:
            print("OK   device/main/font16_data.h 与文案一致（%d 个字形）" % len(charset))
            return 0
        print(
            "FAIL device/main/font16_data.h 已过期：screen_texts.h 的文案变了但没重新生成字库。\n"
            "     跑 ./scripts/gen-font-subset.py 更新后提交。",
            file=sys.stderr,
        )
        return 1

    OUT_H.write_text(text, encoding="utf-8")
    print("已写入 %s（%d 个字形，约 %d 字节）" % (OUT_H, len(charset), len(charset) * 32))
    return 0


if __name__ == "__main__":
    sys.exit(main())
