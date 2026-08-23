// Prevents additional console window on Windows in release, DO NOT REMOVE!!
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use clap::Parser;
use handy_app_lib::cli::client::{dispatch, Dispatch};
use handy_app_lib::CliArgs;

fn main() {
    let cli_args = CliArgs::parse();

    // Client mode must be decided before tauri::Builder is touched: the
    // single-instance plugin's setup hook forwards argv to a running app and
    // exits the process, so anything printed after that never happens.
    if cli_args.command.is_some() {
        // A GUI-subsystem release build has no console, so nothing printed
        // below would be visible until this runs.
        #[cfg(windows)]
        handy_app_lib::cli::console_win::attach_parent_console();

        match dispatch(cli_args) {
            Dispatch::Exit(code) => std::process::exit(code),
            // --local, or a fallback: carry on and start the app normally.
            Dispatch::RunApp(args) => return start_app(*args),
        }
    }

    start_app(cli_args)
}

fn start_app(cli_args: CliArgs) {
    #[cfg(target_os = "linux")]
    {
        // DMABUF renderer causes crashes on various GPU/display server configurations
        // See: https://github.com/tauri-apps/tauri/issues/9394
        std::env::set_var("WEBKIT_DISABLE_DMABUF_RENDERER", "1");
    }

    handy_app_lib::run(cli_args)
}
