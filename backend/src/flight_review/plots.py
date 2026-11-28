"""PNG charts and a PDF report for a motor/flight export bundle.

Reads one JSON request on stdin (out_dir, title, summary rows, extra rows,
charts) and writes <chart>.png per chart and report.pdf into out_dir. Uses
matplotlib's Agg/PdfPages backend only, like the report studio's PDF export.
"""
import json
import os
import sys

import matplotlib

matplotlib.use("Agg")
import matplotlib.pyplot as plt  # noqa: E402
from matplotlib.backends.backend_pdf import PdfPages  # noqa: E402


def draw(chart, fig=None):
    fig = fig or plt.figure(figsize=(10, 4.6))
    ax = fig.add_subplot(111)
    drawn = 0
    for line in chart["lines"]:
        points = [p for p in line["points"] if p[0] is not None and p[1] is not None]
        if not points:
            continue
        xs, ys = zip(*points)
        ax.plot(xs, ys, label=line["label"], color=line.get("color"), linestyle="--" if line.get("dashed") else "-", linewidth=1.6)
        drawn += 1
    for x, label in chart.get("markers", []):
        ax.axvline(x, color="#9ca3af", linestyle=":", linewidth=1)
        ax.annotate(label, (x, 1), xycoords=("data", "axes fraction"), rotation=90, va="top", ha="right", fontsize=7, color="#4b5563")
    ax.set_title(chart["title"])
    ax.set_xlabel(chart["xlabel"])
    ax.set_ylabel(chart["ylabel"])
    ax.grid(True, color="#e5e7eb")
    if drawn:
        ax.legend(loc="upper right", fontsize=8)
    else:
        ax.text(0.5, 0.5, "No data in this range", transform=ax.transAxes, ha="center", color="#6b7280")
    fig.tight_layout()
    return fig


def text_page(pdf, title, rows, heading):
    fig = plt.figure(figsize=(8.27, 11.69))
    fig.text(0.06, 0.96, title, fontsize=14, weight="bold", va="top")
    fig.text(0.06, 0.93, heading, fontsize=11, weight="bold", va="top")
    y = 0.90
    for key, value in rows:
        if y < 0.05:
            pdf.savefig(fig)
            plt.close(fig)
            fig = plt.figure(figsize=(8.27, 11.69))
            y = 0.95
        fig.text(0.06, y, str(key), fontsize=8.5, weight="bold", va="top")
        fig.text(0.36, y, str(value)[:110], fontsize=8.5, va="top")
        y -= 0.022
    pdf.savefig(fig)
    plt.close(fig)


def main():
    request = json.load(sys.stdin)
    out = request["out_dir"]
    for chart in request["charts"]:
        fig = draw(chart)
        fig.savefig(os.path.join(out, chart["file"].replace(".svg", ".png")), dpi=130)
        plt.close(fig)
    with PdfPages(os.path.join(out, "report.pdf")) as pdf:
        text_page(pdf, request["title"], request["summary"], "Summary")
        if request.get("extra"):
            text_page(pdf, request["title"], request["extra"], "Quality flags")
        for chart in request["charts"]:
            fig = draw(chart, plt.figure(figsize=(11.69, 8.27)))
            pdf.savefig(fig)
            plt.close(fig)


main()
