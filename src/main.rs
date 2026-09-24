//! wally — RunAnywhere desktop CLI entry point.
//!
//! Nothing but dispatch. Quieting the SDK lives in `run_main()`, which is the
//! entry both this binary and the Swift MLX host go through.

fn main() {
    std::process::exit(wally::run_main(std::env::args_os().collect()));
}
