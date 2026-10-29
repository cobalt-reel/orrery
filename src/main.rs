use std::io::Write as _;
use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Args, Parser, Subcommand};
use orrery_wm::comp::{self, BackendChoice, RunOptions, ipc_server, net};
use orrery_wm::ipc::{Request, Response};
use orrery_wm::link::Secret;
use orrery_wm::session::Role;
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
        #[arg(long, help = msg_socket_help())]
        socket: Option<PathBuf>,
        /// Run the request on that machine of the session (through the primary).
        #[arg(long, value_name = "MACHINE")]
        to: Option<String>,
        /// Words (`focus-slot left`) or a single JSON request.
        #[arg(required = true, allow_negative_numbers = true)]
        request: Vec<String>,
        #[command(subcommand)]
        spawn: Option<MsgSpawn>,
    },
    /// Validate a config file: `ok` (and the link, if any), or every problem as `FILE:LINE:COL: message`; exit 0
    /// or 1.
    Check {
        #[arg(long, help = format!("Default: `{}`", config_default()))]
        config: Option<PathBuf>,
    },
}

/// `msg spawn` is a subcommand only so that its argv is taken verbatim, flags included; every other request is
/// words, with `--socket` and `--to` before or after them.
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
    /// What orrery runs on.
    #[arg(long, value_enum, default_value = "auto")]
    backend: BackendChoice,
    #[arg(long, help = format!("Default: `{}`; the built-in config when it does not exist", config_default()))]
    config: Option<PathBuf>,
    /// The headless output's size.
    #[arg(long, default_value = "1920x1080", value_parser = comp::parse_size)]
    size: (i32, i32),
    #[arg(long, help = socket_help())]
    socket: Option<PathBuf>,
    /// A testing aid, off by default: answer `inject` (input as if from this machine's own devices) and
    /// `inject-remote` (input messages sent to a satellite as they are).
    #[arg(long)]
    debug_inject: bool,
}

// The default paths in `--help`, from `NAME` (doc comments cannot hold it).
fn socket_help() -> String {
    format!("The control socket. Default: `$XDG_RUNTIME_DIR/{NAME}.sock`")
}

fn msg_socket_help() -> String {
    format!(
        "The control socket. Default: `${}`, else `$XDG_RUNTIME_DIR/{NAME}.sock`",
        ipc_server::socket_env()
    )
}

/// `spawn` is the only subcommand; clap's own second form (`msg <COMMAND>`) would hide `--socket`.
fn msg_usage() -> String {
    let socket = "[--socket <SOCKET>] [--to <MACHINE>]";
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
                debug_inject: args.debug_inject,
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
        Command::Msg {
            socket,
            to,
            request,
            spawn,
        } => {
            let words = match spawn {
                Some(MsgSpawn::Spawn { argv }) => std::iter::once("spawn".to_owned()).chain(argv).collect(),
                None => request,
            };
            msg(socket, to, &words)
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
    let src = match config::read_file(&path, config::MAX_SOURCE) {
        Ok(Some(s)) => s,
        Ok(None) => {
            eprintln!("{}: the config is larger than 1 MiB", path.display());
            return ExitCode::FAILURE;
        }
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
    match config::parse_file(&src, &path, comp::home_dir().as_deref()).config {
        Ok(config) => {
            let Some(link) = &config.link else {
                println!("ok");
                return ExitCode::SUCCESS;
            };
            // The secret file is read as `run` reads it: what is wrong with it is said here, its contents never.
            let secret = net::secret_path(link);
            if let Err(e) = Secret::load(&secret) {
                eprintln!("{}: {e}", path.display());
                return ExitCode::FAILURE;
            }
            let name = net::machine_name(&config);
            let place = match link.role {
                Role::Primary => format!("the primary, listening on {}", link.addr),
                Role::Satellite => format!("a satellite of the primary at {}", link.addr),
            };
            println!("ok");
            println!("link: {name:?}, {place}, secret file {}", secret.display());
            ExitCode::SUCCESS
        }
        Err(e) => {
            // One line per problem, prefixed with its file (this one or one it includes) so editors can jump to it.
            for problem in &e.problems {
                eprintln!("{problem}");
            }
            ExitCode::FAILURE
        }
    }
}

/// The control socket `orrery msg` talks to: `--socket`, else `ORRERY_SOCKET` (`env`, set for the programs an orrery
/// starts; empty is unset), else the default.
fn msg_socket(
    flag: Option<PathBuf>,
    env: Option<std::ffi::OsString>,
    default: impl FnOnce() -> Option<PathBuf>,
) -> Option<PathBuf> {
    flag.or_else(|| env.filter(|e| !e.is_empty()).map(PathBuf::from))
        .or_else(default)
}

/// The request `orrery msg` sends for `words`, run on machine `to` when given (never a `subscribe`).
fn msg_request(to: Option<String>, words: &[String]) -> Result<Request, String> {
    let request = ipc::parse_words(words)?;
    Ok(match to {
        Some(_) if request == Request::Subscribe => return Err(ipc::SUBSCRIBE_HERE.into()),
        Some(machine) => Request::To {
            machine,
            request: Box::new(request),
        },
        None => request,
    })
}

/// `orrery msg`: one request (to `to`, when given), its answer as `print_answer` shows it; `subscribe` prints the
/// stream's lines as they come, until it ends.
fn msg(socket: Option<PathBuf>, to: Option<String>, words: &[String]) -> ExitCode {
    let request = match msg_request(to, words) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("{NAME} msg: {e}");
            return ExitCode::FAILURE;
        }
    };
    let env = std::env::var_os(ipc_server::socket_env());
    let Some(path) = msg_socket(socket, env, ipc_server::default_socket_path) else {
        eprintln!("{NAME} msg: no control socket: pass --socket or set XDG_RUNTIME_DIR");
        return ExitCode::FAILURE;
    };
    if request == Request::Subscribe {
        // Line by line, flushed as each comes: a bar reads them live.
        let print = |line: &str| {
            let mut out = std::io::stdout().lock();
            writeln!(out, "{line}")?;
            out.flush()
        };
        return match ipc_server::subscribe(&path, print) {
            Ok(true) => ExitCode::SUCCESS,
            Ok(false) => {
                eprintln!("{NAME} msg: the subscription was refused");
                ExitCode::FAILURE
            }
            Err(e) => {
                eprintln!("{NAME} msg: {e:#}");
                ExitCode::FAILURE
            }
        };
    }
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
        assert_eq!(args.backend, BackendChoice::Auto);
        let Some(Command::Run(args)) = Cli::parse_from([NAME, "run", "--backend", "drm"]).command else {
            panic!("run parses");
        };
        assert_eq!(args.backend, BackendChoice::Drm);
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
        // `--to` too, before or after; spawn's argv stays verbatim after it.
        let Some(Command::Msg { request, to, .. }) =
            Cli::parse_from([NAME, "msg", "workspace", "2", "--to", "desk"]).command
        else {
            panic!("msg parses");
        };
        assert_eq!(
            (request, to),
            (vec!["workspace".to_owned(), "2".into()], Some("desk".into()))
        );
        let Some(Command::Msg {
            to,
            spawn: Some(MsgSpawn::Spawn { argv }),
            ..
        }) = Cli::parse_from([NAME, "msg", "--to", "desk", "spawn", "foot", "--to", "x"]).command
        else {
            panic!("msg spawn parses");
        };
        assert_eq!(
            (to, argv),
            (Some("desk".into()), vec!["foot".into(), "--to".into(), "x".into()])
        );
    }

    #[test]
    fn msg_finds_the_socket_by_flag_then_environment_then_default() {
        let (flag, env, default) = ("/flag.sock", "/env.sock", "/default.sock");
        let pick = |f: Option<&str>, e: Option<&str>| {
            msg_socket(f.map(PathBuf::from), e.map(std::ffi::OsString::from), || {
                Some(PathBuf::from(default))
            })
        };
        assert_eq!(pick(Some(flag), Some(env)), Some(PathBuf::from(flag)));
        assert_eq!(pick(None, Some(env)), Some(PathBuf::from(env)));
        assert_eq!(pick(None, None), Some(PathBuf::from(default)));
        assert_eq!(pick(None, Some("")), Some(PathBuf::from(default)), "empty is unset");
    }

    #[test]
    fn subscribe_never_goes_to_another_machine() {
        let words = |w: &str| w.split(' ').map(String::from).collect::<Vec<_>>();
        assert_eq!(msg_request(None, &words("subscribe")), Ok(Request::Subscribe));
        let refused = msg_request(Some("side".into()), &words("subscribe")).unwrap_err();
        assert!(refused.contains("control socket only"), "{refused}");
        assert!(matches!(
            msg_request(Some("side".into()), &words("state")),
            Ok(Request::To { .. })
        ));
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
