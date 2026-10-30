# Screen

Screen shows the desktop of another computer in a window of its own over the
Atur Terminal window, and lets you use it with your mouse and keyboard. It
works with the [remote servers](remote-servers.md) you have added, over the
same connection the terminals use.

Nothing about a Screen view is saved. Closing it, or losing the connection to
the server, ends it.

## Open it

- Open the command palette and choose **Screen: server name**.
- Or open the server's status in the status bar and choose **View screen**.
- Or choose **Remote Servers…** in the command palette, pick the server, and
  choose **View Screen**.

These appear only for a server on another computer that is connected and can
share its screen. This computer's own screen is never offered.

## Requirements on the server

- Linux with a Wayland session that uses a wlroots-style compositor, such as
  Hyprland or sway. The compositor must support the wlroots screen capture
  protocols.
- [`wayvnc`](https://github.com/any1/wayvnc) 0.8 or newer, installed and on
  the server's `PATH`.
- Atur Terminal 2.2.0-9 or newer on the server.

If something is missing, the Screen view says what, for example that `wayvnc`
is not installed or that no Wayland session is running. Fix it on the server
and choose **Retry connection**.

## Video decoding

The picture is compressed video, decoded on your computer:

| Your computer | Decoder |
| --- | --- |
| macOS | The system's hardware video decoder. |
| Linux | `ffmpeg` with VA-API when your computer has it, otherwise `ffmpeg` in software. |

Install `ffmpeg` on a Linux desktop to use it. Without `ffmpeg`, the Screen view
still works: it falls back to a lossless image format that needs much more
bandwidth, so it can be slow over the internet.

The toolbar shows what is in use, such as `h264 (videotoolbox)`, with the
frame rate and the data rate.

## Security

The screen sharing program on the server listens only on the server's loopback
address or a private socket that only your user can use. It is never open to
the network. Its traffic travels inside the existing connection to the server,
which is protected by SSH, or by the encrypted UDP connection if you use that
transport.

## Using the view

The toolbar at the top of the view has:

- **Fit** or **1:1**. Fit scales the picture to the window and keeps its
  proportions. 1:1 shows one picture pixel for each pixel of your screen; if
  the picture is larger than the window, it slides as you move the pointer.
- **View only**. When on, nothing you do is sent to the server. It is off by
  default.
- The codec, frame rate, and data rate.
- **Close**.

Your own mouse pointer is hidden over the picture. The server's pointer is part
of the picture.

### Keyboard

While the view has the focus, every key goes to the remote computer, including
keys Atur Terminal uses itself, such as `Cmd+T`, `Ctrl+Tab`, and `Escape`.

- On a Mac, **Command** acts as **Control** on the server, so `Cmd+C`, `Cmd+V`,
  and `Cmd+A` copy, paste, and select all there. **Control** also acts as
  Control, and **Option** acts as Alt.
- On Linux, Control, Alt, and Super act as themselves.
- Keys and mouse buttons still held when the view loses the focus, or when you
  switch to another app, are released on the server.

One chord stays with Atur Terminal: **Ctrl+Alt+G** (**Control+Option+G** on a
Mac) shows or hides the toolbar. Hover the toolbar to see this hint. Use the
toolbar's **Close** button to leave the view.

### Clipboard

Plain text is shared both ways:

- When the view gets the focus, your clipboard text is sent to the server if
  it changed.
- Text copied on the server is put on your clipboard.
- Empty text and text over 1 MiB are not shared.

With **View only** on, your clipboard is not sent to the server.

## On the server

The server starts `wayvnc` when the first viewer opens a screen and stops it 15
seconds after the last viewer leaves. It runs as the same user as the server, on
the Wayland desktop it finds in that user's runtime directory (`WAYLAND_DISPLAY`
when set, otherwise the first `wayland-N` socket). A computer with more than one
screen shows the one `wayvnc` picks first; `wayvnc` chooses it, not Atur Terminal.

These environment variables of the server change what it does:

| Variable | Effect |
| --- | --- |
| `ATUR_TERM_SCREEN=0` | The server does not offer screen sharing at all. |
| `ATUR_TERM_SCREEN_GPU=0` | `wayvnc` is not started with GPU video encoding, so it sends ZRLE instead of H.264. |
| `ATUR_TERM_WAYVNC` | The path of the `wayvnc` program, when it is not on the `PATH`. |

With a GPU, `wayvnc` encodes H.264 on it (`-g`); the server tries that first and
falls back to software encoding, which `wayvnc` does as ZRLE, when it cannot.

## If the picture stops

**Retry connection** starts a new session. If the server's connection is lost,
the view closes and a message tells you; open it again once the server is back.
