//! Extract the useful lines from build/test logs.
//!
//! A failing `cargo test` can print thousands of lines; the model needs the
//! error codes, file locations and failed test names, not the progress bars.
//! This module recognises the common shapes and returns a compact block. The
//! full log is still written to `~/.argent/tool-output/` by the caller.

use argent_core::text::strip_ansi;

/// Return a compact diagnostics block, or `None` when the output does not look
/// like a compiler/test failure (normal head+tail truncation is better then).
pub fn extract(content: &str) -> Option<String> {
    let cleaned = strip_ansi(content);
    let lines: Vec<&str> = cleaned.lines().collect();
    let mut selected: Vec<&str> = Vec::new();
    let mut errors = 0usize;
    let mut failures = 0usize;
    let mut index = 0usize;

    while index < lines.len() {
        let line = lines[index].trim_end();
        let trimmed = line.trim_start();
        if is_error_line(trimmed) {
            errors += 1;
            selected.push(line);
            // The next lines usually carry the file location and a note.
            let mut look = index + 1;
            let mut added = 0;
            while look < lines.len() && added < 3 {
                let next = lines[look].trim_end();
                let next_trimmed = next.trim_start();
                if next.trim().is_empty() {
                    break;
                }
                if next_trimmed.starts_with("-->")
                    || next_trimmed.starts_with('|')
                    || next_trimmed.starts_with("= note")
                    || next_trimmed.starts_with("at ")
                    || next_trimmed.starts_with("File \"")
                    || looks_like_location(next_trimmed)
                {
                    selected.push(next);
                    added += 1;
                    look += 1;
                } else {
                    break;
                }
            }
            index = look;
            continue;
        }
        if is_failure_line(trimmed) {
            failures += 1;
            selected.push(line);
        }
        index += 1;
    }

    if errors + failures == 0 || selected.is_empty() {
        return None;
    }
    // Keep the block useful but bounded; the caller points at the full log.
    selected.truncate(80);
    let mut out = format!(
        "[diagnostics: {} error line(s), {} failure line(s) extracted]\n",
        errors, failures
    );
    out.push_str(&selected.join("\n"));
    Some(argent_core::text::truncate(&out, 12_000))
}

fn is_error_line(line: &str) -> bool {
    line.starts_with("error[")
        || line.starts_with("error:")
        || line.starts_with("error TS")
        || line.contains("): error TS")
        || line.starts_with("error ")
        || line.starts_with("E   ")
        || line.starts_with("panic:")
        || line.starts_with("thread '") && line.contains("panicked at")
        || line.starts_with("AssertionError")
}

fn is_failure_line(line: &str) -> bool {
    line.contains("FAILED")
        || line.starts_with("--- FAIL:")
        || line.starts_with("FAIL\t")
        || line.starts_with("FAIL ")
        || line.starts_with("failures:")
        || line.starts_with("E   AssertionError")
}

/// `src/main.rs:12:5` or `tests/test_x.py:9`.
fn looks_like_location(line: &str) -> bool {
    let Some((path, _)) = line.split_once(':') else {
        return false;
    };
    path.ends_with(".rs")
        || path.ends_with(".ts")
        || path.ends_with(".tsx")
        || path.ends_with(".js")
        || path.ends_with(".py")
        || path.ends_with(".go")
        || path.ends_with(".java")
        || path.ends_with(".cs")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_rust_errors_and_locations() {
        let log = "
   Compiling argent v0.1.0
    Finished dev [unoptimized] target(s)
error[E0425]: cannot find value `x` in this scope
 --> src/main.rs:12:5
  |
12 |     x
  |     ^ not found
error: could not compile `argent` due to previous error
test result: FAILED. 2 passed; 1 failed
";
        let block = extract(log).expect("diagnostics");
        assert!(block.contains("E0425"), "{}", block);
        assert!(block.contains("src/main.rs:12:5"), "{}", block);
        assert!(block.contains("FAILED"), "{}", block);
        assert!(!block.contains("Compiling argent"), "{}", block);
    }

    #[test]
    fn extracts_python_and_ts_failures() {
        let log = "
FAILED tests/test_api.py::test_status - AssertionError: 500 != 200
E   AssertionError: 500 != 200
src/app.ts(14,3): error TS2322: Type 'string' is not assignable to type 'number'.
";
        let block = extract(log).expect("diagnostics");
        assert!(block.contains("test_status"), "{}", block);
        assert!(block.contains("TS2322"), "{}", block);
    }

    #[test]
    fn ordinary_output_is_not_mistaken_for_an_error() {
        assert!(extract("all good\n42 tests passed\n").is_none());
    }
}