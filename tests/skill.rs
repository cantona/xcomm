//! The agent skill: what `--skill` prints, `skill install`, and that the
//! skill's commands are ones xcomm takes - each parsed by the real parser,
//! each example run against a fake device.

mod support;

use std::collections::HashMap;
use std::ffi::OsString;
use std::os::fd::{AsFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use rustix::event::{PollFd, PollFlags, poll};
use support::remote::{Remote, need};
use support::{lock_dir, read_until, spawn_in_pty, write_all};
use xcomm::skill::{SKILL_MD, commands_in, default_dir, install, render_for_install};

const T: Duration = Duration::from_secs(30);

fn scratch(tag: &str) -> PathBuf {
    let d =
        std::env::temp_dir().join(format!("xcomm-skill-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

#[test]
fn skill_prints_the_skill() {
    let o = Command::new(env!("CARGO_BIN_EXE_xcomm"))
        .arg("--skill")
        .output()
        .unwrap();
    assert_eq!(o.status.code(), Some(0));
    assert_eq!(String::from_utf8(o.stdout).unwrap(), SKILL_MD);
    let o = Command::new(env!("CARGO_BIN_EXE_xcomm"))
        .args(["--skill", "x"])
        .output()
        .unwrap();
    assert_eq!(o.status.code(), Some(2));
}

#[test]
fn the_front_matter_names_the_skill() {
    let front = SKILL_MD
        .strip_prefix("---\n")
        .and_then(|r| r.split_once("\n---\n"))
        .unwrap()
        .0;
    assert!(front.lines().any(|l| l == "name: xcomm"));
    // Quoted: unquoted, the ": " in it is a YAML mapping and the whole front
    // matter fails to parse.
    let d = front
        .lines()
        .find_map(|l| l.strip_prefix("description: "))
        .unwrap();
    assert!(d.starts_with('"') && d.ends_with('"'), "{d}");
    assert!(!d[1..d.len() - 1].contains('"'), "{d}");
}

/// `argv` with `<dev>`, the other placeholders and the shell variables
/// filled in.
fn filled(argv: &[String]) -> Vec<String> {
    let samples: HashMap<&str, &str> = [("<dev>", "ttyFAKE"), ("$off", "0")].into();
    argv.iter()
        .map(|w| {
            let mut w = w.clone();
            for (k, v) in &samples {
                w = w.replace(k, v);
            }
            assert!(
                !(w.contains('<') && w.contains('>')) && !w.starts_with('$'),
                "a placeholder or variable with no sample: {w} in {argv:?}"
            );
            w
        })
        .collect()
}

#[test]
fn every_command_in_the_skill_parses() {
    let commands = commands_in(SKILL_MD);
    // The fences were found: a parser that found none would pass anything.
    assert!(commands.len() >= 20, "{}: {commands:?}", commands.len());
    let env = |_: &str| None;
    for argv in &commands {
        let args = filled(argv);
        let parsed = xcomm::cli::parse(args[1..].iter().map(OsString::from), &env);
        assert!(parsed.is_ok(), "{args:?}: {:?}", parsed.err());
    }
}

#[test]
fn no_xcomm_command_in_the_skill_is_missed() {
    // Every `xcomm` in a command's place in a shell block is one the drift
    // test checks: a form commands_in does not know would be skipped.
    let re = regex::Regex::new(r"(?m-u)(^|[\s;&|(`])xcomm\s").unwrap();
    let mut blocks = String::new();
    let mut in_shell = None;
    for line in SKILL_MD.lines() {
        if let Some(lang) = line.trim_start().strip_prefix("```") {
            in_shell = match in_shell {
                None => Some(matches!(lang.trim(), "sh" | "bash")),
                Some(_) => None,
            };
        } else if in_shell == Some(true) {
            blocks.push_str(line);
            blocks.push('\n');
        }
    }
    assert_eq!(re.find_iter(&blocks).count(), commands_in(SKILL_MD).len());
}

#[test]
fn the_exit_codes_and_names_are_xcomms() {
    use xcomm::ctl::code;
    let rows: Vec<(u8, &str)> = SKILL_MD
        .lines()
        .filter_map(|l| {
            let rest = l.strip_prefix("| ")?;
            let (n, rest) = rest.split_once(" | ")?;
            Some((n.parse().ok()?, rest))
        })
        .collect();
    let codes: Vec<u8> = rows.iter().map(|(n, _)| *n).collect();
    assert_eq!(codes, (0..=9).collect::<Vec<u8>>());
    for (n, words) in [
        (code::OK, "ok"),
        (code::FAILED, "timed out"),
        (code::USAGE, "usage error"),
        (code::NO_SESSION, "no session"),
        (code::DISCONNECTED, "disconnected"),
        (code::BUSY, "file transfer"),
        (code::EVICTED, "evicted"),
        (code::FAIL_ON, "--fail-on"),
        (code::DENIED, "--ctl ro"),
        (code::DROPPED, "dropped"),
    ] {
        let row = rows[usize::from(n)].1;
        assert!(row.contains(words), "{n}: {row}");
    }
    // The herdr plugin and action the skill names.
    let manifest: toml::Table = toml::from_str(
        &std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/herdr-plugin.toml"
        ))
        .unwrap(),
    )
    .unwrap();
    assert_eq!(manifest["id"].as_str(), Some(xcomm::pick::PLUGIN_ID));
    assert!(SKILL_MD.contains(&format!(
        "invoke --plugin {} open\n",
        xcomm::pick::PLUGIN_ID
    )));
    assert!(
        manifest["actions"]
            .as_array()
            .unwrap()
            .iter()
            .any(|a| a["id"].as_str() == Some("open"))
    );
}

#[test]
fn commands_are_found_as_sh_reads_them() {
    let md = "\
text xcomm ctl d status
```sh
xcomm ctl d wait 'Hit any key' --timeout 60 && xcomm ctl d send -n ' '
xcomm ctl --json d run \"a \\\"b\\\"\" --until x | jq -r .output
xcomm --headless d &  # comment
xcomm ctl d send \\
  'two lines'
herdr plugin action invoke --plugin p open
off=$(xcomm ctl d status | jq .offset)
FOO=1 xcomm ctl d read > out.txt 2>&1
if xcomm ctl d status; then echo `xcomm ctl d read`; fi
echo 'not \\
xcomm ctl d status   # a comment \\
xcomm ctl <dev> read --since <offset>
```
```text
xcomm not this
```
```bash
xcomm ctl d read;xcomm ctl d status
```
";
    let got = commands_in(md);
    let want: Vec<Vec<&str>> = vec![
        vec![
            "xcomm",
            "ctl",
            "d",
            "wait",
            "Hit any key",
            "--timeout",
            "60",
        ],
        vec!["xcomm", "ctl", "d", "send", "-n", " "],
        vec![
            "xcomm", "ctl", "--json", "d", "run", "a \"b\"", "--until", "x",
        ],
        vec!["xcomm", "--headless", "d"],
        vec!["xcomm", "ctl", "d", "send", "two lines"],
        vec!["xcomm", "ctl", "d", "status"],
        vec!["xcomm", "ctl", "d", "read"],
        vec!["xcomm", "ctl", "d", "status"],
        vec!["xcomm", "ctl", "d", "read"],
        vec!["xcomm", "ctl", "d", "status"],
        vec!["xcomm", "ctl", "<dev>", "read", "--since", "<offset>"],
        vec!["xcomm", "ctl", "d", "read"],
        vec!["xcomm", "ctl", "d", "status"],
    ];
    assert_eq!(got, want);
}

#[test]
fn install_writes_the_marker_and_replaces_only_its_own() {
    let dir = scratch("install").join("skills/xcomm");
    let path = install(&dir).unwrap();
    assert_eq!(path, dir.join("SKILL.md"));
    let text = std::fs::read_to_string(&path).unwrap();
    assert_eq!(text, render_for_install());
    assert!(text.contains(&format!(
        "metadata:\n  generated-by: xcomm {}\n---\n",
        env!("CARGO_PKG_VERSION")
    )));
    // The body is the skill's, word for word.
    assert!(text.ends_with(SKILL_MD.split_once("\n---\n").unwrap().1));
    // Again: its own copy is replaced.
    std::fs::write(&path, text.replace("# xcomm", "# old")).unwrap();
    install(&dir).unwrap();
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        render_for_install()
    );
    // A SKILL.md someone wrote is left as it is.
    let mine = "---\nname: xcomm\n---\nmy notes\n";
    std::fs::write(&path, mine).unwrap();
    let e = install(&dir).unwrap_err();
    assert!(e.0.contains("not one xcomm installed"), "{e}");
    assert_eq!(std::fs::read_to_string(&path).unwrap(), mine);
    // No temporary file is left behind.
    let names: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    assert_eq!(names, ["SKILL.md"]);
    let _ = std::fs::remove_dir_all(dir.parent().unwrap().parent().unwrap());
}

#[test]
fn install_leaves_links_alone_and_keeps_the_mode() {
    use std::os::unix::fs::{PermissionsExt, symlink};
    let root = scratch("install-links");
    // A marked SKILL.md that is a link: replacing the link would leave its
    // target as it was.
    let dir = root.join("a");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(root.join("real.md"), render_for_install()).unwrap();
    symlink(root.join("real.md"), dir.join("SKILL.md")).unwrap();
    let e = install(&dir).unwrap_err();
    assert!(e.0.contains("not a plain file"), "{e}");
    assert!(
        dir.join("SKILL.md")
            .symlink_metadata()
            .unwrap()
            .file_type()
            .is_symlink()
    );
    // The temporary name is taken, by a link or a file a killed install
    // left: neither is followed or in the way.
    let dir = root.join("b");
    std::fs::create_dir_all(&dir).unwrap();
    let victim = root.join("victim");
    std::fs::write(&victim, "keep").unwrap();
    let taken = dir.join(format!(".SKILL.md.{}.0", std::process::id()));
    symlink(&victim, &taken).unwrap();
    std::fs::write(
        dir.join(format!(".SKILL.md.{}.1", std::process::id())),
        "old",
    )
    .unwrap();
    install(&dir).unwrap();
    assert_eq!(std::fs::read_to_string(&victim).unwrap(), "keep");
    assert!(taken.symlink_metadata().unwrap().file_type().is_symlink());
    assert_eq!(
        std::fs::read_to_string(dir.join("SKILL.md")).unwrap(),
        render_for_install()
    );
    // The mode of a copy it replaces stays.
    let dir = root.join("c");
    let path = install(&dir).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640)).unwrap();
    install(&dir).unwrap();
    let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o640);
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn the_default_dir_is_not_in_a_git_work_tree() {
    let home = scratch("home");
    let env = |k: &str| (k == "HOME").then(|| home.display().to_string());
    assert_eq!(
        default_dir(&env).unwrap(),
        home.join(".agents/skills/xcomm")
    );
    // ~/.agents a link into a dotfiles repository.
    let dotfiles = home.join("dotfiles");
    std::fs::create_dir_all(dotfiles.join(".git")).unwrap();
    std::fs::create_dir_all(dotfiles.join("agents/skills")).unwrap();
    std::os::unix::fs::symlink(dotfiles.join("agents"), home.join(".agents")).unwrap();
    let e = default_dir(&env).unwrap_err();
    assert!(
        e.0.contains("git work tree") && e.0.contains("--dir"),
        "{e}"
    );
    // A repository kept apart from $HOME, as the bare-repo way, yadm and
    // vcsh do, or one the environment names.
    let repo = |git_dir: &Path, config: &str| {
        std::fs::create_dir_all(git_dir.join("objects")).unwrap();
        std::fs::write(git_dir.join("HEAD"), "ref: refs/heads/main\n").unwrap();
        std::fs::write(git_dir.join("config"), config).unwrap();
    };
    let managed = |make: &dyn Fn(&Path),
                   xdg_config: Option<&str>|
     -> Result<PathBuf, String> {
        let home = scratch("home-repo");
        make(&home);
        let env = |k: &str| match k {
            "HOME" => Some(home.display().to_string()),
            "XDG_CONFIG_HOME" => xdg_config.map(|c| home.join(c).display().to_string()),
            _ => None,
        };
        let got = default_dir(&env).map_err(|e| e.0);
        let _ = std::fs::remove_dir_all(&home);
        got
    };
    // A mirror or other bare clone in $HOME manages nothing.
    assert!(
        managed(
            &|h| repo(
                &h.join("linux.git"),
                "[core]\n\tbare = true\n[remote \"origin\"]\n\tmirror = true\n"
            ),
            None
        )
        .is_ok()
    );
    // The bare-repo recipe: --work-tree=$HOME on the command line, untracked
    // files hidden in the config.
    let e = managed(
        &|h| {
            repo(
                &h.join(".cfg"),
                "[core]\n\tbare = true\n[status]\n\tshowUntrackedFiles = no\n",
            )
        },
        None,
    )
    .unwrap_err();
    assert!(e.contains(".cfg"), "{e}");
    // yadm and vcsh: not bare, the work tree set to $HOME.
    let e = managed(
        &|h| {
            repo(
                &h.join(".local/share/yadm/repo.git"),
                &format!("[core]\n\tbare = false\n\tworktree = {}\n", h.display()),
            )
        },
        None,
    )
    .unwrap_err();
    assert!(e.contains("yadm"), "{e}");
    let e = managed(
        &|h| {
            repo(
                &h.join("conf/vcsh/repo.d/zsh.git"),
                &format!("[core]\n\tbare = false\n\tworktree = \"{}\"\n", h.display()),
            )
        },
        Some("conf"),
    )
    .unwrap_err();
    assert!(e.contains("vcsh"), "{e}");
    let home3 = scratch("home-env");
    let env3 = |k: &str| match k {
        "HOME" => Some(home3.display().to_string()),
        "GIT_DIR" => Some("/x/repo.git".into()),
        _ => None,
    };
    assert!(default_dir(&env3).unwrap_err().0.contains("GIT_DIR"));
    let _ = std::fs::remove_dir_all(&home3);
    for bad in [None, Some(String::new()), Some("relative".to_string())] {
        assert!(default_dir(&|_: &str| bad.clone()).is_err(), "{bad:?}");
    }
    // The command says so and installs nothing.
    let o = Command::new(env!("CARGO_BIN_EXE_xcomm"))
        .args(["skill", "install"])
        .env("HOME", &home)
        .output()
        .unwrap();
    assert_eq!(o.status.code(), Some(1));
    assert!(!dotfiles.join("agents/skills/xcomm").exists());
    let o = Command::new(env!("CARGO_BIN_EXE_xcomm"))
        .args(["skill", "install", "--dir"])
        .arg(home.join("elsewhere"))
        .output()
        .unwrap();
    assert_eq!(o.status.code(), Some(0), "{o:?}");
    assert!(home.join("elsewhere/SKILL.md").is_file());
    let _ = std::fs::remove_dir_all(&home);
}

/// The shell block that starts a headless session, as the skill gives it.
fn headless_block() -> String {
    let section = SKILL_MD.split_once("\n## Starting a session\n").unwrap().1;
    let section = section.split("\n## ").next().unwrap();
    let start = section.find("```sh\nlog=$(mktemp)").unwrap() + "```sh\n".len();
    section[start..].split_once("```").unwrap().0.to_string()
}

/// A process killed when this goes out of scope, test failed or not.
struct Killed(Option<i32>);

impl Drop for Killed {
    fn drop(&mut self) {
        if let Some(pid) = self.0.and_then(rustix::process::Pid::from_raw) {
            let _ = rustix::process::kill_process(pid, rustix::process::Signal::TERM);
        }
    }
}

/// What the headless start block printed: the pid it started, its log, and
/// the pid that answered on the port (if one did).
struct Started {
    pid: String,
    log: PathBuf,
    answered: Option<String>,
}

/// Runs the headless start block for `path` in `cwd`, with sockets and
/// locks under `lock`. The session it starts goes in `started`. Fails,
/// rather than hangs, when the block's output stays open.
fn start_headless(
    path: &str,
    cwd: &Path,
    lock: &Path,
    started: &mut Vec<Killed>,
) -> Started {
    let bin = Path::new(env!("CARGO_BIN_EXE_xcomm"));
    let mut child = Command::new("/bin/sh")
        .args(["-c", &headless_block().replace("<dev>", path)])
        .current_dir(cwd)
        .env(
            "PATH",
            format!(
                "{}:{}",
                bin.parent().unwrap().display(),
                std::env::var("PATH").unwrap_or_default()
            ),
        )
        .env("XDG_RUNTIME_DIR", lock)
        .env("XCOMM_LOCK_DIR", lock)
        .env_remove("HERDR_SOCKET_PATH")
        .env_remove("HERDR_PANE_ID")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdout = child.stdout.take().unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut out = String::new();
        let _ = std::io::Read::read_to_string(&mut stdout, &mut out);
        let _ = tx.send(out);
    });
    let out = rx.recv_timeout(Duration::from_secs(15));
    let _ = child.kill();
    let _ = child.wait();
    let out = out.expect("the block's output stayed open: the session holds it");
    let mut lines = out.lines();
    let first = lines.next().unwrap_or_default();
    let (pid, log) = first
        .strip_prefix("pid ")
        .and_then(|r| r.split_once(" log "))
        .unwrap_or_else(|| panic!("{out:?}"));
    started.push(Killed(pid.parse().ok()));
    Started {
        pid: pid.to_string(),
        log: PathBuf::from(log),
        answered: lines.next().map(str::to_string),
    }
}

#[test]
fn the_headless_start_block_tells_its_session_from_anothers() {
    if Command::new("jq").arg("--version").output().is_err() {
        eprintln!("skipped: jq is not on PATH");
        return;
    }
    let lock = lock_dir("skill-headless");
    let mut sessions = Vec::new();
    let dev = support::pty();
    let path = dev.slave_path.to_str().unwrap().to_string();
    let cwd = scratch("headless");
    // Its own session: the pid it started answers.
    let mine = start_headless(&path, &cwd, &lock, &mut sessions);
    assert_eq!(mine.answered.as_deref(), Some(mine.pid.as_str()));
    // The log is not left in the directory the agent is in.
    assert!(std::fs::read_dir(&cwd).unwrap().next().is_none());
    // The port is held already: the start fails (its log says why), and
    // the pid that answers is the other session's.
    let other = start_headless(&path, &cwd, &lock, &mut sessions);
    assert_ne!(other.answered.as_deref(), Some(other.pid.as_str()));
    assert_eq!(other.answered.as_deref(), Some(mine.pid.as_str()));
    let log = std::fs::read_to_string(&other.log).unwrap();
    assert!(
        log.contains("held") || log.contains("in use") || log.contains("busy"),
        "{log}"
    );
    drop(sessions);
    let _ = std::fs::remove_file(&mine.log);
    let _ = std::fs::remove_file(&other.log);
    drop(dev);
    let _ = std::fs::remove_dir_all(&cwd);
}

/// The skill's examples: each `### ` title under `## Examples` with its
/// first shell block.
fn examples() -> Vec<(String, String)> {
    let section = SKILL_MD.split_once("\n## Examples\n").unwrap().1;
    let section = section.split("\n## ").next().unwrap();
    section
        .split("\n### ")
        .skip(1)
        .map(|part| {
            let (title, rest) = part.split_once('\n').unwrap();
            let block = rest.split_once("```sh\n").unwrap().1;
            let block = block.split_once("```").unwrap().0;
            (title.to_string(), block.to_string())
        })
        .collect()
}

/// A device on `master`: `step` is given what came from the line (or
/// None, every 100 ms) and returns what the device says. Ends when `step`
/// returns None for good or the test drops the guard.
struct Device {
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Device {
    fn start(
        master: &OwnedFd,
        mut step: impl FnMut(Option<&[u8]>) -> Option<Vec<u8>> + Send + 'static,
    ) -> Device {
        let line = master.try_clone().unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let done = stop.clone();
        let thread = std::thread::spawn(move || {
            let mut buf = [0u8; 4096];
            while !done.load(Ordering::Relaxed) {
                let mut fds = [PollFd::new(&line, PollFlags::IN)];
                let tick = rustix::event::Timespec {
                    tv_sec: 0,
                    tv_nsec: 100_000_000,
                };
                let got = if poll(&mut fds, Some(&tick)).unwrap_or(0) > 0 {
                    match rustix::io::read(&line, &mut buf) {
                        Ok(n) if n > 0 => Some(&buf[..n]),
                        _ => return,
                    }
                } else {
                    None
                };
                match step(got) {
                    Some(out) => write_all(line.as_fd(), &out),
                    None => return,
                }
            }
        });
        Device {
            stop,
            thread: Some(thread),
        }
    }
}

impl Drop for Device {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// Replies by what the line has said so far: the first of `script` whose
/// trigger has come since the last reply is answered.
fn replies(
    script: &'static [(&'static str, &'static str)],
) -> impl FnMut(Option<&[u8]>) -> Option<Vec<u8>> {
    let mut heard = Vec::new();
    move |got| {
        heard.extend_from_slice(got.unwrap_or_default());
        let text = String::from_utf8_lossy(&heard).into_owned();
        for (trigger, reply) in script {
            if text.ends_with(trigger) {
                heard.clear();
                return Some(reply.as_bytes().to_vec());
            }
        }
        Some(Vec::new())
    }
}

/// Runs `block` as the skill shows it, on a session on a fake device in
/// `cwd`; `device` plays the device. Gives the block's stdout.
fn run_example(
    block: &str,
    cwd: &Path,
    device: impl FnOnce(&OwnedFd) -> Device,
) -> (String, support::Spawned) {
    run_example_paced(block, cwd, None, device)
}

/// As run_example, with the shell command `between` run between the
/// block's lines: what a user does there when the skill asks.
fn run_example_paced(
    block: &str,
    cwd: &Path,
    between: Option<&str>,
    device: impl FnOnce(&OwnedFd) -> Device,
) -> (String, support::Spawned) {
    let block = match between {
        Some(cmd) => block
            .lines()
            .collect::<Vec<_>>()
            .join(&format!("\n{cmd}\n")),
        None => block.to_string(),
    };
    let dev = support::pty();
    let path = dev.slave_path.to_str().unwrap().to_string();
    let bin = Path::new(env!("CARGO_BIN_EXE_xcomm"));
    let script = format!(
        "cd '{}' && exec '{}' '{path}'",
        cwd.display(),
        bin.display()
    );
    let lock = lock_dir("skill");
    let s = spawn_in_pty("/bin/sh", &["-c", &script], &lock);
    read_until(s.host.master.as_fd(), b"for help", T);
    let _device = device(&dev.master);
    // The session's terminal is read meanwhile: sz writes its progress
    // there, and a macOS pty holds about 1 KiB.
    let stop = Arc::new(AtomicBool::new(false));
    let reader = {
        let master = s.host.master.try_clone().unwrap();
        let fl = rustix::fs::fcntl_getfl(&master).unwrap();
        rustix::fs::fcntl_setfl(&master, fl | rustix::fs::OFlags::NONBLOCK).unwrap();
        let stop = stop.clone();
        std::thread::spawn(move || {
            let mut buf = [0u8; 4096];
            while !stop.load(Ordering::Relaxed) {
                if rustix::io::read(&master, &mut buf).is_err() {
                    std::thread::sleep(Duration::from_millis(5));
                }
            }
        })
    };
    let path_var = format!(
        "{}:{}",
        bin.parent().unwrap().display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let o = Command::new("/bin/sh")
        .args(["-c", &format!("set -e\n{}", block.replace("<dev>", &path))])
        .current_dir(cwd)
        .env("PATH", path_var)
        .env("XDG_RUNTIME_DIR", &lock)
        .env_remove("HERDR_SOCKET_PATH")
        .env_remove("HERDR_PANE_ID")
        .output()
        .unwrap();
    stop.store(true, Ordering::Relaxed);
    reader.join().unwrap();
    let out = String::from_utf8_lossy(&o.stdout).into_owned();
    assert!(
        o.status.success(),
        "{block}\nexit {:?}\nstdout: {out}\nstderr: {}",
        o.status.code(),
        String::from_utf8_lossy(&o.stderr)
    );
    drop(dev);
    (out, s)
}

fn example(title: &str) -> String {
    examples()
        .into_iter()
        .find(|(t, _)| t == title)
        .unwrap_or_else(|| panic!("no example '{title}'"))
        .1
}

#[test]
fn every_example_has_a_test() {
    let titles: Vec<String> = examples().into_iter().map(|(t, _)| t).collect();
    assert_eq!(
        titles,
        [
            "Stop U-Boot autoboot",
            "Log in",
            "Run and parse --json",
            "Capture a boot log",
            "Upload a file with zmodem",
        ]
    );
}

#[test]
fn example_stop_u_boot_autoboot() {
    let cwd = scratch("uboot");
    // The user resets the board between the offset and the wait (the
    // `reset` file), and U-Boot prints its countdown once - here, before
    // the wait has started (`printed`).
    let (reset, printed) = (cwd.join("reset"), cwd.join("printed"));
    let between = format!(
        "touch '{}'; while [ ! -e '{}' ]; do sleep 0.05; done",
        reset.display(),
        printed.display()
    );
    let (out, _s) = run_example_paced(
        &example("Stop U-Boot autoboot"),
        &cwd,
        Some(&between),
        |m| {
            let mut stopped = false;
            let mut counted = false;
            Device::start(m, move |got| {
                if got.is_some_and(|b| b.contains(&b' ')) && !stopped {
                    stopped = true;
                    return Some(b"\r\n=> ".to_vec());
                }
                if !counted && reset.exists() {
                    counted = true;
                    std::thread::sleep(Duration::from_millis(50));
                    return Some(
                        b"U-Boot 2024.01\r\nHit any key to stop autoboot:  3 ".to_vec(),
                    );
                }
                if counted && !printed.exists() {
                    // Written by now: the step's reply goes out before the
                    // next step.
                    std::fs::write(&printed, "").unwrap();
                }
                Some(Vec::new())
            })
        },
    );
    assert!(out.contains("Hit any key"), "{out}");
    let _ = std::fs::remove_dir_all(&cwd);
}

#[test]
fn example_log_in() {
    let cwd = scratch("login");
    let (_, _s) = run_example(&example("Log in"), &cwd, |m| {
        Device::start(
            m,
            replies(&[
                ("root\r", "root\r\nPassword: "),
                ("secret\r", "\r\n# "),
                ("\r", "\r\nbuildroot login: "),
            ]),
        )
    });
    let _ = std::fs::remove_dir_all(&cwd);
}

#[test]
fn example_run_and_parse_json() {
    if Command::new("jq").arg("--version").output().is_err() {
        eprintln!("skipped: jq is not on PATH");
        return;
    }
    let cwd = scratch("json");
    let (out, _s) = run_example(&example("Run and parse --json"), &cwd, |m| {
        Device::start(
            m,
            replies(&[
                (
                    "cat /proc/version\r",
                    "cat /proc/version\r\nLinux version 6.1.0 (test)\r\nroot@board:~# ",
                ),
                (
                    "ip -4 addr show eth0\r",
                    "ip -4 addr show eth0\r\n2: eth0: <UP> mtu 1500\r\n    \
                     inet 192.168.1.5/24 brd 192.168.1.255 scope global eth0\r\nroot@board:~# ",
                ),
            ]),
        )
    });
    // .output ends with the device's line ending, which jq -r prints.
    let lines: Vec<&str> = out
        .lines()
        .map(|l| l.trim_end_matches('\r'))
        .filter(|l| !l.is_empty())
        .collect();
    assert_eq!(
        lines,
        ["Linux version 6.1.0 (test)", "192.168.1.5"],
        "{out:?}"
    );
    let _ = std::fs::remove_dir_all(&cwd);
}

#[test]
fn example_capture_a_boot_log() {
    let cwd = scratch("capture");
    let (_, _s) = run_example(&example("Capture a boot log"), &cwd, |m| {
        let mut ticks = 0;
        Device::start(m, move |got| {
            if got.is_some() {
                return Some(Vec::new());
            }
            ticks += 1;
            Some(match ticks % 3 {
                0 => b"\r\nbuildroot login: ".to_vec(),
                _ => format!("[ {ticks}.000] booting\r\n").into_bytes(),
            })
        })
    });
    let log = std::fs::read_to_string(cwd.join("boot.log")).unwrap();
    assert!(log.contains("login: "), "{log:?}");
    let _ = std::fs::remove_dir_all(&cwd);
}

#[test]
fn example_upload_a_file_with_zmodem() {
    // The board's receiver is started before the transfer, by the line the
    // example types: read boundaries cannot tell that "rz\r" from sz's.
    let block = example("Upload a file with zmodem");
    let typed = block.find("send 'rz'").expect("the example types rz");
    assert!(typed < block.find("xfer send").unwrap(), "{block}");
    let Some(p) = need(&["rz", "sz"]) else { return };
    let (cwd, there) = (scratch("zm-here"), scratch("zm-there"));
    let sent: Vec<u8> = (0..200_000u32).map(|i| (i * 7 % 251) as u8).collect();
    std::fs::write(cwd.join("fw.bin"), &sent).unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    let rz = p[0].clone();
    let dir = there.clone();
    let (_, _s) = run_example(&example("Upload a file with zmodem"), &cwd, move |m| {
        let master = m.try_clone().unwrap();
        let mut heard = Vec::new();
        // The board's shell, until `rz` is typed: then the receiver has
        // the line, as on a board. sz may already be talking by then.
        Device::start(m, move |got| {
            heard.extend_from_slice(got.unwrap_or_default());
            // Only the "rz" the example types, which comes in a read of its
            // own or before sz's first bytes: sz may open with "rz\r**"
            // itself (lrzsz does on Linux), which would start the receiver
            // with no `send 'rz'` at all.
            let read = got.unwrap_or_default();
            let typed = read
                .windows(3)
                .enumerate()
                .any(|(i, w)| w == b"rz\r" && read.get(i + 3) != Some(&b'*'));
            if typed {
                tx.send(Remote::start(&rz, &["-y"], &dir, &master)).unwrap();
                return None;
            }
            Some(Vec::new())
        })
    });
    let remote: Remote = rx.recv_timeout(T).unwrap();
    remote.received(&there.join("fw.bin"), &sent);
    let _ = std::fs::remove_dir_all(&cwd);
    let _ = std::fs::remove_dir_all(&there);
}
