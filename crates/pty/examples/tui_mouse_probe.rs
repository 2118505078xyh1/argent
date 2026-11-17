//! Verify that mouse reports reach the TUI's own selection logic.
//!
//! The TUI enables SGR mouse reporting at startup. Under ConPTY the terminal
//! turns those reports into console input records, which the input thread must
//! map to `Key::MousePress`. A press alone highlights the row with reverse
//! video, so finding an SGR "reverse" attribute after the report proves the
//! whole chain works without touching the user's clipboard.
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
    std::thread::sleep(Duration::from_millis(400));
    let before = session.output.all();
    if before.contains("\x1b[7m") {
        eprintln!("the opening frame already uses reverse video; probe is ambiguous");
        session.kill();
        std::process::exit(1);
    }
    // SGR 1006 press: button 0 (left), column 20, row 10, both 1-based.
    session.write("\x1b[<0;20;10M").expect("send mouse press");
    std::thread::sleep(Duration::from_millis(600));
    let after = session.output.all();
    if !after.contains("\x1b[7m") {
        eprintln!(
            "a mouse press never reached App::handle_key (no selection highlight):\n{}",
            after
        );
        session.kill();
        std::process::exit(1);
    }
    // Release on the same cell: App treats it as a click on a thinking block,
    // not a copy, so the clipboard is untouched.
    session.write("\x1b[<0;20;10m").expect("send mouse release");
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
    println!("tui mouse probe passed");
}
