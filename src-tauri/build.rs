use std::fs;
use std::path::{Path, PathBuf};

struct Fnv1a {
    hash: u64,
}

impl Fnv1a {
    fn new() -> Self {
        Self {
            hash: 0xcbf29ce484222325,
        }
    }

    fn write(&mut self, bytes: &[u8]) {
        for &byte in bytes {
            self.hash ^= byte as u64;
            self.hash = self.hash.wrapping_mul(0x100000001b3);
        }
    }

    fn finish(&self) -> u64 {
        self.hash
    }
}

fn visit_dirs(dir: &Path, files: &mut Vec<PathBuf>) -> std::io::Result<()> {
    if dir.is_dir() {
        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.is_dir() {
                visit_dirs(&path, files)?;
            } else {
                if let Some(name) = path.file_name() {
                    if name != ".DS_Store" {
                        files.push(path);
                    }
                }
            }
        }
    }
    Ok(())
}

fn main() {
    let webui_var = std::env::var("AW_WEBUI_DIR");
    if let Ok(webui_dir) = webui_var {
        let webui_path = Path::new(&webui_dir);
        if webui_path.exists() {
            let mut files = Vec::new();
            if visit_dirs(webui_path, &mut files).is_ok() {
                files.sort();

                let mut hasher = Fnv1a::new();
                for file_path in &files {
                    // Hash relative path
                    let rel_path = file_path.strip_prefix(webui_path).unwrap_or(file_path);
                    hasher.write(rel_path.to_string_lossy().as_bytes());

                    // Hash content
                    if let Ok(content) = fs::read(file_path) {
                        hasher.write(&content);
                    }

                    // Tell Cargo to re-run build.rs if any individual file changes
                    println!("cargo:rerun-if-changed={}", file_path.display());
                }

                let current_hash = hasher.finish();

                // Save or compare hash in OUT_DIR
                if let Ok(out_dir) = std::env::var("OUT_DIR") {
                    let hash_file_path = Path::new(&out_dir).join("webui_hash.txt");
                    let mut hash_changed = true;
                    if hash_file_path.exists() {
                        if let Ok(old_hash_str) = fs::read_to_string(&hash_file_path) {
                            if let Ok(old_hash) = old_hash_str.trim().parse::<u64>() {
                                if old_hash == current_hash {
                                    hash_changed = false;
                                }
                            }
                        }
                    }

                    if hash_changed {
                        let _ = fs::write(&hash_file_path, current_hash.to_string());
                    }

                    // Tell Cargo to watch the hash file
                    println!("cargo:rerun-if-changed={}", hash_file_path.display());
                }
            }
        }
    }

    println!("cargo:rerun-if-env-changed=AW_WEBUI_DIR");
    println!("cargo:rerun-if-changed=build.rs");

    tauri_build::build();
}
