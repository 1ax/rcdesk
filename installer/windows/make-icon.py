#!/usr/bin/env python3
"""Генерирует rcdesk.ico: монитор в духе значка трея (host/src/agent/icon.rs).

Как перегенерировать (из корня репозитория):

    python3 installer/windows/make-icon.py

Только стандартная библиотека (zlib, struct), без PIL. Кадры 16/32/48/256
пишутся как PNG внутри ICO (Windows Vista+ читает такие кадры). Геометрия --
те же доли размера, что у трея: рамка 1/6 от краёв, экран 4:3, подставка;
цвета -- цвета Windows-ветки трея (рамка серая, экран зелёный, «сеанс»).
Супервыборка 4x даёт сглаженные края на мелких размерах.
"""
import struct
import zlib
from pathlib import Path

SIZES = [16, 32, 48, 256]
SS = 4  # супервыборка
BEZEL = (90, 90, 90, 255)
SCREEN = (40, 170, 70, 255)


def render(size):
    n = size * SS
    s = n
    left = s // 6
    right = s - s // 6
    top = s // 6
    bottom = top + (right - left) * 3 // 4
    stroke = max(s // 12, 1 * SS)
    stand_w = (right - left) // 3
    stand_x = (left + right) // 2 - stand_w // 2
    hi = [[(0, 0, 0, 0)] * n for _ in range(n)]
    for y in range(top, bottom):
        for x in range(left, right):
            bezel = (
                x < left + stroke
                or x >= right - stroke
                or y < top + stroke
                or y >= bottom - stroke
            )
            hi[y][x] = BEZEL if bezel else SCREEN
    for y in range(bottom, min(bottom + stroke, n)):
        for x in range(stand_x, stand_x + stand_w):
            hi[y][x] = BEZEL
    # Усреднение блоков SSxSS с предумноженной альфой.
    rows = []
    for y in range(size):
        row = bytearray([0])  # фильтр PNG: None
        for x in range(size):
            r = g = b = a = 0
            for dy in range(SS):
                for dx in range(SS):
                    pr, pg, pb, pa = hi[y * SS + dy][x * SS + dx]
                    r += pr * pa
                    g += pg * pa
                    b += pb * pa
                    a += pa
            cnt = SS * SS
            if a == 0:
                row += bytes((0, 0, 0, 0))
            else:
                row += bytes((r // a, g // a, b // a, a // cnt))
        rows.append(bytes(row))
    return b"".join(rows)


def png(size):
    def chunk(tag, data):
        body = tag + data
        return struct.pack(">I", len(data)) + body + struct.pack(">I", zlib.crc32(body))

    ihdr = struct.pack(">IIBBBBB", size, size, 8, 6, 0, 0, 0)
    return (
        b"\x89PNG\r\n\x1a\n"
        + chunk(b"IHDR", ihdr)
        + chunk(b"IDAT", zlib.compress(render(size), 9))
        + chunk(b"IEND", b"")
    )


def main():
    frames = [png(s) for s in SIZES]
    head = struct.pack("<HHH", 0, 1, len(SIZES))
    offset = 6 + 16 * len(SIZES)
    entries = b""
    for size, data in zip(SIZES, frames):
        dim = 0 if size >= 256 else size  # 0 означает 256
        entries += struct.pack("<BBBBHHII", dim, dim, 0, 0, 1, 32, len(data), offset)
        offset += len(data)
    out = Path(__file__).with_name("rcdesk.ico")
    out.write_bytes(head + entries + b"".join(frames))
    print(f"{out}: {out.stat().st_size} байт, размеры {SIZES}")


if __name__ == "__main__":
    main()
