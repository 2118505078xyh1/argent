use argent_pty::PtySession;
use std::time::Duration;

fn main() {
    let exe = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "target/debug/argent.exe".to_string());
    let cwd = std::env::current_dir().expect("cwd");
    let mut session = PtySession::spawn(&format!("\"{}\" tui", exe), &cwd, 100, 30).expect("spawn");
    std::thread::sleep(Duration::from_secs(3));
    let out = session.output.all();
    println!(
        "bytes={} contains_mu={} contains_status={}",
        out.len(),
        out.contains("argent"),
        out.contains("Enter send")
    );
    println!("--- raw ---");
    println!("{}", out.replace('\x1b', "<ESC>"));
    session.kill();
}
