#!/usr/bin/env python3
"""Losslessly recompress referenced pet PNGs; --write applies smaller files.

Retain all non-IDAT chunks verbatim, including palettes and color profiles.
Reject any candidate whose decoded RGBA pixels differ. Requires Pillow, like
gen-tray-icons.py. Without --write this only reports the available savings.
"""
import argparse
import io
import json
import struct
import zlib
from pathlib import Path

from PIL import Image

PETS = Path(__file__).resolve().parent.parent / "apps/tokenme-bar/src/assets/pets"
SIGNATURE = b"\x89PNG\r\n\x1a\n"


def chunks(data):
    if not data.startswith(SIGNATURE):
        raise ValueError("not a PNG")
    result, offset = [], len(SIGNATURE)
    while offset < len(data):
        length = struct.unpack_from(">I", data, offset)[0]
        end = offset + length + 12
        if end > len(data):
            raise ValueError("truncated PNG chunk")
        kind, payload = data[offset + 4:offset + 8], data[offset + 8:end - 4]
        crc = struct.unpack_from(">I", data, end - 4)[0]
        if zlib.crc32(kind + payload) != crc:
            raise ValueError("PNG chunk CRC mismatch")
        result.append((kind, payload, data[offset:end]))
        offset = end
    if not result or result[0][0] != b"IHDR" or result[-1][0] != b"IEND":
        raise ValueError("incomplete PNG")
    if any(kind == b"acTL" for kind, _, _ in result):
        raise ValueError("animated PNG needs its own optimizer")
    return result


def idat(parts):
    return b"".join(payload for kind, payload, _ in parts if kind == b"IDAT")


def replace_idat(parts, compressed):
    chunk = struct.pack(">I", len(compressed)) + b"IDAT" + compressed
    chunk += struct.pack(">I", zlib.crc32(b"IDAT" + compressed))
    output, inserted = [SIGNATURE], False
    for kind, _, original in parts:
        if kind != b"IDAT":
            output.append(original)
        elif not inserted:
            output.append(chunk)
            inserted = True
    if not inserted:
        raise ValueError("PNG has no image data")
    return b"".join(output)


def optimized(data):
    parts = chunks(data)
    raw = zlib.decompress(idat(parts))
    candidates = [data, replace_idat(parts, zlib.compress(raw, 9))]
    with Image.open(io.BytesIO(data)) as image:
        pixels, size = image.convert("RGBA").tobytes(), image.size
        encoded = io.BytesIO()
        image.save(encoded, format="PNG", optimize=True, compress_level=9)
    filtered = chunks(encoded.getvalue())
    # Pillow may choose a different palette bit depth. Its filter stream is
    # reusable only when the original header and palette remain unchanged.
    headers = lambda values: [(kind, payload) for kind, payload, _ in values if kind in (b"IHDR", b"PLTE")]
    if headers(parts) == headers(filtered):
        candidates.append(replace_idat(parts, idat(filtered)))
    best = min(candidates, key=len)
    with Image.open(io.BytesIO(best)) as image:
        if image.size != size or image.convert("RGBA").tobytes() != pixels:
            raise ValueError("candidate changed the image's pixels")
    if [(kind, chunk) for kind, _, chunk in chunks(best) if kind != b"IDAT"] != [
        (kind, chunk) for kind, _, chunk in parts if kind != b"IDAT"
    ]:
        raise ValueError("candidate changed PNG metadata")
    return best


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--write", action="store_true")
    args = parser.parse_args()
    manifest = json.loads((PETS / "pet-skins.json").read_text(encoding="utf-8"))
    files = sorted({file for skin in manifest["skins"].values()
        if skin.get("render") != "model" for file in skin["assets"].values()})
    before = after = 0
    for file in files:
        path = (PETS / file).resolve()
        if not path.is_relative_to(PETS.resolve()):
            raise ValueError("asset path escaped the pets directory")
        original = path.read_bytes()
        smaller = optimized(original)
        before += len(original)
        after += len(smaller)
        if args.write and len(smaller) < len(original):
            path.write_bytes(smaller)
        print(f"{file}: {len(original):,} -> {len(smaller):,} bytes", flush=True)
    action = "saved" if args.write else "available"
    print(f"{len(files)} PNGs: {before / 2**20:.2f} -> {after / 2**20:.2f} MiB; "
        f"{(before - after) / 2**20:.2f} MiB {action}; pixels and metadata unchanged")


if __name__ == "__main__":
    main()
