# tuiblick

> A tiny macOS terminal UI for your AmneziaWG and OpenVPN connections. One binary, no daemons, no GUI — just your tunnels in one place.

You keep VPN configs scattered across downloads folders and connect with copy-pasted terminal commands. tuiblick gives you a single keyboard-driven screen to import those files, bring tunnels up and down, watch OpenVPN logs live, and see your exit IP change.

## Quick start

You need macOS (Terminal.app, iTerm2, or tmux, at least 80×24), Rust stable, and the backends you plan to use (`awg-quick` / `awg`, `openvpn`).

```sh
cargo build
./target/debug/tuiblick
```

Press `?` inside the app for the full key table. Everything below is the short version.

## What it does

- **Profile list** — import `.conf` / `.ovpn` files (`i`), even with spaces, parentheses, or unicode in the name. Files live under `~/.config/tuiblick/` (`awg/`, `openvpn/`, plus a `profiles.yaml` mapping). Rescan with `r`; entries whose file vanished show as `(missing)` instead of breaking.
- **One tunnel per backend** — one AWG and one OpenVPN tunnel may be up
  together. `Enter` connects, `d` disconnects the selected profile's
  tunnel. Connecting while the same backend is up asks you to switch
  first. Quitting never kills your tunnels (it asks, then leaves them
  up — stopping is always explicit).
- **AmneziaWG status** — the right pane shows `awg show` output plus a summary: peer count, latest handshake, transfer rx/tx, refreshed every 3 s while connected.
- **OpenVPN logs, live** — timestamped log view with follow toggle (`f`), scrolling (`j/k`, PgUp/PgDn), substring filter (`/`), view clear (`c`), and save to `/tmp` (`s`). If your `.ovpn` contains a `daemon` directive, tuiblick warns you and stays in the foreground so logs keep flowing.
- **Exit-IP header** — always visible: `init <ip> (<gateway>/<iface>) → exit <ip>`, plus a per-tunnel last exit IP. Probes run in the background and never block the UI; when offline you get `unknown` / `n/a`, not a hang.
- **Sudo, asked once** — the first connect that needs privilege opens one masked password prompt per session. The password lives in memory only and is zeroized on quit or auth failure. Run the whole TUI as root and it never prompts at all.

## Shortcuts

| Key | Action |
|-----|--------|
| `j/k`, ↑↓ | move in list / scroll detail |
| `Ctrl+j/k` | move selected profile down/up (order persists) |
| `Tab`, `1/2/3` | switch pane focus |
| `Enter` / `c` | connect selected (`c` clears the log *view* when the OpenVPN pane is focused) |
| `C` | connect all in list order (skips busy/missing, stops on error) |
| `d` | disconnect selected profile's tunnel |
| `r` | rename selected profile (rescan is automatic) |
| `i` / `D` | import file / delete profile (confirm) |
| `/`, `f`, `s` | filter logs / follow toggle / save log to `/tmp` |
| `?` | help popup |
| `q`, Ctrl-C | quit — tunnels left running (asks first if any is up) |
| `Q` | disconnect every tunnel, newest first, then quit |
| `y/N`, Esc | answer / dismiss popups (Enter means No — the safe choice) |

## What it does NOT do

Deliberately, in this version:

- No daemon, background service, auto-reconnect, or launch-at-login
- No persistent connection state — everything runtime-only, nothing but the file mapping touches disk
- No traffic graphs, DNS-leak tests, or connection analytics
- No inline config editor
- No password storage anywhere except process memory (never on disk)

## Docs

- [`docs/manual.md`](docs/manual.md) — setup, config layout, and the 10-step manual test checklist
- [`docs/spec-mvp.md`](docs/spec-mvp.md) — the full MVP spec this was built against
