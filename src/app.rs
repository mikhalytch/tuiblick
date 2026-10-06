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
    pub conn_gen: u64,
    pub import_input: String,
    pub import_vpn: VpnType,
    pub rename_input: String,
    pub filter_editing: bool,
    pub filter_input: String,
    pub log_filter: String,
    pub log_follow: bool,
    pub detail_scroll: usize,
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
            conn_gen: 0,
            import_input: String::new(),
            import_vpn: VpnType::Awg,
            rename_input: String::new(),
            filter_editing: false,
            filter_input: String::new(),
            log_filter: String::new(),
            log_follow: true,
            detail_scroll: 0,
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

    /// Re-clamp after the list shrinks (rescan / delete).
    pub fn clamp_selection(&mut self) {
        if self.profiles.is_empty() {
            self.selected = 0;
        } else if self.selected >= self.profiles.len() {
            self.selected = self.profiles.len() - 1;
        }
    }

    /// Saturating detail-pane scroll (the UI clamps the top edge).
    pub fn scroll_detail(&mut self, delta: isize) {
        let next = (self.detail_scroll as isize + delta).max(0);
        self.detail_scroll = next as usize;
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
        self.log_follow = !self.log_follow;
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
     Tab, 1/2/3    switch pane focus (list / detail)\n\
     Enter, c       connect selected (c clears the OpenVPN log view when the detail pane is focused)\n\
     d              disconnect selected profile's tunnel\n\
     r              rename selected profile (config rescan is automatic)\n\
     i              import file (.ovpn/.conf flips backend, Tab toggles)\n\
     D              delete profile (confirm)\n\
     /              filter openvpn logs\n\
     f              follow toggle\n\
     s              save openvpn log to /tmp\n\
     PgUp/PgDn      scroll detail\n\
     ?              this help\n\
     q, Ctrl-C      quit (active tunnel stays up; confirm if active)\n\
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
    OvpnSpawn {
        gen: u64,
        profile_id: String,
        result: Result<OpenVpnSession, AppError>,
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
pub async fn run() -> anyhow::Result<()> {
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
    spawn_initial_probes(&btx);

    let mut terminal =
        ratatui::Terminal::new(ratatui::backend::CrosstermBackend::new(std::io::stdout()))?;
    let mut render_tick = tokio::time::interval(RENDER_TICK);
    render_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut awg_tick = tokio::time::interval(AWG_POLL_INTERVAL);
    awg_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut rescan_tick = tokio::time::interval(RESCAN_INTERVAL);
    rescan_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

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
/// Ctrl/Alt chords (other than Ctrl-C, handled earlier) are ignored.
fn handle_normal_key(app: &mut App, tx: &UnboundedSender<BackendMsg>, key: KeyEvent) {
    if key.modifiers.contains(KeyModifiers::CONTROL) || key.modifiers.contains(KeyModifiers::ALT) {
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
        KeyCode::Char('D') => open_delete_confirm(app),
        KeyCode::Char('r') => open_rename(app),
        KeyCode::Char('i') => open_import(app),
        KeyCode::Char('/') => {
            app.filter_editing = true;
            app.filter_input.clone_from(&app.log_filter);
        }
        KeyCode::Char('f') => {
            app.toggle_follow();
            set_status(
                app,
                if app.log_follow {
                    "log follow on"
                } else {
                    "log follow off"
                },
            );
        }
        KeyCode::Char('s') => save_log(app),
        KeyCode::Char('?') => app.popup = Popup::Help,
        KeyCode::Char('q') => do_quit(app),
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
            set_status(app, "sudo cancelled — connect aborted");
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
                "config file missing: {} — re-import or delete (D)",
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
                    result,
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
                app.conn_gen = app.conn_gen.wrapping_add(1);
                settle_phase(app);
                app.pending_connect = None;
                app.auto_connect = None;
                set_status(app, "connect cancelled");
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
        BackendMsg::OvpnSpawn {
            gen,
            profile_id,
            result,
        } => complete_ovpn_spawn(app, tx, gen, profile_id, result),
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
            });
            app.awg_status = output;
            app.awg_summary = Some(crate::awg::parse_summary(&app.awg_status));
            set_status(app, format!("connected: {name}"));
            spawn_current_resample(tx, gen);
            spawn_exit_ip(tx, gen, profile_id);
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
            show_error(app, format!("connect failed: {e}"));
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
        }
        Err(AppError::SudoAuth) => {
            // The down path cannot reuse the connect re-prompt slot, so it
            // surfaces directly; the tunnel stays marked up (honest).
            app.auto_connect = None;
            settle_phase(app);
            show_error(
                app,
                "sudo authentication failed during disconnect — tunnel may still be up".to_owned(),
            );
        }
        Err(e) => {
            app.auto_connect = None;
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
            show_error(app, format!("connect failed: {e}"));
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
}

/// 3 s AWG poll, and ONLY while an AWG tunnel is up (spec §4).
/// Overlap is guarded by `awg_poll_inflight`.
fn maybe_poll_awg(app: &mut App, tx: &UnboundedSender<BackendMsg>) {
    if app.phase != ConnPhase::Connected || app.awg_poll_inflight {
        return;
    }
    let Some(awg_id) = app
        .active_of_kind(VpnType::Awg)
        .map(|a| a.profile_id.clone())
    else {
        return;
    };
    let Some(profile) = app.profiles.iter().find(|p| p.id == awg_id).cloned() else {
        return;
    };
    let iface = iface_of(&profile.stored_path());
    if iface.is_empty() {
        return;
    }
    app.awg_poll_inflight = true;
    let gen = app.conn_gen;
    let task_tx = tx.clone();
    let sudo = Arc::clone(&app.sudo);
    tokio::spawn(async move {
        let result = crate::awg::show(&sudo, &iface).await;
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

/// Interface name for `awg show`: the stored file's stem.
fn iface_of(path: &Path) -> String {
    path.file_stem()
        .map(|stem| stem.to_string_lossy().into_owned())
        .unwrap_or_default()
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
            conn_gen: 0,
            import_input: String::new(),
            import_vpn: VpnType::Awg,
            rename_input: String::new(),
            filter_editing: false,
            filter_input: String::new(),
            log_filter: String::new(),
            log_follow: true,
            detail_scroll: 0,
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
    fn detail_scroll_saturates() {
        let mut app = empty_app(false);
        app.scroll_detail(-5);
        assert_eq!(app.detail_scroll, 0);
        app.scroll_detail(7);
        assert_eq!(app.detail_scroll, 7);
        app.scroll_detail(-3);
        assert_eq!(app.detail_scroll, 4);
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
