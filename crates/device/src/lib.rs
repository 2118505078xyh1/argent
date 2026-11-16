//! argent device layer: browser automation over CDP and desktop control.

pub mod browser;
pub mod desktop;
pub mod tools;
pub mod websocket;

pub use browser::{Browser, LaunchOptions};
pub use desktop::{capture_screen, click, list_windows, type_text, MouseButton, Screenshot};
