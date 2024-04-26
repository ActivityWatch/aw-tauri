fn main() {
  let webui_var = std::env::var("AW_WEBUI_DIR");
  
  if let Err(_) = webui_var {
    panic!("AW_WEBUI_DIR environment variable not set, Try running make");
  }
  tauri_build::build();
}
