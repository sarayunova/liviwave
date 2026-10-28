//! `liviwave-cli`: LiVi Wave without a window. See the library crate for the commands.

#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

fn main() {
    // `args_os`, not `args`: a non-Unicode argument must reach clap, not panic before anything is printed.
    let code = liviwave_cli::run(std::env::args_os().skip(1), &mut std::io::stdout(), &mut std::io::stderr());
    // The subcommand's engine and its plugin instances are gone by now. This is process teardown,
    // so it lives here and not in `run`, which tests call many times in one process.
    liviwave_engine::shutdown_plugin_hosts();
    std::process::exit(code);
}
