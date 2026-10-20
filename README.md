# tuiblick

> A tiny macOS terminal UI for your AmneziaWG and OpenVPN connections. One binary, no daemons, no GUI — just your tunnels in one place.

You keep VPN configs scattered across downloads folders and connect with copy-pasted terminal commands. tuiblick gives you a single keyboard-driven screen to import those files, bring tunnels up and down, watch OpenVPN logs live, and see your exit IP change.

## Quick start

You need macOS (Terminal.app, iTerm2, or tmux, at least 80×24), Rust stable, and the backends you plan to use (`awg-quick` / `awg`, `openvpn`).

For an AmneziaWG backend with the macOS DNS notification fix, see
[`amneziawg-mac`](https://github.com/mikhalytch/amneziawg-mac).
Install the backends separately and make them available on your `PATH`.

```sh
git clone https://github.com/mikhalytch/tuiblick.git
cd tuiblick
cargo install --path . --locked
tuiblick
```

Press `?` inside the app for the full key table. Everything below is the short version.

## Configuration directory

`XDG_CONFIG_HOME` is the standard base directory for user configuration.
tuiblick stores profiles in `$XDG_CONFIG_HOME/tuiblick/`. If the variable is
unset, empty, or relative, tuiblick uses `~/.config/tuiblick/` on macOS too.
See the [XDG Base Directory Specification](https://specifications.freedesktop.org/basedir/0.8/).

To choose a different base directory for one run:

```sh
XDG_CONFIG_HOME="$HOME/my-config" tuiblick
```

For existing profiles from an earlier build, see the
[migration notes](docs/manual.md#existing-profiles).

## CLI flags

```sh
tuiblick [OPTIONS]
  -C, --connect-all   connect every profile in list order at startup
                      (exactly as if you pressed C: skips busy/missing,
                      stops on the first real failure)
  -h, --help          print help and exit
```

Or via just: `just build` / `just install` (`~/.cargo/bin/tuiblick`).

## What it does

- **Profile list** — import `.conf` / `.ovpn` files (`i`), even with spaces, parentheses, or unicode in the name. Files live under `$XDG_CONFIG_HOME/tuiblick/` (default `~/.config/tuiblick/`, including on macOS) (`awg/`, `openvpn/`, plus a `profiles.yaml` mapping). The list rescans automatically every 5 seconds; entries whose file vanished show as `(missing)` instead of breaking.
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
| `D` | disconnect all tunnels, newest first (stay open) |
| `Del`/`Bksp` | delete profile (confirm) |
| `r` | rename selected profile (rescan is automatic) |
| `i` | import file |
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

## License

[0BSD](LICENSE) (BSD Zero Clause): you may use, modify, and redistribute
tuiblick, including in proprietary software, without a copyleft requirement.
Dependencies and external VPN backends retain their own licenses.
