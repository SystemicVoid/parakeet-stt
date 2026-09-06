#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.11"
# dependencies = ["fonttools>=4.50", "skia-pathops>=0.8"]
# ///
"""Build the Overlay's bundled font assets (parakeet-ptt/assets/fonts/).

The Overlay renderer rasterises text with fontdue, which reads static TrueType
outlines and only the legacy `kern` table. Newsreader ships as a variable font
with GPOS kerning, so this script:

1. instances Newsreader Regular and Italic at the default optical-size master
   (opsz 18, weight 400) with overlapping contours removed, and
2. bakes the GPOS pair kerning (the `kern` feature, PairPos formats 1 and 2)
   for a Latin subset into a `kern` format 0 table.

Fira Code Regular is copied unmodified. Licence files ship beside the fonts.

Usage (defaults suit a machine with the fonts installed):
    scripts/build-overlay-fonts.py \
        --newsreader-dir ~/.local/share/fonts/Newsreader \
        --firacode /usr/share/fonts/truetype/firacode/FiraCode-Regular.ttf
"""

from __future__ import annotations

import argparse
import shutil
from pathlib import Path

from fontTools.ttLib import TTFont
from fontTools.ttLib.tables import _k_e_r_n
from fontTools.varLib.instancer import OverlapMode, instantiateVariableFont

REPO_ROOT = Path(__file__).resolve().parent.parent
OUT_DIR = REPO_ROOT / "parakeet-ptt" / "assets" / "fonts"
LOCATION = {"opsz": 18, "wght": 400}
SUBSET = [chr(c) for c in range(0x20, 0x7F)] + list(
    "…’‘“”–—àâäéèêëîïôöùûüçÀÂÉÈÊËÎÏÔÙÛÜÇœŒæÆñÑßáíóúÁÍÓÚ"
)


def flatten_pair_kerning(font: TTFont) -> dict[tuple[str, str], int]:
    """Collect XAdvance pair adjustments from the `kern` GPOS feature for SUBSET."""
    cmap = font.getBestCmap()
    glyphs = sorted({cmap[ord(ch)] for ch in SUBSET if ord(ch) in cmap})
    glyph_set = set(glyphs)
    pairs: dict[tuple[str, str], int] = {}
    if "GPOS" not in font:
        return pairs
    gpos = font["GPOS"].table
    lookup_indices: set[int] = set()
    for record in gpos.FeatureList.FeatureRecord:
        if record.FeatureTag == "kern":
            lookup_indices.update(record.Feature.LookupListIndex)
    for index in sorted(lookup_indices):
        for subtable in gpos.LookupList.Lookup[index].SubTable:
            if subtable.LookupType == 9:
                subtable = subtable.ExtSubTable
            if subtable.LookupType != 2:
                continue
            coverage = subtable.Coverage.glyphs
            if subtable.Format == 1:
                for first, pair_set in zip(coverage, subtable.PairSet):
                    if first not in glyph_set:
                        continue
                    for record in pair_set.PairValueRecord:
                        second = record.SecondGlyph
                        value = getattr(record.Value1, "XAdvance", 0) if record.Value1 else 0
                        if second in glyph_set and value:
                            pairs.setdefault((first, second), value)
            elif subtable.Format == 2:
                class1 = subtable.ClassDef1.classDefs
                class2 = subtable.ClassDef2.classDefs
                for first in coverage:
                    if first not in glyph_set:
                        continue
                    row = subtable.Class1Record[class1.get(first, 0)]
                    for second in glyphs:
                        record = row.Class2Record[class2.get(second, 0)]
                        value = getattr(record.Value1, "XAdvance", 0) if record.Value1 else 0
                        if value:
                            pairs.setdefault((first, second), value)
    return pairs


def bake_kern_table(font: TTFont) -> int:
    pairs = flatten_pair_kerning(font)
    table = _k_e_r_n.table__k_e_r_n()
    table.version = 0
    subtable = _k_e_r_n.KernTable_format_0(apple=False)
    subtable.coverage = 1
    subtable.version = 0
    subtable.kernTable = dict(pairs)
    table.kernTables = [subtable]
    font["kern"] = table
    return len(pairs)


def instance_newsreader(source: Path, target: Path) -> None:
    font = TTFont(source)
    static = instantiateVariableFont(
        font,
        LOCATION,
        inplace=False,
        overlap=OverlapMode.REMOVE,
        updateFontNames=False,
    )
    pair_count = bake_kern_table(static)
    static.save(target)
    print(f"{target.name}: {target.stat().st_size} bytes, {pair_count} kern pairs")


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__.split("\n", 1)[0])
    parser.add_argument(
        "--newsreader-dir",
        type=Path,
        default=Path.home() / ".local/share/fonts/Newsreader",
        help="directory holding Newsreader[opsz,wght].ttf, the Italic file and OFL.txt",
    )
    parser.add_argument(
        "--firacode",
        type=Path,
        default=Path("/usr/share/fonts/truetype/firacode/FiraCode-Regular.ttf"),
    )
    parser.add_argument(
        "--firacode-licence",
        type=Path,
        default=Path("/usr/share/doc/fonts-firacode/copyright"),
    )
    args = parser.parse_args()

    OUT_DIR.mkdir(parents=True, exist_ok=True)
    instance_newsreader(
        args.newsreader_dir / "Newsreader[opsz,wght].ttf",
        OUT_DIR / "Newsreader-Regular.ttf",
    )
    instance_newsreader(
        args.newsreader_dir / "Newsreader-Italic[opsz,wght].ttf",
        OUT_DIR / "Newsreader-Italic.ttf",
    )
    shutil.copyfile(args.newsreader_dir / "OFL.txt", OUT_DIR / "OFL-Newsreader.txt")
    shutil.copyfile(args.firacode, OUT_DIR / "FiraCode-Regular.ttf")
    shutil.copyfile(args.firacode_licence, OUT_DIR / "OFL-FiraCode.txt")
    print(f"FiraCode-Regular.ttf: {(OUT_DIR / 'FiraCode-Regular.ttf').stat().st_size} bytes")


if __name__ == "__main__":
    main()
