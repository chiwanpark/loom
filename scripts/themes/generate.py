#!/usr/bin/env python3
"""Generate gpui-component theme files from terminal color schemes.

terminalcolors.json holds the terminal palettes published at
https://terminalcolors.com/ (background, foreground, cursor, selection, the
16 ANSI colors, and the site's accent color for each variant), with the
author, upstream URL, and license of each color scheme.

Every UI and syntax color is derived from those palettes, so the generated
themes keep the terminal colors exactly where Loom's terminal reads them back
(base.* and base.*.light) and approximate the rest.

Usage: scripts/themes/generate.py  (writes crates/loom-gui/assets/themes/)
"""

import json
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
DATA = Path(__file__).with_name("terminalcolors.json")
OUT = ROOT / "crates/loom-gui/assets/themes"
SCHEMA = "https://github.com/longbridge/gpui-kit/raw/refs/heads/main/.theme-schema.json"

# Variants already bundled from gpui-component (or hand-written, for Gotham).
BUNDLED = {
    ("ayu", "dark"),
    ("ayu", "light"),
    ("catppuccin", "frappe"),
    ("catppuccin", "latte"),
    ("catppuccin", "macchiato"),
    ("catppuccin", "mocha"),
    ("everforest", "dark"),
    ("everforest", "light"),
    ("gotham", "default"),
    ("gruvbox", "dark"),
    ("gruvbox", "light"),
    ("solarized", "dark"),
    ("solarized", "light"),
    ("tokyo-night", "default"),
    ("tokyo-night", "moon"),
    ("tokyo-night", "storm"),
}

# Families that already have a bundled file get the remaining variants in a
# separate file, so the files copied from gpui-component stay unmodified.
FILE_NAMES = {
    "ayu": "ayu-mirage",
    "gruvbox": "gruvbox-contrast",
    "tokyo-night": "tokyonight-day",
}

# Families whose variants are color schemes of their own upstream.
VARIANT_NAMES_ONLY = {"nightfox", "zenbones"}


def rgb(hex_color):
    h = hex_color.lstrip("#")
    return tuple(int(h[i : i + 2], 16) for i in (0, 2, 4))


def hex_of(c):
    return "#" + "".join(f"{max(0, min(255, round(v))):02x}" for v in c)


def mix(a, b, t):
    """Blend from color a towards color b by t (0..1)."""
    ca, cb = rgb(a), rgb(b)
    return hex_of(tuple(x + (y - x) * t for x, y in zip(ca, cb)))


def luminance(c):
    def channel(v):
        v /= 255
        return v / 12.92 if v <= 0.04045 else ((v + 0.055) / 1.055) ** 2.4

    r, g, b = (channel(v) for v in rgb(c))
    return 0.2126 * r + 0.7152 * g + 0.0722 * b


def contrast(a, b):
    la, lb = sorted((luminance(a), luminance(b)), reverse=True)
    return (la + 0.05) / (lb + 0.05)


def readable(color, bg, fg, minimum=3.0):
    """Move color towards fg until it reaches the minimum contrast on bg."""
    t = 0.0
    out = color
    while contrast(out, bg) < minimum and t < 1.0:
        t += 0.05
        out = mix(color, fg, t)
    return out


def on(color, bg, fg):
    """The text color, of bg and fg, that reads best on color."""
    return bg if contrast(color, bg) >= contrast(color, fg) else fg


def button(color, bg, fg, minimum=3.0):
    """A background near color, with bg or fg text reaching the minimum contrast."""
    best = None
    for target in (bg, fg):
        t = 0.0
        out = color
        while contrast(out, on(out, bg, fg)) < minimum and t < 1.0:
            t += 0.05
            out = mix(color, target, t)
        if best is None or t < best[0]:
            best = (t, out)
    out = best[1]
    return out, on(out, bg, fg)


def alpha(color, a):
    return f"{color}{a:02x}"


def theme_name(family, variant):
    if variant["id"] == "default":
        return family["name"]
    if family["id"] in VARIANT_NAMES_ONLY:
        return variant["name"]
    if variant["id"] == "default":
        return family["name"]
    return f"{family['name']} {variant['name']}"


def build(family, variant):
    bg = variant["background"]
    fg = variant["foreground"]
    p = variant["palette"]
    dark = luminance(bg) < luminance(fg)

    surface = mix(bg, fg, 0.05)
    hover = mix(bg, fg, 0.09)
    active = mix(bg, fg, 0.14)
    border = mix(bg, fg, 0.16)
    # Bright black is the terminal's dim text; use it unless it is unreadable
    # or as strong as the foreground.
    muted_fg = p[8]
    if not 3.0 <= contrast(muted_fg, bg) <= 0.75 * contrast(fg, bg):
        muted_fg = readable(mix(fg, bg, 0.45), bg, fg, 3.0)
    line_number = mix(muted_fg, bg, 0.35)

    accent = readable(variant["accent"], bg, fg, 2.0)
    primary, primary_fg = button(accent, bg, fg)
    danger, danger_fg = button(p[1], bg, fg)
    success, success_fg = button(p[2], bg, fg)
    warning, warning_fg = button(p[3], bg, fg)
    info, info_fg = button(p[4], bg, fg)

    selection = variant["selection_background"]
    if contrast(selection, fg) < 4.5:
        t = 0.45
        blended = mix(bg, selection, t)
        while contrast(blended, fg) < 4.5 and t > 0.1:
            t -= 0.05
            blended = mix(bg, selection, t)
        selection = blended

    def text(i):
        return readable(p[i], bg, fg)

    red, green, yellow, blue, magenta, cyan = (text(i) for i in range(1, 7))
    bright_red = text(9)

    colors = {
        "accent.background": hover,
        "accent.foreground": fg,
        "background": bg,
        "border": border,
        "caret": variant["cursor"],
        "danger.background": danger,
        "danger.foreground": danger_fg,
        "foreground": fg,
        "info.background": info,
        "info.foreground": info_fg,
        "input.border": border,
        "link.active.foreground": fg,
        "link.foreground": blue,
        "link.hover.foreground": readable(p[12], bg, fg),
        "list.active.background": alpha(accent, 0x33),
        "list.active.border": accent,
        "list.even.background": surface,
        "list.head.background": surface,
        "muted.background": surface,
        "muted.foreground": muted_fg,
        "panel.background": surface,
        "popover.background": surface,
        "popover.foreground": fg,
        "primary.background": primary,
        "primary.foreground": primary_fg,
        "ring": accent,
        "scrollbar.background": alpha(bg, 0),
        "scrollbar.thumb.background": border,
        "secondary.active.background": active,
        "secondary.background": surface,
        "secondary.foreground": fg,
        "secondary.hover.background": hover,
        "selection.background": selection,
        "success.background": success,
        "success.foreground": success_fg,
        "tab.active.background": bg,
        "tab.active.foreground": fg,
        "tab.background": alpha(bg, 0),
        "tab.foreground": muted_fg,
        "tab_bar.background": surface,
        "title_bar.background": surface,
        "title_bar.border": border,
        "status_bar.background": surface,
        "warning.background": warning,
        "warning.foreground": warning_fg,
    }
    for i, name in enumerate(["red", "green", "yellow", "blue", "magenta", "cyan"], 1):
        colors[f"base.{name}"] = p[i]
        colors[f"base.{name}.light"] = p[i + 8]

    def style(color, **extra):
        return {"color": color, **extra}

    highlight = {
        "editor.foreground": fg,
        "editor.background": bg,
        "editor.active_line.background": mix(bg, fg, 0.04),
        "editor.line_number": line_number,
        "editor.active_line_number": muted_fg,
        "editor.invisible": alpha(line_number, 0x66),
        "conflict": bright_red,
        "created": green,
        "deleted": red,
        "error": red,
        "hidden": line_number,
        "hint": muted_fg,
        "ignored": line_number,
        "info": blue,
        "modified": yellow,
        "predictive": line_number,
        "renamed": cyan,
        "success": green,
        "unreachable": line_number,
        "warning": yellow,
        "syntax": {
            "attribute": style(cyan),
            "boolean": style(bright_red),
            "comment": style(muted_fg, font_style="italic"),
            "comment.doc": style(muted_fg, font_style="italic"),
            "constant": style(bright_red),
            "constructor": style(yellow),
            "embedded": style(fg),
            "function": style(blue),
            "keyword": style(magenta),
            "link_text": style(blue, font_style="normal"),
            "link_uri": style(cyan, font_style="italic"),
            "number": style(bright_red),
            "string": style(green),
            "string.escape": style(cyan),
            "string.regex": style(cyan),
            "string.special": style(yellow),
            "string.special.symbol": style(yellow),
            "tag": style(red),
            "text.literal": style(green),
            "title": style(blue, font_weight=600),
            "type": style(yellow),
            "property": style(fg),
            "variable.special": style(red),
        },
    }
    return {
        "name": theme_name(family, variant),
        "mode": "dark" if dark else "light",
        "colors": colors,
        "highlight": highlight,
    }


def main():
    data = json.loads(DATA.read_text())
    written = []
    for family in data["families"]:
        variants = [v for v in family["variants"] if (family["id"], v["id"]) not in BUNDLED]
        if not variants:
            continue
        themes = [build(family, v) for v in variants]
        themes.sort(key=lambda t: (t["mode"] != "dark", t["name"]))
        file_name = FILE_NAMES.get(family["id"], family["id"])
        doc = {
            "$schema": SCHEMA,
            "name": family["name"],
            "author": family["author"],
            "url": family["url"],
            "themes": themes,
        }
        (OUT / f"{file_name}.json").write_text(json.dumps(doc, indent=2, ensure_ascii=False) + "\n")
        written.append(file_name)
    print("\n".join(written))


if __name__ == "__main__":
    main()
