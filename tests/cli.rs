//! CLI-surface tests: behaviours that only exist once the binary is a
//! process, so no unit test can reach them.

use std::process::Command;

fn sh(script: &str) -> String {
    let out = Command::new("sh")
        .arg("-c")
        .arg(script)
        .output()
        .expect("should run");
    format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

/// `gitq ... | head` must not panic.  Rust masks SIGPIPE at startup and
/// turns the resulting EPIPE into a panic on the first `println!` after the
/// reader goes away, so this printed a panic message and a backtrace hint
/// where every other Unix tool simply stops.
#[test]
fn stdout_closing_early_is_not_a_panic() {
    let exe = env!("CARGO_BIN_EXE_gitq");
    for args in [
        "--complete ''",
        "--complete-annotated 'commits via '",
        "'commits'",
        "--sexp 'commits'",
    ] {
        let out = sh(&format!("'{exe}' {args} 2>&1 | head -1"));
        assert!(
            !out.contains("panicked"),
            "{args}: panicked on a closed pipe:\n{out}"
        );
        assert!(
            !out.contains("Broken pipe"),
            "{args}: leaked an EPIPE error:\n{out}"
        );
    }
}

/// Completion must never fail mid-keystroke, whatever is half-typed.
#[test]
fn completion_never_errors_on_partial_input() {
    let exe = env!("CARGO_BIN_EXE_gitq");
    for prefix in [
        "",
        "com",
        "commits ",
        "commits where ",
        "commits where \"unterminated",
    ] {
        let out = sh(&format!("'{exe}' --complete '{prefix}' 2>&1; echo rc=$?"));
        assert!(
            out.contains("rc=0"),
            "prefix {prefix:?} exited non-zero:\n{out}"
        );
        assert!(
            !out.contains("panicked"),
            "prefix {prefix:?} panicked:\n{out}"
        );
    }
}

/// A parse error goes to stderr with a non-zero exit, so shell callers can
/// tell failure from an empty result.
#[test]
fn parse_errors_are_loud_and_exit_nonzero() {
    let exe = env!("CARGO_BIN_EXE_gitq");
    let out = sh(&format!(
        "'{exe}' 'commits where' 2>&1 1>/dev/null; echo rc=$?"
    ));
    assert!(out.contains("requires at least one condition"), "{out}");
    assert!(out.contains("rc=1"), "{out}");
}
