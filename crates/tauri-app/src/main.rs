//! Argent desktop shell: Tauri 2 owns the window; the Rust engine owns the agent.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod backend;

use tauri::Manager;

fn main() {
    tauri::Builder::default()
        .setup(|app| {
            let backend = backend::Backend::start(app.handle().clone())?;
            app.manage(backend);
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            backend::get_state,
            backend::send_prompt,
            backend::abort,
            backend::new_session,
            backend::answer_approval,
        ])
        .run(tauri::generate_context!())
        .expect("failed to run argent-tauri");
}
