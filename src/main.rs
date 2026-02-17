mod nix;
mod ui;

use std::io::{self, Write};
use std::process::Command;

use clap::Parser;
use ratatui::Viewport;

#[derive(Parser)]
#[command(name = "nix-search", about = "Interactive fuzzy search for nixpkgs")]
struct Args {
    /// Height of the TUI in terminal rows
    #[arg(long, default_value_t = 36)]
    height: u16,

    /// Use fullscreen viewport
    #[arg(short, long)]
    full: bool,

    /// Flake to search
    #[arg(long, default_value = "nixpkgs")]
    flake: String,

    /// Install the selected package via `nix profile add`
    #[arg(short, long)]
    add: bool,
}

fn main() -> io::Result<()> {
    let args = Args::parse();

    let viewport = if args.full {
        Viewport::Fullscreen
    } else {
        Viewport::Inline(args.height)
    };

    if let Some(selected) = ui::run(&args.flake, viewport)? {
        let installable = format!("{}#{}", args.flake, selected);

        if args.add {
            eprintln!("Installing {installable}...");
            let status = Command::new("nix")
                .args(["profile", "add", &installable])
                .status()?;
            if !status.success() {
                std::process::exit(status.code().unwrap_or(1));
            }
        } else {
            let stdout = io::stdout();
            let mut out = stdout.lock();
            writeln!(out, "{selected}")?;
        }
    }

    Ok(())
}
