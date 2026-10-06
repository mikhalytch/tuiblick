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
    let args = parse_args(std::env::args().skip(1));
    if args.help {
        println!("{HELP}");
        return Ok(());
    }

    enable_raw_mode()?;
    let mut out = stdout();
    if let Err(e) = execute!(out, EnterAlternateScreen) {
        let _ = disable_raw_mode();
        return Err(e.into());
    }

    let result = app::run(args.connect_all).await;

    // Restore the terminal on all exits (success or backend error).
    let _ = disable_raw_mode();
    let _ = execute!(out, LeaveAlternateScreen);

    result
}

/// CLI flags, parsed by hand (one flag today; no arg-parser dependency).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
struct Args {
    /// `--connect-all` / `-C`: run the `C` connect-all batch right after
    /// startup (same flow as pressing `C`: skips busy/missing, stops on
    /// the first real failure, never quits by itself).
    connect_all: bool,
    /// `--help` / `-h`: print usage and exit.
    help: bool,
}

const HELP: &str = "\
tuiblick — terminal UI for AmneziaWG and OpenVPN connections

Usage: tuiblick [OPTIONS]

Options:
  -C, --connect-all   connect every profile in list order at startup
                      (exactly as if you pressed C: skips busy/missing,
                      stops on the first real failure)
  -h, --help          print this help and exit

Keys inside the app: press ? for the full table.";

fn parse_args(args: impl Iterator<Item = String>) -> Args {
    let mut out = Args::default();
    for arg in args {
        match arg.as_str() {
            "-C" | "--connect-all" => out.connect_all = true,
            "-h" | "--help" => out.help = true,
            _ => {}
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_connect_all_flags() {
        assert_eq!(
            parse_args(["-C".to_owned()].into_iter()),
            Args {
                connect_all: true,
                help: false,
            }
        );
        assert_eq!(
            parse_args(["--connect-all".to_owned()].into_iter()),
            Args {
                connect_all: true,
                help: false,
            }
        );
        assert_eq!(
            parse_args([].into_iter()),
            Args {
                connect_all: false,
                help: false,
            }
        );
    }

    #[test]
    fn parses_help_and_ignores_unknown() {
        assert_eq!(
            parse_args(["-h".to_owned()].into_iter()),
            Args {
                connect_all: false,
                help: true,
            }
        );
        assert_eq!(
            parse_args(["--help".to_owned(), "--frobnicate".to_owned()].into_iter()),
            Args {
                connect_all: false,
                help: true,
            }
        );
    }

    #[test]
    fn help_lists_cli_flags() {
        assert!(HELP.contains("--connect-all"));
        assert!(HELP.contains("-C"));
        assert!(HELP.contains("--help"));
    }
}
