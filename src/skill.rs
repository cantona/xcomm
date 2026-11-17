// SPDX-FileCopyrightText: 2026 Su Kang Yin <paradyse@gmail.com>
// SPDX-License-Identifier: GPL-2.0-only

//! The agent skill: `skills/xcomm/SKILL.md`, printed by `xcomm --skill` and
//! written by `xcomm skill install`.

use std::fmt;
use std::io::Write;
use std::path::{Path, PathBuf};

pub const SKILL_MD: &str = include_str!("../skills/xcomm/SKILL.md");

/// In the front matter of an installed copy: what `install` may replace.
const MARKER: &str = "generated-by: xcomm";

#[derive(Debug)]
pub struct SkillError(pub String);

impl fmt::Display for SkillError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for SkillError {}

/// SKILL_MD with `generated-by: xcomm <version>` in its front matter's
/// metadata, so a later install knows the file is its own.
pub fn render_for_install() -> String {
    let (front, body) = split_front_matter(SKILL_MD).expect("SKILL.md has front matter");
    format!(
        "---\n{front}metadata:\n  {MARKER} {}\n---\n{body}",
        env!("CARGO_PKG_VERSION")
    )
}

/// The front matter's lines (each with its newline) and what follows the
/// closing `---` line.
fn split_front_matter(md: &str) -> Option<(&str, &str)> {
    let rest = md.strip_prefix("---\n")?;
    let end = rest.find("\n---\n")?;
    Some((&rest[..end + 1], &rest[end + 5..]))
}

/// The xcomm version that installed `md`, from the marker in its front
/// matter; None when it has none.
pub fn marker_version(md: &str) -> Option<String> {
    let (front, _) = split_front_matter(md)?;
    front.lines().find_map(|l| {
        l.trim_start()
            .strip_prefix(MARKER)?
            .strip_prefix(' ')
            .map(|v| v.trim().to_string())
    })
}

fn has_marker(md: &str) -> bool {
    marker_version(md).is_some()
}

/// Writes `dir/SKILL.md`. A SKILL.md already there is replaced only when
/// an earlier install wrote it: anything else is someone's own.
pub fn install(dir: &Path) -> Result<PathBuf, SkillError> {
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    let err =
        |what: &Path, e: std::io::Error| SkillError(format!("{}: {e}", what.display()));
    std::fs::create_dir_all(dir).map_err(|e| err(dir, e))?;
    let path = dir.join("SKILL.md");
    let mode = match std::fs::symlink_metadata(&path) {
        // Replacing the link would leave whatever it leads to as it was.
        Ok(m) if !m.file_type().is_file() => {
            return Err(SkillError(format!(
                "{} is not a plain file (a link?): left as it is; give --dir for \
                 where it should go",
                path.display()
            )));
        }
        Ok(m) => {
            let old = std::fs::read_to_string(&path).map_err(|e| err(&path, e))?;
            if !has_marker(&old) {
                return Err(SkillError(format!(
                    "{} is not one xcomm installed (no '{MARKER}' in its front \
                     matter): left as it is",
                    path.display()
                )));
            }
            m.permissions().mode() & 0o7777
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => 0o644,
        Err(e) => return Err(err(&path, e)),
    };
    // Whole or not at all: an agent may read it at any moment. A new name
    // of its own, never one that is there: it might be a link, or left by
    // an install that was killed.
    let (tmp, mut file) = (0..100)
        .find_map(|n| {
            let tmp = dir.join(format!(".SKILL.md.{}.{n}", std::process::id()));
            match std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&tmp)
            {
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => None,
                opened => Some(opened.map(|f| (tmp, f)).map_err(|e| err(dir, e))),
            }
        })
        .unwrap_or_else(|| {
            Err(SkillError(format!(
                "{}: 100 .SKILL.md.{}.* files are in the way",
                dir.display(),
                std::process::id()
            )))
        })?;
    let written = file
        .write_all(render_for_install().as_bytes())
        .and_then(|()| file.set_permissions(std::fs::Permissions::from_mode(mode)))
        .and_then(|()| file.sync_all())
        .map_err(|e| err(&tmp, e))
        .and_then(|()| std::fs::rename(&tmp, &path).map_err(|e| err(&path, e)));
    if let Err(e) = written {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    Ok(path)
}

/// `$HOME/.agents/skills/xcomm`, unless that looks managed by a dotfiles
/// repository: that is the user's to change, so it takes `--dir`.
pub fn default_dir(env: &dyn Fn(&str) -> Option<String>) -> Result<PathBuf, SkillError> {
    let home = env("HOME").filter(|h| h.starts_with('/')).ok_or_else(|| {
        SkillError("HOME is not set to an absolute path: give --dir".into())
    })?;
    let dir = Path::new(&home).join(".agents/skills/xcomm");
    match dotfiles_owner(&dir, Path::new(&home), env) {
        Some(why) => Err(SkillError(format!(
            "{} {why} (dotfiles?): give --dir to install there anyway",
            dir.display()
        ))),
        None => Ok(dir),
    }
}

/// Why `dir`, under `home`, may belong to a dotfiles repository; None when
/// nothing says so.
pub fn dotfiles_owner(
    dir: &Path,
    home: &Path,
    env: &dyn Fn(&str) -> Option<String>,
) -> Option<String> {
    for var in ["GIT_DIR", "GIT_WORK_TREE"] {
        if env(var).is_some_and(|v| !v.is_empty()) {
            return Some(format!("may be in the repository {var} names"));
        }
    }
    if let Some(repo) = git_work_tree(dir, home) {
        return Some(format!("is in the git work tree {}", repo.display()));
    }
    home_repo(home, env).map(|repo| {
        format!(
            "is under $HOME, which the repository {} may manage",
            repo.display()
        )
    })
}

/// The git work tree `path` is in, looked for from its nearest existing
/// ancestor with links resolved, up to `home` and no further: a repository
/// above $HOME is not the user's dotfiles.
fn git_work_tree(path: &Path, home: &Path) -> Option<PathBuf> {
    let real = path
        .ancestors()
        .find_map(|a| std::fs::canonicalize(a).ok())?;
    let home = std::fs::canonicalize(home).ok();
    for a in real.ancestors() {
        if a.join(".git").symlink_metadata().is_ok() {
            return Some(a.to_path_buf());
        }
        if home.as_deref() == Some(a) {
            break;
        }
    }
    None
}

/// A repository kept apart from the tree it manages, as yadm, vcsh and
/// the "bare repo" way of keeping dotfiles do: one directly in $HOME or
/// where yadm and vcsh keep theirs, whose `core.worktree` is $HOME or
/// above it, or a bare one set up as that recipe sets it
/// (`status.showUntrackedFiles = no`; a mirror clone is not). One kept
/// elsewhere, or tied to $HOME only on its command line, is not found.
fn home_repo(home: &Path, env: &dyn Fn(&str) -> Option<String>) -> Option<PathBuf> {
    let real_home = std::fs::canonicalize(home).ok()?;
    let manages_home = |d: &Path| {
        if !d.join("HEAD").is_file() || !d.join("objects").is_dir() {
            return false;
        }
        let Ok(config) = std::fs::read_to_string(d.join("config")) else {
            return false;
        };
        let mut bare = false;
        let mut hides_untracked = false;
        let mut worktree = None;
        for line in config.lines() {
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            let (key, value) = (key.trim().to_ascii_lowercase(), value.trim());
            match key.as_str() {
                "bare" => bare = value.eq_ignore_ascii_case("true"),
                "showuntrackedfiles" => {
                    hides_untracked = value.eq_ignore_ascii_case("no")
                }
                "worktree" => worktree = Some(value.trim_matches('"').to_string()),
                _ => {}
            }
        }
        let tree_holds_home = worktree.is_some_and(|w| {
            std::fs::canonicalize(d.join(w)).is_ok_and(|w| real_home.starts_with(w))
        });
        tree_holds_home || (bare && hides_untracked)
    };
    let dir_of = |var: &str, default: &str| {
        env(var)
            .filter(|v| v.starts_with('/'))
            .map_or_else(|| home.join(default), PathBuf::from)
    };
    let config = dir_of("XDG_CONFIG_HOME", ".config");
    let data = dir_of("XDG_DATA_HOME", ".local/share");
    let list = |d: &Path| -> Vec<PathBuf> {
        std::fs::read_dir(d)
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.path())
            .collect()
    };
    let mut places = list(home);
    places.push(data.join("yadm/repo.git"));
    places.push(config.join("yadm/repo.git"));
    places.extend(list(&config.join("vcsh/repo.d")));
    places.sort();
    places.into_iter().find(|d| manages_home(d))
}

/// The argv of each `xcomm ...` command in the ```sh and ```bash blocks of
/// `md`: at the start of a line or after `&&`, `||`, `;`, `|`, `&`, `$(`,
/// `(` or a backquote, past `NAME=value` assignments and `if`, `!` and the
/// like; a redirection and what it names are not arguments. A `\` that ends a line outside quotes joins the next.
pub fn commands_in(md: &str) -> Vec<Vec<String>> {
    let mut commands = Vec::new();
    let mut in_shell = None;
    let mut pending = String::new();
    for line in md.lines() {
        if pending.is_empty()
            && let Some(lang) = line.trim_start().strip_prefix("```")
        {
            in_shell = match in_shell {
                None => Some(matches!(lang.trim(), "sh" | "bash")),
                Some(_) => None,
            };
            continue;
        }
        if in_shell != Some(true) {
            continue;
        }
        pending.push_str(line);
        let (words, continued) = words(&pending);
        if continued {
            pending.pop();
            continue;
        }
        for segment in words.split(|w| w.operator) {
            let segment: Vec<&Word> = segment
                .iter()
                .skip_while(|w| {
                    is_assignment(&w.text) || PREFIXES.contains(&w.text.as_str())
                })
                .collect();
            if segment.first().is_some_and(|w| w.text == "xcomm") {
                commands.push(
                    segment
                        .iter()
                        .filter(|w| !w.redirect)
                        .map(|w| w.text.clone())
                        .collect(),
                );
            }
        }
        pending.clear();
    }
    commands
}

/// Words that may come before a command and are not it.
const PREFIXES: [&str; 10] = [
    "if", "then", "elif", "else", "while", "until", "do", "!", "time", "exec",
];

/// Whether what follows a `<` is `name>`, a placeholder, rather than a
/// redirection.
fn placeholder(rest: impl Iterator<Item = char>) -> bool {
    let mut n = 0;
    for c in rest {
        match c {
            '>' => return n > 0,
            'a'..='z' | '-' | '_' => n += 1,
            _ => return false,
        }
    }
    false
}

fn is_assignment(word: &str) -> bool {
    word.split_once('=').is_some_and(|(name, _)| {
        name.chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
            && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
    })
}

#[derive(Default)]
struct Word {
    text: String,
    operator: bool,
    /// The target of a redirection (`> out`, `2>&1`).
    redirect: bool,
}

/// Shell words: quotes and backslashes as sh reads them, operators as
/// words of their own, a `#` that starts a word to the end of the line.
/// Also whether the line ends in a `\` that joins the next.
fn words(line: &str) -> (Vec<Word>, bool) {
    let mut out: Vec<Word> = Vec::new();
    let mut cur: Option<String> = None;
    let mut redirect_next = false;
    let mut chars = line.chars().peekable();
    let end = |cur: &mut Option<String>, out: &mut Vec<Word>, redirect: &mut bool| {
        if let Some(text) = cur.take() {
            out.push(Word {
                text,
                redirect: std::mem::take(redirect),
                ..Word::default()
            });
        }
    };
    let op = |text: &str| Word {
        text: text.into(),
        operator: true,
        ..Word::default()
    };
    while let Some(c) = chars.next() {
        match c {
            ' ' | '\t' => end(&mut cur, &mut out, &mut redirect_next),
            '#' if cur.is_none() => break,
            '\'' => {
                let w = cur.get_or_insert_with(String::new);
                for c in chars.by_ref() {
                    if c == '\'' {
                        break;
                    }
                    w.push(c);
                }
            }
            '"' => {
                let w = cur.get_or_insert_with(String::new);
                while let Some(c) = chars.next() {
                    match c {
                        '"' => break,
                        '\\' if matches!(chars.peek(), Some('"' | '\\' | '$' | '`')) => {
                            w.push(chars.next().expect("peeked"));
                        }
                        c => w.push(c),
                    }
                }
            }
            '\\' => match chars.next() {
                Some(c) => cur.get_or_insert_with(String::new).push(c),
                None => return (out, true),
            },
            '$' if chars.peek() == Some(&'(') => {
                chars.next();
                end(&mut cur, &mut out, &mut redirect_next);
                out.push(op("$("));
            }
            '(' | ')' | '`' => {
                end(&mut cur, &mut out, &mut redirect_next);
                out.push(op(&c.to_string()));
            }
            // A placeholder, as the skill writes them: `<dev>`.
            '<' if cur.is_none() && placeholder(chars.clone()) => {
                let w = cur.get_or_insert_with(|| "<".into());
                for c in chars.by_ref() {
                    w.push(c);
                    if c == '>' {
                        break;
                    }
                }
            }
            '>' | '<' => {
                // A descriptor number before it (`2>`) is part of it.
                if cur
                    .as_deref()
                    .is_some_and(|w| w.bytes().all(|b| b.is_ascii_digit()))
                {
                    cur = None;
                }
                end(&mut cur, &mut out, &mut redirect_next);
                if chars.peek() == Some(&'>') {
                    chars.next();
                }
                if chars.peek() == Some(&'&') {
                    chars.next();
                }
                redirect_next = true;
            }
            '&' | '|' | ';' => {
                end(&mut cur, &mut out, &mut redirect_next);
                let mut text = c.to_string();
                if (c == '&' || c == '|') && chars.peek() == Some(&c) {
                    text.push(chars.next().expect("peeked"));
                }
                out.push(op(&text));
            }
            c => cur.get_or_insert_with(String::new).push(c),
        }
    }
    end(&mut cur, &mut out, &mut redirect_next);
    (out, false)
}
