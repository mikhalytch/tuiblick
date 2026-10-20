# Build Prompt: macOS AWG/OpenVPN TUI in Rust

Build a native macOS terminal user interface in Rust for managing AmneziaWG and OpenVPN connections.

The application name is intentionally undecided. Use a clearly marked placeholder such as `APP_NAME` throughout the code and configuration paths. I will rename it later.

## Goal

Create a small, reliable TUI that manages:

1. AmneziaWG connections through `awg-quick`
2. OpenVPN connections through the system `openvpn` command
3. Imported VPN configuration files
4. OpenVPN process logs
5. AmneziaWG interface status

The application must not depend on Vortix, Tunnelblick, standard WireGuard tools, or a GUI.

This is an initial version. Do not implement persistent connection state, profiles database, automatic reconnection, background daemons, or launch-at-login behavior.

## Platform

Target platform:

- macOS only
- Apple Silicon and Intel where possible
- Rust stable
- Terminal application
- Must work from Terminal.app, iTerm2, and tmux

Use platform-specific code only where necessary. Clearly isolate macOS-specific behavior.

## Suggested dependencies

Use current stable versions of these crates where appropriate:

- `ratatui` for the TUI
- `crossterm` for keyboard input and terminal handling
- `tokio` for asynchronous process handling
- `serde` and `serde_yaml` or `serde_json` for internal metadata if needed
- `anyhow` or `thiserror` for errors
- `dirs` for resolving the home directory
- `chrono` for timestamps
- `nix` only if needed for process signaling
- `shlex` only if needed, but prefer passing command arguments directly rather than invoking a shell

Avoid adding dependencies unnecessarily.

## Configuration directory

Use `$XDG_CONFIG_HOME/APP_NAME/`. If `XDG_CONFIG_HOME` is unset, empty,
or relative, default to `~/.config/APP_NAME/`, including on macOS.
The variable must contain an absolute base directory.

```text
$XDG_CONFIG_HOME/APP_NAME/
