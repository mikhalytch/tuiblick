mod app;
mod awg;
mod config;
mod errors;
mod net;
mod openvpn;
mod sudo;
mod ui;

use crossterm::{
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use std::io::stdout;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    enable_raw_mode()?;
    let mut out = stdout();
    if let Err(e) = execute!(out, EnterAlternateScreen) {
        let _ = disable_raw_mode();
        return Err(e.into());
    }

    let result = app::run().await;

    // Restore the terminal on all exits (success or backend error).
    let _ = disable_raw_mode();
    let _ = execute!(out, LeaveAlternateScreen);

    result
}
