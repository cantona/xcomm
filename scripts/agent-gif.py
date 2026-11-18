#!/usr/bin/env python3
# SPDX-FileCopyrightText: 2026 Su Kang Yin <paradyse@gmail.com>
# SPDX-License-Identifier: GPL-2.0-only
"""agent-gif.py CAPTURES OUT: the agent GIF.

An agent's commands (adapted from the agent skill's examples) drive a board's console
while you watch it: stop U-Boot's autoboot, boot, log in, run commands, pick
a value out. Both panes are real output, captured step by step:

    scripts/agent-gif-capture.sh target/release/xcomm CAPTURES
    scripts/agent-gif.py CAPTURES assets/agent.gif

Only the frame around them (borders, titles, captions) is drawn here. Needs
Pillow and DejaVu Sans Mono.
"""

import importlib.util
import os
import sys

sys.dont_write_bytecode = True

from PIL import Image, ImageDraw

HERE = os.path.dirname(os.path.abspath(__file__))
spec = importlib.util.spec_from_file_location("picker_gif", os.path.join(HERE, "picker-gif.py"))
pg = importlib.util.module_from_spec(spec)
spec.loader.exec_module(pg)

COLS, ROWS = pg.COLS, pg.ROWS
LEFT = 72
NOTE = "real xcomm output · a simulated board (scripts/demo-board.py)"

# One caption per captured step, and how long it shows.
STEPS = [
    ("Your board's console in xcomm (left), and a shell for an agent beside it", 3000),
    ("The agent notes the console's offset, so a wait from it misses nothing", 2600),
    ("It wakes the demo board, which boots on its first input (a stand-in for a reset)", 2600),
    ("It catches U-Boot's countdown from that offset and stops autoboot", 3000),
    ("It boots and waits for the login prompt; the boot scrolls by in your console", 3200),
    ("It logs in; --fail-on would turn a wrong password into exit 7", 3000),
    ("It runs a command and reads its output back", 3000),
    ("and picks a value out of another. Each line it sent shows as [ctl] in your console", 5500),
]


def frame(console, agent, caption):
    s = pg.Screen()
    panes = [
        ("xcomm /dev/ttyUSB0 115200 (you)", console, 0, LEFT),
        ("shell (the agent)", agent, LEFT, COLS - LEFT),
    ]
    for title, text, x, w in panes:
        s.box(x, 0, w, ROWS, pg.BORDER, title)
        s.content(x + 1, 1, w - 2, ROWS - 2, pg.cells(text, w - 2, True), bottom=True)
    img = Image.new("RGB", (COLS * pg.CW, ROWS * pg.CH + pg.CAPTION_H), pg.rgb(pg.BG))
    d = ImageDraw.Draw(img)
    pg.paint(d, s.grid, (0, 0, COLS, ROWS))
    d.rectangle([0, ROWS * pg.CH, COLS * pg.CW, ROWS * pg.CH + pg.CAPTION_H], fill=pg.rgb("#14151c"))
    d.text((14, ROWS * pg.CH + 9), caption, font=pg.BOLD, fill=pg.rgb(pg.FG))
    note_w = d.textlength(NOTE, font=pg.FONT)
    d.text((COLS * pg.CW - note_w - 14, ROWS * pg.CH + 9), NOTE, font=pg.FONT,
           fill=pg.rgb(pg.blend(pg.FG, pg.BG)))
    return img


def main():
    if len(sys.argv) != 3:
        sys.exit(__doc__)
    captures, out = sys.argv[1:]
    read = lambda n, f: open(os.path.join(captures, f"{n:02d}-{f}.ansi"), encoding="utf-8").read()
    have = len([f for f in os.listdir(captures) if f.endswith("-console.ansi")])
    if have != len(STEPS):
        sys.exit(f"agent-gif: {have} captured steps, {len(STEPS)} captions")
    frames = [frame(read(i, "console"), read(i, "agent"), cap) for i, (cap, _) in enumerate(STEPS)]
    # One palette from every frame, as in picker-gif.py.
    sheet = Image.new("RGB", (frames[0].width, frames[0].height * len(frames)))
    for i, f in enumerate(frames):
        sheet.paste(f, (0, i * f.height))
    palette = sheet.quantize(colors=128, method=Image.Quantize.MEDIANCUT)
    frames = [f.quantize(palette=palette, dither=Image.Dither.NONE) for f in frames]
    frames[0].save(out, save_all=True, append_images=frames[1:],
                   duration=[ms for _, ms in STEPS], loop=0, optimize=True, disposal=1)


if __name__ == "__main__":
    main()
