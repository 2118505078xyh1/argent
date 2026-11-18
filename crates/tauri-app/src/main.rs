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
            backend::new_tab,
            backend::activate_tab,
            backend::close_tab,
            backend::send_prompt,
            backend::steer,
            backend::abort,
            backend::new_session,
            backend::open_session,
            backend::rename_session,
            backend::delete_session,
            backend::rewind,
            backend::compact,
            backend::set_model,
            backend::set_thinking,
            backend::list_automations,
            backend::create_automation,
            backend::toggle_automation,
            backend::delete_automation,
            backend::run_automation,
            backend::automation_log,
            backend::list_documents,
            backend::read_document,
            backend::answer_approval,
        ])
        .run(tauri::generate_context!())
        .expect("failed to run argent-tauri");
}
