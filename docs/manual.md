# tuiblick — manual

Short usage + manual test checklist (spec §§9–10). Unit gate
(`cargo fmt --check && cargo clippy -- -D warnings && cargo test`)
covers parsers and state transitions; everything below needs a real
macOS box with `awg-quick`, `openvpn`, and sudo.

## Setup

```sh
cargo build
./target/debug/tuiblick
```

Needs: macOS, Terminal.app / iTerm2 / tmux, at least 80x24.
No shell is ever invoked — backend commands run as argv arrays.

## Config root

`~/.config/tuiblick/` (override for tests only: `TUIBLICK_CONFIG_DIR`):

```text
~/.config/tuiblick/
  awg/<sanitized>.conf
  openvpn/<sanitized>.ovpn
  profiles.yaml
```

`sanitize(name)`: lowercase, `[^a-z0-9-_]` → `-`, collapse dashes,
trim, fallback `profile`, dedupe with `-2`, `-3`. The original
filename is kept verbatim in `profiles.yaml` (`original_filename`).

## Keys

Press `?` in the app for the full table. Essentials:

| Key        | Action                                            |
|------------|---------------------------------------------------|
| `j/k`, ↑↓  | move in list / scroll detail                      |
| `Tab, 1/2/3` | switch pane focus                               |
| `Enter/c`  | connect selected (`c` clears the log *view* when the OpenVPN detail pane is focused) |
| `d`        | disconnect active                                 |
| `r`        | rename selected profile (rescan is automatic)     |
| `i`        | import file (path prompt, `Tab` toggles backend)  |
| `D`        | delete profile (confirm)                          |
| `/`, `f`, `s` | filter logs / follow toggle / save log to `/tmp` |
| `?`        | help popup                                        |
| `q`, Ctrl-C | quit (confirm if a tunnel is active)             |

## Notes you must know

- **Quit leaves the tunnel up.** Quitting with an active tunnel asks
  `active tunnel will stay up after quit. Quit anyway? y/N`.
  Stopping is always explicit (`d`). There is no auto-reconnect,
  no daemon, no launch-at-login in MVP.
- **Sudo password lives in memory only.** First connect that needs
  privilege opens one masked prompt per session (`sudo: root |
  sudo cached | sudo needed` in the header). The password is zeroized
  on quit and on auth failure; it is never written to disk.
  Running the TUI itself as root skips prompts entirely.
- **Header:** `init <initial-ip> (<gateway>/<if>) → exit <current-ip>`.
  `resolving…` = probe in flight, `unknown` = IP probe failed,
  `n/a` = route probe failed. Probes never block the UI.
- **`.ovpn` with a `daemon` directive:** the UI warns (status bar +
  log header) and stays foreground — a daemonized child would break
  log capture and SIGTERM shutdown.
- **Known limitation — unexpected child exit:** if the `openvpn`
  child dies on its own (e.g. `kill -9`), the UI keeps showing the
  last logs until you press `d`. `openvpn.rs` exposes no
  non-consuming liveness poll (needs new API + tests; deferred
  beyond MVP wiring on purpose).
- **Naming note:** spec §8 spells the error field `stderr_tail`;
  the code calls it `tail` because AWG captures *combined*
  stdout+stderr output (parked Task 1 ruling: plan's combined-output
  `tail` governs). Same data, last 2 KB.

## Manual test checklist

1. **Import with weird names.** `i`, import an AWG conf and an
   `.ovpn` whose names contain spaces, parentheses, and unicode
   (e.g. `My Office (1).conf`, `café vpn.ovpn`). Expect: copied
   into place — `awg/awgN.conf` for AWG (short interface-safe names,
   since `awg-quick` derives the interface from the file stem) and
   sanitized names for OpenVPN — `original_filename` kept verbatim,
   both listed with readable display names. (Typing a path ending in
   `.ovpn`/`.conf` flips the backend switch as you type; Tab overrides
   it — whatever the switch shows at Enter wins.)
2. **Connect/disconnect each backend.** `Enter` on the AWG profile →
   `connected`, `awg show` text + handshake/transfer summary in the
   right pane, header `exit` IP updates. `d` → `disconnected`.
   Repeat for OpenVPN → live timestamped logs. Connecting while
   another tunnel is up prompts `switch? y/N`.
3. **Sudo prompt appears once.** Fresh session, non-root, no cached
   password: first connect opens the masked prompt once. Type a
   **wrong** password → re-prompt with an error note (exactly one
   re-prompt); type the right one → connects, no further prompts
   this session, no hang. Passwords with trailing newline/unicode
   must work (single trailing `\n` is piped on stdin, EOF after).
4. **Spaced-path `awg-quick up` works.** Connect the profile imported
   from a path with spaces (step 1). Expect success: the full stored
   path travels as one argv element, never shell-split.
5. **`.ovpn` containing `daemon`.** Connect it. Expect: warning in
   the status bar + log header, child stays foreground-captured
   (logs keep flowing).
6. **`kill -9` the openvpn child.** Find the child pid, `kill -9` it.
   Expected (known limitation): logs freeze, UI stays responsive;
   press `d` → `openvpn exited (code signal)…`. No panic, no hang.
7. **Missing-file badge.** Delete a stored file under
   `~/.config/tuiblick/` by hand and wait a few seconds. Expect the
   entry shows `(missing)` (picked up by the automatic rescan);
   connecting to it gives a clear error, not a panic.
   Files on disk with no mapping auto-register the same way.
7b. **Rename.** Select a profile, press `r`, edit the name, Enter.
   Expect: the list shows the new display name; the stored file is
   untouched. Blank names are rejected with an error.
8. **No network.** Disconnect Wi-Fi (or block `api.ipify.org` +
   `myip.opendns.com`). Expect: `unknown` exit-IP / `n/a` route
   with the reason in the status bar; UI still renders and
   connect/disconnect still works. Reconnect network → next
   connect/disconnect resamples.
9. **tmux + Terminal.app smoke at 80x24.** Run inside tmux and in a
   plain Terminal.app window sized 80x24. Expect: no panic, all
   popups (`?`, sudo, import, confirm, error) render and dismiss
   (`q/?/Enter/Esc`), list + detail + status bar visible.
10. **Different-version `awg show`.** If available, point at a box
    with another AmneziaWG version. Expect: raw text always shown,
    parse never fails the 3 s poll (garbage → `peers: 0`, no error).
