#!/usr/bin/env python3

from __future__ import annotations

import shutil
import subprocess
import sys
from pathlib import Path
from textwrap import dedent


ROOT = Path(__file__).resolve().parent.parent
ICON = ROOT / "frontend" / "assets" / "icon_1024x1024.png"
OUT_DIR = ROOT / "artifacts" / "play-store"

BG = "#08111F"
BG2 = "#0D1B2E"
PANEL = "#102238"
PANEL_2 = "#163352"
TEXT = "#EAF4FF"
MUTED = "#9DB6D4"
ACCENT = "#4D79D8"
ACCENT_2 = "#5ED4FF"
SUCCESS = "#40C98B"
WARN = "#FFB84D"
ERROR = "#FF6B6B"


SLIDES = [
    {
        "slug": "mission_map",
        "title": "Track the mission live",
        "subtitle_lines": [
            "Map position, heading, altitude,",
            "and recovery zones in one dashboard.",
        ],
        "kind": "map",
    },
    {
        "slug": "telemetry_trends",
        "title": "Watch telemetry trends",
        "subtitle_lines": [
            "Follow battery, pressure, and flight data",
            "with high-contrast charts.",
        ],
        "kind": "chart",
    },
    {
        "slug": "actions_alerts",
        "title": "Control and respond faster",
        "subtitle_lines": [
            "Surface warnings and trigger mission actions",
            "without leaving the screen.",
        ],
        "kind": "actions",
    },
    {
        "slug": "network_status",
        "title": "Monitor every link",
        "subtitle_lines": [
            "See board status, network topology,",
            "and calibration state at a glance.",
        ],
        "kind": "network",
    },
]

SIZES = {
    "phone": (1080, 1920),
    "tablet7": (1600, 2560),
    "tablet10": (1920, 3072),
}


def run_magick(*args: str) -> None:
    subprocess.run(["magick", *args], check=True)


def render_svg(svg_path: Path, png_path: Path, width: int, height: int) -> None:
    renderer = shutil.which("rsvg-convert")
    if renderer is None:
        raise RuntimeError("rsvg-convert is required to rasterize the generated SVG assets")
    subprocess.run(
        [
            renderer,
            str(svg_path),
            "-w",
            str(width),
            "-h",
            str(height),
            "-o",
            str(png_path),
        ],
        check=True,
    )


def write_text(path: Path, text: str) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(text, encoding="utf-8")


def svg_header(width: int, height: int) -> str:
    return dedent(
        f"""\
        <svg xmlns="http://www.w3.org/2000/svg" width="{width}" height="{height}" viewBox="0 0 {width} {height}">
          <defs>
            <linearGradient id="bg" x1="0" y1="0" x2="1" y2="1">
              <stop offset="0%" stop-color="{BG}"/>
              <stop offset="100%" stop-color="{BG2}"/>
            </linearGradient>
            <linearGradient id="panel" x1="0" y1="0" x2="1" y2="1">
              <stop offset="0%" stop-color="{PANEL_2}"/>
              <stop offset="100%" stop-color="{PANEL}"/>
            </linearGradient>
            <linearGradient id="accent" x1="0" y1="0" x2="1" y2="0">
              <stop offset="0%" stop-color="{ACCENT}"/>
              <stop offset="100%" stop-color="{ACCENT_2}"/>
            </linearGradient>
            <filter id="shadow" x="-20%" y="-20%" width="140%" height="140%">
              <feDropShadow dx="0" dy="24" stdDeviation="30" flood-color="#000814" flood-opacity="0.35"/>
            </filter>
          </defs>
          <rect width="100%" height="100%" fill="url(#bg)"/>
          <circle cx="{int(width * 0.82)}" cy="{int(height * 0.14)}" r="{int(min(width, height) * 0.18)}" fill="#12355A" opacity="0.28"/>
          <circle cx="{int(width * 0.12)}" cy="{int(height * 0.78)}" r="{int(min(width, height) * 0.22)}" fill="#0E2842" opacity="0.42"/>
          <g opacity="0.14" stroke="#8CB8FF" stroke-width="2">
            <path d="M0 {int(height * 0.26)} H{width}"/>
            <path d="M0 {int(height * 0.50)} H{width}"/>
            <path d="M0 {int(height * 0.74)} H{width}"/>
            <path d="M{int(width * 0.2)} 0 V{height}"/>
            <path d="M{int(width * 0.5)} 0 V{height}"/>
            <path d="M{int(width * 0.8)} 0 V{height}"/>
          </g>
        """
    )


def svg_footer() -> str:
    return "</svg>\n"


def text_block(width: int, height: int, title: str, subtitle_lines: list[str]) -> str:
    pad_x = int(width * 0.08)
    subtitle_y = int(height * 0.235)
    subtitle_size = int(height * 0.024)
    subtitle_tspans = "".join(
        f'<tspan x="{pad_x}" dy="{0 if index == 0 else int(subtitle_size * 1.45)}">{line}</tspan>'
        for index, line in enumerate(subtitle_lines)
    )
    return dedent(
        f"""\
          <g font-family="Helvetica, Arial, sans-serif">
            <text x="{pad_x}" y="{int(height * 0.11)}" fill="{MUTED}" font-size="{int(height * 0.024)}" font-weight="700" letter-spacing="3">UBSEDS GROUNDSTATION</text>
            <text x="{pad_x}" y="{int(height * 0.19)}" fill="{TEXT}" font-size="{int(height * 0.045)}" font-weight="800">{title}</text>
            <text x="{pad_x}" y="{subtitle_y}" fill="{MUTED}" font-size="{subtitle_size}">{subtitle_tspans}</text>
          </g>
        """
    )


def top_badge(width: int, height: int) -> str:
    badge_w = int(width * 0.26)
    badge_h = int(height * 0.05)
    x = width - badge_w - int(width * 0.06)
    y = int(height * 0.08)
    return dedent(
        f"""\
          <g filter="url(#shadow)">
            <rect x="{x}" y="{y}" width="{badge_w}" height="{badge_h}" rx="{int(badge_h / 2)}" fill="#091321" opacity="0.92"/>
            <text x="{x + int(badge_w * 0.12)}" y="{y + int(badge_h * 0.64)}" fill="{ACCENT_2}" font-family="Helvetica, Arial, sans-serif" font-size="{int(height * 0.017)}" font-weight="700">LIVE TELEMETRY</text>
          </g>
        """
    )


def map_panel(width: int, height: int) -> str:
    x = int(width * 0.07)
    y = int(height * 0.30)
    w = int(width * 0.86)
    h = int(height * 0.58)
    return dedent(
        f"""\
          <g filter="url(#shadow)">
            <rect x="{x}" y="{y}" width="{w}" height="{h}" rx="36" fill="url(#panel)" stroke="#28507A" stroke-width="3"/>
            <rect x="{x + 24}" y="{y + 24}" width="{int(w * 0.62)}" height="{h - 48}" rx="24" fill="#0A1728" stroke="#244665" stroke-width="2"/>
            <g opacity="0.2" stroke="#86B8FF" stroke-width="2">
              <path d="M{x + 48} {y + 140} H{x + int(w * 0.62) - 24}"/>
              <path d="M{x + 48} {y + 260} H{x + int(w * 0.62) - 24}"/>
              <path d="M{x + 48} {y + 380} H{x + int(w * 0.62) - 24}"/>
              <path d="M{x + 140} {y + 48} V{y + h - 48}"/>
              <path d="M{x + 300} {y + 48} V{y + h - 48}"/>
              <path d="M{x + 460} {y + 48} V{y + h - 48}"/>
            </g>
            <path d="M{x + 120} {y + 430} C{x + 200} {y + 360}, {x + 270} {y + 250}, {x + 370} {y + 270} S{x + 560} {y + 390}, {x + 650} {y + 210}" fill="none" stroke="url(#accent)" stroke-width="12" stroke-linecap="round"/>
            <circle cx="{x + 650}" cy="{y + 210}" r="18" fill="{ACCENT_2}"/>
            <circle cx="{x + 230}" cy="{y + 320}" r="14" fill="{SUCCESS}"/>
            <circle cx="{x + 410}" cy="{y + 290}" r="14" fill="{WARN}"/>
            <rect x="{x + int(w * 0.66)}" y="{y + 24}" width="{int(w * 0.29)}" height="{int(h * 0.26)}" rx="24" fill="#0B1828"/>
            <text x="{x + int(w * 0.69)}" y="{y + 80}" fill="{MUTED}" font-family="Helvetica, Arial, sans-serif" font-size="34">Altitude</text>
            <text x="{x + int(w * 0.69)}" y="{y + 150}" fill="{TEXT}" font-family="Helvetica, Arial, sans-serif" font-size="74" font-weight="700">2,430 ft</text>
            <text x="{x + int(w * 0.69)}" y="{y + 220}" fill="{ACCENT_2}" font-family="Helvetica, Arial, sans-serif" font-size="34">Heading 148°</text>
            <rect x="{x + int(w * 0.66)}" y="{y + int(h * 0.33)}" width="{int(w * 0.29)}" height="{int(h * 0.22)}" rx="24" fill="#0B1828"/>
            <text x="{x + int(w * 0.69)}" y="{y + int(h * 0.42)}" fill="{MUTED}" font-family="Helvetica, Arial, sans-serif" font-size="34">Ground Speed</text>
            <text x="{x + int(w * 0.69)}" y="{y + int(h * 0.50)}" fill="{TEXT}" font-family="Helvetica, Arial, sans-serif" font-size="66" font-weight="700">42 mph</text>
            <rect x="{x + int(w * 0.66)}" y="{y + int(h * 0.60)}" width="{int(w * 0.29)}" height="{int(h * 0.30)}" rx="24" fill="#0B1828"/>
            <text x="{x + int(w * 0.69)}" y="{y + int(h * 0.70)}" fill="{MUTED}" font-family="Helvetica, Arial, sans-serif" font-size="34">Recovery Zone</text>
            <text x="{x + int(w * 0.69)}" y="{y + int(h * 0.78)}" fill="{TEXT}" font-family="Helvetica, Arial, sans-serif" font-size="48" font-weight="700">Northern Field</text>
            <text x="{x + int(w * 0.69)}" y="{y + int(h * 0.84)}" fill="{SUCCESS}" font-family="Helvetica, Arial, sans-serif" font-size="34">GPS lock stable</text>
          </g>
        """
    )


def chart_panel(width: int, height: int) -> str:
    x = int(width * 0.07)
    y = int(height * 0.30)
    w = int(width * 0.86)
    h = int(height * 0.58)
    return dedent(
        f"""\
          <g filter="url(#shadow)">
            <rect x="{x}" y="{y}" width="{w}" height="{h}" rx="36" fill="url(#panel)" stroke="#28507A" stroke-width="3"/>
            <rect x="{x + 24}" y="{y + 24}" width="{w - 48}" height="{int(h * 0.52)}" rx="28" fill="#0A1728"/>
            <g opacity="0.2" stroke="#86B8FF" stroke-width="2">
              <path d="M{x + 60} {y + 120} H{x + w - 60}"/>
              <path d="M{x + 60} {y + 240} H{x + w - 60}"/>
              <path d="M{x + 60} {y + 360} H{x + w - 60}"/>
            </g>
            <path d="M{x + 70} {y + 330} C{x + 180} {y + 290}, {x + 250} {y + 180}, {x + 360} {y + 210} S{x + 520} {y + 380}, {x + 660} {y + 170} S{x + 830} {y + 120}, {x + 930} {y + 155}" fill="none" stroke="{ACCENT}" stroke-width="10" stroke-linecap="round"/>
            <path d="M{x + 70} {y + 390} C{x + 160} {y + 350}, {x + 260} {y + 320}, {x + 360} {y + 280} S{x + 580} {y + 240}, {x + 930} {y + 260}" fill="none" stroke="{ACCENT_2}" stroke-width="8" stroke-linecap="round" opacity="0.9"/>
            <rect x="{x + 24}" y="{y + int(h * 0.59)}" width="{int(w * 0.29)}" height="{int(h * 0.28)}" rx="24" fill="#0B1828"/>
            <rect x="{x + int(w * 0.355)}" y="{y + int(h * 0.59)}" width="{int(w * 0.29)}" height="{int(h * 0.28)}" rx="24" fill="#0B1828"/>
            <rect x="{x + int(w * 0.69)}" y="{y + int(h * 0.59)}" width="{int(w * 0.26)}" height="{int(h * 0.28)}" rx="24" fill="#0B1828"/>
            <text x="{x + 56}" y="{y + int(h * 0.69)}" fill="{MUTED}" font-family="Helvetica, Arial, sans-serif" font-size="34">Battery</text>
            <text x="{x + 56}" y="{y + int(h * 0.78)}" fill="{TEXT}" font-family="Helvetica, Arial, sans-serif" font-size="66" font-weight="700">84%</text>
            <text x="{x + 56}" y="{y + int(h * 0.84)}" fill="{SUCCESS}" font-family="Helvetica, Arial, sans-serif" font-size="30">Estimated 42 min left</text>
            <text x="{x + int(w * 0.39)}" y="{y + int(h * 0.69)}" fill="{MUTED}" font-family="Helvetica, Arial, sans-serif" font-size="34">Tank Pressure</text>
            <text x="{x + int(w * 0.39)}" y="{y + int(h * 0.78)}" fill="{TEXT}" font-family="Helvetica, Arial, sans-serif" font-size="66" font-weight="700">311 psi</text>
            <text x="{x + int(w * 0.39)}" y="{y + int(h * 0.84)}" fill="{WARN}" font-family="Helvetica, Arial, sans-serif" font-size="30">Trending down 1.8 psi/min</text>
            <text x="{x + int(w * 0.72)}" y="{y + int(h * 0.69)}" fill="{MUTED}" font-family="Helvetica, Arial, sans-serif" font-size="34">Flight State</text>
            <text x="{x + int(w * 0.72)}" y="{y + int(h * 0.78)}" fill="{TEXT}" font-family="Helvetica, Arial, sans-serif" font-size="52" font-weight="700">Ascent</text>
            <text x="{x + int(w * 0.72)}" y="{y + int(h * 0.84)}" fill="{ACCENT_2}" font-family="Helvetica, Arial, sans-serif" font-size="30">Charts auto-refit</text>
          </g>
        """
    )


def actions_panel(width: int, height: int) -> str:
    x = int(width * 0.07)
    y = int(height * 0.30)
    w = int(width * 0.86)
    h = int(height * 0.58)
    return dedent(
        f"""\
          <g filter="url(#shadow)">
            <rect x="{x}" y="{y}" width="{w}" height="{h}" rx="36" fill="url(#panel)" stroke="#28507A" stroke-width="3"/>
            <rect x="{x + 24}" y="{y + 24}" width="{int(w * 0.44)}" height="{h - 48}" rx="28" fill="#0B1828"/>
            <rect x="{x + int(w * 0.50)}" y="{y + 24}" width="{int(w * 0.45)}" height="{h - 48}" rx="28" fill="#0B1828"/>
            <rect x="{x + 56}" y="{y + 90}" width="{int(w * 0.36)}" height="110" rx="24" fill="#11355B" stroke="#4A7DCA" stroke-width="2"/>
            <rect x="{x + 56}" y="{y + 235}" width="{int(w * 0.36)}" height="110" rx="24" fill="#1A402A" stroke="#40C98B" stroke-width="2"/>
            <rect x="{x + 56}" y="{y + 380}" width="{int(w * 0.36)}" height="110" rx="24" fill="#4B2618" stroke="#FF8B6C" stroke-width="2"/>
            <text x="{x + 95}" y="{y + 160}" fill="{TEXT}" font-family="Helvetica, Arial, sans-serif" font-size="42" font-weight="700">Arm Sequence</text>
            <text x="{x + 95}" y="{y + 305}" fill="{TEXT}" font-family="Helvetica, Arial, sans-serif" font-size="42" font-weight="700">Enable Logging</text>
            <text x="{x + 95}" y="{y + 450}" fill="{TEXT}" font-family="Helvetica, Arial, sans-serif" font-size="42" font-weight="700">Abort Link</text>
            <text x="{x + int(w * 0.54)}" y="{y + 85}" fill="{MUTED}" font-family="Helvetica, Arial, sans-serif" font-size="34">Warnings and notifications</text>
            <rect x="{x + int(w * 0.54)}" y="{y + 120}" width="{int(w * 0.34)}" height="92" rx="20" fill="#2C1F10"/>
            <rect x="{x + int(w * 0.54)}" y="{y + 232}" width="{int(w * 0.34)}" height="92" rx="20" fill="#102A1E"/>
            <rect x="{x + int(w * 0.54)}" y="{y + 344}" width="{int(w * 0.34)}" height="92" rx="20" fill="#2B161A"/>
            <text x="{x + int(w * 0.57)}" y="{y + 175}" fill="{WARN}" font-family="Helvetica, Arial, sans-serif" font-size="30" font-weight="700">Battery estimator recalculating</text>
            <text x="{x + int(w * 0.57)}" y="{y + 287}" fill="{SUCCESS}" font-family="Helvetica, Arial, sans-serif" font-size="30" font-weight="700">Telemetry queue healthy</text>
            <text x="{x + int(w * 0.57)}" y="{y + 399}" fill="{ERROR}" font-family="Helvetica, Arial, sans-serif" font-size="30" font-weight="700">Radio latency spike detected</text>
            <text x="{x + int(w * 0.57)}" y="{y + 505}" fill="{MUTED}" font-family="Helvetica, Arial, sans-serif" font-size="30">Critical actions stay one tap away.</text>
          </g>
        """
    )


def network_panel(width: int, height: int) -> str:
    x = int(width * 0.07)
    y = int(height * 0.30)
    w = int(width * 0.86)
    h = int(height * 0.58)
    return dedent(
        f"""\
          <g filter="url(#shadow)">
            <rect x="{x}" y="{y}" width="{w}" height="{h}" rx="36" fill="url(#panel)" stroke="#28507A" stroke-width="3"/>
            <rect x="{x + 24}" y="{y + 24}" width="{int(w * 0.54)}" height="{h - 48}" rx="28" fill="#0B1828"/>
            <rect x="{x + int(w * 0.61)}" y="{y + 24}" width="{int(w * 0.34)}" height="{h - 48}" rx="28" fill="#0B1828"/>
            <path d="M{x + 240} {y + 180} L{x + 440} {y + 280} L{x + 260} {y + 420}" stroke="#4477C9" stroke-width="8" fill="none"/>
            <path d="M{x + 440} {y + 280} L{x + 620} {y + 180} L{x + 620} {y + 420}" stroke="#4477C9" stroke-width="8" fill="none"/>
            <circle cx="{x + 240}" cy="{y + 180}" r="38" fill="{SUCCESS}"/>
            <circle cx="{x + 260}" cy="{y + 420}" r="38" fill="{SUCCESS}"/>
            <circle cx="{x + 440}" cy="{y + 280}" r="44" fill="{ACCENT_2}"/>
            <circle cx="{x + 620}" cy="{y + 180}" r="38" fill="{WARN}"/>
            <circle cx="{x + 620}" cy="{y + 420}" r="38" fill="{SUCCESS}"/>
            <text x="{x + 170}" y="{y + 250}" fill="{TEXT}" font-family="Helvetica, Arial, sans-serif" font-size="28">Flight CPU</text>
            <text x="{x + 175}" y="{y + 490}" fill="{TEXT}" font-family="Helvetica, Arial, sans-serif" font-size="28">Radio Link</text>
            <text x="{x + 380}" y="{y + 350}" fill="{TEXT}" font-family="Helvetica, Arial, sans-serif" font-size="30" font-weight="700">Ground Station</text>
            <text x="{x + 555}" y="{y + 250}" fill="{TEXT}" font-family="Helvetica, Arial, sans-serif" font-size="28">Camera</text>
            <text x="{x + 540}" y="{y + 490}" fill="{TEXT}" font-family="Helvetica, Arial, sans-serif" font-size="28">Tracker</text>
            <text x="{x + int(w * 0.65)}" y="{y + 90}" fill="{MUTED}" font-family="Helvetica, Arial, sans-serif" font-size="34">Calibration</text>
            <rect x="{x + int(w * 0.65)}" y="{y + 130}" width="{int(w * 0.24)}" height="78" rx="18" fill="#102A1E"/>
            <rect x="{x + int(w * 0.65)}" y="{y + 228}" width="{int(w * 0.24)}" height="78" rx="18" fill="#102A1E"/>
            <rect x="{x + int(w * 0.65)}" y="{y + 326}" width="{int(w * 0.24)}" height="78" rx="18" fill="#2C1F10"/>
            <rect x="{x + int(w * 0.65)}" y="{y + 424}" width="{int(w * 0.24)}" height="78" rx="18" fill="#102A1E"/>
            <text x="{x + int(w * 0.69)}" y="{y + 180}" fill="{SUCCESS}" font-family="Helvetica, Arial, sans-serif" font-size="28" font-weight="700">GPS aligned</text>
            <text x="{x + int(w * 0.69)}" y="{y + 278}" fill="{SUCCESS}" font-family="Helvetica, Arial, sans-serif" font-size="28" font-weight="700">IMU leveled</text>
            <text x="{x + int(w * 0.69)}" y="{y + 376}" fill="{WARN}" font-family="Helvetica, Arial, sans-serif" font-size="28" font-weight="700">Barometer pending</text>
            <text x="{x + int(w * 0.69)}" y="{y + 474}" fill="{SUCCESS}" font-family="Helvetica, Arial, sans-serif" font-size="28" font-weight="700">Radio verified</text>
          </g>
        """
    )


def slide_svg(width: int, height: int, slide: dict[str, str]) -> str:
    panel_map = {
        "map": map_panel,
        "chart": chart_panel,
        "actions": actions_panel,
        "network": network_panel,
    }
    panel = panel_map[slide["kind"]](width, height)
    return (
        svg_header(width, height)
        + text_block(width, height, slide["title"], slide["subtitle_lines"])
        + top_badge(width, height)
        + panel
        + svg_footer()
    )


def feature_svg(width: int, height: int) -> str:
    return dedent(
        f"""\
        <svg xmlns="http://www.w3.org/2000/svg" width="{width}" height="{height}" viewBox="0 0 {width} {height}">
          <defs>
            <linearGradient id="bg" x1="0" y1="0" x2="1" y2="1">
              <stop offset="0%" stop-color="{BG}"/>
              <stop offset="100%" stop-color="{BG2}"/>
            </linearGradient>
            <linearGradient id="accent" x1="0" y1="0" x2="1" y2="0">
              <stop offset="0%" stop-color="{ACCENT}"/>
              <stop offset="100%" stop-color="{ACCENT_2}"/>
            </linearGradient>
            <filter id="shadow" x="-20%" y="-20%" width="140%" height="140%">
              <feDropShadow dx="0" dy="16" stdDeviation="20" flood-color="#000814" flood-opacity="0.35"/>
            </filter>
          </defs>
          <rect width="100%" height="100%" fill="url(#bg)"/>
          <circle cx="850" cy="100" r="150" fill="#12355A" opacity="0.30"/>
          <circle cx="120" cy="420" r="180" fill="#0E2842" opacity="0.42"/>
          <g opacity="0.15" stroke="#8CB8FF" stroke-width="2">
            <path d="M0 120 H1024"/>
            <path d="M0 260 H1024"/>
            <path d="M0 400 H1024"/>
            <path d="M200 0 V500"/>
            <path d="M520 0 V500"/>
            <path d="M840 0 V500"/>
          </g>
          <g filter="url(#shadow)">
            <rect x="675" y="70" width="275" height="360" rx="34" fill="#0B1828" stroke="#244665" stroke-width="2"/>
            <rect x="698" y="110" width="104" height="145" rx="22" fill="#102238"/>
            <rect x="817" y="110" width="106" height="145" rx="22" fill="#102238"/>
            <rect x="698" y="275" width="225" height="125" rx="22" fill="#102238"/>
            <path d="M706 226 C732 210, 748 184, 776 190 S820 225, 890 165" fill="none" stroke="url(#accent)" stroke-width="8" stroke-linecap="round"/>
            <text x="716" y="160" fill="{MUTED}" font-family="Helvetica, Arial, sans-serif" font-size="18">Map</text>
            <text x="716" y="202" fill="{TEXT}" font-family="Helvetica, Arial, sans-serif" font-size="26" font-weight="700">Tracking</text>
            <text x="830" y="160" fill="{MUTED}" font-family="Helvetica, Arial, sans-serif" font-size="18">Status</text>
            <text x="830" y="203" fill="{SUCCESS}" font-family="Helvetica, Arial, sans-serif" font-size="22" font-weight="700">Connected</text>
            <text x="716" y="330" fill="{MUTED}" font-family="Helvetica, Arial, sans-serif" font-size="18">Telemetry</text>
            <text x="716" y="364" fill="{TEXT}" font-family="Helvetica, Arial, sans-serif" font-size="22" font-weight="700">Maps, charts,</text>
            <text x="716" y="392" fill="{TEXT}" font-family="Helvetica, Arial, sans-serif" font-size="22" font-weight="700">actions, alerts</text>
          </g>
          <text x="250" y="124" fill="{MUTED}" font-family="Helvetica, Arial, sans-serif" font-size="24" font-weight="700" letter-spacing="4">UBSEDS</text>
          <text x="250" y="196" fill="{TEXT}" font-family="Helvetica, Arial, sans-serif" font-size="62" font-weight="800">GroundStation</text>
          <text x="250" y="250" fill="{ACCENT_2}" font-family="Helvetica, Arial, sans-serif" font-size="32" font-weight="700">Mission telemetry on one screen</text>
          <text x="250" y="312" fill="{MUTED}" font-family="Helvetica, Arial, sans-serif" font-size="26">Track position, inspect live data, monitor warnings,</text>
          <text x="250" y="346" fill="{MUTED}" font-family="Helvetica, Arial, sans-serif" font-size="26">and command critical actions.</text>
        </svg>
        """
    )


def create_feature() -> None:
    svg_path = OUT_DIR / "feature_graphic.svg"
    base_png_path = OUT_DIR / "feature_graphic_base.png"
    png_path = OUT_DIR / "feature_graphic_1024x500.png"
    write_text(svg_path, feature_svg(1024, 500))
    render_svg(svg_path, base_png_path, 1024, 500)
    run_magick(
        str(base_png_path),
        "(",
        str(ICON),
        "-resize",
        "170x170",
        ")",
        "-gravity",
        "west",
        "-geometry",
        "+36-78",
        "-compose",
        "over",
        "-composite",
        str(png_path),
    )


def create_slides() -> None:
    for family, (width, height) in SIZES.items():
        family_dir = OUT_DIR / family
        family_dir.mkdir(parents=True, exist_ok=True)
        for index, slide in enumerate(SLIDES, start=1):
            svg_path = family_dir / f"{index:02d}_{slide['slug']}.svg"
            png_path = family_dir / f"{index:02d}_{slide['slug']}.png"
            write_text(svg_path, slide_svg(width, height, slide))
            render_svg(svg_path, png_path, width, height)


def main() -> None:
    OUT_DIR.mkdir(parents=True, exist_ok=True)
    create_feature()
    create_slides()
    print(f"Generated Play Store assets in {OUT_DIR}")


if __name__ == "__main__":
    try:
        main()
    except KeyboardInterrupt:
        print("\nImage generation interrupted.", file=sys.stderr)
        raise SystemExit(130)
