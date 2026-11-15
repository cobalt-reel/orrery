//! `orrery check`: the binary run on temp files, never the user's config.

mod support;

use std::os::unix::fs::PermissionsExt as _;
use std::process::{Command, Output};

use orrery_wm::comp::config_path_in;
use orrery_wm::{NAME, config};

fn check(args: &[&std::ffi::OsStr], config_home: &std::path::Path) -> Output {
    Command::new(support::BIN)
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

    std::fs::create_dir(home.path().join(NAME)).unwrap();
    std::fs::write(config_path_in(home.path()), config::DEFAULT).unwrap();
    let out = check(&[], home.path());
    assert!(out.status.success(), "{}", text(&out.stderr));
}

#[test]
fn the_link_is_reported_and_its_secret_file_checked() {
    let dir = tempfile::tempdir().unwrap();
    let secret = dir.path().join("secret");
    let contents = "never-shown-0b5e and enough more to be 32 bytes";
    std::fs::write(&secret, contents).unwrap();
    std::fs::set_permissions(&secret, std::fs::Permissions::from_mode(0o600)).unwrap();
    let path = dir.path().join("config.kdl");
    let link = format!("listen \"127.0.0.1:7340\" secret-file=\"{}\"", secret.display());
    std::fs::write(&path, format!("{}\nname \"desk\"\n{link}\n", config::DEFAULT)).unwrap();
    let out = check(&["--config".as_ref(), path.as_os_str()], dir.path());
    assert!(out.status.success(), "{}", text(&out.stderr));
    let said = text(&out.stdout);
    assert!(
        said.starts_with("ok\nlink: \"desk\", the sun, listening on 127.0.0.1:7340"),
        "{said}"
    );
    assert!(
        said.contains(&secret.display().to_string()) && !said.contains("never-shown"),
        "{said}"
    );

    // Open to others: refused, with the path and never the contents.
    std::fs::set_permissions(&secret, std::fs::Permissions::from_mode(0o644)).unwrap();
    let out = check(&["--config".as_ref(), path.as_os_str()], dir.path());
    assert_eq!(out.status.code(), Some(1));
    let err = text(&out.stderr);
    assert!(
        err.contains(&secret.display().to_string()) && err.contains("chmod 600"),
        "{err}"
    );
    assert!(!err.contains("never-shown") && out.stdout.is_empty(), "{err}");
}
