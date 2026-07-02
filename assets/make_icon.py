#!/usr/bin/env python3
"""Generate the Probe Shell icon.

Outputs:
  - icon.png (256×256)
  - icon@512.png (512×512)
  - probe-shell.ico (multi-size Windows icon)

The icon is intentionally generic and original: a glass terminal card over a
blue/violet sci-fi background, matching the Win12-style Probe Shell theme.
"""
from pathlib import Path

from PIL import Image, ImageDraw, ImageFilter, ImageFont


def font(size: int, bold: bool = False) -> ImageFont.FreeTypeFont | ImageFont.ImageFont:
    candidates = [
        "/usr/share/fonts/truetype/dejavu/DejaVuSansMono-Bold.ttf" if bold else "/usr/share/fonts/truetype/dejavu/DejaVuSansMono.ttf",
        "/usr/share/fonts/truetype/dejavu/DejaVuSans-Bold.ttf" if bold else "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
    ]
    for candidate in candidates:
        if Path(candidate).exists():
            return ImageFont.truetype(candidate, size)
    return ImageFont.load_default()


def make(size: int) -> Image.Image:
    scale = size / 512
    img = Image.new("RGBA", (size, size), (0, 0, 0, 0))

    bg = Image.new("RGBA", (size, size))
    pix = bg.load()
    for y in range(size):
        for x in range(size):
            t = (x + y) / (2 * size)
            pix[x, y] = (
                int(6 * (1 - t) + 24 * t),
                int(14 * (1 - t) + 44 * t),
                int(34 * (1 - t) + 80 * t),
                255,
            )

    mask = Image.new("L", (size, size), 0)
    d = ImageDraw.Draw(mask)
    d.rounded_rectangle(
        [int(26 * scale), int(26 * scale), size - int(26 * scale), size - int(26 * scale)],
        radius=int(112 * scale),
        fill=255,
    )
    img.alpha_composite(Image.composite(bg, Image.new("RGBA", (size, size), (0, 0, 0, 0)), mask))

    glow = Image.new("RGBA", (size, size), (0, 0, 0, 0))
    gd = ImageDraw.Draw(glow)
    gd.ellipse([int(250 * scale), int(-70 * scale), int(650 * scale), int(330 * scale)], fill=(70, 190, 255, 80))
    gd.ellipse([int(-120 * scale), int(250 * scale), int(260 * scale), int(650 * scale)], fill=(120, 85, 255, 70))
    img.alpha_composite(glow.filter(ImageFilter.GaussianBlur(int(28 * scale))))

    card = Image.new("RGBA", (size, size), (0, 0, 0, 0))
    cd = ImageDraw.Draw(card)
    rect = [int(92 * scale), int(134 * scale), int(420 * scale), int(372 * scale)]
    cd.rounded_rectangle(rect, radius=int(42 * scale), fill=(255, 255, 255, 34), outline=(135, 215, 255, 120), width=max(1, int(3 * scale)))
    for i, color in enumerate([(255, 105, 135, 210), (255, 195, 95, 210), (78, 225, 174, 210)]):
        x = int((128 + i * 36) * scale)
        y = int(170 * scale)
        r = int(8 * scale)
        cd.ellipse([x - r, y - r, x + r, y + r], fill=color)

    cd.text((int(126 * scale), int(222 * scale)), ">", font=font(int(80 * scale), True), fill=(105, 216, 255, 255))
    cd.rounded_rectangle([int(205 * scale), int(266 * scale), int(350 * scale), int(288 * scale)], radius=int(11 * scale), fill=(180, 235, 255, 235))
    cd.text((int(186 * scale), int(312 * scale)), "PS", font=font(int(34 * scale), True), fill=(235, 247, 255, 245))
    cd.arc([int(26 * scale), int(26 * scale), size - int(26 * scale), size - int(26 * scale)], start=205, end=315, fill=(255, 255, 255, 90), width=max(1, int(4 * scale)))
    img.alpha_composite(card)
    return img


if __name__ == "__main__":
    img512 = make(512)
    img256 = make(256)
    img256.save("icon.png")
    img512.save("icon@512.png")
    img512.save(
        "probe-shell.ico",
        format="ICO",
        sizes=[(16, 16), (24, 24), (32, 32), (48, 48), (64, 64), (128, 128), (256, 256)],
    )
    print("OK: icon.png, icon@512.png, probe-shell.ico written")
