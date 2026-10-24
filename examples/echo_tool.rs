//! The smallest tool on the daemon: prints its arguments and who reads them.
//!
//! ```sh
//! cargo run --example echo_tool -- hello      # starts a daemon, runs there
//! AXECHO_DAEMON=off cargo run --example echo_tool -- hello
//! cargo run --example echo_tool -- daemon status
//! cargo run --example echo_tool -- daemon stop
//! ```

use axkit::daemon::{self, Cancel, Config, Handler, Io, Outcome, Request};
use axkit::exit::Exit;
use std::process::ExitCode;

struct Echo;

impl Handler for Echo {
    fn run(&self, request: &Request, io: &mut Io<'_>) -> Outcome {
        let args: Vec<String> = request.args.iter().skip(1).map(|a| a.to_string_lossy().into_owned()).collect();
        let _ = writeln!(io.out, "{}", args.join(" "));
        io.progress("# progress: echoed");
        let _ = writeln!(io.err, "mode {:?}", request.mode());
        Outcome::Exit(Exit::Error)
    }
}

fn main() -> ExitCode {
    let config = Config::new("axecho", "0");
    let args: Vec<String> = std::env::args().skip(1).collect();
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    match args[..] {
        ["daemon", "serve"] => {
            return match daemon::serve(&config, &Echo) {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => {
                    eprintln!("Error: {e}");
                    Exit::Error.into()
                }
            };
        }
        ["daemon", "status"] => {
            daemon::status(&config).unwrap_or_default().iter().for_each(|s| println!("{s}"));
            return ExitCode::SUCCESS;
        }
        ["daemon", "stop"] => {
            println!("stopped {}", daemon::stop(&config, true).unwrap_or(0));
            return ExitCode::SUCCESS;
        }
        _ => {}
    }
    if let Some(code) = daemon::forward(&config) {
        return ExitCode::from(code);
    }
    let request = match Request::capture(&config) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("Error: {e}");
            return Exit::Error.into();
        }
    };
    let (stdout, stderr) = (std::io::stdout(), std::io::stderr());
    let (mut out, mut err) = (stdout.lock(), stderr.lock());
    match Echo.run(&request, &mut Io::new(&mut out, &mut err, Cancel::default())) {
        Outcome::Exit(exit) => exit.into(),
        Outcome::Decline(_) => Exit::Error.into(),
    }
}
