"""Render the Timelens mark (ui/icons/logo.svg) into Windows icon assets.

Usage: python render_icon.py
Writes timelens.ico (16-256 px) and timelens-256.png next to this script.
Requires Pillow. Geometry and colors mirror logo.svg on a 64-unit grid.
"""

import math
from pathlib import Path

from PIL import Image, ImageDraw

HERE = Path(__file__).resolve().parent
SCALE = 16  # Draw at 1024 px and downsample for clean anti-aliasing.
CANVAS = 64 * SCALE


def lerp(a, b, t):
    return tuple(round(x + (y - x) * t) for x, y in zip(a, b))


def linear_gradient(size, start, end, p0, p1):
    """A gradient from p0 to p1 (canvas units), clamped beyond both ends."""
    image = Image.new("RGBA", (size, size))
    pixels = image.load()
    dx, dy = p1[0] - p0[0], p1[1] - p0[1]
    length = dx * dx + dy * dy
    for y in range(size):
        for x in range(size):
            t = ((x - p0[0]) * dx + (y - p0[1]) * dy) / length
            pixels[x, y] = lerp(start, end, min(1.0, max(0.0, t))) + (255,)
    return image


def u(value):
    return value * SCALE


def render():
    image = Image.new("RGBA", (CANVAS, CANVAS), (0, 0, 0, 0))

    # Tile.
    tile = linear_gradient(CANVAS, (0x5A, 0x60, 0xE3), (0x2B, 0x2F, 0x92), (u(6), u(4)), (u(58), u(60)))
    mask = Image.new("L", (CANVAS, CANVAS), 0)
    ImageDraw.Draw(mask).rounded_rectangle((u(2), u(2), u(62), u(62)), radius=u(15), fill=255)
    image.paste(tile, (0, 0), mask)

    center, radius, width = u(32), u(17), u(6)
    box = (center - radius, center - radius, center + radius, center + radius)

    # Faint rim.
    rim = Image.new("RGBA", (CANVAS, CANVAS), (0, 0, 0, 0))
    ImageDraw.Draw(rim).ellipse(
        (box[0] - width / 2, box[1] - width / 2, box[2] + width / 2, box[3] + width / 2),
        outline=(255, 255, 255, round(255 * 0.3)),
        width=width,
    )
    image.alpha_composite(rim)

    # Sweep: 120 degrees clockwise from the top, with round caps.
    sweep_mask = Image.new("L", (CANVAS, CANVAS), 0)
    draw = ImageDraw.Draw(sweep_mask)
    draw.arc(
        (box[0] - width / 2, box[1] - width / 2, box[2] + width / 2, box[3] + width / 2),
        start=-90,
        end=30,
        fill=255,
        width=width,
    )
    for degrees in (-90, 30):
        angle = math.radians(degrees)
        cx, cy = center + radius * math.cos(angle), center + radius * math.sin(angle)
        draw.ellipse((cx - width / 2, cy - width / 2, cx + width / 2, cy + width / 2), fill=255)
    sweep = linear_gradient(CANVAS, (0xFF, 0xD3, 0x7C), (0xFF, 0x78, 0x59), (u(32), u(14)), (u(47), u(41)))
    image.paste(sweep, (0, 0), sweep_mask)

    # Pupil.
    ImageDraw.Draw(image).ellipse((u(27), u(27), u(37), u(37)), fill=(255, 255, 255, 255))
    return image


def main():
    master = render()
    sizes = [16, 20, 24, 32, 40, 48, 64, 128, 256]
    frames = [master.resize((size, size), Image.LANCZOS) for size in sizes]
    frames[-1].save(HERE / "timelens-256.png")
    frames[-1].save(HERE / "timelens.ico", sizes=[(s, s) for s in sizes], append_images=frames[:-1])


if __name__ == "__main__":
    main()
