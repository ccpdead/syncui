import struct, zlib, os

def make_png(path, size, rgba=(54, 120, 214, 255)):
    w = h = size
    raw = bytearray()
    r, g, b, a = rgba
    for y in range(h):
        raw.append(0)  # filter type 0
        for x in range(w):
            # simple diagonal gradient so the icon isn't totally flat
            t = (x + y) / (2 * size)
            raw += bytes([
                int(r * (0.7 + 0.3 * t)),
                int(g * (0.7 + 0.3 * t)),
                int(b * (0.7 + 0.3 * t)),
                a,
            ])

    def chunk(typ, data):
        c = struct.pack(">I", len(data)) + typ + data
        c += struct.pack(">I", zlib.crc32(typ + data) & 0xFFFFFFFF)
        return c

    sig = b"\x89PNG\r\n\x1a\n"
    ihdr = struct.pack(">IIBBBBB", w, h, 8, 6, 0, 0, 0)
    idat = zlib.compress(bytes(raw), 9)
    png = sig + chunk(b"IHDR", ihdr) + chunk(b"IDAT", idat) + chunk(b"IEND", b"")
    with open(path, "wb") as f:
        f.write(png)
    print("wrote", path, size)

here = os.path.join(os.path.dirname(__file__), "icons")
os.makedirs(here, exist_ok=True)
make_png(os.path.join(here, "32x32.png"), 32)
make_png(os.path.join(here, "128x128.png"), 128)
make_png(os.path.join(here, "128x128@2x.png"), 256)
make_png(os.path.join(here, "icon.png"), 512)
