"""Render the dark installer's artwork: the glowing backdrop, lens animation frames,
the orbiting dot and the two amber buttons.

Usage: python render_installer_art.py
Writes PNG files next to this script; Timelens.iss packs them with `dontcopy`.
Requires Pillow. The lens mirrors crates/timelens-app/ui/icons/logo.svg on a
100-unit grid: ring radius 40, stroke 10, pupil radius 10.
"""

import math
import random
from pathlib import Path

from PIL import Image, ImageDraw, ImageFont

HERE = Path(__file__).resolve().parent
FPS = 20
LENS_PX = 240  # 2x of the 120 px lens; the installer scales it down at 100 % DPI.
SUPER = 4  # Draw at 4x and downsample for clean anti-aliasing.
UNIT = LENS_PX * SUPER / 100
# Timelens.iss hard-codes these frame counts; keep them in step.
PROGRESS_STEPS = 25
BEAT_FRAMES = 8

BACK = (0x0E, 0x10, 0x15)
GLOW = (0x51, 0x57, 0xD6)
SWEEP_A = (0xFF, 0xD3, 0x7C)
SWEEP_B = (0xFF, 0x78, 0x59)
PILL_INK = (0x2A, 0x16, 0x08)
FONT_BOLD = "C:/Windows/Fonts/msyhbd.ttc"


def lerp(a, b, t):
    return tuple(round(x + (y - x) * t) for x, y in zip(a, b))


def bezier(x1, y1, x2, y2):
    """A CSS cubic-bezier timing function."""

    def curve(t):
        lo, hi = 0.0, 1.0
        for _ in range(30):
            mid = (lo + hi) / 2
            x = 3 * (1 - mid) ** 2 * mid * x1 + 3 * (1 - mid) * mid**2 * x2 + mid**3
            lo, hi = (mid, hi) if x < t else (lo, mid)
        s = (lo + hi) / 2
        return 3 * (1 - s) ** 2 * s * y1 + 3 * (1 - s) * s**2 * y2 + s**3

    return curve


def phase(ms, start, length, ease):
    return ease(min(1.0, max(0.0, (ms - start) / length)))


def sweep_gradient():
    """The sweep's amber gradient for an arc that starts at the top, computed once."""
    size = round(100 * UNIT)
    image = Image.new("RGB", (size, size))
    pixels = image.load()
    cx = cy = 50 * UNIT
    # From just left of the arc's start toward its 120-degree end, as in logo.svg.
    p0 = (cx - 8 * UNIT, cy - 40 * UNIT)
    p1 = (cx + 36 * UNIT, cy + 18 * UNIT)
    dx, dy = p1[0] - p0[0], p1[1] - p0[1]
    length = dx * dx + dy * dy
    for y in range(size):
        for x in range(size):
            t = ((x - p0[0]) * dx + (y - p0[1]) * dy) / length
            pixels[x, y] = lerp(SWEEP_A, SWEEP_B, min(1.0, max(0.0, t)))
    return image


def arc_mask(size, start_deg, sweep_deg, width, radius):
    """An arc clockwise from start_deg (0 = top) with round caps."""
    mask = Image.new("L", (size, size), 0)
    if sweep_deg <= 0:
        return mask
    draw = ImageDraw.Draw(mask)
    c = size / 2
    outer = radius + width / 2
    if sweep_deg >= 359.9:
        draw.ellipse((c - outer, c - outer, c + outer, c + outer), outline=255, width=round(width))
        return mask
    draw.arc((c - outer, c - outer, c + outer, c + outer), start_deg - 90, start_deg - 90 + sweep_deg, fill=255, width=round(width))
    for deg in (start_deg, start_deg + sweep_deg):
        a = math.radians(deg - 90)
        x, y = c + radius * math.cos(a), c + radius * math.sin(a)
        draw.ellipse((x - width / 2, y - width / 2, x + width / 2, y + width / 2), fill=255)
    return mask


def lens(gradient, rim_deg, sweep_deg, rotation, pupil_scale):
    size = gradient.width
    radius, width = 40 * UNIT, 10 * UNIT
    image = Image.new("RGBA", (size, size), (0, 0, 0, 0))

    rim = Image.new("RGBA", (size, size), (255, 255, 255, 0))
    rim.putalpha(arc_mask(size, 0, rim_deg, width, radius).point(lambda v: round(v * 0.15)))
    image.alpha_composite(rim)

    sweep_mask = arc_mask(size, rotation, sweep_deg, width, radius)
    turned = gradient.rotate(-rotation, resample=Image.BICUBIC) if rotation else gradient
    image.paste(turned, (0, 0), sweep_mask)

    if pupil_scale > 0.02:
        r, c = 10 * UNIT * pupil_scale, size / 2
        ImageDraw.Draw(image).ellipse((c - r, c - r, c + r, c + r), fill=(255, 255, 255, 255))
    return image.resize((LENS_PX, LENS_PX), Image.LANCZOS)


def pop(t):
    """Scale-in with a little overshoot, like cubic-bezier(.3, 1.6, .5, 1)."""
    return bezier(0.3, 1.6, 0.5, 1)(t)


def render_lens_frames(gradient):
    # Opening: the rim draws in, the sweep spins once into place, the pupil pops.
    rim_ease = bezier(0.6, 0.1, 0.3, 1)
    sweep_ease = bezier(0.3, 0.6, 0.2, 1)
    total_ms = 2000
    count = total_ms * FPS // 1000
    for i in range(count + 1):
        ms = i * 1000 / FPS
        s = phase(ms, 350, 1600, sweep_ease)
        frame = lens(
            gradient,
            rim_deg=360 * phase(ms, 0, 1100, rim_ease),
            sweep_deg=120 * s,
            rotation=-360 * (1 - s),
            pupil_scale=pop(min(1.0, max(0.0, (ms - 1500) / 500))),
        )
        frame.save(HERE / f"lens-intro-{i:02d}.png", optimize=True)

    # Installing: the sweep grows from nothing to a full ring in 4 % steps.
    for i in range(PROGRESS_STEPS + 1):
        lens(gradient, 360, 360 * i / PROGRESS_STEPS, 0, 1).save(HERE / f"lens-progress-{i:02d}.png", optimize=True)

    # Finished: the full ring holds while the pupil beats once.
    for i in range(BEAT_FRAMES):
        t = (i + 1) / BEAT_FRAMES
        scale = 1 + 0.35 * math.sin(math.pi * t)
        lens(gradient, 360, 360, 0, scale).save(HERE / f"lens-done-{i:02d}.png", optimize=True)


def render_orbit_dot():
    size = 16 * SUPER
    image = Image.new("RGBA", (size, size), SWEEP_A + (0,))
    draw = ImageDraw.Draw(image)
    c = size / 2
    for r, alpha in ((8 * SUPER, 40), (5.5 * SUPER, 90), (3.2 * SUPER, 255)):
        layer = Image.new("RGBA", (size, size), SWEEP_A + (0,))
        ImageDraw.Draw(layer).ellipse((c - r, c - r, c + r, c + r), fill=SWEEP_A + (alpha,))
        image.alpha_composite(layer)
    del draw
    image.resize((16, 16), Image.LANCZOS).save(HERE / "orbit-dot.png", optimize=True)


def render_pill(name, text):
    """An amber pill button at 2x: 168 x 40 logical pixels, transparent around it."""
    w, h = 168 * 2 * SUPER, 40 * 2 * SUPER
    gradient = Image.new("RGB", (w, h))
    pixels = gradient.load()
    for x in range(w):
        color = lerp(SWEEP_A, SWEEP_B, x / (w - 1))
        for y in range(h):
            pixels[x, y] = color
    mask = Image.new("L", (w, h), 0)
    ImageDraw.Draw(mask).rounded_rectangle((0, 0, w - 1, h - 1), radius=h // 2, fill=255)
    pill = gradient.convert("RGBA")
    pill.putalpha(mask)
    font = ImageFont.truetype(FONT_BOLD, 14 * 2 * SUPER)
    draw = ImageDraw.Draw(pill)
    box = draw.textbbox((0, 0), text, font=font)
    draw.text(((w - (box[2] - box[0])) / 2 - box[0], (h - (box[3] - box[1])) / 2 - box[1]), text, font=font, fill=PILL_INK)
    pill.resize((w // SUPER, h // SUPER), Image.LANCZOS).save(HERE / f"{name}.png", optimize=True)


def render_backdrop(width, height, name):
    """The page backdrop: an indigo glow centred above the middle, where the lens sits."""
    rng = random.Random(7)
    image = Image.new("RGB", (width, height))
    pixels = image.load()
    cx, cy = width * 0.5, height * 0.36
    rx, ry = width * 0.6, height * 0.55
    for y in range(height):
        for x in range(width):
            d = math.hypot((x - cx) / rx, (y - cy) / ry)
            t = max(0.0, 1 - d / 0.7) ** 1.6 * 0.2
            # A little noise keeps the dark gradient from banding.
            t = min(1.0, max(0.0, t + (rng.random() - 0.5) / 255))
            pixels[x, y] = lerp(BACK, GLOW, t)
    image.save(HERE / f"{name}.png", optimize=True)


def main():
    for stale in HERE.glob("*.png"):
        stale.unlink()
    render_lens_frames(sweep_gradient())
    render_orbit_dot()
    render_pill("button-install", "立即安装")
    render_pill("button-finish", "完成")
    render_backdrop(596, 432, "backdrop-100")
    render_backdrop(1192, 864, "backdrop-200")


if __name__ == "__main__":
    main()
