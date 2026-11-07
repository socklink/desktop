/// Restart the app after the signed updater plugin has installed an update.
///
/// Updates are downloaded and verified by `tauri-plugin-updater` (minisign signature
/// against the public key in `tauri.conf.json`). There is deliberately no command that
/// installs an arbitrary package URL.
#[tauri::command]
pub fn relaunch_app(app: tauri::AppHandle) {
    app.restart();
}
