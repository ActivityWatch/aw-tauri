use fern::colors::{Color, ColoredLevelConfig};
use log::LevelFilter;
use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

const MAX_LOG_SIZE: u64 = 32 * 1024 * 1024; // 32MB
const MAX_ROTATED_LOGS: usize = 5; // Keep last 5 rotated logs

/// Rotate log file if it exceeds MAX_LOG_SIZE
pub fn rotate_log_if_needed() -> Result<(), std::io::Error> {
    rotate_if_larger_than(&get_log_path(), MAX_LOG_SIZE)
}

fn rotate_if_larger_than(log_path: &Path, max_size: u64) -> Result<(), std::io::Error> {
    // Check if log file exists and get its size
    if !log_path.exists() {
        return Ok(());
    }

    let metadata = fs::metadata(log_path)?;
    let file_size = metadata.len();

    // Only rotate if file exceeds the size limit
    if file_size <= max_size {
        return Ok(());
    }

    // Create rotated filename with timestamp
    let timestamp = chrono::Local::now().format("%Y-%m-%d_%H-%M-%S");
    let log_dir = log_path.parent().expect("Failed to get log dir");
    let log_name = log_path.file_stem().expect("Failed to get log filename");
    let rotated_name = format!("{}.{}.log", log_name.to_string_lossy(), timestamp);
    let rotated_path = log_dir.join(rotated_name);

    // Rename current log file
    fs::rename(log_path, &rotated_path)?;

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
    let file: Box<dyn Write + Send> = Box::new(RotatingFile::open(log_path, MAX_LOG_SIZE)?);

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

/// Log file that rotates itself once it passes `max_size`.
///
/// Rotating only at startup isn't enough: aw-tauri usually runs for weeks, so the log could grow
/// far past the limit in between. fern flushes after every record, so rotating in `flush` never
/// splits a line across files.
struct RotatingFile {
    path: PathBuf,
    max_size: u64,
    /// `None` only if reopening after a rotation failed; the next write retries.
    file: Option<File>,
    size: u64,
}

impl RotatingFile {
    fn open(path: PathBuf, max_size: u64) -> io::Result<Self> {
        let file = fern::log_file(&path)?;
        let size = file.metadata()?.len();
        Ok(Self {
            path,
            max_size,
            file: Some(file),
            size,
        })
    }

    fn file(&mut self) -> io::Result<&mut File> {
        if self.file.is_none() {
            let file = fern::log_file(&self.path)?;
            self.size = file.metadata()?.len();
            self.file = Some(file);
        }
        Ok(self.file.as_mut().expect("log file was just opened"))
    }

    fn rotate(&mut self) -> io::Result<()> {
        // Close the file first: Windows can't rename a file that is still open.
        self.file = None;
        let rotated = rotate_if_larger_than(&self.path, self.max_size);
        self.file()?;
        rotated
    }
}

impl Write for RotatingFile {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let written = self.file()?.write(buf)?;
        self.size += written as u64;
        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.file()?.flush()?;
        if self.size > self.max_size {
            self.rotate()?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rotating_file_rotates_once_past_max_size() {
        let dir = std::env::temp_dir().join(format!("aw-tauri-log-test-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("aw-tauri.log");

        let mut file = RotatingFile::open(path.clone(), 100).unwrap();
        file.write_all(&[b'a'; 60]).unwrap();
        file.flush().unwrap();
        file.write_all(&[b'b'; 60]).unwrap();
        file.flush().unwrap();
        file.write_all(b"after").unwrap();
        file.flush().unwrap();

        let rotated: Vec<_> = fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.path() != path)
            .collect();
        assert_eq!(rotated.len(), 1);
        assert_eq!(fs::metadata(rotated[0].path()).unwrap().len(), 120);
        assert_eq!(fs::read(&path).unwrap(), b"after");

        fs::remove_dir_all(&dir).unwrap();
    }
}
