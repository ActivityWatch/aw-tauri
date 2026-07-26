fn main() {
    // AW_WEBUI_DIR is consumed at runtime (see lib.rs) to serve the dashboard from
    // disk. Watching the webui tree here is unnecessary and previously caused
    // rebuild loops when a content-hash stamp in OUT_DIR was itself listed as
    // cargo:rerun-if-changed (any concurrent cargo/rust-analyzer run touching
    // that stamp forced a full aw-tauri recompile on the next `make dev`).
    println!("cargo:rerun-if-env-changed=AW_WEBUI_DIR");
    println!("cargo:rerun-if-changed=build.rs");

    tauri_build::build();
}
