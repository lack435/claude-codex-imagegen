//! codex-imagegen: an MCP server that lets Claude Code generate and refine images through a
//! local Codex CLI. The design lives in docs/design.md.

// Windows-only by design: the server reaps the Codex process tree with a job object and locks
// its session store with LockFileEx. Said here so a build on another host fails with the reason
// rather than a pile of unrelated-looking errors.
#[cfg(not(windows))]
compile_error!(
    "codex-imagegen targets Windows only: it depends on job objects and LockFileEx file locking."
);

mod appserver;
mod cancel;
mod cleanup;
mod codex;
mod config;
mod delete;
mod errors;
mod jsonrpc;
mod mcp;
mod output;
mod preview;
mod registry;
mod session;
#[cfg(test)]
mod testutil;
mod tools;
mod turn;
mod winjob;

use std::sync::Arc;

use config::{Config, Mode, USAGE};

const VERSION: &str = env!("CARGO_PKG_VERSION");

/// The commit this binary was built from. CI sets `CODEX_IMAGEGEN_BUILD`; a local build has none.
const BUILD: Option<&str> = option_env!("CODEX_IMAGEGEN_BUILD");

fn version_line() -> String {
    match BUILD {
        Some(build) => format!("codex-imagegen {VERSION} ({build})"),
        None => format!("codex-imagegen {VERSION} (local build)"),
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();

    // Before parsing, so help and the version are available even alongside a bad flag.
    if args.iter().any(|a| a == "--help" || a == "-h") {
        print!("{USAGE}");
        return;
    }
    if args.iter().any(|a| a == "--version" || a == "-V") {
        println!("{}", version_line());
        return;
    }

    let cfg = match Config::from_args(&args) {
        Ok(cfg) => cfg,
        Err(message) => {
            eprintln!("codex-imagegen: {message}\n");
            eprintln!("Run with --help for usage.");
            std::process::exit(2);
        }
    };

    if let Err(e) = std::fs::create_dir_all(&cfg.state_dir) {
        // Not fatal here: whatever needs the directory reports its own error when it runs.
        eprintln!(
            "codex-imagegen: warning: could not create the state directory {}: {e}",
            cfg.state_dir.display()
        );
    }

    match cfg.mode {
        Mode::Help => print!("{USAGE}"),
        Mode::Version => println!("{}", version_line()),
        Mode::Doctor => {
            // The same report as the status tool, from a terminal. Free: no image is generated.
            // Exits 1 when Codex is not ready, so a script can tell.
            let (report, ready) = tools::App::new(cfg).doctor();
            print!("{report}");
            std::process::exit(if ready { 0 } else { 1 });
        }
        Mode::Cleanup { older_than_days } => {
            // From a terminal, no Claude needed: every project's expired sessions. Prints what it
            // removed and skipped; exits 1 when a store could not be read or Codex not started.
            let code = cleanup::sweep(
                &cfg,
                &tools::CodexLauncher,
                older_than_days,
                &mut std::io::stdout(),
            );
            std::process::exit(code);
        }
        Mode::Serve => mcp::serve(Arc::new(tools::App::serving(cfg))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_line_names_the_binary_version_and_build() {
        let line = version_line();
        assert!(
            line.starts_with(&format!("codex-imagegen {VERSION} (")),
            "{line}"
        );
        assert!(line.ends_with(')'), "{line}");
    }
}
