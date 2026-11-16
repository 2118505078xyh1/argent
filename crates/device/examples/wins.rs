//! Locate and capture one window, for visual verification of the GUI.

#[repr(C)]
#[derive(Default, Clone, Copy)]
struct Rect {
    left: i32,
    top: i32,
    right: i32,
    bottom: i32,
}

extern "system" {
    fn GetWindowRect(hwnd: *mut core::ffi::c_void, rect: *mut Rect) -> i32;
}

fn main() {
    let wanted = std::env::args().nth(1).unwrap_or_else(|| "argent".to_string());
    let out = std::env::args()
        .nth(2)
        .unwrap_or_else(|| "window.bmp".to_string());
    let windows = match argent_device::desktop::list_windows() {
        Ok(windows) => windows,
        Err(e) => {
            eprintln!("list failed: {}", e);
            return;
        }
    };
    let Some(window) = windows.iter().find(|w| w.title == wanted) else {
        eprintln!("no window titled {}", wanted);
        for window in &windows {
            eprintln!("  {}", window.title);
        }
        return;
    };
    let mut rect = Rect::default();
    let ok = unsafe {
        GetWindowRect(
            window.handle as *mut core::ffi::c_void,
            &mut rect as *mut Rect,
        )
    };
    if ok == 0 {
        eprintln!("GetWindowRect failed");
        return;
    }
    let width = (rect.right - rect.left).max(1) as u32;
    let height = (rect.bottom - rect.top).max(1) as u32;
    println!(
        "window {} at {},{} {}x{}",
        window.title, rect.left, rect.top, width, height
    );
    let shot = match argent_device::desktop::capture_region(rect.left, rect.top, width, height) {
        Ok(shot) => shot,
        Err(e) => {
            eprintln!("capture failed: {}", e);
            return;
        }
    };
    std::fs::write(&out, argent_device::desktop::encode_bmp(&shot)).unwrap();
    println!("wrote {}", out);
}
