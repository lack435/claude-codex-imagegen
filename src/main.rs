//! codex-imagegen: an MCP server that lets Claude Code generate and refine images through a
//! local Codex CLI. The design lives in docs/design.md.

// Much of the foundation below (the job object, the Codex failure constructors, the app-server
// side of the framing) has no caller until the Codex client and tool layer land later in this
// milestone. Remove this allowance with that change, so dead code is reported again.
#![allow(dead_code)]

// Windows-only by design: the server reaps the Codex process tree with a job object and locks
// its session store with LockFileEx. Said here so a build on another host fails with the reason
// rather than a pile of unrelated-looking errors.
#[cfg(not(windows))]
compile_error!(
    "codex-imagegen targets Windows only: it depends on job objects and LockFileEx file locking."
);

mod cancel;
mod config;
mod errors;
mod jsonrpc;
mod mcp;
#[cfg(test)]
mod testutil;
mod winjob;

use std::sync::Arc;

use config::{Config, Mode, USAGE};
use serde_json::Value;

const VERSION: &str = env!("CARGO_PKG_VERSION");

/// The commit this binary was built from. CI sets `CODEX_IMAGEGEN_BUILD`; a local build has none.
const BUILD: Option<&str> = option_env!("CODEX_IMAGEGEN_BUILD");

fn version_line() -> String {
    match BUILD {
        Some(build) => format!("codex-imagegen {VERSION} ({build})"),
        None => format!("codex-imagegen {VERSION} (local build)"),
    }
}

/// Stands in for the tool layer until it exists: no tools, and a clear refusal if one is called
/// anyway.
struct NoTools;

impl mcp::ToolHost for NoTools {
    fn instructions(&self) -> String {
        "This build of codex-imagegen offers no tools yet.".to_string()
    }

    fn tool_definitions(&self) -> Vec<Value> {
        Vec::new()
    }

    fn call_tool(&self, _name: &str, _args: &Value, _ctx: &mcp::CallContext) -> Value {
        mcp::failure_result(&errors::not_implemented_yet())
    }

    fn begin_shutdown(&self) {}
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
            println!("{}", version_line());
            println!("The Codex status checks are not implemented in this build yet.");
            std::process::exit(1);
        }
        Mode::Cleanup { .. } => {
            eprintln!("codex-imagegen: --cleanup is not implemented yet.");
            std::process::exit(1);
        }
        Mode::Serve => mcp::serve(Arc::new(NoTools)),
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
