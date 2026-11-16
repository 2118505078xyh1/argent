//! Capture the screen to a BMP, for visual verification of the GUI.

fn main() {
    let path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "screen.bmp".to_string());
    let shot = match argent_device::desktop::capture_screen() {
        Ok(shot) => shot,
        Err(e) => {
            eprintln!("capture failed: {}", e);
            std::process::exit(1);
        }
    };
    let bmp = argent_device::desktop::encode_bmp(&shot);
    if let Err(e) = std::fs::write(&path, &bmp) {
        eprintln!("write {} failed: {}", path, e);
        std::process::exit(1);
    }
    println!("captured {}x{} -> {}", shot.width, shot.height, path);
}
