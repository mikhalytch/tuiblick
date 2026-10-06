//! Minimal placeholder renderer (Task 7 linkage).
//!
//! Task 8 (`ui`) overwrites this file fully with the real layout: left
//! profile list, right detail (AWG status / OpenVPN logs), header
//! `init → exit`, bottom status bar, and all popups.
use ratatui::widgets::Paragraph;

use crate::app::App;

/// Placeholder: proves the `app → ui` linkage compiles and draws without
/// panicking. Shows profile count/selection/phase plus the status line.
pub fn render(frame: &mut ratatui::Frame, app: &App) {
    let area = frame.area();
    let text = format!(
        "tuiblick (UI lands in Task 8)\nprofiles: {}  selected: {}  phase: {:?}\nsudo: {}\n{}",
        app.profiles.len(),
        app.selected,
        app.phase,
        app.sudo_status_text(),
        app.status_message,
    );
    frame.render_widget(Paragraph::new(text), area);
}
