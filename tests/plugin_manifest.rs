mod support;

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use support::lock_dir;
use toml::Value;

fn repo() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn manifest() -> Value {
    let text = std::fs::read_to_string(repo().join("herdr-plugin.toml")).unwrap();
    Value::Table(toml::from_str(&text).unwrap())
}

fn commands(m: &Value) -> Vec<Vec<String>> {
    ["build", "startup", "panes", "actions"]
        .iter()
        .flat_map(|t| {
            m.get(*t)
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default()
        })
        .map(|item| {
            item["command"]
                .as_array()
                .unwrap()
                .iter()
                .map(|v| v.as_str().unwrap().to_string())
                .collect()
        })
        .collect()
}

#[test]
fn the_manifest_is_what_herdr_takes() {
    let m = manifest();
    assert_eq!(m["id"].as_str(), Some("cantona.herdr-xcomm"));
    for key in ["name", "version", "min_herdr_version", "description"] {
        assert!(m[key].as_str().is_some_and(|s| !s.is_empty()), "{key}");
    }
    assert_eq!(m["version"].as_str(), Some(env!("CARGO_PKG_VERSION")));
    assert_eq!(m["min_herdr_version"].as_str(), Some("0.9.5"));
    for table in ["actions", "panes"] {
        let ids: Vec<&str> = m[table]
            .as_array()
            .unwrap()
            .iter()
            .map(|i| i["id"].as_str().unwrap())
            .collect();
        let mut unique = ids.clone();
        unique.sort();
        unique.dedup();
        assert_eq!(unique.len(), ids.len(), "{table}: {ids:?}");
        // herdr: letters, digits, colon, underscore, hyphen; no dots.
        for id in ids {
            assert!(
                id.chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, ':' | '_' | '-')),
                "{id}"
            );
        }
    }
    for action in m["actions"].as_array().unwrap() {
        assert!(
            action["title"]
                .as_str()
                .is_some_and(|t| t.starts_with("xcomm: "))
        );
        for c in action["contexts"].as_array().unwrap() {
            assert!(
                ["global", "workspace", "tab", "pane", "selection"].contains(&c.as_str().unwrap()),
                "{c}"
            );
        }
    }
    for pane in m["panes"].as_array().unwrap() {
        let popup = pane.get("placement").and_then(Value::as_str) == Some("popup");
        assert!(popup || (pane.get("width").is_none() && pane.get("height").is_none()));
    }
}

#[test]
fn every_command_is_there() {
    let bin = Path::new(env!("CARGO_BIN_EXE_xcomm"))
        .file_name()
        .unwrap()
        .to_owned();
    for argv in commands(&manifest()) {
        match argv[0].as_str() {
            "sh" => {
                let rest = &argv[1..];
                if rest[0] == "-c" {
                    continue;
                }
                let script = repo().join(&rest[0]);
                let mode = std::fs::metadata(&script).unwrap().permissions().mode();
                assert!(mode & 0o111 != 0, "{} is not executable", script.display());
            }
            // Built by scripts/build.sh: the binary cargo makes.
            other => {
                let path = Path::new(other);
                assert_eq!(path.parent(), Some(Path::new("target/release")), "{argv:?}");
                assert_eq!(path.file_name(), Some(bin.as_os_str()), "{argv:?}");
            }
        }
    }
    for script in [
        "scripts/build.sh",
        "scripts/install.sh",
        "scripts/plugin-action.sh",
    ] {
        let mode = std::fs::metadata(repo().join(script))
            .unwrap()
            .permissions()
            .mode();
        assert!(mode & 0o111 != 0, "{script}");
    }
}

/// A plugin root in a temp dir: the scripts, and fakes that log their
/// argv: target/release/xcomm (and `herdr`, through HERDR_BIN_PATH).
struct Root {
    dir: PathBuf,
}

impl Root {
    /// `xcomm_script` is the fake xcomm's body after logging.
    fn new(xcomm_script: &str) -> Root {
        let dir = lock_dir("plugin");
        std::fs::create_dir_all(dir.join("target/release")).unwrap();
        std::fs::create_dir_all(dir.join("scripts")).unwrap();
        std::fs::copy(
            repo().join("scripts/plugin-action.sh"),
            dir.join("scripts/plugin-action.sh"),
        )
        .unwrap();
        let fake = |path: PathBuf, log: &str, body: &str| {
            std::fs::write(
                &path,
                format!(
                    "#!/bin/sh\nprintf '%s|' \"$@\" >> '{}'\necho >> '{}'\n{body}\n",
                    dir.join(log).display(),
                    dir.join(log).display()
                ),
            )
            .unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        };
        fake(dir.join("target/release/xcomm"), "xcomm.log", xcomm_script);
        fake(dir.join("herdr"), "herdr.log", "exit ${HERDR_RC:-0}");
        Root { dir }
    }

    fn run(&self, argv: &[&str], env: &[(&str, &str)]) -> std::process::Output {
        Command::new(argv[0])
            .args(&argv[1..])
            .current_dir(&self.dir)
            .env("HERDR_BIN_PATH", self.dir.join("herdr"))
            .env("HERDR_PLUGIN_ID", "cantona.herdr-xcomm")
            .envs(env.iter().copied())
            .output()
            .unwrap()
    }

    fn log(&self, name: &str) -> String {
        std::fs::read_to_string(self.dir.join(name)).unwrap_or_default()
    }
}

impl Drop for Root {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn pane_command(id: &str) -> Vec<String> {
    let m = manifest();
    let pane = m["panes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["id"].as_str() == Some(id))
        .unwrap()
        .clone();
    pane["command"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect()
}

#[test]
fn the_session_and_prompt_panes_take_their_env() {
    let root = Root::new("exit 0");
    let argv = pane_command("session");
    let argv: Vec<&str> = argv.iter().map(String::as_str).collect();
    let o = root.run(
        &argv,
        &[
            ("XCOMM_DEVICE", "/dev/serial/by-id/usb-FTDI X"),
            ("XCOMM_BAUD", "9600"),
        ],
    );
    assert!(o.status.success());
    assert_eq!(
        root.log("xcomm.log"),
        "-D|/dev/serial/by-id/usb-FTDI X|-b|9600|\n"
    );
    let root = Root::new("exit 0");
    let argv = pane_command("prompt");
    let argv: Vec<&str> = argv.iter().map(String::as_str).collect();
    root.run(&argv, &[("XCOMM_PROMPT", "baud"), ("XCOMM_PANE", "w1:p2")]);
    assert_eq!(root.log("xcomm.log"), "pick|--prompt|baud|--pane|w1:p2|\n");
    // Opened by hand, without them: it says so rather than only flash.
    let root = Root::new("exit 0");
    let o = root.run(&argv, &[]);
    assert_eq!(o.status.code(), Some(2));
    assert!(
        String::from_utf8_lossy(&o.stdout).contains("opened by the xcomm actions"),
        "{o:?}"
    );
    assert_eq!(root.log("xcomm.log"), "");
}

const FAKE_XCOMM: &str = r#"case "$*" in
"ctl --pane-from-context") echo w1:p2 ;;
*"dtr toggle") echo "DTR off" ;;
*"--json status") if [ -n "${NO_SESSION:-}" ]; then echo "xcomm: no xcomm session in pane w1:p2" >&2; exit 3; fi; rw='{"ctl":"rw"}'; echo "${STATUS:-$rw}" ;;
*"capture stop") echo "/home/u/cap.txt (12 bytes)" ;;
*" break") echo "no xcomm session in pane w1:p2" >&2; exit 3 ;;
"watch status") echo "running (pid 9)" ;;
esac"#;

#[test]
fn pane_actions_act_on_the_pane_and_say_what_happened() {
    let action = |name: &str, env: &[(&str, &str)]| {
        let root = Root::new(FAKE_XCOMM);
        let o = root.run(&["sh", "scripts/plugin-action.sh", name], env);
        (
            o.status.code(),
            root.log("xcomm.log"),
            root.log("herdr.log"),
        )
    };
    let (code, xcomm, herdr) = action("dtr", &[]);
    assert_eq!(code, Some(0));
    assert!(xcomm.contains("ctl|--pane|w1:p2|dtr|toggle|"), "{xcomm}");
    assert_eq!(herdr, "notification|show|xcomm: dtr|--body|DTR off|\n");
    // An error is shown, and the action fails.
    let (code, _, herdr) = action("break", &[]);
    assert_eq!(code, Some(1));
    assert!(
        herdr.contains("xcomm: break|--body|no xcomm session in pane w1:p2|"),
        "{herdr}"
    );
    // Capture: none running -> the prompt popup; running -> stopped.
    let (_, _, herdr) = action("capture", &[("STATUS", r#"{"capture":null}"#)]);
    assert_eq!(
        herdr,
        "plugin|pane|open|--plugin|cantona.herdr-xcomm|--entrypoint|prompt|--env|XCOMM_PROMPT=capture|--env|XCOMM_PANE=w1:p2|\n"
    );
    let (_, xcomm, herdr) = action("capture", &[("STATUS", r#"{"capture":"/home/u/cap.txt"}"#)]);
    assert!(xcomm.contains("capture|stop|"), "{xcomm}");
    assert!(
        herdr.contains("capture stopped|--body|/home/u/cap.txt (12 bytes)"),
        "{herdr}"
    );
    let (_, _, herdr) = action("baud", &[]);
    assert!(
        herdr.contains("--entrypoint|prompt|--env|XCOMM_PROMPT=baud|"),
        "{herdr}"
    );
    // A prompt only opens on a session: otherwise the error is shown at
    // once, not after the answer has been typed.
    let (code, _, herdr) = action("capture", &[("STATUS", r#"{"capture":null,"ctl":"ro"}"#)]);
    assert_eq!(code, Some(1));
    assert_eq!(
        herdr,
        "notification|show|xcomm: capture|--body|xcomm: the control socket is read-only (--ctl ro)|\n"
    );
    for name in ["baud", "send-file", "recv-file"] {
        let (code, xcomm, herdr) = action(name, &[("NO_SESSION", "1")]);
        assert_eq!(code, Some(1), "{name}");
        assert!(
            xcomm.contains("ctl|--pane|w1:p2|--json|status|"),
            "{name}: {xcomm}"
        );
        assert_eq!(
            herdr,
            format!(
                "notification|show|xcomm: {name}|--body|xcomm: no xcomm session in pane w1:p2|\n"
            ),
            "{name}"
        );
        // A read-only control socket would refuse the answer: said at once too.
        let (code, _, herdr) = action(name, &[("STATUS", r#"{"ctl":"ro"}"#)]);
        assert_eq!(code, Some(1), "{name}");
        assert_eq!(
            herdr,
            format!(
                "notification|show|xcomm: {name}|--body|xcomm: the control socket is read-only (--ctl ro)|\n"
            ),
            "{name}"
        );
    }
    let (code, xcomm, herdr) = action("watch-status", &[]);
    assert_eq!(code, Some(0));
    assert!(xcomm.contains("watch|status|"), "{xcomm}");
    assert!(
        herdr.contains("hotplug watcher|--body|running (pid 9)"),
        "{herdr}"
    );
    let (code, _, _) = action("frob", &[]);
    assert_eq!(code, Some(2));
    // Opening a popup is an action like any other: a failure is shown.
    let (code, _, herdr) = action("open", &[]);
    assert_eq!(code, Some(0));
    assert!(
        herdr.contains("plugin|pane|open|--plugin|cantona.herdr-xcomm|--entrypoint|picker|"),
        "{herdr}"
    );
    let (code, _, herdr) = action("open", &[("HERDR_RC", "7")]);
    assert_eq!(code, Some(1));
    assert!(herdr.contains("notification|show|xcomm: open|"), "{herdr}");
    let (code, _, herdr) = action(
        "capture",
        &[("STATUS", r#"{"capture":null}"#), ("HERDR_RC", "7")],
    );
    assert_eq!(code, Some(1));
    assert!(
        herdr.contains("notification|show|xcomm: capture|"),
        "{herdr}"
    );
    // A break says so: `xcomm ctl break` prints nothing of its own.
    let root = Root::new("exit 0");
    let o = root.run(&["sh", "scripts/plugin-action.sh", "break"], &[]);
    assert_eq!(o.status.code(), Some(0));
    assert!(
        root.log("herdr.log")
            .contains("notification|show|xcomm: break|--body|break sent|"),
        "{}",
        root.log("herdr.log")
    );
}

#[test]
fn a_pane_action_outside_a_session_says_so() {
    let root = Root::new(
        r#"echo "no pane in HERDR_PLUGIN_CONTEXT_JSON (run from a herdr plugin action)" >&2; exit 1"#,
    );
    let o = root.run(&["sh", "scripts/plugin-action.sh", "dtr"], &[]);
    assert_eq!(o.status.code(), Some(1));
    assert!(
        root.log("herdr.log")
            .contains("xcomm: dtr|--body|no pane in HERDR_PLUGIN_CONTEXT_JSON"),
        "{}",
        root.log("herdr.log")
    );
}
