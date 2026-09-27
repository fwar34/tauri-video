use tauri::ipc::{Response, Channel};

pub mod video;

// Learn more about Tauri commands at https://tauri.app/develop/calling-rust/
#[tauri::command]
fn greet(name: &str) -> String {
    format!("Hello, {}! You've been greeted from Rust!", name)
}

#[tauri::command]
async fn start(video_path: String, on_chunk: Channel<Response>) -> Result<video::MediaInfo, video::VideoError> {
    video::start(&video_path, on_chunk).await
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dialog::init())
        .invoke_handler(tauri::generate_handler![greet, start])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
