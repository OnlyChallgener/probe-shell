from pathlib import Path

path = Path("src/wallpaper.rs")
text = path.read_text(encoding="utf-8")

names = [
    "dark-neon-city",
    "dark-cyber-warrior",
    "light-future-city",
    "light-crystal-guardian",
]

for name in names:
    text = text.replace(f'"{name}.png"', f'"{name}.jpg"')
    text = text.replace(
        f'../assets/wallpapers/{name}.png',
        f'../assets/wallpapers/{name}.jpg',
    )

remaining = [name for name in names if f'{name}.png' in text]
if remaining:
    raise SystemExit(f"remaining PNG references in src/wallpaper.rs: {remaining}")

path.write_text(text, encoding="utf-8")
