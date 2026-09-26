use std::io::{Read, Write};
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
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
            crate::daemon::run()
        }
        Command::Toggle => send_verb("toggle"),
        Command::Show => send_verb("show"),
        Command::Hide => send_verb("hide"),
        Command::Clipboard => send_verb("clipboard"),
        Command::Add { file } => send_verb(&format!("add {}", file.display())),
        Command::Stop => send_verb("stop"),
        Command::Status => send_verb("status"),
    }
}

pub fn socket_path() -> PathBuf {
    let runtime = std::env::var_os("XDG_RUNTIME_DIR").unwrap_or_default();
    PathBuf::from(runtime).join("impin.sock")
}

fn send_verb(verb: &str) -> anyhow::Result<()> {
    let mut stream = UnixStream::connect(socket_path()).map_err(|_| {
        anyhow::anyhow!(
            "impin not running (socket {}); start it with: impin daemon start",
            socket_path().display()
        )
    })?;
    stream.write_all(verb.as_bytes())?;
    stream.shutdown(Shutdown::Write)?;
    let mut reply = String::new();
    stream.read_to_string(&mut reply)?;
    let reply = reply.trim();
    if let Some(rest) = reply.strip_prefix("ok") {
        let rest = rest.trim();
        if !rest.is_empty() {
            println!("{rest}");
        }
        Ok(())
    } else if let Some(rest) = reply.strip_prefix("err") {
        anyhow::bail!("{}", rest.trim())
    } else {
        anyhow::bail!("malformed reply: {reply:?}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cli_definition_is_valid() {
        Cli::command().debug_assert();
    }
}
