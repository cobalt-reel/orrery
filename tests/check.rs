//! `orrery check`: the binary run on temp files, never the user's config.

use std::process::{Command, Output};

use orrery_wm::config;

fn check(args: &[&std::ffi::OsStr], config_home: &std::path::Path) -> Output {
    Command::new(env!("CARGO_BIN_EXE_orrery"))
        .arg("check")
        .args(args)
        .env("XDG_CONFIG_HOME", config_home)
        .env_remove("WAYLAND_DISPLAY")
        .output()
        .expect("run orrery check")
}

fn text(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

#[test]
fn the_example_is_ok() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.kdl");
    std::fs::write(&path, config::DEFAULT).unwrap();
    let out = check(&["--config".as_ref(), path.as_os_str()], dir.path());
    assert!(out.status.success(), "{}", text(&out.stderr));
    assert_eq!(text(&out.stdout), "ok\n");
}

#[test]
fn a_broken_file_lists_every_problem_with_its_place() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("broken.kdl");
    std::fs::write(&path, "mod \"Alt\"\nbogus 1\ncolors {\n    focus \"pink\"\n}\n").unwrap();
    let out = check(&["--config".as_ref(), path.as_os_str()], dir.path());
    assert_eq!(out.status.code(), Some(1));
    assert!(out.stdout.is_empty(), "{}", text(&out.stdout));
    let err = text(&out.stderr);
    let lines: Vec<&str> = err.lines().collect();
    let at = |place: &str| format!("{}:{place}", path.display());
    assert!(lines.iter().any(|l| l.starts_with(&at("2:1:"))), "{err}");
    assert!(lines.iter().any(|l| l.starts_with(&at("4:"))), "{err}");
}

#[test]
fn without_config_it_checks_the_default_path() {
    let home = tempfile::tempdir().unwrap();
    let out = check(&[], home.path());
    assert_eq!(out.status.code(), Some(1), "a missing file is not ok");
    assert!(text(&out.stderr).contains("built-in"), "{}", text(&out.stderr));

    std::fs::create_dir(home.path().join("orrery")).unwrap();
    std::fs::write(home.path().join("orrery").join("config.kdl"), config::DEFAULT).unwrap();
    let out = check(&[], home.path());
    assert!(out.status.success(), "{}", text(&out.stderr));
}
