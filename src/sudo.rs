//! Privilege escalation front-end (spec §3).
//!
//! - If the process itself runs as root (`euid == 0`), backend commands run
//!   directly, never through `sudo`, and no password is ever requested.
//! - Otherwise [`SudoRunner`] pipes an in-memory password to
//!   `sudo -S -p <sentinel> <prog> <args...>` (argv arrays only, no shell).
//!   The password lives in `Option<Zeroizing<String>>`: never on disk,
//!   dropped (zeroized) on [`AppError::SudoAuth`] and via [`SudoRunner::clear`]
//!   on quit.
//! - `sudo`'s prompt is forced to the fixed [`SUDO_PROMPT_SENTINEL`] so auth
//!   failures can be told apart from backend-command failures: a non-zero
//!   exit whose combined output carries an auth marker (`Sorry, try again`,
//!   …) or the sentinel maps to [`AppError::SudoAuth`]; any other non-zero
//!   exit maps to [`AppError::Process`] with the last 2 KB of output.
//! - No-hang guarantee: stdin is always piped; the password (if any) is
//!   written once and the handle is then dropped, so `sudo` sees EOF
//!   instead of blocking on a terminal-less read. When no password is set,
//!   stdin is closed immediately and `sudo` fails fast.
//!
//! [`probe_cached`] is a synchronous `sudo -n true` by design: `-n` never
//! prompts, it runs once at startup, and async callers can wrap it in
//! `spawn_blocking` if ever needed.

use std::process::Output;
use std::sync::Mutex;

use tokio::io::AsyncWriteExt;
use zeroize::Zeroizing;

use crate::errors::AppError;

/// `sudo -p` prompt string: fixed, and unlikely to appear in normal backend
/// command output, so its presence in captured stderr means `sudo` itself
/// prompted (and hence any exit failure is most likely an auth problem).
pub const SUDO_PROMPT_SENTINEL: &str = "[tuiblick-sudo-prompt]";

/// Lowercase auth-failure markers matched against combined stdout+stderr.
/// `Sorry, try again` covers both `Sorry, try again` and `Sorry, try again.`.
const AUTH_MARKERS: &[&str] = &[
    "sorry, try again",
    "incorrect password attempt",
    "sudo: no password was provided",
    "sudo: a password is required",
];

/// Max bytes of combined output kept in `AppError::Process.tail`.
const OUTPUT_TAIL_MAX: usize = 2048;

/// Holds the sudo password in memory only and fronts every privileged call.
///
/// `&self` receivers + a short-lived `Mutex` (never held across `.await`):
/// [`run`](SudoRunner::run) must be able to drop the password on auth
/// failure through a shared reference.
pub struct SudoRunner {
    is_root: bool,
    password: Mutex<Option<Zeroizing<String>>>,
}

// Custom `Debug`: must never leak the password into logs.
impl std::fmt::Debug for SudoRunner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SudoRunner")
            .field("is_root", &self.is_root)
            .field("has_password", &self.has_password())
            .finish()
    }
}

impl SudoRunner {
    /// `is_root` should come from [`is_root_process`]; taken as a parameter
    /// so tests and callers can construct either mode explicitly.
    pub fn new(is_root: bool) -> Self {
        Self {
            is_root,
            password: Mutex::new(None),
        }
    }

    /// True when backend commands run directly (TUI itself launched as root).
    pub fn is_root(&self) -> bool {
        self.is_root
    }

    /// True when a password is currently held in memory.
    pub fn has_password(&self) -> bool {
        self.password.lock().unwrap().is_some()
    }

    /// Store `password` in memory (replaces any previous one).
    /// The `String` is moved in, so the caller holds no second copy;
    /// callers should pass an owned buffer they no longer need.
    pub fn set_password(&self, password: String) {
        *self.password.lock().unwrap() = Some(Zeroizing::new(password));
    }

    /// Drop the stored password now (zeroized on drop). Called on quit and
    /// automatically on [`AppError::SudoAuth`].
    pub fn clear(&self) {
        *self.password.lock().unwrap() = None;
    }

    /// Clone the password into pipe-ready bytes (`password + "\n"`, exactly
    /// one trailing newline) under the lock; the lock is released before any
    /// I/O. The returned buffer is itself zeroizing.
    fn password_bytes(&self) -> Option<Zeroizing<Vec<u8>>> {
        self.password
            .lock()
            .unwrap()
            .as_ref()
            .map(|pw| Zeroizing::new(password_stdin_bytes(pw)))
    }

    /// Run `prog` with `args`: directly when root, else via
    /// `sudo -S -p <sentinel>`. Async via `tokio::process`; never uses a shell.
    pub async fn run(&self, prog: &str, args: &[&str]) -> Result<Output, AppError> {
        let (program, argv) = command_line(self.is_root, prog, args);
        let cmd_display = display_cmd(&program, &argv);
        let mut child = tokio::process::Command::new(&program)
            .args(&argv)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .map_err(AppError::Io)?;

        // Pipe the password (if any) once, then close stdin so sudo sees EOF
        // instead of blocking forever on a terminal-less read. The child may
        // exit before reading (bad-password path); a broken pipe is not an
        // error here.
        if let Some(stdin) = child.stdin.take() {
            let mut stdin = stdin;
            if let Some(secret) = self.password_bytes() {
                let _ = stdin.write_all(secret.as_slice()).await;
            }
        }

        let output = child.wait_with_output().await.map_err(AppError::Io)?;
        self.classify(cmd_display, output)
    }

    /// Spawn a long-lived backend child without waiting for it (openvpn, Task 5).
    ///
    /// Mirrors [`run`](Self::run): root mode spawns `prog` directly, otherwise
    /// `sudo -S -p <sentinel> prog ...args` with the password piped once and
    /// stdin then closed, so sudo sees EOF instead of hanging on a
    /// terminal-less read. Non-root with no password stored fails fast with
    /// [`AppError::User`] (the caller prompts first) — a spawn here never
    /// blocks waiting on a sudo prompt.
    ///
    /// Unlike [`run`](Self::run), auth failures CANNOT be reported here: the
    /// child is still starting. A wrong password surfaces as sudo error lines
    /// in the child's captured stderr; the caller sniffs the log for that.
    /// Paths with spaces/unicode stay single argv elements (no shell).
    pub(crate) async fn spawn_long_lived(
        &self,
        prog: &str,
        args: &[&str],
    ) -> Result<tokio::process::Child, AppError> {
        if !self.is_root && !self.has_password() {
            return Err(AppError::User("sudo password required".to_owned()));
        }
        let (program, argv) = command_line(self.is_root, prog, args);
        let mut child = tokio::process::Command::new(&program)
            .args(&argv)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .map_err(AppError::Io)?;

        // Same no-hang contract as `run`: write once, then close stdin.
        // With no password stored the handle is dropped immediately and a
        // timestamp-less sudo fails fast instead of blocking.
        if let Some(stdin) = child.stdin.take() {
            let mut stdin = stdin;
            if let Some(secret) = self.password_bytes() {
                let _ = stdin.write_all(secret.as_slice()).await;
            }
        }
        Ok(child)
    }

    /// Map a finished child to `Ok` / `SudoAuth` / `Process`, dropping the
    /// stored password on auth failure so the caller re-prompts.
    fn classify(&self, cmd_display: String, output: Output) -> Result<Output, AppError> {
        if output.status.success() {
            // Auth may have prompted (sentinel in stderr) and still succeeded:
            // success is success, password kept.
            return Ok(output);
        }
        let combined = combined_text(&output);
        // Only non-root runs go through sudo at all; in root mode even
        // marker-like backend output is a plain backend failure.
        if !self.is_root
            && (contains_auth_marker(&combined) || combined.contains(SUDO_PROMPT_SENTINEL))
        {
            self.clear();
            return Err(AppError::SudoAuth);
        }
        Err(AppError::Process {
            cmd: cmd_display,
            code: output.status.code(),
            tail: tail_2k(&combined),
        })
    }
}

/// `(program, argv)` for a backend invocation: `[prog, ...args]` when root,
/// `sudo -S -p <sentinel> prog ...args` otherwise. Paths with spaces/unicode
/// stay single argv elements (no shell splitting, Review Focus).
pub(crate) fn command_line(is_root: bool, prog: &str, args: &[&str]) -> (String, Vec<String>) {
    if is_root {
        (
            prog.to_owned(),
            args.iter().map(|s| (*s).to_owned()).collect(),
        )
    } else {
        let mut argv = Vec::with_capacity(args.len() + 4);
        argv.push("-S".to_owned());
        argv.push("-p".to_owned());
        argv.push(SUDO_PROMPT_SENTINEL.to_owned());
        argv.push(prog.to_owned());
        argv.extend(args.iter().map(|s| (*s).to_owned()));
        ("sudo".to_owned(), argv)
    }
}

/// Pipe-ready stdin bytes: the password plus exactly one trailing `\n`,
/// even if it already ends with newline(s)/CRLF or contains interior
/// newlines/unicode (Review Focus: trailing-newline + unicode passwords).
pub(crate) fn password_stdin_bytes(password: &str) -> Vec<u8> {
    // Strip all trailing newline/CR bytes, then add exactly one `\n`.
    // Interior newlines and multi-byte UTF-8 pass through untouched.
    let trimmed = password.trim_end_matches(['\n', '\r']);
    let mut out = Vec::with_capacity(trimmed.len() + 1);
    out.extend_from_slice(trimmed.as_bytes());
    out.push(b'\n');
    out
}

/// True when combined child output carries a sudo auth-failure marker
/// (case-insensitive).
pub(crate) fn contains_auth_marker(combined: &str) -> bool {
    let lower = combined.to_lowercase();
    AUTH_MARKERS.iter().any(|marker| lower.contains(marker))
}

/// Last `OUTPUT_TAIL_MAX` bytes of `combined`, re-anchored to a char
/// boundary, for `AppError::Process.tail`.
pub(crate) fn tail_2k(combined: &str) -> String {
    let bytes = combined.as_bytes();
    if bytes.len() <= OUTPUT_TAIL_MAX {
        return combined.to_owned();
    }
    // Keep the last OUTPUT_TAIL_MAX bytes, re-anchored to a char boundary
    // so multi-byte UTF-8 at the cut is not split.
    let mut start = bytes.len() - OUTPUT_TAIL_MAX;
    while start < bytes.len() && !combined.is_char_boundary(start) {
        start += 1;
    }
    combined[start..].to_owned()
}

/// Human-readable command line for `AppError::Process.cmd`
/// (never contains the password — it travels via stdin only).
pub(crate) fn display_cmd(program: &str, argv: &[String]) -> String {
    if argv.is_empty() {
        program.to_owned()
    } else {
        format!("{} {}", program, argv.join(" "))
    }
}

/// Concatenated stdout+stderr as lossy text for marker search and tails.
fn combined_text(output: &Output) -> String {
    let mut buf = Vec::with_capacity(output.stdout.len() + output.stderr.len());
    buf.extend_from_slice(&output.stdout);
    buf.extend_from_slice(&output.stderr);
    String::from_utf8_lossy(&buf).into_owned()
}

/// True when this process runs as root (`geteuid() == 0`).
pub fn is_root_process() -> bool {
    // SAFETY: `geteuid` is async-signal-safe, takes no arguments and has no
    // preconditions; it cannot fail.
    unsafe { libc::geteuid() == 0 }
}

/// True when `sudo -n true` succeeds, i.e. the sudo timestamp is cached and
/// no password is currently needed.
///
/// Synchronous `std::process::Command` by design: `-n` never prompts, stdin
/// is nulled for extra hang safety, and the check runs rarely (startup /
/// reconnect). Async callers can wrap it in `spawn_blocking`.
pub fn probe_cached() -> bool {
    std::process::Command::new("sudo")
        .args(["-n", "true"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::process::ExitStatusExt;

    /// `Output` with a unix exit code and given stdout/stderr (no processes).
    fn failed_output(code: i32, stdout: &str, stderr: &str) -> Output {
        Output {
            status: std::process::ExitStatus::from_raw(code << 8),
            stdout: stdout.as_bytes().to_vec(),
            stderr: stderr.as_bytes().to_vec(),
        }
    }

    fn success_output(stdout: &str, stderr: &str) -> Output {
        Output {
            status: std::process::ExitStatus::from_raw(0),
            stdout: stdout.as_bytes().to_vec(),
            stderr: stderr.as_bytes().to_vec(),
        }
    }

    #[test]
    fn non_root_argv_goes_through_sudo_with_sentinel() {
        let (program, argv) = command_line(false, "awg-quick", &["up", "/Users/x/My Office.conf"]);
        assert_eq!(program, "sudo");
        assert_eq!(
            argv,
            vec![
                "-S".to_owned(),
                "-p".to_owned(),
                SUDO_PROMPT_SENTINEL.to_owned(),
                "awg-quick".to_owned(),
                "up".to_owned(),
                "/Users/x/My Office.conf".to_owned(),
            ]
        );
    }

    #[test]
    fn root_argv_runs_backend_directly() {
        let (program, argv) = command_line(true, "awg-quick", &["up", "/Users/x/My Office.conf"]);
        assert_eq!(program, "awg-quick");
        assert_eq!(
            argv,
            vec!["up".to_owned(), "/Users/x/My Office.conf".to_owned()]
        );
    }

    #[test]
    fn sorry_try_again_with_sentinel_maps_to_sudo_auth() {
        let runner = SudoRunner::new(false);
        runner.set_password("wrong".to_owned());
        let stderr = format!("{SUDO_PROMPT_SENTINEL}Sorry, try again.\n");
        let err = runner
            .classify(
                "sudo awg-quick up x".to_owned(),
                failed_output(1, "", &stderr),
            )
            .unwrap_err();
        assert!(matches!(err, AppError::SudoAuth), "got {err:?}");
        assert!(
            !runner.has_password(),
            "password must be dropped on SudoAuth"
        );
    }

    #[test]
    fn sorry_try_again_without_period_or_sentinel_maps_to_sudo_auth() {
        let runner = SudoRunner::new(false);
        runner.set_password("wrong".to_owned());
        let err = runner
            .classify(
                "sudo true".to_owned(),
                failed_output(1, "", "Sorry, try again\n"),
            )
            .unwrap_err();
        assert!(matches!(err, AppError::SudoAuth), "got {err:?}");
        assert!(!runner.has_password());
    }

    #[test]
    fn sentinel_present_on_exit_failure_maps_to_sudo_auth() {
        // No marker text at all: sudo prompted (sentinel in stderr) and the
        // invocation failed → treat as auth failure per spec §3.
        let runner = SudoRunner::new(false);
        runner.set_password("wrong".to_owned());
        let stderr = format!("{SUDO_PROMPT_SENTINEL}sudo: 1 incorrect password attempt\n");
        let err = runner
            .classify("sudo true".to_owned(), failed_output(1, "", &stderr))
            .unwrap_err();
        assert!(matches!(err, AppError::SudoAuth), "got {err:?}");
        assert!(!runner.has_password());
    }

    #[test]
    fn non_auth_failure_maps_to_process_and_keeps_password() {
        let runner = SudoRunner::new(false);
        runner.set_password("right".to_owned());
        let err = runner
            .classify(
                "sudo awg-quick up x".to_owned(),
                failed_output(2, "interface: foo\n", "plain backend boom"),
            )
            .unwrap_err();
        match err {
            AppError::Process { cmd, code, tail } => {
                assert_eq!(cmd, "sudo awg-quick up x");
                assert_eq!(code, Some(2));
                assert!(tail.contains("plain backend boom"), "tail: {tail:?}");
            }
            other => panic!("expected Process, got {other:?}"),
        }
        assert!(
            runner.has_password(),
            "password must survive non-auth errors"
        );
    }

    #[test]
    fn root_mode_never_maps_to_sudo_auth() {
        // Direct run, no sudo involved: even marker-like output is a plain
        // backend failure.
        let runner = SudoRunner::new(true);
        let err = runner
            .classify(
                "awg-quick up x".to_owned(),
                failed_output(1, "", "Sorry, try again.\n"),
            )
            .unwrap_err();
        assert!(matches!(err, AppError::Process { .. }), "got {err:?}");
    }

    #[test]
    fn success_output_is_ok_even_with_sentinel_present() {
        // sudo prompts (sentinel in stderr) but auth succeeds and the command
        // passes: not an auth failure, password kept.
        let runner = SudoRunner::new(false);
        runner.set_password("right".to_owned());
        let out = success_output("up ok\n", SUDO_PROMPT_SENTINEL);
        assert!(runner.classify("sudo x".to_owned(), out).is_ok());
        assert!(runner.has_password());
    }

    #[test]
    fn process_tail_capped_at_2k() {
        let runner = SudoRunner::new(false);
        let big = "y".repeat(5000) + "END";
        let err = runner
            .classify("sudo x".to_owned(), failed_output(3, &big, ""))
            .unwrap_err();
        match err {
            AppError::Process { code, tail, .. } => {
                assert_eq!(code, Some(3));
                assert!(tail.len() <= 2048, "tail len {}", tail.len());
                assert!(tail.ends_with("END"), "tail must keep the end");
            }
            other => panic!("expected Process, got {other:?}"),
        }
    }

    #[test]
    fn password_stdin_gets_single_trailing_newline() {
        assert_eq!(password_stdin_bytes("secret"), b"secret\n");
        assert_eq!(password_stdin_bytes("secret\n"), b"secret\n");
        assert_eq!(password_stdin_bytes("secret\n\n\n"), b"secret\n");
        assert_eq!(password_stdin_bytes("a\nb"), b"a\nb\n");
        assert_eq!(password_stdin_bytes("a\nb\n\n"), b"a\nb\n");
    }

    #[test]
    fn password_stdin_preserves_unicode() {
        // Review Focus: unicode + trailing newline piped correctly.
        assert_eq!(
            password_stdin_bytes("pässwörd🔑\n"),
            "pässwörd🔑\n".as_bytes()
        );
        assert_eq!(
            password_stdin_bytes("pässwörd🔑"),
            "pässwörd🔑\n".as_bytes()
        );
    }

    #[test]
    fn password_stdin_normalizes_crlf() {
        assert_eq!(password_stdin_bytes("secret\r\n"), b"secret\n");
    }

    #[test]
    fn password_accessors() {
        let runner = SudoRunner::new(false);
        assert!(!runner.is_root());
        assert!(!runner.has_password());
        runner.set_password("a".to_owned());
        assert!(runner.has_password());
        runner.clear();
        assert!(!runner.has_password());
        assert!(SudoRunner::new(true).is_root());
    }

    #[test]
    fn debug_never_leaks_password() {
        let runner = SudoRunner::new(false);
        runner.set_password("s3cret-pw".to_owned());
        let dbg = format!("{runner:?}");
        assert!(!dbg.contains("s3cret-pw"), "debug leaked password: {dbg}");
    }
}
