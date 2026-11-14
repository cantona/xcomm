use std::process::ExitCode;
use xcomm::cli::{self, Command};
use xcomm::out;

fn env(name: &str) -> Option<String> {
    // Lossy, not `var().ok()`: a non-UTF-8 value must reach the parser and fail
    // there instead of being treated as unset.
    std::env::var_os(name).map(|v| v.to_string_lossy().into_owned())
}

fn print(line: &str) -> ExitCode {
    match out::stdout(format!("{line}\n").as_bytes()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            out::report(&format!("cannot write to stdout: {e}"));
            ExitCode::FAILURE
        }
    }
}

fn main() -> ExitCode {
    match cli::parse(std::env::args_os().skip(1), &env) {
        Err(e) => {
            out::report(&e.0);
            ExitCode::from(2)
        }
        Ok(Command::Version) => print(&format!("xcomm {}", env!("CARGO_PKG_VERSION"))),
        Ok(Command::ShowPrefix(p)) => print(&p.describe()),
        Ok(Command::Term(_)) => {
            out::report("terminal sessions are not implemented yet");
            ExitCode::FAILURE
        }
    }
}
