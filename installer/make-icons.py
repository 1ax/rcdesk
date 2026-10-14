#!/usr/bin/env python3
"""Генерирует все иконки rcdesk из installer/logo.svg (источник правды).

Как перегенерировать (из корня репозитория, macOS):

    python3 installer/make-icons.py

Пишет:
  installer/windows/rcdesk.ico        кадры 16/32/48/256 (PNG внутри ICO)
  installer/macos/rcdesk.icns         iconset на белом фоне -> iconutil
  web/public/icons/                   icon-192/512, apple-touch-icon, favicon
  host/src/agent/icon/*.rgba          сырой RGBA для значка трея/строки меню

Только стандартная библиотека; рендер SVG -- qlmanage, ресайз -- sips,
сборка icns -- iconutil (все есть в macOS). Варианты SVG: white (как есть),
transparent (без фона), mono (без фона, все заливки чёрные). Скрипт
идемпотентен. qlmanage всегда кладёт картинку на непрозрачный фон, поэтому
прозрачные варианты получаются из двух рендеров (на белом и на чёрном):
alpha = 255 - (W - B), цвет = B * 255 / alpha.
"""
import re
import shutil
import struct
import subprocess
import sys
import tempfile
import zlib
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
LOGO = ROOT / "installer" / "logo.svg"
WRITTEN = []


def die(msg):
    sys.exit(f"make-icons: {msg}")


def write(path, data):
    path.parent.mkdir(parents=True, exist_ok=True)
    if isinstance(data, str):
        path.write_text(data, encoding="utf-8")
    else:
        path.write_bytes(data)
    WRITTEN.append(path)


def variants(svg):
    no_bg = re.sub(r"\s*<rect\b[^>]*/>", "", svg, count=1)
    if no_bg == svg:
        die("в logo.svg не найден <rect> фона")
    mono = re.sub(r'fill="#[0-9a-fA-F]{3,6}"', 'fill="#000"', no_bg)
    return {"white": svg, "transparent": no_bg, "mono": mono}


def with_bg(svg_text, color):
    return re.sub(r"(<svg\b[^>]*>)", rf'\1<rect width="1024" height="1024" fill="{color}"/>',
                  svg_text, count=1)


def render_1024(svg_text, name, tmp):
    src = tmp / f"{name}.svg"
    src.write_text(svg_text, encoding="utf-8")
    out = tmp / f"out-{name}"
    out.mkdir()
    subprocess.run(
        ["qlmanage", "-t", "-s", "1024", "-o", str(out), str(src)],
        check=True, capture_output=True,
    )
    png = out / f"{name}.svg.png"
    if not png.exists():
        die(f"qlmanage не создал {png}")
    return png


def resize(png, size, dst):
    subprocess.run(
        ["sips", "-z", str(size), str(size), str(png), "--out", str(dst)],
        check=True, capture_output=True,
    )
    return dst


def decode_png(data):
    """Минимальный декодер PNG: 8 бит RGBA, без interlace. -> (w, h, bytes)."""
    if data[:8] != b"\x89PNG\r\n\x1a\n":
        die("не PNG")
    pos, idat, ihdr = 8, b"", None
    while pos < len(data):
        ln, typ = struct.unpack(">I4s", data[pos:pos + 8])
        body = data[pos + 8:pos + 8 + ln]
        pos += 12 + ln
        if typ == b"IHDR":
            ihdr = struct.unpack(">IIBBBBB", body)
        elif typ == b"IDAT":
            idat += body
        elif typ == b"IEND":
            break
    w, h, depth, ctype, _, _, interlace = ihdr
    if (depth, ctype, interlace) != (8, 6, 0):
        die(f"PNG не 8-бит RGBA non-interlaced: depth={depth} color={ctype} interlace={interlace}")
    raw = zlib.decompress(idat)
    stride = w * 4
    out = bytearray()
    prev = bytearray(stride)
    p = 0
    for _ in range(h):
        ft = raw[p]
        line = bytearray(raw[p + 1:p + 1 + stride])
        p += 1 + stride
        for i in range(stride):
            a = line[i - 4] if i >= 4 else 0
            b = prev[i]
            c = prev[i - 4] if i >= 4 else 0
            if ft == 0:
                x = 0
            elif ft == 1:
                x = a
            elif ft == 2:
                x = b
            elif ft == 3:
                x = (a + b) // 2
            elif ft == 4:
                pa, pb, pc = abs(b - c), abs(a - c), abs(a + b - 2 * c)
                x = a if pa <= pb and pa <= pc else (b if pb <= pc else c)
            else:
                die(f"неизвестный фильтр PNG {ft}")
            line[i] = (line[i] + x) & 0xFF
        out += line
        prev = line
    return w, h, bytes(out)


def to_rgba(px):
    px = bytearray(px)
    for i in range(3, len(px), 4):
        if px[i] >= 250:  # qlmanage может давать 254 вместо 255
            px[i] = 255
    return bytes(px)


def encode_png(w, h, rgba):
    def chunk(tag, data):
        body = tag + data
        return struct.pack(">I", len(data)) + body + struct.pack(">I", zlib.crc32(body))

    rows = b"".join(b"\x00" + rgba[y * w * 4:(y + 1) * w * 4] for y in range(h))
    return (b"\x89PNG\r\n\x1a\n"
            + chunk(b"IHDR", struct.pack(">IIBBBBB", w, h, 8, 6, 0, 0, 0))
            + chunk(b"IDAT", zlib.compress(rows, 9)) + chunk(b"IEND", b""))


def unmatte(white, black):
    """Два композита (на белом и чёрном) -> straight RGBA."""
    out = bytearray(len(white))
    for i in range(0, len(white), 4):
        diff = sum(white[i + k] - black[i + k] for k in range(3)) // 3
        a = max(0, min(255, 255 - diff))
        out[i + 3] = a
        if a:
            for k in range(3):
                out[i + k] = min(255, (black[i + k] * 255 + a // 2) // a)
    return bytes(out)


def make_ico(frames):
    n = len(frames)
    head = struct.pack("<HHH", 0, 1, n)
    entries, blobs, off = b"", b"", 6 + 16 * n
    for size, data in frames:
        d = 0 if size >= 256 else size
        entries += struct.pack("<BBBBHHII", d, d, 0, 0, 1, 32, len(data), off)
        blobs += data
        off += len(data)
    return head + entries + blobs


def main():
    if not LOGO.exists():
        die(f"нет {LOGO}")
    vs = variants(LOGO.read_text(encoding="utf-8"))
    with tempfile.TemporaryDirectory() as t:
        tmp = Path(t)
        big = {"white": render_1024(vs["white"], "white", tmp)}
        for k in ("transparent", "mono"):
            big[k + "-w"] = render_1024(with_bg(vs[k], "#fff"), k + "-w", tmp)
            big[k + "-b"] = render_1024(with_bg(vs[k], "#000"), k + "-b", tmp)

        def sized(variant, size):
            if variant == "white":
                return resize(big[variant], size, tmp / f"white-{size}.png")
            dst = tmp / f"{variant}-{size}.png"
            px = []
            for bg in ("w", "b"):
                r = resize(big[f"{variant}-{bg}"], size, tmp / f"{variant}-{bg}-{size}.png")
                w, h, d = decode_png(r.read_bytes())
                if (w, h) != (size, size):
                    die(f"ожидали {size}x{size}, получили {w}x{h}")
                px.append(d)
            dst.write_bytes(encode_png(size, size, unmatte(*px)))
            return dst

        # ICO
        frames = [(s, sized("transparent", s).read_bytes()) for s in (16, 32, 48, 256)]
        write(ROOT / "installer/windows/rcdesk.ico", make_ico(frames))

        # ICNS
        iconset = tmp / "rcdesk.iconset"
        iconset.mkdir()
        for base in (16, 32, 128, 256, 512):
            for scale, suffix in ((1, ""), (2, "@2x")):
                px = base * scale
                src = big["white"] if px == 1024 else sized("white", px)
                shutil.copy(src, iconset / f"icon_{base}x{base}{suffix}.png")
        icns = tmp / "rcdesk.icns"
        subprocess.run(["iconutil", "-c", "icns", str(iconset), "-o", str(icns)], check=True)
        write(ROOT / "installer/macos/rcdesk.icns", icns.read_bytes())

        # web
        icons = ROOT / "web/public/icons"
        write(icons / "icon-192.png", sized("white", 192).read_bytes())
        write(icons / "icon-512.png", sized("white", 512).read_bytes())
        write(icons / "apple-touch-icon.png", sized("white", 180).read_bytes())
        write(icons / "favicon-32.png", sized("transparent", 32).read_bytes())
        write(icons / "favicon.svg", vs["transparent"])

        # трей
        tray = ROOT / "host/src/agent/icon"
        write(tray / "mono-44.rgba", to_rgba(decode_png(sized("mono", 44).read_bytes())[2]))
        write(tray / "color-32.rgba", to_rgba(decode_png(sized("transparent", 32).read_bytes())[2]))

    for p in WRITTEN:
        print(f"{p.relative_to(ROOT)}  {p.stat().st_size} B")


if __name__ == "__main__":
    main()
