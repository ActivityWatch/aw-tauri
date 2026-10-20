//! Native download destinations; never fall back to the process working directory.
use std::collections::HashSet;
use std::path::{Path, PathBuf};

#[derive(Default)]
pub struct Destinations {
    // Keep reservations for the window lifetime: completion may have no path (macOS,
    // or failed transfers), and multiple transfers can share a URL. Never release
    // another transfer's reservation based on an ambiguous completion event.
    reserved: HashSet<PathBuf>,
}

impl Destinations {
    pub fn destination(&mut self, downloads: &Path, suggested: &Path) -> Result<PathBuf, String> {
        if !downloads.is_absolute() {
            return Err("Downloads directory must be absolute".into());
        }
        let filename = suggested
            .file_name()
            .and_then(|name| name.to_str())
            .filter(|name| !name.is_empty() && *name != "." && *name != "..")
            .ok_or("Download has no filename")?;
        // A suggestion is a filename, not an arbitrary native path (including Windows ADS).
        if filename
            .chars()
            .any(|c| c.is_control() || matches!(c, '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|'))
        {
            return Err("Download filename contains unsafe characters".into());
        }
        let basename = filename
            .split('.')
            .next()
            .unwrap_or_default()
            .to_ascii_uppercase();
        if filename.ends_with(['.', ' '])
            || matches!(basename.as_str(), "CON" | "PRN" | "AUX" | "NUL")
            || (basename.len() == 4
                && (basename.starts_with("COM") || basename.starts_with("LPT"))
                && matches!(basename.as_bytes()[3], b'1'..=b'9'))
        {
            return Err("Download filename is reserved on Windows".into());
        }
        std::fs::create_dir_all(downloads).map_err(|error| error.to_string())?;
        let mut path = downloads.join(filename);
        let stem = Path::new(filename).file_stem().unwrap().to_string_lossy();
        let extension = Path::new(filename)
            .extension()
            .map(|s| format!(".{}", s.to_string_lossy()))
            .unwrap_or_default();
        let mut index = 1u32;
        // symlink_metadata also detects dangling symlinks; don't replace an existing entry.
        while self.reserved.contains(&path) || path.symlink_metadata().is_ok() {
            path = downloads.join(format!("{stem} ({index}){extension}"));
            index = index.checked_add(1).ok_or("Too many existing downloads")?;
        }
        self.reserved.insert(path.clone());
        Ok(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn creates_downloads_and_preserves_existing_export() {
        let dir = std::env::temp_dir().join(format!("aw-download-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut destinations = Destinations::default();
        let first = destinations
            .destination(&dir, Path::new("/unwanted/cwd/aw-bucket-export.json"))
            .unwrap();
        assert_eq!(first, dir.join("aw-bucket-export.json"));
        std::fs::write(&first, "original").unwrap();
        let second = destinations
            .destination(&dir, Path::new("aw-bucket-export.json"))
            .unwrap();
        assert_eq!(second, dir.join("aw-bucket-export (1).json"));
        assert_eq!(std::fs::read_to_string(first).unwrap(), "original");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn overlapping_requests_do_not_share_a_destination() {
        let dir = std::env::temp_dir().join(format!("aw-overlap-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut destinations = Destinations::default();
        let first = destinations
            .destination(&dir, Path::new("export.json"))
            .unwrap();
        // Neither native transfer has created its file yet.
        let second = destinations
            .destination(&dir, Path::new("export.json"))
            .unwrap();
        assert_ne!(first, second);
        assert!(!first.exists());
        assert!(!second.exists());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn refuses_relative_directory_and_unsafe_names() {
        let mut destinations = Destinations::default();
        assert!(destinations
            .destination(Path::new("relative"), Path::new("export.json"))
            .is_err());
        for name in [
            "..",
            "bad:stream.json",
            "bad\\name.json",
            "bad\nname.json",
            "NUL.json",
            "COM1.csv",
            "trailing.",
            "trailing ",
        ] {
            assert!(destinations
                .destination(&std::env::temp_dir(), Path::new(name))
                .is_err());
        }
    }
}
