//! The chat-approval hook script (`packaging/macos/approve-hook.sh`) with a fake `winhands` that
//! only records its arguments. Nothing here touches a daemon, the screen or Accessibility.
#![cfg(unix)]

use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};

fn script() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../packaging/macos/approve-hook.sh")
}

/// Run the hook on `stdin`; returns (stdout, exit code, the lines the fake winhands recorded).
fn run(stdin: &str, fake_body: &str) -> (String, i32, Vec<String>) {
    static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("wh-hook-{}-{n}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let log = dir.join("calls.txt");
    let _ = std::fs::remove_file(&log);
    let fake = dir.join("fake-winhands");
    std::fs::write(
        &fake,
        format!(
            "#!/bin/sh\necho \"$@\" >> '{}'\n{fake_body}\n",
            log.display()
        ),
    )
    .unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let mut child = Command::new("sh")
        .arg(script())
        .env("WINHANDS_BIN", &fake)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(stdin.as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    let calls = std::fs::read_to_string(&log)
        .unwrap_or_default()
        .lines()
        .map(str::to_string)
        .collect();
    (
        String::from_utf8_lossy(&out.stdout).into_owned() + &String::from_utf8_lossy(&out.stderr),
        out.status.code().unwrap_or(-1),
        calls,
    )
}

fn prompt(p: &str) -> String {
    serde_json::json!({"session_id": "s1", "cwd": "/tmp", "prompt": p}).to_string()
}

#[test]
fn allow_and_deny_run_the_matching_command_silently() {
    let (out, code, calls) = run(&prompt("allow K7QM"), "");
    assert_eq!((out.as_str(), code), ("", 0));
    assert_eq!(calls, ["approve K7QM"]);
    let (out, code, calls) = run(&prompt("Deny k7qm please"), "");
    assert_eq!((out.as_str(), code), ("", 0));
    assert_eq!(calls, ["deny K7QM"]);
}

#[test]
fn several_codes_in_one_prompt_and_a_repeated_one_runs_once() {
    let (_, code, calls) = run(
        &prompt("ok.\nallow AB23, deny CD45 and allow AB23 again\nALLOW ef67"),
        "",
    );
    assert_eq!(code, 0);
    assert_eq!(calls, ["approve AB23", "deny CD45", "approve EF67"]);
}

#[test]
fn only_whole_words_with_the_safe_characters_count() {
    for text in [
        "disallow K7QM",
        "allow K7QMX",
        "allow K7Q",
        "allow K0QM",
        "allow KIQM",
        "allow KOQM",
        "I allowed K7QM",
        "no codes here",
    ] {
        let (out, code, calls) = run(&prompt(text), "");
        assert_eq!((out.as_str(), code), ("", 0), "{text}");
        assert!(calls.is_empty(), "{text}: {calls:?}");
    }
}

#[test]
fn only_the_prompt_field_is_read() {
    let json = serde_json::json!({"cwd": "/work/allow AB23", "prompt": "hello"}).to_string();
    let (_, _, calls) = run(&json, "");
    assert!(calls.is_empty(), "{calls:?}");
}

#[test]
fn a_failing_or_missing_winhands_never_fails_the_prompt() {
    let (out, code, calls) = run(&prompt("allow K7QM"), "echo boom >&2; exit 3");
    assert_eq!((out.as_str(), code), ("", 0));
    assert_eq!(calls, ["approve K7QM"]);
    // garbage on stdin, no winhands at all
    let out = Command::new("sh")
        .arg(script())
        .env("WINHANDS_BIN", "/nonexistent/winhands")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .and_then(|mut c| {
            c.stdin
                .take()
                .unwrap()
                .write_all(b"\xff not json allow K7QM")
                .unwrap();
            c.wait_with_output()
        })
        .unwrap();
    assert_eq!(out.status.code(), Some(0));
    assert!(out.stdout.is_empty());
}
