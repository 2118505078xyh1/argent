//! Native argent frontend: iced owns the window, `argent-engine` owns the agent.
//!
//! There is no WebView and no JavaScript. The engine worker is polled from the
//! iced event loop, and `argent-ui-core` decides what a run means before this
//! crate draws it.

// A window, not a console: opening the app from a shortcut should not show a
// black box next to it.
#![windows_subsystem = "windows"]

mod app;
mod style;
mod view;

fn main() -> iced::Result {
    // 兼容旧版：先把 MU_* 环境变量镜像成 ARGENT_*，再把 ~/.mu 迁移到 ~/.argent。
    argent_core::env::inherit_legacy();
    let _ = argent_config::migrate_legacy_home();
    iced::application(app::App::new, app::App::update, app::App::view)
        .title(|app: &app::App| app.title())
        .theme(app::App::theme)
        .subscription(app::App::subscription)
        .default_font(style::ui_font())
        .window_size((1180.0, 780.0))
        .centered()
        .antialiasing(true)
        .run()
}