// SPDX-FileCopyrightText: 2026 Su Kang Yin <paradyse@gmail.com>
// SPDX-License-Identifier: GPL-2.0-only

//! `xcomm uninstall` and `xcomm doctor`: what an install leaves, taken
//! away in order, or reported.

use std::fmt;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::pick::PLUGIN_ID;

#[derive(Debug)]
pub struct InstallError(pub String);

impl fmt::Display for InstallError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for InstallError {}

/// Where an install puts things, and the programs that take them away.
pub struct Env {
    pub home: PathBuf,
    /// `herdr` on PATH.
    pub herdr: Option<PathBuf>,
    /// `cargo` on PATH.
    pub cargo: Option<PathBuf>,
    /// PATH, to find the xcomm binaries.
    pub path: Vec<PathBuf>,
    /// Where herdr keeps the plugin's state: a directory per herdr server,
    /// each with its watcher's pid file.
    pub watch_state: PathBuf,
    pub sock_dirs: Vec<PathBuf>,
    pub lock_dirs: Vec<PathBuf>,
    /// Where cargo may have installed xcomm: ~/.local (build.sh), then
    /// CARGO_INSTALL_ROOT and CARGO_HOME (`cargo install --git`).
    pub cargo_roots: Vec<PathBuf>,
    /// Why the skill's directory may belong to a dotfiles repository.
    pub dotfiles: Option<String>,
}

impl Env {
    pub fn from_env(env: &dyn Fn(&str) -> Option<String>) -> Result<Env, InstallError> {
        let home = env("HOME")
            .filter(|h| h.starts_with('/'))
            .map(PathBuf::from)
            .ok_or_else(|| InstallError("HOME is not set to an absolute path".into()))?;
        let path: Vec<PathBuf> = env("PATH")
            .map(|p| std::env::split_paths(&p).collect())
            .unwrap_or_default();
        let state = env("XDG_STATE_HOME")
            .filter(|s| s.starts_with('/'))
            .map_or_else(|| home.join(".local/state"), PathBuf::from);
        let absolute =
            |var: &str| env(var).filter(|v| v.starts_with('/')).map(PathBuf::from);
        let mut cargo_roots = vec![home.join(".local")];
        cargo_roots.extend(absolute("CARGO_INSTALL_ROOT"));
        cargo_roots.push(absolute("CARGO_HOME").unwrap_or_else(|| home.join(".cargo")));
        let mut lock_dirs: Vec<PathBuf> = Vec::new();
        // /var/lock is often a link to /run/lock: each lock once.
        for d in crate::lock::lock_dirs_to_read(env) {
            let real = std::fs::canonicalize(&d).unwrap_or_else(|_| d.clone());
            if !lock_dirs
                .iter()
                .any(|l| std::fs::canonicalize(l).unwrap_or_else(|_| l.clone()) == real)
            {
                lock_dirs.push(d);
            }
        }
        let skill_dir = home.join(".agents/skills/xcomm");
        Ok(Env {
            herdr: which(&path, "herdr"),
            cargo: which(&path, "cargo"),
            watch_state: state.join("herdr/plugins").join(PLUGIN_ID),
            sock_dirs: crate::ctl::path::socket_dirs(env),
            lock_dirs,
            cargo_roots,
            dotfiles: crate::skill::dotfiles_owner(&skill_dir, &home, env),
            home,
            path,
        })
    }

    /// Where `xcomm skill install` puts the skill by default.
    pub fn skill(&self) -> PathBuf {
        self.home.join(".agents/skills/xcomm/SKILL.md")
    }
}

/// The skill at the default place, as uninstall and doctor see it.
enum SkillState {
    Absent,
    /// A plain file xcomm wrote, there for uninstall to remove: the
    /// version that wrote it.
    Ours(String),
    /// Something uninstall leaves alone, why, and the version that wrote
    /// it if xcomm did.
    NotOurs(String, Option<String>),
}

fn skill_state(env: &Env) -> Result<SkillState, InstallError> {
    let path = env.skill();
    let err = |e: std::io::Error| InstallError(format!("{}: {e}", path.display()));
    let meta = match std::fs::symlink_metadata(&path) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(SkillState::Absent);
        }
        Err(e) => return Err(err(e)),
    };
    if !meta.file_type().is_file() {
        return Ok(SkillState::NotOurs(
            "not a plain file (a link?): left to what made it".into(),
            None,
        ));
    }
    let text = std::fs::read(&path).map_err(err)?;
    let Some(version) = crate::skill::marker_version(&String::from_utf8_lossy(&text))
    else {
        return Ok(SkillState::NotOurs(
            "not written by xcomm (no generated-by marker): left as it is".into(),
            None,
        ));
    };
    if let Some(why) = &env.dotfiles {
        return Ok(SkillState::NotOurs(
            format!("{why}: left alone; remove it yourself if it should go"),
            Some(version),
        ));
    }
    Ok(SkillState::Ours(version))
}

/// The first `name` on `path` that is an executable file.
fn which(path: &[PathBuf], name: &str) -> Option<PathBuf> {
    path.iter().map(|d| d.join(name)).find(|p| executable(p))
}

fn executable(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

#[derive(Debug, PartialEq, Eq)]
pub enum PluginState {
    NotRegistered,
    Github,
    Linked(PathBuf),
}

/// Runs `program args`; its stdout, or why it failed.
fn run(program: &Path, args: &[&str]) -> Result<String, InstallError> {
    let o = Command::new(program)
        .args(args)
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|e| InstallError(format!("{}: {e}", program.display())))?;
    if o.status.success() {
        return Ok(String::from_utf8_lossy(&o.stdout).into_owned());
    }
    let mut why = String::from_utf8_lossy(&o.stderr).trim().to_string();
    if why.is_empty() {
        why = String::from_utf8_lossy(&o.stdout).trim().to_string();
    }
    Err(InstallError(format!(
        "{} {} ({}): {why}",
        program.display(),
        args.join(" "),
        o.status
    )))
}

/// How herdr has the plugin: `herdr plugin list --json`.
pub fn plugin_state(env: &Env) -> Result<PluginState, InstallError> {
    let Some(herdr) = &env.herdr else {
        return Ok(PluginState::NotRegistered);
    };
    let out = run(herdr, &["plugin", "list", "--json", "--plugin", PLUGIN_ID])?;
    let v: serde_json::Value = serde_json::from_str(&out)
        .map_err(|e| InstallError(format!("herdr plugin list: {e}: {out}")))?;
    let plugins = v["result"]["plugins"]
        .as_array()
        .ok_or_else(|| InstallError(format!("herdr plugin list: no plugins in {out}")))?;
    let Some(p) = plugins.iter().find(|p| p["plugin_id"] == PLUGIN_ID) else {
        return Ok(PluginState::NotRegistered);
    };
    match p["source"]["kind"].as_str() {
        Some("github") => Ok(PluginState::Github),
        Some("local") => Ok(PluginState::Linked(PathBuf::from(
            p["plugin_root"].as_str().unwrap_or_default(),
        ))),
        other => Err(InstallError(format!(
            "herdr plugin list: {PLUGIN_ID} has a source of kind {other:?}"
        ))),
    }
}

/// A step's outcome, as uninstall prints it.
enum Step {
    Done,
    Skipped(String),
}

/// The watchers herdr started, one per server: their state directories.
fn watch_dirs(env: &Env) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = std::fs::read_dir(&env.watch_state)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect();
    dirs.sort();
    dirs
}

/// `xcomm watch <action>` for the watcher kept in `dir`.
fn watch_act(
    dir: &Path,
    action: crate::watch::WatchAction,
) -> Result<String, crate::watch::WatchError> {
    crate::watch::act_in(dir, action, &|_| None)
}

fn stop_watchers(env: &Env) -> Result<Step, InstallError> {
    let mut did = false;
    for dir in watch_dirs(env) {
        if dir.join("watch.pid").exists() {
            let out = watch_act(&dir, crate::watch::WatchAction::Stop)
                .map_err(|e| InstallError(format!("{}: {e}", dir.display())))?;
            did |= out != "not running";
        }
        // What it left: its pid file and logs, then the directory.
        for name in ["watch.pid", "watch.log", "watch.log.1"] {
            did |= std::fs::remove_file(dir.join(name)).is_ok();
        }
        let _ = std::fs::remove_dir(&dir);
    }
    let _ = std::fs::remove_dir(&env.watch_state);
    Ok(if did {
        Step::Done
    } else {
        Step::Skipped("none running".into())
    })
}

fn remove_skill(env: &Env) -> Result<Step, InstallError> {
    let path = env.skill();
    match skill_state(env)? {
        SkillState::Absent => {
            Ok(Step::Skipped(format!("no skill at {}", path.display())))
        }
        SkillState::NotOurs(why, _) => Ok(Step::Skipped(why)),
        SkillState::Ours(_) => {
            std::fs::remove_file(&path)
                .map_err(|e| InstallError(format!("{}: {e}", path.display())))?;
            // Its directory too, if nothing else is in it.
            if let Some(dir) = path.parent() {
                let _ = std::fs::remove_dir(dir);
            }
            Ok(Step::Done)
        }
    }
}

fn remove_plugin(env: &Env) -> Result<Step, InstallError> {
    let Some(herdr) = &env.herdr else {
        return Ok(Step::Skipped("herdr is not on PATH".into()));
    };
    match plugin_state(env)? {
        PluginState::NotRegistered => {
            Ok(Step::Skipped("not registered with herdr".into()))
        }
        PluginState::Github => {
            run(herdr, &["plugin", "uninstall", PLUGIN_ID]).map(|_| Step::Done)
        }
        PluginState::Linked(_) => {
            run(herdr, &["plugin", "unlink", PLUGIN_ID]).map(|_| Step::Done)
        }
    }
}

/// How to put back what cargo installed under `root`, from where its
/// record says xcomm came from.
fn reinstall_command(root: &Path) -> String {
    let record = std::fs::read_to_string(root.join(".crates.toml")).unwrap_or_default();
    let source = record.lines().find_map(|l| {
        let rest = l.strip_prefix("\"xcomm ")?;
        let inner = rest.split_once(" (")?.1.split_once(")\"")?.0;
        Some(inner.to_string())
    });
    let root = quote(&root.display().to_string());
    let generic = format!(
        "reinstall from a copy of xcomm's source: cargo install --path <src> --root {root}"
    );
    let Some(source) = source else {
        return generic;
    };
    if let Some(path) = source.strip_prefix("path+file://") {
        let path = percent_decode(path);
        if !Path::new(&path).join("Cargo.toml").is_file() {
            // A herdr checkout, say, gone with the plugin.
            return format!("{path} is gone; {generic}");
        }
        return format!(
            "reinstall with cargo install --path {} --root {root}",
            quote(&path)
        );
    }
    if let Some(git) = source.strip_prefix("git+") {
        let git = git.split_once('#').map_or(git, |(u, _)| u);
        let (url, query) = git.split_once('?').unwrap_or((git, ""));
        let mut cmd = format!("reinstall with cargo install --git {}", quote(url));
        for kv in query.split('&').filter(|kv| !kv.is_empty()) {
            if let Some((k @ ("branch" | "tag" | "rev"), v)) = kv.split_once('=') {
                cmd.push_str(&format!(" --{k} {}", quote(&percent_decode(v))));
            }
        }
        return format!("{cmd} --root {root}");
    }
    generic
}

/// `%XX` escapes in a URL's path, decoded.
fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        let hex = |c: u8| (c as char).to_digit(16);
        match (
            b[i],
            b.get(i + 1).copied().and_then(hex),
            b.get(i + 2).copied().and_then(hex),
        ) {
            (b'%', Some(h), Some(l)) => {
                out.push((h * 16 + l) as u8);
                i += 3;
            }
            (c, ..) => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// `s` as one shell word.
fn quote(s: &str) -> String {
    if !s.is_empty()
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"/._-+:=@,".contains(&b))
    {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', r"'\''"))
    }
}

/// Whether cargo has xcomm installed under `root`.
fn cargo_has_xcomm(root: &Path) -> bool {
    [".crates.toml", ".crates2.json"].iter().any(|f| {
        std::fs::read_to_string(root.join(f)).is_ok_and(|t| t.contains("\"xcomm "))
    })
}

fn remove_binary(env: &Env) -> Result<Step, InstallError> {
    // Each root once, however it is named: cargo's record of it goes with
    // the first uninstall.
    let mut roots: Vec<(&PathBuf, PathBuf)> = Vec::new();
    for r in &env.cargo_roots {
        let real = std::fs::canonicalize(r).unwrap_or_else(|_| r.clone());
        if cargo_has_xcomm(r) && !roots.iter().any(|(_, seen)| *seen == real) {
            roots.push((r, real));
        }
    }
    let roots: Vec<&PathBuf> = roots.into_iter().map(|(r, _)| r).collect();
    let mut removed = false;
    for root in roots {
        let cargo = env
            .cargo
            .as_ref()
            .ok_or_else(|| InstallError("cargo is not on PATH".into()))?;
        let reinstall = reinstall_command(root);
        let root = root.display().to_string();
        run(cargo, &["uninstall", "--root", &root, "xcomm"]).map_err(|e| {
            InstallError(format!(
                "{e} (if a file it installed is gone: {reinstall}, then run xcomm \
                 uninstall again)"
            ))
        })?;
        removed = true;
    }
    // What cargo did not install is not cargo's to take away.
    let left: Vec<String> = binaries(env)
        .into_iter()
        .map(|(p, _)| p.display().to_string())
        .collect();
    if !left.is_empty() {
        return Err(InstallError(format!(
            "still on PATH, not installed by cargo under {}: {}; remove it yourself",
            env.cargo_roots
                .iter()
                .map(|r| r.display().to_string())
                .collect::<Vec<_>>()
                .join(", "),
            left.join(", ")
        )));
    }
    Ok(if removed {
        Step::Done
    } else {
        Step::Skipped("no xcomm installed".into())
    })
}

/// Takes away, in order, the hotplug watchers, the skill, the herdr plugin
/// and the binaries, saying how each went; at the first failure it stops
/// and says what is left. False when a step failed (already said).
pub fn uninstall(env: &Env, out: &mut dyn Write) -> Result<bool, InstallError> {
    type Action = fn(&Env) -> Result<Step, InstallError>;
    let skill = format!("skill {}", env.skill().display());
    let steps: [(&str, Action); 4] = [
        ("hotplug watcher", stop_watchers),
        (&skill, remove_skill),
        ("herdr plugin", remove_plugin),
        ("binaries", remove_binary),
    ];
    let io = |e: std::io::Error| InstallError(format!("output: {e}"));
    for (i, (name, action)) in steps.iter().enumerate() {
        match action(env) {
            Ok(Step::Done) => writeln!(out, "{name}: ok").map_err(io)?,
            Ok(Step::Skipped(why)) => {
                writeln!(out, "{name}: skip ({why})").map_err(io)?
            }
            Err(e) => {
                writeln!(out, "{name}: FAILED: {e}").map_err(io)?;
                let left: Vec<&str> = steps[i..].iter().map(|(n, _)| *n).collect();
                writeln!(out, "not done: {}", left.join(", ")).map_err(io)?;
                return Ok(false);
            }
        }
    }
    Ok(true)
}

/// What a program named xcomm answers to `--version`.
enum Probe {
    Answered(String),
    /// It ran and refused the option: another program.
    Refused,
    /// It could not run, died of a signal, or did not answer in time.
    Broken(String),
}

/// Asks `p --version`, within a time limit, in a process group of its
/// own that is taken down afterwards: a wrapper may start a terminal
/// program or a daemon, and it must not hold doctor up. (A child that
/// leaves the group, with setsid, is out of reach: it is only not waited
/// for.)
fn version_of(p: &Path) -> Probe {
    use std::io::Read;
    use std::os::unix::process::CommandExt;
    let limit = std::time::Duration::from_secs(3);
    let mut child = match Command::new(p)
        .arg("--version")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .process_group(0)
        .spawn()
    {
        Ok(c) => c,
        Err(e) => return Probe::Broken(e.to_string()),
    };
    let group = rustix::process::Pid::from_raw(child.id() as i32);
    let kill_group = || {
        if let Some(g) = group {
            let _ = rustix::process::kill_process_group(g, rustix::process::Signal::KILL);
        }
    };
    // Read as it comes, so a long answer does not fill the pipe; the start
    // of it is passed on as it is read, not at the end, which a child
    // holding the pipe may put off for good.
    let mut stdout = child.stdout.take().expect("piped");
    let (tx, rx) = std::sync::mpsc::channel::<Vec<u8>>();
    std::thread::spawn(move || {
        let (mut sent, mut buf) = (0, [0u8; 4096]);
        while let Ok(n @ 1..) = stdout.read(&mut buf) {
            if sent < 4096 && tx.send(buf[..n].to_vec()).is_err() {
                return;
            }
            sent += n;
        }
    });
    let until = std::time::Instant::now() + limit;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) if std::time::Instant::now() < until => {
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            _ => break None,
        }
    };
    // Whatever it left running goes too, which also ends the read.
    kill_group();
    let Some(status) = status else {
        let _ = child.wait();
        return Probe::Broken(format!("no answer to --version in {} s", limit.as_secs()));
    };
    // What it wrote is in the pipe by now: take it until the pipe closes,
    // or briefly if something outside the group holds it open.
    let mut out = Vec::new();
    let settle = std::time::Instant::now() + std::time::Duration::from_millis(300);
    while let Ok(chunk) =
        rx.recv_timeout(settle.saturating_duration_since(std::time::Instant::now()))
    {
        out.extend_from_slice(&chunk);
    }
    use std::os::unix::process::ExitStatusExt;
    match (status.code(), status.signal()) {
        (Some(0), _) => Probe::Answered(String::from_utf8_lossy(&out).trim().to_string()),
        (_, Some(sig)) => Probe::Broken(format!("killed by signal {sig}")),
        // The shell's and the loader's: it could not be run at all (a
        // missing library, the wrong architecture).
        (Some(code @ (126 | 127)), _) => {
            Probe::Broken(format!("exit status {code}: it cannot be run"))
        }
        _ => Probe::Refused,
    }
}

/// A program on PATH, with its version or why it does not run.
type Found = (PathBuf, String);

/// The xcomm binaries on PATH with their versions, and those named xcomm
/// that do not run (a broken install, or another program: no telling).
/// One that runs and answers as something else, or refuses `--version`,
/// is another program.
fn binaries_and_broken(env: &Env) -> (Vec<Found>, Vec<Found>) {
    let mut seen: Vec<PathBuf> = Vec::new();
    let (mut ours, mut broken) = (Vec::new(), Vec::new());
    for dir in &env.path {
        let p = dir.join("xcomm");
        if !executable(&p) {
            continue;
        }
        let real = std::fs::canonicalize(&p).unwrap_or_else(|_| p.clone());
        if seen.contains(&real) {
            continue;
        }
        seen.push(real);
        match version_of(&p) {
            Probe::Answered(v) if v.starts_with("xcomm ") => ours.push((p, v)),
            Probe::Answered(_) | Probe::Refused => {}
            Probe::Broken(why) => broken.push((p, why)),
        }
    }
    (ours, broken)
}

fn binaries(env: &Env) -> Vec<(PathBuf, String)> {
    binaries_and_broken(env).0
}

/// Sockets in the socket directories that no session answers on.
fn stale_sockets(env: &Env) -> Vec<PathBuf> {
    use std::os::unix::fs::FileTypeExt;
    let mut stale = Vec::new();
    for dir in &env.sock_dirs {
        for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
            let p = entry.path();
            let is_socket = entry.file_type().is_ok_and(|t| t.is_socket());
            if is_socket
                && p.extension().is_some_and(|e| e == "sock")
                && std::os::unix::net::UnixStream::connect(&p)
                    .is_err_and(|e| e.kind() == std::io::ErrorKind::ConnectionRefused)
            {
                stale.push(p);
            }
        }
    }
    stale.sort();
    stale
}

/// UUCP lock files whose process is gone.
fn stale_locks(env: &Env) -> Vec<PathBuf> {
    let mut stale = Vec::new();
    for dir in &env.lock_dirs {
        for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            let Some(tty) = name.strip_prefix("LCK..") else {
                continue;
            };
            if matches!(crate::lock::holder(dir, tty), Ok(None)) {
                stale.push(entry.path());
            }
        }
    }
    stale.sort();
    stale
}

/// Reports what is installed and what is left over. True when nothing
/// needs attention.
pub fn doctor(env: &Env, out: &mut dyn Write) -> Result<bool, InstallError> {
    let mut lines = Vec::new();
    let mut problems = Vec::new();
    let (found, broken) = binaries_and_broken(env);
    for (p, why) in &broken {
        problems.push(format!("{} does not run ({why})", p.display()));
    }
    if found.is_empty() {
        lines.push("binary: no xcomm on PATH".to_string());
    }
    for (p, v) in &found {
        // Where xcomm looks for it: beside the binary a link on PATH
        // leads to, not beside the link.
        let real = std::fs::canonicalize(p).unwrap_or_else(|_| p.clone());
        let wait = real.with_file_name(crate::ctl::WAIT_HELPER);
        lines.push(format!("binary: {} ({v})", p.display()));
        if !executable(&wait) {
            problems.push(format!(
                "{} is missing: xcomm ctl wait and run need it beside xcomm",
                wait.display()
            ));
        }
    }
    let versions: Vec<&str> = found.iter().map(|(_, v)| v.as_str()).collect();
    if versions.windows(2).any(|w| w[0] != w[1]) {
        problems.push("the xcomm binaries on PATH are not the same version".into());
    }
    let skill = env.skill();
    let written_by = match skill_state(env) {
        Ok(SkillState::Absent) => {
            lines.push(format!("skill: none at {}", skill.display()));
            None
        }
        Ok(SkillState::NotOurs(why, v)) => {
            lines.push(format!("skill: {} ({why})", skill.display()));
            v
        }
        Ok(SkillState::Ours(v)) => {
            lines.push(format!(
                "skill: {} (generated by xcomm {v})",
                skill.display()
            ));
            Some(v)
        }
        Err(e) => {
            problems.push(format!("skill: {e}"));
            None
        }
    };
    // One xcomm wrote, wherever it is kept: it should name a tool that is
    // there.
    if let Some(v) = written_by {
        if found.is_empty() {
            problems.push(format!(
                "the skill at {} is left from an install: no xcomm is on PATH",
                skill.display()
            ));
        } else if !versions.contains(&format!("xcomm {v}").as_str()) {
            problems.push(format!(
                "the skill is xcomm {v}'s, the binary is not: run xcomm skill install{}",
                match &env.dotfiles {
                    Some(_) =>
                        format!(" --dir {}", skill.parent().unwrap_or(&skill).display()),
                    None => String::new(),
                }
            ));
        }
    }
    let plugin = plugin_state(env);
    let registered = !matches!(plugin, Ok(PluginState::NotRegistered));
    match (&env.herdr, plugin) {
        (None, _) => lines.push("herdr plugin: herdr is not on PATH".into()),
        (_, Ok(PluginState::NotRegistered)) => {
            lines.push("herdr plugin: not registered".into())
        }
        (_, Ok(PluginState::Github)) => {
            lines.push("herdr plugin: installed from GitHub".into())
        }
        (_, Ok(PluginState::Linked(root))) => {
            lines.push(format!("herdr plugin: linked to {}", root.display()));
            if !root.join("herdr-plugin.toml").is_file() {
                problems.push(format!(
                    "the plugin is linked to {}, which has no herdr-plugin.toml",
                    root.display()
                ));
            }
        }
        (_, Err(e)) => problems.push(format!("herdr plugin: cannot tell: {e}")),
    }
    // Only where a watcher was started: asking creates its pid file.
    for dir in watch_dirs(env)
        .into_iter()
        .filter(|d| d.join("watch.pid").exists())
    {
        // Its first line: the rest is the config and the log's tail.
        let status = watch_act(&dir, crate::watch::WatchAction::Status)
            .map(|s| s.lines().next().unwrap_or_default().to_string())
            .unwrap_or_else(|e| format!("cannot tell: {e}"));
        // What an xcomm up to v0.1.2 left once its watcher ended: nothing to
        // report. Not removed here or by watch: an old xcomm may be taking
        // its lock that moment (uninstall removes it).
        if crate::watch::is_old_state_dir(&dir) && status.starts_with("not running") {
            continue;
        }
        lines.push(format!("hotplug watcher {}: {status}", dir.display()));
        if status.starts_with("running") && (found.is_empty() || !registered) {
            problems.push(format!(
                "a hotplug watcher still runs ({}) with {}: xcomm uninstall stops it",
                dir.display(),
                if found.is_empty() {
                    "no xcomm on PATH"
                } else {
                    "the herdr plugin not registered"
                }
            ));
        }
    }
    for p in stale_sockets(env) {
        problems.push(format!("stale socket {} (no session answers)", p.display()));
    }
    for p in stale_locks(env) {
        problems.push(format!("stale lock {} (its process is gone)", p.display()));
    }
    let io = |e: std::io::Error| InstallError(format!("output: {e}"));
    for l in &lines {
        writeln!(out, "{l}").map_err(io)?;
    }
    for p in &problems {
        writeln!(out, "problem: {p}").map_err(io)?;
    }
    match problems.len() {
        0 => writeln!(out, "clean").map_err(io)?,
        n => writeln!(out, "{n} problem{}", if n == 1 { "" } else { "s" }).map_err(io)?,
    }
    Ok(problems.is_empty())
}
