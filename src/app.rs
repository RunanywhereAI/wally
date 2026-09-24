//! Shared wally app wiring for the binary and in-process tests (port of
//! src/app.cpp). Owner: the CLI port.

use crate::bootstrap::GlobalOptions;
use crate::cli::App;

/// Registers the whole command tree on `app` (C++ configure_app).
pub fn configure_app(app: &mut App) {
    let _ = app;
    todo!("CLI port: configure_app")
}

/// Run wally with `args` (program name at index 0). Returns the exit code.
pub fn run(args: &[String]) -> i32 {
    let _ = args;
    todo!("CLI port: run")
}

/// Rewrites a command line so a passthrough subcommand's tool arguments survive
/// the parse: a `--` is inserted before the first token that belongs to the
/// wrapped tool. `argv` includes the program name at 0.
pub fn split_passthrough_argv(argv: &[String]) -> Vec<String> {
    let _ = argv;
    todo!("CLI port: SplitPassthroughArgv")
}

/// Builds GlobalOptions from the root command's parse (C++ bound the root
/// options straight into GlobalOptions).
pub fn global_options_from(root: &crate::cli::Parsed) -> GlobalOptions {
    let _ = root;
    todo!("CLI port: root options → GlobalOptions")
}
