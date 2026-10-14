"""Generate the diskscope app icon: assets/diskscope.ico (16-256 px) and assets/icon_256.png.

The mark: a treemap in the app's palette with a scope (a lens ring and four reticle ticks)
locked onto the one tile that actually holds the space. That tile glows in the accent colour.
Inside the lens the treemap is magnified and at full brightness; outside it is dimmed toward
the dark plate, so the eye goes straight to the hotspot.

48-256 px are drawn from one design in 256-unit coordinates (thicker ring and ticks at 48 and
64). 16, 24 and 32 px use a simplified layout on their own pixel grid: fewer tiles, a thick ring,
no glow, sheen or magnification. Everything is drawn 8x supersampled and LANCZOS-downsampled.

    python assets/make_icon.py [--preview out.png]

Needs Pillow. The .ico stores BMP (32-bit BGRA + AND mask) frames below 256 px and a PNG frame
at 256 px, the layout Windows' own icons use.
"""

import io
import math
import struct
import sys
from pathlib import Path

from PIL import Image, ImageChops, ImageDraw, ImageFilter

HERE = Path(__file__).resolve().parent
SIZES = [16, 24, 32, 48, 64, 128, 256]
SS = 8  # supersampling factor

# colours (src/theme.rs palette)
ACCENT = (134, 148, 255)
BLUE = (98, 128, 210)
TEAL = (70, 160, 152)
SKY = (82, 152, 196)
VIOLET = (136, 116, 208)
FILE = (92, 102, 120)
GREY = (110, 114, 124)  # what the dimmed tiles lean toward
PLATE_TOP = (44, 48, 61)
PLATE_BOTTOM = (13, 14, 18)
LENS_FLOOR = (15, 16, 21)
HOT_IN = (158, 170, 255)  # hotspot: bright core ...
HOT_OUT = (92, 106, 236)  # ... deeper periwinkle at the edges
RING_TOP = (222, 228, 255)
RING_BOTTOM = (130, 144, 246)


def shade(c, f):
    return tuple(max(0, min(255, round(v * f))) for v in c[:3])


def mix(a, b, t):
    return tuple(round(a[i] + (b[i] - a[i]) * t) for i in range(3))


def dimmed(col, dim, desat):
    return shade(mix(col, GREY, desat), dim)


# ---------------------------------------------------------------------------------------------
# Supersampled canvas


class Canvas:
    """An RGBA canvas s*SS pixels square; drawing coordinates are in units of `d` per side."""

    def __init__(self, s, d):
        self.s = s
        self.n = s * SS
        self.u = self.n / d
        self.img = Image.new("RGBA", (self.n, self.n), (0, 0, 0, 0))

    def p(self, v):
        return v * self.u

    def mask(self, v=0):
        return Image.new("L", (self.n, self.n), v)

    def rrect(self, x0, y0, x1, y1, r, m=None, fill=255):
        m = m or self.mask()
        box = [self.p(x0), self.p(y0), self.p(x1) - 1, self.p(y1) - 1]
        ImageDraw.Draw(m).rounded_rectangle(box, radius=self.p(r), fill=fill)
        return m

    def circle(self, cx, cy, r, m=None, fill=255):
        m = m or self.mask()
        box = [self.p(cx - r), self.p(cy - r), self.p(cx + r) - 1, self.p(cy + r) - 1]
        ImageDraw.Draw(m).ellipse(box, fill=fill)
        return m

    def ring(self, cx, cy, r_out, r_in):
        return self.circle(cx, cy, r_in, self.circle(cx, cy, r_out), 0)

    def solid(self, c):
        return Image.new("RGB", (self.n, self.n), c[:3])

    def vgrad(self, c0, c1, y0, y1):
        """c0 at y0 to c1 at y1, held flat beyond them."""
        a, b = int(self.p(y0)), int(self.p(y1))
        g = Image.linear_gradient("L").resize((self.n, max(1, b - a)), Image.BILINEAR)
        m = self.mask()
        m.paste(255, (0, max(0, b), self.n, self.n))
        m.paste(g, (0, a))
        return Image.composite(self.solid(c1), self.solid(c0), m)

    def rgrad(self, cx, cy, r, c_in, c_out, gamma=1.0):
        """c_in at the centre to c_out at radius r and beyond."""
        g = Image.radial_gradient("L")
        if gamma != 1.0:
            g = g.point(lambda v: round(255 * (v / 255) ** gamma))
        rr = int(self.p(r))
        m = self.mask(255)
        m.paste(g.resize((2 * rr, 2 * rr), Image.BILINEAR), (int(self.p(cx)) - rr, int(self.p(cy)) - rr))
        return Image.composite(self.solid(c_out), self.solid(c_in), m)

    def blur(self, m, r):
        return m.filter(ImageFilter.GaussianBlur(self.p(r)))

    def paint(self, fill, m, opacity=1.0):
        if isinstance(fill, tuple):
            fill = self.solid(fill)
        if opacity != 1.0:
            m = m.point(lambda v: round(v * opacity))
        layer = fill.convert("RGBA")
        layer.putalpha(m)
        self.img.alpha_composite(layer)

    def light(self, col, m, opacity):
        """Additive light: brightens what is already there, never adds coverage."""
        pre = Image.composite(self.solid(col), self.solid((0, 0, 0)), m.point(lambda v: round(v * opacity)))
        a = self.img.getchannel("A")
        self.img = ImageChops.add(self.img.convert("RGB"), pre).convert("RGBA")
        self.img.putalpha(a)

    def clip(self, m):
        self.img.putalpha(ImageChops.multiply(self.img.getchannel("A"), m))

    def out(self):
        return self.img.resize((self.s, self.s), Image.LANCZOS)


def sub(a, b):
    return ImageChops.subtract(a, b)


def mul(a, b):
    return ImageChops.multiply(a, b)


# ---------------------------------------------------------------------------------------------
# 48-256 px: the full design, in 256 units

TILES = [
    (32, 32, 112, 150, BLUE),
    (32, 156, 112, 224, shade(BLUE, 0.8)),
    (118, 32, 224, 110, TEAL),
    (164, 116, 224, 180, SKY),
    (164, 186, 224, 224, FILE),
    (118, 166, 158, 224, VIOLET),
]
HOT = (118, 116, 158, 160)  # the tile the scope is locked onto
LENS = (138, 138, 66)  # centre x, centre y, ring radius (mid-stroke)
MAG = 1.3  # lens magnification
TILE_R = 8
DIM, DESAT = 0.74, 0.08  # treemap outside the lens
PLATE = (8, 54)  # margin, corner radius

FULL = {
    256: dict(ring_w=9, tick_w=5, tick_len=9, tick_gap=6, sheen=0.14, spill=0.12, glow=0.6),
    128: dict(ring_w=9, tick_w=5, tick_len=9, tick_gap=6, sheen=0.14, spill=0.12, glow=0.6),
    64: dict(ring_w=11, tick_w=7, tick_len=10, tick_gap=6, sheen=0.10, spill=0.10, glow=0.5),
    48: dict(ring_w=14, tick_w=9, tick_len=12, tick_gap=7, sheen=0.0, spill=0.06, glow=0.45),
}


def plate(c, s):
    m, r = PLATE
    pm = c.rrect(m, m, 256 - m, 256 - m, r)
    c.paint(c.vgrad(PLATE_TOP, PLATE_BOTTOM, m, 256 - m), pm)
    # gentle highlight along the top edge, fading out by mid-height
    w = max(1.6, 256 / s * 0.9)
    rim = sub(pm, c.rrect(m + w, m + w, 256 - m - w, 256 - m - w, r - w))
    c.paint((255, 255, 255), mul(rim, c.vgrad((70,) * 3, (8,) * 3, m, 150).convert("L")))
    return pm


def treemap(s, dim, desat):
    """The tiles (hotspot included) on their own layer."""
    c = Canvas(s, 256)
    for x0, y0, x1, y1, col in TILES:
        col = dimmed(col, dim, desat)
        m = c.rrect(x0, y0, x1, y1, TILE_R)
        c.paint(c.vgrad(shade(col, 1.08), shade(col, 0.80), y0, y1), m)
        c.paint((255, 255, 255), sub(m, c.rrect(x0, y0 + 1.6, x1, y1, TILE_R)), 0.16)
    x0, y0, x1, y1 = HOT
    m = c.rrect(x0, y0, x1, y1, TILE_R)
    c.paint(c.rgrad((x0 + x1) / 2, (y0 + y1) / 2 - 5, max(x1 - x0, y1 - y0) * 0.85, HOT_IN, HOT_OUT), m)
    c.paint((255, 255, 255), sub(m, c.rrect(x0, y0 + 1.6, x1, y1, TILE_R)), 0.55)
    return c.img


def ticks(c, cx, cy, r0, length, w):
    """Four rounded reticle ticks pointing at the lens, starting r0 from its centre."""
    m = c.mask()
    d = ImageDraw.Draw(m)
    q = c.p(w / 2)
    for ang in (0, 90, 180, 270):
        a = math.radians(ang)
        pts = [(c.p(cx + math.cos(a) * r), c.p(cy + math.sin(a) * r)) for r in (r0, r0 + length)]
        d.line(pts, fill=255, width=round(c.p(w)))
        for x, y in pts:
            d.ellipse([x - q, y - q, x + q, y + q], fill=255)
    return m


def render_full(s):
    P = FULL[s]
    c = Canvas(s, 256)
    pm = plate(c, s)
    cx, cy, R = LENS
    rw = P["ring_w"]
    ri = R - rw / 2
    # treemap outside the lens: dimmed, a vignette away from it, a little accent light around it
    c.img.alpha_composite(treemap(s, DIM, DESAT))
    vg = c.rgrad(cx, cy, 200, (0, 0, 0), (80,) * 3, gamma=1.4).convert("L")
    c.paint((8, 9, 12), vg)
    if P["spill"]:
        c.light(ACCENT, c.blur(c.circle(cx, cy, R + 8), 14), P["spill"])
    # inside the lens: the same treemap magnified around the hotspot, at full brightness
    inner = c.circle(cx, cy, ri)
    c.paint(LENS_FLOOR, inner)
    k = 1 / MAG
    mag = treemap(s, 0.94, 0.0).transform(
        c.img.size, Image.AFFINE,
        (k, 0, c.p(cx) * (1 - k), 0, k, c.p(cy) * (1 - k)), Image.BICUBIC,
    )
    c.img.alpha_composite(Image.composite(mag, Image.new("RGBA", mag.size), inner))
    x0, y0, x1, y1 = (cx + (v - cx) * MAG if i % 2 == 0 else cy + (v - cy) * MAG for i, v in enumerate(HOT))
    hot = c.rrect(x0, y0, x1, y1, TILE_R * MAG)
    c.light(ACCENT, mul(c.blur(hot, 12), inner), 0.35)  # the tile lights up ...
    c.light(ACCENT, mul(sub(c.blur(hot, 10), hot), inner), 0.8)  # ... and its surroundings
    # glass: a soft inner shadow at the rim and a faint sheen along the top-left
    c.paint((6, 7, 10), sub(inner, c.blur(c.circle(cx, cy + 2, ri - 3), 4)), 0.6)
    if P["sheen"]:
        cres = sub(c.circle(cx, cy, ri - 2), c.circle(cx + 7, cy + 9, ri))
        c.paint((255, 255, 255), c.blur(cres, 1.2), P["sheen"])
    # ring and ticks: drop shadow, accent glow, gradient body, specular top-left
    ring = c.ring(cx, cy, R + rw / 2, ri)
    shape = ImageChops.add(ring, ticks(c, cx, cy, R + rw / 2 + P["tick_gap"], P["tick_len"], P["tick_w"]))
    c.paint((0, 0, 0), ImageChops.offset(c.blur(shape, 5), 0, round(c.p(4))), 0.6)
    c.light(ACCENT, c.blur(shape, 9), P["glow"])
    c.paint(c.vgrad(RING_TOP, RING_BOTTOM, cy - R - 20, cy + R + 20), shape)
    spec = c.rgrad(cx - R * 0.7, cy - R * 0.7, R * 0.9, (255, 255, 255), (0, 0, 0)).convert("L")
    c.paint((255, 255, 255), mul(ring, spec), 0.7)
    c.paint((40, 46, 96), c.ring(cx, cy, ri + 1, ri), 0.7)
    c.clip(pm)
    return c.out()


# ---------------------------------------------------------------------------------------------
# 16-32 px: simplified, on each size's own pixel grid

SMALL = {
    # lens: centre x, centre y, outer and inner ring radius; ticks: width, from, to (radius)
    16: dict(
        plate=(0, 3.5), tile_r=1, lens=(8, 8, 5.75, 4), hot=(6, 6, 10, 10), hot_r=0.5, ticks=None,
        tiles=[(1, 1, 5, 8, BLUE), (1, 9, 5, 15, shade(BLUE, 0.8)), (6, 1, 15, 5, TEAL),
               (11, 6, 15, 15, SKY), (6, 11, 10, 15, VIOLET)],
    ),
    24: dict(
        plate=(0, 5), tile_r=1.5, lens=(12, 12, 7.75, 5.75), hot=(9, 9, 15, 15), hot_r=1, ticks=(2, 9, 11),
        tiles=[(2, 2, 8, 14, BLUE), (2, 15, 8, 22, shade(BLUE, 0.8)), (9, 2, 22, 8, TEAL),
               (16, 9, 22, 16, SKY), (16, 17, 22, 22, FILE), (9, 17, 15, 22, VIOLET)],
    ),
    32: dict(
        plate=(1, 7), tile_r=1.5, lens=(16, 16, 9.75, 7.25), hot=(12, 12, 20, 20), hot_r=1.5, ticks=(2, 11, 14),
        tiles=[(3, 3, 11, 18, BLUE), (3, 19, 11, 29, shade(BLUE, 0.8)), (12, 3, 29, 11, TEAL),
               (21, 12, 29, 21, SKY), (21, 22, 29, 29, FILE), (12, 21, 20, 29, VIOLET)],
    ),
}


def render_small(s):
    L = SMALL[s]
    c = Canvas(s, s)
    m, r = L["plate"]
    pm = c.rrect(m, m, s - m, s - m, r)
    c.paint(c.vgrad((46, 50, 63), (16, 17, 22), m, s - m), pm)
    for x0, y0, x1, y1, col in L["tiles"]:
        c.paint(dimmed(col, DIM, DESAT), c.rrect(x0, y0, x1, y1, L["tile_r"]))
    cx, cy, ro, ri = L["lens"]
    c.paint(LENS_FLOOR, c.circle(cx, cy, ri))
    x0, y0, x1, y1 = L["hot"]
    c.paint(c.vgrad((180, 190, 255), (118, 132, 250), y0, y1), c.rrect(x0, y0, x1, y1, L["hot_r"]))
    ring = c.ring(cx, cy, ro, ri)
    if L["ticks"]:
        w, r0, r1 = L["ticks"]
        d = ImageDraw.Draw(ring)
        for box in (
            (cx - w / 2, cy - r1, cx + w / 2, cy - r0), (cx - w / 2, cy + r0, cx + w / 2, cy + r1),
            (cx - r1, cy - w / 2, cx - r0, cy + w / 2), (cx + r0, cy - w / 2, cx + r1, cy + w / 2),
        ):
            d.rectangle([c.p(box[0]), c.p(box[1]), c.p(box[2]) - 1, c.p(box[3]) - 1], fill=255)
    c.paint(c.vgrad(RING_TOP, RING_BOTTOM, cy - ro, cy + ro), ring)
    c.clip(pm)
    return c.out()


def render(s):
    return render_small(s) if s in SMALL else render_full(s)


# ---------------------------------------------------------------------------------------------
# .ico + preview


def bmp_frame(img):
    """32-bit BGRA DIB + 1 bpp AND mask, bottom-up, as stored inside an .ico."""
    w, h = img.size
    px = img.load()
    xor = bytearray()
    for y in range(h - 1, -1, -1):
        for x in range(w):
            r, g, b, a = px[x, y]
            xor += bytes((b, g, r, a))
    row = ((w + 31) // 32) * 4
    andm = bytearray()
    for y in range(h - 1, -1, -1):
        bits = bytearray(row)
        for x in range(w):
            if px[x, y][3] == 0:
                bits[x // 8] |= 0x80 >> (x % 8)
        andm += bits
    header = struct.pack("<IiiHHIIiiII", 40, w, h * 2, 1, 32, 0, len(xor) + len(andm), 0, 0, 0, 0)
    return header + bytes(xor) + bytes(andm)


def png_bytes(img):
    buf = io.BytesIO()
    img.save(buf, format="PNG", optimize=True)
    return buf.getvalue()


def write_ico(path, images):
    frames = [png_bytes(im) if im.size[0] >= 256 else bmp_frame(im) for im in images]
    out = bytearray(struct.pack("<HHH", 0, 1, len(images)))
    offset = 6 + 16 * len(images)
    for im, data in zip(images, frames):
        w, h = im.size
        out += struct.pack("<BBBBHHII", w % 256, h % 256, 0, 0, 1, 32, len(data), offset)
        offset += len(data)
    for data in frames:
        out += data
    Path(path).write_bytes(bytes(out))


def preview(path, images):
    """Each size at 1x on dark and light strips, plus the small ones blown up 8x (nearest)."""
    pad = 16
    width = pad + sum(im.size[0] + pad for im in images)
    zoom = [im for im in images if im.size[0] <= 48]
    zh = max(im.size[1] for im in zoom) * 8
    zw = pad + sum(im.size[0] * 8 + pad for im in zoom)
    width = max(width, zw)
    strip = 256 + 2 * pad
    zstrip = zh + 2 * pad
    sheet = Image.new("RGB", (width, strip * 2 + zstrip * 2), (32, 32, 32))
    sheet.paste((243, 243, 243), (0, strip, width, strip * 2))
    sheet.paste((243, 243, 243), (0, strip * 2 + zstrip, width, strip * 2 + zstrip * 2))
    for y in (pad, strip + pad):
        x = pad
        for im in images:
            sheet.paste(im, (x, y), im)
            x += im.size[0] + pad
    for y in (strip * 2 + pad, strip * 2 + zstrip + pad):
        x = pad
        for im in zoom:
            z = im.resize((im.size[0] * 8, im.size[1] * 8), Image.NEAREST)
            sheet.paste(z, (x, y), z)
            x += z.size[0] + pad
    sheet.save(path)


def main():
    images = [render(s) for s in SIZES]
    write_ico(HERE / "diskscope.ico", images)
    images[-1].save(HERE / "icon_256.png", optimize=True)
    if "--preview" in sys.argv:
        preview(sys.argv[sys.argv.index("--preview") + 1], images)
    print("wrote", HERE / "diskscope.ico", "and", HERE / "icon_256.png")


if __name__ == "__main__":
    main()
