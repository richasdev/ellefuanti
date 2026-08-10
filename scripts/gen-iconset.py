#!/usr/bin/env python3
"""Regenerate assets/macos/ellefuanti.iconset from the pixel matrices in assets/macos/.

Run after editing art-18x10.txt or art-16x10.txt:

    python3 scripts/gen-iconset.py

Why this exists instead of `rsvg-convert app-icon.svg -w 16`:

The artwork is 18x10 art pixels. Asking any renderer to fit that into a 16pt icon means
resampling 18 source pixels onto ~13 device pixels, and the result was measured, not
guessed -- the eyes (single dark pixels) became two grey smudges and the legs (single-pixel
columns) vanished entirely. The elephant was an unreadable blob.

So every size here is drawn at an INTEGER pixel scale instead. One art pixel becomes an
NxN block, N whole, offset by whole pixels. Nothing is ever resampled, so 16x16 is as sharp
as 1024x1024.

Only the standard library is used, so this runs on a clean macOS box with no pip install.
"""

import os
import struct
import sys
import zlib

HERE = os.path.dirname(os.path.abspath(__file__))
ASSETS = os.path.join(HERE, "..", "assets", "macos")

# The four colours of the source artwork. "." is the transparent background -- macOS masks
# app icons itself, so an opaque fill would render as a white square in the Dock.
PALETTE = {
    "K": (0x01, 0x01, 0x01, 255),  # eyes
    "D": (0x70, 0x72, 0x76, 255),  # ears, feet -- the darker grey
    "M": (0x9F, 0x9F, 0xA3, 255),  # body -- the mid grey
    ".": (0, 0, 0, 0),
}


def write_png(path, size, rgba):
    scanlines = b"".join(
        b"\x00" + bytes(rgba[y * size * 4 : (y + 1) * size * 4]) for y in range(size)
    )

    def chunk(tag, data):
        body = tag + data
        return struct.pack(">I", len(data)) + body + struct.pack(">I", zlib.crc32(body) & 0xFFFFFFFF)

    png = b"\x89PNG\r\n\x1a\n"
    png += chunk(b"IHDR", struct.pack(">IIBBBBB", size, size, 8, 6, 0, 0, 0))
    png += chunk(b"IDAT", zlib.compress(scanlines, 9))
    png += chunk(b"IEND", b"")
    with open(path, "wb") as fh:
        fh.write(png)


def render(art, canvas, scale, path):
    """Blit `art` centred on a transparent canvas x canvas PNG at `scale` px per art pixel."""
    rows, cols = len(art), len(art[0])
    w, h = cols * scale, rows * scale
    if w > canvas or h > canvas:
        raise SystemExit("art %dx%d at x%d does not fit %d" % (cols, rows, scale, canvas))
    # Integer offsets. A half-pixel offset would reintroduce exactly the blurring that
    # drawing at integer scale is here to avoid.
    ox, oy = (canvas - w) // 2, (canvas - h) // 2
    buf = bytearray(canvas * canvas * 4)
    for r, line in enumerate(art):
        for c, sym in enumerate(line):
            colour = PALETTE[sym]
            if colour[3] == 0:
                continue
            run = bytes(colour) * scale
            for dy in range(scale):
                start = ((oy + r * scale + dy) * canvas + ox + c * scale) * 4
                buf[start : start + 4 * scale] = run
    write_png(path, canvas, buf)
    return w, h


def load(name):
    with open(os.path.join(ASSETS, name)) as fh:
        art = fh.read().rstrip("\n").split("\n")
    if len({len(line) for line in art}) != 1:
        raise SystemExit("%s: rows are not all the same length" % name)
    return art


def main():
    full = load("art-18x10.txt")  # the whole elephant, including the detached tail tip
    small = load("art-16x10.txt")  # tail dropped; see the comment on the 16pt entries below

    # (iconset name, canvas px, art, integer scale)
    #
    # Scale is the largest whole number keeping the art inside ~80% of the canvas, which is
    # the Big Sur+ safe area.
    #
    # The 16 and 32 device-pixel entries are the exception: 80% of 16px is 12 pixels, and no
    # elephant survives being drawn 12 pixels wide. Those use the 16-wide matrix edge to
    # edge. Filling the frame at menu-bar and list-view sizes is normal -- Apple's own small
    # icons do it -- and the alternative here is not a smaller elephant but an unreadable one.
    plan = [
        ("icon_16x16.png", 16, small, 1),
        ("icon_16x16@2x.png", 32, small, 2),
        ("icon_32x32.png", 32, small, 2),
        ("icon_32x32@2x.png", 64, full, 3),
        ("icon_128x128.png", 128, full, 6),
        ("icon_128x128@2x.png", 256, full, 11),
        ("icon_256x256.png", 256, full, 11),
        ("icon_256x256@2x.png", 512, full, 22),
        ("icon_512x512.png", 512, full, 22),
        ("icon_512x512@2x.png", 1024, full, 45),
    ]

    out = os.path.join(ASSETS, "ellefuanti.iconset")
    os.makedirs(out, exist_ok=True)
    for name, canvas, art, scale in plan:
        w, _h = render(art, canvas, scale, os.path.join(out, name))
        print("%-22s %4dpx  x%-2d  %3d%% wide" % (name, canvas, scale, round(100 * w / canvas)))
    print("wrote", out)


if __name__ == "__main__":
    sys.exit(main())
