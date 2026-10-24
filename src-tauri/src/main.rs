// Prevents additional console window on Windows in release, DO NOT REMOVE!!
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    // A successful process start proves that the OS resolved the packaged DLLs.
    // Skip UI, GPU initialization and user data access for this loader check.
    if std::env::args_os().any(|arg| arg == "--check-runtime") {
        return;
    }
    transcriptor_lib::run()
}
