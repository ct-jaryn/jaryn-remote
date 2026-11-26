#!/usr/bin/env python3
"""Render the Jaryn Remote vector mark to a macOS iconset and PNG preview.

Requires Pillow. The bundled app consumes the generated assets/JarynRemote.icns;
packaging itself has no Python dependency.
"""

from pathlib import Path
import subprocess
import tempfile

from PIL import Image, ImageDraw


ROOT = Path(__file__).resolve().parents[1]
ASSETS = ROOT / "assets"
SCALE = 2
SIZE = 1024


def scaled_box(box):
    return tuple(round(value * SCALE) for value in box)


def cubic(a, b, c, d, count=36):
    for index in range(count + 1):
        t = index / count
        u = 1 - t
        yield (
            u**3 * a[0] + 3 * u**2 * t * b[0] + 3 * u * t**2 * c[0] + t**3 * d[0],
            u**3 * a[1] + 3 * u**2 * t * b[1] + 3 * u * t**2 * c[1] + t**3 * d[1],
        )


def render():
    canvas = Image.new("RGBA", (SIZE * SCALE, SIZE * SCALE), (0, 0, 0, 0))
    gradient = Image.new("RGBA", canvas.size)
    pixels = gradient.load()
    for y in range(gradient.height):
        for x in range(gradient.width):
            t = min(1.0, (x + y) / (2 * gradient.width))
            pixels[x, y] = (
                round(11 + 22 * t),
                round(23 + 36 * t),
                round(56 + 74 * t),
                255,
            )
    mask = Image.new("L", canvas.size)
    ImageDraw.Draw(mask).rounded_rectangle(scaled_box((32, 32, 992, 992)), radius=224 * SCALE, fill=255)
    canvas.paste(gradient, (0, 0), mask)
    draw = ImageDraw.Draw(canvas)
    draw.rounded_rectangle(scaled_box((174, 332, 688, 684)), radius=72 * SCALE,
                           fill="#29447F", outline="#7288E4", width=34 * SCALE)
    draw.rounded_rectangle(scaled_box((286, 246, 848, 658)), radius=74 * SCALE,
                           fill="#122956", outline="#F7FAFF", width=42 * SCALE)

    path = [(486, 386), (650, 386), (650, 493)]
    path += list(cubic((650, 493), (650, 564), (600, 602), (535, 602)))[1:]
    path += list(cubic((535, 602), (493, 602), (457, 584), (438, 556)))[1:]
    points = [(round(x * SCALE), round(y * SCALE)) for x, y in path]
    draw.line(points, fill="#53E8DF", width=60 * SCALE, joint="curve")
    for point in (points[0], points[-1]):
        radius = 30 * SCALE
        draw.ellipse((point[0] - radius, point[1] - radius,
                      point[0] + radius, point[1] + radius), fill="#53E8DF")

    for a, b in (((568, 680), (568, 740)), ((468, 758), (668, 758))):
        draw.line((scaled_box(a + b)), fill="#F7FAFF", width=46 * SCALE)
        for point in (a, b):
            x, y = (coord * SCALE for coord in point)
            radius = 23 * SCALE
            draw.ellipse((x - radius, y - radius, x + radius, y + radius), fill="#F7FAFF")
    return canvas.resize((SIZE, SIZE), Image.Resampling.LANCZOS)


def main():
    ASSETS.mkdir(exist_ok=True)
    image = render()
    image.save(ASSETS / "jaryn-remote-logo.png")
    with tempfile.TemporaryDirectory(prefix="jaryn-icon-") as temporary:
        iconset = Path(temporary) / "JarynRemote.iconset"
        iconset.mkdir()
        for points in (16, 32, 128, 256, 512):
            image.resize((points, points), Image.Resampling.LANCZOS).save(iconset / f"icon_{points}x{points}.png")
            image.resize((points * 2, points * 2), Image.Resampling.LANCZOS).save(iconset / f"icon_{points}x{points}@2x.png")
        subprocess.run(["iconutil", "-c", "icns", str(iconset), "-o", str(ASSETS / "JarynRemote.icns")], check=True)


if __name__ == "__main__":
    main()
