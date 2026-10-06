# tuiblick — MVP Spec

## 0. Overview

`tuiblick` is a small native macOS terminal UI (Rust stable) for managing
AmneziaWG (`awg-quick` / `awg`) and OpenVPN (`openvpn`) connections.

- No Vortix, Tunnelblick, standard WireGuard tools, GUI, daemon,
  persistent connection state, profiles DB beyond import mapping,
  auto-reconnect, or launch-at-login in MVP.
- Must work in Terminal.app, iTerm2, tmux. 80x24 minimum.
- App name is `tuiblick` (placeholder `APP_NAME` is gone).

## 1. Crate layout

```text
tuiblick/
  Cargo.toml
  src/
    main.rs      — terminal setup/teardown, tokio main, App loop
    app.rs       — App state machine, screens, key handling
    config.rs    — paths, scan, profiles.yaml mapping, import/delete
    sudo.rs      — SudoRunner (euid check, in-memory password, sudo -S)
    awg.rs       — AwgManager: up/down/show
    openvpn.rs   — OpenVpnManager: spawn/kill, log ring
    net.rs       — default route + public/exit IP probing
    ui.rs        — ratatui rendering
  docs/
    spec-mvp.md  — this file
    manual.md    — short usage (written at end of MVP)
```

Dependencies (current stable, no extras without reason):

- `ratatui`, `crossterm`, `tokio` (process + time + fs), `serde` + `serde_yaml`,
  `thiserror` (+ `anyhow` only in `main`), `dirs`, `chrono`,
  `libc` (for `geteuid`), `zeroize` (sudo password in memory only),
  `uuid` (v4 for `profiles.yaml` ids).
- `nix` only if signal handling needs it; prefer `tokio::process::Child::kill`
  + `signal::kill` fallback. No `shlex` (argv arrays, no shell).

Each source file lands as its own reviewable git commit.

## 2. Configuration directory

```text
~/.config/tuiblick/
  awg/<sanitized>.conf
  openvpn/<sanitized>.ovpn
  profiles.yaml
```

`profiles.yaml` (list):

```yaml
- id: ulid-or-uuid-string
  display_name: "my-office"        # UI name, defaults to original file stem
  vpn_type: awg | openvpn
  stored_file: "awg/my-office.conf"  # relative to ~/.config/tuiblick
  original_filename: "My Office (1).conf"  # arbitrary user filename, kept verbatim
  created_at: "2026-10-06T12:00:00Z"
```

Rules:

- `sanitize(name)`: lowercase, `[^a-z0-9-_]` → `-`, collapse dashes,
  trim, fallback `profile`, dedupe with `-2`, `-3` suffix. Keeps
  `awg-quick` interface-name happy while preserving the original name
  in the mapping.
- Scan `awg/*.conf` + `openvpn/*.ovpn|*.conf` on start and automatically
  every few seconds (there is no manual rescan key).
  Entries in `profiles.yaml` pointing at missing files show as
  `(missing)`; files on disk with no mapping get auto-registered
  (display name = file stem).
- Import (`i`): TUI input for source path (absolute or `~`-expanded),
  validate exists + readable, copy into the right subdir under the
  sanitized name, append mapping. No move/delete of the source.
- Delete (`D` + confirm): remove stored file + mapping entry.

## 3. Privilege model (`sudo.rs`)

- If `euid == 0` (TUI itself run under sudo): run all backend commands
  directly, never prompt.
- Else `SudoRunner` holds `Option<String>` password in memory only
  (no disk, `zeroize` on quit / on auth failure).
- First connect that needs sudo opens a masked password prompt.
  Commands run as `sudo -S -p <sentinel> <argv...>`, password piped
  on stdin followed by `\n`. On `Sorry, try again` / exit 1 with
  auth sentinel → drop stored password, re-prompt once.
- Status bar shows `root | sudo cached | sudo needed`.
- Probe at startup: `sudo -n true` → cached vs needed (non-blocking).
- `awg-quick`, `awg`, and `openvpn` always go through `SudoRunner`
  (both need tun/tap + routes on macOS).

## 4. AmneziaWG (`awg.rs`)

- Up: `awg-quick up <full-stored-path>` (e.g.
  `~/.config/tuiblick/awg/my-office.conf`). Full path is required:
  bare `up <iface>` only searches `/etc/amnezia/...` and
  `/usr/local/etc/...`. Interface name is derived from the file stem.
- Down: `awg-quick down <full-stored-path>` (same path form; works
  for both CONFIG_FILE and INTERFACE forms of `awg-quick`).
- Both short-lived via `tokio::process::Command`, combined output
  captured, last output kept for the status pane, errors surfaced.
- Status (only for the active AWG profile, every 3 s):
  `awg show <iface>` (raw text shown) + best-effort parse of
  `latest handshake`, `transfer rx/tx`, peer count. Parse failures
  never fail the poll — raw text is the source of truth.
- At most one tunnel per backend in MVP (one AWG + one OpenVPN may be up
  together). Connecting while the same backend is up prompts `switch? y/N`.

## 5. OpenVPN (`openvpn.rs`)

- Up: `openvpn --config <stored .ovpn> --verb 3` as long-lived
  `tokio::process::Child`, stdout+stderr piped + merged. Always
  foreground: MVP never passes `--daemon` and warns (status bar +
  log header) if the user's `.ovpn` contains a `daemon` directive,
  since a daemonized child breaks log capture and SIGTERM shutdown.
- Reader task pushes timestamped lines into
  `VecDeque<String>` capped at 2000 lines.
- Down: SIGTERM, 3 s grace, then SIGKILL. Record exit code + time.
- Log pane controls: follow toggle, `j/k`, PgUp/PgDn, `/` substring
  filter, `c` clear view (not the backing buffer), `s` save raw
  buffer to `/tmp/tuiblick-openvpn-<ts>.log`.

## 6. Network / exit-IP (`net.rs`)

Every tunnel changes routes, so the UI shows what changed.

Captured values:

- `initial`: default route + public IP sampled once at startup
  (before any connect pressed), kept for the whole session.
- `current`: re-sampled after each connect/disconnect completes.
- `per-tunnel last exit IP`: after a tunnel reaches `connected`,
  sample once and store on the profile runtime state
  (`last_exit_ip: Option<String>`); shown in that tunnel's panel.
  Never persisted to disk in MVP.

How (macOS, no heavy deps):

- Default route: `route -n get default` → parse `gateway:` +
  `interface:`. Fallback: `route -n get 1.1.1.1`. Failure → `n/a`,
  never blocks.
- Public/exit IP: `curl -s --max-time 5 https://api.ipify.org`
  via `tokio::process` (no reqwest/TLS in tree). Fallback:
  `dig +short +time=3 +tries=1 myip.opendns.com @208.67.222.222`.
  Timeout total ≤ 8 s, runs in background task, UI shows
  `resolving…` meanwhile. Failure → `unknown`, with reason in
  status bar. No IP fetch ever blocks input rendering.

UI placement:

- Header bar: `init <initial-ip> (<gateway>/<if>) → exit <current-ip>`.
- AWG status panel: `last exit: <ip|—>` + handshake/transfer.
- OpenVPN panel header: `last exit: <ip|—>` above the log view.

## 7. TUI (`app.rs` + `ui.rs`)

Layout: left profile list, right detail (status for AWG / logs for
OpenVPN), bottom status bar. `1/2/3` or Tab switches right-pane
focus; works without mouse.

Keys (also listed under `?`):

| Key       | Action                              |
|-----------|-------------------------------------|
| `j/k`,↑↓  | move in list / scroll logs          |
| `Enter/c` | connect selected                    |
| `d`       | disconnect selected profile's tunnel |
| `r`       | rename selected profile (rescan is automatic) |
| `i`       | import file (path prompt)           |
| `D`       | delete profile (confirm)            |
| `/`       | filter logs (openvpn)               |
| `f`       | toggle log follow                   |
| `s`       | save openvpn log to /tmp            |
| `?`       | help popup                          |
| `q`,Ctrl-C| disconnect? no — just quit (confirm if active) |

States: `Profiles | Connecting | Connected | Disconnecting | ErrorPopup |
HelpPopup | SudoPrompt | ImportPrompt | RenamePrompt | ConfirmPopup`. All backend
work is async; the render loop never awaits a child.

Quit with an active tunnel: confirm popup
`active tunnel will stay up after quit. Quit anyway? y/N`
(MVP deliberately leaves the tunnel up; `down` is explicit.)

## 8. Errors

`thiserror` enum: `Io | Config(msg) | SudoAuth | Process { cmd, code,
stderr_tail } | NetProbe(msg) | User(msg)`. Status bar shows one
line; popup shows full chain. `anyhow` only at `main` boundary.

## 9. Testing

- `cargo fmt --check`, `cargo clippy -- -D warnings`, `cargo test`.
- Unit tests (no root, no real sudo/vpn):
  filename sanitize + dedupe, mapping load/save round-trip,
  log ring cap + filter, sudo argv construction, `route` output
  parser, ipify/dig output trimmer.
- Manual checklist (in `docs/manual.md`): import awg + ovpn with
  weird names, connect/disconnect each, sudo prompt once, kill -9
  openvpn path, missing-file badge, tmux + Terminal.app smoke.

## 10. Non-goals (MVP)

No daemon, no reconnect, no launch-at-login, no persistent
connected-state, no traffic graphs, no DNS-leak tests, no
inline config editor, no password storage on disk.
