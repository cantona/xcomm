use std::process::ExitCode;
use xcomm::cli::{self, Command};
use xcomm::out;
use xcomm::session::{self, ExitReason, Failure};

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
            out::report(&format!(
                "{}\nTry 'xcomm --help' for more information.",
                e.0
            ));
            ExitCode::from(2)
        }
        Ok(Command::Help(prefix, broken)) => print(
            cli::help(
                env!("CARGO_PKG_VERSION"),
                &prefix,
                broken.as_deref(),
                &session::commands(&prefix.key.display()),
            )
            .trim_end(),
        ),
        Ok(Command::Version) => print(&format!("xcomm {}", env!("CARGO_PKG_VERSION"))),
        Ok(Command::ShowPrefix(p)) => print(&p.describe()),
        Ok(Command::Ctl(args)) => ExitCode::from(xcomm::ctl::client::run(&args, &env)),
        Ok(Command::Term(opts)) => match session::run(&opts, &env) {
            Ok(ExitReason::DeviceGone) => ExitCode::FAILURE,
            Ok(_) => ExitCode::SUCCESS,
            Err(Failure::Busy(msg)) => {
                out::report(&msg);
                ExitCode::from(2)
            }
            Err(Failure::Other(msg)) => {
                out::report(&msg);
                ExitCode::FAILURE
            }
        },
    }
}
