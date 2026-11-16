//! Write the argent mark as an .ico file, for a desktop shortcut.

fn main() {
    let path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "argent.ico".to_string());
    let size: usize = std::env::args()
        .nth(2)
        .and_then(|value| value.parse().ok())
        .unwrap_or(256);
    match argent_ui_core::write_ico(std::path::Path::new(&path), size) {
        Ok(()) => println!("wrote {} ({}px)", path, size),
        Err(e) => eprintln!("failed: {}", e),
    }
}
