#!/usr/bin/env python3
"""Prints Kitty graphics commands to the terminal, the way an application or
`kitty +kitten icat` does, for the herdr image check (herdr-images-e2e.sh).

  kitty_image.py direct  --id N --cols C --rows R   transmit and place in one command,
                                                    sent in 4096-byte chunks (a=T, m=1)
  kitty_image.py put     --id N --cols C --rows R   transmit (a=t), then place (a=p)
  kitty_image.py unicode --id N --cols C --rows R   transmit, place virtually (U=1) and
                                                    print the placeholder cells
  kitty_image.py delete                             delete every image (a=d, d=A)

The picture is 64 x 32 pixels: the left half red, the right half green.
"""

import argparse
import base64
import struct
import sys
import zlib

WIDTH, HEIGHT = 64, 32
ESC = "\x1b"
# The first diacritics of the Kitty placeholder table: row and column numbers.
DIACRITICS = [
    0x0305, 0x030D, 0x030E, 0x0310, 0x0312, 0x033D, 0x033E, 0x033F,
    0x0346, 0x034A, 0x034B, 0x034C, 0x0350, 0x0351, 0x0352, 0x0357,
]
PLACEHOLDER = "\U0010EEEE"


def png():
    rows = b""
    for _ in range(HEIGHT):
        rows += b"\x00"
        for x in range(WIDTH):
            rows += bytes([255, 0, 128, 255] if x < WIDTH // 2 else [0, 255, 128, 255])

    def chunk(kind, data):
        body = kind + data
        return struct.pack(">I", len(data)) + body + struct.pack(">I", zlib.crc32(body))

    return (
        b"\x89PNG\r\n\x1a\n"
        + chunk(b"IHDR", struct.pack(">IIBBBBB", WIDTH, HEIGHT, 8, 6, 0, 0, 0))
        + chunk(b"IDAT", zlib.compress(rows))
        + chunk(b"IEND", b"")
    )


def apc(control, payload=""):
    return f"{ESC}_G{control};{payload}{ESC}\\" if payload else f"{ESC}_G{control}{ESC}\\"


def transmit(action, ident, extra=""):
    """The picture as a chunked direct transmission: `m=1` on all but the last chunk."""
    data = base64.b64encode(png()).decode()
    pieces = [data[start:start + 4096] for start in range(0, len(data), 4096)]
    out = ""
    for index, piece in enumerate(pieces):
        more = 1 if index + 1 < len(pieces) else 0
        if index == 0:
            out += apc(f"a={action},f=100,i={ident},q=2{extra},m={more}", piece)
        else:
            out += apc(f"m={more},q=2", piece)
    return out


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("mode", choices=["direct", "put", "unicode", "delete"])
    parser.add_argument("--id", type=int, default=1)
    parser.add_argument("--cols", type=int, default=10)
    parser.add_argument("--rows", type=int, default=4)
    args = parser.parse_args()
    cells = f",c={args.cols},r={args.rows}"
    out = ""
    if args.mode == "direct":
        out = transmit("T", args.id, cells)
    elif args.mode == "put":
        out = transmit("t", args.id) + apc(f"a=p,i={args.id},p=1{cells},q=2")
    elif args.mode == "unicode":
        out = transmit("t", args.id) + apc(f"a=p,i={args.id},U=1{cells},q=2")
        out += f"{ESC}[38;5;{args.id}m"
        for row in range(args.rows):
            out += "".join(
                PLACEHOLDER + chr(DIACRITICS[row]) + chr(DIACRITICS[col])
                for col in range(args.cols)
            )
            out += "\n"
        out += f"{ESC}[0m"
    else:
        out = apc("a=d,d=A,q=2")
    sys.stdout.write(out + "\n")
    sys.stdout.flush()


if __name__ == "__main__":
    main()
