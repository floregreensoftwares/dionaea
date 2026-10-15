use std::io::Write;
use std::process::ExitCode;

fn main() -> ExitCode {
    let mut stdout = std::io::stdout().lock();
    match std::env::args().nth(1).as_deref() {
        Some("--version" | "-V") => {
            let _ = writeln!(stdout, "dionaea {}", env!("CARGO_PKG_VERSION"));
            ExitCode::SUCCESS
        }
        _ => {
            let _ = writeln!(stdout, "dionaea {}", env!("CARGO_PKG_VERSION"));
            let _ = writeln!(stdout, "No commands are available yet.");
            ExitCode::FAILURE
        }
    }
}
