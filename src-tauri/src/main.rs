// Prevents additional console window on Windows in release, DO NOT REMOVE!!
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use clap::Parser;

/// ActivityWatch UI built with Tauri
#[derive(Parser, Debug)]
#[command(name = "aw-tauri", version, about)]
struct Cli {
    /// Run in testing mode (port 5666, separate database). Alias for --profile testing.
    #[arg(long)]
    testing: bool,

    /// Run an isolated instance under this profile name (data, config, logs
    /// and lockfile are separate). --testing is an alias for --profile testing.
    #[arg(long)]
    profile: Option<String>,

    /// Enable verbose/debug logging
    #[arg(short, long)]
    verbose: bool,

    /// Override the port number
    #[arg(long)]
    port: Option<u16>,

    /// Run without GUI — no tray icon or windows, suitable for headless servers
    #[arg(long)]
    daemon: bool,

    /// Run the lightweight tray/server mode without the Tauri WebView (~400 MB saved on Linux)
    #[arg(long, conflicts_with = "daemon")]
    mini: bool,
}

fn main() {
    // Before anything spawns threads or initializes GTK/WebKit.
    aw_tauri_lib::apply_webkit_env_workarounds();
    let cli = Cli::parse();
    let profile = match aw_tauri_lib::resolve_profile(cli.profile.as_deref(), cli.testing) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("error: {e}");
            std::process::exit(2);
        }
    };
    // Modules are spawned as subprocesses and inherit the profile from here.
    aw_tauri_lib::export_profile(&profile);
    aw_tauri_lib::set_cli_args(aw_tauri_lib::CliArgs {
        testing: aw_tauri_lib::is_testing(&profile),
        verbose: cli.verbose,
        port: cli.port,
        daemon: cli.daemon,
        mini: cli.mini,
        profile,
    });
    aw_tauri_lib::run();
}
