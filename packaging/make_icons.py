#!/usr/bin/env python3
"""Builds the app icons and the window logo from packaging/quitcode-logo.svg.

Writes packaging/icons/*.png (app icon on a rounded tile), icon.ico and
logo.png (the bare logo, 128 px, shown in the window). Needs `resvg` on PATH
(`cargo install resvg --locked`)."""

import re
import struct
import subprocess
import tempfile
from pathlib import Path

HERE = Path(__file__).resolve().parent
OUT = HERE / "icons"
LOGO = HERE / "quitcode-logo.svg"


def logo_body():
    """The logo's <defs> and <g> content, to embed in another SVG."""
    svg = LOGO.read_text()
    return svg[svg.index(">", svg.index("<svg")) + 1:svg.rindex("</svg>")]


def tile_svg():
    # macOS icon grid: 824 px body with ~185 px corners on a 1024 canvas.
    s = 600 / 106.68
    return f"""<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 1024 1024">
  <rect x="100" y="100" width="824" height="824" rx="185" fill="#ffffff"/>
  <rect x="100.5" y="100.5" width="823" height="823" rx="185" fill="none" stroke="#000" stroke-opacity="0.08"/>
  <g transform="translate(212 212) scale({s:.5f})">{logo_body()}</g>
</svg>"""


def render(svg_path, size, out):
    subprocess.run(["resvg", "-w", str(size), "-h", str(size), str(svg_path), str(out)], check=True)


def ico(images):
    header = struct.pack("<HHH", 0, 1, len(images))
    offset = 6 + 16 * len(images)
    entries, blobs = b"", b""
    for size, data in images:
        dim = 0 if size >= 256 else size
        entries += struct.pack("<BBBBHHII", dim, dim, 0, 0, 1, 32, len(data), offset)
        blobs += data
        offset += len(data)
    return header + entries + blobs


def main():
    OUT.mkdir(exist_ok=True)
    with tempfile.TemporaryDirectory() as tmp:
        tile = Path(tmp) / "tile.svg"
        tile.write_text(tile_svg())
        for size in (32, 128, 256, 512, 1024):
            render(tile, size, OUT / f"{size}x{size}.png")
        small = {}
        for size in (16, 48):
            render(tile, size, Path(tmp) / f"{size}.png")
            small[size] = (Path(tmp) / f"{size}.png").read_bytes()
        pngs = {16: small[16], 32: (OUT / "32x32.png").read_bytes(), 48: small[48],
                256: (OUT / "256x256.png").read_bytes()}
        (OUT / "icon.ico").write_bytes(ico(sorted(pngs.items())))
    render(LOGO, 128, OUT / "logo.png")
    print("written to", OUT)


if __name__ == "__main__":
    main()
