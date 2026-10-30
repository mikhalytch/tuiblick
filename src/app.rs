//! App state machine + event loop (spec §7).
//!
//! State mapping to the spec §7 states:
//!
//! - `Profiles` = `phase == ConnPhase::Idle` with `popup == Popup::None`
//! - `Connecting` / `Connected` / `Disconnecting` = [`ConnPhase`] directly
//!   (`Connected` always carries at least one [`ActiveConn`]; `Idle` implies none)
//! - `ErrorPopup | HelpPopup | SudoPrompt | ImportPrompt | ConfirmPopup` =
//!   [`Popup`] overlays on top of whatever `phase` is current
//!
//! Concurrency: the loop owns `App` outright (Elm-style). All backend work
//! runs in spawned `tokio` tasks that report back over an mpsc channel
//! ([`BackendMsg`]); the render tick never awaits a child. `SudoRunner` is
//! shared via `Arc` (its methods already take `&self`) so tasks can run
//! privileged commands without ever holding the loop's `&mut App`.
//!
//! Events: crossterm events arrive from a dedicated thread running a
//! blocking `event::poll`/`event::read` loop, forwarded over a second mpsc
//! channel. `EventStream` was deliberately NOT used: consuming it needs
//! `StreamExt::next` from `futures`/`tokio-stream`, which are not
//! dependencies, and no new deps are allowed. The thread exits promptly on
//! quit: it polls with a 200 ms timeout and watches `sender.is_closed()`.
//!
//! Key resolutions worth knowing:
//!
//! - `c` connects from the list pane, but over a focused OpenVPN detail pane
//!   it clears the log *view* (spec §5), never the backing buffer — so both
//!   spec meanings of `c` hold, disambiguated by [`Focus`].
//! - `1` selects the list pane, `2`/`3` the detail pane, Tab toggles.
//! - `q`/`?`/Enter/Esc dismiss `Help`; `q`/Enter/Esc dismiss `Error`.
//!   In a confirm popup Enter means No (the safe default).
//! - Ctrl-C always goes through the quit flow (confirm when active).
//!
//! Not detected in MVP: an OpenVPN child that dies on its own (e.g.
//! `kill -9`) — `openvpn.rs` exposes no non-consuming poll, so the UI keeps
//! showing the last logs until the next explicit `d`. Flagged for Task 9.

// The render-facing surface (`header_line`, `help_text`, `ovpn_view`,
// `ActiveConn::started_at`, …) is consumed by `ui`.

use std::collections::VecDeque;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use tokio::sync::mpsc::{self, UnboundedSender};

use crate::awg::AwgSummary;
use crate::config::{self, Profile, VpnType};
use crate::errors::AppError;
use crate::net::RouteInfo;
use crate::openvpn::{LogRing, OpenVpnSession};
use crate::sudo::{
    contains_auth_marker, is_root_process, probe_cached, SudoRunner, SUDO_PROMPT_SENTINEL,
};

/// Render tick: log-cache refresh + redraw (never awaits a child).
const RENDER_TICK: Duration = Duration::from_millis(200);
/// AWG status poll, only while the active tunnel is AWG (spec §4).
const AWG_POLL_INTERVAL: Duration = Duration::from_secs(3);
/// Config-dir rescan: external adds/removes are picked up automatically,
/// so there is no manual rescan key (`r` renames the selected profile).
const RESCAN_INTERVAL: Duration = Duration::from_secs(5);
/// Blocking event-thread poll quantum (also bounds thread shutdown delay).
const EVENT_POLL_TIMEOUT: Duration = Duration::from_millis(200);
/// Detail-pane scroll steps for `j/k` vs PgUp/PgDn.
const SCROLL_STEP: isize = 1;
const PAGE_STEP: isize = 10;
/// Quit confirmation (spec §7, verbatim). Shared with `ui` rendering.
pub(crate) const QUIT_CONFIRM_MSG: &str = "active tunnel will stay up after quit. Quit anyway? y/N";

/// Active tunnel runtime state (never persisted to disk, spec §6).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActiveConn {
    pub profile_id: String,
    pub kind: VpnType,
    pub started_at: String,
    pub last_exit_ip: Option<String>,
    /// True when the tunnel was already up before this session (startup
    /// adoption of a lingering interface) rather than started here. The
    /// UI badges it; everything else (poll, `d`, batch skip, exit IP)
    /// treats it like any active tunnel.
    pub adopted: bool,
}

/// Connection phase: spec states `Profiles | Connecting | Connected |
/// Disconnecting` (`Profiles` is `Idle` with no popup).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ConnPhase {
    #[default]
    Idle,
    Connecting,
    Connected,
    Disconnecting,
}

/// Which pane receives `j/k` (spec §7: `1/2/3` or Tab switches focus).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Focus {
    #[default]
    List,
    Detail,
}

/// What the confirm popup is asking about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfirmKind {
    Quit,
    Switch {
        target: usize,
    },
    Delete {
        profile_id: String,
        display_name: String,
    },
}

/// Popup overlays: spec states `ErrorPopup | HelpPopup | SudoPrompt |
/// ImportPrompt | ConfirmPopup` (`None` = no overlay). `Rename` is an
/// addition for the `r` (rename profile) prompt.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Popup {
    #[default]
    None,
    Help,
    Sudo,
    Import,
    Rename,
    Confirm(ConfirmKind),
    Error(String),
}

/// Pure decision of [`App::request_connect`]: the caller performs the effect,
/// so unit tests assert the state transition without spawning any backend.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConnectAction {
    StartNow(usize),
    SudoPrompt(usize),
    SwitchConfirm(usize),
    Ignored(String),
}

/// Pure decision of [`App::request_quit`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuitAction {
    QuitNow,
    ConfirmQuit,
}

/// Pure outcome of answering a confirm popup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfirmEffect {
    Quit,
    DeleteProfile(String),
    SwitchTo(usize),
    Nothing,
}

/// Terminal action of a tunnel drain: quit the app, or stay open.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DrainMode {
    Quit,
    Stay,
}

/// Outcome of a sudo-auth failure: re-prompt once, then surface an error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SudoFailure {
    Reprompt,
    Fatal,
}

/// One side (initial/current) of the exit-IP panel (spec §6).
/// `None` = probe still in flight (UI shows `resolving…`);
/// failures are stored as `Some("unknown")` / `Some("n/a")`.
#[derive(Debug, Clone, Default)]
pub struct NetSnapshot {
    pub ip: Option<String>,
    pub gateway: Option<String>,
    pub iface: Option<String>,
}

/// Full TUI state. `sudo` is shared (`Arc`) so spawned backend tasks can run
/// privileged commands without the render loop ever awaiting a child.
pub struct App {
    pub profiles: Vec<Profile>,
    pub selected: usize,
    /// Live tunnels: at most one per backend, so one AWG and one OpenVPN
    /// tunnel may be up together. `Connected` implies non-empty.
    pub active: Vec<ActiveConn>,
    pub sudo: Arc<SudoRunner>,
    pub phase: ConnPhase,
    pub focus: Focus,
    pub popup: Popup,
    pub status_message: String,
    pub sudo_input: String,
    pub sudo_error: Option<String>,
    pub sudo_reprompted: bool,
    pub sudo_cached: Option<bool>,
    pub pending_connect: Option<usize>,
    pub auto_connect: Option<usize>,
    /// Drain flow (`Q` quit-all / `D` disconnect-all): newest tunnel first
    /// until none are left. `Quit` exits at the end, `Stay` reports and
    /// keeps the app open. Set at flow start, cleared on finish or
    /// disconnect failure (which aborts the flow and surfaces the error).
    pub drain: Option<DrainMode>,
    /// `C` flow (connect-all): remaining profile IDs in list order,
    /// non-empty only while a batch is running.
    pub batch_queue: VecDeque<String>,
    /// `C` flow is in progress (queue runs down, or the last item's
    /// bring-up is still in flight).
    pub batch_active: bool,
    /// `C` flow counters for the finish report.
    pub batch_done: usize,
    pub batch_skipped: usize,
    pub conn_gen: u64,
    pub import_input: String,
    pub import_vpn: VpnType,
    pub rename_input: String,
    pub filter_editing: bool,
    pub filter_input: String,
    pub log_filter: String,
    pub log_follow: bool,
    pub detail_scroll: usize,
    /// Scroll offset of the AWG status panel (per-panel: the OpenVPN log
    /// view uses `detail_scroll` + `log_follow` instead).
    pub awg_scroll: usize,
    /// Tail-pin of the AWG status panel, mirroring `log_follow` for the log
    /// view (`f` toggles whichever panel is selected).
    pub awg_follow: bool,
    pub awg_status: String,
    pub awg_summary: Option<AwgSummary>,
    pub awg_poll_inflight: bool,
    pub ovpn_session: Option<OpenVpnSession>,
    pub ovpn_logs: LogRing,
    pub ovpn_daemon_warn: bool,
    pub ovpn_auth_flagged: bool,
    pub initial: NetSnapshot,
    pub current: NetSnapshot,
    pub should_quit: bool,
}

impl App {
    /// Load profiles (`scan_and_reconcile`) and build the initial state.
    /// The sudo-cache probe and net probes run as background tasks in
    /// [`run`], never here, so construction never blocks.
    pub fn new() -> Result<Self, AppError> {
        let profiles = config::scan_and_reconcile()?;
        Ok(Self {
            profiles,
            selected: 0,
            active: Vec::new(),
            sudo: Arc::new(SudoRunner::new(is_root_process())),
            phase: ConnPhase::Idle,
            focus: Focus::List,
            popup: Popup::None,
            status_message: "ready — ? for help, q to quit".to_owned(),
            sudo_input: String::new(),
            sudo_error: None,
            sudo_reprompted: false,
            sudo_cached: None,
            pending_connect: None,
            auto_connect: None,
            drain: None,
            batch_queue: VecDeque::new(),
            batch_active: false,
            batch_done: 0,
            batch_skipped: 0,
            conn_gen: 0,
            import_input: String::new(),
            import_vpn: VpnType::Awg,
            rename_input: String::new(),
            filter_editing: false,
            filter_input: String::new(),
            log_filter: String::new(),
            log_follow: true,
            detail_scroll: 0,
            awg_scroll: 0,
            awg_follow: true,
            awg_status: String::new(),
            awg_summary: None,
            awg_poll_inflight: false,
            ovpn_session: None,
            ovpn_logs: LogRing::new(),
            ovpn_daemon_warn: false,
            ovpn_auth_flagged: false,
            initial: NetSnapshot::default(),
            current: NetSnapshot::default(),
            should_quit: false,
        })
    }

    /// Sudo is needed only when neither root nor holding a password.
    pub fn needs_sudo_prompt(&self) -> bool {
        !self.sudo.is_root() && !self.sudo.has_password()
    }

    /// Status-bar sudo segment: `root | sudo cached | sudo needed` (spec §3).
    /// A stored password counts as cached: no prompt will appear.
    pub fn sudo_status_text(&self) -> &'static str {
        if self.sudo.is_root() {
            "root"
        } else if self.sudo.has_password() || self.sudo_cached == Some(true) {
            "sudo cached"
        } else {
            "sudo needed"
        }
    }

    pub fn selected_profile(&self) -> Option<&Profile> {
        self.profiles.get(self.selected)
    }

    /// Live tunnel of one backend, if any (at most one per backend).
    pub fn active_of_kind(&self, kind: VpnType) -> Option<&ActiveConn> {
        self.active.iter().find(|a| a.kind == kind)
    }

    /// True when the profile's own tunnel is up.
    pub fn is_profile_active(&self, id: &str) -> bool {
        self.active.iter().any(|a| a.profile_id == id)
    }

    /// Clamped list movement (no-op on an empty list).
    pub fn move_selection(&mut self, delta: isize) {
        if self.profiles.is_empty() {
            self.selected = 0;
            return;
        }
        let last = self.profiles.len() - 1;
        let next = (self.selected as isize + delta).clamp(0, last as isize);
        self.selected = next as usize;
    }

    /// Re-clamp after the list shrinks (auto-rescan / delete).
    pub fn clamp_selection(&mut self) {
        if self.profiles.is_empty() {
            self.selected = 0;
        } else if self.selected >= self.profiles.len() {
            self.selected = self.profiles.len() - 1;
        }
    }

    /// Saturating detail-pane scroll on the *selected* panel (matches the
    /// highlight): the AWG status panel moves `awg_scroll`; the OpenVPN log
    /// view moves `detail_scroll` and any manual scroll drops `log_follow`
    /// (standard pager behavior — otherwise the keys visibly do nothing
    /// while follow pins the tail).
    pub fn scroll_detail(&mut self, delta: isize) {
        if self.is_awg_selected() {
            self.awg_follow = false;
            let next = (self.awg_scroll as isize + delta).max(0);
            self.awg_scroll = next as usize;
        } else {
            self.log_follow = false;
            let next = (self.detail_scroll as isize + delta).max(0);
            self.detail_scroll = next as usize;
        }
    }

    /// Pure connect decision: switch-confirm when the same backend already
    /// has a tunnel up, sudo prompt when privilege is missing, immediate
    /// start otherwise (the other backend being up is fine).
    /// Never spawns: the caller executes the returned action.
    pub fn request_connect(&mut self, index: usize) -> ConnectAction {
        let Some(profile) = self.profiles.get(index) else {
            return ConnectAction::Ignored("no profile selected".to_owned());
        };
        if self.is_profile_active(&profile.id) {
            return ConnectAction::Ignored("already connected to this profile".to_owned());
        }
        if self.active_of_kind(profile.vpn_type).is_some() {
            self.pending_connect = None;
            self.popup = Popup::Confirm(ConfirmKind::Switch { target: index });
            return ConnectAction::SwitchConfirm(index);
        }
        if self.needs_sudo_prompt() {
            self.pending_connect = Some(index);
            self.sudo_reprompted = false;
            self.sudo_error = None;
            self.sudo_input.clear();
            self.popup = Popup::Sudo;
            return ConnectAction::SudoPrompt(index);
        }
        ConnectAction::StartNow(index)
    }

    /// Pure quit decision: confirm when any tunnel is up (quit leaves them
    /// up, spec §7), immediate quit otherwise.
    pub fn request_quit(&mut self) -> QuitAction {
        if self.active.is_empty() {
            self.should_quit = true;
            QuitAction::QuitNow
        } else {
            self.popup = Popup::Confirm(ConfirmKind::Quit);
            QuitAction::ConfirmQuit
        }
    }

    /// Pure confirm resolution: clears the popup and returns the effect for
    /// the caller to execute (quit flag / delete / switch-disconnect chain).
    pub fn resolve_confirm(&mut self, yes: bool) -> ConfirmEffect {
        let Popup::Confirm(kind) = self.popup.clone() else {
            return ConfirmEffect::Nothing;
        };
        self.popup = Popup::None;
        self.pending_connect = None;
        if !yes {
            return ConfirmEffect::Nothing;
        }
        match kind {
            ConfirmKind::Quit => {
                self.should_quit = true;
                ConfirmEffect::Quit
            }
            ConfirmKind::Switch { target } => ConfirmEffect::SwitchTo(target),
            ConfirmKind::Delete { profile_id, .. } => ConfirmEffect::DeleteProfile(profile_id),
        }
    }

    /// First auth failure re-prompts once (the runner already dropped the
    /// stored password); the second surfaces an error popup.
    pub fn sudo_auth_failed(&mut self) -> SudoFailure {
        if self.sudo_reprompted {
            self.pending_connect = None;
            self.sudo_input.clear();
            self.sudo_error = None;
            // A batch cannot survive this: nothing will ever resume it.
            batch_clear(self);
            self.popup =
                Popup::Error("sudo authentication failed twice — tunnel not started".to_owned());
            SudoFailure::Fatal
        } else {
            self.sudo_reprompted = true;
            self.sudo_input.clear();
            self.sudo_error = Some("sudo authentication failed — try again".to_owned());
            self.popup = Popup::Sudo;
            SudoFailure::Reprompt
        }
    }

    pub fn toggle_follow(&mut self) {
        if self.is_awg_selected() {
            self.awg_follow = !self.awg_follow;
        } else {
            self.log_follow = !self.log_follow;
        }
    }

    /// Follow state of the selected panel (for the `f` status message).
    /// Returns the panel label plus the state: `("awg", true)`, `("log", …)`.
    pub fn follow_state(&self) -> (&'static str, bool) {
        if self.is_awg_selected() {
            ("awg", self.awg_follow)
        } else {
            ("log", self.log_follow)
        }
    }

    /// True when the selected profile belongs to the AWG backend.
    fn is_awg_selected(&self) -> bool {
        self.selected_profile()
            .is_some_and(|p| p.vpn_type == VpnType::Awg)
    }

    pub fn apply_filter(&mut self) {
        self.log_filter.clone_from(&self.filter_input);
        self.filter_editing = false;
        self.detail_scroll = 0;
    }

    /// OpenVPN log view with the substring filter applied.
    pub fn ovpn_view(&self) -> Vec<String> {
        if self.log_filter.is_empty() {
            self.ovpn_logs.lines()
        } else {
            self.ovpn_logs.filtered(&self.log_filter)
        }
    }

    /// Header line for Task 8: `init <ip> (<gw>/<if>) → exit <ip>` (spec §6).
    pub fn header_line(&self) -> String {
        let init_ip = ip_display(&self.initial.ip);
        let gateway = route_display(&self.initial.gateway);
        let iface = route_display(&self.initial.iface);
        let exit_ip = ip_display(&self.current.ip);
        format!("init {init_ip} ({gateway}/{iface}) → exit {exit_ip}")
    }
}

/// Reject blank import paths; `~`-expansion + existence checks live in
/// `config::import_file`.
pub fn clean_import_path(raw: &str) -> Result<String, String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        Err("import path is empty".to_owned())
    } else {
        Ok(trimmed.to_owned())
    }
}

/// Backend implied by the typed path's extension: `.ovpn` → OpenVPN,
/// `.conf` → AWG, anything else → `None` (leave the switch alone, so a
/// manual Tab choice survives).
pub fn infer_import_vpn(path: &str) -> Option<VpnType> {
    let ext = Path::new(path.trim_end())
        .extension()
        .map(|ext| ext.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    match ext.as_str() {
        "ovpn" => Some(VpnType::OpenVpn),
        "conf" => Some(VpnType::Awg),
        _ => None,
    }
}

/// The backend switch follows the typed path: a trailing `.ovpn`/`.conf`
/// flips it, anything else keeps the current (possibly toggled) choice.
/// Runs on every text edit — never on Tab, so a manual toggle sticks
/// until the next keystroke.
fn follow_input_backend(app: &mut App) {
    if let Some(vpn) = infer_import_vpn(&app.import_input) {
        app.import_vpn = vpn;
    }
}

/// `None` (probe in flight) renders as `resolving…`.
pub fn ip_display(value: &Option<String>) -> &str {
    value.as_deref().unwrap_or("resolving…")
}

/// `None` (probe in flight) renders as `resolving…`.
pub fn route_display(value: &Option<String>) -> &str {
    value.as_deref().unwrap_or("resolving…")
}

/// Key map text (spec §7) for the `?` popup; Task 8 renders this.
pub fn help_text() -> &'static str {
    "tuiblick — keys\n\
     j/k, ↑/↓      move in list / scroll detail\n\
     Ctrl+j/k       move selected profile down/up (order persists)\n\
     Tab, 1/2/3    switch pane focus (list / detail)\n\
     Enter, c       connect selected (c clears the OpenVPN log view when the detail pane is focused)\n\
     C              connect all in list order (skips busy/missing, stops on error)\n\
     d              disconnect selected profile's tunnel\n\
     D              disconnect all tunnels (newest first), stay open\n\
     r              rename selected profile (config rescan is automatic)\n\
     i              import file (.ovpn/.conf flips backend, Tab toggles)\n\
     Del,Bksp       delete profile (confirm)\n\
     /              filter openvpn logs\n\
     f              follow toggle (selected panel)\n\
     s              save openvpn log to /tmp\n\
     PgUp/PgDn      scroll detail (leaves log-follow)\n\
     ?              this help\n\
     q, Ctrl-C      quit (active tunnel stays up; confirm if active)\n\
     Q              disconnect all tunnels (newest first), then quit\n\
     y/N            answer confirm popups   Esc  close popup / cancel input\n\
     sudo password is asked once per session (status: root | sudo cached | sudo needed)"
}

/// Messages from spawned backend tasks back to the event loop.
/// `gen` is the [`App::conn_gen`] at spawn time; stale messages are ignored
/// (or cleaned up: a late success brings its own tunnel back down).
#[allow(clippy::enum_variant_names)]
enum BackendMsg {
    SudoProbe(bool),
    InitialRoute(Result<RouteInfo, String>),
    InitialIp(Result<String, String>),
    CurrentRoute {
        gen: u64,
        result: Result<RouteInfo, String>,
    },
    CurrentIp {
        gen: u64,
        result: Result<String, String>,
    },
    ExitIp {
        gen: u64,
        profile_id: String,
        result: Result<String, String>,
    },
    AwgUp {
        gen: u64,
        profile_id: String,
        result: Result<String, AppError>,
    },
    AwgDown {
        gen: u64,
        cleanup: bool,
        result: Result<String, AppError>,
    },
    AwgPoll {
        gen: u64,
        result: Result<String, AppError>,
    },
    AdoptAwg(Result<String, AppError>),
    OvpnSpawn {
        gen: u64,
        profile_id: String,
        result: Box<Result<OpenVpnSession, AppError>>,
    },
    OvpnDown {
        gen: u64,
        cleanup: bool,
        logs: LogRing,
        exit_code: Option<i32>,
        ended_at: Option<String>,
    },
}

/// Event loop: owns `App`, drives backend tasks + `crate::ui::render`.
/// The terminal (raw mode + alternate screen) is set up by `main`.
/// `connect_all` runs the `C` batch once at startup (same flow as pressing
/// `C`: skips busy/missing, stops on the first real failure).
pub async fn run(connect_all: bool) -> anyhow::Result<()> {
    let mut app = App::new().map_err(anyhow::Error::from)?;

    let (etx, mut erx) = mpsc::unbounded_channel::<Event>();
    {
        let sender = etx.clone();
        tokio::task::spawn_blocking(move || event_thread(sender));
    }
    drop(etx);

    let (btx, mut brx) = mpsc::unbounded_channel::<BackendMsg>();

    // Sudo-cache probe: `sudo -n true` is synchronous by design, so it runs
    // on the blocking pool and reports back without stalling input.
    {
        let tx = btx.clone();
        tokio::task::spawn_blocking(move || {
            let _ = tx.send(BackendMsg::SudoProbe(probe_cached()));
        });
    }
    // Lingering-tunnel adoption (Linux, AWG only): quit leaves tunnels up,
    // so a reopen may find live interfaces with a clean runtime state.
    // Attempted only when passwordless (root / stored / cached) — the scan
    // never prompts just to look.
    {
        let tx = btx.clone();
        let sudo = Arc::clone(&app.sudo);
        tokio::spawn(async move {
            if let Some(result) = try_adopt_show(&sudo).await {
                let _ = tx.send(BackendMsg::AdoptAwg(result));
            }
        });
    }
    spawn_initial_probes(&btx);

    let mut terminal =
        ratatui::Terminal::new(ratatui::backend::CrosstermBackend::new(std::io::stdout()))?;
    let mut render_tick = tokio::time::interval(RENDER_TICK);
    render_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut awg_tick = tokio::time::interval(AWG_POLL_INTERVAL);
    awg_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut rescan_tick = tokio::time::interval(RESCAN_INTERVAL);
    rescan_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    if connect_all {
        start_connect_all(&mut app, &btx);
    }

    loop {
        tokio::select! {
            Some(event) = erx.recv() => handle_crossterm_event(&mut app, &btx, event),
            Some(msg) = brx.recv() => handle_backend_msg(&mut app, &btx, msg),
            _ = render_tick.tick() => {
                refresh_ovpn_cache(&mut app);
                sniff_ovpn_auth(&mut app);
                terminal.draw(|frame| crate::ui::render(frame, &app))?;
            }
            _ = awg_tick.tick() => maybe_poll_awg(&mut app, &btx),
            _ = rescan_tick.tick() => auto_rescan(&mut app),
        }
        if app.should_quit {
            break;
        }
    }
    // Password lives in memory only: zeroize on quit (spec §3).
    app.sudo.clear();
    Ok(())
}

/// Passwordless `awg show` for startup adoption (Linux only): `Some` when
/// the check can run without prompting (root, stored password, or cached
/// sudo timestamp), `None` when it would need a prompt. The timestamp
/// probe is synchronous by design, so it runs on the blocking pool like
/// the startup sudo-cache probe.
#[cfg(target_os = "linux")]
async fn try_adopt_show(sudo: &SudoRunner) -> Option<Result<String, AppError>> {
    if sudo.is_root() || sudo.has_password() {
        return Some(crate::awg::show(sudo).await);
    }
    let cached = tokio::task::spawn_blocking(probe_cached)
        .await
        .unwrap_or(false);
    if cached {
        Some(crate::awg::show(sudo).await)
    } else {
        None
    }
}

/// Passwordless `awg show` for startup adoption: always `None` off Linux,
/// where live interface names cannot map to profiles (`utunN` is
/// system-assigned, not the config stem).
#[cfg(not(target_os = "linux"))]
async fn try_adopt_show(_sudo: &SudoRunner) -> Option<Result<String, AppError>> {
    None
}

/// Dedicated event thread: blocking `poll`/`read`, forwarded over `tx`.
/// Exits when the receiver is gone or `read` fails.
fn event_thread(tx: UnboundedSender<Event>) {
    loop {
        if tx.is_closed() {
            break;
        }
        match crossterm::event::poll(EVENT_POLL_TIMEOUT) {
            Ok(true) => match crossterm::event::read() {
                Ok(event) => {
                    if tx.send(event).is_err() {
                        break;
                    }
                }
                Err(_) => break,
            },
            Ok(false) => {}
            Err(_) => break,
        }
    }
}

fn handle_crossterm_event(app: &mut App, tx: &UnboundedSender<BackendMsg>, event: Event) {
    // Resize needs no action: the next render tick redraws.
    if let Event::Key(key) = event {
        handle_key(app, tx, key);
    }
}

fn handle_key(app: &mut App, tx: &UnboundedSender<BackendMsg>, key: KeyEvent) {
    if key.kind == KeyEventKind::Release {
        return;
    }
    // Ctrl-C quits (confirming first when a tunnel is active), from anywhere.
    if key.modifiers.contains(KeyModifiers::CONTROL)
        && matches!(key.code, KeyCode::Char('c') | KeyCode::Char('C'))
    {
        do_quit(app);
        return;
    }
    match app.popup.clone() {
        Popup::Sudo => handle_sudo_key(app, tx, key),
        Popup::Import => handle_import_key(app, key),
        Popup::Rename => handle_rename_key(app, key),
        Popup::Confirm(_) => handle_confirm_key(app, tx, key),
        Popup::Help => {
            if matches!(
                key.code,
                KeyCode::Esc | KeyCode::Enter | KeyCode::Char('q') | KeyCode::Char('?')
            ) {
                app.popup = Popup::None;
            }
        }
        Popup::Error(_) => {
            if matches!(key.code, KeyCode::Esc | KeyCode::Enter | KeyCode::Char('q')) {
                app.popup = Popup::None;
            }
        }
        Popup::None => {
            if app.filter_editing {
                handle_filter_key(app, key);
            } else {
                handle_normal_key(app, tx, key);
            }
        }
    }
}

/// Case-sensitive single-letter map (spec §7: `d` disconnects, `D` deletes).
/// Ctrl+j/k reorder the selected profile (verified against crossterm 0.28:
/// they arrive as `Char('j'/'k')` + CONTROL, distinct from Enter/j/k).
/// Other Ctrl/Alt chords (except Ctrl-C, handled earlier) are ignored.
fn handle_normal_key(app: &mut App, tx: &UnboundedSender<BackendMsg>, key: KeyEvent) {
    if key.modifiers.contains(KeyModifiers::CONTROL) {
        match key.code {
            KeyCode::Char('j') | KeyCode::Char('J') => {
                move_selected_profile(app, 1);
            }
            KeyCode::Char('k') | KeyCode::Char('K') => {
                move_selected_profile(app, -1);
            }
            _ => {}
        }
        return;
    }
    if key.modifiers.contains(KeyModifiers::ALT) {
        return;
    }
    match key.code {
        KeyCode::Up | KeyCode::Char('k') => move_or_scroll(app, -SCROLL_STEP),
        KeyCode::Down | KeyCode::Char('j') => move_or_scroll(app, SCROLL_STEP),
        KeyCode::PageUp => app.scroll_detail(-PAGE_STEP),
        KeyCode::PageDown => app.scroll_detail(PAGE_STEP),
        KeyCode::Tab | KeyCode::BackTab => {
            app.focus = match app.focus {
                Focus::List => Focus::Detail,
                Focus::Detail => Focus::List,
            };
        }
        KeyCode::Char('1') => app.focus = Focus::List,
        KeyCode::Char('2') | KeyCode::Char('3') => app.focus = Focus::Detail,
        KeyCode::Enter => connect_selected(app, tx),
        KeyCode::Char('c') => {
            if app.focus == Focus::Detail && app.active.iter().any(|a| a.kind == VpnType::OpenVpn) {
                app.ovpn_logs.clear_view();
                if let Some(session) = app.ovpn_session.as_ref() {
                    session.clear_view();
                }
                set_status(
                    app,
                    "log view cleared (backing buffer kept — s still saves all)",
                );
            } else {
                connect_selected(app, tx);
            }
        }
        KeyCode::Char('d') => do_disconnect(app, tx),
        KeyCode::Char('D') => do_disconnect_all(app, tx),
        KeyCode::Delete | KeyCode::Backspace => open_delete_confirm(app),
        KeyCode::Char('C') => start_connect_all(app, tx),
        KeyCode::Char('r') => open_rename(app),
        KeyCode::Char('i') => open_import(app),
        KeyCode::Char('/') => {
            app.filter_editing = true;
            app.filter_input.clone_from(&app.log_filter);
        }
        KeyCode::Char('f') => {
            app.toggle_follow();
            let (panel, on) = app.follow_state();
            set_status(
                app,
                if on {
                    format!("{panel} follow on")
                } else {
                    format!("{panel} follow off")
                },
            );
        }
        KeyCode::Char('s') => save_log(app),
        KeyCode::Char('?') => app.popup = Popup::Help,
        KeyCode::Char('q') => do_quit(app),
        KeyCode::Char('Q') => do_quit_all(app, tx),
        KeyCode::Esc => {}
        _ => {}
    }
}

/// `j/k` move the list, or scroll the detail pane when it is focused.
fn move_or_scroll(app: &mut App, delta: isize) {
    match app.focus {
        Focus::List => app.move_selection(delta),
        Focus::Detail => app.scroll_detail(delta),
    }
}

fn handle_sudo_key(app: &mut App, tx: &UnboundedSender<BackendMsg>, key: KeyEvent) {
    match key.code {
        KeyCode::Esc => {
            app.sudo_input.clear();
            app.sudo_error = None;
            app.pending_connect = None;
            app.popup = Popup::None;
            if app.batch_active {
                batch_clear(app);
                set_status(app, "connect-all cancelled");
            } else {
                set_status(app, "sudo cancelled — connect aborted");
            }
        }
        KeyCode::Enter => submit_sudo(app, tx),
        KeyCode::Backspace => {
            app.sudo_input.pop();
        }
        _ => {
            // Printable chars only: the password may contain anything, but
            // Ctrl/Alt chords are not text.
            push_text_key(&mut app.sudo_input, key);
        }
    }
}

fn handle_import_key(app: &mut App, key: KeyEvent) {
    match key.code {
        KeyCode::Esc => {
            app.popup = Popup::None;
            set_status(app, "import cancelled");
        }
        KeyCode::Enter => submit_import(app),
        KeyCode::Tab | KeyCode::BackTab => {
            app.import_vpn = match app.import_vpn {
                VpnType::Awg => VpnType::OpenVpn,
                VpnType::OpenVpn => VpnType::Awg,
            };
        }
        KeyCode::Backspace => {
            app.import_input.pop();
            follow_input_backend(app);
        }
        _ => {
            if push_text_key(&mut app.import_input, key) {
                follow_input_backend(app);
            }
        }
    }
}

fn handle_confirm_key(app: &mut App, tx: &UnboundedSender<BackendMsg>, key: KeyEvent) {
    match key.code {
        KeyCode::Char('y') | KeyCode::Char('Y') => {
            let effect = app.resolve_confirm(true);
            exec_confirm(app, tx, effect);
        }
        // Enter is the safe default (No); `q`/Esc also cancel.
        KeyCode::Char('n')
        | KeyCode::Char('N')
        | KeyCode::Char('q')
        | KeyCode::Enter
        | KeyCode::Esc => {
            let _ = app.resolve_confirm(false);
            set_status(app, "cancelled");
        }
        _ => {}
    }
}

fn handle_filter_key(app: &mut App, key: KeyEvent) {
    match key.code {
        KeyCode::Esc => {
            app.filter_editing = false;
        }
        KeyCode::Enter => app.apply_filter(),
        KeyCode::Backspace => {
            app.filter_input.pop();
        }
        _ => {
            push_text_key(&mut app.filter_input, key);
        }
    }
}

/// Push printable characters (no Ctrl/Alt held) into a text buffer.
/// Returns whether anything was pushed.
fn push_text_key(buf: &mut String, key: KeyEvent) -> bool {
    if let KeyCode::Char(c) = key.code {
        if !key.modifiers.contains(KeyModifiers::CONTROL)
            && !key.modifiers.contains(KeyModifiers::ALT)
        {
            buf.push(c);
            return true;
        }
    }
    false
}

fn connect_selected(app: &mut App, tx: &UnboundedSender<BackendMsg>) {
    match app.phase {
        ConnPhase::Connecting | ConnPhase::Disconnecting => {
            set_status(app, "operation in progress…");
        }
        ConnPhase::Idle | ConnPhase::Connected => {
            let action = app.request_connect(app.selected);
            act_on_connect(app, tx, action);
        }
    }
}

fn act_on_connect(app: &mut App, tx: &UnboundedSender<BackendMsg>, action: ConnectAction) {
    match action {
        ConnectAction::StartNow(target) => start_connect(app, tx, target),
        // SudoPrompt / SwitchConfirm already set their popup in `request_connect`.
        ConnectAction::SudoPrompt(_) | ConnectAction::SwitchConfirm(_) => {}
        ConnectAction::Ignored(message) => set_status(app, message),
    }
}

/// `C`: connect every profile in list order, one at a time. Each item goes
/// through the same decision as a manual `c` (sudo prompt when needed);
/// same-backend-busy and missing-file items are skipped, any real failure
/// stops the batch. Queue holds profile IDs (not indices) so a rescan,
/// reorder, or delete mid-batch cannot misfire a connect.
fn start_connect_all(app: &mut App, tx: &UnboundedSender<BackendMsg>) {
    match app.phase {
        ConnPhase::Connecting | ConnPhase::Disconnecting => {
            set_status(app, "operation in progress…");
            return;
        }
        ConnPhase::Idle | ConnPhase::Connected => {}
    }
    let queue = batch_queue_for(app);
    if queue.is_empty() {
        set_status(
            app,
            if app.profiles.is_empty() {
                "no profiles to connect"
            } else {
                "everything already connected"
            },
        );
        return;
    }
    let total = queue.len();
    app.batch_queue = queue;
    app.batch_active = true;
    app.batch_done = 0;
    app.batch_skipped = 0;
    set_status(app, format!("connect-all: 0/{total}…"));
    batch_next(app, tx);
}

/// Profiles still needing a tunnel, in list order. Pure (no spawning),
/// so unit tests pin the skip-already-up rule directly.
fn batch_queue_for(app: &App) -> VecDeque<String> {
    app.profiles
        .iter()
        .filter(|p| !app.is_profile_active(&p.id))
        .map(|p| p.id.clone())
        .collect()
}

/// True when a bring-up failure means the interface is already up outside
/// this session (quit-leaves-up + reopen): only `Process` errors carrying
/// the backend's "already exists" tail count — auth, I/O, and any other
/// backend failure stay fatal to the batch.
fn is_lingering_interface(e: &AppError) -> bool {
    match e {
        AppError::Process { tail, .. } => crate::awg::is_already_exists_tail(tail),
        _ => false,
    }
}

/// Config-file stem of an AWG profile (`awg/awg0.conf` → `awg0`): on Linux
/// the live interface IS this stem, which is what makes attribution exact.
/// `None` for non-UTF8 paths (never adopted, never panics).
fn awg_interface_stem(profile: &Profile) -> Option<&str> {
    std::path::Path::new(&profile.stored_file)
        .file_stem()
        .and_then(|stem| stem.to_str())
}

/// Adopt one lingering AWG tunnel into runtime state (startup adoption).
/// Matches live interface names against AWG profile stored stems in list
/// order and adopts the first match — at most one tunnel per backend —
/// unless that backend is already active. Seeds phase, status dump, and
/// summary from the adoption dump so the panel and the 3 s poll work at
/// once; the caller resamples routes/exit IP. Failures and empty matches
/// are silent (`None`): nothing was found, nothing changes. Pure over the
/// app state (no spawning) so unit tests pin the mapping directly.
fn adopt_lingering_awg(app: &mut App, result: Result<String, AppError>) -> Option<String> {
    let raw = result.ok()?;
    if app.active_of_kind(VpnType::Awg).is_some() {
        return None;
    }
    let interfaces = crate::awg::parse_interface_names(&raw);
    let profile = app.profiles.iter().find(|p| {
        p.vpn_type == VpnType::Awg
            && interfaces
                .iter()
                .any(|name| Some(name.as_str()) == awg_interface_stem(p))
    })?;
    let id = profile.id.clone();
    let name = profile.display_name.clone();
    app.active.push(ActiveConn {
        profile_id: id.clone(),
        kind: VpnType::Awg,
        started_at: "unknown (adopted)".to_owned(),
        last_exit_ip: None,
        adopted: true,
    });
    app.phase = ConnPhase::Connected;
    app.awg_status = raw;
    app.awg_summary = Some(crate::awg::parse_summary(&app.awg_status));
    set_status(
        app,
        format!("adopted lingering tunnel {name} (started outside this session)"),
    );
    Some(id)
}

/// One batch step: pop items until one spawns/pauses the batch, or the
/// queue runs dry (finish). Returns what the caller must do — only the
/// `Spawn` arm touches backends, so tests cover every other path without
/// spawning processes.
fn batch_step(app: &mut App) -> BatchStep {
    loop {
        let Some(id) = app.batch_queue.front().cloned() else {
            batch_finish(app);
            return BatchStep::Done;
        };
        let Some(index) = app.profiles.iter().position(|p| p.id == id) else {
            // Deleted mid-batch: drop silently, the list no longer has it.
            app.batch_queue.pop_front();
            continue;
        };
        let profile = &app.profiles[index];
        if profile.is_missing() {
            // `start_connect` would refuse without spawning (and stall the
            // batch: popped item, no completion ever). Skip up front.
            app.batch_queue.pop_front();
            app.batch_skipped += 1;
            continue;
        }
        match app.request_connect(index) {
            ConnectAction::StartNow(target) => {
                app.batch_queue.pop_front();
                return BatchStep::Spawn(target);
            }
            ConnectAction::SudoPrompt(_) => {
                // Popup is up; the batch pauses until submit/Esc/failure.
                app.batch_queue.pop_front();
                return BatchStep::Wait;
            }
            ConnectAction::SwitchConfirm(_) => {
                // Same backend already up: never auto-switch in batch (that
                // would tear down the tunnel just connected). Clear the
                // popup `request_connect` raised and skip.
                app.popup = Popup::None;
                app.batch_queue.pop_front();
                app.batch_skipped += 1;
                continue;
            }
            ConnectAction::Ignored(_) => {
                // Came up between queue build and now (manual connect
                // during a sudo pause): already what the batch wants.
                app.batch_queue.pop_front();
                continue;
            }
        }
    }
}

/// Outcome of [`batch_step`]: what the batch driver must do next.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BatchStep {
    /// Bring up this profile index now (the only arm that spawns).
    Spawn(usize),
    /// Sudo prompt is up; completions/cancel/failure resume the batch.
    Wait,
    /// Queue drained; batch finished (status already set).
    Done,
}

/// Drive the batch forward: one step, spawning at most one bring-up.
/// Called at `C` start and from every successful connect completion.
fn batch_next(app: &mut App, tx: &UnboundedSender<BackendMsg>) {
    match batch_step(app) {
        BatchStep::Spawn(target) => start_connect(app, tx, target),
        BatchStep::Wait | BatchStep::Done => {}
    }
}

/// Queue drained: report and stand down. Counts reset for the next run.
fn batch_finish(app: &mut App) {
    app.batch_active = false;
    let (done, skipped) = (app.batch_done, app.batch_skipped);
    app.batch_queue.clear();
    app.batch_done = 0;
    app.batch_skipped = 0;
    set_status(
        app,
        if skipped == 0 {
            format!("connect-all finished: {done} connected")
        } else {
            format!("connect-all finished: {done} connected, {skipped} skipped")
        },
    );
}

/// Stand the batch down silently (caller sets its own status/error).
fn batch_clear(app: &mut App) {
    app.batch_active = false;
    app.batch_queue.clear();
    app.batch_done = 0;
    app.batch_skipped = 0;
}

fn exec_confirm(app: &mut App, tx: &UnboundedSender<BackendMsg>, effect: ConfirmEffect) {
    match effect {
        ConfirmEffect::Quit | ConfirmEffect::Nothing => {}
        ConfirmEffect::DeleteProfile(id) => do_delete_profile(app, &id),
        ConfirmEffect::SwitchTo(target) => {
            let same_kind_up = app
                .profiles
                .get(target)
                .is_some_and(|p| app.active_of_kind(p.vpn_type).is_some());
            if same_kind_up && app.phase == ConnPhase::Connected {
                // Switch = stop the old same-backend tunnel, then auto-connect
                // the target when the down-task reports back. The other
                // backend's tunnel (if any) is untouched.
                app.auto_connect = Some(target);
                let kind = app.profiles[target].vpn_type;
                do_disconnect_kind(app, tx, kind);
            } else {
                let action = app.request_connect(target);
                act_on_connect(app, tx, action);
            }
        }
    }
}

fn do_quit(app: &mut App) {
    // Quit never stops the tunnel (spec §7): it stays up after exit.
    if app.request_quit() == QuitAction::ConfirmQuit {
        set_status(app, QUIT_CONFIRM_MSG);
    }
}

/// `Q`: disconnect every live tunnel, newest first, then quit. The chain
/// runs through the normal down-completions (`drain_next`); any disconnect
/// failure aborts the quit and surfaces the error instead of silently
/// leaving a tunnel up.
fn do_quit_all(app: &mut App, tx: &UnboundedSender<BackendMsg>) {
    start_drain(app, tx, DrainMode::Quit);
}

/// `D`: disconnect every live tunnel, newest first, and stay open.
/// Same chain as `Q` without the exit at the end; pressing either key
/// during a drain retargets it (last key wins).
fn do_disconnect_all(app: &mut App, tx: &UnboundedSender<BackendMsg>) {
    start_drain(app, tx, DrainMode::Stay);
}

/// Begin (or retarget) a tunnel drain. A switch or connect-all batch in
/// progress is abandoned: its target must not come up while everything
/// is being brought down.
fn start_drain(app: &mut App, tx: &UnboundedSender<BackendMsg>, mode: DrainMode) {
    app.auto_connect = None;
    batch_clear(app);
    match app.phase {
        ConnPhase::Disconnecting => {
            // An op is already in flight; join the flow — its completion
            // continues the chain from the remaining tunnels.
            app.drain = Some(mode);
            set_status(app, "disconnecting all…");
        }
        _ => {
            if app.phase == ConnPhase::Connecting {
                // Cancel the in-flight bring-up; a late arrival is cleaned
                // up by the message handler (brought back down).
                app.conn_gen = app.conn_gen.wrapping_add(1);
                app.pending_connect = None;
            }
            if app.active.is_empty() {
                app.drain = None;
                match mode {
                    DrainMode::Quit => app.should_quit = true,
                    DrainMode::Stay => set_status(app, "no active tunnels"),
                }
            } else {
                app.drain = Some(mode);
                drain_next(app, tx);
            }
        }
    }
}

/// One drain step: bring down the newest remaining tunnel, or run the
/// terminal action when none are left. Only called with no other
/// transitional operation in flight, so the kind disconnect always spawns.
fn drain_next(app: &mut App, tx: &UnboundedSender<BackendMsg>) {
    match app.active.last().cloned() {
        Some(newest) => {
            set_status(
                app,
                format!("disconnecting {}…", profile_name(app, &newest.profile_id)),
            );
            do_disconnect_kind(app, tx, newest.kind);
        }
        None => match app.drain {
            Some(DrainMode::Quit) => {
                app.drain = None;
                app.should_quit = true;
            }
            _ => {
                app.drain = None;
                set_status(app, "all tunnels disconnected");
            }
        },
    }
}

/// Spawn the backend bring-up. Synchronous: it only reads state and spawns
/// tasks; results arrive as [`BackendMsg`].
fn start_connect(app: &mut App, tx: &UnboundedSender<BackendMsg>, target: usize) {
    let Some(profile) = app.profiles.get(target).cloned() else {
        set_status(app, "no profile selected");
        return;
    };
    if profile.is_missing() {
        show_error(
            app,
            format!(
                "config file missing: {} — re-import or delete (Del)",
                profile.stored_path().display()
            ),
        );
        return;
    }
    app.pending_connect = Some(target);
    app.conn_gen = app.conn_gen.wrapping_add(1);
    let gen = app.conn_gen;
    app.phase = ConnPhase::Connecting;
    set_status(app, format!("connecting to {}…", profile.display_name));
    match profile.vpn_type {
        VpnType::Awg => {
            app.awg_summary = None;
            let task_tx = tx.clone();
            let sudo = Arc::clone(&app.sudo);
            let path = profile.stored_path();
            let profile_id = profile.id.clone();
            tokio::spawn(async move {
                let result = crate::awg::up(&sudo, &path).await;
                let _ = task_tx.send(BackendMsg::AwgUp {
                    gen,
                    profile_id,
                    result,
                });
            });
        }
        VpnType::OpenVpn => {
            app.ovpn_daemon_warn = false;
            app.ovpn_auth_flagged = false;
            let path = profile.stored_path();
            if OpenVpnSession::has_daemon_directive(&path) {
                app.ovpn_daemon_warn = true;
                set_status(
                    app,
                    format!(
                        "warning: {} contains a daemon directive — staying foreground; connecting…",
                        profile.display_name
                    ),
                );
            }
            let task_tx = tx.clone();
            let sudo = Arc::clone(&app.sudo);
            let profile_id = profile.id.clone();
            tokio::spawn(async move {
                let result = if sudo.is_root() {
                    OpenVpnSession::spawn(&path)
                } else {
                    OpenVpnSession::spawn_privileged(&sudo, &path).await
                };
                let _ = task_tx.send(BackendMsg::OvpnSpawn {
                    gen,
                    profile_id,
                    result: Box::new(result),
                });
            });
        }
    }
}

/// Recompute the global phase from the live tunnels. Transitional phases
/// are set by their own operations; settling lands on `Connected` when
/// anything is still up, `Idle` otherwise.
fn settle_phase(app: &mut App) {
    app.phase = if app.active.is_empty() {
        ConnPhase::Idle
    } else {
        ConnPhase::Connected
    };
}

/// `d` key: disconnect the *selected* profile's tunnel. A connect in flight
/// can only be cancelled when the selected profile is its target; the other
/// backend's tunnel is never touched.
fn do_disconnect(app: &mut App, tx: &UnboundedSender<BackendMsg>) {
    match app.phase {
        ConnPhase::Connecting => {
            if app.pending_connect == Some(app.selected) {
                // Cancel just this bring-up; its late arrival is cleaned up
                // in the message handler. The other tunnel (if any) stays up.
                // A batch cannot survive the cancel: nothing would resume it.
                app.conn_gen = app.conn_gen.wrapping_add(1);
                settle_phase(app);
                app.pending_connect = None;
                app.auto_connect = None;
                if app.batch_active {
                    batch_clear(app);
                    set_status(app, "connect-all cancelled");
                } else {
                    set_status(app, "connect cancelled");
                }
            } else {
                set_status(app, "operation in progress…");
            }
        }
        ConnPhase::Disconnecting => set_status(app, "already disconnecting…"),
        ConnPhase::Idle => set_status(app, "no active tunnel"),
        ConnPhase::Connected => {
            let kind = app
                .selected_profile()
                .filter(|p| app.is_profile_active(&p.id))
                .map(|p| p.vpn_type);
            match kind {
                Some(kind) => do_disconnect_kind(app, tx, kind),
                None => set_status(app, "selected profile is not connected"),
            }
        }
    }
}

/// Bring down the live tunnel of one backend (plus the switch chain behind
/// it). Operations stay serialized: callers only get here when no other
/// transitional operation is in flight.
fn do_disconnect_kind(app: &mut App, tx: &UnboundedSender<BackendMsg>, kind: VpnType) {
    let Some(active) = app.active.iter().find(|a| a.kind == kind).cloned() else {
        settle_phase(app);
        set_status(app, "no active tunnel");
        return;
    };
    match app.phase {
        ConnPhase::Connecting => {
            app.conn_gen = app.conn_gen.wrapping_add(1);
            settle_phase(app);
            app.pending_connect = None;
            app.auto_connect = None;
            set_status(app, "connect cancelled");
        }
        ConnPhase::Disconnecting => set_status(app, "already disconnecting…"),
        ConnPhase::Idle => set_status(app, "no active tunnel"),
        ConnPhase::Connected => {
            app.conn_gen = app.conn_gen.wrapping_add(1);
            let gen = app.conn_gen;
            app.phase = ConnPhase::Disconnecting;
            let name = profile_name(app, &active.profile_id);
            set_status(app, format!("disconnecting {name}…"));
            match active.kind {
                VpnType::Awg => {
                    let Some(profile) = app.profiles.iter().find(|p| p.id == active.profile_id)
                    else {
                        settle_phase(app);
                        show_error(
                            app,
                            "profile entry gone — tunnel may still be up".to_owned(),
                        );
                        return;
                    };
                    let task_tx = tx.clone();
                    let sudo = Arc::clone(&app.sudo);
                    let path = profile.stored_path();
                    tokio::spawn(async move {
                        let result = crate::awg::down(&sudo, &path).await;
                        let _ = task_tx.send(BackendMsg::AwgDown {
                            gen,
                            cleanup: false,
                            result,
                        });
                    });
                }
                VpnType::OpenVpn => match app.ovpn_session.take() {
                    Some(session) => {
                        let task_tx = tx.clone();
                        tokio::spawn(async move {
                            let mut session = session;
                            session.stop().await;
                            let exit_code = session.exit_code();
                            let ended_at = session.ended_at();
                            let logs = session.take_logs();
                            let _ = task_tx.send(BackendMsg::OvpnDown {
                                gen,
                                cleanup: false,
                                logs,
                                exit_code,
                                ended_at,
                            });
                        });
                    }
                    None => {
                        settle_phase(app);
                        show_error(
                            app,
                            "openvpn session handle missing — tunnel may still be up".to_owned(),
                        );
                    }
                },
            }
        }
    }
}

fn open_delete_confirm(app: &mut App) {
    let Some(profile) = app.selected_profile().cloned() else {
        set_status(app, "no profile to delete");
        return;
    };
    if app.active.iter().any(|a| a.profile_id == profile.id) {
        show_error(
            app,
            format!(
                "cannot delete \"{}\" while its tunnel is up — disconnect (d) first",
                profile.display_name
            ),
        );
        return;
    }
    if is_pending_connect_target(app, &profile.id) {
        show_error(
            app,
            format!(
                "cannot delete \"{}\" while it is connecting — cancel first",
                profile.display_name
            ),
        );
        return;
    }
    app.popup = Popup::Confirm(ConfirmKind::Delete {
        profile_id: profile.id,
        display_name: profile.display_name,
    });
}

fn do_delete_profile(app: &mut App, id: &str) {
    if app.active.iter().any(|a| a.profile_id == id) {
        show_error(
            app,
            "cannot delete the active tunnel's profile — disconnect (d) first".to_owned(),
        );
        return;
    }
    if is_pending_connect_target(app, id) {
        show_error(
            app,
            "cannot delete profile while it is connecting — cancel first".to_owned(),
        );
        return;
    }
    match config::delete_profile(id) {
        Ok(()) => {
            app.profiles.retain(|p| p.id != id);
            app.clamp_selection();
            set_status(app, "profile deleted");
        }
        Err(e) => show_error(app, format!("delete failed: {e}")),
    }
}

/// True when `id` is the in-flight connect target (phase Connecting with
/// `pending_connect` pointing at it). Deleting it would orphan the tunnel
/// that is about to come up, so both delete paths block it.
fn is_pending_connect_target(app: &App, id: &str) -> bool {
    app.pending_connect
        .and_then(|i| app.profiles.get(i))
        .is_some_and(|p| p.id == id)
}

fn open_import(app: &mut App) {
    app.import_vpn = app
        .selected_profile()
        .map(|p| p.vpn_type)
        .unwrap_or(VpnType::Awg);
    app.import_input.clear();
    app.popup = Popup::Import;
}

/// Open the rename prompt for the selected profile, prefilled with its
/// current display name. Only the mapping changes (stored file untouched).
fn open_rename(app: &mut App) {
    let Some(profile) = app.selected_profile().cloned() else {
        set_status(app, "no profile to rename");
        return;
    };
    app.rename_input = profile.display_name;
    app.popup = Popup::Rename;
}

fn handle_rename_key(app: &mut App, key: KeyEvent) {
    match key.code {
        KeyCode::Esc => {
            app.popup = Popup::None;
            set_status(app, "rename cancelled");
        }
        KeyCode::Enter => submit_rename(app),
        KeyCode::Backspace => {
            app.rename_input.pop();
        }
        _ => {
            push_text_key(&mut app.rename_input, key);
        }
    }
}

fn submit_rename(app: &mut App) {
    let Some(profile) = app.selected_profile().cloned() else {
        app.popup = Popup::None;
        set_status(app, "no profile to rename");
        return;
    };
    let new_name = app.rename_input.trim().to_owned();
    if new_name.is_empty() {
        show_error(app, "profile name is empty".to_owned());
        return;
    }
    match config::rename_profile(&profile.id, &new_name) {
        Ok(()) => {
            if let Some(entry) = app.profiles.iter_mut().find(|p| p.id == profile.id) {
                entry.display_name = new_name.clone();
            }
            app.rename_input.clear();
            app.popup = Popup::None;
            set_status(app, format!("renamed to {new_name}"));
        }
        Err(e) => show_error(app, format!("rename failed: {e}")),
    }
}

/// Ctrl+j/k: move the selected profile one slot down/up. The order is the
/// list order and is persisted (`profiles.yaml`); selection follows the
/// moved profile. Safe mid-connection: tunnels are referenced by id.
fn move_selected_profile(app: &mut App, delta: isize) {
    let Some(profile) = app.selected_profile().cloned() else {
        set_status(app, "no profile to move");
        return;
    };
    match config::move_profile(&profile.id, delta) {
        Ok((profiles, moved)) => {
            app.profiles = profiles;
            match moved {
                Some(index) => {
                    app.selected = index;
                    set_status(
                        app,
                        format!(
                            "moved {} {}",
                            profile.display_name,
                            if delta > 0 { "down" } else { "up" }
                        ),
                    );
                }
                None => set_status(
                    app,
                    format!(
                        "{} is already at the {}",
                        profile.display_name,
                        if delta > 0 { "bottom" } else { "top" }
                    ),
                ),
            }
        }
        Err(e) => show_error(app, format!("reorder failed: {e}")),
    }
}

fn submit_import(app: &mut App) {
    let cleaned = match clean_import_path(&app.import_input.clone()) {
        Ok(path) => path,
        Err(e) => {
            show_error(app, e);
            return;
        }
    };
    let vpn = app.import_vpn;
    match config::import_file(Path::new(&cleaned), vpn) {
        Ok(profile) => match config::scan_and_reconcile() {
            Ok(profiles) => {
                let id = profile.id.clone();
                let name = profile.display_name.clone();
                app.profiles = profiles;
                if let Some(index) = app.profiles.iter().position(|p| p.id == id) {
                    app.selected = index;
                }
                app.import_input.clear();
                app.popup = Popup::None;
                set_status(app, format!("imported {name}"));
            }
            Err(e) => show_error(app, format!("import saved but rescan failed: {e}")),
        },
        Err(e) => show_error(app, format!("import failed: {e}")),
    }
}

fn submit_sudo(app: &mut App, tx: &UnboundedSender<BackendMsg>) {
    if app.sudo_input.is_empty() {
        app.sudo_error = Some("password required".to_owned());
        return;
    }
    let password = std::mem::take(&mut app.sudo_input);
    app.sudo.set_password(password);
    app.sudo_error = None;
    app.popup = Popup::None;
    if let Some(target) = app.pending_connect.take() {
        let action = app.request_connect(target);
        act_on_connect(app, tx, action);
    }
}

fn save_log(app: &mut App) {
    let lines = app.ovpn_logs.raw_lines();
    if lines.is_empty() {
        set_status(app, "no openvpn log to save");
        return;
    }
    let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S");
    let path = format!("/tmp/tuiblick-openvpn-{stamp}.log");
    match std::fs::write(&path, lines.join("\n")) {
        Ok(()) => set_status(app, format!("saved {} lines to {path}", lines.len())),
        Err(e) => show_error(app, format!("save failed: {e}")),
    }
}

/// Automatic rescan: picks up files added/removed outside the app and
/// refreshes the `(missing)` badges. Runs on a timer (no manual key);
/// stays quiet unless the list actually changed, so it never spams the
/// status bar. Safe mid-connection: entries are only appended or flagged,
/// never dropped, and selection is re-clamped.
fn auto_rescan(app: &mut App) {
    match config::scan_and_reconcile() {
        Ok(profiles) => {
            if profiles != app.profiles {
                let count = profiles.len();
                app.profiles = profiles;
                app.clamp_selection();
                set_status(app, format!("auto-rescan: {count} profiles"));
            }
        }
        Err(e) => set_status(app, format!("auto-rescan failed: {e}")),
    }
}

fn handle_backend_msg(app: &mut App, tx: &UnboundedSender<BackendMsg>, msg: BackendMsg) {
    match msg {
        BackendMsg::SudoProbe(cached) => {
            app.sudo_cached = Some(cached);
        }
        BackendMsg::InitialRoute(result) => match result {
            Ok(info) => {
                app.initial.gateway = Some(info.gateway);
                app.initial.iface = Some(info.iface);
            }
            Err(e) => {
                app.initial.gateway = Some("n/a".to_owned());
                app.initial.iface = Some("n/a".to_owned());
                set_status(app, format!("route probe failed: {e}"));
            }
        },
        BackendMsg::InitialIp(result) => match result {
            Ok(ip) => {
                app.initial.ip = Some(ip);
            }
            Err(e) => {
                app.initial.ip = Some("unknown".to_owned());
                set_status(app, format!("exit-IP probe failed: {e}"));
            }
        },
        BackendMsg::CurrentRoute { gen, result } => {
            if gen != app.conn_gen {
                return;
            }
            match result {
                Ok(info) => {
                    app.current.gateway = Some(info.gateway);
                    app.current.iface = Some(info.iface);
                }
                Err(e) => {
                    app.current.gateway = Some("n/a".to_owned());
                    app.current.iface = Some("n/a".to_owned());
                    set_status(app, format!("route probe failed: {e}"));
                }
            }
        }
        BackendMsg::CurrentIp { gen, result } => {
            if gen != app.conn_gen {
                return;
            }
            match result {
                Ok(ip) => {
                    app.current.ip = Some(ip);
                }
                Err(e) => {
                    app.current.ip = Some("unknown".to_owned());
                    set_status(app, format!("exit-IP probe failed: {e}"));
                }
            }
        }
        BackendMsg::ExitIp {
            gen,
            profile_id,
            result,
        } => {
            if gen != app.conn_gen {
                return;
            }
            let for_active = app.active.iter().any(|a| a.profile_id == profile_id);
            if !for_active {
                return;
            }
            match result {
                Ok(ip) => {
                    if let Some(active) = app.active.iter_mut().find(|a| a.profile_id == profile_id)
                    {
                        active.last_exit_ip = Some(ip);
                    }
                }
                Err(e) => {
                    if let Some(active) = app.active.iter_mut().find(|a| a.profile_id == profile_id)
                    {
                        active.last_exit_ip = Some("unknown".to_owned());
                    }
                    set_status(app, format!("exit-IP probe failed: {e}"));
                }
            }
        }
        BackendMsg::AwgUp {
            gen,
            profile_id,
            result,
        } => complete_awg_up(app, tx, gen, profile_id, result),
        BackendMsg::AwgDown {
            gen,
            cleanup,
            result,
        } => complete_awg_down(app, tx, gen, cleanup, result),
        BackendMsg::AwgPoll { gen, result } => complete_awg_poll(app, gen, result),
        BackendMsg::AdoptAwg(result) => {
            if let Some(id) = adopt_lingering_awg(app, result) {
                let gen = app.conn_gen;
                spawn_current_resample(tx, gen);
                spawn_exit_ip(tx, gen, id);
            }
        }
        BackendMsg::OvpnSpawn {
            gen,
            profile_id,
            result,
        } => complete_ovpn_spawn(app, tx, gen, profile_id, *result),
        BackendMsg::OvpnDown {
            gen,
            cleanup,
            logs,
            exit_code,
            ended_at,
        } => complete_ovpn_down(app, tx, gen, cleanup, logs, exit_code, ended_at),
    }
}

fn complete_awg_up(
    app: &mut App,
    tx: &UnboundedSender<BackendMsg>,
    gen: u64,
    profile_id: String,
    result: Result<String, AppError>,
) {
    if gen != app.conn_gen {
        // Late arrival after cancel/disconnect: the tunnel just came up
        // behind our back — bring it down so no orphan stays up.
        if result.is_ok() {
            if let Some(profile) = app.profiles.iter().find(|p| p.id == profile_id) {
                let task_tx = tx.clone();
                let sudo = Arc::clone(&app.sudo);
                let path = profile.stored_path();
                tokio::spawn(async move {
                    let result = crate::awg::down(&sudo, &path).await;
                    let _ = task_tx.send(BackendMsg::AwgDown {
                        gen,
                        cleanup: true,
                        result,
                    });
                });
                set_status(app, "late connect finished — bringing the tunnel down");
            }
        }
        return;
    }
    match result {
        Ok(output) => {
            let name = profile_name(app, &profile_id);
            app.phase = ConnPhase::Connected;
            app.pending_connect = None;
            app.sudo_reprompted = false;
            // One tunnel per backend: the switch flow always brings the old
            // same-kind tunnel down first, but retain-then-push keeps the
            // invariant even if two bring-ups ever raced.
            app.active.retain(|a| a.kind != VpnType::Awg);
            app.active.push(ActiveConn {
                profile_id: profile_id.clone(),
                kind: VpnType::Awg,
                started_at: now_rfc3339(),
                last_exit_ip: None,
                adopted: false,
            });
            app.awg_status = output;
            app.awg_summary = Some(crate::awg::parse_summary(&app.awg_status));
            set_status(app, format!("connected: {name}"));
            spawn_current_resample(tx, gen);
            spawn_exit_ip(tx, gen, profile_id);
            if app.batch_active {
                app.batch_done += 1;
                batch_next(app, tx);
            }
        }
        Err(AppError::SudoAuth) => {
            settle_phase(app);
            match app.sudo_auth_failed() {
                SudoFailure::Reprompt => set_status(app, "sudo authentication failed — try again"),
                SudoFailure::Fatal => set_status(app, "sudo authentication failed twice"),
            }
        }
        Err(e) => {
            settle_phase(app);
            app.pending_connect = None;
            if app.batch_active {
                let failed = profile_name(app, &profile_id);
                if is_lingering_interface(&e) {
                    // Quit leaves tunnels up by design, so after a reopen
                    // the runtime state is clean while the interface
                    // lingers: `awg-quick up` answers "already exists".
                    // That is a busy backend, not a failure — skip like a
                    // runtime-known busy backend and keep the batch going.
                    app.batch_skipped += 1;
                    batch_next(app, tx);
                } else {
                    batch_clear(app);
                    show_error(app, format!("connect-all stopped at {failed}: {e}"));
                }
            } else {
                show_error(app, format!("connect failed: {e}"));
            }
        }
    }
}

fn complete_awg_down(
    app: &mut App,
    tx: &UnboundedSender<BackendMsg>,
    gen: u64,
    cleanup: bool,
    result: Result<String, AppError>,
) {
    if cleanup {
        match result {
            Ok(_) => set_status(app, "late tunnel brought down"),
            Err(e) => set_status(app, format!("late cleanup failed: {e}")),
        }
        return;
    }
    if gen != app.conn_gen {
        return;
    }
    match result {
        Ok(_) => {
            app.active.retain(|a| a.kind != VpnType::Awg);
            settle_phase(app);
            set_status(app, "disconnected");
            spawn_current_resample(tx, gen);
            if let Some(target) = app.auto_connect.take() {
                let action = app.request_connect(target);
                act_on_connect(app, tx, action);
            }
            if app.drain.is_some() {
                drain_next(app, tx);
            }
        }
        Err(AppError::SudoAuth) => {
            // The down path cannot reuse the connect re-prompt slot, so it
            // surfaces directly; the tunnel stays marked up (honest).
            app.auto_connect = None;
            app.drain = None;
            settle_phase(app);
            show_error(
                app,
                "sudo authentication failed during disconnect — tunnel may still be up".to_owned(),
            );
        }
        Err(e) => {
            app.auto_connect = None;
            app.drain = None;
            settle_phase(app);
            show_error(
                app,
                format!("disconnect failed (tunnel may still be up): {e}"),
            );
        }
    }
}

fn complete_awg_poll(app: &mut App, gen: u64, result: Result<String, AppError>) {
    app.awg_poll_inflight = false;
    let current = gen == app.conn_gen
        && app.phase == ConnPhase::Connected
        && app.active.iter().any(|a| a.kind == VpnType::Awg);
    if !current {
        return;
    }
    match result {
        Ok(raw) => {
            app.awg_status = raw;
            app.awg_summary = Some(crate::awg::parse_summary(&app.awg_status));
        }
        Err(e) => set_status(app, format!("awg status poll failed: {e}")),
    }
}

fn complete_ovpn_spawn(
    app: &mut App,
    tx: &UnboundedSender<BackendMsg>,
    gen: u64,
    profile_id: String,
    result: Result<OpenVpnSession, AppError>,
) {
    if gen != app.conn_gen {
        // Late spawn after cancel/disconnect: stop it at once, then drop.
        if let Ok(session) = result {
            let task_tx = tx.clone();
            tokio::spawn(async move {
                let mut session = session;
                session.stop().await;
                let exit_code = session.exit_code();
                let ended_at = session.ended_at();
                let logs = session.take_logs();
                let _ = task_tx.send(BackendMsg::OvpnDown {
                    gen,
                    cleanup: true,
                    logs,
                    exit_code,
                    ended_at,
                });
            });
            set_status(app, "late openvpn spawn stopped");
        }
        return;
    }
    match result {
        Ok(session) => {
            let name = profile_name(app, &profile_id);
            app.ovpn_session = Some(session);
            app.phase = ConnPhase::Connected;
            app.pending_connect = None;
            app.sudo_reprompted = false;
            app.active.retain(|a| a.kind != VpnType::OpenVpn);
            app.active.push(ActiveConn {
                profile_id: profile_id.clone(),
                kind: VpnType::OpenVpn,
                started_at: now_rfc3339(),
                last_exit_ip: None,
                adopted: false,
            });
            if app.ovpn_daemon_warn {
                set_status(
                    app,
                    format!(
                        "connected: {name} — .ovpn has a daemon directive (staying foreground)"
                    ),
                );
            } else {
                set_status(app, format!("connected: {name}"));
            }
            spawn_current_resample(tx, gen);
            spawn_exit_ip(tx, gen, profile_id);
            if app.batch_active {
                app.batch_done += 1;
                batch_next(app, tx);
            }
        }
        // `spawn_privileged` cannot return `SudoAuth` (auth failures surface
        // in the child's logs, see `sniff_ovpn_auth`); handled for uniformity.
        Err(AppError::SudoAuth) => {
            settle_phase(app);
            match app.sudo_auth_failed() {
                SudoFailure::Reprompt => set_status(app, "sudo authentication failed — try again"),
                SudoFailure::Fatal => set_status(app, "sudo authentication failed twice"),
            }
        }
        Err(e) => {
            settle_phase(app);
            app.pending_connect = None;
            if app.batch_active {
                let failed = profile_name(app, &profile_id);
                batch_clear(app);
                show_error(app, format!("connect-all stopped at {failed}: {e}"));
            } else {
                show_error(app, format!("connect failed: {e}"));
            }
        }
    }
}

fn complete_ovpn_down(
    app: &mut App,
    tx: &UnboundedSender<BackendMsg>,
    gen: u64,
    cleanup: bool,
    logs: LogRing,
    exit_code: Option<i32>,
    ended_at: Option<String>,
) {
    // Stale/cleanup arrivals must not clobber the live log view: only the
    // current generation's logs replace the cache.
    if cleanup {
        set_status(app, "late openvpn spawn stopped");
        return;
    }
    if gen != app.conn_gen {
        return;
    }
    app.ovpn_logs = logs;
    app.active.retain(|a| a.kind != VpnType::OpenVpn);
    settle_phase(app);
    let exit = exit_code
        .map(|c| c.to_string())
        .unwrap_or_else(|| "signal".to_owned());
    let when = ended_at.unwrap_or_else(|| "unknown time".to_owned());
    set_status(app, format!("openvpn exited (code {exit}) at {when}"));
    spawn_current_resample(tx, gen);
    if let Some(target) = app.auto_connect.take() {
        let action = app.request_connect(target);
        act_on_connect(app, tx, action);
    }
    if app.drain.is_some() {
        drain_next(app, tx);
    }
}

/// 3 s AWG poll, and ONLY while an AWG tunnel is up (spec §4).
/// Polls the whole `awg show` dump (the live interface name is platform
/// specific — system-assigned utunN on macOS, the config file stem on
/// Linux — so no interface argument is ever passed). Overlap is guarded by
/// `awg_poll_inflight`.
fn maybe_poll_awg(app: &mut App, tx: &UnboundedSender<BackendMsg>) {
    if app.phase != ConnPhase::Connected || app.awg_poll_inflight {
        return;
    }
    if app.active_of_kind(VpnType::Awg).is_none() {
        return;
    }
    app.awg_poll_inflight = true;
    let gen = app.conn_gen;
    let task_tx = tx.clone();
    let sudo = Arc::clone(&app.sudo);
    tokio::spawn(async move {
        let result = crate::awg::show(&sudo).await;
        let _ = task_tx.send(BackendMsg::AwgPoll { gen, result });
    });
}

/// Pull the session's ring into the cached copy (sync lock, no await).
fn refresh_ovpn_cache(app: &mut App) {
    if let Some(session) = app.ovpn_session.as_ref() {
        app.ovpn_logs = session.snapshot();
    }
}

/// Best-effort sudo-auth sniff for the OpenVPN path: a wrong password does
/// not fail the spawn (it fails inside the child), so once per connection we
/// scan the captured output for sudo markers. On a hit the password is
/// dropped and an error popup tells the user to disconnect and retry.
fn sniff_ovpn_auth(app: &mut App) {
    if app.ovpn_auth_flagged || app.phase != ConnPhase::Connected {
        return;
    }
    if !app.active.iter().any(|a| a.kind == VpnType::OpenVpn) {
        return;
    }
    let joined = app.ovpn_logs.raw_lines().join("\n");
    if contains_auth_marker(&joined) || joined.contains(SUDO_PROMPT_SENTINEL) {
        app.ovpn_auth_flagged = true;
        app.sudo.clear();
        show_error(
            app,
            "openvpn output suggests sudo authentication failed — disconnect (d), then reconnect"
                .to_owned(),
        );
    }
}

fn spawn_initial_probes(tx: &UnboundedSender<BackendMsg>) {
    let route_tx = tx.clone();
    tokio::spawn(async move {
        let result = crate::net::get_default_route()
            .await
            .map_err(|e| e.to_string());
        let _ = route_tx.send(BackendMsg::InitialRoute(result));
    });
    let ip_tx = tx.clone();
    tokio::spawn(async move {
        let result = crate::net::get_public_ip().await.map_err(|e| e.to_string());
        let _ = ip_tx.send(BackendMsg::InitialIp(result));
    });
}

fn spawn_current_resample(tx: &UnboundedSender<BackendMsg>, gen: u64) {
    let route_tx = tx.clone();
    tokio::spawn(async move {
        let result = crate::net::get_default_route()
            .await
            .map_err(|e| e.to_string());
        let _ = route_tx.send(BackendMsg::CurrentRoute { gen, result });
    });
    let ip_tx = tx.clone();
    tokio::spawn(async move {
        let result = crate::net::get_public_ip().await.map_err(|e| e.to_string());
        let _ = ip_tx.send(BackendMsg::CurrentIp { gen, result });
    });
}

fn spawn_exit_ip(tx: &UnboundedSender<BackendMsg>, gen: u64, profile_id: String) {
    let task_tx = tx.clone();
    tokio::spawn(async move {
        let result = crate::net::get_public_ip().await.map_err(|e| e.to_string());
        let _ = task_tx.send(BackendMsg::ExitIp {
            gen,
            profile_id,
            result,
        });
    });
}

fn profile_name(app: &App, id: &str) -> String {
    app.profiles
        .iter()
        .find(|p| p.id == id)
        .map(|p| p.display_name.clone())
        .unwrap_or_else(|| id.to_owned())
}

fn now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

/// Status bar shows one line (spec §8); popups carry the full chain.
fn set_status(app: &mut App, message: impl Into<String>) {
    app.status_message = message.into();
}

fn show_error(app: &mut App, message: String) {
    app.status_message = message.lines().next().unwrap_or("error").to_owned();
    app.popup = Popup::Error(message);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn empty_app(is_root: bool) -> App {
        App {
            profiles: Vec::new(),
            selected: 0,
            active: Vec::new(),
            sudo: Arc::new(SudoRunner::new(is_root)),
            phase: ConnPhase::Idle,
            focus: Focus::List,
            popup: Popup::None,
            status_message: String::new(),
            sudo_input: String::new(),
            sudo_error: None,
            sudo_reprompted: false,
            sudo_cached: None,
            pending_connect: None,
            auto_connect: None,
            drain: None,
            batch_queue: VecDeque::new(),
            batch_active: false,
            batch_done: 0,
            batch_skipped: 0,
            conn_gen: 0,
            import_input: String::new(),
            import_vpn: VpnType::Awg,
            rename_input: String::new(),
            filter_editing: false,
            filter_input: String::new(),
            log_filter: String::new(),
            log_follow: true,
            detail_scroll: 0,
            awg_scroll: 0,
            awg_follow: true,
            awg_status: String::new(),
            awg_summary: None,
            awg_poll_inflight: false,
            ovpn_session: None,
            ovpn_logs: LogRing::new(),
            ovpn_daemon_warn: false,
            ovpn_auth_flagged: false,
            initial: NetSnapshot::default(),
            current: NetSnapshot::default(),
            should_quit: false,
        }
    }

    fn sample_profile(id: &str, name: &str, vpn: VpnType) -> Profile {
        let (subdir, ext) = match vpn {
            VpnType::Awg => ("awg", "conf"),
            VpnType::OpenVpn => ("openvpn", "ovpn"),
        };
        Profile {
            id: id.to_owned(),
            display_name: name.to_owned(),
            vpn_type: vpn,
            stored_file: format!("{subdir}/{name}.{ext}"),
            original_filename: format!("{name}.{ext}"),
            created_at: "2026-10-06T12:00:00Z".to_owned(),
        }
    }

    fn connected_app() -> App {
        let mut app = empty_app(false);
        app.sudo.set_password("pw".to_owned());
        app.profiles = vec![
            sample_profile("a", "office", VpnType::Awg),
            sample_profile("b", "tunnel", VpnType::OpenVpn),
            sample_profile("c", "branch", VpnType::Awg),
        ];
        app.phase = ConnPhase::Connected;
        app.active = vec![ActiveConn {
            profile_id: "a".to_owned(),
            kind: VpnType::Awg,
            started_at: "2026-10-06T12:00:00Z".to_owned(),
            last_exit_ip: None,
            adopted: false,
        }];
        app
    }

    #[test]
    fn connect_same_backend_requests_switch_confirm_without_spawning() {
        let mut app = connected_app();
        let action = app.request_connect(2);
        assert_eq!(action, ConnectAction::SwitchConfirm(2));
        assert_eq!(app.popup, Popup::Confirm(ConfirmKind::Switch { target: 2 }));
        // Nothing spawned: no session, still connected to the old tunnel.
        assert!(app.ovpn_session.is_none());
        assert_eq!(app.phase, ConnPhase::Connected);
        assert_eq!(app.active[0].profile_id, "a");
        assert_eq!(app.pending_connect, None);
    }

    #[test]
    fn connect_other_backend_starts_without_confirm() {
        // AWG is up; connecting the OpenVPN profile needs no switch confirm.
        let mut app = connected_app();
        let action = app.request_connect(1);
        assert_eq!(action, ConnectAction::StartNow(1));
        assert_eq!(app.popup, Popup::None);
        assert_eq!(app.phase, ConnPhase::Connected);
        assert_eq!(app.active.len(), 1);
    }

    #[test]
    fn quit_with_two_tunnels_still_confirms() {
        let mut app = connected_app();
        app.active.push(ActiveConn {
            profile_id: "b".to_owned(),
            kind: VpnType::OpenVpn,
            started_at: "2026-10-06T12:00:00Z".to_owned(),
            last_exit_ip: None,
            adopted: false,
        });
        assert_eq!(app.request_quit(), QuitAction::ConfirmQuit);
        assert_eq!(app.popup, Popup::Confirm(ConfirmKind::Quit));
        assert!(!app.should_quit, "quit must wait for confirmation");
    }

    #[test]
    fn disconnect_cancel_keeps_other_tunnel_connected() {
        // AWG up + OpenVPN connecting; cancelling the OpenVPN bring-up via
        // `d` on its (pending, selected) profile must leave AWG connected.
        let mut app = connected_app();
        app.selected = 1;
        app.phase = ConnPhase::Connecting;
        app.pending_connect = Some(1);
        let (tx, _rx) = mpsc::unbounded_channel();
        do_disconnect(&mut app, &tx);
        assert_eq!(app.phase, ConnPhase::Connected);
        assert_eq!(app.active.len(), 1);
        assert_eq!(app.active[0].profile_id, "a");
        assert_eq!(app.pending_connect, None);
    }

    #[test]
    fn disconnect_other_profile_reports_in_progress() {
        // Same setup, but `d` on a non-pending profile must not disturb it.
        let mut app = connected_app();
        app.selected = 0;
        app.phase = ConnPhase::Connecting;
        app.pending_connect = Some(1);
        let (tx, _rx) = mpsc::unbounded_channel();
        do_disconnect(&mut app, &tx);
        assert_eq!(app.phase, ConnPhase::Connecting);
        assert_eq!(app.pending_connect, Some(1));
    }

    #[test]
    fn connect_idle_prompts_sudo_only_when_needed() {
        // Non-root without password → masked prompt, connect deferred.
        let mut app = empty_app(false);
        app.profiles = vec![sample_profile("a", "office", VpnType::Awg)];
        let action = app.request_connect(0);
        assert_eq!(action, ConnectAction::SudoPrompt(0));
        assert_eq!(app.popup, Popup::Sudo);
        assert_eq!(app.pending_connect, Some(0));

        // Non-root with password → immediate start.
        let mut app = empty_app(false);
        app.sudo.set_password("pw".to_owned());
        app.profiles = vec![sample_profile("a", "office", VpnType::Awg)];
        assert_eq!(app.request_connect(0), ConnectAction::StartNow(0));
        assert_eq!(app.popup, Popup::None);

        // Root never prompts, with or without a stored password.
        for with_pw in [false, true] {
            let mut app = empty_app(true);
            if with_pw {
                app.sudo.set_password("pw".to_owned());
            }
            app.profiles = vec![sample_profile("a", "office", VpnType::Awg)];
            assert_eq!(app.request_connect(0), ConnectAction::StartNow(0));
            assert_eq!(app.popup, Popup::None);
        }
    }

    #[test]
    fn connect_same_profile_is_ignored() {
        let mut app = connected_app();
        let action = app.request_connect(0);
        assert!(matches!(action, ConnectAction::Ignored(_)));
        assert_eq!(app.popup, Popup::None);
    }

    #[test]
    fn quit_with_active_sets_confirm_popup() {
        let mut app = connected_app();
        assert_eq!(app.request_quit(), QuitAction::ConfirmQuit);
        assert_eq!(app.popup, Popup::Confirm(ConfirmKind::Quit));
        assert!(!app.should_quit, "quit must wait for confirmation");
    }

    #[test]
    fn quit_without_active_quits_immediately() {
        let mut app = empty_app(false);
        assert_eq!(app.request_quit(), QuitAction::QuitNow);
        assert!(app.should_quit);
        assert_eq!(app.popup, Popup::None);
    }

    #[test]
    fn quit_all_with_nothing_up_quits_immediately() {
        let mut app = empty_app(false);
        let (tx, _rx) = mpsc::unbounded_channel();
        do_quit_all(&mut app, &tx);
        assert!(app.should_quit);
        assert_eq!(app.drain, None);
    }

    #[test]
    fn quit_all_during_connect_cancels_and_quits_when_idle() {
        let mut app = empty_app(false);
        app.phase = ConnPhase::Connecting;
        app.pending_connect = Some(0);
        let (tx, _rx) = mpsc::unbounded_channel();
        do_quit_all(&mut app, &tx);
        assert!(app.should_quit);
        assert_eq!(app.pending_connect, None);
        assert_eq!(app.drain, None);
    }

    #[test]
    fn quit_all_during_disconnect_joins_the_flow() {
        let mut app = connected_app();
        app.phase = ConnPhase::Disconnecting;
        let (tx, _rx) = mpsc::unbounded_channel();
        do_quit_all(&mut app, &tx);
        assert!(!app.should_quit, "quit waits for the in-flight disconnect");
        assert_eq!(app.drain, Some(DrainMode::Quit));
    }

    #[test]
    fn quit_all_chain_ends_when_no_tunnels_left() {
        let mut app = empty_app(false);
        app.drain = Some(DrainMode::Quit);
        let (tx, _rx) = mpsc::unbounded_channel();
        drain_next(&mut app, &tx);
        assert!(app.should_quit);
        assert_eq!(app.drain, None);
    }

    #[test]
    fn quit_all_chain_aborts_on_disconnect_failure() {
        // A failed down must not silently leave a tunnel up: the quit is
        // aborted and the error is surfaced.
        let mut app = connected_app();
        app.drain = Some(DrainMode::Quit);
        let (tx, _rx) = mpsc::unbounded_channel();
        let gen = app.conn_gen;
        complete_awg_down(
            &mut app,
            &tx,
            gen,
            false,
            Err(AppError::User("boom".to_owned())),
        );
        assert!(!app.should_quit);
        assert_eq!(app.drain, None);
        assert!(matches!(app.popup, Popup::Error(_)));
        assert_eq!(app.active.len(), 1, "failed tunnel stays marked up");
    }

    #[test]
    fn disconnect_all_with_nothing_up_reports_status() {
        let mut app = empty_app(false);
        let (tx, _rx) = mpsc::unbounded_channel();
        do_disconnect_all(&mut app, &tx);
        assert!(!app.should_quit, "D never quits");
        assert_eq!(app.drain, None);
        assert_eq!(app.status_message, "no active tunnels");
    }

    #[test]
    fn disconnect_all_chain_ends_without_quitting() {
        let mut app = empty_app(false);
        app.drain = Some(DrainMode::Stay);
        let (tx, _rx) = mpsc::unbounded_channel();
        drain_next(&mut app, &tx);
        assert!(!app.should_quit);
        assert_eq!(app.drain, None);
        assert_eq!(app.status_message, "all tunnels disconnected");
    }

    #[test]
    fn disconnect_all_joins_inflight_drain() {
        let mut app = connected_app();
        app.phase = ConnPhase::Disconnecting;
        let (tx, _rx) = mpsc::unbounded_channel();
        do_disconnect_all(&mut app, &tx);
        assert_eq!(app.drain, Some(DrainMode::Stay));
        assert!(!app.should_quit);
    }

    #[test]
    fn quit_retargets_stay_drain_and_vice_versa() {
        // Last key wins: either flow can retarget an in-flight drain.
        let mut app = connected_app();
        app.phase = ConnPhase::Disconnecting;
        let (tx, _rx) = mpsc::unbounded_channel();
        do_disconnect_all(&mut app, &tx);
        assert_eq!(app.drain, Some(DrainMode::Stay));
        do_quit_all(&mut app, &tx);
        assert_eq!(app.drain, Some(DrainMode::Quit));
        do_disconnect_all(&mut app, &tx);
        assert_eq!(app.drain, Some(DrainMode::Stay));
    }

    #[test]
    fn delete_keys_open_delete_confirm() {
        let mut app = empty_app(true);
        app.profiles = vec![sample_profile("a", "office", VpnType::Awg)];
        let (tx, _rx) = mpsc::unbounded_channel();
        for code in [KeyCode::Delete, KeyCode::Backspace] {
            app.popup = Popup::None;
            let key = KeyEvent::new(code, KeyModifiers::empty());
            handle_normal_key(&mut app, &tx, key);
            assert!(
                matches!(app.popup, Popup::Confirm(ConfirmKind::Delete { .. })),
                "no delete confirm for {code:?}"
            );
        }
    }

    #[test]
    fn ctrl_j_k_reorders_selection_and_persists() {
        // Redirect the config root for this test only (restored on drop,
        let _guard = TempConfig::fresh(
            "keys",
            &["awg/office.conf", "openvpn/tunnel.ovpn", "awg/branch.conf"],
        );
        let seed = vec![
            sample_profile("a", "office", VpnType::Awg),
            sample_profile("b", "tunnel", VpnType::OpenVpn),
            sample_profile("c", "branch", VpnType::Awg),
        ];
        config::save_profiles(&seed).expect("seed profiles.yaml");

        let ctrl = |c: char| KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL);
        let (tx, _rx) = mpsc::unbounded_channel();
        let ids = |app: &App| {
            app.profiles
                .iter()
                .map(|p| p.id.clone())
                .collect::<Vec<_>>()
        };

        let mut app = App::new().expect("build");
        assert_eq!(ids(&app), vec!["a", "b", "c"]);
        assert_eq!(app.selected, 0);

        // Ctrl+J moves "a" down; selection follows it.
        handle_normal_key(&mut app, &tx, ctrl('j'));
        assert_eq!(ids(&app), vec!["b", "a", "c"]);
        assert_eq!(app.selected, 1);

        // Ctrl+K moves it back up.
        handle_normal_key(&mut app, &tx, ctrl('k'));
        assert_eq!(ids(&app), vec!["a", "b", "c"]);
        assert_eq!(app.selected, 0);

        // Edge: already at the top — order untouched, status says so.
        handle_normal_key(&mut app, &tx, ctrl('k'));
        assert_eq!(ids(&app), vec!["a", "b", "c"]);
        assert_eq!(app.selected, 0);
        assert!(app.status_message.contains("already at the top"));

        // Persisted: reload shows the same order.
        let back = config::load_profiles().expect("reload");
        assert_eq!(
            back.iter().map(|p| p.id.clone()).collect::<Vec<_>>(),
            vec!["a", "b", "c"]
        );
    }

    /// Redirect the config root at a fresh temp dir for one test (restored
    /// on drop, even on panic). Seed `files` (config-relative paths) so
    /// `is_missing()` answers are deterministic.
    ///
    /// Share the config module's process-wide lock: `XDG_CONFIG_HOME` is
    /// process-global, so config and app tests must not change it concurrently.
    struct TempConfig {
        old: Option<std::ffi::OsString>,
        dir: std::path::PathBuf,
        _env_lock: std::sync::MutexGuard<'static, ()>,
    }
    impl TempConfig {
        fn fresh(tag: &str, files: &[&str]) -> Self {
            let env_lock = config::CONFIG_ENV_LOCK
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            let dir =
                std::env::temp_dir().join(format!("tuiblick-test-{tag}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            for f in files {
                let path = dir.join("tuiblick").join(f);
                std::fs::create_dir_all(path.parent().unwrap()).unwrap();
                std::fs::write(&path, "fixture").unwrap();
            }
            let old = std::env::var_os("XDG_CONFIG_HOME");
            std::env::set_var("XDG_CONFIG_HOME", &dir);
            Self {
                old,
                dir,
                _env_lock: env_lock,
            }
        }
    }
    impl Drop for TempConfig {
        fn drop(&mut self) {
            match &self.old {
                Some(v) => std::env::set_var("XDG_CONFIG_HOME", v),
                None => std::env::remove_var("XDG_CONFIG_HOME"),
            }
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    #[test]
    fn batch_queue_skips_connected_profiles() {
        let app = connected_app();
        // "a" (awg) is up; queue holds the rest in list order.
        let queue = batch_queue_for(&app);
        assert_eq!(
            queue.into_iter().collect::<Vec<_>>(),
            vec!["b".to_owned(), "c".to_owned()]
        );
    }

    #[test]
    fn batch_step_drains_to_finish_report() {
        let mut app = empty_app(true);
        app.batch_active = true;
        app.batch_queue = VecDeque::from(["x".to_owned()]);
        app.batch_done = 2;
        app.batch_skipped = 1;
        // "x" vanished: dropped silently, then the queue is dry.
        assert_eq!(batch_step(&mut app), BatchStep::Done);
        assert!(!app.batch_active);
        assert!(app.batch_queue.is_empty());
        assert_eq!(
            app.status_message,
            "connect-all finished: 2 connected, 1 skipped"
        );
    }

    #[test]
    fn batch_finish_without_skips() {
        let mut app = empty_app(true);
        app.batch_active = true;
        app.batch_done = 1;
        batch_finish(&mut app);
        assert!(!app.batch_active);
        assert_eq!(app.status_message, "connect-all finished: 1 connected");
    }

    #[test]
    fn batch_step_spawns_first_runnable() {
        let _guard = TempConfig::fresh("spawn", &["awg/office.conf", "openvpn/tunnel.ovpn"]);
        let mut app = empty_app(true);
        app.profiles = vec![
            sample_profile("a", "office", VpnType::Awg),
            sample_profile("b", "tunnel", VpnType::OpenVpn),
        ];
        app.batch_active = true;
        // "ghost" was deleted mid-batch: dropped, then "a" spawns (root, no
        // sudo prompt). Nothing is spawned by the step itself.
        app.batch_queue = VecDeque::from(["ghost".to_owned(), "a".to_owned()]);
        assert_eq!(batch_step(&mut app), BatchStep::Spawn(0));
        assert!(app.batch_queue.is_empty());
        assert_eq!(app.batch_skipped, 0);
    }

    #[test]
    fn batch_step_skips_busy_backend_without_popup() {
        let _guard = TempConfig::fresh(
            "busy",
            &["awg/office.conf", "openvpn/tunnel.ovpn", "awg/branch.conf"],
        );
        // AWG "a" is up; "c" is same-backend: skip, never switch-confirm.
        let mut app = connected_app();
        app.batch_active = true;
        app.batch_queue = VecDeque::from(["c".to_owned()]);
        assert_eq!(batch_step(&mut app), BatchStep::Done);
        assert_eq!(app.popup, Popup::None, "no stray switch popup");
        assert_eq!(
            app.status_message,
            "connect-all finished: 0 connected, 1 skipped"
        );
    }

    #[test]
    fn batch_step_skips_missing_files_silently() {
        // Nothing seeded: every stored file is missing. Skips must not raise
        // error popups (the badges already say so); the finish counts them.
        let _guard = TempConfig::fresh("missing", &[]);
        let mut app = empty_app(true);
        app.profiles = vec![
            sample_profile("a", "office", VpnType::Awg),
            sample_profile("b", "tunnel", VpnType::OpenVpn),
        ];
        app.batch_active = true;
        app.batch_queue = VecDeque::from(["a".to_owned(), "b".to_owned()]);
        assert_eq!(batch_step(&mut app), BatchStep::Done);
        assert_eq!(app.popup, Popup::None, "no error popup for skips");
        assert_eq!(
            app.status_message,
            "connect-all finished: 0 connected, 2 skipped"
        );
    }

    #[test]
    fn batch_step_pauses_on_sudo_prompt() {
        let _guard = TempConfig::fresh("sudo", &["awg/office.conf", "openvpn/tunnel.ovpn"]);
        let mut app = empty_app(false);
        app.profiles = vec![
            sample_profile("a", "office", VpnType::Awg),
            sample_profile("b", "tunnel", VpnType::OpenVpn),
        ];
        app.batch_active = true;
        app.batch_queue = VecDeque::from(["a".to_owned(), "b".to_owned()]);
        assert_eq!(batch_step(&mut app), BatchStep::Wait);
        assert_eq!(app.popup, Popup::Sudo);
        assert_eq!(app.pending_connect, Some(0));
        // Current item popped; the rest waits for submit/Esc/failure.
        assert_eq!(
            app.batch_queue.into_iter().collect::<Vec<_>>(),
            vec!["b".to_owned()]
        );
    }

    #[test]
    fn sudo_esc_cancels_batch() {
        let mut app = empty_app(false);
        app.batch_active = true;
        app.batch_queue = VecDeque::from(["b".to_owned()]);
        app.batch_done = 1;
        app.popup = Popup::Sudo;
        app.pending_connect = Some(0);
        let (tx, _rx) = mpsc::unbounded_channel();
        let esc = KeyEvent::new(KeyCode::Esc, KeyModifiers::empty());
        app.popup = Popup::Sudo;
        handle_sudo_key(&mut app, &tx, esc);
        assert!(!app.batch_active);
        assert!(app.batch_queue.is_empty());
        assert_eq!(app.popup, Popup::None);
        assert_eq!(app.status_message, "connect-all cancelled");
    }

    #[test]
    fn fatal_sudo_failure_aborts_batch_but_reprompt_does_not() {
        let mut app = empty_app(false);
        app.batch_active = true;
        app.batch_queue = VecDeque::from(["b".to_owned()]);
        assert_eq!(app.sudo_auth_failed(), SudoFailure::Reprompt);
        assert!(app.batch_active, "re-prompt must keep the batch alive");
        assert_eq!(app.sudo_auth_failed(), SudoFailure::Fatal);
        assert!(!app.batch_active);
        assert!(app.batch_queue.is_empty());
    }

    #[test]
    fn connect_failure_aborts_batch() {
        let mut app = empty_app(true);
        app.profiles = vec![sample_profile("a", "office", VpnType::Awg)];
        app.phase = ConnPhase::Connecting;
        app.batch_active = true;
        app.batch_queue = VecDeque::from(["b".to_owned()]);
        let (tx, _rx) = mpsc::unbounded_channel();
        let gen = app.conn_gen;
        complete_awg_up(
            &mut app,
            &tx,
            gen,
            "a".to_owned(),
            Err(AppError::User("boom".to_owned())),
        );
        assert!(!app.batch_active);
        assert!(app.batch_queue.is_empty());
        assert!(matches!(app.popup, Popup::Error(_)));
        assert!(app.status_message.contains("connect-all stopped at office"));
    }

    #[test]
    fn adopt_matches_stored_stem_in_list_order() {
        let mut app = empty_app(true);
        app.profiles = vec![
            sample_profile("a", "office", VpnType::Awg),
            sample_profile("b", "tunnel", VpnType::OpenVpn),
            sample_profile("c", "branch", VpnType::Awg),
        ];
        let raw = "interface: branch\n  listening port: 51820\n\npeer: aaa=\n  endpoint: 1.1.1.1\n";
        assert_eq!(
            adopt_lingering_awg(&mut app, Ok(raw.to_owned())),
            Some("c".to_owned())
        );
        assert_eq!(app.phase, ConnPhase::Connected);
        assert_eq!(app.active.len(), 1);
        assert_eq!(app.active[0].profile_id, "c");
        assert!(app.active[0].adopted, "badge must distinguish adoption");
        assert!(app.awg_summary.is_some(), "panel seeds from the dump");
        assert!(
            app.status_message
                .contains("adopted lingering tunnel branch"),
            "got {}",
            app.status_message
        );
    }

    #[test]
    fn adopt_ignores_non_awg_and_active_backend() {
        // OpenVPN stems never match (AWG-only adoption).
        let mut app = empty_app(true);
        app.profiles = vec![sample_profile("b", "tunnel", VpnType::OpenVpn)];
        assert_eq!(
            adopt_lingering_awg(&mut app, Ok("interface: tunnel\n".to_owned())),
            None
        );
        assert_eq!(app.phase, ConnPhase::Idle);
        assert!(app.active.is_empty());

        // An already-active AWG backend blocks a second adoption: at most
        // one tunnel per backend.
        let mut app = connected_app();
        assert_eq!(
            adopt_lingering_awg(&mut app, Ok("interface: office\n".to_owned())),
            None
        );
        assert_eq!(app.active.len(), 1);
        assert!(!app.active[0].adopted);
    }

    #[test]
    fn adopt_failure_and_empty_dump_are_silent() {
        let mut app = empty_app(true);
        app.profiles = vec![sample_profile("a", "office", VpnType::Awg)];
        assert_eq!(
            adopt_lingering_awg(&mut app, Err(AppError::User("boom".to_owned()))),
            None
        );
        assert_eq!(adopt_lingering_awg(&mut app, Ok(String::new())), None);
        // macOS shape: utun names never equal a stored stem — no match.
        assert_eq!(
            adopt_lingering_awg(&mut app, Ok("interface: utun4\n".to_owned())),
            None
        );
        assert_eq!(app.phase, ConnPhase::Idle);
        assert!(app.active.is_empty());
        assert!(app.status_message.is_empty());
    }

    #[test]
    fn lingering_interface_skips_batch_without_popup() {
        // Quit-leaves-up + reopen: runtime state is clean but awg0 lingers,
        // so `awg-quick up` answers "already exists". Busy, not fatal —
        // the batch skips and drains instead of stopping with an error.
        let mut app = empty_app(true);
        app.profiles = vec![sample_profile("a", "office", VpnType::Awg)];
        app.phase = ConnPhase::Connecting;
        app.batch_active = true;
        app.batch_queue = VecDeque::from(["ghost".to_owned()]);
        let (tx, _rx) = mpsc::unbounded_channel();
        let gen = app.conn_gen;
        complete_awg_up(
            &mut app,
            &tx,
            gen,
            "a".to_owned(),
            Err(AppError::Process {
                cmd: "awg-quick up /root/.config/tuiblick/awg/awg0.conf".to_owned(),
                code: Some(1),
                tail: "Warning: `/root/.config/tuiblick/awg/awg0.conf' is world accessible\nawg-quick: `awg0' already exists".to_owned(),
            }),
        );
        assert!(
            matches!(app.popup, Popup::None),
            "skip must not raise an error popup"
        );
        assert!(!app.batch_active, "drained queue finishes the batch");
        assert_eq!(
            app.status_message,
            "connect-all finished: 0 connected, 1 skipped"
        );
    }

    #[test]
    fn ovpn_spawn_failure_aborts_batch() {
        let mut app = empty_app(true);
        app.profiles = vec![sample_profile("b", "tunnel", VpnType::OpenVpn)];
        app.phase = ConnPhase::Connecting;
        app.batch_active = true;
        let (tx, _rx) = mpsc::unbounded_channel();
        let gen = app.conn_gen;
        complete_ovpn_spawn(
            &mut app,
            &tx,
            gen,
            "b".to_owned(),
            Err(AppError::User("boom".to_owned())),
        );
        assert!(!app.batch_active);
        assert!(matches!(app.popup, Popup::Error(_)));
        assert!(app.status_message.contains("connect-all stopped at tunnel"));
    }

    #[test]
    fn disconnect_cancel_aborts_batch() {
        let mut app = empty_app(true);
        app.profiles = vec![sample_profile("a", "office", VpnType::Awg)];
        app.phase = ConnPhase::Connecting;
        app.pending_connect = Some(0);
        app.batch_active = true;
        app.batch_queue = VecDeque::from(["b".to_owned()]);
        let (tx, _rx) = mpsc::unbounded_channel();
        do_disconnect(&mut app, &tx);
        assert!(!app.batch_active);
        assert!(app.batch_queue.is_empty());
        assert_eq!(app.status_message, "connect-all cancelled");
    }

    #[test]
    fn quit_all_idle_clears_batch() {
        let mut app = empty_app(true);
        app.batch_active = true;
        app.batch_queue = VecDeque::from(["a".to_owned()]);
        app.batch_done = 1;
        let (tx, _rx) = mpsc::unbounded_channel();
        do_quit_all(&mut app, &tx);
        assert!(app.should_quit);
        assert!(!app.batch_active);
        assert!(app.batch_queue.is_empty());
    }

    #[test]
    fn start_connect_all_refuses_transitional_phase() {
        let mut app = empty_app(true);
        app.profiles = vec![sample_profile("a", "office", VpnType::Awg)];
        app.phase = ConnPhase::Connecting;
        let (tx, _rx) = mpsc::unbounded_channel();
        start_connect_all(&mut app, &tx);
        assert!(!app.batch_active);
        assert_eq!(app.status_message, "operation in progress…");
    }

    #[test]
    fn start_connect_all_reports_all_up() {
        let mut app = connected_app();
        for (id, kind) in [("b", VpnType::OpenVpn), ("c", VpnType::Awg)] {
            app.active.push(ActiveConn {
                profile_id: id.to_owned(),
                kind,
                started_at: "2026-10-06T12:00:00Z".to_owned(),
                last_exit_ip: None,
                adopted: false,
            });
        }
        let (tx, _rx) = mpsc::unbounded_channel();
        start_connect_all(&mut app, &tx);
        assert!(!app.batch_active);
        assert_eq!(app.status_message, "everything already connected");
    }

    #[test]
    fn start_connect_all_skips_up_and_missing() {
        // "a" and "b" are up; "c" is missing on disk (nothing seeded, so
        // every stored file is missing): the batch starts, skips "c", and
        // finishes without spawning anything.
        let _guard = TempConfig::fresh("allup", &[]);
        let mut app = connected_app();
        app.active.push(ActiveConn {
            profile_id: "b".to_owned(),
            kind: VpnType::OpenVpn,
            started_at: "2026-10-06T12:00:00Z".to_owned(),
            last_exit_ip: None,
            adopted: false,
        });
        let (tx, _rx) = mpsc::unbounded_channel();
        start_connect_all(&mut app, &tx);
        assert!(!app.batch_active, "batch drains synchronously");
        assert_eq!(
            app.status_message,
            "connect-all finished: 0 connected, 1 skipped"
        );
    }

    #[test]
    fn sudo_prompt_condition_polarities() {
        assert!(empty_app(false).needs_sudo_prompt());
        let app = empty_app(false);
        app.sudo.set_password("pw".to_owned());
        assert!(!app.needs_sudo_prompt());
        assert!(!empty_app(true).needs_sudo_prompt());
        let app = empty_app(true);
        app.sudo.set_password("pw".to_owned());
        assert!(!app.needs_sudo_prompt());
    }

    #[test]
    fn sudo_status_text_values() {
        assert_eq!(empty_app(true).sudo_status_text(), "root");
        assert_eq!(empty_app(false).sudo_status_text(), "sudo needed");
        let mut app = empty_app(false);
        app.sudo_cached = Some(true);
        assert_eq!(app.sudo_status_text(), "sudo cached");
        let app = empty_app(false);
        app.sudo.set_password("pw".to_owned());
        assert_eq!(app.sudo_status_text(), "sudo cached");
    }

    #[test]
    fn sudo_auth_fails_once_then_surfaces_error() {
        let mut app = empty_app(false);
        app.pending_connect = Some(0);
        assert_eq!(app.sudo_auth_failed(), SudoFailure::Reprompt);
        assert!(app.sudo_reprompted);
        assert_eq!(app.popup, Popup::Sudo);
        assert!(app.sudo_error.is_some());
        // Pending connect survives the single re-prompt.
        assert_eq!(app.pending_connect, Some(0));

        assert_eq!(app.sudo_auth_failed(), SudoFailure::Fatal);
        assert!(matches!(app.popup, Popup::Error(_)));
        assert_eq!(app.pending_connect, None);
    }

    #[test]
    fn confirm_resolution_effects() {
        let mut app = empty_app(false);
        app.popup = Popup::Confirm(ConfirmKind::Quit);
        assert_eq!(app.resolve_confirm(true), ConfirmEffect::Quit);
        assert!(app.should_quit);

        let mut app = empty_app(false);
        app.popup = Popup::Confirm(ConfirmKind::Quit);
        assert_eq!(app.resolve_confirm(false), ConfirmEffect::Nothing);
        assert!(!app.should_quit);
        assert_eq!(app.popup, Popup::None);

        let mut app = empty_app(false);
        app.popup = Popup::Confirm(ConfirmKind::Switch { target: 2 });
        assert_eq!(app.resolve_confirm(true), ConfirmEffect::SwitchTo(2));

        let mut app = empty_app(false);
        app.popup = Popup::Confirm(ConfirmKind::Delete {
            profile_id: "x".to_owned(),
            display_name: "gone".to_owned(),
        });
        assert_eq!(
            app.resolve_confirm(true),
            ConfirmEffect::DeleteProfile("x".to_owned())
        );
        assert_eq!(app.popup, Popup::None);
    }

    #[test]
    fn selection_clamps_at_bounds() {
        let mut app = empty_app(false);
        app.move_selection(1);
        assert_eq!(app.selected, 0);
        app.profiles = vec![
            sample_profile("a", "one", VpnType::Awg),
            sample_profile("b", "two", VpnType::Awg),
        ];
        app.move_selection(99);
        assert_eq!(app.selected, 1);
        app.move_selection(-99);
        assert_eq!(app.selected, 0);
        app.selected = 99;
        app.clamp_selection();
        assert_eq!(app.selected, 1);
    }

    #[test]
    fn import_path_validation() {
        assert!(clean_import_path("").is_err());
        assert!(clean_import_path("   ").is_err());
        assert_eq!(
            clean_import_path("  ~/vpn/office.conf  ").unwrap(),
            "~/vpn/office.conf"
        );
    }

    #[test]
    fn import_vpn_inference() {
        assert_eq!(infer_import_vpn("tun.ovpn"), Some(VpnType::OpenVpn));
        assert_eq!(infer_import_vpn("TUN.OVPN"), Some(VpnType::OpenVpn));
        assert_eq!(infer_import_vpn("a.conf"), Some(VpnType::Awg));
        assert_eq!(infer_import_vpn("A.CONF"), Some(VpnType::Awg));
        assert_eq!(infer_import_vpn("tun.ovpn "), Some(VpnType::OpenVpn));
        assert_eq!(infer_import_vpn("no-ext"), None);
        assert_eq!(infer_import_vpn("tun.txt"), None);
        assert_eq!(infer_import_vpn(""), None);
    }

    #[test]
    fn import_switch_follows_input_but_tab_has_last_word() {
        let mut app = empty_app(true);
        // Typing a path flips the switch to the implied backend.
        app.import_input = "tun.ovpn".to_owned();
        follow_input_backend(&mut app);
        assert_eq!(app.import_vpn, VpnType::OpenVpn);
        // Manual Tab sticks while the text is untouched...
        app.import_vpn = VpnType::Awg;
        // ...and survives edits that imply nothing...
        app.import_input = "tun.ovp".to_owned();
        follow_input_backend(&mut app);
        assert_eq!(app.import_vpn, VpnType::Awg);
        // ...but the next implying keystroke flips it again.
        app.import_input = "tun.ovpn".to_owned();
        follow_input_backend(&mut app);
        assert_eq!(app.import_vpn, VpnType::OpenVpn);
    }

    #[test]
    fn follow_toggle_and_filter_apply() {
        let mut app = empty_app(false);
        assert!(app.log_follow);
        app.toggle_follow();
        assert!(!app.log_follow);
        app.toggle_follow();
        assert!(app.log_follow);

        app.filter_editing = true;
        app.filter_input = "TLS".to_owned();
        app.detail_scroll = 40;
        app.apply_filter();
        assert_eq!(app.log_filter, "TLS");
        assert!(!app.filter_editing);
        assert_eq!(app.detail_scroll, 0);

        let mut ring_app = empty_app(false);
        ring_app.ovpn_logs.push("TLS handshake started".to_owned());
        ring_app
            .ovpn_logs
            .push("Initialization Sequence Completed".to_owned());
        ring_app.log_filter = "TLS".to_owned();
        assert_eq!(
            ring_app.ovpn_view(),
            vec!["TLS handshake started".to_owned()]
        );
    }

    #[test]
    fn detail_scroll_saturates_per_panel() {
        // No profiles: falls back to the log-view branch.
        let mut app = empty_app(false);
        app.scroll_detail(-5);
        assert_eq!(app.detail_scroll, 0);
        app.scroll_detail(7);
        assert_eq!(app.detail_scroll, 7);
        app.scroll_detail(-3);
        assert_eq!(app.detail_scroll, 4);
    }

    #[test]
    fn scroll_on_ovpn_panel_drops_follow() {
        // The reported bug: with follow on, scroll keys visibly did nothing.
        // Any manual scroll now drops follow so the keys always move.
        let mut app = empty_app(true);
        app.profiles = vec![sample_profile("b", "tunnel", VpnType::OpenVpn)];
        assert!(app.log_follow);
        app.scroll_detail(10);
        assert!(!app.log_follow, "scroll must drop follow");
        assert_eq!(app.detail_scroll, 10);
        assert_eq!(app.awg_scroll, 0, "awg offset untouched");
    }

    #[test]
    fn scroll_on_awg_panel_uses_awg_offset_only() {
        let mut app = empty_app(true);
        app.profiles = vec![sample_profile("a", "office", VpnType::Awg)];
        app.scroll_detail(5);
        assert_eq!(app.awg_scroll, 5);
        assert!(!app.awg_follow, "scroll must drop awg follow too");
        assert_eq!(app.detail_scroll, 0, "log offset untouched");
        assert!(app.log_follow, "log follow untouched on the awg panel");
        app.scroll_detail(-9);
        assert_eq!(app.awg_scroll, 0);
    }

    #[test]
    fn follow_toggle_targets_selected_panel() {
        let mut app = empty_app(true);
        app.profiles = vec![
            sample_profile("a", "office", VpnType::Awg),
            sample_profile("b", "tunnel", VpnType::OpenVpn),
        ];
        // AWG selected: `f` flips the panel pin, not the log.
        app.selected = 0;
        app.toggle_follow();
        assert!(!app.awg_follow);
        assert!(app.log_follow);
        assert_eq!(app.follow_state(), ("awg", false));
        // OpenVPN selected: `f` flips the log.
        app.selected = 1;
        app.toggle_follow();
        assert!(!app.log_follow);
        assert_eq!(app.follow_state(), ("log", false));
    }

    #[test]
    fn pagedown_key_drops_follow_in_normal_mode() {
        let mut app = empty_app(true);
        app.profiles = vec![sample_profile("b", "tunnel", VpnType::OpenVpn)];
        app.focus = Focus::Detail;
        assert!(app.log_follow);
        let (tx, _rx) = mpsc::unbounded_channel();
        let pgdn = KeyEvent::new(KeyCode::PageDown, KeyModifiers::empty());
        handle_normal_key(&mut app, &tx, pgdn);
        assert!(!app.log_follow, "PgDn must drop follow");
        assert_eq!(app.detail_scroll, PAGE_STEP as usize);
    }

    #[test]
    fn header_line_shows_resolving_then_values() {
        let app = empty_app(false);
        let line = app.header_line();
        assert!(line.contains("resolving"), "got {line:?}");

        let mut app = empty_app(false);
        app.initial.ip = Some("1.1.1.1".to_owned());
        app.initial.gateway = Some("192.168.0.1".to_owned());
        app.initial.iface = Some("en0".to_owned());
        app.current.ip = Some("2.2.2.2".to_owned());
        let line = app.header_line();
        assert!(line.contains("1.1.1.1"), "got {line:?}");
        assert!(line.contains("2.2.2.2"), "got {line:?}");
        assert!(line.contains("192.168.0.1"), "got {line:?}");
    }

    #[test]
    fn help_text_lists_spec_keys() {
        let help = help_text();
        for token in [
            "Enter",
            "Ctrl+j",
            "connect all",
            "disconnect",
            "rescan",
            "rename",
            "import",
            "follow",
            "sudo",
            "quit",
        ] {
            assert!(help.contains(token), "help missing {token:?}");
        }
    }

    #[test]
    fn request_connect_out_of_range_is_ignored() {
        let mut app = empty_app(true);
        let action = app.request_connect(0);
        assert!(matches!(action, ConnectAction::Ignored(_)));
        assert_eq!(app.popup, Popup::None);
    }

    #[test]
    fn rename_opens_prefilled_prompt() {
        let mut app = empty_app(true);
        app.profiles = vec![sample_profile("a", "office", VpnType::Awg)];
        open_rename(&mut app);
        assert_eq!(app.popup, Popup::Rename);
        assert_eq!(app.rename_input, "office");
    }

    #[test]
    fn rename_with_no_profiles_reports_status() {
        let mut app = empty_app(true);
        open_rename(&mut app);
        assert_eq!(app.popup, Popup::None);
        assert!(!app.status_message.is_empty());
    }

    #[test]
    fn rename_blank_name_shows_error_without_touching_fs() {
        // Blank is rejected before any config write: hermetic (no fs).
        let mut app = empty_app(true);
        app.profiles = vec![sample_profile("a", "office", VpnType::Awg)];
        app.popup = Popup::Rename;
        app.rename_input = "   ".to_owned();
        submit_rename(&mut app);
        assert!(matches!(app.popup, Popup::Error(_)));
        assert_eq!(app.profiles[0].display_name, "office");
    }

    #[test]
    fn delete_blocked_while_connecting_pending() {
        // Enter on P (Connecting) then D: the confirm popup must NOT open.
        let mut app = empty_app(true);
        app.profiles = vec![
            sample_profile("a", "office", VpnType::Awg),
            sample_profile("b", "tunnel", VpnType::OpenVpn),
        ];
        app.selected = 0;
        app.phase = ConnPhase::Connecting;
        app.pending_connect = Some(0);
        open_delete_confirm(&mut app);
        assert!(
            !matches!(app.popup, Popup::Confirm(_)),
            "no confirm popup while connecting, got {:?}",
            app.popup
        );
        assert!(
            !app.status_message.is_empty(),
            "a status error must explain the connecting block"
        );
        assert_eq!(app.profiles.len(), 2);
    }

    #[test]
    fn do_delete_profile_blocked_for_pending_target() {
        let mut app = empty_app(true);
        app.profiles = vec![
            sample_profile("a", "office", VpnType::Awg),
            sample_profile("b", "tunnel", VpnType::OpenVpn),
        ];
        app.phase = ConnPhase::Connecting;
        app.pending_connect = Some(0);
        do_delete_profile(&mut app, "a");
        assert_eq!(app.profiles.len(), 2, "pending profile must survive");
        assert!(app.profiles.iter().any(|p| p.id == "a"));
        assert!(
            !app.status_message.is_empty(),
            "a status error must explain the connecting block"
        );
        // Guard is target-scoped: the other profile is not a pending target.
        assert!(!is_pending_connect_target(&app, "b"));
        assert!(is_pending_connect_target(&app, "a"));
    }
}
