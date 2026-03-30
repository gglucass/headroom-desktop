fn main() {
    println!("cargo:rerun-if-env-changed=HEADROOM_UPDATER_PUBLIC_KEY");
    println!("cargo:rerun-if-env-changed=HEADROOM_UPDATER_ENDPOINTS");
    tauri_build::build()
}
