//! Verify that typed keys reach the TUI input box under ConPTY.
use argent_pty::PtySession;
use std::time::Duration;

fn main() {
    let exe = std::env::args().nth(1).expect("path to argent.exe");
    let cwd = std::env::current_dir().expect("cwd");
    let mut session =
        PtySession::spawn(&format!("\"{}\" tui", exe), &cwd, 100, 30).expect("start TUI");
    if !session.wait_for("argent", Duration::from_secs(10)) {
        eprintln!("TUI did not start:\n{}", session.output.all());
        std::process::exit(1);
    }
    // 不连模型也能验证输入：字符若被输入线程收到，首帧之后会重绘输入框。
    session.write("typing works").expect("send keys");
    std::thread::sleep(Duration::from_millis(800));
    let output = session.output.all();
    if !output.contains("typing works") {
        eprintln!("typed text was not rendered:\n{}", output);
        session.kill();
        std::process::exit(1);
    }
    // 先把验证文字逐字删掉，避免 /exit 被拼成普通消息。
    for _ in 0..40 {
        session.write("\x08").expect("backspace");
    }
    std::thread::sleep(Duration::from_millis(250));
    session.write("/exit\r").expect("send /exit");
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while std::time::Instant::now() < deadline && !session.has_exited() {
        std::thread::sleep(Duration::from_millis(50));
    }
    if !session.has_exited() {
        session.kill();
        eprintln!("TUI did not exit:\n{}", session.output.all());
        std::process::exit(1);
    }
    println!("tui typing probe passed");
}
