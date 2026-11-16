//! Click, type and capture: drives the GUI from a script so its output can be
//! reviewed without a human at the keyboard.
//!
//! usage: drive <x> <y> <text> <out.bmp> [wait_ms]

fn main() {
    let mut args = std::env::args().skip(1);
    let x: i32 = args.next().unwrap_or_default().parse().unwrap_or(0);
    let y: i32 = args.next().unwrap_or_default().parse().unwrap_or(0);
    let text = args.next().unwrap_or_default();
    let out = args.next().unwrap_or_else(|| "drive.bmp".to_string());
    let wait: u64 = args.next().unwrap_or_default().parse().unwrap_or(0);
    let press_enter = args.next().map(|value| value != "noenter").unwrap_or(true);

    if let Err(e) = argent_device::desktop::click(x, y, argent_device::desktop::MouseButton::Left) {
        eprintln!("click failed: {}", e);
        return;
    }
    std::thread::sleep(std::time::Duration::from_millis(250));
    if !text.is_empty() {
        if let Err(e) = argent_device::desktop::type_text(&text) {
            eprintln!("type failed: {}", e);
        }
        std::thread::sleep(std::time::Duration::from_millis(250));
        if press_enter {
            if let Err(e) = argent_device::desktop::press_keys("enter") {
                eprintln!("enter failed: {}", e);
            }
        }
    }
    if wait > 0 {
        std::thread::sleep(std::time::Duration::from_millis(wait));
    }
    match argent_device::desktop::capture_screen() {
        Ok(shot) => match std::fs::write(&out, argent_device::desktop::encode_bmp(&shot)) {
            Ok(()) => println!("captured {}", out),
            Err(e) => eprintln!("write failed: {}", e),
        },
        Err(e) => eprintln!("capture failed: {}", e),
    }
}
