//! codex-imagegen: an MCP server that lets Claude Code generate and refine images through a
//! local Codex CLI. The design lives in docs/design.md.

// Windows-only by design: the server reaps the Codex process tree with a job object and locks
// its session store with LockFileEx. Said here so a build on another host fails with the reason
// rather than a pile of unrelated-looking errors.
#[cfg(not(windows))]
compile_error!(
    "codex-imagegen targets Windows only: it depends on job objects and LockFileEx file locking."
);

const VERSION: &str = env!("CARGO_PKG_VERSION");

/// The commit this binary was built from. CI sets `CODEX_IMAGEGEN_BUILD`; a local build has none.
const BUILD: Option<&str> = option_env!("CODEX_IMAGEGEN_BUILD");

const USAGE: &str = "\
codex-imagegen - MCP server that lets Claude Code generate images through a local Codex CLI

USAGE:
    codex-imagegen [--help | --version]

The MCP server is not implemented yet; docs/design.md describes the plan.
";

fn version_line() -> String {
    match BUILD {
        Some(build) => format!("codex-imagegen {VERSION} ({build})"),
        None => format!("codex-imagegen {VERSION} (local build)"),
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();

    if args.iter().any(|a| a == "--help" || a == "-h") {
        print!("{USAGE}");
        return;
    }
    if args.iter().any(|a| a == "--version" || a == "-V") {
        println!("{}", version_line());
        return;
    }
    if let Some(arg) = args.first() {
        eprintln!("codex-imagegen: unknown argument '{arg}'\n");
        eprintln!("Run with --help for usage.");
        std::process::exit(2);
    }

    eprintln!("codex-imagegen: the MCP server is not implemented yet.");
    std::process::exit(1);
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
