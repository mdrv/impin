#[cfg(unix)]
use std::io::{Read, Write};
#[cfg(unix)]
use std::net::Shutdown;
#[cfg(unix)]
use std::os::unix::net::UnixStream;
use std::path::PathBuf;

#[cfg(target_os = "windows")]
use anyhow::Context as _;
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
    #[cfg(windows)]
    {
        // The fixed named pipe (display only; the pipe IS the endpoint).
        PathBuf::from(r"\\.\pipe\impin")
    }
    #[cfg(not(windows))]
    {
        // Linux: $XDG_RUNTIME_DIR per spec. macOS leaves it unset; the
        // per-user temp dir is the equivalent per-user runtime base.
        match std::env::var_os("XDG_RUNTIME_DIR").filter(|v| !v.is_empty()) {
            Some(dir) => PathBuf::from(dir).join("impin.sock"),
            None => std::env::temp_dir().join("impin.sock"),
        }
    }
}

fn send_verb(verb: &str) -> anyhow::Result<()> {
    let reply = send_verb_raw(verb)?;
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

#[cfg(unix)]
fn send_verb_raw(verb: &str) -> anyhow::Result<String> {
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
    Ok(reply)
}

/// Windows: open `\\.\pipe\impin` duplex, write u32-LE length + verb, flush
/// (completes the verb — a byte pipe can't half-close like shutdown(Write)),
/// then read until the server disconnects (EOF = end of reply).
#[cfg(windows)]
fn send_verb_raw(verb: &str) -> anyhow::Result<String> {
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::{CloseHandle, HANDLE};
    use windows::Win32::Storage::FileSystem::{
        CreateFileW, FlushFileBuffers, ReadFile, WriteFile, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_READ,
        FILE_SHARE_WRITE, OPEN_EXISTING,
    };

    // GENERIC_READ | GENERIC_WRITE (kept raw: the windows-rs const names
    // for access rights shift between releases).
    const GENERIC_READ_WRITE: u32 = 0x8000_0000 | 0x4000_0000;

    let name: Vec<u16> = "\\\\.\\pipe\\impin\0".encode_utf16().collect();
    let pipe: HANDLE = unsafe {
        CreateFileW(
            PCWSTR(name.as_ptr()),
            GENERIC_READ_WRITE,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            None,
            OPEN_EXISTING,
            FILE_ATTRIBUTE_NORMAL,
            None,
        )
    }
    .map_err(|_| {
        anyhow::anyhow!("impin not running (\\\\.\\pipe\\impin); start it with: impin daemon start")
    })?;

    let result = (|| -> anyhow::Result<String> {
        let mut wire = Vec::with_capacity(verb.len() + 4);
        wire.extend_from_slice(&(verb.len() as u32).to_le_bytes());
        wire.extend_from_slice(verb.as_bytes());
        let mut written: u32 = 0;
        unsafe { WriteFile(pipe, Some(&wire), Some(&mut written), None) }
            .context("writing to the impin pipe")?;
        let _ = unsafe { FlushFileBuffers(pipe) };
        // Read until the server disconnects (broken pipe = EOF here).
        let mut reply = String::new();
        let mut buf = [0u8; 4096];
        loop {
            let mut n: u32 = 0;
            match unsafe { ReadFile(pipe, Some(&mut buf), Some(&mut n), None) } {
                Ok(()) if n > 0 => {
                    reply.push_str(&String::from_utf8_lossy(&buf[..n as usize]));
                }
                _ => break,
            }
        }
        Ok(reply)
    })();
    let _ = unsafe { CloseHandle(pipe) };
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cli_definition_is_valid() {
        Cli::command().debug_assert();
    }
}
