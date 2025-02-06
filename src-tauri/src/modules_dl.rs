/// Downloads essential modules such as the window and afk watchers
/// Module metadata is stored in a csv file that is downloaded
/// the fields appear in the order below
/// name,os,display_server,version,arch,release_date,link
///
/// More fields can be added as long as it maintains backward compatibility
use crate::get_config;
use csv::ReaderBuilder;
use log::error;
use std::{fs::File, io::Write, vec};
use tauri_plugin_http::reqwest;

fn is_wayland() -> bool {
    std::env::var("XDG_SESSION_TYPE").unwrap_or_default() == "wayland"
}

async fn download_module(url: &str) -> Result<(), Box<dyn std::error::Error>> {
    let mut response = reqwest::get(url).await?;
    let file_name = url.split('/').last().unwrap();
    let file_path = get_config().defaults.discovery_path.clone().join(file_name);
    let mut file = File::create(file_path.clone())?;
    while let Some(chunk) = response.chunk().await? {
        file.write_all(&chunk)?;
    }
    if file_name.ends_with(".zip") {
        let output = std::process::Command::new("unzip")
            .arg(&file_path)
            .arg("-d")
            .arg(get_config().defaults.discovery_path.clone())
            .output()?;
        error!("{}", String::from_utf8_lossy(&output.stdout));
    } else if file_name.ends_with(".tar") || file_name.ends_with(".tar.gz") {
        let output = std::process::Command::new("tar")
            .arg("-xvf")
            .arg(&file_path)
            .arg("-C")
            .arg(get_config().defaults.discovery_path.clone())
            .output()?;
        error!("{}", String::from_utf8_lossy(&output.stdout));
    }
    Ok(())
}

async fn fetch_releases_file() -> Result<String, Box<dyn std::error::Error>> {
    // TODO: use a better source
    let url = "https://gist.githubusercontent.com/0xbrayo/f7b25a2ff9ed24ce21fa8397837265b6/raw/120ddb3d31d7f009d66f070bd4a0dc06d3c0aacf/aw-releases.csv";
    let response = reqwest::get(url).await?;
    let body = response.text().await?;
    Ok(body)
}

pub(crate) async fn download_modules() -> Result<(), Box<dyn std::error::Error>> {
    let releases = fetch_releases_file().await?;
    let mut reader = ReaderBuilder::new().from_reader(releases.as_bytes());

    if cfg!(target_os = "linux") {
        let display_server = if is_wayland() { "wayland" } else { "x11" };
        for row in reader.records() {
            let row = row.expect("Malformed releases file");
            if &row[1] != "linux" {
                continue;
            }
            if !row[2].is_empty() && &row[2] != display_server {
                continue;
            }
            let url = &row[6];
            download_module(url).await?;
        }
    } else if cfg!(target_os = "windows") {
        for row in reader.records() {
            let row = row.expect("Malformed releases file");
            if &row[1] != "windows" {
                continue;
            }
            let url = &row[6];
            download_module(url).await?;
        }
    } else if cfg!(target_os = "macos") {
        for row in reader.records() {
            let row = row.expect("Malformed releases file");
            if &row[2] != "macos" {
                continue;
            }
            let url = &row[6];
            download_module(url).await?;
        }
    } else {
        // should be unreachable
        panic!("Unsupported OS");
    }
    Ok(())
}

#[cfg(target_os = "linux")]
pub(crate) fn has_essential_modules(modules: Vec<String>) -> bool {
    let essential_modules = if is_wayland() {
        vec!["aw-awatcher".to_string()]
    } else {
        vec![
            "aw-watcher-afk".to_string(),
            "aw-watcher-window".to_string(),
        ]
    };

    for module in essential_modules {
        if !modules.iter().any(|m| m == &module) {
            return false;
        }
    }
    true
}

#[cfg(not(any(target_os = "linux")))]
pub(crate) fn has_essential_modules(modules: Vec<String>) -> bool {
    let essential_modules = vec![
        "aw-watcher-afk".to_string(),
        "aw-watcher-window".to_string(),
    ];

    for module in essential_modules {
        if !modules.iter().any(|m| m == &module) {
            return false;
        }
    }
    true
}
