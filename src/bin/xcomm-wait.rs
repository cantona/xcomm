//! `xcomm ctl wait` and `xcomm ctl run`: `xcomm` execs this with its own
//! arguments, so the regex engine is not in the xcomm binary.

use std::process::ExitCode;
use xcomm::cli::{self, Command};
use xcomm::out;

fn env(name: &str) -> Option<String> {
    std::env::var_os(name).map(|v| v.to_string_lossy().into_owned())
}

fn main() -> ExitCode {
    match cli::parse(std::env::args_os().skip(1), &env) {
        Ok(Command::Ctl(args)) => ExitCode::from(xcomm::ctl::wait::run(&args, &env)),
        Ok(_) => {
            out::report("xcomm-wait runs xcomm ctl wait and xcomm ctl run");
            ExitCode::from(2)
        }
        Err(e) => {
            out::report(&format!(
                "{}\nTry 'xcomm --help' for more information.",
                e.0
            ));
            ExitCode::from(2)
        }
    }
}
