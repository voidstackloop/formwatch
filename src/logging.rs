//! Structured logging setup.
//!
//! formwatch's user-facing output (colored text, `--json`, HTML) stays on
//! **stdout** exactly as before. Everything diagnostic — retries,
//! timeouts, browser lifecycle, notification delivery — goes through the
//! `tracing` facade to **stderr**, so it never corrupts a machine-readable
//! stdout stream. `--log-format json` switches the diagnostics to
//! newline-delimited JSON for log shippers.
//!
//! Default level is `WARN`. `-v` raises it to `INFO`, `-vv` to `DEBUG`;
//! `--quiet` drops it to `ERROR`.

use clap::ValueEnum;
use std::io::IsTerminal;
use tracing::Level;
use tracing_subscriber::fmt;

/// How diagnostic logs are rendered on stderr.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, ValueEnum)]
pub enum LogFormat {
    /// Human-readable, colorized when stderr is a terminal.
    #[default]
    Text,
    /// Newline-delimited JSON, one object per event.
    Json,
}

/// Installs the global tracing subscriber. Safe to call once; subsequent
/// calls (e.g. from tests) are ignored rather than panicking.
pub fn init(verbosity: u8, quiet: bool, format: LogFormat) {
    let level = if quiet {
        Level::ERROR
    } else {
        match verbosity {
            0 => Level::WARN,
            1 => Level::INFO,
            _ => Level::DEBUG,
        }
    };

    let ansi = std::io::stderr().is_terminal();
    let builder = fmt::Subscriber::builder()
        .with_max_level(level)
        .with_writer(std::io::stderr)
        .with_ansi(ansi)
        .with_target(true);

    match format {
        LogFormat::Json => {
            let subscriber = builder.json().finish();
            let _ = tracing::subscriber::set_global_default(subscriber);
        }
        LogFormat::Text => {
            let subscriber = builder.finish();
            let _ = tracing::subscriber::set_global_default(subscriber);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn log_format_parses_its_wire_names() {
        assert_eq!(
            LogFormat::from_str("json", true).expect("json"),
            LogFormat::Json
        );
        assert_eq!(
            LogFormat::from_str("text", true).expect("text"),
            LogFormat::Text
        );
    }
}
