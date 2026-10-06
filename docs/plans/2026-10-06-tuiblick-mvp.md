# tuiblick MVP Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build the tuiblick MVP TUI per spec — one binary managing AWG + OpenVPN with import, logs, status, exit-IP.

**Architecture:** Single Rust binary; `SudoRunner` fronts all privileged calls; `AwgManager` = short-lived `awg-quick` calls, `OpenVpnManager` = long-lived tokio child + 2000-line ring; `net` probes in background tasks; `app` state machine + `ui` rendering.

**Tech Stack:** Rust stable, ratatui + crossterm, tokio (full: process/time/fs/macros), serde + serde_yaml, thiserror (+ anyhow in main only), dirs, chrono, libc, zeroize, uuid.

**Spec:** `docs/spec-mvp.md`

## Global Constraints

- macOS only; Apple Silicon + Intel; Rust stable; works in Terminal.app, iTerm2, tmux; 80x24 minimum.
- No Vortix, Tunnelblick, standard WireGuard tools, GUI, daemon, persistent connection state, auto-reconnect, launch-at-login.
- Config root is `~/.config/tuiblick/` with `awg/`, `openvpn/`, `profiles.yaml`; no shell invocation (argv arrays only).
- `cargo fmt --check`, `cargo clippy -- -D warnings`, `cargo test` must pass on every task.
- One tunnel active at a time; quit leaves tunnel up (explicit `d` to stop).
- Sudo password in memory only, zeroized on quit/auth-failure; never on disk.
- Each task ends with its own reviewable git commit.

## Review Focus

- `awg-quick up <path with spaces>` — path with spaces/unicode must work (argv, no shell splitting); expect success.
- Sudo password with trailing newline/unicode + wrong password once — first fails with re-prompt, second succeeds; expect no hang.
- `.ovpn` containing `daemon` directive — expect warning in UI, child stays foreground-captured.
- `awg show` output from a different AmneziaWG version — expect raw text shown, parse never fails the poll.
- No network (ipify + dig both fail) — expect `unknown` exit-IP, UI still renders and connects.

---

### Task 1: Scaffold + errors + main loop shell

**Files:**
- Create: `Cargo.toml`, `src/main.rs`, `src/errors.rs`
- Test: `cargo build` is the test (no unit tests yet)

**Interfaces:**
- Consumes: none
- Produces:
  - `errors::AppError::{Io, Config(String), SudoAuth, Process{cmd:String,code:Option<i32>,tail:String}, NetProbe(String), User(String)}: thiserror::Error`
  - `main()` → `anyhow::Result<()>`: terminal setup/teardown only, calls `app::run().await`

- [ ] **Step 1: Write `Cargo.toml` with exact deps** (`ratatui`, `crossterm`, `tokio` full, `serde`+derive, `serde_yaml`, `thiserror`, `anyhow`, `dirs`, `chrono`, `libc`, `zeroize`, `uuid` v4)
- [ ] **Step 2: Write `src/errors.rs` with `AppError` enum above (6 variants, exact names)**
- [ ] **Step 3: Write `src/main.rs`** — `#[tokio::main]`, crossterm raw-mode + alternate screen, construct `App`, `app::run`, restore terminal on all exits
- [ ] **Step 4: Verify** — Run: `cargo fmt && cargo clippy -- -D warnings && cargo build` Expected: PASS
- [ ] **Step 5: Commit** — `git add Cargo.toml src/main.rs src/errors.rs && git commit -m "feat: scaffold binary and error types"`

### Task 2: config — paths, sanitize, mapping, scan/import/delete

**Files:**
- Create: `src/config.rs`
- Test: `src/config.rs` unit tests (`#[cfg(test)]` inline)

**Interfaces:**
- Consumes: `errors::AppError`
- Produces:
  - `config::Profile { id:String, display_name:String, vpn_type:VpnType, stored_file:String, original_filename:String, created_at:String }`
  - `config::VpnType { Awg, OpenVpn }`
  - `config::sanitize_filename(stem:&str) -> String`
  - `config::tuiblick_dir() -> PathBuf`, `config::load_profiles() -> Result<Vec<Profile>,AppError>`, `config::save_profiles(&[Profile]) -> Result<(),AppError>`, `config::scan_and_reconcile() -> Result<Vec<Profile>,AppError>`, `config::import_file(src:&Path, vpn:VpnType) -> Result<Profile,AppError>`, `config::delete_profile(id:&str) -> Result<(),AppError>`

- [ ] **Step 1: Write failing tests** — `sanitize_lowercases_and_dashes("My Office (1)") == "my-office-1"`; `scan` with orphan file auto-registers; `import` keeps `original_filename`; missing-file entry flagged (test via tempdir `TUlicblick` override or injectable root param)
- [ ] **Step 2: Run to verify fail** — Run: `cargo test config::` Expected: FAIL (module missing)
- [ ] **Step 3: Implement `src/config.rs`** — `tuiblick_dir()` via `dirs::config_dir()/tuiblick`; sanitize: lowercase, `[^a-z0-9-_]`→`-`, collapse, trim `-`, fallback `profile`; reconcile: read yaml (missing file → empty), walk `awg/*.conf` + `openvpn/*.{ovpn,conf}`, dedupe `-2/-3`; import copies file; delete removes file+mapping
- [ ] **Step 4: Verify pass** — Run: `cargo test config:: && cargo clippy -- -D warnings` Expected: PASS
- [ ] **Step 5: Commit** — `git add src/config.rs && git commit -m "feat: config scan, import mapping and sanitize"`

### Task 3: sudo — SudoRunner

**Files:**
- Create: `src/sudo.rs`
- Test: inline unit tests (argv building, sentinel parsing — no real sudo)

**Interfaces:**
- Consumes: `errors::AppError`
- Produces:
  - `sudo::SudoRunner { new(is_root:bool), set_password(String), clear(), has_password() -> bool, is_root() -> bool }`
  - `sudo::SudoRunner::run(&self, prog:&str, args:&[&str]) -> Result<std::process::Output>` (async via tokio)
  - `sudo::is_root_process() -> bool` (via `libc::geteuid() == 0`)
  - `sudo::probe_cached() -> bool` (`sudo -n true`)

- [ ] **Step 1: Write failing tests** — argv is `["-S","-p",sentinel,prog,...]` when not root, `[prog,...]` when root; output containing sentinel+`Sorry, try again` maps to `AppError::SudoAuth`; password with `\n`/unicode is piped with single trailing `\n`
- [ ] **Step 2: Run to verify fail** — Run: `cargo test sudo::` Expected: FAIL
- [ ] **Step 3: Implement** — `tokio::process::Command`, `stdin(Stdio::piped)`, write `password + "\n"`, `Zeroizing<String>` field, drop on `SudoAuth`
- [ ] **Step 4: Verify** — Run: `cargo test sudo:: && cargo clippy -- -D warnings` Expected: PASS (+ Review Focus sudo test pinned here)
- [ ] **Step 5: Commit** — `git add src/sudo.rs && git commit -m "feat: sudo runner with in-memory password"`

### Task 4: awg — up/down/show

**Files:**
- Create: `src/awg.rs`
- Test: inline tests (argv, output parsers — no real device)

**Interfaces:**
- Consumes: `sudo::SudoRunner`, `errors::AppError`
- Produces:
  - `awg::up(runner:&SudoRunner, conf_path:&Path) -> Result<String,AppError>` (returns stdout tail)
  - `awg::down(runner:&SudoRunner, conf_path:&Path) -> Result<String,AppError>`
  - `awg::show(runner:&SudoRunner, iface:&str) -> Result<String,AppError>` (raw text)
  - `awg::parse_summary(raw:&str) -> AwgSummary { peers:usize, latest_handshake:Option<String>, rx:Option<String>, tx:Option<String> }`

- [ ] **Step 1: Write failing tests** — `up` argv is `awg-quick up <full-path>` (path with spaces kept as one arg); `parse_summary` on fixture returns peer count; garbage input returns `peers:0` not error
- [ ] **Step 2: Run to verify fail** — Run: `cargo test awg::` Expected: FAIL
- [ ] **Step 3: Implement** — short-lived `tokio::process`, full-path form per spec §4, combined-output tail ≤2KB on `Process` error
- [ ] **Step 4: Verify** — Run: `cargo test awg:: && cargo clippy -- -D warnings` Expected: PASS (+ Review Focus awg-path + version-tolerance tests pinned here)
- [ ] **Step 5: Commit** — `git add src/awg.rs && git commit -m "feat: amneziawg up down show"`

### Task 5: openvpn — spawn/kill/log ring

**Files:**
- Create: `src/openvpn.rs`
- Test: inline tests (ring cap, filter, daemon sniff — spawn real `sleep`, never real openvpn)

**Interfaces:**
- Consumes: `errors::AppError`
- Produces:
  - `openvpn::LogRing { push(String), lines() -> Vec<String>, filtered(substr:&str) -> Vec<String>, clear_view(), len() }` (cap 2000)
  - `openvpn::OpenVpnSession { spawn(config:&Path) -> Result<Self,AppError>, stop(mut self), take_logs() -> LogRing, has_daemon_directive(config:&Path) -> bool }`

- [ ] **Step 1: Write failing tests** — push 2500 → `len()==2000`; `filtered("TLS")` substring; `has_daemon_directive` detects `daemon` line (comment lines ignored)
- [ ] **Step 2: Run to verify fail** — Run: `cargo test openvpn::` Expected: FAIL
- [ ] **Step 3: Implement** — `tokio::process::Child` stdout+stderr piped, reader tasks merge with `chrono` timestamps; `stop`: SIGTERM via `child.kill` (TERM on unix), 3s grace, then force kill; record exit code
- [ ] **Step 4: Verify** — Run: `cargo test openvpn:: && cargo clippy -- -D warnings` Expected: PASS (+ Review Focus daemon test pinned here)
- [ ] **Step 5: Commit** — `git add src/openvpn.rs && git commit -m "feat: openvpn spawn kill and log ring"`

### Task 6: net — default route + exit IP

**Files:**
- Create: `src/net.rs`
- Test: inline tests (parsers only — no network)

**Interfaces:**
- Consumes: `errors::AppError`
- Produces:
  - `net::RouteInfo { gateway:String, iface:String }`
  - `net::parse_route_get_default(out:&str) -> Option<RouteInfo>`
  - `net::trim_ip(out:&str) -> Option<String>`
  - `net::get_default_route() -> Result<RouteInfo,AppError>` (async, runs `route -n get default`)
  - `net::get_public_ip() -> Result<String,AppError>` (async, `curl --max-time 5 api.ipify.org` → `dig` fallback, total ≤8s)

- [ ] **Step 1: Write failing tests** — parse fixture `gateway: 192.168.0.1 / interface: en0`; `trim_ip("  1.2.3.4\n")=="1.2.3.4"`; garbage → `None`
- [ ] **Step 2: Run to verify fail** — Run: `cargo test net::` Expected: FAIL
- [ ] **Step 3: Implement** — `tokio::process` + `tokio::time::timeout`, never block caller; failure → `AppError::NetProbe`
- [ ] **Step 4: Verify** — Run: `cargo test net:: && cargo clippy -- -D warnings` Expected: PASS (+ Review Focus offline test pinned here: both fail → `NetProbe`, caller shows `unknown`)
- [ ] **Step 5: Commit** — `git add src/net.rs && git commit -m "feat: route and exit-ip probing"`

### Task 7: app — state machine + key handling

**Files:**
- Create: `src/app.rs`
- Test: inline tests (pure state transitions, mocked backends via injected enums — no processes)

**Interfaces:**
- Consumes: all of Tasks 2–6
- Produces:
  - `app::App { profiles, selected, active: Option<ActiveConn>, sudo: SudoRunner, ... }`
  - `app::ActiveConn { profile_id:String, kind:VpnType, started_at:String, last_exit_ip:Option<String> }`
  - `app::run() -> anyhow::Result<()>` (event loop: crossterm events + tokio backend messages, render never awaits child)
  - Key map from spec §7 implemented exactly

- [ ] **Step 1: Write failing tests** — `connect` while active requests switch-confirm (does not spawn); `quit` with active sets confirm-popup; sudo prompt appears only when `!is_root && !has_password`
- [ ] **Step 2: Run to verify fail** — Run: `cargo test app::` Expected: FAIL
- [ ] **Step 3: Implement** — states from spec §7, 3s AWG poll only for active AWG, background net tasks update `initial/current/last_exit_ip`, log reader pumps `LogRing`
- [ ] **Step 4: Verify** — Run: `cargo test app:: && cargo clippy -- -D warnings` Expected: PASS
- [ ] **Step 5: Commit** — `git add src/app.rs && git commit -m "feat: app state machine and key handling"`

### Task 8: ui — ratatui rendering

**Files:**
- Create: `src/ui.rs`
- Test: manual render smoke (no snapshot lib): `cargo run` renders 80x24 without panic; unit test for exit-IP header string builder

**Interfaces:**
- Consumes: `app::App`
- Produces: `ui::render(frame:&mut Frame, app:&App)` — left list, right detail (AWG status / OpenVPN logs), header `init → exit`, bottom status bar, popups (`?`, sudo, import, confirm, error)

- [ ] **Step 1: Write failing test** — `header_line("1.1.1.1","2.2.2.2")` contains both; popup flag renders without panic via `TestBackend`
- [ ] **Step 2: Run to verify fail** — Run: `cargo test ui::` Expected: FAIL
- [ ] **Step 3: Implement** — ratatui layout, no mouse, works in tmux; wire into `app::run`
- [ ] **Step 4: Verify** — Run: `cargo test ui:: && cargo fmt --check && cargo clippy -- -D warnings && timeout 3 cargo run -- --help || true` Expected: PASS / renders
- [ ] **Step 5: Commit** — `git add src/ui.rs && git commit -m "feat: tui layout and popups"`

### Task 9: wiring, manual, polish

**Files:**
- Modify: `src/main.rs` (module decls), `Cargo.toml` if needed
- Create: `docs/manual.md`

**Interfaces:**
- Consumes: all tasks
- Produces: shippable MVP + manual test checklist from spec §9

- [ ] **Step 1: Wire modules + end-to-end build** — `cargo build`, fix warnings
- [ ] **Step 2: Full gate** — Run: `cargo fmt --check && cargo clippy -- -D warnings && cargo test` Expected: PASS
- [ ] **Step 3: Write `docs/manual.md`** — import/connect/disconnect/sudo-once/kill-9/missing-file/tmux smoke steps
- [ ] **Step 4: Commit** — `git add -A && git commit -m "docs: mvp manual and final wiring"`
