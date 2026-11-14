use std::process::ExitCode;
use xcomm::out;

fn main() -> ExitCode {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    if args.len() == 1 && args[0] == "--version" {
        let line = format!("xcomm {}\n", env!("CARGO_PKG_VERSION"));
        if let Err(e) = out::stdout(line.as_bytes()) {
            out::report(&format!("cannot write to stdout: {e}"));
            return ExitCode::FAILURE;
        }
        return ExitCode::SUCCESS;
    }
    out::report("usage: xcomm [options] <device>");
    ExitCode::from(2)
}
