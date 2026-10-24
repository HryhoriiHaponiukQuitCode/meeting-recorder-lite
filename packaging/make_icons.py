#!/usr/bin/env python3
"""Draws the app icon (dark rounded square, red dot, two level bars) and
writes packaging/icons/*.png and icon.ico. Pure standard library."""

import math
import struct
import zlib
from pathlib import Path

OUT = Path(__file__).resolve().parent / "icons"


def coverage_round_rect(x, y, x0, y0, x1, y1, r):
    cx = min(max(x, x0 + r), x1 - r)
    cy = min(max(y, y0 + r), y1 - r)
    d = math.hypot(x - cx, y - cy)
    return min(max(r + 0.5 - d, 0.0), 1.0)


def draw(size):
    s = size / 1024.0
    px = bytearray(size * size * 4)
    bg = (28, 28, 30)
    red = (255, 59, 48)
    green = (52, 199, 89)
    bars = [(600, 330, 690, 760), (760, 470, 850, 760)]  # x0, top, x1, bottom
    for y in range(size):
        for x in range(size):
            fx, fy = (x + 0.5) / s, (y + 0.5) / s
            a = coverage_round_rect(fx, fy, 64, 64, 960, 960, 200) if s else 0
            a = min(1.0, a * 1.0)
            rgb = bg
            if a > 0:
                d = math.hypot(fx - 380, fy - 512)
                c = min(max((190 + 0.5 / s - d) * s, 0.0), 1.0)
                if c > 0:
                    rgb = tuple(round(bg[i] * (1 - c) + red[i] * c) for i in range(3))
                for x0, top, x1, bottom in bars:
                    c = coverage_round_rect(fx, fy, x0, top, x1, bottom, 45)
                    c = min(max(c, 0.0), 1.0)
                    if c > 0:
                        rgb = tuple(round(rgb[i] * (1 - c) + green[i] * c) for i in range(3))
            i = (y * size + x) * 4
            px[i:i + 4] = bytes((*rgb, round(a * 255)))
    return bytes(px)


def png(size, rgba):
    raw = b"".join(b"\x00" + rgba[y * size * 4:(y + 1) * size * 4] for y in range(size))

    def chunk(kind, data):
        return struct.pack(">I", len(data)) + kind + data + struct.pack(">I", zlib.crc32(kind + data))

    return (b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", struct.pack(">IIBBBBB", size, size, 8, 6, 0, 0, 0))
            + chunk(b"IDAT", zlib.compress(raw, 9)) + chunk(b"IEND", b""))


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
    pngs = {}
    for size in (16, 32, 48, 128, 256, 512, 1024):
        pngs[size] = png(size, draw(size))
    for size in (32, 128, 256, 512, 1024):
        (OUT / f"{size}x{size}.png").write_bytes(pngs[size])
    (OUT / "icon.ico").write_bytes(ico([(s, pngs[s]) for s in (16, 32, 48, 256)]))
    print("written to", OUT)


if __name__ == "__main__":
    main()
