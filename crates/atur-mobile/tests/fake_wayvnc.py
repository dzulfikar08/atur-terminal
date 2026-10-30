#!/usr/bin/env python3
"""A stand-in for wayvnc: an RFB 3.8 server on the unix socket it is given (the last argument).

It is steered by files in $FAKE_WAYVNC_DIR: `colour` (hex RRGGBB) and `size` (WxH) are the desktop,
`units.bin` holds H.264 access units (each with a 4 byte big-endian length in front) and `fail`
makes the program exit as it starts. It appends what clients send to `input.log`: the encodings, a
line for every key and pointer event, full update requests and clipboard text.

A client that asks for H.264 gets the access units, the first one (and the first one again after
every full update request) flagged as a new context; any other client gets ZRLE pictures in the
current colour. Updates the client has not asked for are never sent, and an incremental request
is held until the colour or the size changed, like a real server does. Clipboard text a client
sends comes back as `echo:` and the text.
"""
import os
import socket
import struct
import sys
import threading
import time
import zlib

directory = os.environ["FAKE_WAYVNC_DIR"]
parent = os.getppid()


def path(name):
    return os.path.join(directory, name)


if os.path.exists(path("fail")):
    sys.stderr.write("fake wayvnc: told to fail\n")
    sys.exit(1)

address = sys.argv[-1]
if not address.startswith("unix:"):
    sys.exit("only unix sockets here")
listener = socket.socket(socket.AF_UNIX)
listener.bind(address[len("unix:"):])
listener.listen(4)

units = []
if os.path.exists(path("units.bin")):
    with open(path("units.bin"), "rb") as source:
        data = source.read()
    at = 0
    while at < len(data):
        (length,) = struct.unpack(">I", data[at:at + 4])
        units.append(data[at + 4:at + 4 + length])
        at += 4 + length

log_lock = threading.Lock()


def log(line):
    with log_lock:
        with open(path("input.log"), "a", encoding="utf-8") as out:
            out.write(line + "\n")


def read_exact(connection, count):
    data = b""
    while len(data) < count:
        chunk = connection.recv(count - len(data))
        if not chunk:
            raise EOFError
        data += chunk
    return data


def read_text(name, default):
    try:
        with open(path(name), encoding="utf-8") as source:
            return source.read().strip()
    except OSError:
        return default


def colour():
    text = read_text("colour", "c81e3c")
    return tuple(int(text[i:i + 2], 16) for i in (0, 2, 4))


def size():
    width, height = read_text("size", "320x180").split("x")
    return int(width), int(height)


def listen(connection, state, guard):
    """Reads the client's messages: input is logged, requests are counted."""
    try:
        while True:
            kind = read_exact(connection, 1)[0]
            if kind == 0:  # SetPixelFormat
                read_exact(connection, 19)
            elif kind == 2:  # SetEncodings
                head = read_exact(connection, 3)
                (count,) = struct.unpack(">H", head[1:3])
                encodings = struct.unpack(">%di" % count, read_exact(connection, 4 * count)) if count else ()
                with guard:
                    state["encodings"] = list(encodings)
                log("encodings " + ",".join(str(e) for e in encodings))
            elif kind == 3:  # FramebufferUpdateRequest
                body = read_exact(connection, 9)
                with guard:
                    state["requests"] += 1
                    if body[0] == 0:
                        state["fulls"] += 1
                if body[0] == 0:
                    log("full-update")
            elif kind == 4:  # KeyEvent
                body = read_exact(connection, 7)
                (keysym,) = struct.unpack(">I", body[3:7])
                log("key %d %d" % (body[0], keysym))
            elif kind == 5:  # PointerEvent
                body = read_exact(connection, 5)
                x, y = struct.unpack(">HH", body[1:5])
                log("pointer %d %d %d" % (body[0], x, y))
            elif kind == 6:  # ClientCutText
                head = read_exact(connection, 7)
                (length,) = struct.unpack(">I", head[3:7])
                text = read_exact(connection, length).decode("latin-1")
                log("cut " + text)
                with guard:
                    state["cut"] = text
            elif kind == 150:  # EnableContinuousUpdates
                read_exact(connection, 9)
            else:
                log("unknown message %d" % kind)
                raise EOFError
    except (EOFError, OSError):
        with guard:
            state["done"] = True


def zrle_update(zstream, width, height, rgb, resized):
    """One update with the whole desktop in one colour; a DesktopSize rectangle first on a resize."""
    red, green, blue = rgb
    tiles = ((width + 63) // 64) * ((height + 63) // 64)
    raw = (b"\x01" + bytes([blue, green, red])) * tiles  # solid tiles, CPIXEL is B, G, R
    data = zstream.compress(raw) + zstream.flush(zlib.Z_SYNC_FLUSH)
    rects = []
    if resized:
        rects.append(struct.pack(">HHHHi", 0, 0, width, height, -223))
    rects.append(struct.pack(">HHHHi", 0, 0, width, height, 16) + struct.pack(">I", len(data)) + data)
    return struct.pack(">BxH", 0, len(rects)) + b"".join(rects)


def h264_update(width, height, unit, flags):
    rect = struct.pack(">HHHHi", 0, 0, width, height, 50) + struct.pack(">II", len(unit), flags) + unit
    return struct.pack(">BxH", 0, 1) + rect


def converse(connection):
    width, height = size()
    connection.sendall(b"RFB 003.008\n")
    read_exact(connection, 12)
    connection.sendall(bytes([1, 1]))  # one security type: none
    read_exact(connection, 1)
    connection.sendall(struct.pack(">I", 0))
    read_exact(connection, 1)  # ClientInit
    name = b"fake-desktop"
    pixel_format = bytes([32, 24, 0, 1, 0, 255, 0, 255, 0, 255, 16, 8, 0, 0, 0, 0])
    connection.sendall(struct.pack(">HH", width, height) + pixel_format + struct.pack(">I", len(name)) + name)
    state = {"requests": 0, "fulls": 0, "encodings": [], "cut": None, "done": False}
    guard = threading.Lock()
    threading.Thread(target=listen, args=(connection, state, guard), daemon=True).start()
    zstream = zlib.compressobj()
    served_requests = served_fulls = 0
    sent_colour = None
    sent_size = (width, height)
    next_unit = 0
    while True:
        time.sleep(0.02)
        with guard:
            if state["done"]:
                return
            requests, fulls = state["requests"], state["fulls"]
            encodings = list(state["encodings"])
            cut, state["cut"] = state["cut"], None
        if cut is not None:
            reply = ("echo:" + cut).encode("latin-1", "replace")
            connection.sendall(struct.pack(">B3xI", 3, len(reply)) + reply)
        if requests <= served_requests:
            continue
        full = fulls > served_fulls
        rgb, desktop = colour(), size()
        if 50 in encodings and units:
            if full:
                update = h264_update(desktop[0], desktop[1], units[0], 1)
                next_unit = 1
            elif next_unit < len(units):
                update = h264_update(desktop[0], desktop[1], units[next_unit], 0)
                next_unit += 1
            else:
                continue
        else:
            if not full and rgb == sent_colour and desktop == sent_size:
                continue
            update = zrle_update(zstream, desktop[0], desktop[1], rgb, desktop != sent_size)
            sent_colour, sent_size = rgb, desktop
        served_requests, served_fulls = requests, fulls
        connection.sendall(update)


def serve(connection):
    try:
        converse(connection)
    except (EOFError, OSError):
        pass
    finally:
        connection.close()


def watch():
    """Exits with the server that started it."""
    while os.getppid() == parent:
        time.sleep(0.2)
    os._exit(0)


threading.Thread(target=watch, daemon=True).start()
while True:
    connection, _ = listener.accept()
    threading.Thread(target=serve, args=(connection,), daemon=True).start()
