from pathlib import Path
import re


def read(path: str) -> str:
    return Path(path).read_text(encoding="utf-8")


def write(path: str, text: str) -> None:
    Path(path).write_text(text, encoding="utf-8")


def replace_once(text: str, old: str, new: str, label: str) -> str:
    count = text.count(old)
    if count != 1:
        raise SystemExit(f"{label}: expected exactly one match, found {count}")
    return text.replace(old, new, 1)


# Light wallpapers were successfully loaded, but Theme.frost returned fully opaque
# light surfaces whenever Theme.dark was false. That covered the wallpaper image
# with the same blue/white daytime palette. Miku is also classified as light by
# its average luminance, so it followed the same hidden path.
path = "ui/theme.slint"
text = read(path)

text = replace_once(
    text,
    "// Direction: practical Windows 11-style surfaces. Dark mode may keep the\n"
    "// immersive wallpaper; light mode is intentionally opaque, muted and readable\n"
    "// so terminal/SFTP text never turns into a white haze.\n",
    "// Direction: practical Windows 11-style surfaces. Both dark and light themes\n"
    "// support immersive wallpapers; light mode uses a slightly stronger frost so\n"
    "// terminal/SFTP text remains readable without hiding the selected image.\n",
    "theme direction comment",
)

old_frost = '''    pure function frost(base: color, alpha: float) -> brush {
        // Wallpaper immersion is intentionally dark-mode only. In light mode the
        // old frosted surfaces became a grey/white wash over the sci-fi wallpaper,
        // making text and SFTP tables hard to read. Keep light mode crisp and
        // opaque while preserving the immersive wallpaper in dark mode.
        return root.wallpaper-active && root.dark
            ? base.mix(root.wp-tint, 1.0 - root.tint-amt).with-alpha(alpha)
            : base;
    }
'''
new_frost = '''    pure function frost(base: color, alpha: float) -> brush {
        // Keep the selected wallpaper visible in both themes. Light mode uses a
        // slightly stronger frost than dark mode for text/table contrast, but it
        // must not fall back to a fully opaque blue/white surface: that previously
        // hid Future City, Crystal Guardian and Miku even though they were loaded.
        return root.wallpaper-active
            ? base.mix(root.wp-tint, 1.0 - root.tint-amt).with-alpha(
                root.dark ? alpha : min(1.0, alpha + 0.04)
              )
            : base;
    }
'''
text = replace_once(text, old_frost, new_frost, "light wallpaper frost")

text = replace_once(
    text,
    "    out property <brush> accent:         (root.wallpaper-active && root.dark) ? root.wp-accent          : (dark ? #59b8ff : #3178d4);\n"
    "    out property <brush> accent-hover:   (root.wallpaper-active && root.dark) ? root.wp-accent.brighter(0.12) : (dark ? #7cc8ff : #4489e1);\n"
    "    out property <brush> accent-pressed: (root.wallpaper-active && root.dark) ? root.wp-accent.darker(0.12)   : (dark ? #2f8fd3 : #2264b5);\n",
    "    out property <brush> accent:         root.wallpaper-active ? root.wp-accent                 : (dark ? #59b8ff : #3178d4);\n"
    "    out property <brush> accent-hover:   root.wallpaper-active ? root.wp-accent.brighter(0.12) : (dark ? #7cc8ff : #4489e1);\n"
    "    out property <brush> accent-pressed: root.wallpaper-active ? root.wp-accent.darker(0.12)   : (dark ? #2f8fd3 : #2264b5);\n",
    "wallpaper accent in light mode",
)

text = replace_once(
    text,
    "    out property <brush> term-bg: (root.wallpaper-active && root.dark) ? base-term-bg.with-alpha(max(0.0, root.panel-alpha - 0.14)) : base-term-bg;\n",
    "    out property <brush> term-bg: root.wallpaper-active\n"
    "        ? base-term-bg.with-alpha(root.dark\n"
    "            ? max(0.0, root.panel-alpha - 0.14)\n"
    "            : min(1.0, root.panel-alpha + 0.02))\n"
    "        : base-term-bg;\n",
    "terminal frost in light mode",
)

if "root.wallpaper-active && root.dark" in text:
    raise SystemExit("theme.slint still contains a dark-only wallpaper gate")
write(path, text)

# Version bump.
path = "Cargo.toml"
text = replace_once(read(path), 'version = "0.7.2"', 'version = "0.7.3"', "Cargo version")
write(path, text)

path = "Cargo.lock"
text, count = re.subn(
    r'(name = "probe-shell"\nversion = ")0\.7\.2("\n)',
    r'\g<1>0.7.3\2',
    read(path),
    count=1,
)
if count != 1:
    raise SystemExit(f"Cargo.lock version: expected one match, found {count}")
write(path, text)

path = "CHANGELOG.md"
text = read(path)
entry = '''## v0.7.3

- 修复未来之城、冰晶守望等浅色壁纸加载后被日间模式蓝白底层遮住的问题。
- 修复 Miku 因亮度识别为浅色而无法显示的问题。
- 日间模式改为可读性更强的半透明磨砂层，并允许壁纸配色驱动强调色与终端背景。

'''
if not text.startswith("## v0.7.3"):
    write(path, entry + text)
