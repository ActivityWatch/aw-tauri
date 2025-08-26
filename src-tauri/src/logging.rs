#[allow(unused_imports)]
use directories::{ProjectDirs, UserDirs};
use fern::colors::{Color, ColoredLevelConfig};
use log::LevelFilter;
use std::path::PathBuf;

/// Set up logging configuration - only capture log calls, suppress all other output
pub fn setup_logging() -> Result<(), fern::InitError> {
    // Check environment variables for verbose logging
    let aw_trace = std::env::var("AW_TRACE").is_ok();
    let aw_debug = std::env::var("AW_DEBUG").is_ok();

    let log_path = get_log_path();
    let log_dir = log_path.parent().expect("Failed to get log dir");
    std::fs::create_dir_all(log_dir)?;

    // Configure colors for log levels
    let colors = ColoredLevelConfig::new()
        .error(Color::Red)
        .warn(Color::Yellow)
        .info(Color::Green)
        .debug(Color::Blue)
        .trace(Color::BrightBlue);

    // Determine log levels based on environment variables
    let logging_level = if aw_trace {
        LevelFilter::Trace
    } else if aw_debug {
        LevelFilter::Debug
    } else {
        LevelFilter::Info
    };

    let module_logging_level = if aw_debug || aw_trace {
        LevelFilter::Info
    } else {
        LevelFilter::Error
    };

    // Base configuration
    let base_config = fern::Dispatch::new()
        .format(move |out, message, record| {
            out.finish(format_args!(
                "[{timestamp}][{level}][{target}] {message}",
                timestamp = chrono::Local::now().format("%Y-%m-%d %H:%M:%S"),
                level = colors.color(record.level()),
                target = record.target(),
                message = message,
            ))
        })
        .level(module_logging_level) // Default level based on environment variables
        // Set specific log levels for modules - only show our own code
        .level_for("aw_tauri", LevelFilter::Debug)
        .level_for("aw_tauri_lib", logging_level);

    // Configure output to file
    let file = fern::log_file(log_path)?;

    // Build the final dispatcher
    base_config
        .chain(fern::Dispatch::new().level(LevelFilter::Info).chain(file))
        .apply()?;

    log::info!("Logging initialized");
    Ok(())
}

pub fn get_log_path() -> PathBuf {
    #[cfg(target_os = "windows")]
    {
        // Windows: C:\Users\<USER>\AppData\Local\activitywatch\activitywatch\Logs\aw-tauri
        let user_dirs = UserDirs::new().expect("Failed to get user directories");
        let home_dir = user_dirs.home_dir();
        home_dir
            .join("AppData")
            .join("Local")
            .join("activitywatch")
            .join("activitywatch")
            .join("Logs")
            .join("aw-tauri")
            .join("aw-tauri.log")
    }
    #[cfg(target_os = "macos")]
    {
        // macOS: ~/Library/Logs/activitywatch/aw-tauri
        let user_dirs = UserDirs::new().expect("Failed to get user directories");
        let home_dir = user_dirs.home_dir();
        home_dir
            .join("Library")
            .join("Logs")
            .join("activitywatch")
            .join("aw-tauri")
            .join("aw-tauri.log")
    }
    #[cfg(target_os = "linux")]
    {
        // Linux: ~/.cache/activitywatch/logs/aw-tauri/
        let user_dirs = UserDirs::new().expect("Failed to get user directories");
        let home_dir = user_dirs.home_dir();
        home_dir
            .join(".cache")
            .join("activitywatch")
            .join("logs")
            .join("aw-tauri")
            .join("aw-tauri.log")
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos", target_os = "linux")))]
    {
        // Fallback for other platforms
        let project_dirs = ProjectDirs::from("net", "ActivityWatch", "Aw-Tauri")
            .expect("Failed to get project dirs");
        project_dirs.data_dir().join("logs").join("aw-tauri.log")
    }
}
