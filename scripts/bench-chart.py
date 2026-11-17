#!/usr/bin/env python3
# SPDX-FileCopyrightText: 2026 Su Kang Yin <paradyse@gmail.com>
# SPDX-License-Identifier: GPL-2.0-only
"""bench-chart.py OUT.svg REPORT.json...: the README's benchmark chart.

One panel per metric, one bar per tool, from `xcomm-bench run` reports, or
from one file that holds them all under "tools" (as the README's figures
were kept: each tool's 256 MiB report, its latency the median of the
latency rounds). Each panel says whether more or less is better; xcomm's
bar is coloured. The README's chart:

    scripts/bench-chart.py assets/bench.svg bench.json
"""

import html
import json
import sys

METRICS = [
    ("throughput_text_mib_s", "Throughput, text", "MiB/s", True),
    ("throughput_bin_mib_s", "Throughput, binary", "MiB/s", True),
    ("cpu_ms_per_mib", "CPU time", "ms per MiB", False),
    ("latency_p99_us", "Keystroke latency p99", "µs", False),
    ("peak_rss_kib", "Peak memory (RSS)", "MiB", False),
    ("syscalls_per_mib", "System calls", "per MiB", False),
]

PANEL_W, BAR_H, GAP, LABEL_W, VALUE_W = 420, 28, 8, 92, 110
# The dark theme of the README's terminal pictures (scripts/ansi2svg.py).
BG, BORDER, TEXT, HINT = "#1e1f29", "#44475a", "#e6e6e6", "#8a8fa8"
OURS, OTHERS = "#50fa7b", "#6272a4"
FONT = "font-family=\"-apple-system, 'Segoe UI', Helvetica, Arial, sans-serif\""


def fmt(v):
    return f"{v:,.0f}" if v >= 100 else f"{v:,.1f}" if v >= 10 else f"{v:,.2f}"


def main():
    out_path, paths = sys.argv[1], sys.argv[2:]
    reports = []
    for p in paths:
        doc = json.load(open(p))
        reports.extend(doc["tools"].values() if "tools" in doc else [doc])
    # xcomm first, then the others as given.
    reports.sort(key=lambda r: r["tool"] != "xcomm")
    n = len(reports)
    panel_h = 48 + n * (BAR_H + GAP)
    cols = 2
    rows = (len(METRICS) + cols - 1) // cols
    width = cols * PANEL_W + (cols + 1) * 20
    height = 20 + rows * (panel_h + 20)
    svg = [
        f'<svg xmlns="http://www.w3.org/2000/svg" width="{width}" height="{height}" '
        f'viewBox="0 0 {width} {height}" {FONT} font-size="16">',
        f'<rect width="100%" height="100%" rx="10" fill="{BG}" stroke="{BORDER}"/>',
    ]
    for i, (key, title, unit, more_better) in enumerate(METRICS):
        x0 = 20 + (i % cols) * (PANEL_W + 20)
        y0 = 20 + (i // cols) * (panel_h + 20)
        # None: the tool could not do it (minicom passes no binary data).
        values = [
            None if r[key] is None else r[key] / 1024 if key == "peak_rss_kib" else r[key]
            for r in reports
        ]
        top = max(v for v in values if v is not None) or 1
        hint = "higher is better" if more_better else "lower is better"
        svg.append(
            f'<text x="{x0}" y="{y0 + 17}" font-weight="600" font-size="17" fill="{TEXT}">'
            f"{html.escape(title)}</text>"
            f'<text x="{x0 + PANEL_W}" y="{y0 + 17}" text-anchor="end" fill="{HINT}" '
            f'font-size="14">{unit}, {hint}</text>'
        )
        bar_w = PANEL_W - LABEL_W - VALUE_W
        for j, (r, v) in enumerate(zip(reports, values)):
            y = y0 + 34 + j * (BAR_H + GAP)
            w = 0 if v is None else max(2, bar_w * v / top)
            ours = r["tool"] == "xcomm"
            fill = OURS if ours else OTHERS
            weight = ' font-weight="600"' if ours else ""
            svg.append(
                f'<text x="{x0}" y="{y + 20}" fill="{TEXT}"{weight}>{html.escape(r["tool"])}</text>'
                f'<rect x="{x0 + LABEL_W}" y="{y}" width="{w:.1f}" height="{BAR_H}" rx="3" '
                f'fill="{fill}"/>'
                f'<text x="{x0 + LABEL_W + w + 6:.1f}" y="{y + 20}" fill="{TEXT}">'
                f'{"not measured (rewrites the bytes)" if v is None else fmt(v)}</text>'
            )
    svg.append("</svg>")
    open(out_path, "w").write("\n".join(svg) + "\n")


if __name__ == "__main__":
    main()
