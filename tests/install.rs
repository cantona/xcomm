// SPDX-FileCopyrightText: 2026 Su Kang Yin <paradyse@gmail.com>
// SPDX-License-Identifier: GPL-2.0-only

//! `xcomm uninstall` and `xcomm doctor`, run as a user would, under a
//! temporary HOME with fake `herdr` and `cargo` that log their argv.

mod support;

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const LINKED: &str = r#"{"id":"cli:plugin","result":{"plugins":[{"plugin_id":"cantona.herdr-xcomm","plugin_root":"/src/xcomm","source":{"kind":"local"}}],"type":"plugin_list"}}"#;
const GITHUB: &str = r#"{"id":"cli:plugin","result":{"plugins":[{"plugin_id":"cantona.herdr-xcomm","plugin_root":"/x","source":{"kind":"github","owner":"cantona","repo":"xcomm"}}],"type":"plugin_list"}}"#;
const NONE: &str = r#"{"id":"cli:plugin","result":{"plugins":[],"type":"plugin_list"}}"#;

/// Whether `pid` runs. A zombie does not: where nothing reaps orphans, as in
/// an autopkgtest unshare testbed, a killed process stays one, and kill(0)
/// still finds it.
fn running(pid: i32) -> bool {
    let pid_ = rustix::process::Pid::from_raw(pid).unwrap();
    if rustix::process::test_kill_process(pid_).is_err() {
        return false;
    }
    // The state is the field after the ")" that ends the command name.
    #[cfg(target_os = "linux")]
    if let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat"))
        && let Some((_, rest)) = stat.rsplit_once(')')
        && rest.trim_start().starts_with('Z')
    {
        return false;
    }
    true
}

/// A HOME as an install leaves it, with fakes on PATH.
struct Home {
    root: PathBuf,
}

impl Home {
    fn new(tag: &str) -> Home {
        let root = std::env::temp_dir()
            .join(format!("xcomm-install-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        for d in ["home", "bin", "tools", "run", "locks", "state"] {
            std::fs::create_dir_all(root.join(d)).unwrap();
        }
        // The system's tools the fakes run, and nothing else from /usr/bin:
        // an xcomm installed there (a distribution package) would otherwise
        // be on PATH too, and doctor and uninstall would report it.
        // (macOS has no setsid; the fake that runs it does without.)
        for tool in ["sleep", "setsid", "head", "tr"] {
            if let Some(found) = ["/usr/bin", "/bin"]
                .iter()
                .map(|d| Path::new(d).join(tool))
                .find(|p| p.exists())
            {
                std::os::unix::fs::symlink(found, root.join("tools").join(tool)).unwrap();
            }
        }
        std::fs::set_permissions(
            root.join("run"),
            std::fs::Permissions::from_mode(0o700),
        )
        .unwrap();
        Home { root }
    }

    fn home(&self) -> PathBuf {
        self.root.join("home")
    }

    /// A fake `name` on PATH: logs its argv, answers `plugin list` with
    /// $FAKE_LIST, and exits with $<NAME>_RC after printing $<NAME>_ERR.
    fn fake(&self, name: &str) {
        let up = name.to_uppercase();
        let path = self.root.join("bin").join(name);
        std::fs::write(
            &path,
            format!(
                "#!/bin/sh\nprintf '%s|' \"$@\" >> '{log}'\necho >> '{log}'\n\
                 case \"$*\" in \"plugin list\"*) printf '%s\\n' \"$FAKE_LIST\"; exit 0 ;; esac\n\
                 [ -n \"${{{up}_ERR:-}}\" ] && echo \"${{{up}_ERR}}\" >&2\nexit ${{{up}_RC:-0}}\n",
                log = self.root.join(format!("{name}.log")).display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    fn log(&self, name: &str) -> String {
        std::fs::read_to_string(self.root.join(format!("{name}.log"))).unwrap_or_default()
    }

    fn skill(&self) -> PathBuf {
        self.home().join(".agents/skills/xcomm/SKILL.md")
    }

    fn install_skill(&self) {
        xcomm::skill::install(self.skill().parent().unwrap()).unwrap();
    }

    /// What `cargo install --root ~/.local` records.
    fn cargo_record(&self) {
        let root = self.home().join(".local");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(
            root.join(".crates.toml"),
            "[v1]\n\"xcomm 0.1.0 (path+file:///src/xcomm)\" = [\"xcomm\", \"xcomm-wait\"]\n",
        )
        .unwrap();
    }

    fn run(&self, cmd: &str, env: &[(&str, &str)]) -> Output {
        let path = format!(
            "{}:{}",
            self.root.join("bin").display(),
            self.root.join("tools").display()
        );
        Command::new(env!("CARGO_BIN_EXE_xcomm"))
            .arg(cmd)
            .env_clear()
            .env("HOME", self.home())
            .env("PATH", path)
            .env("XDG_RUNTIME_DIR", self.root.join("run"))
            .env("XCOMM_LOCK_DIR", self.root.join("locks"))
            .env("XDG_STATE_HOME", self.root.join("state"))
            .envs(env.iter().copied())
            .output()
            .unwrap()
    }
}

impl Drop for Home {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn stdout(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

#[test]
fn uninstall_takes_everything_away_in_order() {
    let h = Home::new("order");
    h.fake("herdr");
    h.fake("cargo");
    h.install_skill();
    h.cargo_record();
    let o = h.run("uninstall", &[("FAKE_LIST", LINKED)]);
    assert_eq!(o.status.code(), Some(0), "{o:?}");
    let skill = h.skill();
    assert_eq!(
        stdout(&o),
        format!(
            "hotplug watcher: skip (none running)\n\
             skill {}: ok\n\
             herdr plugin: ok\n\
             binaries: ok\n",
            skill.display(),
        )
    );
    assert!(!skill.exists());
    assert!(!skill.parent().unwrap().exists());
    assert_eq!(
        h.log("herdr"),
        "plugin|list|--json|--plugin|cantona.herdr-xcomm|\nplugin|unlink|cantona.herdr-xcomm|\n"
    );
    assert_eq!(
        h.log("cargo"),
        format!(
            "uninstall|--root|{}|xcomm|\n",
            h.home().join(".local").display()
        )
    );
    // One from GitHub is uninstalled, not unlinked.
    let h = Home::new("github");
    h.fake("herdr");
    let o = h.run("uninstall", &[("FAKE_LIST", GITHUB)]);
    assert_eq!(o.status.code(), Some(0), "{o:?}");
    assert!(
        h.log("herdr")
            .contains("plugin|uninstall|cantona.herdr-xcomm|")
    );
}

#[test]
fn a_skill_xcomm_did_not_write_is_left_and_the_rest_goes() {
    let h = Home::new("mine");
    h.fake("herdr");
    h.fake("cargo");
    h.cargo_record();
    std::fs::create_dir_all(h.skill().parent().unwrap()).unwrap();
    std::fs::write(h.skill(), "---\nname: xcomm\n---\nmine\n").unwrap();
    let o = h.run("uninstall", &[("FAKE_LIST", LINKED)]);
    assert_eq!(o.status.code(), Some(0), "{o:?}");
    let out = stdout(&o);
    assert!(
        out.contains(&format!(
            "skill {}: skip (not written by xcomm (no generated-by marker): left as it is)\n",
            h.skill().display()
        )),
        "{out}"
    );
    assert_eq!(
        std::fs::read_to_string(h.skill()).unwrap(),
        "---\nname: xcomm\n---\nmine\n"
    );
    assert!(h.log("herdr").contains("plugin|unlink|"));
    assert!(h.log("cargo").starts_with("uninstall|"));
    // doctor says the same about it, and has nothing against it.
    let o = h.run("doctor", &[("FAKE_LIST", NONE)]);
    assert!(
        stdout(&o).contains("(not written by xcomm"),
        "{}",
        stdout(&o)
    );
    assert_eq!(o.status.code(), Some(0), "{}", stdout(&o));
}

#[test]
fn a_skill_in_a_dotfiles_repository_or_a_link_is_left() {
    // The skill xcomm wrote, with --dir, into a dotfiles checkout.
    let h = Home::new("dotfiles");
    let dotfiles = h.home().join("dotfiles");
    std::fs::create_dir_all(dotfiles.join(".git")).unwrap();
    std::fs::create_dir_all(dotfiles.join("agents")).unwrap();
    std::os::unix::fs::symlink(dotfiles.join("agents"), h.home().join(".agents"))
        .unwrap();
    h.install_skill();
    let o = h.run("uninstall", &[]);
    assert_eq!(o.status.code(), Some(0), "{o:?}");
    let out = stdout(&o);
    assert!(
        out.contains("git work tree") && out.contains("left alone; remove it yourself"),
        "{out}"
    );
    assert!(dotfiles.join("agents/skills/xcomm/SKILL.md").is_file());
    // Kept, but xcomm wrote it and no xcomm is left: doctor says so.
    let o = h.run("doctor", &[]);
    assert_eq!(o.status.code(), Some(1), "{}", stdout(&o));
    assert!(
        stdout(&o).contains("problem: the skill at ")
            && stdout(&o).contains("no xcomm is on PATH"),
        "{}",
        stdout(&o)
    );
    // A link (stow and the like) is left to what made it.
    let h = Home::new("link");
    let real = h.root.join("real.md");
    std::fs::write(&real, xcomm::skill::render_for_install()).unwrap();
    std::fs::create_dir_all(h.skill().parent().unwrap()).unwrap();
    std::os::unix::fs::symlink(&real, h.skill()).unwrap();
    let o = h.run("uninstall", &[]);
    assert_eq!(o.status.code(), Some(0), "{o:?}");
    assert!(stdout(&o).contains("not a plain file"), "{}", stdout(&o));
    assert!(h.skill().symlink_metadata().is_ok() && real.is_file());
}

#[test]
fn binaries_cargo_did_not_install_here_are_named() {
    // `cargo install --git`: in ~/.cargo, not ~/.local.
    let h = Home::new("cargohome");
    h.fake("cargo");
    let root = h.home().join(".cargo");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(
        root.join(".crates.toml"),
        "[v1]\n\"xcomm 0.1.0 (git+https://x/xcomm#abc)\" = [\"xcomm\", \"xcomm-wait\"]\n",
    )
    .unwrap();
    let o = h.run("uninstall", &[]);
    assert_eq!(o.status.code(), Some(0), "{o:?}");
    assert_eq!(
        h.log("cargo"),
        format!("uninstall|--root|{}|xcomm|\n", root.display())
    );
    // A copy on PATH that no cargo record names: uninstall says so.
    let h = Home::new("copied");
    let bin = h.root.join("bin");
    std::fs::copy(env!("CARGO_BIN_EXE_xcomm"), bin.join("xcomm")).unwrap();
    let o = h.run("uninstall", &[]);
    assert_eq!(o.status.code(), Some(1));
    let out = stdout(&o);
    assert!(
        out.contains("binaries: FAILED: still on PATH, not installed by cargo under ")
            && out.contains(&bin.join("xcomm").display().to_string()),
        "{out}"
    );
    assert!(out.ends_with("not done: binaries\n"), "{out}");
    // Said once, not again on stderr.
    assert_eq!(String::from_utf8_lossy(&o.stderr), "");
    // Another program that happens to be called xcomm is not this one.
    let h = Home::new("other");
    let other = h.root.join("bin/xcomm");
    std::fs::write(
        &other,
        "#!/bin/sh\necho 'xcomm-unrelated serial tool 3.2'\n",
    )
    .unwrap();
    std::fs::set_permissions(&other, std::fs::Permissions::from_mode(0o755)).unwrap();
    let o = h.run("uninstall", &[]);
    assert_eq!(o.status.code(), Some(0), "{o:?}");
    let o = h.run("doctor", &[]);
    assert!(
        stdout(&o).contains("binary: no xcomm on PATH"),
        "{}",
        stdout(&o)
    );
    assert!(other.is_file());
    // One root by two names is uninstalled once.
    let h = Home::new("alias");
    h.fake("cargo");
    h.cargo_record();
    let alias = h.root.join("alias");
    std::os::unix::fs::symlink(h.home().join(".local"), &alias).unwrap();
    let o = h.run(
        "uninstall",
        &[("CARGO_INSTALL_ROOT", alias.to_str().unwrap())],
    );
    assert_eq!(o.status.code(), Some(0), "{o:?}");
    assert_eq!(h.log("cargo").lines().count(), 1, "{}", h.log("cargo"));
}

#[test]
fn without_herdr_the_plugin_step_is_skipped() {
    let h = Home::new("noherdr");
    h.fake("cargo");
    h.cargo_record();
    let o = h.run("uninstall", &[]);
    assert_eq!(o.status.code(), Some(0), "{o:?}");
    let out = stdout(&o);
    assert!(
        out.contains("herdr plugin: skip (herdr is not on PATH)\n"),
        "{out}"
    );
    assert!(
        out.contains("skill ") && out.contains(": skip (no skill at "),
        "{out}"
    );
    assert!(h.log("cargo").starts_with("uninstall|"));
    // Nothing installed at all: every step is skipped, and that is fine.
    let h = Home::new("nothing");
    h.fake("herdr");
    let o = h.run("uninstall", &[("FAKE_LIST", NONE)]);
    assert_eq!(o.status.code(), Some(0), "{o:?}");
    assert!(stdout(&o).contains("herdr plugin: skip (not registered with herdr)"));
    assert!(stdout(&o).contains("binaries: skip (no xcomm installed)"));
}

#[test]
fn a_failing_cargo_is_reported_with_what_is_left() {
    let h = Home::new("cargofail");
    h.fake("herdr");
    h.fake("cargo");
    h.install_skill();
    h.cargo_record();
    let o = h.run(
        "uninstall",
        &[
            ("FAKE_LIST", LINKED),
            ("CARGO_RC", "101"),
            ("CARGO_ERR", "error: permission denied"),
        ],
    );
    assert_eq!(o.status.code(), Some(1));
    let out = stdout(&o);
    let root = h.home().join(".local");
    assert!(
        out.contains("binaries: FAILED: ")
            && out.contains("error: permission denied")
            // /src/xcomm, where the record says it came from, is not here.
            && out.contains("/src/xcomm is gone; reinstall from a copy of xcomm's source"),
        "{out}"
    );
    assert!(out.ends_with("not done: binaries\n"), "{out}");
    // The remedy is a command cargo takes, from what its record says.
    let src = h.root.join("my src");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(src.join("Cargo.toml"), "").unwrap();
    for (record, want) in [
        (
            format!(
                "path+file://{}",
                src.display().to_string().replace(' ', "%20")
            ),
            format!("cargo install --path '{}' --root ", src.display()),
        ),
        (
            "git+file:///repo/xcomm?branch=dev#0123abc".to_string(),
            "cargo install --git file:///repo/xcomm --branch dev --root ".to_string(),
        ),
        (
            "git+https://example.com/xcomm?rev=0123abc#0123abc".to_string(),
            "cargo install --git https://example.com/xcomm --rev 0123abc --root "
                .to_string(),
        ),
    ] {
        std::fs::write(
            root.join(".crates.toml"),
            format!("[v1]\n\"xcomm 0.1.0 ({record})\" = [\"xcomm\", \"xcomm-wait\"]\n"),
        )
        .unwrap();
        let o = h.run(
            "uninstall",
            &[
                ("FAKE_LIST", NONE),
                ("CARGO_RC", "101"),
                ("CARGO_ERR", "corrupt metadata"),
            ],
        );
        assert!(stdout(&o).contains(&want), "{want} in {}", stdout(&o));
    }
    // A failing herdr stops it before the binaries.
    let h = Home::new("herdrfail");
    h.fake("herdr");
    h.fake("cargo");
    h.cargo_record();
    let o = h.run(
        "uninstall",
        &[
            ("FAKE_LIST", LINKED),
            ("HERDR_RC", "1"),
            ("HERDR_ERR", "no server"),
        ],
    );
    assert_eq!(o.status.code(), Some(1));
    assert!(
        stdout(&o).contains("herdr plugin: FAILED: "),
        "{}",
        stdout(&o)
    );
    assert_eq!(h.log("cargo"), "");
}

#[test]
fn doctor_flags_a_stale_socket_and_a_dangling_skill() {
    let h = Home::new("doctor");
    h.fake("herdr");
    // A session that went away without removing its socket.
    let sock_dir = h.root.join("run/xcomm");
    std::fs::create_dir_all(&sock_dir).unwrap();
    std::fs::set_permissions(&sock_dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    let sock = sock_dir.join("ttyUSB9.sock");
    drop(std::os::unix::net::UnixListener::bind(&sock).unwrap());
    // A lock whose process is gone.
    std::fs::write(h.root.join("locks/LCK..ttyUSB9"), "  999999999\n").unwrap();
    // The skill an install left, with no binary any more.
    h.install_skill();
    let o = h.run("doctor", &[("FAKE_LIST", NONE)]);
    assert_eq!(o.status.code(), Some(1), "{o:?}");
    let out = stdout(&o);
    for want in [
        "binary: no xcomm on PATH\n",
        &format!(
            "skill: {} (generated by xcomm {})\n",
            h.skill().display(),
            env!("CARGO_PKG_VERSION")
        ),
        "herdr plugin: not registered\n",
        &format!("problem: stale socket {}", sock.display()),
        "problem: stale lock ",
        "problem: the skill at ",
        "3 problems\n",
    ] {
        assert!(out.contains(want), "{want:?} in {out}");
    }
    // A skill that cannot be read is not "none".
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(h.skill(), std::fs::Permissions::from_mode(0o000))
            .unwrap();
        let o = h.run("doctor", &[("FAKE_LIST", NONE)]);
        assert!(stdout(&o).contains("problem: skill: "), "{}", stdout(&o));
        std::fs::set_permissions(h.skill(), std::fs::Permissions::from_mode(0o644))
            .unwrap();
    }
    // A live socket is not stale.
    std::fs::remove_file(&sock).unwrap();
    let _live = std::os::unix::net::UnixListener::bind(&sock).unwrap();
    std::fs::remove_file(h.root.join("locks/LCK..ttyUSB9")).unwrap();
    std::fs::remove_file(h.skill()).unwrap();
    let o = h.run("doctor", &[("FAKE_LIST", NONE)]);
    assert_eq!(o.status.code(), Some(0), "{}", stdout(&o));
    assert!(stdout(&o).ends_with("clean\n"));
}

#[test]
fn doctor_leaves_out_what_an_older_watcher_left() {
    let h = Home::new("doctor-old-watch");
    h.fake("herdr");
    // The directory xcomm up to v0.1.2 used, its watcher long gone; and one
    // of today's, also stopped.
    let state = h.root.join("state/herdr/plugins/cantona.herdr-xcomm");
    for dir in ["herdr.sock", "herdr.sock-0123456789abcdef"] {
        std::fs::create_dir_all(state.join(dir)).unwrap();
        std::fs::write(state.join(dir).join("watch.pid"), "").unwrap();
    }
    let out = stdout(&h.run("doctor", &[("FAKE_LIST", NONE)]));
    assert!(
        out.contains("herdr.sock-0123456789abcdef: not running"),
        "{out}"
    );
    assert!(!out.contains("herdr.sock: "), "{out}");
}

#[test]
fn doctor_reports_an_install_and_a_broken_link() {
    let h = Home::new("installed");
    h.fake("herdr");
    // The binaries as build.sh installs them: a copy of this build.
    let bin = h.home().join(".local/bin");
    std::fs::create_dir_all(&bin).unwrap();
    let built = Path::new(env!("CARGO_BIN_EXE_xcomm"));
    for name in ["xcomm", "xcomm-wait"] {
        std::fs::copy(built.with_file_name(name), bin.join(name)).unwrap();
    }
    h.install_skill();
    let path = format!(
        "{}:{}:{}",
        h.root.join("bin").display(),
        bin.display(),
        h.root.join("tools").display()
    );
    let o = h.run("doctor", &[("FAKE_LIST", LINKED), ("PATH", &path)]);
    let out = stdout(&o);
    assert!(
        out.contains(&format!(
            "binary: {} (xcomm {})\n",
            bin.join("xcomm").display(),
            env!("CARGO_PKG_VERSION")
        )),
        "{out}"
    );
    assert!(
        out.contains("herdr plugin: linked to /src/xcomm\n"),
        "{out}"
    );
    // /src/xcomm has no manifest: that is the one problem.
    assert!(
        out.contains("problem: the plugin is linked to /src/xcomm"),
        "{out}"
    );
    assert!(out.ends_with("1 problem\n"), "{out}");
    assert_eq!(o.status.code(), Some(1));
    // A link on PATH to the binaries elsewhere: xcomm-wait is looked for
    // beside the binary, where ctl wait and run look, not beside the link.
    let linked = h.home().join("linkbin");
    std::fs::create_dir_all(&linked).unwrap();
    std::os::unix::fs::symlink(bin.join("xcomm"), linked.join("xcomm")).unwrap();
    let path2 = format!(
        "{}:{}:{}",
        h.root.join("bin").display(),
        linked.display(),
        h.root.join("tools").display()
    );
    let o = h.run("doctor", &[("FAKE_LIST", NONE), ("PATH", &path2)]);
    assert!(
        !stdout(&o).contains("xcomm-wait is missing"),
        "{}",
        stdout(&o)
    );
    // Without xcomm-wait beside it, ctl wait and run cannot work.
    std::fs::remove_file(bin.join("xcomm-wait")).unwrap();
    let o = h.run("doctor", &[("FAKE_LIST", NONE), ("PATH", &path)]);
    assert!(
        stdout(&o).contains("xcomm-wait is missing"),
        "{}",
        stdout(&o)
    );
}

#[test]
fn doctor_reports_an_xcomm_that_does_not_run_and_does_not_wait_for_it() {
    let h = Home::new("broken");
    h.fake("herdr");
    let bin = h.root.join("bin");
    // First on PATH, and broken: the shell would run this one.
    std::fs::write(bin.join("xcomm"), "#!/bin/sh\nkill -SEGV $$\n").unwrap();
    std::fs::set_permissions(bin.join("xcomm"), std::fs::Permissions::from_mode(0o755))
        .unwrap();
    let o = h.run("doctor", &[("FAKE_LIST", NONE)]);
    assert_eq!(o.status.code(), Some(1), "{}", stdout(&o));
    assert!(
        stdout(&o).contains(&format!(
            "problem: {} does not run",
            bin.join("xcomm").display()
        )),
        "{}",
        stdout(&o)
    );
    // One that never answers is given up on, and what it started goes
    // with it.
    let pid_file = h.root.join("child.pid");
    std::fs::write(
        bin.join("xcomm"),
        format!(
            "#!/bin/sh\nsleep 47 &\necho $! > '{}'\nwait\n",
            pid_file.display()
        ),
    )
    .unwrap();
    let started = std::time::Instant::now();
    let o = h.run("doctor", &[("FAKE_LIST", NONE)]);
    assert!(started.elapsed() < std::time::Duration::from_secs(10));
    assert!(
        stdout(&o).contains("no answer to --version"),
        "{}",
        stdout(&o)
    );
    let pid: i32 = std::fs::read_to_string(&pid_file)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    let until = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while running(pid) {
        assert!(
            std::time::Instant::now() < until,
            "its child {pid} still runs"
        );
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    // Another program: one that leaves a child holding its output, one that
    // answers at length, one that refuses the option. None is a problem,
    // and none holds doctor up.
    // One whose loader fails (a missing library): 127, not "another program".
    std::fs::write(
        bin.join("xcomm"),
        "#!/bin/sh\necho 'xcomm: error while loading shared libraries: libgone.so' >&2\nexit 127\n",
    )
    .unwrap();
    let o = h.run("doctor", &[("FAKE_LIST", NONE)]);
    assert!(stdout(&o).contains("cannot be run"), "{}", stdout(&o));
    // One that answers, but leaves a child outside its group holding the
    // output: the answer still counts, and doctor does not wait for it.
    std::fs::write(
        bin.join("xcomm"),
        "#!/bin/sh\nsetsid sleep 9 >&1 &\necho 'xcomm 0.0.1'\n",
    )
    .unwrap();
    let started = std::time::Instant::now();
    let o = h.run("doctor", &[("FAKE_LIST", NONE)]);
    assert!(started.elapsed() < std::time::Duration::from_secs(5));
    assert!(stdout(&o).contains("(xcomm 0.0.1)"), "{}", stdout(&o));
    for script in [
        "#!/bin/sh\n(sleep 20) &\necho 'other tool 1.0'\n",
        "#!/bin/sh\nhead -c 200000 /dev/zero | tr '\\0' x\n",
        "#!/bin/sh\necho 'usage: xcomm [-q] host' >&2\nexit 1\n",
    ] {
        std::fs::write(bin.join("xcomm"), script).unwrap();
        let started = std::time::Instant::now();
        let o = h.run("doctor", &[("FAKE_LIST", NONE)]);
        assert!(
            started.elapsed() < std::time::Duration::from_secs(5),
            "{script}"
        );
        assert_eq!(o.status.code(), Some(0), "{script}: {}", stdout(&o));
    }
}

#[test]
fn uninstall_and_doctor_take_no_arguments() {
    for cmd in [&["uninstall", "x"][..], &["doctor", "--json"]] {
        let o = Command::new(env!("CARGO_BIN_EXE_xcomm"))
            .args(cmd)
            .output()
            .unwrap();
        assert_eq!(o.status.code(), Some(2), "{cmd:?}");
    }
}

#[test]
fn uninstall_stops_a_running_hotplug_watcher() {
    let h = Home::new("watcher");
    h.fake("herdr");
    // A herdr server that answers every request: the watcher runs as long
    // as its socket is there.
    let sock = h.root.join("herdr.sock");
    let listener = std::os::unix::net::UnixListener::bind(&sock).unwrap();
    listener.set_nonblocking(true).unwrap();
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let server = {
        let stop = stop.clone();
        std::thread::spawn(move || {
            use std::io::{BufRead, Write};
            while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                let Ok((stream, _)) = listener.accept() else {
                    std::thread::sleep(std::time::Duration::from_millis(5));
                    continue;
                };
                stream.set_nonblocking(false).unwrap();
                let mut line = String::new();
                if std::io::BufReader::new(&stream)
                    .read_line(&mut line)
                    .is_ok()
                {
                    let req: serde_json::Value =
                        serde_json::from_str(&line).unwrap_or_default();
                    let reply = serde_json::json!({ "id": req["id"], "result": { "type": "ok" } });
                    let _ = (&stream).write_all(format!("{reply}\n").as_bytes());
                }
            }
        })
    };
    // Started as the plugin starts it: its state where herdr keeps it.
    let state = h.root.join("state/herdr/plugins/cantona.herdr-xcomm");
    std::fs::create_dir_all(&state).unwrap();
    std::fs::create_dir_all(h.root.join("watchroot/dev")).unwrap();
    let o = Command::new(env!("CARGO_BIN_EXE_xcomm"))
        .args(["watch", "start"])
        .env_remove("HERDR_PANE_ID")
        .env("HERDR_PLUGIN_STATE_DIR", &state)
        .env("HERDR_SOCKET_PATH", &sock)
        .env("XCOMM_WATCH_ROOT", h.root.join("watchroot"))
        .env("XCOMM_LOCK_DIR", h.root.join("locks"))
        .env("XDG_RUNTIME_DIR", h.root.join("run"))
        .output()
        .unwrap();
    assert!(o.status.success(), "{o:?}");
    let get = |k: &str| match k {
        "HERDR_PLUGIN_STATE_DIR" => Some(state.to_str().unwrap().to_string()),
        "HERDR_SOCKET_PATH" => Some(sock.to_str().unwrap().to_string()),
        _ => None,
    };
    let pid_file = xcomm::watch::state_dir(&get).unwrap().join("watch.pid");
    let until = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let pid: i32 = loop {
        if let Some(pid) = std::fs::read_to_string(&pid_file)
            .ok()
            .and_then(|p| p.trim().parse().ok())
        {
            break pid;
        }
        assert!(std::time::Instant::now() < until, "no watch.pid");
        std::thread::sleep(std::time::Duration::from_millis(20));
    };
    let alive = || running(pid);
    assert!(alive());
    let o = h.run("doctor", &[("FAKE_LIST", NONE)]);
    let out = stdout(&o);
    let dir = pid_file.parent().unwrap().display().to_string();
    assert!(out.contains(&format!("{dir}: running")), "{out}");
    // No binary on PATH and a watcher still running: a leftover.
    assert!(
        out.contains("problem: a hotplug watcher still runs"),
        "{out}"
    );
    let o = h.run("uninstall", &[("FAKE_LIST", NONE)]);
    assert!(o.status.success(), "{o:?}");
    assert!(
        stdout(&o).starts_with("hotplug watcher: ok\n"),
        "{}",
        stdout(&o)
    );
    // What it left is gone too.
    assert!(!pid_file.parent().unwrap().exists());
    let until = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while alive() {
        assert!(std::time::Instant::now() < until, "the watcher still runs");
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    server.join().unwrap();
}
