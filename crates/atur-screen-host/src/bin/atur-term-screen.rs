//! `atur-term-screen`: shares this Mac's screen. Started by the Atur Terminal
//! server (or `atur-term screen`), never by hand; see the `atur-screen-source`
//! crate for how and why.

use std::io::{self, Write};
use std::process::ExitCode;

use atur_screen_host::options;

/// Logs to standard error, which Launch Services sends nowhere unless asked.
struct StderrLogger;

impl log::Log for StderrLogger {
    fn enabled(&self, metadata: &log::Metadata<'_>) -> bool {
        metadata.level() <= log::Level::Info
    }

    fn log(&self, record: &log::Record<'_>) {
        if self.enabled(record.metadata()) {
            let _ = writeln!(io::stderr(), "{}: {}", record.level(), record.args());
        }
    }

    fn flush(&self) {}
}

fn main() -> ExitCode {
    let mode = match options::parse(std::env::args().skip(1)) {
        Ok(mode) => mode,
        Err(reason) => {
            let _ = writeln!(io::stderr(), "atur-term-screen: {reason}");
            return ExitCode::from(2);
        }
    };
    if log::set_logger(&StderrLogger).is_ok() {
        log::set_max_level(log::LevelFilter::Info);
    }
    #[cfg(target_os = "macos")]
    {
        atur_screen_host::mac::run::main(mode)
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = mode;
        let _ = writeln!(
            io::stderr(),
            "atur-term-screen shares the screen of a Mac and only runs on macOS."
        );
        ExitCode::from(1)
    }
}
