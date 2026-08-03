from pathlib import Path
import re

VERSION = "0.7.0"


def must_replace(text: str, old: str, new: str, label: str) -> str:
    if old not in text:
        raise SystemExit(f"Patch anchor not found: {label}")
    return text.replace(old, new, 1)


# Version. Cargo refreshes Cargo.lock before the locked test.
cargo = Path("Cargo.toml")
text = cargo.read_text(encoding="utf-8")
text, count = re.subn(
    r'(?ms)(\[package\].*?^version\s*=\s*")[^"]+("\s*$)',
    rf'\g<1>{VERSION}\2',
    text,
    count=1,
)
if count != 1:
    raise SystemExit("Could not update Cargo.toml package version")
cargo.write_text(text, encoding="utf-8")

# New installs default to the female mecha wallpaper; legacy ids remain valid.
config = Path("src/config.rs")
text = config.read_text(encoding="utf-8")
text = text.replace(
    '"builtin:tech".to_string()',
    '"builtin:dark-mecha".to_string()',
)
text = text.replace(
    "fn wallpaper_defaults_to_tech_but_keeps_explicit_choice()",
    "fn wallpaper_defaults_to_mecha_but_keeps_explicit_choice()",
)
text = text.replace(
    'assert_eq!(fresh_config().wallpaper, "builtin:tech");',
    'assert_eq!(fresh_config().wallpaper, "builtin:dark-mecha");',
)
text = text.replace(
    'assert_eq!(cfg.wallpaper, "builtin:tech");',
    'assert_eq!(cfg.wallpaper, "builtin:dark-mecha");',
    1,
)
config.write_text(text, encoding="utf-8")

wallpaper = Path("src/wallpaper.rs")
text = wallpaper.read_text(encoding="utf-8")
old_match = '''    let buf = match id {
        "builtin:light" => render_builtin(false),
        "builtin:dark" => render_builtin(true),
        "builtin:tech" => render_tech(),
        "builtin:aurora" | "builtin:miku" => decode_aurora()?,
        path => decode_custom(path)?,
    };'''
new_match = '''    let buf = match id {
        // Legacy ids are preserved so existing configs never break.
        "builtin:light" | "builtin:light-crystal" => decode_bundled("light-crystal.jpg")?,
        "builtin:dark" | "builtin:dark-mecha" => decode_bundled("dark-mecha.jpg")?,
        "builtin:tech" | "builtin:dark-network" => decode_bundled("dark-network.jpg")?,
        "builtin:dark-city" => decode_bundled("dark-city.jpg")?,
        "builtin:light-network" => decode_bundled("light-network.jpg")?,
        "builtin:light-lab" => decode_bundled("light-lab.jpg")?,
        "builtin:aurora" | "builtin:miku" => decode_aurora()?,
        path => decode_custom(path)?,
    };'''
text = must_replace(text, old_match, new_match, "wallpaper load match")

text, count = re.subn(
    r'pub fn is_builtin\(id: &str\) -> bool \{.*?\n\}',
    '''pub fn is_builtin(id: &str) -> bool {
    matches!(
        id,
        "builtin:light"
            | "builtin:dark"
            | "builtin:tech"
            | "builtin:aurora"
            | "builtin:miku"
            | "builtin:dark-mecha"
            | "builtin:dark-city"
            | "builtin:dark-network"
            | "builtin:light-crystal"
            | "builtin:light-network"
            | "builtin:light-lab"
    )
}''',
    text,
    count=1,
    flags=re.S,
)
if count != 1:
    raise SystemExit("Could not patch wallpaper is_builtin")

bundled_fn = '''
/// Decode one of Probe Shell's bundled wallpaper assets.
fn decode_bundled(name: &str) -> Option<SharedPixelBuffer<Rgba8Pixel>> {
    let bytes: &[u8] = match name {
        "dark-mecha.jpg" => include_bytes!("../assets/wallpapers/dark-mecha.jpg"),
        "dark-city.jpg" => include_bytes!("../assets/wallpapers/dark-city.jpg"),
        "dark-network.jpg" => include_bytes!("../assets/wallpapers/dark-network.jpg"),
        "light-crystal.jpg" => include_bytes!("../assets/wallpapers/light-crystal.jpg"),
        "light-network.jpg" => include_bytes!("../assets/wallpapers/light-network.jpg"),
        "light-lab.jpg" => include_bytes!("../assets/wallpapers/light-lab.jpg"),
        _ => return None,
    };
    Some(to_buffer(image::load_from_memory(bytes).ok()?.to_rgba8()))
}

'''
anchor = "fn decode_custom(path: &str) -> Option<SharedPixelBuffer<Rgba8Pixel>> {"
if "fn decode_bundled(" not in text:
    if anchor not in text:
        raise SystemExit("Could not find decode_custom insertion point")
    text = text.replace(anchor, bundled_fn + anchor, 1)
wallpaper.write_text(text, encoding="utf-8")

panel = Path("ui/interface_panel.slint")
text = panel.read_text(encoding="utf-8")
text, count = re.subn(
    r'    property <bool> wp-is-custom:\n.*?;\n\n    callback set-term-font',
    '''    property <bool> wp-is-custom:
        root.current-wallpaper != ""
        && root.current-wallpaper != "builtin:light"
        && root.current-wallpaper != "builtin:dark"
        && root.current-wallpaper != "builtin:tech"
        && root.current-wallpaper != "builtin:aurora"
        && root.current-wallpaper != "builtin:miku"
        && root.current-wallpaper != "builtin:dark-mecha"
        && root.current-wallpaper != "builtin:dark-city"
        && root.current-wallpaper != "builtin:dark-network"
        && root.current-wallpaper != "builtin:light-crystal"
        && root.current-wallpaper != "builtin:light-network"
        && root.current-wallpaper != "builtin:light-lab";

    callback set-term-font''',
    text,
    count=1,
    flags=re.S,
)
if count != 1:
    raise SystemExit("Could not patch custom-wallpaper detection")

wallpaper_page = r'''            // --- Wallpaper page ----------------------------------------
            if root.ifd-page == "wallpaper" : VerticalLayout {
                padding-left: 20px;
                padding-right: 20px;
                padding-top: 14px;
                spacing: 0;
                alignment: start;

                SectionHeader { text: root.lang-en ? "WALLPAPER" : "壁纸"; height: 24px; }
                Text {
                    text: root.lang-en
                        ? "Six built-in wallpapers are tuned for terminal readability. Light themes use misty blue-grey rather than bright white."
                        : "六张内置壁纸均针对终端文字可读性调整；日间主题采用雾蓝灰，不使用刺眼纯白。";
                    color: Theme.text-muted;
                    font-size: 11px * Theme.panel-font;
                    wrap: word-wrap;
                }
                Rectangle { height: 10px; }

                SectionHeader { text: root.lang-en ? "DARK" : "深色模式"; height: 20px; }
                HorizontalLayout {
                    spacing: 11px;
                    alignment: start;
                    VerticalLayout {
                        spacing: 5px;
                        WallpaperSwatch {
                            selected: root.current-wallpaper == "builtin:dark-mecha" || root.current-wallpaper == "builtin:dark";
                            clicked => { root.set-wallpaper("builtin:dark-mecha"); }
                            Image { width: 100%; height: 100%; source: @image-url("../assets/wallpapers/dark-mecha.jpg"); image-fit: cover; }
                        }
                        Text { text: root.lang-en ? "Mecha Core" : "机甲核心"; font-size: 11px * Theme.panel-font; color: Theme.text-secondary; horizontal-alignment: center; }
                    }
                    VerticalLayout {
                        spacing: 5px;
                        WallpaperSwatch {
                            selected: root.current-wallpaper == "builtin:dark-city";
                            clicked => { root.set-wallpaper("builtin:dark-city"); }
                            Image { width: 100%; height: 100%; source: @image-url("../assets/wallpapers/dark-city.jpg"); image-fit: cover; }
                        }
                        Text { text: root.lang-en ? "Cyber City" : "赛博城市"; font-size: 11px * Theme.panel-font; color: Theme.text-secondary; horizontal-alignment: center; }
                    }
                    VerticalLayout {
                        spacing: 5px;
                        WallpaperSwatch {
                            selected: root.current-wallpaper == "builtin:dark-network" || root.current-wallpaper == "builtin:tech";
                            clicked => { root.set-wallpaper("builtin:dark-network"); }
                            Image { width: 100%; height: 100%; source: @image-url("../assets/wallpapers/dark-network.jpg"); image-fit: cover; }
                        }
                        Text { text: root.lang-en ? "Network Space" : "网络星空"; font-size: 11px * Theme.panel-font; color: Theme.text-secondary; horizontal-alignment: center; }
                    }
                }

                Rectangle { height: 10px; }
                SectionHeader { text: root.lang-en ? "LIGHT" : "日间模式"; height: 20px; }
                HorizontalLayout {
                    spacing: 11px;
                    alignment: start;
                    VerticalLayout {
                        spacing: 5px;
                        WallpaperSwatch {
                            selected: root.current-wallpaper == "builtin:light-crystal" || root.current-wallpaper == "builtin:light";
                            clicked => { root.set-wallpaper("builtin:light-crystal"); }
                            Image { width: 100%; height: 100%; source: @image-url("../assets/wallpapers/light-crystal.jpg"); image-fit: cover; }
                        }
                        Text { text: root.lang-en ? "Crystal Future" : "冰晶未来"; font-size: 11px * Theme.panel-font; color: Theme.text-secondary; horizontal-alignment: center; }
                    }
                    VerticalLayout {
                        spacing: 5px;
                        WallpaperSwatch {
                            selected: root.current-wallpaper == "builtin:light-network";
                            clicked => { root.set-wallpaper("builtin:light-network"); }
                            Image { width: 100%; height: 100%; source: @image-url("../assets/wallpapers/light-network.jpg"); image-fit: cover; }
                        }
                        Text { text: root.lang-en ? "Network Atlas" : "网络星图"; font-size: 11px * Theme.panel-font; color: Theme.text-secondary; horizontal-alignment: center; }
                    }
                    VerticalLayout {
                        spacing: 5px;
                        WallpaperSwatch {
                            selected: root.current-wallpaper == "builtin:light-lab";
                            clicked => { root.set-wallpaper("builtin:light-lab"); }
                            Image { width: 100%; height: 100%; source: @image-url("../assets/wallpapers/light-lab.jpg"); image-fit: cover; }
                        }
                        Text { text: root.lang-en ? "Future Lab" : "未来实验室"; font-size: 11px * Theme.panel-font; color: Theme.text-secondary; horizontal-alignment: center; }
                    }
                }

                Rectangle { height: 10px; }
                HorizontalLayout {
                    spacing: 11px;
                    alignment: start;
                    VerticalLayout {
                        spacing: 5px;
                        WallpaperSwatch {
                            width: 100px;
                            selected: root.current-wallpaper == "";
                            clicked => { root.set-wallpaper(""); }
                            Rectangle { width: 100%; height: 100%; background: Theme.window-base; }
                            Text { text: "\u{E14C}"; font-family: "Material Icons"; font-size: 26px * Theme.panel-font; color: Theme.text-muted; horizontal-alignment: center; vertical-alignment: center; }
                        }
                        Text { text: root.lang-en ? "None" : "无"; font-size: 11px * Theme.panel-font; color: Theme.text-secondary; horizontal-alignment: center; }
                    }
                    VerticalLayout {
                        spacing: 5px;
                        WallpaperSwatch {
                            width: 150px;
                            selected: root.wp-is-custom;
                            clicked => { root.pick-wallpaper-file(); }
                            Rectangle { width: 100%; height: 100%; background: Theme.bg-elevated; }
                            Text { text: "\u{E2C7}"; font-family: "Material Icons"; font-size: 25px * Theme.panel-font; color: Theme.accent; horizontal-alignment: center; vertical-alignment: center; }
                        }
                        Text {
                            text: root.custom-wallpaper-name != "" ? root.custom-wallpaper-name : (root.lang-en ? "Custom image" : "自定义图片");
                            font-size: 11px * Theme.panel-font;
                            color: Theme.text-secondary;
                            horizontal-alignment: center;
                        }
                    }
                }

                Rectangle { height: 8px; }
                SettingRow {
                    label: root.lang-en ? "Panel opacity" : "面板透明度";
                    desc: root.lang-en ? "Higher values improve text contrast on detailed images." : "壁纸细节较多时，提高数值可增强文字对比度。";
                    show-divider: false;
                    Stepper {
                        value: round(root.wallpaper-overlay * 100);
                        minimum: 40;
                        maximum: 100;
                        step: 5;
                        unit: "%";
                        changed(v) => { root.persist-wallpaper-overlay(v / 100); }
                    }
                }
            }

'''
pattern = r'            // --- Wallpaper page -+\n.*?(?=            // --- SFTP page)'
text, count = re.subn(pattern, lambda _match: wallpaper_page, text, count=1, flags=re.S)
if count != 1:
    raise SystemExit("Could not replace wallpaper settings page")
panel.write_text(text, encoding="utf-8")
