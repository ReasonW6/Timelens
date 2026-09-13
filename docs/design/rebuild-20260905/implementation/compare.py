"""Normalize the preserved source and native capture for repeatable visual QA.

Run after saving final-timeline.png from the 1440x900 native content viewport.
This only prepares comparison evidence; it does not alter either source image.
"""
from pathlib import Path

from PIL import Image, ImageDraw


directory = Path(__file__).resolve().parent
source = Image.open(directory.parent / "options" / "display-02-paper.png").convert("RGB")
native = Image.open(directory / "final-timeline.png").convert("RGB")
if source.size != (1586, 992) or native.size != (1442, 932):
    raise ValueError(f"Unexpected source or native dimensions: {source.size}, {native.size}")

reference = source.resize((1440, 900), Image.Resampling.LANCZOS)
implementation = native.crop((1, 31, 1441, 931))
reference.save(directory / "reference-1440x900.png")
implementation.save(directory / "implementation-1440x900.png")


def comparison(name, first, second):
    if first.size != second.size:
        raise ValueError("Comparison images must have equal pixel dimensions")
    width, height = first.size
    sheet = Image.new("RGB", (width * 2, height + 26), "#e5dfd6")
    draw = ImageDraw.Draw(sheet)
    draw.text((12, 7), "SELECTED REFERENCE", fill="#302c28")
    draw.text((width + 12, 7), "NATIVE IMPLEMENTATION", fill="#302c28")
    sheet.paste(first, (0, 26))
    sheet.paste(second, (width, 26))
    sheet.save(directory / name)


comparison("comparison-full.png", reference, implementation)
comparison("comparison-header.png", reference.crop((0, 0, 1044, 250)), implementation.crop((0, 0, 1044, 250)))
comparison("comparison-details.png", reference.crop((1044, 0, 1440, 900)), implementation.crop((1044, 0, 1440, 900)))
print("Saved equal-density full-view, header and detail comparisons.")
