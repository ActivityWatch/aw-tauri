//! Native download destinations; never fall back to the process working directory.
use std::collections::HashSet;
use std::path::{Path, PathBuf};

#[derive(Default)]
pub struct Destinations {
    // Keep reservations for the window lifetime: completion may have no path (macOS,
    // or failed transfers), and multiple transfers can share a URL. Never release
    // another transfer's reservation based on an ambiguous completion event.
    reserved: HashSet<String>,
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
        loop {
            // Conservatively reserve case variants on every platform, including
            // case-insensitive Windows/macOS Downloads filesystems. No file need
            // exist yet for an earlier transfer to own this filename.
            let key = path.file_name().unwrap().to_string_lossy().to_uppercase();
            // symlink_metadata also detects dangling symlinks. Only NotFound
            // means absence; permission/length errors must cancel the request.
            let exists = match path.symlink_metadata() {
                Ok(_) => true,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
                Err(error) => return Err(error.to_string()),
            };
            if !self.reserved.contains(&key) && !exists {
                self.reserved.insert(key);
                return Ok(path);
            }
            let suffix = format!(" ({index}){extension}");
            // 255 UTF-8 bytes also fit Windows' 255 UTF-16-unit component
            // limit. Trim only the stem, at a character boundary, not the
            // extension or the collision number.
            let budget = 255usize
                .checked_sub(suffix.len())
                .ok_or("Download extension is too long")?;
            let mut end = stem.len().min(budget);
            while !stem.is_char_boundary(end) {
                end -= 1;
            }
            if end == 0 {
                return Err("Download filename has no room for a collision suffix".into());
            }
            path = downloads.join(format!("{}{suffix}", &stem[..end]));
            index = index.checked_add(1).ok_or("Too many existing downloads")?;
        }
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
    fn case_variant_requests_do_not_share_a_destination() {
        let dir = std::env::temp_dir().join(format!("aw-case-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut destinations = Destinations::default();
        let first = destinations
            .destination(&dir, Path::new("export.json"))
            .unwrap();
        let second = destinations
            .destination(&dir, Path::new("EXPORT.json"))
            .unwrap();
        assert_ne!(
            first.to_string_lossy().to_uppercase(),
            second.to_string_lossy().to_uppercase()
        );
        assert!(!first.exists());
        assert!(!second.exists());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn long_collision_names_remain_writable() {
        let dir = std::env::temp_dir().join(format!("aw-long-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut destinations = Destinations::default();
        for stem in ["a".repeat(250), "日".repeat(83)] {
            let name = format!("{stem}.json");
            let first = destinations.destination(&dir, Path::new(&name)).unwrap();
            std::fs::write(&first, "original").unwrap();
            let second = destinations.destination(&dir, Path::new(&name)).unwrap();
            std::fs::write(&second, "copy").unwrap();
            assert_ne!(first, second);
            assert!(second.file_name().unwrap().to_str().unwrap().len() <= 255);
            assert!(second
                .file_name()
                .unwrap()
                .to_str()
                .unwrap()
                .ends_with(" (1).json"));
            assert_eq!(std::fs::read_to_string(first).unwrap(), "original");
        }
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn metadata_errors_do_not_return_a_destination() {
        let dir = std::env::temp_dir().join(format!("aw-metadata-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut destinations = Destinations::default();
        let name = format!("{}.json", "a".repeat(256));
        assert!(destinations.destination(&dir, Path::new(&name)).is_err());
        assert!(destinations.reserved.is_empty());
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
            "bad\nname.json",
            "NUL.json",
            "COM1.csv",
            "trailing.",
            "trailing ",
        ] {
            assert!(
                destinations
                    .destination(&std::env::temp_dir(), Path::new(name))
                    .is_err(),
                "accepted unsafe filename: {name:?}"
            );
        }
        // Backslash is a filename character on Unix, but a directory separator
        // on Windows: file_name() must strip it there, just like a forward slash.
        let dir = std::env::temp_dir().join(format!("aw-unsafe-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let result = destinations.destination(&dir, Path::new("bad\\name.json"));
        #[cfg(not(windows))]
        assert!(result.is_err());
        #[cfg(windows)]
        assert_eq!(result.unwrap(), dir.join("name.json"));
        if dir.exists() {
            std::fs::remove_dir_all(dir).unwrap();
        }
    }
}
