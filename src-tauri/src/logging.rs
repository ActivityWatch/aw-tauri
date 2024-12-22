use directories::ProjectDirs;
use fern::colors::{Color, ColoredLevelConfig};
use log::LevelFilter;

pub fn setup_logging() -> Result<(), fern::InitError> {
    let project_dirs =
        ProjectDirs::from("net", "ActivityWatch", "Aw-Tauri").expect("Failed to get project dirs");
    let log_path = project_dirs.data_dir().join("logs");
    std::fs::create_dir_all(&log_path)?;
    let log_file = log_path.join("aw-tauri.log");

    // Configure colors for log levels
    let colors = ColoredLevelConfig::new()
        .error(Color::Red)
        .warn(Color::Yellow)
        .info(Color::Green)
        .debug(Color::Blue)
        .trace(Color::White);

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
        .level(LevelFilter::Info)
        // Set specific log levels for modules
        .level_for("aw_tauri", LevelFilter::Debug)
        .level_for("aw_server", LevelFilter::Info);

    // Configure output to file
    let file = fern::log_file(log_file)?;

    // Build the final dispatcher
    base_config
        .chain(fern::Dispatch::new().level(LevelFilter::Debug).chain(file))
        .chain(
            fern::Dispatch::new()
                .level(LevelFilter::Info)
                .chain(std::io::stdout()),
        )
        .apply()?;

    log::info!("Logging initialized");
    Ok(())
}

// #[allow(dead_code)]
// pub fn get_log_file() -> PathBuf {
//     let project_dirs =
//         ProjectDirs::from("net", "ActivityWatch", "Aw-Tauri").expect("Failed to get project dirs");
//     project_dirs.data_dir().join("logs").join("aw-tauri.log")
// }
