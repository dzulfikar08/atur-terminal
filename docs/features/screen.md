# Screen

Screen shows the desktop of another computer in a window of its own over the
Atur Terminal window, and lets you use it with your mouse and keyboard. It
works with Linux and Mac servers among the [remote servers](remote-servers.md)
you have added, over the same connection the terminals use.

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

The computer you want to see needs the following. If something is missing, the
Screen view says what. Fix it on that computer and choose **Retry connection**.

### Linux

- Linux with a Wayland session that uses a wlroots-style compositor, such as
  Hyprland or sway. The compositor must support the wlroots screen capture
  protocols.
- [`wayvnc`](https://github.com/any1/wayvnc) 0.8 or newer, installed and on
  the server's `PATH`.
- Atur Terminal 2.2.0-9 or newer on the server.

The Screen view says, for example, that `wayvnc` is not installed or that no
Wayland session is running.

### macOS

- macOS 14 or newer, on Apple silicon or Intel.
- Atur Terminal 2.2.0-16 or newer, installed in Applications.
- Someone logged in on the Mac's own screen. The Mac must be awake and
  unlocked: when the display is asleep or the screen is locked, the picture is
  black or frozen, and when nobody is logged in, sharing cannot start.
- A one-time permission, described below.

Atur Terminal includes a small program for this, **Atur Terminal Screen** (on
the beta channel, **Atur Terminal Beta Screen**), stored inside the app. The
server starts it when the first viewer opens the screen and stops it 15 seconds
after the last viewer leaves. It captures the Mac's main display, with the
pointer, and compresses it as H.264 on the Mac's video hardware. The picture
quality follows the speed of the connection.

#### Allow screen sharing on the Mac (once)

macOS asks you to allow Atur Terminal Screen before it can show or control the
screen. Do this once, at the Mac, or by reaching it with the Mac's own Screen
Sharing:

1. In Terminal on the Mac, run `atur-term screen permission`. Or open Screen
   for that Mac from another computer once. Either way, a prompt appears on
   the Mac: "Atur Terminal Screen would like to record this computer's screen".
2. Click **Open System Settings**. In **Privacy & Security → Screen & System
   Audio Recording**, turn on **Atur Terminal Screen**. If macOS offers **Quit
   & Reopen** or **Later**, either is fine.
3. In **Privacy & Security → Accessibility**, turn on **Atur Terminal
   Screen**. This lets your mouse and keyboard work. Without it the screen is
   shown but is view only, and the name of the Screen view says so.
4. In the Screen view, choose **Retry connection**.

Until the permission is given, the Screen view shows: "Screen recording
permission needed on *Mac name*: open System Settings → Privacy & Security →
Screen & System Audio Recording and turn on Atur Terminal Screen." followed by
what to do next.

The permission is remembered and stays through updates of Atur Terminal builds
that come from a release. macOS may ask you to confirm it again from time to
time, because it reminds you periodically which apps record the screen.

#### What to expect on a Mac

- Only the Mac's main display is shared. You cannot pick another display.
- On a Retina display, the picture has the display's real pixel size, up to
  2560 pixels on the long side. A larger display is scaled down. If the
  display's resolution changes, the view follows it.
- The picture follows what changes on the display: a still screen sends
  nothing. The frame rate goes up to 60 frames per second, and on a good
  connection the delay is a few frames.
- Text on the clipboard is shared both ways. The clipboard shortcuts work as
  described under [Keyboard](#keyboard).

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
- Sharing a Mac, **Ctrl** acts as **Command**, **Alt** as **Option**, and
  **Super** (the Windows key) as **Control**. This makes `Ctrl+C`, `Ctrl+V`,
  `Ctrl+A`, and `Ctrl+T` copy, paste, select all, and open a tab on the Mac. To
  make the keys act as themselves instead (Control as Control, Alt as Option,
  Super as Command), set `ATUR_TERM_SCREEN_KEYS=literal` on the Mac.
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

### Linux

The server starts `wayvnc` when the first viewer opens a screen and stops it 15
seconds after the last viewer leaves. It runs as the same user as the server, on
the Wayland desktop it finds in that user's runtime directory (`WAYLAND_DISPLAY`
when set, otherwise the first `wayland-N` socket). A computer with more than one
screen shows the one `wayvnc` picks first; `wayvnc` chooses it, not Atur Terminal.

With a GPU, `wayvnc` encodes H.264 on it (`-g`); the server tries that first and
falls back to software encoding, which `wayvnc` does as ZRLE, when it cannot.

### macOS

The server starts Atur Terminal Screen when the first viewer opens a screen and
stops it 15 seconds after the last viewer leaves. It shares the Mac's main
display only.

### Settings

These environment variables of the server change what it does:

| Variable | Effect |
| --- | --- |
| `ATUR_TERM_SCREEN=0` | The server does not offer screen sharing at all. Linux and Mac. |
| `ATUR_TERM_SCREEN_GPU=0` | `wayvnc` is not started with GPU video encoding, so it sends ZRLE instead of H.264. Linux only. |
| `ATUR_TERM_WAYVNC` | The path of the `wayvnc` program, when it is not on the `PATH`. Linux only. |
| `ATUR_TERM_SCREEN_FPS` | The highest frame rate. The default is 60. Mac only. |
| `ATUR_TERM_SCREEN_MAX_EDGE` | The longest side of the picture in pixels; a larger display is scaled down to it. The default is 2560. Mac only. |
| `ATUR_TERM_SCREEN_KEYS` | `literal` makes Control, Alt, and Super act as themselves instead of Command, Option, and Control. Mac only. |

## If the picture stops

**Retry connection** starts a new session. If the server's connection is lost,
the view closes and a message tells you; open it again once the server is back.

On a Mac:

- A black or frozen picture usually means the display is asleep or the screen
  is locked. Wake and unlock the Mac. The picture comes back by itself.
- If the permission was turned off later, the message names the setting:
  **Privacy & Security → Screen & System Audio Recording → Atur Terminal
  Screen**. Turn it on and choose **Retry connection**.
- If the view is view only, turn on **Atur Terminal Screen** in **Privacy &
  Security → Accessibility**, then choose **Retry connection**.
