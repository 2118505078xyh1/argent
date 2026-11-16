use argent_pty::PtySession;
use std::time::{Duration, Instant};

fn main() {
    let exe = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "target/debug/argent.exe".to_string());
    let port = std::env::args()
        .nth(2)
        .unwrap_or_else(|| "38475".to_string());
    let cwd = std::env::current_dir().expect("cwd");
    let command = format!(
        "\"{}\" tui --provider anthropic --base-url http://127.0.0.1:{} --api-key test --mode build --no-color",
        exe, port
    );
    let mut session = PtySession::spawn(&command, &cwd, 110, 32).expect("start TUI");
    if !session.wait_for("Enter send", Duration::from_secs(10)) {
        eprintln!("TUI did not start:\n{}", session.output.all());
        std::process::exit(1);
    }
    session.write("write a file\r").expect("send prompt");
    if !session.wait_for("permission", Duration::from_secs(15)) {
        eprintln!("approval modal did not appear:\n{}", session.output.all());
        session.kill();
        std::process::exit(1);
    }
    session.write("y").expect("approve");
    if !session.wait_for("approved and written", Duration::from_secs(15)) {
        eprintln!("answer did not arrive:\n{}", session.output.all());
        session.kill();
        std::process::exit(1);
    }
    session.write("/exit\r").expect("quit");
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline && !session.has_exited() {
        std::thread::sleep(Duration::from_millis(50));
    }
    if !session.has_exited() {
        session.kill();
        eprintln!("TUI did not exit:\n{}", session.output.all());
        std::process::exit(1);
    }
    println!(
        "tui approval probe passed, exit {}",
        session.exit_code().unwrap_or(-1)
    );
}
