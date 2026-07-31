#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use serde::Serialize;
use tauri::Emitter;

#[derive(Clone, Serialize)]
struct OpcosEvent {
    kind: &'static str,
    message: &'static str,
}

#[tauri::command]
fn ping() -> &'static str {
    "pong"
}

#[tauri::command]
fn emit_demo(app: tauri::AppHandle) -> Result<(), String> {
    app.emit(
        "opcos://event",
        OpcosEvent {
            kind: "system",
            message: "OPCOS backend is ready",
        },
    )
    .map_err(|error| error.to_string())
}

fn main() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .invoke_handler(tauri::generate_handler![ping, emit_demo])
        .setup(|app| {
            let handle = app.handle().clone();
            app.run_on_main_thread(move || {
                let _ = handle.emit(
                    "opcos://event",
                    OpcosEvent {
                        kind: "system",
                        message: "OPCOS started",
                    },
                );
            })
            .ok();
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running OPCOS");
}
