use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Args, Parser, Subcommand};
use orrery_wm::comp::{self, Backend, RunOptions, ipc_server};
use orrery_wm::{config, ipc};

#[derive(Parser)]
#[command(name = "orrery", version, about = "A static tiling Wayland compositor")]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Run the compositor (the default).
    Run(RunArgs),
    /// Send one request to a running orrery.
    Msg {
        /// The control socket. Default: `$XDG_RUNTIME_DIR/orrery.sock`.
        #[arg(long)]
        socket: Option<PathBuf>,
        /// Words (`focus-slot left`) or a single JSON request.
        #[arg(required = true)]
        request: Vec<String>,
    },
    /// Validate a config file: `ok`, or every problem as `FILE:LINE:COL: message`; exit 0 or 1.
    Check {
        /// Default: `$XDG_CONFIG_HOME/orrery/config.kdl`.
        #[arg(long)]
        config: Option<PathBuf>,
    },
}

#[derive(Args)]
struct RunArgs {
    /// `winit` runs as a window inside the current session; `headless` has no screen (tests).
    #[arg(long, value_enum, default_value = "winit")]
    backend: Backend,
    /// Default: `$XDG_CONFIG_HOME/orrery/config.kdl`; the built-in config when it does not exist.
    #[arg(long)]
    config: Option<PathBuf>,
    /// The headless output's size.
    #[arg(long, default_value = "1920x1080", value_parser = comp::parse_size)]
    size: (i32, i32),
    /// The control socket. Default: `$XDG_RUNTIME_DIR/orrery.sock`.
    #[arg(long)]
    socket: Option<PathBuf>,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    // No verb means `run` with clap's own defaults, so they are written once (in the attributes above).
    let command = cli
        .command
        .unwrap_or_else(|| Cli::parse_from(["orrery", "run"]).command.expect("`run` parses"));
    match command {
        Command::Run(args) => {
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
                    eprintln!("orrery: {e:#}");
                    ExitCode::FAILURE
                }
            }
        }
        Command::Msg { socket, request } => msg(socket, &request),
        Command::Check { config } => check(config),
    }
}

/// `orrery check`. Unlike `run`, a missing file is not ok: the point is to check a file.
fn check(path: Option<PathBuf>) -> ExitCode {
    let Some(path) = path.or_else(comp::default_config_path) else {
        eprintln!("orrery check: no config path: pass --config or set XDG_CONFIG_HOME or HOME");
        return ExitCode::FAILURE;
    };
    let src = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            eprintln!(
                "{}: no such file (orrery would use the built-in config)",
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

/// `orrery msg`: the answer as JSON on stdout (pretty when it carries the state), the error also on stderr;
/// exit 0 only when orrery said ok.
fn msg(socket: Option<PathBuf>, words: &[String]) -> ExitCode {
    let request = match ipc::parse_words(words) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("orrery msg: {e}");
            return ExitCode::FAILURE;
        }
    };
    let Some(path) = socket.or_else(ipc_server::default_socket_path) else {
        eprintln!("orrery msg: no control socket: pass --socket or set XDG_RUNTIME_DIR");
        return ExitCode::FAILURE;
    };
    let response = match ipc_server::send(&path, &request) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("orrery msg: {e:#}");
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
        Err(e) => eprintln!("orrery msg: {e}"),
    }
    if let Some(e) = &response.error {
        eprintln!("orrery: {e}");
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
        let Some(Command::Run(args)) = Cli::parse_from(["orrery", "run"]).command else {
            panic!("run parses");
        };
        assert_eq!(args.backend, Backend::Winit);
        assert_eq!(args.size, (1920, 1080));
        assert!(args.config.is_none() && args.socket.is_none());
    }

    #[test]
    fn msg_takes_options_before_or_after_the_words() {
        let Some(Command::Msg { request, socket }) =
            Cli::parse_from(["orrery", "msg", "--socket", "/s", "layout", "wall"]).command
        else {
            panic!("msg parses");
        };
        assert_eq!(request, ["layout", "wall"]);
        assert_eq!(socket, Some(PathBuf::from("/s")));
        let Some(Command::Msg { request, socket }) =
            Cli::parse_from(["orrery", "msg", "focus-slot", "left", "--socket", "/t"]).command
        else {
            panic!("msg parses");
        };
        assert_eq!(request, ["focus-slot", "left"]);
        assert_eq!(socket, Some(PathBuf::from("/t")));
    }
}
