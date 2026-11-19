//! Argent desktop shell: Tauri 2 owns the window; the Rust engine owns the agent.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod backend;

use tauri::menu::{Menu, MenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{Manager, WindowEvent};

fn main() {
    tauri::Builder::default()
        .plugin(tauri_plugin_deep_link::init())
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            if let Some(window) = app.get_webview_window("main") {
                let _ = window.unminimize();
                let _ = window.set_focus();
            }
        }))
        .plugin(tauri_plugin_window_state::Builder::default().build())
        .setup(|app| {
            let backend = backend::Backend::start(app.handle().clone())?;
            if let Some(window) = app.get_webview_window("main") {
                let shutdown = backend.clone();
                let hide_on_close = window.clone();
                window.on_window_event(move |event| {
                    if let WindowEvent::CloseRequested { api, .. } = event {
                        shutdown.shutdown_all();
                        api.prevent_close();
                        let _ = hide_on_close.hide();
                    }
                });
            }

            let show = MenuItem::with_id(app, "show", "显示窗口", true, None::<&str>)?;
            let quit = MenuItem::with_id(app, "quit", "退出", true, None::<&str>)?;
            let menu = Menu::with_items(app, &[&show, &quit])?;
            let icon = app
                .default_window_icon()
                .cloned()
                .ok_or("missing default window icon")?;
            let _tray = TrayIconBuilder::with_id("main")
                .icon(icon)
                .menu(&menu)
                .tooltip("Argent")
                .on_menu_event(|app, event| match event.id().as_ref() {
                    "quit" => app.exit(0),
                    "show" => {
                        if let Some(window) = app.get_webview_window("main") {
                            let _ = window.show();
                            let _ = window.set_focus();
                        }
                    }
                    _ => {}
                })
                .on_tray_icon_event(|tray, event| {
                    if let TrayIconEvent::Click {
                        button: MouseButton::Left,
                        button_state: MouseButtonState::Up,
                        ..
                    } = event
                    {
                        if let Some(window) = tray.app_handle().get_webview_window("main") {
                            let _ = window.show();
                            let _ = window.set_focus();
                        }
                    }
                })
                .build(app)?;

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
            backend::preview_schedule,
            backend::list_automations,
            backend::create_automation,
            backend::toggle_automation,
            backend::delete_automation,
            backend::run_automation,
            backend::automation_log,
            backend::list_workflows,
            backend::list_jobs,
            backend::start_job,
            backend::job_tail,
            backend::kill_job,
            backend::list_mcp_servers,
            backend::git_status,
            backend::git_diff,
            backend::usage_summary,
            backend::list_extensions,
            backend::search_memory,
            backend::list_memory,
            backend::add_memory,
            backend::forget_memory,
            backend::import_session_text,
            backend::search_sessions,
            backend::list_files,
            backend::list_documents,
            backend::read_document,
            backend::answer_approval,
            backend::allow_tool,
        ])
        .run(tauri::generate_context!())
        .expect("failed to run argent-tauri");
}
