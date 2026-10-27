//! `spawn` binds and `orrery msg spawn`: start a program with orrery as its display, detached from orrery's
//! terminal and signals.
//! Children are reaped on SIGCHLD (`Orrery::reap_children`, from the signal source in `run`). They are not
//! signalled when orrery exits: each leads its own process group, deliberately, so a program started from
//! orrery outlives it like one started from a launcher.
//! Each start is recorded as a `Launch`, so its window opens where the start was made (see `placement`).

use std::ffi::OsStr;
use std::os::unix::process::CommandExt as _;
use std::process::{Child, Command, Stdio};
use std::time::Instant;

use super::Orrery;
use crate::placement::Launch;

/// Where a launched program finds its xdg-activation token: the protocol's name, and the older startup-notification
/// one that some toolkits still read.
const TOKEN_VARS: [&str; 2] = ["XDG_ACTIVATION_TOKEN", "DESKTOP_STARTUP_ID"];

/// The command for `argv` (no shell): `WAYLAND_DISPLAY` set to orrery's socket and `DISPLAY` removed (so no
/// toolkit falls back to the host's Xwayland) for the child only (orrery's
/// own environment keeps the host's, which winit needs), `token` as its xdg-activation token under both names
/// toolkits read (removed without one: a token orrery was started with is not orrery's to hand on), no stdio, its
/// own process group (a Ctrl+C in orrery's terminal does not reach it), and an empty signal mask. None for an
/// empty argv.
#[must_use]
pub fn command(argv: &[String], display: &OsStr, token: Option<&str>) -> Option<Command> {
    let (program, args) = argv.split_first()?;
    let mut cmd = Command::new(program);
    cmd.args(args).env("WAYLAND_DISPLAY", display).env_remove("DISPLAY");
    for name in TOKEN_VARS {
        match token {
            Some(t) => cmd.env(name, t),
            None => cmd.env_remove(name),
        };
    }
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0);
    // calloop's Signals blocks SIGTERM/SIGINT in orrery's thread and a child inherits the mask across exec:
    // without this, the child could never be stopped with SIGTERM.
    // A pre_exec hook makes std fork and exec rather than posix_spawn; that is what lets this run in the child.
    // Safety: between fork and exec only async-signal-safe calls are allowed; sigemptyset and sigprocmask are
    // (pthread_sigmask is not on POSIX's list), and nothing here allocates.
    unsafe {
        cmd.pre_exec(|| {
            let mut empty = std::mem::MaybeUninit::<libc::sigset_t>::uninit();
            libc::sigemptyset(empty.as_mut_ptr());
            if libc::sigprocmask(libc::SIG_SETMASK, empty.as_ptr(), std::ptr::null_mut()) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    Some(cmd)
}

impl Orrery {
    /// Starts `argv` (see `command`) with a fresh xdg-activation token and records it as a launch aimed at the
    /// focused slot; the error says why it could not be started (a key logs it, `msg` answers it).
    pub fn spawn(&mut self, argv: &[String]) -> Result<(), String> {
        // Smithay mints it; the launch keeps it. Smithay's pool holds only the client tokens orrery keeps (see
        // `token_created`), whose rules are not a launch's, so it is taken out again.
        let token = self.xdg_activation_state.create_external_token(None).0.clone();
        self.xdg_activation_state.remove_token(&token);
        let token = String::from(token);
        let Some(mut cmd) = command(argv, &self.socket_name, Some(&token)) else {
            return Err("nothing to spawn: the argv is empty".into());
        };
        match cmd.spawn() {
            Ok(child) => {
                tracing::info!("spawned {argv:?} (pid {})", child.id());
                self.record_launch(child.id(), token);
                self.children.push(child);
                Ok(())
            }
            Err(e) => Err(format!("cannot spawn {argv:?}: {e}")),
        }
    }

    /// A launch of `pid` aimed at the focused workspace's focused slot as it is now. Without a monitor there is no
    /// target, and nothing is recorded: the window goes to `pending` either way.
    fn record_launch(&mut self, pid: u32, token: String) {
        let now = Instant::now();
        self.launches.expire(now);
        let Some(ws) = self.stacks.shown(self.stacks.focused_monitor()) else {
            return;
        };
        self.launches.push(Launch {
            pid,
            token,
            workspace: ws.number(),
            slot: ws.focus(),
            at: now,
            claimed: false,
        });
    }

    /// Waits for children that have exited, so none stays a zombie. Called on SIGCHLD; signals coalesce, so
    /// every child is tried each time.
    pub fn reap_children(&mut self) {
        self.children.retain_mut(|child: &mut Child| match child.try_wait() {
            Ok(Some(status)) => {
                tracing::debug!("child {} exited: {status}", child.id());
                false
            }
            Ok(None) => true,
            Err(e) => {
                tracing::warn!("cannot wait for child {}: {e}", child.id());
                false
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Blocks SIGTERM and SIGINT in this thread, as calloop does in orrery's.
    fn block_term() {
        unsafe {
            let mut set = std::mem::MaybeUninit::<libc::sigset_t>::uninit();
            libc::sigemptyset(set.as_mut_ptr());
            libc::sigaddset(set.as_mut_ptr(), libc::SIGTERM);
            libc::sigaddset(set.as_mut_ptr(), libc::SIGINT);
            assert_eq!(
                libc::pthread_sigmask(libc::SIG_BLOCK, set.as_ptr(), std::ptr::null_mut()),
                0
            );
        }
    }

    #[test]
    fn the_child_does_not_see_the_host_x_display() {
        // With DISPLAY set (Xwayland in the host), toolkits may pick X and open outside orrery.
        let cmd = command(&["true".to_owned()], OsStr::new("wayland-9"), None).unwrap();
        let envs: Vec<_> = cmd.get_envs().collect();
        assert!(envs.contains(&(OsStr::new("DISPLAY"), None)), "{envs:?}");
        assert!(envs.contains(&(OsStr::new("WAYLAND_DISPLAY"), Some(OsStr::new("wayland-9")))));
    }

    #[test]
    fn the_child_gets_its_activation_token_under_both_names() {
        let cmd = command(&["true".to_owned()], OsStr::new("wayland-9"), Some("tok123")).unwrap();
        let envs: Vec<_> = cmd.get_envs().collect();
        for name in ["XDG_ACTIVATION_TOKEN", "DESKTOP_STARTUP_ID"] {
            assert!(
                envs.contains(&(OsStr::new(name), Some(OsStr::new("tok123")))),
                "{name}: {envs:?}"
            );
        }
        // Without one, a token orrery itself was started with is not handed on: it is not orrery's to give.
        let cmd = command(&["true".to_owned()], OsStr::new("wayland-9"), None).unwrap();
        let envs: Vec<_> = cmd.get_envs().collect();
        for name in ["XDG_ACTIVATION_TOKEN", "DESKTOP_STARTUP_ID"] {
            assert!(envs.contains(&(OsStr::new(name), None)), "{name}: {envs:?}");
        }
    }

    #[test]
    fn empty_argv_is_nothing() {
        assert!(command(&[], OsStr::new("wayland-9"), None).is_none());
    }

    #[test]
    fn the_child_gets_the_display_its_own_group_and_no_blocked_signals() {
        // Run in its own thread so the blocked mask does not leak into other tests.
        let out = std::thread::spawn(|| {
            block_term();
            let argv: Vec<String> = [
                "sh",
                "-c",
                "echo \"$WAYLAND_DISPLAY\"; echo $$; cut -d' ' -f5 /proc/self/stat; grep SigBlk /proc/self/status",
            ]
            .map(String::from)
            .to_vec();
            let mut cmd = command(&argv, OsStr::new("wayland-test-7"), None).unwrap();
            cmd.stdout(Stdio::piped());
            cmd.output().unwrap()
        })
        .join()
        .unwrap();
        assert!(out.status.success());
        let text = String::from_utf8(out.stdout).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines[0], "wayland-test-7");
        assert_eq!(lines[1], lines[2], "the shell leads its own process group: {text}");
        let mask = lines[3].trim_start_matches("SigBlk:").trim();
        assert!(mask.chars().all(|c| c == '0'), "no signal blocked in the child: {text}");
    }
}
