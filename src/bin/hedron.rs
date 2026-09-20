//! Product CLI: `hedron import`, `hedron hql`, `hedron jev-intent`, `hedron mcp`.
//! No daemon, no listen port.

use std::env;
use std::process;

use hedron_core::{hql, import, jev, mcp};

const ROOT_HELP: &str = "\
HedronDB product CLI.

Usage:
  hedron <COMMAND> [OPTIONS]

Commands:
  import      Load a markdown tree into a HedronDB store
  hql         Run a read-only HQL v0 pipeline
  jev-intent  Named-ask Jev gate: intent vs evidence (shadow)
  mcp         MCP stdio server (`jev_intent`)

Options:
  -h, --help  Print help

`hedron-import` is a thin alias of `hedron import`.
Python `python/hql` is a result-twin of `hedron hql`.
Jev is named asks only — no grind clocks.
";

fn main() {
    if let Err(err) = run(env::args().skip(1).collect()) {
        eprintln!("{err}");
        process::exit(1);
    }
}

fn run(mut args: Vec<String>) -> Result<(), String> {
    if args.is_empty() {
        print!("{ROOT_HELP}");
        return Ok(());
    }
    let cmd = args.remove(0);
    match cmd.as_str() {
        "-h" | "--help" | "help" => {
            print!("{ROOT_HELP}");
            Ok(())
        }
        "import" => import::run_cli(args),
        "hql" => hql::run_cli(args),
        "jev-intent" => jev::run_cli(args),
        "mcp" => mcp::run_cli(args),
        other => Err(format!(
            "unknown command {other:?}\n\nRun `hedron --help` for usage."
        )),
    }
}
