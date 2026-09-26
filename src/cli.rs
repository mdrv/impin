use std::path::PathBuf;

use clap::{CommandFactory, Parser, Subcommand};

/// Floating reference-image pins for creative work.
#[derive(Parser, Debug)]
#[command(name = "impin", version, about)]
pub struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Run the daemon (owns the pin windows)
    Daemon {
        #[command(subcommand)]
        cmd: Daemon,
    },
    /// Toggle all pins (hide if shown, show if hidden)
    Toggle,
    /// Show all pins
    Show,
    /// Hide all pins (state kept)
    Hide,
    /// Pin the image currently on the clipboard
    Clipboard,
    /// Pin an image file
    Add {
        /// Path to the image file
        file: PathBuf,
    },
    /// Stop the daemon (state saved)
    Stop,
    /// Print daemon status
    Status,
}

#[derive(Subcommand, Debug)]
pub enum Daemon {
    /// Run in the foreground
    Start {
        /// Kept for symmetry; foreground is the only mode
        #[arg(long)]
        foreground: bool,
    },
}

pub fn run() -> anyhow::Result<()> {
    let Some(command) = Cli::parse().command else {
        let _ = Cli::command().print_help();
        return Ok(());
    };
    match command {
        Command::Daemon {
            cmd: Daemon::Start { foreground },
        } => {
            if !foreground {
                log::debug!("--foreground not passed; foreground is the only mode");
            }
            unimplemented_verb("daemon start")
        }
        Command::Toggle => unimplemented_verb("toggle"),
        Command::Show => unimplemented_verb("show"),
        Command::Hide => unimplemented_verb("hide"),
        Command::Clipboard => unimplemented_verb("clipboard"),
        Command::Add { file } => unimplemented_verb(&format!("add {}", file.display())),
        Command::Stop => unimplemented_verb("stop"),
        Command::Status => unimplemented_verb("status"),
    }
}

pub fn socket_path() -> PathBuf {
    let runtime = std::env::var_os("XDG_RUNTIME_DIR").unwrap_or_default();
    PathBuf::from(runtime).join("impin.sock")
}

fn unimplemented_verb(verb: &str) -> anyhow::Result<()> {
    anyhow::bail!(
        "{verb}: daemon IPC lands in M1 (socket {})",
        socket_path().display()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cli_definition_is_valid() {
        Cli::command().debug_assert();
    }
}
