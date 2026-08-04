from pathlib import Path
import re


def read(path: str) -> str:
    return Path(path).read_text(encoding="utf-8")


def write(path: str, text: str) -> None:
    Path(path).write_text(text, encoding="utf-8")


def replace_once(text: str, old: str, new: str, label: str) -> str:
    count = text.count(old)
    if count != 1:
        raise SystemExit(f"{label}: expected 1 match, found {count}")
    return text.replace(old, new, 1)


# 1) Light and pink wallpapers must render too. The old condition hid every
# wallpaper whose derived palette selected light mode, including Miku.
path = "ui/app.slint"
text = read(path)
text = replace_once(
    text,
    "if Theme.wallpaper-active && Theme.dark : Image {",
    "if Theme.wallpaper-active : Image {",
    "wallpaper layer condition",
)
write(path, text)

# 2) Restore the original blue/white procedural built-in and use optimized JPEG
# files for the four uploaded wallpapers.
path = "src/wallpaper.rs"
text = read(path)
text = replace_once(
    text,
    '        "builtin:tech" => render_tech(),\n        "builtin:aurora" => decode_aurora()?,',
    '        "builtin:tech" => render_tech(),\n        "builtin:light" => render_builtin(false),\n        "builtin:aurora" => decode_aurora()?,',
    "restore builtin light",
)
text = text.replace('decode_bundled("dark-neon-city.png")?', 'decode_bundled("dark-neon-city.jpg")?')
text = text.replace('decode_bundled("dark-cyber-warrior.png")?', 'decode_bundled("dark-cyber-warrior.jpg")?')
text = text.replace('decode_bundled("light-future-city.png")?', 'decode_bundled("light-future-city.jpg")?')
text = text.replace('decode_bundled("light-crystal-guardian.png")?', 'decode_bundled("light-crystal-guardian.jpg")?')
text = replace_once(
    text,
    '        "builtin:light" | "builtin:light-network" | "builtin:light-lab" => {\n            decode_bundled("light-future-city.jpg")?\n        }',
    '        "builtin:light-network" | "builtin:light-lab" => {\n            decode_bundled("light-future-city.jpg")?\n        }',
    "separate original light alias",
)
text = text.replace('"dark-neon-city.png" => include_bytes!("../assets/wallpapers/dark-neon-city.png")', '"dark-neon-city.jpg" => include_bytes!("../assets/wallpapers/dark-neon-city.jpg")')
text = text.replace('"dark-cyber-warrior.png" => include_bytes!("../assets/wallpapers/dark-cyber-warrior.png")', '"dark-cyber-warrior.jpg" => include_bytes!("../assets/wallpapers/dark-cyber-warrior.jpg")')
text = text.replace('"light-future-city.png" => include_bytes!("../assets/wallpapers/light-future-city.png")', '"light-future-city.jpg" => include_bytes!("../assets/wallpapers/light-future-city.jpg")')
text = text.replace('"light-crystal-guardian.png" => include_bytes!("../assets/wallpapers/light-crystal-guardian.png")', '"light-crystal-guardian.jpg" => include_bytes!("../assets/wallpapers/light-crystal-guardian.jpg")')
write(path, text)

# 3) Rebuild the wallpaper picker grouping:
# Dark: Neon City / Cyber Warrior / Fantasy 3048
# Light: original blue-white / Future City / Crystal Guardian
# Classic: Aurora / Miku
path = "ui/interface_panel.slint"
text = read(path)
start = text.index('            SectionHeader { text: root.lang-en ? "DARK"')
end = text.index('            Rectangle { height: 12px; }\n            SectionHeader { text: root.lang-en ? "CUSTOM"', start)
new_picker = r'''            SectionHeader { text: root.lang-en ? "DARK" : "深色壁纸"; height: 20px; }
            HorizontalLayout {
                spacing: 11px;
                alignment: start;
                VerticalLayout {
                    spacing: 5px;
                    WallpaperSwatch {
                        selected: root.current-wallpaper == "builtin:dark-neon-city"
                            || root.current-wallpaper == "builtin:dark"
                            || root.current-wallpaper == "builtin:dark-city"
                            || root.current-wallpaper == "builtin:dark-network";
                        clicked => { root.set-wallpaper("builtin:dark-neon-city"); }
                        Image { width: 100%; height: 100%; source: @image-url("../assets/wallpapers/dark-neon-city.jpg"); image-fit: cover; }
                    }
                    Text { text: root.lang-en ? "Neon City" : "霓虹城市"; font-size: 11px * Theme.panel-font; color: Theme.text-secondary; horizontal-alignment: center; }
                }
                VerticalLayout {
                    spacing: 5px;
                    WallpaperSwatch {
                        selected: root.current-wallpaper == "builtin:dark-cyber-warrior"
                            || root.current-wallpaper == "builtin:dark-mecha";
                        clicked => { root.set-wallpaper("builtin:dark-cyber-warrior"); }
                        Image { width: 100%; height: 100%; source: @image-url("../assets/wallpapers/dark-cyber-warrior.jpg"); image-fit: cover; }
                    }
                    Text { text: root.lang-en ? "Cyber Warrior" : "赛博战姬"; font-size: 11px * Theme.panel-font; color: Theme.text-secondary; horizontal-alignment: center; }
                }
                VerticalLayout {
                    spacing: 5px;
                    WallpaperSwatch {
                        selected: root.current-wallpaper == "builtin:tech";
                        clicked => { root.set-wallpaper("builtin:tech"); }
                        Rectangle {
                            width: 100%; height: 100%;
                            background: @linear-gradient(180deg, #05060f 0%, #0b1430 55%, #1a0a2e 100%);
                            clip: true;
                            Rectangle {
                                x: parent.width * 0.5 - 16px; y: parent.height * 0.22;
                                width: 32px; height: 32px; border-radius: 16px;
                                background: @radial-gradient(circle, #ff5ea8 0%, #ff5ea800 70%);
                            }
                            Rectangle { y: parent.height * 0.58; width: parent.width; height: 2px; background: #19e6ff; opacity: 0.85; }
                        }
                    }
                    Text { text: root.lang-en ? "Fantasy 3048" : "幻想3048"; font-size: 11px * Theme.panel-font; color: Theme.text-secondary; horizontal-alignment: center; }
                }
            }

            Rectangle { height: 10px; }
            SectionHeader { text: root.lang-en ? "LIGHT" : "浅色壁纸"; height: 20px; }
            HorizontalLayout {
                spacing: 11px;
                alignment: start;
                VerticalLayout {
                    spacing: 5px;
                    WallpaperSwatch {
                        selected: root.current-wallpaper == "builtin:light";
                        clicked => { root.set-wallpaper("builtin:light"); }
                        Rectangle {
                            width: 100%; height: 100%;
                            background: @linear-gradient(145deg, #f4f8ff 0%, #dbe8f8 58%, #bfd3ed 100%);
                            Rectangle {
                                x: parent.width * 0.62; y: -10px;
                                width: 62px; height: 62px; border-radius: 31px;
                                background: @radial-gradient(circle, #79b9ff99 0%, #79b9ff00 72%);
                            }
                        }
                    }
                    Text { text: root.lang-en ? "Blue & White" : "蓝白经典"; font-size: 11px * Theme.panel-font; color: Theme.text-secondary; horizontal-alignment: center; }
                }
                VerticalLayout {
                    spacing: 5px;
                    WallpaperSwatch {
                        selected: root.current-wallpaper == "builtin:light-future-city"
                            || root.current-wallpaper == "builtin:light-network"
                            || root.current-wallpaper == "builtin:light-lab";
                        clicked => { root.set-wallpaper("builtin:light-future-city"); }
                        Image { width: 100%; height: 100%; source: @image-url("../assets/wallpapers/light-future-city.jpg"); image-fit: cover; }
                    }
                    Text { text: root.lang-en ? "Future City" : "未来之城"; font-size: 11px * Theme.panel-font; color: Theme.text-secondary; horizontal-alignment: center; }
                }
                VerticalLayout {
                    spacing: 5px;
                    WallpaperSwatch {
                        selected: root.current-wallpaper == "builtin:light-crystal-guardian"
                            || root.current-wallpaper == "builtin:light-crystal";
                        clicked => { root.set-wallpaper("builtin:light-crystal-guardian"); }
                        Image { width: 100%; height: 100%; source: @image-url("../assets/wallpapers/light-crystal-guardian.jpg"); image-fit: cover; }
                    }
                    Text { text: root.lang-en ? "Crystal Guardian" : "冰晶守望"; font-size: 11px * Theme.panel-font; color: Theme.text-secondary; horizontal-alignment: center; }
                }
            }

            Rectangle { height: 10px; }
            SectionHeader { text: root.lang-en ? "CLASSIC" : "经典壁纸"; height: 20px; }
            HorizontalLayout {
                spacing: 11px;
                alignment: start;
                VerticalLayout {
                    spacing: 5px;
                    WallpaperSwatch {
                        selected: root.current-wallpaper == "builtin:aurora";
                        clicked => { root.set-wallpaper("builtin:aurora"); }
                        Image { width: 100%; height: 100%; source: @image-url("../assets/aurora.jpg"); image-fit: cover; }
                    }
                    Text { text: root.lang-en ? "Aurora" : "极光"; font-size: 11px * Theme.panel-font; color: Theme.text-secondary; horizontal-alignment: center; }
                }
                VerticalLayout {
                    spacing: 5px;
                    WallpaperSwatch {
                        selected: root.current-wallpaper == "builtin:miku";
                        clicked => { root.set-wallpaper("builtin:miku"); }
                        Image { width: 100%; height: 100%; source: @image-url("../assets/miku.jpg"); image-fit: cover; }
                    }
                    Text { text: "Miku"; font-size: 11px * Theme.panel-font; color: Theme.text-secondary; horizontal-alignment: center; }
                }
            }

'''
text = text[:start] + new_picker + text[end:]
write(path, text)

# 4) Version and changelog.
path = "Cargo.toml"
text = replace_once(read(path), 'version = "0.7.1"', 'version = "0.7.2"', "Cargo version")
write(path, text)

path = "Cargo.lock"
text, count = re.subn(
    r'(name = "probe-shell"\nversion = ")0\.7\.1("\n)',
    r'\g<1>0.7.2\2',
    read(path),
    count=1,
)
if count != 1:
    raise SystemExit(f"Cargo.lock version: expected 1 match, found {count}")
write(path, text)

path = "CHANGELOG.md"
text = read(path)
entry = '''## v0.7.2

- 修复浅色壁纸与 Miku 选择后不显示的问题。
- 恢复原来的蓝白内置壁纸，并将极光与 Miku 调整到“经典壁纸”。
- 四张新壁纸改为适合桌面显示的优化 JPEG，显著降低安装包体积。

'''
if not text.startswith("## v0.7.2"):
    write(path, entry + text)
