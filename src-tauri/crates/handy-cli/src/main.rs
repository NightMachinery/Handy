//! `handy` — the command-line front door.
//!
//! A console-subsystem binary, separate from the app. That separation buys
//! three things a second `[[bin]]` in the app's crate could not: no console
//! attachment hack on Windows, a bare invocation that can print help instead of
//! launching a GUI, and a small binary — the app links transcribe-cpp and ONNX
//! Runtime through build scripts, so sharing its crate would drag the whole
//! inference stack in whether it is called or not.
//!
//! Anything that is really an app invocation is handed to the app binary
//! untouched, so every documented flag keeps working through this one command.

mod args;
mod client;

use args::{is_app_invocation, Cli};
use clap::Parser;
use std::ffi::OsString;
use std::path::PathBuf;

fn main() {
    let argv: Vec<OsString> = std::env::args_os().skip(1).collect();

    // With no arguments at all, print help. The app binary cannot do this —
    // macOS launches a bundle with an empty argv, so for it "no arguments"
    // has to mean "start the GUI".
    if argv.is_empty() {
        let _ = <Cli as clap::CommandFactory>::command().print_help();
        println!();
        std::process::exit(0);
    }

    // App flags are not modelled here; forward the command line verbatim so
    // there is only ever one definition of them.
    if is_app_invocation(&argv) {
        exec_app(&argv);
    }

    let cli = Cli::parse();
    match client::dispatch(cli) {
        client::Dispatch::Exit(code) => std::process::exit(code),
        client::Dispatch::RunLocal(local_argv) => exec_app(&local_argv),
    }
}

/// Replace this process with the app binary.
///
/// `exec` rather than spawn-and-wait so the app inherits the terminal and the
/// exit status directly, and no extra process sits in the tree.
fn exec_app(argv: &[OsString]) -> ! {
    let app = match locate_app_binary() {
        Some(path) => path,
        None => {
            eprintln!(
                "handy: cannot find the Handy application binary.\n  \
                 set HANDY_APP_BIN to its path, e.g. \
                 /Applications/Handy.app/Contents/MacOS/handy"
            );
            std::process::exit(2);
        }
    };

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        let err = std::process::Command::new(&app).args(argv).exec();
        eprintln!("handy: could not run {}: {err}", app.display());
        std::process::exit(1);
    }
    #[cfg(not(unix))]
    {
        match std::process::Command::new(&app).args(argv).status() {
            Ok(status) => std::process::exit(status.code().unwrap_or(1)),
            Err(err) => {
                eprintln!("handy: could not run {}: {err}", app.display());
                std::process::exit(1);
            }
        }
    }
}

/// Find the app binary that pairs with this CLI.
fn locate_app_binary() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("HANDY_APP_BIN") {
        let path = PathBuf::from(path);
        if path.is_file() {
            return Some(path);
        }
    }

    // Installed layout: both binaries sit in Handy.app/Contents/MacOS/.
    if let Ok(exe) = std::env::current_exe() {
        // Resolve symlinks first — this is normally reached through ~/bin/handy.
        let exe = std::fs::canonicalize(&exe).unwrap_or(exe);
        if let Some(dir) = exe.parent() {
            let sibling = dir.join(app_binary_name());
            if sibling.is_file() && sibling != exe {
                return Some(sibling);
            }
            // Cargo layout: target/<profile>/handy next to target/<profile>/handy-cli.
            let up = dir.join("..").join(app_binary_name());
            if up.is_file() {
                return Some(up);
            }
        }
    }

    #[cfg(target_os = "macos")]
    {
        let installed = PathBuf::from("/Applications/Handy.app/Contents/MacOS/handy");
        if installed.is_file() {
            return Some(installed);
        }
    }

    None
}

fn app_binary_name() -> &'static str {
    if cfg!(windows) {
        "handy.exe"
    } else {
        "handy"
    }
}
