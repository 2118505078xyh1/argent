//! Drive the real `argent tui` through ConPTY.
//!
//! This is the integration test the unit tests cannot provide: raw mode,
//! alternate screen, key parsing and clean exit against an actual terminal.
//!
//! Run with:
//!   cargo run -p argent-pty --example tui_probe -- target/debug/argent.exe

use std::time::{Duration, Instant};

use argent_pty::PtySession;

fn main() {
    let exe = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "target/debug/argent.exe".to_string());
    let cwd = std::env::current_dir().expect("cwd");
    let command = format!("\"{}\" tui", exe);
    let mut session =
        PtySession::spawn(&command, &cwd, 100, 30).expect("start the TUI in a ConPTY");

    // 键位提示现在只在首帧出现，启动探针改等首屏标题而不是状态栏文案。
    if !session.wait_for("argent", Duration::from_secs(10)) {
        eprintln!("TUI did not draw its first frame:\n{}", session.output.all());
        std::process::exit(1);
    }

    // Let the Ready event paint the header before capture.
    std::thread::sleep(Duration::from_millis(300));
    let rendered = session.output.all();
    for needle in ["argent"] {
        if !rendered.contains(needle) {
            eprintln!("missing {:?} in the rendered UI:\n{}", needle, rendered);
            std::process::exit(1);
        }
    }

    session.write("/exit\r").expect("send /exit");
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline && !session.has_exited() {
        std::thread::sleep(Duration::from_millis(50));
    }
    if !session.has_exited() {
        session.kill();
        eprintln!("TUI did not exit after /exit:\n{}", session.output.all());
        std::process::exit(1);
    }

    let code = session.exit_code().unwrap_or(-1);
    println!("tui exited with {}", code);
    if code != 0 {
        eprintln!("{}", session.output.all());
        std::process::exit(1);
    }
    println!("tui probe passed ({} bytes rendered)", rendered.len());
}
