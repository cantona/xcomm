use std::process::ExitCode;
use xcomm::cli::{self, Command, CtlCmd};
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

/// `ctl wait` and `ctl run` match regexes: xcomm-wait, beside this binary,
/// does that, so the regex engine is not in every session's memory.
fn exec_wait() -> ExitCode {
    use std::os::unix::process::CommandExt;
    let helper = match std::env::current_exe().and_then(std::fs::canonicalize) {
        Ok(exe) => exe.with_file_name(xcomm::ctl::WAIT_HELPER),
        Err(e) => {
            out::report(&format!("cannot find the xcomm binary's path: {e}"));
            return ExitCode::from(2);
        }
    };
    let e = std::process::Command::new(&helper)
        .arg0(xcomm::ctl::WAIT_HELPER)
        .args(std::env::args_os().skip(1))
        .exec();
    // Not 1: that is a wait that timed out, which an agent would retry.
    out::report(&format!(
        "{}: {e}; xcomm ctl wait and run need it installed beside xcomm",
        helper.display()
    ));
    ExitCode::from(2)
}

fn list(json: bool) -> ExitCode {
    let ports = match xcomm::list::enumerate(&env) {
        Ok(ports) => ports,
        Err(e) => {
            out::report(&format!("cannot list ports: {e}"));
            return ExitCode::FAILURE;
        }
    };
    let text = if json {
        let mut line = serde_json::to_string(&ports).expect("ports serialize");
        line.push('\n');
        line
    } else {
        xcomm::list::table(&ports)
    };
    if let Err(e) = out::stdout(text.as_bytes()) {
        out::report(&format!("cannot write to stdout: {e}"));
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
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
        Ok(Command::Ctl(args)) if matches!(args.cmd, CtlCmd::Wait { .. } | CtlCmd::Run { .. }) => {
            exec_wait()
        }
        Ok(Command::Ctl(args)) => ExitCode::from(xcomm::ctl::client::run(&args, &env)),
        Ok(Command::List { json }) => list(json),
        Ok(Command::Pick) => {
            use std::os::fd::AsFd;
            let (stdin, stdout) = (std::io::stdin(), std::io::stdout());
            match xcomm::pick::run(stdin.as_fd(), stdout.as_fd(), &env) {
                Ok(code) => ExitCode::from(u8::try_from(code).unwrap_or(1)),
                Err(e) => {
                    out::report(&format!("pick: {e}"));
                    ExitCode::FAILURE
                }
            }
        }
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
