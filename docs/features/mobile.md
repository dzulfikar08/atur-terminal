# Mobile

The Atur Terminal phone apps connect straight to the server on your computer. There is
no cloud service and no account. A paired phone sees the same projects and
terminals as your desktop.

A phone connects in one of two ways:

- **Paired:** over your local network, a VPN such as Tailscale, or a public
  address. The connection is encrypted and the phone trusts only your server.
- **Over SSH:** the phone logs in like `atur-term --host` does. No pairing or open
  port needed. See [Remote servers](remote-servers.md).

## Pair a phone

From the desktop app, open the command palette and choose **Pair a Phone…**.
It opens **Settings → Phones**, turns on **Allow mobile devices** if it is off,
and shows a pairing code:

- a QR code to scan with the Atur Terminal app, and the `atur://pair` link under
  it with **Copy Link**;
- the addresses the phone will try, such as your local network address,
  Tailscale, and the computer's `.local` name;
- a countdown to the end of the five minutes, and **Waiting for the phone…**
  until a phone uses the code, then **Paired as** and the phone's name.

You can also open **Settings → Phones** yourself and choose **Show Pairing
Code**, or use **Pair a Phone…** in a server's status popover.

Each server has phones of its own. For a computer you reach over SSH, choose
**Pair a Phone on <name>…** in the command palette, or its **Pair a Phone…**
under **Remote Servers…**. The code points at that computer's addresses, so the
phone connects to it directly and no longer needs this computer.

From a shell, on any computer with `atur-term`:

```bash
atur-term mobile enable
atur-term mobile pair
```

- A pairing code works once, for five minutes, and only while it is on screen.
- The code lists your computer's addresses. If phones reach it by another name,
  such as a cloud server's public name, put it first:
  `atur-term mobile pair --address devbox.example.com`.
- Phones connect on port `7419` by default. Change it in **Settings → Phones**
  or with `atur-term mobile enable --port <port>`.

## Manage phones

| Task | Desktop | CLI |
| --- | --- | --- |
| See status and phones | **Settings → Phones** | `atur-term mobile` |
| Revoke a phone | **Revoke** next to it | `atur-term mobile revoke <id>` |
| Turn access off | **Allow mobile devices** off | `atur-term mobile disable` |

Revoking a phone, or turning access off, disconnects it at once.

## What a phone can do

A paired phone gets full terminal access: it can use projects, terminals, Git,
and files. It can't manage mobile access, change server settings, stop the
server, or run programs with `exec`.

## Troubleshooting

- On macOS, allow `atur-term-server` if the firewall asks.
- On iOS, the Atur Terminal app needs local network permission.
- The phone and computer must reach each other: same network, a VPN, or an
  address added with `--address`.
