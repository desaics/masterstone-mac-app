// Masterstone CRM — Tauri runtime entry.
// Session 1 scope: open the main window with the bundled HTML. No commands wired yet.
// SQLite, IPC commands, and storage layer arrive in Session 2+.

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .setup(|_app| Ok(()))
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
