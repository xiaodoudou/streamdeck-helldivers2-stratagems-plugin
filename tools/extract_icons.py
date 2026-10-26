"""Find the stratagem icon textures in the game's files that data/icons/ doesn't have yet.

    python tools/extract_icons.py                 # finds the game through Steam's library folders
    python tools/extract_icons.py --data "F:\\SteamLibrary\\steamapps\\common\\Helldivers 2\\data"

The icons of data/icons/ are the game's own textures: 256x256, red marking the parts drawn in the stratagem type's
colour, green the cream parts, blue unused. This reads every texture of that kind in the game's bundles, compares it
with the icons we have, and writes to dist/icons/:

    new/<hash>.png    textures unlike any of ours: new stratagems, and much other art in the same format
    near/<hash>.png   textures close to one of ours but not the same: a redrawn icon, or a new stratagem drawn like
                      an old one (the LAS-44 Nova is the Quasar Cannon's sun and beam over a slimmer rifle)
    sheet.png         both, numbered and painted roughly in the game's colours, to pick the stratagems by eye

To add a stratagem, copy its texture to data/icons/ under the name its "icon" entry in data/stratagems.json gives
(with .png), and add the entry: fetch_wiki.py lists the stratagems the wiki has and the file lacks.

The game keeps its files in "DSAR" archives (bundles.NN.nxa) of LZ4 or stored chunks; bundles.nxa indexes the files
they hold, each a Stingray bundle assembled from fragments. Patch bundles (<hash>.patch_N) sit raw in the data folder.
Nothing here writes to the game's folder. Takes a minute or two. Needs numpy and Pillow.
"""
import argparse
import bisect
import io
import os
import re
import struct
import sys
from pathlib import Path

import numpy as np
from PIL import Image, ImageDraw

ROOT = Path(__file__).resolve().parent.parent
ICONS = ROOT / "data" / "icons"
OUT = ROOT / "dist" / "icons"
TEXTURE = 0xCD4238C6A0C69E32  # murmur64a("texture"), the type of a texture in a bundle
BUNDLE = 0xF0000011
NEAR, SAME = 6.0, 0.8  # mean channel difference: up to SAME is one of ours, up to NEAR is close to one


def lz4_block(src: bytes, size: int) -> bytes:
    """Decompress one LZ4 block (no frame), which is what DSAR's type 3 chunks hold."""
    dst = bytearray()
    i, n = 0, len(src)
    while i < n:
        token = src[i]
        i += 1
        lit = token >> 4
        if lit == 15:
            while True:
                b = src[i]
                i += 1
                lit += b
                if b != 255:
                    break
        dst += src[i:i + lit]
        i += lit
        if i >= n:
            break
        off = src[i] | (src[i + 1] << 8)
        i += 2
        ml = token & 15
        if ml == 15:
            while True:
                b = src[i]
                i += 1
                ml += b
                if b != 255:
                    break
        ml += 4
        start = len(dst) - off
        if off == 0 or start < 0:
            raise ValueError("bad LZ4 match offset")
        if off >= ml:
            dst += dst[start:start + ml]
        else:  # the match overlaps what it writes
            for k in range(ml):
                dst.append(dst[start + k])
    if len(dst) != size:
        raise ValueError(f"LZ4 block is {len(dst)} bytes, expected {size}")
    return bytes(dst)


class Dsar:
    """An archive of chunks, each LZ4-compressed (type 3) or stored (type 0), read as one byte stream."""

    def __init__(self, path: Path):
        self.f = open(path, "rb")
        head = self.f.read(0x20)
        if head[:4] != b"DSAR":
            raise ValueError(f"{path}: not a DSAR archive")
        count, = struct.unpack_from("<I", head, 8)
        self.total, = struct.unpack_from("<Q", head, 16)
        raw = self.f.read(count * 32)
        self.chunks = [struct.unpack_from("<QQIIB", raw, 32 * k) for k in range(count)]
        self.starts = [c[0] for c in self.chunks]
        self.cache: dict[int, bytes] = {}

    def chunk(self, k: int) -> bytes:
        if k not in self.cache:
            _, offset, size, packed, kind = self.chunks[k]
            self.f.seek(offset)
            data = self.f.read(packed)
            if kind == 3:
                data = lz4_block(data, size)
            elif kind != 0:
                raise ValueError(f"chunk type {kind} unknown")
            if len(self.cache) > 64:
                self.cache.clear()
            self.cache[k] = data[:size]
        return self.cache[k]

    def read(self, offset: int, size: int) -> bytes:
        out = bytearray()
        while size > 0:
            k = bisect.bisect_right(self.starts, offset) - 1
            start, _, length, _, _ = self.chunks[k]
            take = min(size, length - (offset - start))
            out += self.chunk(k)[offset - start:offset - start + take]
            offset += take
            size -= take
        return bytes(out)


class Files:
    """The files bundles.nxa lists, each made of fragments spread over the bundles.NN.nxa archives."""

    def __init__(self, data: Path):
        self.data = data
        index = Dsar(data / "bundles.nxa")
        idx = index.read(0, index.total)
        count, = struct.unpack_from("<I", idx, 16)  # after the magic, two versions, the total size and the archive count
        self.files: dict[str, tuple[int, list[tuple[int, int, int]]]] = {}
        for k in range(count):
            size, name_at, nfrag, frag_at, _ = struct.unpack_from("<QIIII", idx, 0x18 + 24 * k)
            name = idx[name_at:idx.index(b"\0", name_at)].decode()
            frags = []
            for j in range(nfrag):
                at, where = struct.unpack_from("<QQ", idx, frag_at + 16 * j)
                frags.append((at, where & ((1 << 56) - 1), where >> 56))  # file offset, archive offset, archive
            self.files[name] = (size, sorted(frags))
        self.archives: dict[int, Dsar] = {}

    def read(self, name: str, offset: int, size: int) -> bytes:
        total, frags = self.files[name]
        size = min(size, total - offset)
        starts = [f[0] for f in frags]
        out = bytearray()
        while size > 0:
            i = bisect.bisect_right(starts, offset) - 1
            at, where, n = frags[i]
            end = frags[i + 1][0] if i + 1 < len(frags) else total
            take = min(size, end - offset)
            if n not in self.archives:
                self.archives[n] = Dsar(self.data / f"bundles.{n:02d}.nxa")
            out += self.archives[n].read(where + offset - at, take)
            offset += take
            size -= take
        return bytes(out)


def textures(read) -> list[tuple[int, int, int, int, int, int, int]]:
    """The textures a bundle lists: (name hash, main offset, stream offset, gpu offset, main, stream, gpu sizes)."""
    magic, ntypes, nfiles = struct.unpack_from("<III", read(0, 12), 0)
    if magic != BUNDLE:
        return []
    body = read(0x48, ntypes * 32 + nfiles * 80)
    out = []
    for k in range(nfiles):
        e = struct.unpack_from("<QQQQQQQIII", body, ntypes * 32 + 80 * k)
        if e[1] == TEXTURE:
            out.append((e[0], e[2], e[3], e[4], e[7], e[8], e[9]))
    return out


def is_icon_size(main: bytes) -> bool:
    """Whether the texture is 256x256, from its DDS header, before its pixels are read (most of the time it costs)."""
    dds = main[0xC0:]
    return dds[:4] == b"DDS " and struct.unpack_from("<II", dds, 12) == (256, 256)


def icon(main: bytes, pixels: bytes):
    """The texture as RGB, if it is drawn like a stratagem icon: red and green parts, no blue."""
    dds = main[0xC0:]
    head = 4 + 124 + (20 if dds[84:88] == b"DX10" else 0)
    try:
        img = Image.open(io.BytesIO(dds[:head] + pixels))
        img.load()
    except Exception:  # a format Pillow can't decode is no icon of ours
        return None
    a = np.asarray(img.convert("RGB"))
    ok = (a[:, :, 0] > 128).any() and (a[:, :, 1] > 128).any() and a[:, :, 2].max() <= 40
    return a if ok else None


def game_data() -> Path:
    """The game's data folder, from the library folders Steam lists."""
    for steam in (Path(os.environ.get("ProgramFiles(x86)", r"C:\Program Files (x86)")) / "Steam", Path(r"C:\Program Files\Steam")):
        vdf = steam / "steamapps" / "libraryfolders.vdf"
        if vdf.exists():
            for lib in re.findall(r'"path"\s+"([^"]+)"', vdf.read_text(encoding="utf-8", errors="replace")):
                data = Path(lib.replace("\\\\", "\\")) / "steamapps" / "common" / "Helldivers 2" / "data"
                if (data / "bundles.nxa").exists():
                    return data
    sys.exit("Helldivers 2 not found in Steam's library folders: give its data folder with --data")


def painted(a, size: int = 128) -> Image.Image:
    """The texture roughly as the game draws it: red parts in the offensive red, green parts cream."""
    r, g = a[:, :, 0] / 255.0, a[:, :, 1] / 255.0
    rgb = np.zeros(a.shape, float)
    for c, (red, cream) in enumerate(zip((200, 60, 50), (235, 225, 200))):
        rgb[:, :, c] = 30 * (1 - np.maximum(r, g)) + red * r * (1 - g) + cream * g
    return Image.fromarray(rgb.clip(0, 255).astype(np.uint8)).resize((size, size))


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    ap.add_argument("--data", type=Path, help="the game's data folder (found through Steam if not given)")
    data = ap.parse_args().data or game_data()
    names = sorted(p.name for p in ICONS.glob("*.png"))
    ours = [np.asarray(Image.open(ICONS / n).convert("RGB")).astype(np.int16) for n in names]

    found: dict[int, tuple] = {}
    files = Files(data)
    bundles = [n for n in files.files if "." not in n]
    print(f"{data}: {len(bundles)} bundles")
    for b in bundles:
        for h, main_at, stream_at, gpu_at, main_size, stream_size, gpu_size in textures(lambda o, s: files.read(b, o, s)):
            if h in found:
                continue
            main_ = files.read(b, main_at, main_size)
            if not is_icon_size(main_):
                continue
            try:
                pixels = files.read(b + ".stream", stream_at, stream_size) if stream_size else files.read(b + ".gpu_resources", gpu_at, gpu_size)
            except KeyError:
                continue
            a = icon(main_, pixels)
            if a is not None:
                found[h] = a
    for p in sorted(data.glob("*.patch_*")):
        if not p.suffix[len(".patch_"):].isdigit():
            continue
        blob = p.read_bytes()
        side = {s: (p.with_name(p.name + s).read_bytes() if p.with_name(p.name + s).exists() else b"") for s in (".stream", ".gpu_resources")}
        for h, main_at, stream_at, gpu_at, main_size, stream_size, gpu_size in textures(lambda o, s: blob[o:o + s]):
            main_ = blob[main_at:main_at + main_size]
            if not is_icon_size(main_):
                continue
            pixels = side[".stream"][stream_at:stream_at + stream_size] if stream_size else side[".gpu_resources"][gpu_at:gpu_at + gpu_size]
            a = icon(main_, pixels)
            if a is not None:
                found[h] = a  # a patch replaces the bundle's texture

    picked = []  # (kind, hash, texture, closest of ours, difference)
    for h, a in found.items():
        diffs = [np.abs(a.astype(np.int16) - o).mean() for o in ours]
        k = int(np.argmin(diffs))
        if diffs[k] > SAME:
            picked.append(("near" if diffs[k] <= NEAR else "new", h, a, names[k], diffs[k]))
    picked.sort(key=lambda p: (p[0] != "near", p[1]))
    for kind in ("new", "near"):
        (OUT / kind).mkdir(parents=True, exist_ok=True)
        for old in (OUT / kind).glob("*.png"):
            old.unlink()
    cols = 8
    sheet = Image.new("RGB", (cols * 136, max(1, -(-len(picked) // cols)) * 150), (30, 30, 30))
    draw = ImageDraw.Draw(sheet)
    for i, (kind, h, a, closest, diff) in enumerate(picked):
        Image.fromarray(a).save(OUT / kind / f"{h:016x}.png")
        x, y = (i % cols) * 136 + 4, (i // cols) * 150 + 4
        sheet.paste(painted(a), (x, y))
        draw.text((x, y + 130), f"{i} {kind} {h:016x}"[:22], fill=(220, 220, 220))
        print(f"{i:3} {kind:4} {h:016x}  " + (f"close to {closest} ({diff:.1f})" if kind == "near" else f"difference {diff:.1f}"))
    sheet.save(OUT / "sheet.png")
    print(f"{len(found)} icon-like textures, {len(found) - len(picked)} of them ours; "
          f"{sum(p[0] == 'near' for p in picked)} near, {sum(p[0] == 'new' for p in picked)} new, in {OUT}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
