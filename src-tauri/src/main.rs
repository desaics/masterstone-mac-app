// Prevents an extra console window on Windows release builds. No effect on macOS.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    masterstone_lib::run()
}
