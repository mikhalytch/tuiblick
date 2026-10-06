//! AmneziaWG backend: up/down/show (spec §4).
//!
//! - Up: `awg-quick up <full-stored-path>`; down mirrors it. The full path
//!   is required (bare `up <iface>` only searches `/etc/amnezia/...` and
//!   `/usr/local/etc/...`) and travels as ONE argv element — paths with
//!   spaces/unicode are never shell-split (argv arrays only, no shell).
//! - Status: `awg show <iface>` returns the raw text; [`parse_summary`]
//!   derives a best-effort [`AwgSummary`] (peer count, latest handshake,
//!   transfer rx/tx). Parsing never fails: unknown, garbage, or
//!   different-version output yields `peers: 0` plus `None`s — never an
//!   error or panic. Raw text stays the source of truth.
//! - All calls are short-lived via [`SudoRunner::run`]. Non-zero exits come
//!   back as [`AppError::SudoAuth`] (the runner already dropped the stored
//!   password — callers re-prompt once, spec §3) or [`AppError::Process`]
//!   with `cmd` in backend form (`awg-quick up <path>`, never the
//!   `sudo -S ...` wrapper) and the last 2 KB of combined output.

// Task 4: first consumer is Task 7 (`app`); allow dead code until then,
// mirroring `sudo.rs` / `config.rs`.
#![allow(dead_code)]

use std::path::Path;
use std::process::Output;

use crate::errors::AppError;
use crate::sudo::SudoRunner;

/// Bytes of output kept in success text / `AppError::Process.tail`.
const OUTPUT_TAIL_MAX: usize = 2048;

/// Best-effort summary of `awg show <iface>` output.
///
/// Produced by [`parse_summary`], which never fails: fields are `None`
/// (and `peers` is `0`) whenever the raw text does not carry them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AwgSummary {
    /// `peer: <key>` lines (ASCII case-insensitive; a bare `peer:` with no
    /// key is degenerate output and is not counted).
    pub peers: usize,
    /// Value after `latest handshake:` on the first peer block carrying one.
    pub latest_handshake: Option<String>,
    /// Value before `received` on the first `transfer:` line carrying one.
    pub rx: Option<String>,
    /// Value between `received` and `sent` on the first `transfer:` line.
    pub tx: Option<String>,
}

/// Bring the tunnel for `conf_path` up: `awg-quick up <full-stored-path>`.
///
/// Returns the combined success text (trimmed, tail-capped if huge).
/// `SudoAuth` propagates as-is (re-prompt once); other failures arrive as
/// `Process` with the backend-form `cmd`.
pub async fn up(runner: &SudoRunner, conf_path: &Path) -> Result<String, AppError> {
    let (prog, argv) = up_command(conf_path);
    run_backend(runner, &prog, argv).await
}

/// Bring the tunnel for `conf_path` down: `awg-quick down <full-stored-path>`.
/// Same output/error contract as [`up`].
pub async fn down(runner: &SudoRunner, conf_path: &Path) -> Result<String, AppError> {
    let (prog, argv) = down_command(conf_path);
    run_backend(runner, &prog, argv).await
}

/// Raw `awg show <iface>` text (unparsed; feed to [`parse_summary`]).
/// Same error contract as [`up`].
pub async fn show(runner: &SudoRunner, iface: &str) -> Result<String, AppError> {
    let (prog, argv) = show_command(iface);
    run_backend(runner, &prog, argv).await
}

/// Best-effort [`AwgSummary`] of raw `awg show` text. Never fails, never
/// panics: garbage/empty/different-version input yields `peers: 0` + `None`s.
pub fn parse_summary(raw: &str) -> AwgSummary {
    let mut summary = AwgSummary {
        peers: 0,
        latest_handshake: None,
        rx: None,
        tx: None,
    };
    for line in raw.lines() {
        if is_peer_line(line) {
            summary.peers += 1;
        }
        if summary.latest_handshake.is_none() {
            if let Some(value) = field_value(line, "latest handshake") {
                summary.latest_handshake = Some(value);
            }
        }
        if summary.rx.is_none() || summary.tx.is_none() {
            let (rx, tx) = transfer_values(line);
            if summary.rx.is_none() {
                summary.rx = rx;
            }
            if summary.tx.is_none() {
                summary.tx = tx;
            }
        }
    }
    summary
}

/// True for `peer: <key>` lines (ASCII case-insensitive): a bare `peer:`
/// with no key is degenerate output, not a peer. Real `awg show` always
/// prints the key on the same line, so requiring it cannot undercount.
fn is_peer_line(line: &str) -> bool {
    let trimmed = line.trim_start();
    let bytes = trimmed.as_bytes();
    // A match means the first 5 bytes are ASCII, so slicing at 5 is a valid
    // char boundary (non-ASCII bytes can never equal ASCII case-insensitively).
    bytes.len() >= 5 && bytes[..5].eq_ignore_ascii_case(b"peer:") && !trimmed[5..].trim().is_empty()
}

/// Value after the first `:` on lines whose key part mentions `marker`
/// (ASCII case-insensitive), trimmed; `None` when absent or empty.
fn field_value(line: &str, marker: &str) -> Option<String> {
    let colon = line.find(':')?;
    find_ascii_ci(&line[..colon], marker)?;
    let value = line[colon + 1..].trim();
    if value.is_empty() {
        None
    } else {
        Some(value.to_owned())
    }
}

/// `(rx, tx)` from a `transfer:` line, best-effort. Standard form:
/// `transfer: 1.23 MiB received, 456.78 KiB sent`.
fn transfer_values(line: &str) -> (Option<String>, Option<String>) {
    let colon = match line.find(':') {
        Some(c) => c,
        None => return (None, None),
    };
    if find_ascii_ci(&line[..colon], "transfer").is_none() {
        return (None, None);
    }
    let rest = &line[colon + 1..];
    let received_at = find_ascii_ci(rest, "received");
    let rx = received_at
        .map(|pos| rest[..pos].trim().to_owned())
        .filter(|s| !s.is_empty());
    let tx = find_ascii_ci(rest, "sent").and_then(|sent_at| {
        let after_received = match received_at {
            Some(rpos) if rpos < sent_at => rpos + "received".len(),
            _ => 0,
        };
        let chunk = rest[after_received..sent_at]
            .trim()
            .trim_start_matches(',')
            .trim();
        if chunk.is_empty() {
            None
        } else {
            Some(chunk.to_owned())
        }
    });
    (rx, tx)
}

/// Byte-wise ASCII case-insensitive substring search. The returned index
/// always starts at an ASCII byte (a non-ASCII byte can never equal an
/// ASCII needle byte), so it is a valid `str` slicing boundary.
/// `None` for empty or overlong needles — never panics.
fn find_ascii_ci(haystack: &str, needle: &str) -> Option<usize> {
    let (h, n) = (haystack.as_bytes(), needle.as_bytes());
    if n.is_empty() || n.len() > h.len() {
        return None;
    }
    h.windows(n.len())
        .position(|window| window.eq_ignore_ascii_case(n))
}

/// Shared short-lived backend run: success text on `Ok`, backend-form
/// `Process` error otherwise, `SudoAuth` propagated for a one-time re-prompt.
async fn run_backend(
    runner: &SudoRunner,
    prog: &str,
    argv: Vec<String>,
) -> Result<String, AppError> {
    let cmd = backend_display(prog, &argv);
    let args: Vec<&str> = argv.iter().map(String::as_str).collect();
    match runner.run(prog, &args).await {
        Ok(output) => finish_output(&cmd, output),
        Err(err) => Err(remap_process_cmd(&cmd, err)),
    }
}

/// `(program, argv)` for `awg-quick up <full-path>`: the path stays ONE
/// argv element (spaces/unicode safe, no shell).
pub(crate) fn up_command(conf_path: &Path) -> (String, Vec<String>) {
    (
        "awg-quick".to_owned(),
        vec!["up".to_owned(), path_arg(conf_path)],
    )
}

/// `(program, argv)` for `awg-quick down <full-path>`.
pub(crate) fn down_command(conf_path: &Path) -> (String, Vec<String>) {
    (
        "awg-quick".to_owned(),
        vec!["down".to_owned(), path_arg(conf_path)],
    )
}

/// `(program, argv)` for `awg show <iface>`.
pub(crate) fn show_command(iface: &str) -> (String, Vec<String>) {
    ("awg".to_owned(), vec!["show".to_owned(), iface.to_owned()])
}

/// Full config path as one argv element. Non-UTF8 paths are lossy-converted;
/// the backend then reports file-not-found as a `Process` error.
fn path_arg(path: &Path) -> String {
    path.as_os_str().to_string_lossy().into_owned()
}

/// Backend command line for `AppError::Process.cmd`
/// (e.g. `awg-quick up /a b/c.conf`): never the `sudo -S …` wrapper, never
/// the password (it travels via stdin only).
pub(crate) fn backend_display(prog: &str, argv: &[String]) -> String {
    if argv.is_empty() {
        prog.to_owned()
    } else {
        format!("{prog} {}", argv.join(" "))
    }
}

/// Map a finished child `Output` to success text or a backend-form
/// `Process` error. Pure: forged `Output` in, `Ok(String)` /
/// `Err(Process)` out.
pub(crate) fn finish_output(backend_cmd: &str, output: Output) -> Result<String, AppError> {
    if output.status.success() {
        Ok(success_text(&output))
    } else {
        Err(AppError::Process {
            cmd: backend_cmd.to_owned(),
            code: output.status.code(),
            tail: tail_2k(&combined_text(&output)),
        })
    }
}

/// Rewrite the runner's `Process` error to the backend-form `cmd`;
/// `SudoAuth` (and `Io`, …) pass through untouched.
pub(crate) fn remap_process_cmd(backend_cmd: &str, err: AppError) -> AppError {
    match err {
        AppError::Process { code, tail, .. } => AppError::Process {
            cmd: backend_cmd.to_owned(),
            code,
            tail,
        },
        other => other,
    }
}

/// Combined stdout+stderr success text: trimmed, last 2 KB if huge.
pub(crate) fn success_text(output: &Output) -> String {
    tail_2k(combined_text(output).trim())
}

/// Concatenated stdout+stderr as lossy text.
fn combined_text(output: &Output) -> String {
    let mut buf = Vec::with_capacity(output.stdout.len() + output.stderr.len());
    buf.extend_from_slice(&output.stdout);
    buf.extend_from_slice(&output.stderr);
    String::from_utf8_lossy(&buf).into_owned()
}

/// Last `OUTPUT_TAIL_MAX` bytes of `text`, re-anchored to a char boundary
/// so multi-byte UTF-8 at the cut is not split.
fn tail_2k(text: &str) -> String {
    let bytes = text.as_bytes();
    if bytes.len() <= OUTPUT_TAIL_MAX {
        return text.to_owned();
    }
    let mut start = bytes.len() - OUTPUT_TAIL_MAX;
    while start < bytes.len() && !text.is_char_boundary(start) {
        start += 1;
    }
    text[start..].to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::process::ExitStatusExt;

    /// Forged successful child output (no processes spawned).
    fn ok_output(stdout: &str, stderr: &str) -> Output {
        Output {
            status: std::process::ExitStatus::from_raw(0),
            stdout: stdout.as_bytes().to_vec(),
            stderr: stderr.as_bytes().to_vec(),
        }
    }

    /// Forged failed child output (no processes spawned).
    fn failed_output(code: i32, stdout: &str, stderr: &str) -> Output {
        Output {
            status: std::process::ExitStatus::from_raw(code << 8),
            stdout: stdout.as_bytes().to_vec(),
            stderr: stderr.as_bytes().to_vec(),
        }
    }

    const REALISTIC_SHOW: &str = "\
interface: awg0
  public key: abc1234567890=
  private key: (hidden)
  listening port: 51820

peer: def4567890abc=
  endpoint: 203.0.113.7:51820
  allowed ips: 0.0.0.0/0
  latest handshake: 1 minute, 23 seconds ago
  transfer: 1.23 MiB received, 456.78 KiB sent
  persistent keepalive: every 25 seconds
";

    // --- argv builders: full path stays one element (Review Focus) ---

    #[test]
    fn up_argv_keeps_spaced_path_as_single_element() {
        let (prog, argv) = up_command(Path::new("/Users/x/My Office (1)/my office.conf"));
        assert_eq!(prog, "awg-quick");
        assert_eq!(
            argv,
            vec![
                "up".to_owned(),
                "/Users/x/My Office (1)/my office.conf".to_owned()
            ]
        );
    }

    #[test]
    fn up_argv_preserves_unicode_path() {
        let (prog, argv) = up_command(Path::new("/Users/x/Мой офис🔑/tun.conf"));
        assert_eq!(prog, "awg-quick");
        assert_eq!(argv.len(), 2);
        assert_eq!(argv[0], "up");
        assert_eq!(argv[1], "/Users/x/Мой офис🔑/tun.conf");
    }

    #[test]
    fn down_argv_keeps_spaced_path_as_single_element() {
        let (prog, argv) = down_command(Path::new("/Users/x/My Office (1)/my office.conf"));
        assert_eq!(prog, "awg-quick");
        assert_eq!(
            argv,
            vec![
                "down".to_owned(),
                "/Users/x/My Office (1)/my office.conf".to_owned()
            ]
        );
    }

    #[test]
    fn show_argv_is_awg_show_iface() {
        let (prog, argv) = show_command("awg0");
        assert_eq!(prog, "awg");
        assert_eq!(argv, vec!["show".to_owned(), "awg0".to_owned()]);
    }

    #[test]
    fn backend_display_joins_prog_and_argv() {
        assert_eq!(
            backend_display("awg-quick", &["up".to_owned(), "/a b/c.conf".to_owned()]),
            "awg-quick up /a b/c.conf"
        );
    }

    // --- classification helper: forged Output -> Ok / Process ---

    #[test]
    fn finish_output_ok_returns_trimmed_success_text() {
        let out = finish_output(
            "awg-quick up /x/y.conf",
            ok_output("[#] ip link add up\nup ok\n", ""),
        )
        .unwrap();
        assert!(out.contains("up ok"), "got {out:?}");
        assert_eq!(out, out.trim(), "success text must be trimmed");
    }

    #[test]
    fn finish_output_ok_combines_stdout_and_stderr() {
        let out = finish_output(
            "awg-quick up /x/y.conf",
            ok_output("out-line\n", "err-line\n"),
        )
        .unwrap();
        assert!(out.contains("out-line"), "got {out:?}");
        assert!(out.contains("err-line"), "got {out:?}");
    }

    #[test]
    fn finish_output_ok_caps_huge_output_at_2k() {
        let big = "y".repeat(5000) + "END";
        let out = finish_output("awg-quick up /x/y.conf", ok_output(&big, "")).unwrap();
        assert!(out.len() <= 2048, "len {}", out.len());
        assert!(out.ends_with("END"), "must keep the tail");
    }

    #[test]
    fn finish_output_failure_maps_to_backend_process_error() {
        let big = "z".repeat(5000) + "TAIL";
        let err = finish_output("awg show awg0", failed_output(2, &big, "e\n")).unwrap_err();
        match err {
            AppError::Process { cmd, code, tail } => {
                assert_eq!(cmd, "awg show awg0");
                assert_eq!(code, Some(2));
                assert!(tail.len() <= 2048, "len {}", tail.len());
                // Combined stdout+stderr: stdout's TAIL marker then stderr's "e".
                assert!(
                    tail.ends_with("TAILe\n"),
                    "must keep the tail, got {tail:?}"
                );
            }
            other => panic!("expected Process, got {other:?}"),
        }
    }

    #[test]
    fn remap_process_cmd_rewrites_sudo_wrapper_to_backend() {
        let forged = AppError::Process {
            cmd: "sudo -S -p [sentinel] awg-quick up /x/y.conf".to_owned(),
            code: Some(1),
            tail: "boom".to_owned(),
        };
        match remap_process_cmd("awg-quick up /x/y.conf", forged) {
            AppError::Process { cmd, code, tail } => {
                assert_eq!(cmd, "awg-quick up /x/y.conf");
                assert_eq!(code, Some(1));
                assert_eq!(tail, "boom");
            }
            other => panic!("expected Process, got {other:?}"),
        }
    }

    #[test]
    fn remap_process_cmd_leaves_sudo_auth_alone() {
        // The runner already dropped the password; the caller re-prompts once.
        let err = remap_process_cmd("awg-quick up /x/y.conf", AppError::SudoAuth);
        assert!(matches!(err, AppError::SudoAuth), "got {err:?}");
    }

    // --- parse_summary ---

    #[test]
    fn parse_summary_on_realistic_fixture() {
        let s = parse_summary(REALISTIC_SHOW);
        assert_eq!(s.peers, 1);
        assert_eq!(
            s.latest_handshake.as_deref(),
            Some("1 minute, 23 seconds ago")
        );
        assert_eq!(s.rx.as_deref(), Some("1.23 MiB"));
        assert_eq!(s.tx.as_deref(), Some("456.78 KiB"));
    }

    #[test]
    fn parse_summary_counts_two_peers() {
        let raw = "\
interface: awg0
peer: aaa=
  latest handshake: 5 seconds ago
  transfer: 1 KiB received, 2 KiB sent
peer: bbb=
  latest handshake: 1 hour ago
  transfer: 3 KiB received, 4 KiB sent
";
        let s = parse_summary(raw);
        assert_eq!(s.peers, 2);
        assert!(s.latest_handshake.is_some(), "got {s:?}");
        assert!(s.rx.is_some() && s.tx.is_some(), "got {s:?}");
    }

    #[test]
    fn parse_summary_tolerates_different_version_output() {
        // Different AmneziaWG version: reordered, re-cased keys, extra fields.
        let raw = "\
INTERFACE: awg7
  Some Future Field: shiny
  Listening Port: 51821
PEER: aaa=
  Latest Handshake: 2 minutes ago
  TRANSFER: 9.99 GiB received, 1.01 GiB sent
  Some Other Counter: 42
";
        let s = parse_summary(raw);
        assert_eq!(s.peers, 1, "got {s:?}");
        assert_eq!(s.latest_handshake.as_deref(), Some("2 minutes ago"));
        assert_eq!(s.rx.as_deref(), Some("9.99 GiB"));
        assert_eq!(s.tx.as_deref(), Some("1.01 GiB"));
    }

    #[test]
    fn parse_summary_missing_transfer_leaves_rx_tx_none() {
        let raw = "interface: awg0\npeer: aaa=\n  endpoint: 1.2.3.4:5\n";
        let s = parse_summary(raw);
        assert_eq!(s.peers, 1);
        assert_eq!(s.latest_handshake, None);
        assert_eq!(s.rx, None);
        assert_eq!(s.tx, None);
    }

    #[test]
    fn parse_summary_garbage_empty_never_fails() {
        for raw in [
            "",
            "\n",
            "blarg not awg output\n???\n",
            "peer\npeer:\n: : :\ntransfer:\ntransfer: received\n",
            "🔑💥\r\n\x7f peer\u{200b}:\u{feff} x\n",
        ] {
            let s = parse_summary(raw);
            assert_eq!(s.peers, 0, "input {raw:?} gave {s:?}");
            assert_eq!(s.latest_handshake, None, "input {raw:?}");
            assert_eq!(s.rx, None, "input {raw:?}");
            assert_eq!(s.tx, None, "input {raw:?}");
        }
    }
}
