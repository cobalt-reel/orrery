//! `examples/kitty/`: the snippet joins a config, the programs it starts are there and runnable, the shell scripts
//! parse and the bar's own tests pass (when python3 is installed). Nothing here starts kitty or orrery.

mod support;

use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::Command;

use orrery_wm::config::{self, Action};

fn kitty_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/kitty")
}

/// The example config with the snippet included, in a temp dir.
fn config_with_snippet(dir: &Path) -> PathBuf {
    let path = dir.join("config.kdl");
    let snippet = kitty_dir().join("orrery.kdl");
    std::fs::write(&path, format!("{}include \"{}\"\n", config::DEFAULT, snippet.display())).unwrap();
    path
}

/// Whether `program` runs here at all (`--version`), for the parts that need it.
fn have(program: &str) -> bool {
    Command::new(program)
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success())
}

#[test]
fn orrery_check_takes_a_config_including_the_snippet() {
    let dir = tempfile::tempdir().unwrap();
    let path = config_with_snippet(dir.path());
    let out = Command::new(support::BIN)
        .args(["check", "--config"])
        .arg(&path)
        .env("XDG_CONFIG_HOME", dir.path())
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(String::from_utf8_lossy(&out.stdout), "ok\n");
}

#[test]
fn the_snippets_programs_are_the_scripts_beside_it_and_executable() {
    let dir = tempfile::tempdir().unwrap();
    let path = config_with_snippet(dir.path());
    let read = config::parse_file(&std::fs::read_to_string(&path).unwrap(), &path, None);
    let c = read.config.expect("parses");
    let starts: Vec<&config::Start> = c.spawn_at_start.iter().collect();
    assert!(starts.iter().all(|s| !s.launch), "none opens a window: {starts:?}");
    let mut programs: Vec<String> = starts.iter().map(|s| s.argv[0].clone()).collect();
    for b in &c.binds {
        match &b.action {
            Action::SpawnNoLaunch(argv) => programs.push(argv[0].clone()),
            Action::Spawn(argv) => assert!(!argv[0].contains('/'), "the snippet's binds record no launch"),
            _ => {}
        }
    }
    let ours: Vec<&String> = programs.iter().filter(|p| p.contains('/')).collect();
    assert_eq!(ours.len(), 3, "bars.py, dropdown, launcher.sh: {programs:?}");
    for p in ours {
        let p = Path::new(p);
        assert!(p.starts_with(kitty_dir()), "{}", p.display());
        let mode = std::fs::metadata(p).unwrap().permissions().mode();
        assert!(mode & 0o111 == 0o111, "{} is not executable", p.display());
    }
}

#[test]
fn the_shell_scripts_parse() {
    if !Path::new("/bin/sh").exists() {
        eprintln!("no /bin/sh: skipped");
        return;
    }
    for script in ["dropdown", "launcher.sh"] {
        let out = Command::new("/bin/sh")
            .arg("-n")
            .arg(kitty_dir().join(script))
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{script}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
}

#[test]
fn the_bars_own_tests_pass() {
    if !have("python3") {
        eprintln!("no python3: skipped");
        return;
    }
    let out = Command::new("python3")
        .args(["-B", "-m", "unittest", "discover", "-s"])
        .arg(kitty_dir())
        .env("PYTHONDONTWRITEBYTECODE", "1")
        .env_remove("ORRERY")
        .env_remove("ORRERY_SOCKET")
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
}
