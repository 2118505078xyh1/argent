use argent_pty::PtySession;
use std::time::Duration;

fn main() {
    let cwd = std::env::temp_dir();
    eprintln!("[pty] spawning");
    let mut session = match PtySession::spawn(
        "cmd /C \"echo hello-from-pty & echo second-line\"",
        &cwd,
        80,
        24,
    ) {
        Ok(session) => session,
        Err(e) => {
            eprintln!("[pty] spawn failed: {}", e);
            return;
        }
    };
    eprintln!("[pty] spawned pid-alive={}", !session.has_exited());
    std::thread::sleep(Duration::from_millis(800));
    let text = session.read();
    eprintln!("[pty] output {:?}", text);
    eprintln!(
        "[pty] exited={:?} code={:?}",
        session.has_exited(),
        session.exit_code()
    );
    session.kill();
}
