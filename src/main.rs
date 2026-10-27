use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Args, Parser, Subcommand};
use orrery_wm::comp::{self, Backend, RunOptions, ipc_server};
use orrery_wm::ipc::Response;
use orrery_wm::{NAME, config, ipc};

#[derive(Parser)]
#[command(name = NAME, version, about = "A static tiling Wayland compositor")]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Run the compositor (the default).
    Run(RunArgs),
    #[command(
        about = format!("Send one request to a running {NAME}"),
        subcommand_negates_reqs = true,
        disable_help_subcommand = true,
        override_usage = msg_usage()
    )]
    Msg {
        #[arg(long, help = socket_help())]
        socket: Option<PathBuf>,
        /// Words (`focus-slot left`) or a single JSON request.
        #[arg(required = true)]
        request: Vec<String>,
        #[command(subcommand)]
        spawn: Option<MsgSpawn>,
    },
    /// Validate a config file: `ok`, or every problem as `FILE:LINE:COL: message`; exit 0 or 1.
    Check {
        #[arg(long, help = format!("Default: `{}`", config_default()))]
        config: Option<PathBuf>,
    },
}

/// `msg spawn` is a subcommand only so that its argv is taken verbatim, flags included; every other request is
/// words, with `--socket` before or after them.
#[derive(Subcommand)]
enum MsgSpawn {
    /// Launch a program as a `spawn` bind does: every word after `spawn` is its argv (no shell).
    Spawn {
        #[arg(required = true, trailing_var_arg = true, allow_hyphen_values = true)]
        argv: Vec<String>,
    },
}

#[derive(Args)]
struct RunArgs {
    /// `winit` runs as a window inside the current session; `headless` has no screen (tests).
    #[arg(long, value_enum, default_value = "winit")]
    backend: Backend,
    #[arg(long, help = format!("Default: `{}`; the built-in config when it does not exist", config_default()))]
    config: Option<PathBuf>,
    /// The headless output's size.
    #[arg(long, default_value = "1920x1080", value_parser = comp::parse_size)]
    size: (i32, i32),
    #[arg(long, help = socket_help())]
    socket: Option<PathBuf>,
}

// The default paths in `--help`, from `NAME` (doc comments cannot hold it).
fn socket_help() -> String {
    format!("The control socket. Default: `$XDG_RUNTIME_DIR/{NAME}.sock`")
}

/// `spawn` is the only subcommand; clap's own second form (`msg <COMMAND>`) would hide `--socket`.
fn msg_usage() -> String {
    let socket = "[--socket <SOCKET>]";
    format!("{NAME} msg {socket} <REQUEST>...\n       {NAME} msg {socket} spawn <PROGRAM> [ARGS]...")
}

/// How long a `run` refused by a starting orrery waits for it to answer, to join it.
const JOIN_WAIT: std::time::Duration = std::time::Duration::from_secs(1);

fn config_default() -> String {
    format!("$XDG_CONFIG_HOME/{NAME}/config.kdl")
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    // No verb means `run` with clap's own defaults, so they are written once (in the attributes above).
    let command = cli
        .command
        .unwrap_or_else(|| Cli::parse_from([NAME, "run"]).command.expect("`run` parses"));
    match command {
        Command::Run(args) => {
            // An orrery already answering on the control socket opens one more screen instead.
            let control = args.socket.clone().or_else(ipc_server::default_socket_path);
            if let Some(path) = &control
                && let Some(answer) = ipc_server::join(path)
            {
                return print_answer("run", answer);
            }
            // Logs go to stderr: stdout carries the WAYLAND_DISPLAY line for scripts.
            tracing_subscriber::fmt().with_writer(std::io::stderr).init();
            let opts = RunOptions {
                backend: args.backend,
                config: args.config,
                size: args.size,
                socket: args.socket,
            };
            match comp::run(opts) {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => {
                    // Another orrery took the socket between the probe and here (most likely one still starting,
                    // holding the lock): joined once it answers, if it does soon.
                    if e.is::<ipc_server::Taken>()
                        && let Some(path) = &control
                        && let Some(answer) = ipc_server::join_within(path, JOIN_WAIT)
                    {
                        return print_answer("run", answer);
                    }
                    eprintln!("{NAME}: {e:#}");
                    ExitCode::FAILURE
                }
            }
        }
        Command::Msg { socket, request, spawn } => {
            let words = match spawn {
                Some(MsgSpawn::Spawn { argv }) => std::iter::once("spawn".to_owned()).chain(argv).collect(),
                None => request,
            };
            msg(socket, &words)
        }
        Command::Check { config } => check(config),
    }
}

/// `orrery check`. Unlike `run`, a missing file is not ok: the point is to check a file.
fn check(path: Option<PathBuf>) -> ExitCode {
    let Some(path) = path.or_else(comp::default_config_path) else {
        eprintln!("{NAME} check: no config path: pass --config or set XDG_CONFIG_HOME or HOME");
        return ExitCode::FAILURE;
    };
    let src = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            eprintln!(
                "{}: no such file ({NAME} would use the built-in config)",
                path.display()
            );
            return ExitCode::FAILURE;
        }
        Err(e) => {
            eprintln!("cannot read {}: {e}", path.display());
            return ExitCode::FAILURE;
        }
    };
    match config::parse(&src) {
        Ok(_) => {
            println!("ok");
            ExitCode::SUCCESS
        }
        Err(e) => {
            // One line per problem, prefixed with the file so editors can jump to it.
            for problem in &e.problems {
                eprintln!("{}:{problem}", path.display());
            }
            ExitCode::FAILURE
        }
    }
}

/// `orrery msg`: one request, its answer as `print_answer` shows it.
fn msg(socket: Option<PathBuf>, words: &[String]) -> ExitCode {
    let request = match ipc::parse_words(words) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("{NAME} msg: {e}");
            return ExitCode::FAILURE;
        }
    };
    let Some(path) = socket.or_else(ipc_server::default_socket_path) else {
        eprintln!("{NAME} msg: no control socket: pass --socket or set XDG_RUNTIME_DIR");
        return ExitCode::FAILURE;
    };
    print_answer("msg", ipc_server::send(&path, &request))
}

/// A running orrery's answer: as JSON on stdout (pretty when it carries the state), the error also on stderr; exit
/// 0 only when it said ok. `verb` names the command when no answer came.
fn print_answer(verb: &str, answer: anyhow::Result<Response>) -> ExitCode {
    let response = match answer {
        Ok(r) => r,
        Err(e) => {
            eprintln!("{NAME} {verb}: {e:#}");
            return ExitCode::FAILURE;
        }
    };
    let json = if response.state.is_some() {
        serde_json::to_string_pretty(&response)
    } else {
        serde_json::to_string(&response)
    };
    match json {
        Ok(j) => println!("{j}"),
        Err(e) => eprintln!("{NAME} msg: {e}"),
    }
    if let Some(e) = &response.error {
        eprintln!("{NAME}: {e}");
    }
    if response.ok {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_defaults_come_from_clap() {
        let Some(Command::Run(args)) = Cli::parse_from([NAME, "run"]).command else {
            panic!("run parses");
        };
        assert_eq!(args.backend, Backend::Winit);
        assert_eq!(args.size, (1920, 1080));
        assert!(args.config.is_none() && args.socket.is_none());
    }

    #[test]
    fn msg_takes_options_before_or_after_the_words() {
        let Some(Command::Msg { request, socket, .. }) =
            Cli::parse_from([NAME, "msg", "--socket", "/s", "layout", "wall"]).command
        else {
            panic!("msg parses");
        };
        assert_eq!(request, ["layout", "wall"]);
        assert_eq!(socket, Some(PathBuf::from("/s")));
        let Some(Command::Msg { request, socket, .. }) =
            Cli::parse_from([NAME, "msg", "focus-slot", "left", "--socket", "/t"]).command
        else {
            panic!("msg parses");
        };
        assert_eq!(request, ["focus-slot", "left"]);
        assert_eq!(socket, Some(PathBuf::from("/t")));
    }

    #[test]
    fn msg_spawn_passes_the_programs_own_flags() {
        let Some(Command::Msg {
            socket,
            spawn: Some(MsgSpawn::Spawn { argv }),
            ..
        }) = Cli::parse_from([
            NAME,
            "msg",
            "--socket",
            "/s",
            "spawn",
            "kitty",
            "--single-instance",
            "--socket",
            "x",
        ])
        .command
        else {
            panic!("msg spawn parses");
        };
        assert_eq!(
            argv,
            ["kitty", "--single-instance", "--socket", "x"],
            "all of it is argv"
        );
        assert_eq!(socket, Some(PathBuf::from("/s")));
        assert!(
            Cli::try_parse_from([NAME, "msg", "spawn"]).is_err(),
            "spawn needs a program"
        );
        // Only a first word is the subcommand: a layout may be called spawn.
        let Some(Command::Msg {
            request, spawn: None, ..
        }) = Cli::parse_from([NAME, "msg", "layout", "spawn", "x"]).command
        else {
            panic!("msg parses as words");
        };
        assert_eq!(request, ["layout", "spawn", "x"]);
        // `help` is no subcommand (an unknown request, answered as such), and a program's --help is its own.
        let Some(Command::Msg {
            request, spawn: None, ..
        }) = Cli::parse_from([NAME, "msg", "help"]).command
        else {
            panic!("msg help parses as words");
        };
        assert_eq!(request, ["help"]);
        let Some(Command::Msg {
            spawn: Some(MsgSpawn::Spawn { argv }),
            ..
        }) = Cli::parse_from([NAME, "msg", "spawn", "kitty", "--help"]).command
        else {
            panic!("msg spawn parses");
        };
        assert_eq!(argv, ["kitty", "--help"]);
    }
}
