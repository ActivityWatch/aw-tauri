use fern::colors::{Color, ColoredLevelConfig};
use log::LevelFilter;
use std::fs;
use std::path::PathBuf;

const MAX_LOG_SIZE: u64 = 32 * 1024 * 1024; // 32MB
const MAX_ROTATED_LOGS: usize = 5; // Keep last 5 rotated logs

/// Rotate log file if it exceeds MAX_LOG_SIZE
pub fn rotate_log_if_needed() -> Result<(), std::io::Error> {
    let log_path = get_log_path();

    // Check if log file exists and get its size
    if !log_path.exists() {
        return Ok(());
    }

    let metadata = fs::metadata(&log_path)?;
    let file_size = metadata.len();

    // Only rotate if file exceeds MAX_LOG_SIZE
    if file_size <= MAX_LOG_SIZE {
        return Ok(());
    }

    // Create rotated filename with timestamp
    let timestamp = chrono::Local::now().format("%Y-%m-%d_%H-%M-%S");
    let log_dir = log_path.parent().expect("Failed to get log dir");
    let log_name = log_path.file_stem().expect("Failed to get log filename");
    let rotated_name = format!("{}.{}.log", log_name.to_string_lossy(), timestamp);
    let rotated_path = log_dir.join(rotated_name);

    // Rename current log file
    fs::rename(&log_path, &rotated_path)?;

    // Clean up old rotated logs, keeping only MAX_ROTATED_LOGS most recent
    cleanup_old_logs(log_dir, log_name.to_string_lossy().as_ref())?;

    Ok(())
}

/// Remove old rotated logs, keeping only the most recent MAX_ROTATED_LOGS
fn cleanup_old_logs(log_dir: &std::path::Path, log_name: &str) -> Result<(), std::io::Error> {
    let mut rotated_logs: Vec<_> = fs::read_dir(log_dir)?
        .filter_map(|entry| entry.ok())
        .filter(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .starts_with(&format!("{}.", log_name))
                && entry.file_name().to_string_lossy().ends_with(".log")
                && entry.file_name().to_string_lossy() != format!("{}.log", log_name)
        })
        .collect();

    // Sort by modification time (newest first)
    rotated_logs.sort_by_key(|entry| {
        entry
            .metadata()
            .and_then(|m| m.modified())
            .unwrap_or(std::time::SystemTime::UNIX_EPOCH)
    });
    rotated_logs.reverse();

    // Remove logs beyond MAX_ROTATED_LOGS
    for log_to_remove in rotated_logs.iter().skip(MAX_ROTATED_LOGS) {
        fs::remove_file(log_to_remove.path())?;
    }

    Ok(())
}

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
    crate::dirs::get_log_path()
}
