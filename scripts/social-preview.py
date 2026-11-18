#!/usr/bin/env python3
# SPDX-FileCopyrightText: 2026 Su Kang Yin <paradyse@gmail.com>
# SPDX-License-Identifier: GPL-2.0-only
"""social-preview.py CONSOLE_ANSI OUT.png: xcomm's GitHub social preview.

1280x640 (drawn at 2x, then scaled down), for the repository's Settings,
General, Social preview. The terminal on the right is the end of the agent
GIF's console capture: real xcomm output on the simulated board.

    scripts/agent-gif-capture.sh target/release/xcomm CAPTURES
    scripts/social-preview.py CAPTURES/07-console.ansi assets/social-preview.png

Needs Pillow, Noto Sans and JetBrains Mono.
"""

import importlib.util
import os
import sys

sys.dont_write_bytecode = True
from PIL import Image, ImageDraw, ImageFilter, ImageFont

HERE = os.path.dirname(os.path.abspath(__file__))
spec = importlib.util.spec_from_file_location("ansi2svg", os.path.join(HERE, "ansi2svg.py"))
ansi = importlib.util.module_from_spec(spec)
spec.loader.exec_module(ansi)

S = 2  # drawn at twice the size
W, H = 1280 * S, 640 * S
NOTO = "/usr/share/fonts/truetype/noto/NotoSans-{}.ttf"
JB = "/usr/share/fonts/truetype/jetbrains-mono/JetBrainsMono-{}.ttf"
font = lambda path, w, px: ImageFont.truetype(path.format(w), px * S)

BG0, BG1 = (13, 15, 22), (24, 27, 38)
FG = (230, 233, 242)
MUTED = (150, 156, 178)
ACCENT = (80, 250, 123)
CYAN = (139, 233, 253)
CARD = (30, 32, 44)
CARD_EDGE = (66, 70, 92)


def hexrgb(c):
    c = c.lstrip("#")
    return tuple(int(c[i:i + 2], 16) for i in (0, 2, 4))


def background():
    img = Image.new("RGB", (W, H), BG0)
    d = ImageDraw.Draw(img)
    for y in range(H):
        t = y / H
        d.line([(0, y), (W, y)], fill=tuple(int(a + (b - a) * t) for a, b in zip(BG0, BG1)))
    # A faint grid, and a soft green glow behind the terminal.
    for x in range(0, W, 40 * S):
        d.line([(x, 0), (x, H)], fill=(20, 23, 32))
    for y in range(0, H, 40 * S):
        d.line([(0, y), (W, y)], fill=(20, 23, 32))
    glow = Image.new("RGB", (W, H), (0, 0, 0))
    ImageDraw.Draw(glow).ellipse([W * 0.55, H * 0.05, W * 1.05, H * 0.95], fill=(20, 70, 40))
    glow = glow.filter(ImageFilter.GaussianBlur(120 * S))
    return Image.blend(img, Image.composite(glow, img, glow.convert("L")), 0.6)


def chip(d, x, y, text, f):
    w = d.textlength(text, font=f)
    pad = 12 * S
    d.rounded_rectangle([x, y, x + w + 2 * pad, y + 34 * S], radius=17 * S,
                        fill=(30, 34, 46), outline=(60, 66, 88), width=S)
    d.text((x + pad, y + 6 * S), text, font=f, fill=FG)
    return x + w + 2 * pad + 10 * S


def terminal(img, d, box, rows):
    x0, y0, x1, y1 = box
    shadow = Image.new("L", (W, H), 0)
    ImageDraw.Draw(shadow).rounded_rectangle([x0 + 8 * S, y0 + 14 * S, x1 + 8 * S, y1 + 14 * S],
                                             radius=14 * S, fill=170)
    shadow = shadow.filter(ImageFilter.GaussianBlur(14 * S))
    img.paste((0, 0, 0), mask=shadow)
    d.rounded_rectangle(box, radius=14 * S, fill=CARD, outline=CARD_EDGE, width=S)
    # Title bar.
    d.rounded_rectangle([x0, y0, x1, y0 + 34 * S], radius=14 * S, fill=(40, 43, 58))
    d.rectangle([x0, y0 + 20 * S, x1, y0 + 34 * S], fill=(40, 43, 58))
    for i, c in enumerate([(255, 95, 86), (255, 189, 46), (39, 201, 63)]):
        cx, cy = x0 + (20 + 18 * i) * S, y0 + 17 * S
        d.ellipse([cx - 6 * S, cy - 6 * S, cx + 6 * S, cy + 6 * S], fill=c)
    tf = font(JB, "Medium", 13)
    title = "xcomm /dev/ttyUSB0 115200"
    d.text(((x0 + x1 - d.textlength(title, font=tf)) / 2, y0 + 9 * S), title, font=tf, fill=MUTED)
    # Content: the capture's cells, in its own colours.
    mono = font(JB, "Regular", 13)
    bold = font(JB, "Bold", 13)
    cw = d.textlength("M", font=mono)
    lh = 20 * S
    tx, ty = x0 + 18 * S, y0 + 48 * S
    ctl_y = []
    for r, row in enumerate(rows):
        x = tx
        for ch, fg, bg, b in row:
            if ch != " ":
                d.text((x, ty + r * lh), ch, font=bold if b else mono, fill=fg)
            x += cw
        line = "".join(c[0] for c in row).rstrip()
        if line.startswith("[ctl]"):
            ctl_y.append((ty + r * lh, tx + cw * len(line)))
    return ctl_y


def console_rows(path, cols, keep):
    rows = []
    for runs in ansi.parse(open(path, encoding="utf-8").read(), 10_000):
        row = []
        for style, chunk in runs:
            fg = hexrgb(style.fg or ansi.FG)
            if style.dim:
                fg = tuple((2 * a + b) // 3 for a, b in zip(fg, CARD))
            for ch in chunk:
                row.append((ch, fg, None, style.bold))
        while len(row) > cols:
            rows.append(row[:cols])
            row = row[cols:]
        rows.append(row)
    while rows and not rows[-1]:
        rows.pop()
    text = ["".join(c[0] for c in r) for r in rows]
    starts = [i for i, t in enumerate(text) if t.startswith("Demo Linux")]
    if not starts:
        sys.exit(f"social-preview: no 'Demo Linux' line in {path} (the board's login banner)")
    return rows[starts[-1]:][-keep:]


def main():
    if len(sys.argv) != 3:
        sys.exit(__doc__)
    img = background()
    d = ImageDraw.Draw(img)
    L = 64 * S

    # Left: what it is.
    d.text((L, 58 * S), "SERIAL TERMINAL  ·  LINUX & macOS  ·  OPEN SOURCE",
           font=font(JB, "Medium", 14), fill=ACCENT)
    wm = font(JB, "ExtraBold", 104)
    d.text((L - 6 * S, 78 * S), "xcomm", font=wm, fill=FG)
    cur_x = L - 6 * S + d.textlength("xcomm", font=wm) + 10 * S
    d.rectangle([cur_x, 108 * S, cur_x + 40 * S, 196 * S], fill=ACCENT)

    tag = font(NOTO, "Bold", 31)
    d.text((L, 222 * S), "Built for speed. Made for engineers.", font=tag, fill=FG)
    d.text((L, 262 * S), "Ready for AI agents.", font=tag, fill=ACCENT)

    big = font(JB, "ExtraBold", 46)
    lab = font(NOTO, "SemiBold", 17)
    y = 330 * S
    d.text((L, y), "40×", font=big, fill=CYAN)
    d.text((L + 2 * S, y + 60 * S), "minicom's throughput", font=lab, fill=FG)
    x2 = L + 250 * S
    d.text((x2, y), "1/50", font=big, fill=CYAN)
    d.text((x2 + 2 * S, y + 60 * S), "of its CPU per MiB", font=lab, fill=FG)
    # Big enough to read wherever the numbers are: at README width too.
    d.text((L, y + 88 * S), "measured through a pty · method in the README",
           font=font(JB, "Medium", 16), fill=FG)

    cf = font(NOTO, "SemiBold", 15)
    x, cy = L, 456 * S
    # The herdr plugin has its install line below.
    for t in ["agent control socket", "reconnects on unplug", "minicom-style keys"]:
        # Onto the next row rather than under the terminal.
        if x + d.textlength(t, font=cf) + 24 * S > 696 * S:
            x, cy = L, cy + 42 * S
        x = chip(d, x, cy, t, cf)

    d.text((L, 516 * S), "github.com/cantona/xcomm", font=font(JB, "Bold", 22), fill=FG)
    inst = font(JB, "Regular", 14)
    for i, line in enumerate(["brew install cantona/tap/xcomm",
                              "cargo install --locked xcomm",
                              "herdr plugin install cantona/xcomm"]):
        yy = (554 + 22 * i) * S
        d.text((L, yy), "$", font=inst, fill=ACCENT)
        d.text((L + d.textlength("$ ", font=inst), yy), line, font=inst, fill=MUTED)

    # Right: a real console, with what came over the control socket marked.
    rows = console_rows(sys.argv[1], 60, 24)
    # The window fits its content, centred on the canvas's height.
    h = 48 + 20 * len(rows) + 14
    top = (640 - h) // 2
    box = (720 * S, top * S, 1232 * S, (top + h) * S)
    ctl = terminal(img, d, box, rows)
    # A pill just after a [ctl] line, within its row and the window: the
    # second-to-last such line that has room, else any that has.
    lf = font(NOTO, "Bold", 13)
    af = font(JB, "Bold", 13)
    label = "sent over the control socket"
    aw = d.textlength("← ", font=af)
    w = aw + d.textlength(label, font=lf)
    room = [(y, end) for y, end in ctl if end + 34 * S + w < box[2] - 12 * S]
    if not room:
        sys.exit("social-preview: no [ctl] line with room for its label in the capture's last rows")
    yy, end = (room[-2] if len(room) > 1 else room[-1])
    lx = end + 24 * S
    d.rounded_rectangle([lx - 10 * S, yy, lx + w + 10 * S, yy + 19 * S],
                        radius=9 * S, fill=ACCENT)
    d.text((lx, yy + 1 * S), "←", font=af, fill=(14, 30, 20))
    d.text((lx + aw, yy), label, font=lf, fill=(14, 30, 20))

    img.resize((W // S, H // S), Image.LANCZOS).save(sys.argv[2], optimize=True)


if __name__ == "__main__":
    main()
