#!/usr/bin/env python3
"""Generate the Probe Shell icon assets.

Outputs:
  - icon.png (256×256)
  - icon@512.png (512×512)
  - probe-shell.ico (multi-size Windows icon)

Direction: Windows 11-style app icon, mid-light technology blue palette.
It is intentionally not dark, but the base is deeper than the previous pale
white icon so the taskbar icon does not look like a white tile.
"""
from pathlib import Path
from PIL import Image, ImageDraw, ImageFilter


def rounded_mask(size: int, margin: int, radius: int) -> Image.Image:
    mask = Image.new("L", (size, size), 0)
    d = ImageDraw.Draw(mask)
    d.rounded_rectangle([margin, margin, size - margin, size - margin], radius=radius, fill=255)
    return mask


def gradient(size: int) -> Image.Image:
    # Medium-light tech-blue background: not white, not dark.
    tl = (199, 226, 247, 255)
    tr = (150, 203, 239, 255)
    bl = (116, 167, 226, 255)
    br = (85, 137, 210, 255)
    img = Image.new("RGBA", (size, size))
    pix = img.load()
    for y in range(size):
        fy = y / max(1, size - 1)
        for x in range(size):
            fx = x / max(1, size - 1)
            top = tuple(int(tl[i] * (1 - fx) + tr[i] * fx) for i in range(4))
            bot = tuple(int(bl[i] * (1 - fx) + br[i] * fx) for i in range(4))
            pix[x, y] = tuple(int(top[i] * (1 - fy) + bot[i] * fy) for i in range(4))
    return img


def draw_rounded_line(draw: ImageDraw.ImageDraw, points, fill, width):
    draw.line(points, fill=fill, width=width, joint="curve")
    r = width // 2
    for x, y in (points[0], points[-1]):
        draw.ellipse([x - r, y - r, x + r, y + r], fill=fill)


def make(size: int) -> Image.Image:
    s = size / 512
    img = Image.new("RGBA", (size, size), (0, 0, 0, 0))

    margin = int(18 * s)
    radius = int(104 * s)
    mask = rounded_mask(size, margin, radius)

    base = gradient(size)
    # Subtle lower-left blue depth and upper-right light source.
    fx = Image.new("RGBA", (size, size), (0, 0, 0, 0))
    fd = ImageDraw.Draw(fx)
    fd.ellipse([int(-140*s), int(270*s), int(300*s), int(690*s)], fill=(43, 100, 195, 70))
    fd.ellipse([int(210*s), int(-170*s), int(700*s), int(270*s)], fill=(255, 255, 255, 70))
    fx = fx.filter(ImageFilter.GaussianBlur(int(28*s)))
    base.alpha_composite(fx)

    # Soft shadow behind the icon body.
    shadow = Image.new("RGBA", (size, size), (0, 0, 0, 0))
    sd = ImageDraw.Draw(shadow)
    sd.rounded_rectangle([margin, margin + int(10*s), size - margin, size - margin + int(10*s)], radius=radius, fill=(33, 73, 120, 90))
    shadow = shadow.filter(ImageFilter.GaussianBlur(int(14*s)))
    img.alpha_composite(shadow)
    img.alpha_composite(Image.composite(base, Image.new("RGBA", (size, size), (0, 0, 0, 0)), mask))

    # Outer rim.
    rim = Image.new("RGBA", (size, size), (0, 0, 0, 0))
    rd = ImageDraw.Draw(rim)
    rd.rounded_rectangle([margin, margin, size - margin, size - margin], radius=radius, outline=(60, 120, 190, 115), width=max(1, int(3*s)))
    rd.rounded_rectangle([margin + int(4*s), margin + int(4*s), size - margin - int(4*s), size - margin - int(4*s)], radius=radius-int(4*s), outline=(255, 255, 255, 75), width=max(1, int(2*s)))
    img.alpha_composite(rim)

    # Orbit / network arc with a crisp white edge and blue/teal core.
    orbit = Image.new("RGBA", (size, size), (0, 0, 0, 0))
    od = ImageDraw.Draw(orbit)
    bbox = [int(154*s), int(92*s), int(464*s), int(410*s)]
    od.arc(bbox, start=205, end=35, fill=(245, 252, 255, 235), width=max(3, int(34*s)))
    od.arc(bbox, start=205, end=315, fill=(23, 192, 206, 255), width=max(2, int(25*s)))
    od.arc(bbox, start=315, end=35, fill=(54, 97, 215, 255), width=max(2, int(25*s)))
    img.alpha_composite(orbit.filter(ImageFilter.GaussianBlur(int(0.15*s))))

    # Terminal prompt: shadow then light shape.
    sym = Image.new("RGBA", (size, size), (0, 0, 0, 0))
    sd = ImageDraw.Draw(sym)
    w = max(10, int(34*s))
    pts1 = [(int(126*s), int(184*s)), (int(224*s), int(256*s)), (int(126*s), int(328*s))]
    # shadow
    draw_rounded_line(sd, [(x+int(7*s), y+int(9*s)) for x,y in pts1], (49, 98, 155, 105), w)
    sd.rounded_rectangle([int(218*s), int(318*s), int(330*s), int(348*s)], radius=int(15*s), fill=(49, 98, 155, 105))
    sym = sym.filter(ImageFilter.GaussianBlur(int(2*s)))
    img.alpha_composite(sym)

    sym = Image.new("RGBA", (size, size), (0, 0, 0, 0))
    sd = ImageDraw.Draw(sym)
    draw_rounded_line(sd, pts1, (248, 253, 255, 255), w)
    sd.rounded_rectangle([int(214*s), int(314*s), int(330*s), int(342*s)], radius=int(14*s), fill=(248, 253, 255, 255))
    img.alpha_composite(sym)

    # Nodes: white ring + colored ring + inner soft blue center.
    nd = ImageDraw.Draw(img)
    nodes = [
        (int(270*s), int(102*s), int(42*s), (16, 200, 204, 255)),
        (int(418*s), int(248*s), int(39*s), (18, 149, 215, 255)),
        (int(256*s), int(400*s), int(40*s), (76, 91, 220, 255)),
    ]
    for x, y, r, color in nodes:
        nd.ellipse([x-r-int(4*s), y-r-int(4*s), x+r+int(4*s), y+r+int(4*s)], fill=(255,255,255,235))
        nd.ellipse([x-r, y-r, x+r, y+r], fill=color)
        inner = int(r*0.48)
        nd.ellipse([x-inner, y-inner, x+inner, y+inner], fill=(225, 240, 252, 255))

    return img


if __name__ == "__main__":
    out = Path(__file__).resolve().parent
    img512 = make(512)
    img256 = make(256)
    img256.save(out / "icon.png")
    img512.save(out / "icon@512.png")
    img512.save(
        out / "probe-shell.ico",
        format="ICO",
        sizes=[(16, 16), (24, 24), (32, 32), (48, 48), (64, 64), (128, 128), (256, 256)],
    )
    print("OK: icon.png, icon@512.png, probe-shell.ico written")
