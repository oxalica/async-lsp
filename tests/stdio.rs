use std::io::{Read, Write};
use std::process::Stdio;
use std::time::Duration;

use async_lsp::stdio::{PipeStdin, PipeStdout};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::time::timeout;

const CHILD_ENV: &str = "IS_STDIO_TEST_CHILD";

const READ_TIMEOUT: Duration = Duration::from_millis(500);
const SCHED_TIMEOUT: Duration = Duration::from_millis(100);

fn main() {
    if std::env::var(CHILD_ENV).is_err() {
        parent();
    } else {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(child());
    }
}

fn parent() {
    let this_exe = std::env::current_exe().unwrap();
    let mut child = std::process::Command::new(this_exe)
        .env(CHILD_ENV, "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("failed to spawn");
    let childin = child.stdin.as_mut().unwrap();
    let childout = child.stdout.as_mut().unwrap();

    let mut buf = [0u8; 64];
    // Wait for the signal.
    assert_eq!(childout.read(&mut buf).unwrap(), 4);
    assert_eq!(&buf[..4], b"ping");
    // Reply back.
    childin.write_all(b"pong").unwrap();

    // NB. Wait for its exit first, without draining `childout`. Because the child keeps writing
    // until the kernel buffer is full.
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success());
    // The last one is written by the std blocking call `print!`.
    assert_eq!(output.stdout, b"2");
}

async fn child() {
    let mut stdin = PipeStdin::lock_tokio().unwrap();
    let mut stdout = PipeStdout::lock_tokio().unwrap();
    let mut buf = [0u8; 64];

    // Should be blocked since we are holding lock guards in `PipeStd{in,out}`.
    let std_stdin = tokio::task::spawn_blocking(|| drop(std::io::stdin().lock()));
    let std_stdout = tokio::task::spawn_blocking(|| print!("2"));

    timeout(READ_TIMEOUT, stdin.read(&mut buf))
        .await
        .expect_err("should timeout");

    // Signal the parent to send us something. This should not block due to pipe buffer.
    timeout(Duration::ZERO, stdout.write_all(b"ping"))
        .await
        .expect("should not block")
        .unwrap();

    assert_eq!(
        timeout(SCHED_TIMEOUT, stdin.read(&mut buf))
            .await
            .expect("should not timeout")
            .expect("should read something"),
        4
    );
    assert_eq!(&buf[..4], b"pong");

    // Still blocked yet.
    assert!(!std_stdin.is_finished());
    assert!(!std_stdout.is_finished());

    // Drop lock guards, then std operations unblock.
    drop(stdin);
    drop(stdout);
    timeout(SCHED_TIMEOUT, std_stdin)
        .await
        .expect("no timeout")
        .expect("no panic");
    timeout(SCHED_TIMEOUT, std_stdout)
        .await
        .expect("no timeout")
        .expect("no panic");
}
