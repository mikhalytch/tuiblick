//! OpenVPN backend: spawn/kill + timestamped log ring (spec §5).
//!
//! - Up: `openvpn --config <stored .ovpn> --verb 3` as a long-lived
//!   `tokio::process::Child` with stdout+stderr piped and merged. MVP never
//!   passes `--daemon`; [`OpenVpnSession::has_daemon_directive`] sniffs the
//!   `.ovpn` so the UI can warn (a daemonized child would break log capture
//!   and SIGTERM shutdown).
//! - Reader tasks stamp every line `HH:MM:SS <line>` (local time) and push
//!   into a shared [`LogRing`] (`VecDeque`, cap 2000, oldest evicted).
//! - Down ([`OpenVpnSession::stop`]): SIGTERM via `libc::kill`, 3 s grace,
//!   then SIGKILL; exit code + end time are recorded on the session.
//! - Privilege split (spec §3): [`OpenVpnSession::spawn`] runs the backend
//!   directly (root mode: the TUI itself runs as root, plus the test path),
//!   while [`OpenVpnSession::spawn_privileged`] fronts the non-root path
//!   through [`SudoRunner`](crate::sudo::SudoRunner). Both share one
//!   supervision core. Non-root without a stored password fails fast with
//!   [`AppError::User`](crate::errors::AppError::User) — a spawn never hangs
//!   on a sudo prompt. Auth failures after spawn surface as sudo error lines
//!   in the captured stderr log, not as spawn errors.

use std::collections::VecDeque;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::Local;
use tokio::io::{AsyncBufReadExt, AsyncRead, BufReader};

use crate::errors::AppError;
use crate::sudo::SudoRunner;

/// Max buffered log lines; older lines are evicted first.
pub const LOG_CAP: usize = 2000;

/// SIGTERM grace in [`OpenVpnSession::stop`] before SIGKILL.
const STOP_GRACE: Duration = Duration::from_secs(3);

/// Capped log buffer with a separate *view* watermark.
///
/// - [`LogRing::push`] appends (evicting the oldest line beyond [`LOG_CAP`]).
/// - [`LogRing::lines`] / [`LogRing::len`] / [`LogRing::filtered`] expose the
///   **view**: buffered lines pushed after the last [`LogRing::clear_view`],
///   oldest → newest.
/// - [`LogRing::clear_view`] only moves the watermark to the current end; the
///   backing buffer is kept, so `s` (save raw buffer to `/tmp`) still sees
///   everything via [`LogRing::raw_lines`] / [`LogRing::backing_len`].
/// - [`LogRing::filtered`] is a case-sensitive substring match
///   (`str::contains`); the empty substring matches the whole view.
#[derive(Debug, Clone, Default)]
pub struct LogRing {
    buf: VecDeque<LogEntry>,
    next_seq: u64,
    view_floor: u64,
}

#[derive(Debug, Clone)]
struct LogEntry {
    seq: u64,
    line: String,
}

impl LogRing {
    /// Empty ring.
    pub fn new() -> Self {
        Self::default()
    }

    /// Append one line (reader tasks pass already-timestamped lines).
    /// Evicts the oldest line once the buffer exceeds [`LOG_CAP`].
    pub fn push(&mut self, line: String) {
        let seq = self.next_seq;
        self.next_seq = self.next_seq.saturating_add(1);
        self.buf.push_back(LogEntry { seq, line });
        while self.buf.len() > LOG_CAP {
            self.buf.pop_front();
        }
    }

    /// View lines (pushed after the last [`LogRing::clear_view`]),
    /// oldest → newest.
    pub fn lines(&self) -> Vec<String> {
        self.visible().map(|entry| entry.line.clone()).collect()
    }

    /// View lines containing `substr` (case-sensitive substring,
    /// `str::contains`), oldest → newest.
    pub fn filtered(&self, substr: &str) -> Vec<String> {
        self.visible()
            .filter(|entry| entry.line.contains(substr))
            .map(|entry| entry.line.clone())
            .collect()
    }

    /// Move the view watermark to the current end (the UI re-renders from
    /// here); the backing buffer is untouched.
    pub fn clear_view(&mut self) {
        self.view_floor = self.next_seq;
    }

    /// Number of lines in the view.
    pub fn len(&self) -> usize {
        self.visible().count()
    }

    /// True when the view holds no lines.
    /// (Only exercised by unit tests; the UI checks the filtered `Vec`.)
    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Number of lines in the backing buffer (ignores the view watermark).
    /// (Only exercised by unit tests; `s`-save goes through `raw_lines`.)
    #[allow(dead_code)]
    pub fn backing_len(&self) -> usize {
        self.buf.len()
    }

    /// All buffered lines, oldest → newest (ignores the view watermark).
    /// This is what `s` (save log to `/tmp`) writes.
    pub fn raw_lines(&self) -> Vec<String> {
        self.buf.iter().map(|entry| entry.line.clone()).collect()
    }

    fn visible(&self) -> impl Iterator<Item = &LogEntry> {
        self.buf.iter().filter(|entry| entry.seq >= self.view_floor)
    }
}

/// Long-lived foreground `openvpn` child + its log ring.
///
/// Receiver choice (brief says `stop(mut self)` / `take_logs()`): `stop`
/// takes `&mut self` and is idempotent, so the caller can
/// `stop().await` and *then* `take_logs()` (consuming). Dropping a session
/// without `stop` intentionally leaves the child running — spec §7
/// quit-leaves-up (`down` is explicit).
pub struct OpenVpnSession {
    child: tokio::process::Child,
    logs: Arc<Mutex<LogRing>>,
    readers: Vec<tokio::task::JoinHandle<()>>,
    // Diagnostic only (which command the session supervises); exit reporting
    // uses `exit_code` / `ended_at`.
    #[allow(dead_code)]
    cmd: String,
    stopped: bool,
    exit_code: Option<i32>,
    ended_at: Option<String>,
}

impl OpenVpnSession {
    /// Spawn `openvpn --config <config> --verb 3` directly.
    ///
    /// Root-mode path (the TUI itself runs as root: backends run directly,
    /// never through sudo) and the unit-test path. Stdin is nulled so the
    /// child can never read the terminal; a missing `openvpn` binary fails
    /// fast with [`AppError::Io`](crate::errors::AppError::Io).
    pub fn spawn(config: &Path) -> Result<Self, AppError> {
        let (prog, argv) = openvpn_command(config);
        let cmd = crate::sudo::display_cmd(&prog, &argv);
        let child = tokio::process::Command::new(&prog)
            .args(&argv)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .map_err(AppError::Io)?;
        Self::supervise(child, cmd)
    }

    /// Spawn through [`SudoRunner`](crate::sudo::SudoRunner) for non-root
    /// use (spec §3): `sudo -S -p <sentinel> openvpn --config <config>
    /// --verb 3`, password piped + stdin closed (mirrors the `sudo.rs`
    /// no-hang contract). Root mode delegates to [`OpenVpnSession::spawn`].
    /// Non-root with no stored password fails fast with
    /// [`AppError::User`](crate::errors::AppError::User) — never hangs.
    pub async fn spawn_privileged(runner: &SudoRunner, config: &Path) -> Result<Self, AppError> {
        if runner.is_root() {
            return Self::spawn(config);
        }
        let config_str = config.to_string_lossy().into_owned();
        let args = ["--config", config_str.as_str(), "--verb", "3"];
        let (program, argv) = crate::sudo::command_line(false, "openvpn", &args);
        let cmd = crate::sudo::display_cmd(&program, &argv);
        let child = runner.spawn_long_lived("openvpn", &args).await?;
        Self::supervise(child, cmd)
    }

    /// Stop the child: SIGTERM via `libc::kill`, 3 s grace, then SIGKILL.
    /// Records the exit code (`None` when killed by signal) + end time
    /// (RFC3339 UTC) and aborts the reader tasks. Idempotent: a second call
    /// is a no-op (tokio `wait` may only reap once).
    pub async fn stop(&mut self) {
        if self.stopped {
            return;
        }
        self.stopped = true;
        if let Some(pid) = self.child.id() {
            // SAFETY: `kill` with a plain signal number has no preconditions
            // beyond a valid pid; ESRCH (already exited) / EPERM are ignored
            // — `wait` below is the source of truth.
            unsafe {
                libc::kill(pid as i32, libc::SIGTERM);
            }
        }
        match tokio::time::timeout(STOP_GRACE, self.child.wait()).await {
            Ok(Ok(status)) => {
                self.exit_code = status.code();
            }
            Ok(Err(_)) => {}
            Err(_) => {
                // Grace expired: SIGKILL, then reap.
                let _ = self.child.kill().await;
                if let Ok(status) = self.child.wait().await {
                    self.exit_code = status.code();
                }
            }
        }
        self.ended_at = Some(chrono::Utc::now().to_rfc3339());
        for reader in &self.readers {
            reader.abort();
        }
    }

    /// Consume the session and return a snapshot of its log ring.
    /// (Stop first: `stop().await` borrows, `take_logs()` consumes.)
    pub fn take_logs(self) -> LogRing {
        self.snapshot()
    }

    /// Non-consuming snapshot of the current log ring (for the UI loop,
    /// which polls while the child is still running).
    pub fn snapshot(&self) -> LogRing {
        self.logs
            .lock()
            .map(|ring| ring.clone())
            .unwrap_or_default()
    }

    /// True when the `.ovpn` contains a bare `daemon` directive: the first
    /// whitespace-separated token of a line equals `daemon`
    /// (ASCII case-insensitive), so `daemon`, `DAEMON`, `daemon 1`, and
    /// `daemon  # comment` all count. Lines that are blank or start with
    /// `#` / `;` (after trimming) are comments and ignored. A
    /// missing/unreadable file reads as `false` — the caller already
    /// validated the import, so this is a warn path, not an error.
    pub fn has_daemon_directive(config: &Path) -> bool {
        let text = match std::fs::read_to_string(config) {
            Ok(text) => text,
            Err(_) => return false,
        };
        text.lines().any(|line| {
            let trimmed = line.trim();
            if trimmed.is_empty() || trimmed.starts_with('#') || trimmed.starts_with(';') {
                return false;
            }
            trimmed
                .split_whitespace()
                .next()
                .is_some_and(|first| first.eq_ignore_ascii_case("daemon"))
        })
    }

    /// Recorded exit code: `None` = killed by signal, or not stopped yet.
    pub fn exit_code(&self) -> Option<i32> {
        self.exit_code
    }

    /// RFC3339 UTC end time, set by [`OpenVpnSession::stop`].
    pub fn ended_at(&self) -> Option<String> {
        self.ended_at.clone()
    }

    /// Test-only helper: supervise an arbitrary command with the same core
    /// (stdout+stderr piped, merged timestamped readers). Tests must NEVER
    /// invoke real `openvpn`; typically `sleep`.
    #[cfg(test)]
    pub(crate) fn spawn_command_for_test(program: &str, args: &[&str]) -> Result<Self, AppError> {
        let argv: Vec<String> = args.iter().map(|s| (*s).to_owned()).collect();
        let cmd = crate::sudo::display_cmd(program, &argv);
        let child = tokio::process::Command::new(program)
            .args(&argv)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .map_err(AppError::Io)?;
        Self::supervise(child, cmd)
    }

    /// Shared supervision core: take the child's piped stdout+stderr and
    /// pump timestamped lines into a shared ring (tasks exit on EOF and are
    /// aborted by [`OpenVpnSession::stop`]).
    fn supervise(child: tokio::process::Child, cmd: String) -> Result<Self, AppError> {
        let logs = Arc::new(Mutex::new(LogRing::new()));
        let mut readers = Vec::new();
        let mut child = child;
        if let Some(stdout) = child.stdout.take() {
            readers.push(spawn_pipe_reader(stdout, Arc::clone(&logs)));
        }
        if let Some(stderr) = child.stderr.take() {
            readers.push(spawn_pipe_reader(stderr, Arc::clone(&logs)));
        }
        Ok(Self {
            child,
            logs,
            readers,
            cmd,
            stopped: false,
            exit_code: None,
            ended_at: None,
        })
    }
}

/// `(program, argv)` for a direct backend invocation (root mode):
/// `openvpn --config <path> --verb 3`. The path stays one argv element
/// (spaces/unicode safe, no shell). MVP never passes `--daemon`.
fn openvpn_command(config: &Path) -> (String, Vec<String>) {
    let path = config.to_string_lossy().into_owned();
    (
        "openvpn".to_owned(),
        vec![
            "--config".to_owned(),
            path,
            "--verb".to_owned(),
            "3".to_owned(),
        ],
    )
}

/// Pump one pipe (stdout or stderr) into the shared ring, prefixing every
/// line `HH:MM:SS <line>` (local time). Ends on EOF. The `std` mutex is held
/// only for the push itself (never across `.await`), so async readers are
/// never blocked.
fn spawn_pipe_reader<R>(pipe: R, logs: Arc<Mutex<LogRing>>) -> tokio::task::JoinHandle<()>
where
    R: AsyncRead + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        let mut lines = BufReader::new(pipe).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            let stamped = format!("{} {line}", Local::now().format("%H:%M:%S"));
            if let Ok(mut ring) = logs.lock() {
                ring.push(stamped);
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn ring_cap_evicts_oldest() {
        let mut ring = LogRing::new();
        for i in 0..2500 {
            ring.push(format!("line {i}"));
        }
        assert_eq!(ring.len(), 2000);
        assert_eq!(ring.backing_len(), 2000);
        let lines = ring.lines();
        assert_eq!(lines.len(), 2000);
        assert_eq!(lines[0], "line 500");
        assert_eq!(lines[1999], "line 2499");
    }

    #[test]
    fn filtered_is_case_sensitive_substring() {
        let mut ring = LogRing::new();
        ring.push("TLS handshake started".to_owned());
        ring.push("TLS Error: handshake failed".to_owned());
        ring.push("Initialization Sequence Completed".to_owned());
        let hits = ring.filtered("TLS");
        assert_eq!(hits.len(), 2);
        assert!(hits[0].contains("TLS handshake"));
        // Case-sensitive: lowercase matches nothing.
        assert!(ring.filtered("tls").is_empty());
        // Empty substring matches the whole view.
        assert_eq!(ring.filtered("").len(), 3);
    }

    #[test]
    fn clear_view_keeps_backing_buffer() {
        let mut ring = LogRing::new();
        ring.push("a".to_owned());
        ring.push("b".to_owned());
        ring.clear_view();
        assert_eq!(ring.len(), 0);
        assert!(ring.is_empty());
        assert_eq!(ring.backing_len(), 2);
        assert_eq!(ring.raw_lines().len(), 2);
        ring.push("c".to_owned());
        assert_eq!(ring.lines(), vec!["c".to_owned()]);
        assert_eq!(ring.backing_len(), 3);
    }

    fn temp_ovpn(name: &str, contents: &str) -> std::path::PathBuf {
        let path =
            std::env::temp_dir().join(format!("tuiblick-task5-{name}-{}", std::process::id()));
        std::fs::write(&path, contents).unwrap();
        path
    }

    #[test]
    fn daemon_directive_detected() {
        let path = temp_ovpn("daemon-yes", "client\ndev tun\ndaemon\nverb 3\n");
        assert!(OpenVpnSession::has_daemon_directive(&path));
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn daemon_directive_case_and_args() {
        for body in ["DAEMON\n", "daemon 1\n", "  daemon  # fork\n"] {
            let path = temp_ovpn("daemon-variants", body);
            assert!(
                OpenVpnSession::has_daemon_directive(&path),
                "body {body:?} should count"
            );
            std::fs::remove_file(&path).ok();
        }
    }

    #[test]
    fn daemon_directive_ignores_comments_and_missing() {
        let path = temp_ovpn(
            "daemon-no",
            "client\n#daemon\n; daemon\n  # daemon\nverb 3\n",
        );
        assert!(!OpenVpnSession::has_daemon_directive(&path));
        std::fs::remove_file(&path).ok();
        assert!(!OpenVpnSession::has_daemon_directive(&path));
        assert!(!OpenVpnSession::has_daemon_directive(std::path::Path::new(
            "/nonexistent-tuiblick/does-not-exist.ovpn"
        )));
    }

    #[tokio::test]
    async fn session_stop_kills_sleep_fast() {
        let mut sess = OpenVpnSession::spawn_command_for_test("sleep", &["30"]).unwrap();
        let start = std::time::Instant::now();
        sess.stop().await;
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "SIGTERM should end sleep well before grace expiry"
        );
        assert!(sess.ended_at().is_some());
    }

    #[tokio::test]
    async fn session_records_exit_code_of_exited_child() {
        // SIGTERM-ing a just-spawned short-lived child would usually kill it
        // before it exits on its own (racy), so: spawn `sleep 0.1`, let it
        // die naturally into an unreaped zombie, then stop must reap the
        // natural exit code (kill on a zombie is a no-op success/ESRCH,
        // ignored either way — `wait` is the source of truth).
        let mut sess = OpenVpnSession::spawn_command_for_test("sleep", &["0.1"]).unwrap();
        tokio::time::sleep(Duration::from_secs(1)).await;
        sess.stop().await;
        assert_eq!(sess.exit_code(), Some(0));
        assert!(sess.ended_at().is_some());
    }

    #[tokio::test]
    async fn session_stop_is_idempotent() {
        let mut sess = OpenVpnSession::spawn_command_for_test("sleep", &["30"]).unwrap();
        sess.stop().await;
        sess.stop().await;
        assert!(sess.ended_at().is_some());
    }

    #[tokio::test]
    async fn session_captures_stdout_and_stderr_merged() {
        let mut sess = OpenVpnSession::spawn_command_for_test("echo", &["hello-ovpn"]).unwrap();
        let mut saw_out = false;
        for _ in 0..100 {
            tokio::time::sleep(Duration::from_millis(50)).await;
            if sess
                .snapshot()
                .lines()
                .iter()
                .any(|l| l.contains("hello-ovpn"))
            {
                saw_out = true;
                break;
            }
        }
        assert!(saw_out, "stdout line never reached the ring");

        let mut sess2 =
            OpenVpnSession::spawn_command_for_test("ls", &["/nonexistent-tuiblick-xyz"]).unwrap();
        let mut saw_err = false;
        for _ in 0..100 {
            tokio::time::sleep(Duration::from_millis(50)).await;
            if sess2
                .snapshot()
                .lines()
                .iter()
                .any(|l| l.contains("nonexistent-tuiblick-xyz"))
            {
                saw_err = true;
                break;
            }
        }
        assert!(saw_err, "stderr line never reached the ring");
        sess.stop().await;
        sess2.stop().await;
        let logs = sess.take_logs();
        assert!(
            logs.lines().iter().any(|l| l.contains("hello-ovpn")),
            "take_logs must carry the captured stdout line"
        );
        let logs2 = sess2.take_logs();
        assert!(
            logs2
                .lines()
                .iter()
                .any(|l| l.contains("nonexistent-tuiblick-xyz")),
            "take_logs must carry the captured stderr line"
        );
    }
}
