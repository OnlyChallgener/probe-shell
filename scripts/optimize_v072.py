from pathlib import Path
from PIL import Image, ImageOps
import json

names = [
    "dark-neon-city",
    "dark-cyber-warrior",
    "light-future-city",
    "light-crystal-guardian",
]
resampling = getattr(Image, "Resampling", Image)
report = []

for name in names:
    src = Path("assets/wallpapers") / f"{name}.png"
    dst = Path("assets/wallpapers") / f"{name}.jpg"
    before = src.stat().st_size
    with Image.open(src) as image:
        image = ImageOps.exif_transpose(image)
        original_dimensions = image.size
        image.thumbnail((1920, 1080), resampling.LANCZOS)
        if image.mode in ("RGBA", "LA"):
            background = Image.new("RGB", image.size, (18, 22, 30))
            background.paste(image, mask=image.getchannel("A"))
            image = background
        else:
            image = image.convert("RGB")
        image.save(
            dst,
            format="JPEG",
            quality=89,
            subsampling=1,
            optimize=True,
            progressive=True,
        )
        final_dimensions = image.size
    after = dst.stat().st_size
    report.append(
        {
            "name": name,
            "original_dimensions": original_dimensions,
            "final_dimensions": final_dimensions,
            "png_bytes": before,
            "jpeg_bytes": after,
            "saved_bytes": before - after,
        }
    )
    src.unlink()

print(json.dumps(report, ensure_ascii=False, indent=2))
print("total_saved_bytes:", sum(item["saved_bytes"] for item in report))
