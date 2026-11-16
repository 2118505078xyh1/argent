//! Send a prompt through the real TUI and verify the streamed answer.
//!
//!   cargo run -p argent-pty --example tui_chat_probe -- target/debug/argent.exe <port>

use argent_pty::PtySession;
use std::time::{Duration, Instant};

fn main() {
    let exe = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "target/debug/argent.exe".to_string());
    let port = std::env::args()
        .nth(2)
        .unwrap_or_else(|| "38474".to_string());
    let cwd = std::env::current_dir().expect("cwd");
    let command = format!(
        "\"{}\" tui --provider anthropic --base-url http://127.0.0.1:{} --api-key test --no-color",
        exe, port
    );
    let mut session = PtySession::spawn(&command, &cwd, 110, 32).expect("start TUI");
    // 键位提示只在首帧出现，启动判定改等首屏标题。
    if !session.wait_for("argent", Duration::from_secs(10)) {
        eprintln!("TUI did not start:\n{}", session.output.all());
        std::process::exit(1);
    }
    session.write("say hello\r").expect("send prompt");
    if !session.wait_for("Hello from fake Anthropic", Duration::from_secs(15)) {
        eprintln!("answer did not arrive:\n{}", session.output.all());
        session.kill();
        std::process::exit(1);
    }
    session.write("/exit\r").expect("send /exit");
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline && !session.has_exited() {
        std::thread::sleep(Duration::from_millis(50));
    }
    if !session.has_exited() {
        session.kill();
        eprintln!("TUI did not exit:\n{}", session.output.all());
        std::process::exit(1);
    }
    let code = session.exit_code().unwrap_or(-1);
    println!("tui chat exited with {}", code);
    if code != 0 {
        eprintln!("{}", session.output.all());
        std::process::exit(1);
    }
    let _ = std::fs::write(".tmp/tui-chat-output.raw", session.output.all());
    println!("tui chat probe passed");
}
