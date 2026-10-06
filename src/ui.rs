//! ratatui rendering (spec §6 header/exit-IP placement, §7 layout/keys/popups).
//!
//! Layout: header bar (`init <ip> (<gw>/<if>) → exit <ip>` + sudo/phase
//! segment), main split (left profile list / right detail), bottom status
//! bar. Popups (`?`, sudo, import, confirm, error) render as centered
//! overlays; the log filter renders as an inline input line. No mouse;
//! everything is keyboard-driven. All rendering is defensive about tiny
//! areas (80x24 minimum, narrower must not panic): zero-size areas are
//! skipped and text is wrapped, never unwrapped.

use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, List, ListItem, Paragraph, Wrap},
    Frame,
};

use crate::app::{help_text, App, ConfirmKind, ConnPhase, Focus, Popup, QUIT_CONFIRM_MSG};
use crate::config::VpnType;

/// Mask glyph for the sudo password field (never render plaintext).
const MASK_GLYPH: char = '•';

/// Top-level render entry point, called from the `app::run` render tick.
pub fn render(frame: &mut Frame, app: &App) {
    let area = frame.area();
    if area.width == 0 || area.height == 0 {
        return;
    }
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2),
            Constraint::Min(1),
            Constraint::Length(2),
        ])
        .split(area);
    render_header(frame, app, chunks[0]);
    render_main(frame, app, chunks[1]);
    render_status(frame, app, chunks[2]);
    render_popup(frame, app, area);
}

// ---------------------------------------------------------------------------
// Header
// ---------------------------------------------------------------------------

/// Header bar: `App::header_line` (spec §6) plus the sudo/phase/focus segment.
fn render_header(frame: &mut Frame, app: &App, area: Rect) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let phase = format!("{:?}", app.phase).to_lowercase();
    let focus = match app.focus {
        Focus::List => "list",
        Focus::Detail => "detail",
    };
    let lines = vec![
        Line::from(vec![
            Span::styled("tuiblick ", Style::default().add_modifier(Modifier::BOLD)),
            Span::raw(app.header_line()),
        ]),
        Line::from(vec![
            Span::raw(format!("sudo: {}  ", app.sudo_status_text())),
            Span::raw(format!("phase: {phase}  focus: {focus} (1/2/3/Tab)")),
        ]),
    ];
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), area);
}

// ---------------------------------------------------------------------------
// Main split
// ---------------------------------------------------------------------------

fn render_main(frame: &mut Frame, app: &App, area: Rect) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(35), Constraint::Percentage(65)])
        .split(area);
    render_profile_list(frame, app, cols[0]);
    render_detail(frame, app, cols[1]);
}

/// Left pane: `display_name` + backend badge + active marker + `(missing)`.
fn render_profile_list(frame: &mut Frame, app: &App, area: Rect) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let focused = app.focus == Focus::List;
    let title = format!(" Profiles ({}) ", app.profiles.len());
    if app.profiles.is_empty() {
        let body = Paragraph::new("no profiles\n\ni: import  r: rename")
            .block(pane_block(&title, focused))
            .wrap(Wrap { trim: false });
        frame.render_widget(body, area);
        return;
    }
    let items: Vec<ListItem> = app
        .profiles
        .iter()
        .enumerate()
        .map(|(index, profile)| {
            let active = app
                .active
                .as_ref()
                .is_some_and(|a| a.profile_id == profile.id);
            let marker = if active { "● " } else { "  " };
            let badge = match profile.vpn_type {
                VpnType::Awg => "[awg]",
                VpnType::OpenVpn => "[ovpn]",
            };
            let missing = if profile.is_missing() {
                " (missing)"
            } else {
                ""
            };
            let line = Line::from(vec![
                Span::raw(marker),
                Span::styled(
                    profile.display_name.clone(),
                    if index == app.selected {
                        Style::default().add_modifier(Modifier::BOLD)
                    } else {
                        Style::default()
                    },
                ),
                Span::styled(
                    format!(" {badge}{missing}"),
                    Style::default().fg(Color::DarkGray),
                ),
            ]);
            ListItem::new(line)
        })
        .collect();
    let selected_style = if focused {
        Style::default()
            .bg(Color::Blue)
            .fg(Color::White)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().bg(Color::DarkGray).fg(Color::White)
    };
    let list = List::new(items)
        .block(pane_block(&title, focused))
        .highlight_style(selected_style)
        .highlight_symbol("» ");
    let mut state = ratatui::widgets::ListState::default();
    state.select(Some(app.selected.min(app.profiles.len().saturating_sub(1))));
    frame.render_stateful_widget(list, area, &mut state);
}

/// Right pane: AWG status panel or OpenVPN log panel for the selection.
fn render_detail(frame: &mut Frame, app: &App, area: Rect) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let Some(profile) = app.selected_profile() else {
        let body = Paragraph::new(
            "No profiles yet.\n\nPress i to import a file,\nr to rename, ? for help.",
        )
        .block(pane_block(" Detail ", app.focus == Focus::Detail))
        .wrap(Wrap { trim: false });
        frame.render_widget(body, area);
        return;
    };
    let focused = app.focus == Focus::Detail;
    match profile.vpn_type {
        VpnType::Awg => render_awg_detail(frame, app, area, focused),
        VpnType::OpenVpn => render_ovpn_detail(frame, app, area, focused),
    }
}

/// AWG panel: phase/status text, parse summary, `last exit`, raw output.
fn render_awg_detail(frame: &mut Frame, app: &App, area: Rect, focused: bool) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let Some(profile) = app.selected_profile() else {
        return;
    };
    let active_for_selected = app
        .active
        .as_ref()
        .filter(|a| a.profile_id == profile.id && a.kind == VpnType::Awg);
    let title = format!(" {} [awg] ", profile.display_name);
    let mut lines: Vec<Line> = vec![Line::from(format!("phase: {:?}", app.phase).to_lowercase())];
    match active_for_selected {
        Some(active) => {
            lines.push(Line::from(format!("started: {}", active.started_at)));
            lines.push(Line::from(format!(
                "last exit: {}",
                active.last_exit_ip.as_deref().unwrap_or("—")
            )));
        }
        None => {
            lines.push(Line::from("last exit: — (not connected)"));
        }
    }
    if let Some(summary) = app
        .awg_summary
        .as_ref()
        .filter(|_| active_for_selected.is_some())
    {
        lines.push(Line::from(format!("peers: {}", summary.peers)));
        lines.push(Line::from(format!(
            "handshake: {}",
            summary.latest_handshake.as_deref().unwrap_or("—")
        )));
        lines.push(Line::from(format!(
            "rx/tx: {}/{}",
            summary.rx.as_deref().unwrap_or("—"),
            summary.tx.as_deref().unwrap_or("—")
        )));
    }
    if app.awg_status.trim().is_empty() {
        lines.push(Line::from(""));
        lines.push(Line::from(
            "Press Enter/c to connect. `awg show` output appears here.",
        ));
    } else {
        lines.push(Line::from(""));
        for raw in app.awg_status.lines() {
            lines.push(Line::from(raw.to_owned()));
        }
    }
    let body = Paragraph::new(lines)
        .block(pane_block(&title, focused))
        .wrap(Wrap { trim: false });
    frame.render_widget(body, area);
}

/// OpenVPN panel: daemon-warn header, `last exit`, filterable log view.
fn render_ovpn_detail(frame: &mut Frame, app: &App, area: Rect, focused: bool) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let Some(profile) = app.selected_profile() else {
        return;
    };
    let title = format!(" {} [openvpn] ", profile.display_name);
    let block = pane_block(&title, focused);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.width == 0 || inner.height == 0 {
        return;
    }

    let mut header: Vec<Line> = Vec::new();
    if app.ovpn_daemon_warn {
        header.push(Line::styled(
            "warning: .ovpn has a daemon directive — staying foreground",
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ));
    }
    let last_exit = app
        .active
        .as_ref()
        .filter(|a| a.profile_id == profile.id && a.kind == VpnType::OpenVpn)
        .and_then(|a| a.last_exit_ip.clone())
        .unwrap_or_else(|| "—".to_owned());
    header.push(Line::from(format!("last exit: {last_exit}")));
    let view = app.ovpn_view();
    let total = view.len();
    let shown = if app.log_filter.is_empty() {
        format!("{total} lines")
    } else {
        format!("{total} shown (filter: {})", app.log_filter)
    };
    header.push(Line::from(format!(
        "follow: {}  scroll: {}  {shown}",
        if app.log_follow { "on(f)" } else { "off(f)" },
        app.detail_scroll,
    )));

    let filter_rows: u16 = if app.filter_editing { 1 } else { 0 };
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(header.len().min(u16::MAX as usize) as u16),
            Constraint::Min(1),
            Constraint::Length(filter_rows),
        ])
        .split(inner);
    frame.render_widget(Paragraph::new(header).wrap(Wrap { trim: false }), rows[0]);
    render_log_view(frame, app, &view, rows[1]);
    if app.filter_editing {
        let input = Paragraph::new(Line::from(vec![
            Span::raw("/"),
            Span::styled(
                app.filter_input.clone(),
                Style::default().add_modifier(Modifier::BOLD),
            ),
            Span::styled("▌", Style::default().fg(Color::Yellow)),
        ]))
        .wrap(Wrap { trim: false });
        frame.render_widget(input, rows[2]);
    }
}

/// Log view: `follow=true` pins the tail (scroll clamped to the bottom);
/// otherwise `detail_scroll` is honored as the top offset.
fn render_log_view(frame: &mut Frame, app: &App, view: &[String], area: Rect) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    if view.is_empty() {
        let hint = if app.phase == ConnPhase::Connected
            && app
                .active
                .as_ref()
                .is_some_and(|a| a.kind == VpnType::OpenVpn)
        {
            "waiting for openvpn output…"
        } else {
            "No log lines. Press Enter/c to connect; / filters, f follows."
        };
        frame.render_widget(Paragraph::new(hint).wrap(Wrap { trim: false }), area);
        return;
    }
    let visible = area.height as usize;
    let top = if app.log_follow {
        view.len().saturating_sub(visible)
    } else {
        app.detail_scroll.min(view.len().saturating_sub(1))
    };
    let lines: Vec<Line> = view
        .iter()
        .skip(top)
        .take(visible.max(1))
        .map(|l| Line::from(l.clone()))
        .collect();
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), area);
}

// ---------------------------------------------------------------------------
// Status bar
// ---------------------------------------------------------------------------

/// Bottom bar: one-line status message + sudo/phase/counts segment.
fn render_status(frame: &mut Frame, app: &App, area: Rect) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let first = app.status_message.lines().next().unwrap_or("").to_owned();
    let second = format!(
        "sudo: {}  profiles: {}  logs: {}  (? help, q quit)",
        app.sudo_status_text(),
        app.profiles.len(),
        app.ovpn_logs.len(),
    );
    let bar =
        Paragraph::new(vec![Line::from(first), Line::from(second)]).wrap(Wrap { trim: false });
    frame.render_widget(bar, area);
}

// ---------------------------------------------------------------------------
// Popups
// ---------------------------------------------------------------------------

/// Centered overlays for every [`Popup`] variant (no-op for `None`).
fn render_popup(frame: &mut Frame, app: &App, area: Rect) {
    let (title, body, width_pct, height_pct) = match &app.popup {
        Popup::None => return,
        Popup::Help => {
            let mut body = help_text().to_owned();
            body.push_str("\nq / ? / Enter / Esc closes this help");
            (" Help (?) ", body, 80, 80)
        }
        Popup::Sudo => {
            let masked: String =
                std::iter::repeat_n(MASK_GLYPH, app.sudo_input.chars().count()).collect();
            let mut text = format!(
                "sudo password (asked once per session):\n{masked}▌\nEnter: submit   Esc: cancel"
            );
            if let Some(err) = &app.sudo_error {
                text.push_str(&format!("\nerror: {err}"));
            }
            (" Sudo ", text, 60, 40)
        }
        Popup::Import => {
            let backend = match app.import_vpn {
                VpnType::Awg => "awg",
                VpnType::OpenVpn => "openvpn",
            };
            let text = format!(
                "source path (absolute or ~):\n{}▌\nbackend: {backend}  (Tab toggles)\nnote: .ovpn implies openvpn\nEnter: import   Esc: cancel",
                app.import_input
            );
            (" Import ", text, 70, 50)
        }
        Popup::Rename => {
            let current = app
                .selected_profile()
                .map(|p| p.display_name.clone())
                .unwrap_or_default();
            let text = format!(
                "renaming \"{current}\" (display name only, file untouched):\n{}▌\nEnter: rename   Esc: cancel",
                app.rename_input
            );
            (" Rename ", text, 60, 40)
        }
        Popup::Confirm(kind) => match kind {
            // Wide enough (62 inner cells at 80x24) that the verbatim quit
            // line never wraps mid-sentence.
            ConfirmKind::Quit => (
                " Confirm quit ",
                format!("{QUIT_CONFIRM_MSG}\n(y/N, Enter=No)"),
                80,
                30,
            ),
            ConfirmKind::Switch { target } => {
                let name = app
                    .profiles
                    .get(*target)
                    .map(|p| p.display_name.clone())
                    .unwrap_or_else(|| format!("#{target}"));
                (
                    " Confirm switch ",
                    format!("switch to \"{name}\"? y/N\n(Enter=No, current tunnel stops first)"),
                    60,
                    30,
                )
            }
            ConfirmKind::Delete { display_name, .. } => (
                " Confirm delete ",
                format!("delete \"{display_name}\"? y/N\n(Enter=No, removes file + mapping)"),
                60,
                30,
            ),
        },
        Popup::Error(message) => {
            let text = format!("{message}\n(q/Enter/Esc to dismiss)");
            (" Error ", text, 70, 50)
        }
    };
    let popup_area = centered_rect(width_pct, height_pct, area);
    if popup_area.width == 0 || popup_area.height == 0 {
        return;
    }
    frame.render_widget(Clear, popup_area);
    let paragraph = Paragraph::new(body)
        .block(
            Block::default()
                .title(title)
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Yellow)),
        )
        .wrap(Wrap { trim: false });
    frame.render_widget(paragraph, popup_area);
}

/// Centered rectangle (percentages of the available area; safe on tiny areas).
fn centered_rect(width_pct: u16, height_pct: u16, area: Rect) -> Rect {
    let height_pct = height_pct.min(100);
    let width_pct = width_pct.min(100);
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - height_pct) / 2),
            Constraint::Percentage(height_pct),
            Constraint::Percentage(100 - height_pct - (100 - height_pct) / 2),
        ])
        .split(area);
    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - width_pct) / 2),
            Constraint::Percentage(width_pct),
            Constraint::Percentage(100 - width_pct - (100 - width_pct) / 2),
        ])
        .split(vertical[1])[1]
}

/// Pane border: highlighted when the pane has focus.
fn pane_block<'a>(title: &'a str, focused: bool) -> Block<'a> {
    Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_style(if focused {
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::DarkGray)
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    use crate::app::{ActiveConn, ConfirmKind, ConnPhase, Focus, NetSnapshot, Popup};
    use crate::config::{Profile, VpnType};
    use crate::openvpn::LogRing;
    use crate::sudo::SudoRunner;

    /// NOTE (reconciliation vs the task brief): the brief names a free
    /// function `header_line("1.1.1.1","2.2.2.2")`, but the committed
    /// `app.rs` surface exposes it as the method `App::header_line(&self)`.
    /// The tests below build an `App` fixture with
    /// `initial.ip=1.1.1.1` / `current.ip=2.2.2.2` and assert through the
    /// real method plus the rendered header.
    fn fixture_app() -> App {
        App {
            profiles: vec![
                Profile {
                    id: "a".to_owned(),
                    display_name: "office".to_owned(),
                    vpn_type: VpnType::Awg,
                    stored_file: "awg/office.conf".to_owned(),
                    original_filename: "office.conf".to_owned(),
                    created_at: "2026-10-06T12:00:00Z".to_owned(),
                },
                Profile {
                    id: "b".to_owned(),
                    display_name: "tunnel".to_owned(),
                    vpn_type: VpnType::OpenVpn,
                    stored_file: "openvpn/tunnel.ovpn".to_owned(),
                    original_filename: "tunnel.ovpn".to_owned(),
                    created_at: "2026-10-06T12:00:00Z".to_owned(),
                },
            ],
            selected: 0,
            active: None,
            sudo: Arc::new(SudoRunner::new(false)),
            phase: ConnPhase::Idle,
            focus: Focus::List,
            popup: Popup::None,
            status_message: "ready".to_owned(),
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
            initial: NetSnapshot {
                ip: Some("1.1.1.1".to_owned()),
                gateway: Some("192.168.0.1".to_owned()),
                iface: Some("en0".to_owned()),
            },
            current: NetSnapshot {
                ip: Some("2.2.2.2".to_owned()),
                gateway: Some("192.168.0.1".to_owned()),
                iface: Some("en0".to_owned()),
            },
            should_quit: false,
        }
    }

    fn buffer_text(terminal: &Terminal<TestBackend>) -> String {
        let buf = terminal.backend().buffer();
        let mut out = String::new();
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                if let Some(cell) = buf.cell((x, y)) {
                    out.push_str(cell.symbol());
                }
            }
            out.push('\n');
        }
        out
    }

    fn render_text(app: &App, width: u16, height: u16) -> String {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        terminal
            .draw(|frame| render(frame, app))
            .expect("render must not panic");
        buffer_text(&terminal)
    }

    #[test]
    fn header_line_method_contains_both_ips() {
        let app = fixture_app();
        let line = app.header_line();
        assert!(line.contains("1.1.1.1"), "got {line:?}");
        assert!(line.contains("2.2.2.2"), "got {line:?}");
    }

    #[test]
    fn rendered_header_contains_both_ips() {
        let screen = render_text(&fixture_app(), 80, 24);
        assert!(screen.contains("1.1.1.1"), "header missing init ip");
        assert!(screen.contains("2.2.2.2"), "header missing exit ip");
    }

    #[test]
    fn popups_render_without_panic_at_80x24() {
        let mut app = fixture_app();
        app.active = Some(ActiveConn {
            profile_id: "a".to_owned(),
            kind: VpnType::Awg,
            started_at: "2026-10-06T12:00:00Z".to_owned(),
            last_exit_ip: Some("2.2.2.2".to_owned()),
        });
        let popups = vec![
            Popup::Help,
            Popup::Sudo,
            Popup::Import,
            Popup::Rename,
            Popup::Confirm(ConfirmKind::Quit),
            Popup::Confirm(ConfirmKind::Switch { target: 1 }),
            Popup::Confirm(ConfirmKind::Delete {
                profile_id: "a".to_owned(),
                display_name: "office".to_owned(),
            }),
            Popup::Error("connect failed: boom\ncaused by: test".to_owned()),
        ];
        for popup in popups {
            app.popup = popup;
            let _ = render_text(&app, 80, 24);
        }
    }

    #[test]
    fn quit_confirm_text_is_verbatim() {
        let mut app = fixture_app();
        app.popup = Popup::Confirm(ConfirmKind::Quit);
        let screen = render_text(&app, 80, 24);
        assert!(
            screen.contains("active tunnel will stay up after quit. Quit anyway? y/N"),
            "quit confirm not verbatim"
        );
    }

    #[test]
    fn sudo_input_is_masked_never_plaintext() {
        let mut app = fixture_app();
        app.popup = Popup::Sudo;
        app.sudo_input = "s3cret-pw".to_owned();
        let screen = render_text(&app, 80, 24);
        assert!(!screen.contains("s3cret-pw"), "sudo password leaked");
    }

    #[test]
    fn ovpn_panel_shows_daemon_warn_last_exit_and_tail() {
        let mut app = fixture_app();
        app.selected = 1;
        app.focus = Focus::Detail;
        app.phase = ConnPhase::Connected;
        app.active = Some(ActiveConn {
            profile_id: "b".to_owned(),
            kind: VpnType::OpenVpn,
            started_at: "2026-10-06T12:00:00Z".to_owned(),
            last_exit_ip: Some("9.9.9.9".to_owned()),
        });
        app.ovpn_daemon_warn = true;
        for i in 0..30 {
            app.ovpn_logs.push(format!("log line {i:02}"));
        }
        // Follow pins the tail: the last line must be visible, the first not.
        let screen = render_text(&app, 80, 24);
        assert!(screen.contains("daemon"), "daemon warn missing");
        assert!(screen.contains("9.9.9.9"), "last exit missing");
        assert!(screen.contains("log line 29"), "tail missing under follow");
        assert!(!screen.contains("log line 00"), "head shown under follow");

        // No follow honors the scroll offset from the top.
        app.log_follow = false;
        app.detail_scroll = 0;
        let screen = render_text(&app, 80, 24);
        assert!(screen.contains("log line 00"), "scroll offset ignored");
    }

    #[test]
    fn renders_without_panic_when_narrow_or_empty() {
        let empty = App {
            profiles: Vec::new(),
            ..fixture_app()
        };
        let _ = render_text(&empty, 80, 24);
        let _ = render_text(&empty, 40, 12);
        let _ = render_text(&fixture_app(), 50, 10);
        let _ = render_text(&fixture_app(), 10, 5);
    }
}
