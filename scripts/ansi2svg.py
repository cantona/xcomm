#!/usr/bin/env python3
"""ansi2svg.py: renders a terminal capture (text with SGR escapes, as
`herdr pane read --format ansi` gives it) as an SVG terminal window.

Usage: ansi2svg.py [--title TITLE] [--cols N] < capture.ansi > shot.svg

Only what a screen capture holds is understood: SGR colours (16, 256 and
truecolour), bold, dim, italic, underline and reverse. Other escapes are
dropped. Lines longer than --cols are cut.
"""

import argparse
import html
import re
import sys

# A dark theme close to common terminal defaults.
BG = "#1e1f29"
FG = "#e6e6e6"
PALETTE = [
    "#21222c", "#ff5555", "#50fa7b", "#f1fa8c", "#6272e4", "#ff79c6", "#8be9fd", "#d0d0d0",
    "#6272a4", "#ff6e6e", "#69ff94", "#ffffa5", "#a4b4ff", "#ff92df", "#a4ffff", "#ffffff",
]

CHAR_W = 8.4
LINE_H = 18
PAD_X = 16
PAD_TOP = 40
PAD_BOTTOM = 16


def colour256(n):
    if n < 16:
        return PALETTE[n]
    if n < 232:
        n -= 16
        steps = [0, 95, 135, 175, 215, 255]
        return "#%02x%02x%02x" % (steps[n // 36], steps[(n // 6) % 6], steps[n % 6])
    v = 8 + (n - 232) * 10
    return "#%02x%02x%02x" % (v, v, v)


class Style:
    def __init__(self):
        self.reset()

    def reset(self):
        self.fg = None
        self.bg = None
        self.bold = self.dim = self.italic = self.underline = self.reverse = False

    def copy(self):
        s = Style()
        s.__dict__.update(self.__dict__)
        return s

    def key(self):
        return tuple(sorted(self.__dict__.items()))

    def apply(self, params):
        nums = [int(p) if p else 0 for p in params.split(";")] if params else [0]
        i = 0
        while i < len(nums):
            n = nums[i]
            if n == 0:
                self.reset()
            elif n == 1:
                self.bold = True
            elif n == 2:
                self.dim = True
            elif n == 3:
                self.italic = True
            elif n == 4:
                self.underline = True
            elif n == 7:
                self.reverse = True
            elif n == 22:
                self.bold = self.dim = False
            elif n == 23:
                self.italic = False
            elif n == 24:
                self.underline = False
            elif n == 27:
                self.reverse = False
            elif 30 <= n <= 37:
                self.fg = PALETTE[n - 30]
            elif 90 <= n <= 97:
                self.fg = PALETTE[n - 90 + 8]
            elif n == 39:
                self.fg = None
            elif 40 <= n <= 47:
                self.bg = PALETTE[n - 40]
            elif 100 <= n <= 107:
                self.bg = PALETTE[n - 100 + 8]
            elif n == 49:
                self.bg = None
            elif n in (38, 48) and i + 1 < len(nums):
                if nums[i + 1] == 5 and i + 2 < len(nums):
                    c = colour256(nums[i + 2])
                    i += 2
                elif nums[i + 1] == 2 and i + 4 < len(nums):
                    c = "#%02x%02x%02x" % tuple(nums[i + 2:i + 5])
                    i += 4
                else:
                    c = None
                if n == 38:
                    self.fg = c
                else:
                    self.bg = c
            i += 1


ESC = re.compile(r"\x1b\[([0-9;?]*)([A-Za-z])|\x1b[()][A-Za-z0-9]|\x1b[=>]|\x1b\][^\x07\x1b]*(?:\x07|\x1b\\)")


def parse(text, cols):
    """Lines of (style, text) runs."""
    lines = []
    style = Style()
    for raw in text.split("\n"):
        raw = raw.rstrip("\r")
        runs, width, pos = [], 0, 0
        for m in ESC.finditer(raw):
            chunk = raw[pos:m.start()]
            pos = m.end()
            if chunk and width < cols:
                chunk = chunk[:cols - width]
                runs.append((style.copy(), chunk))
                width += len(chunk)
            if m.group(2) == "m":
                style.apply(m.group(1))
        chunk = raw[pos:]
        if chunk and width < cols:
            runs.append((style.copy(), chunk[:cols - width]))
        lines.append(runs)
    while lines and not any(t.strip() for _, t in lines[-1]):
        lines.pop()
    return lines


def render(lines, cols, title):
    width = PAD_X * 2 + cols * CHAR_W
    height = PAD_TOP + max(len(lines), 1) * LINE_H + PAD_BOTTOM
    out = [
        f'<svg xmlns="http://www.w3.org/2000/svg" width="{width:.0f}" height="{height}" '
        f'viewBox="0 0 {width:.0f} {height}" font-family="\'JetBrains Mono\', \'DejaVu Sans Mono\', '
        f'Menlo, Consolas, monospace" font-size="14" style="font-variant-ligatures: none">',
        f'<rect width="100%" height="100%" rx="10" fill="{BG}"/>',
        '<circle cx="20" cy="18" r="6" fill="#ff5f56"/>',
        '<circle cx="40" cy="18" r="6" fill="#ffbd2e"/>',
        '<circle cx="60" cy="18" r="6" fill="#27c93f"/>',
    ]
    if title:
        out.append(
            f'<text x="{width / 2:.0f}" y="22" fill="#9a9cb0" text-anchor="middle" '
            f'font-size="13">{html.escape(title)}</text>'
        )
    for row, runs in enumerate(lines):
        y = PAD_TOP + row * LINE_H
        col = 0
        for style, text in runs:
            fg, bg = style.fg or FG, style.bg
            if style.reverse:
                fg, bg = (style.bg or BG), (style.fg or FG)
            x = PAD_X + col * CHAR_W
            if bg:
                out.append(
                    f'<rect x="{x:.1f}" y="{y}" width="{len(text) * CHAR_W:.1f}" '
                    f'height="{LINE_H}" fill="{bg}"/>'
                )
            attrs = [f'fill="{fg}"']
            if style.bold:
                attrs.append('font-weight="bold"')
            if style.dim:
                attrs.append('opacity="0.6"')
            if style.italic:
                attrs.append('font-style="italic"')
            if style.underline:
                attrs.append('text-decoration="underline"')
            # Each word at its own column: renderers differ on spaces.
            for m in re.finditer(r"\S+", text):
                wx = x + m.start() * CHAR_W
                out.append(
                    f'<text x="{wx:.1f}" y="{y + 13}" {" ".join(attrs)}>'
                    f"{html.escape(m.group(), quote=False)}</text>"
                )
            col += len(text)
    out.append("</svg>")
    return "\n".join(out) + "\n"


def main():
    ap = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    ap.add_argument("--title", default="")
    ap.add_argument("--cols", type=int, default=100)
    args = ap.parse_args()
    text = sys.stdin.read()
    sys.stdout.write(render(parse(text, args.cols), args.cols, args.title))


if __name__ == "__main__":
    main()
