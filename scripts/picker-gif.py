#!/usr/bin/env python3
# SPDX-FileCopyrightText: 2026 Su Kang Yin <paradyse@gmail.com>
# SPDX-License-Identifier: GPL-2.0-only
"""picker-gif.py SCREENS CAPTURES OUT: the README's picker GIF.

Shows what Enter does in the herdr port picker: on a port open in a pane it
goes to that pane, on a free port it opens a new pane, and on a port another
program holds it says so. What is in the panes is xcomm's own output:

    cargo run --example screens -- SCREENS        # the picker, SCREENS/gif-*.ansi
    scripts/picker-gif-capture.sh target/release/xcomm CAPTURES   # the sessions
    scripts/picker-gif.py SCREENS CAPTURES assets/picker.gif

Only herdr's frame around them (borders, titles, the popup box, the split)
is drawn here. Needs Pillow and DejaVu Sans Mono.
"""

import importlib.util
import os
import sys

# Loading ansi2svg.py would leave a __pycache__ in scripts/.
sys.dont_write_bytecode = True

from PIL import Image, ImageDraw, ImageFont

HERE = os.path.dirname(os.path.abspath(__file__))
spec = importlib.util.spec_from_file_location("ansi2svg", os.path.join(HERE, "ansi2svg.py"))
ansi = importlib.util.module_from_spec(spec)
spec.loader.exec_module(ansi)

FONT_DIR = "/usr/share/fonts/truetype/dejavu"
FONT = ImageFont.truetype(os.path.join(FONT_DIR, "DejaVuSansMono.ttf"), 13)
BOLD = ImageFont.truetype(os.path.join(FONT_DIR, "DejaVuSansMono-Bold.ttf"), 13)
# A row holds the font's whole ascent and descent (13 + 4 at 13 px), so no
# glyph reaches into the next row.
CW, CH = 8, 17
COLS, ROWS = 150, 32
CAPTION_H = 34
BG, FG = ansi.BG, ansi.FG
BORDER, FOCUS, TITLE = "#565a6e", "#50fa7b", "#c0c4d8"
POPUP_W, POPUP_H = 90, 21
POPUP_BG = "#262836"


# Box-drawing characters as strokes from the cell's centre.
LINES = {
    "─": [(-1, 0), (1, 0)],
    "│": [(0, -1), (0, 1)],
    "┌": [(1, 0), (0, 1)],
    "┐": [(-1, 0), (0, 1)],
    "└": [(1, 0), (0, -1)],
    "┘": [(-1, 0), (0, -1)],
}


def rgb(c):
    c = c.lstrip("#")
    return tuple(int(c[i:i + 2], 16) for i in (0, 2, 4))


def blend(a, b):
    return "#%02x%02x%02x" % tuple((x + y) // 2 for x, y in zip(rgb(a), rgb(b)))


def cells(text, cols, wrap):
    """Rows of (char, fg, bg, bold) cells, wrapped (or cut) to cols."""
    rows = []
    for runs in ansi.parse(text, 10_000):
        row = []
        for style, chunk in runs:
            fg, bg = style.fg or FG, style.bg
            if style.dim:
                fg = blend(fg, BG)
            if style.reverse:
                fg, bg = bg or BG, fg
            for ch in chunk:
                row.append((ch, fg, bg, style.bold))
        if not row:
            rows.append([])
        while row:
            rows.append(row[:cols])
            row = row[cols:] if wrap else []
    return rows


class Screen:
    def __init__(self):
        self.grid = [[(" ", FG, None, False)] * COLS for _ in range(ROWS)]

    def put(self, x, y, text, fg=FG, bg=None, bold=False):
        for i, ch in enumerate(text):
            if 0 <= x + i < COLS and 0 <= y < ROWS:
                self.grid[y][x + i] = (ch, fg, bg, bold)

    def box(self, x, y, w, h, colour, title="", tag="", fill=None):
        if fill:
            for r in range(y, y + h):
                self.put(x, r, " " * w, bg=fill)
        self.put(x, y, "┌" + "─" * (w - 2) + "┐", colour, fill)
        for r in range(y + 1, y + h - 1):
            self.put(x, r, "│", colour, fill)
            self.put(x + w - 1, r, "│", colour, fill)
        self.put(x, y + h - 1, "└" + "─" * (w - 2) + "┘", colour, fill)
        if title:
            self.put(x + 2, y, f" {title} ", TITLE, fill, True)
        if tag:
            self.put(x + w - len(tag) - 4, y, f" {tag} ", colour, fill)

    def content(self, x, y, w, h, rows, bottom, fill=None):
        rows = rows[-h:] if bottom else rows[:h]
        for r, row in enumerate(rows):
            for c, (ch, fg, bg, bold) in enumerate(row[:w]):
                self.grid[y + r][x + c] = (ch, fg, bg or fill, bold)


def paint(d, grid, area, cover=False):
    """Paints the cells of grid inside area (x, y, w, h); a cover hides what
    was painted there before, even where its cells have no background."""
    ax, ay, aw, ah = area
    cells = [(r, c, grid[r][c]) for r in range(ay, ay + ah) for c in range(ax, ax + aw)]
    # Every background first: a glyph ("_", the tail of a "g") reaches into
    # the row below, whose background would otherwise paint over it.
    for r, c, (_, _, bg, _) in cells:
        if bg or cover:
            d.rectangle([c * CW, r * CH, (c + 1) * CW - 1, (r + 1) * CH - 1], fill=rgb(bg or BG))
    for r, c, (ch, fg, _, bold) in cells:
        if ch in LINES:
            # Drawn, not typeset: a font's box glyphs leave gaps between rows.
            mx, my = c * CW + CW // 2, r * CH + CH // 2
            for dx, dy in LINES[ch]:
                d.line([mx, my, mx + dx * CW // 2, my + dy * CH // 2], fill=rgb(fg))
        elif ch != " ":
            d.text((c * CW, r * CH), ch, font=BOLD if bold else FONT, fill=rgb(fg))


def frame(panes, popup=None, caption=""):
    """panes: (title, tag, rows, focused, width); popup: rows of the picker."""
    s = Screen()
    s.put(0, 0, " herdr ", BG, FOCUS, True)
    s.put(8, 0, "bench", TITLE)
    x = 0
    for title, tag, rows, focused, w in panes:
        s.box(x, 1, w, ROWS - 1, FOCUS if focused else BORDER, title, tag)
        s.content(x + 1, 2, w - 2, ROWS - 3, rows(w - 2), bottom=True)
        x += w
    img = Image.new("RGB", (COLS * CW, ROWS * CH + CAPTION_H), rgb(BG))
    d = ImageDraw.Draw(img)
    paint(d, s.grid, (0, 0, COLS, ROWS))
    if popup is not None:
        # A layer of its own, painted over the panes as herdr's popup is, so
        # nothing of theirs (a "_" reaching down a pixel) shows through.
        x, y = (COLS - POPUP_W) // 2, (ROWS - POPUP_H) // 2
        p = Screen()
        p.box(x, y, POPUP_W, POPUP_H, FOCUS, "xcomm: open port", fill=POPUP_BG)
        p.content(x + 1, y + 1, POPUP_W - 2, POPUP_H - 2, popup, bottom=False, fill=POPUP_BG)
        paint(d, p.grid, (x, y, POPUP_W, POPUP_H), cover=True)
    d.rectangle([0, ROWS * CH, COLS * CW, ROWS * CH + CAPTION_H], fill=rgb("#14151c"))
    d.text((14, ROWS * CH + 9), caption, font=BOLD, fill=rgb(FG))
    return img


def main():
    if len(sys.argv) != 4:
        sys.exit(__doc__)
    screens, captures, out = sys.argv[1:]
    s0, s1 = (os.path.join(captures, f"s{i}.ansi") for i in (0, 1))
    read = lambda p: open(p, encoding="utf-8").read()
    pick = lambda n: cells(read(os.path.join(screens, f"gif-{n}.ansi")), POPUP_W - 2, False)
    ttyusb0 = lambda w: cells(read(s0), w, True)
    ttyusb1 = lambda w: cells(read(s1), w, True)
    shell = lambda w: [[("$", FG, None, False), (" ", FG, None, False), (" ", BG, FG, False)]]
    p0 = ("xcomm ttyUSB0 115200", "w1:p3", ttyusb0)
    p1 = ("xcomm ttyUSB1 115200", "w1:p4", ttyusb1)
    sh = ("shell", "w1:p2", shell)
    # A split right halves the pane it splits: w1:p3's half becomes two.
    half = COLS // 2
    two = lambda focus: [(*p, p[1] == focus, w) for p, w in ((p0, half), (sh, COLS - half))]
    three = lambda focus: [
        (*p, p[1] == focus, w)
        for p, w in ((p0, half // 2), (p1, half - half // 2), (sh, COLS - half))
    ]
    steps = [
        (two("w1:p2"), None, "ttyUSB0 is open in pane w1:p3; you are in the shell beside it", 2200),
        (two("w1:p2"), "a-0", "prefix+shift+s: the port picker", 1300),
        (two("w1:p2"), "a-1", "ttyUSB0 says pane w1:p3 ... Enter", 1600),
        (two("w1:p3"), None, "Enter goes to pane w1:p3: no second session", 2400),
        (two("w1:p3"), "a-0", "prefix+shift+s again", 900),
        (two("w1:p3"), "a-1", "prefix+shift+s again", 400),
        (two("w1:p3"), "b-2", "ttyUSB1 is free ... Enter", 1600),
        (three("w1:p4"), None, "Enter opens it in a new pane, titled with its port and baud", 2600),
        (three("w1:p4"), "c-0", "prefix+shift+s: ttyUSB1 now says pane w1:p4", 900),
        (three("w1:p4"), "c-1", "prefix+shift+s: ttyUSB1 now says pane w1:p4", 400),
        (three("w1:p4"), "c-2", "prefix+shift+s: ttyUSB1 now says pane w1:p4", 1300),
        (three("w1:p4"), "c-3", "ttyUSB2 is held by another program ... Enter", 1500),
        (three("w1:p4"), "c-in-use", "the picker says who holds it and opens nothing", 2800),
    ]
    frames, durations = [], []
    for panes, popup, caption, ms in steps:
        frames.append(frame(panes, pick(popup) if popup else None, caption))
        durations.append(ms)
    # One palette for every frame, from all of them, so that no colour (a
    # thin border) is dropped from one frame and none flickers between them.
    sheet = Image.new("RGB", (frames[0].width, frames[0].height * len(frames)))
    for i, f in enumerate(frames):
        sheet.paste(f, (0, i * f.height))
    palette = sheet.quantize(colors=128, method=Image.Quantize.MEDIANCUT)
    frames = [f.quantize(palette=palette, dither=Image.Dither.NONE) for f in frames]
    frames[0].save(out, save_all=True, append_images=frames[1:], duration=durations,
                   loop=0, optimize=True, disposal=1)


if __name__ == "__main__":
    main()
