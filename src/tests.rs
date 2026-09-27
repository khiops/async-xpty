//! Integration tests for `async-xpty`.
//!
//! These tests require a Unix environment and `/bin/sh`.

#![cfg(test)]
#![cfg(unix)]

use std::mem::MaybeUninit;

use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::{CommandBuilder, PtySize};

/// SC-01: Basic PTY spawn returns a valid PID.
#[tokio::test]
async fn test_spawn_pty() {
    let pty = CommandBuilder::new("/bin/sh").spawn().await.unwrap();
    assert!(pty.pid() > 0, "expected non-zero PID");
    pty.kill().ok();
}

/// SC-02: Read PTY output from a one-shot command.
#[tokio::test]
async fn test_read_output() {
    let pty = CommandBuilder::new("/bin/sh")
        .arg("-c")
        .arg("echo hello")
        .spawn()
        .await
        .unwrap();

    let mut reader = pty.reader();
    let mut output = String::new();

    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        reader.read_to_string(&mut output),
    )
    .await
    .expect("read timed out")
    .expect("read error");

    assert!(output.contains("hello"), "expected 'hello' in {:?}", output);
}

/// SC-03: Write to PTY stdin and read the echoed output back.
#[tokio::test]
async fn test_write_input() {
    let pty = CommandBuilder::new("/bin/sh").spawn().await.unwrap();

    let mut writer = pty.writer();
    writer.write_all(b"echo test123\n").await.unwrap();

    // Give the shell time to execute and echo
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    let mut reader = pty.reader();
    let mut buf = vec![0u8; 4096];
    let n = tokio::time::timeout(std::time::Duration::from_secs(5), reader.read(&mut buf))
        .await
        .expect("read timed out")
        .expect("read error");

    let output = String::from_utf8_lossy(&buf[..n]);
    assert!(
        output.contains("test123"),
        "expected 'test123' in {:?}",
        output
    );

    pty.kill().ok();
}

/// SC-04: Resize the PTY without error.
#[tokio::test]
async fn test_resize() {
    let pty = CommandBuilder::new("/bin/sh").spawn().await.unwrap();
    pty.resize(PtySize {
        cols: 120,
        rows: 40,
    })
    .await
    .unwrap();
    pty.kill().ok();
}

/// SC-05: Exit code is propagated correctly.
#[tokio::test]
async fn test_exit_code() {
    let mut pty = CommandBuilder::new("/bin/sh")
        .arg("-c")
        .arg("exit 42")
        .spawn()
        .await
        .unwrap();

    let status = tokio::time::timeout(std::time::Duration::from_secs(10), pty.wait())
        .await
        .expect("wait timed out")
        .expect("wait error");

    assert_eq!(
        status.code(),
        Some(42),
        "expected exit code 42, got {:?}",
        status
    );
}

/// SC-06: Env and cwd are set in the child.
#[tokio::test]
async fn test_env_and_cwd() {
    let pty = CommandBuilder::new("/bin/sh")
        .arg("-c")
        .arg("echo $FOO && pwd")
        .env("FOO", "bar")
        .current_dir("/tmp")
        .spawn()
        .await
        .unwrap();

    let mut reader = pty.reader();
    let mut output = String::new();

    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        reader.read_to_string(&mut output),
    )
    .await
    .expect("read timed out")
    .expect("read error");

    assert!(output.contains("bar"), "expected 'bar' in {:?}", output);
    assert!(output.contains("/tmp"), "expected '/tmp' in {:?}", output);
}

/// SC-08: Spawning a non-existent program returns an error.
#[tokio::test]
async fn test_spawn_nonexistent() {
    let result = CommandBuilder::new("/nonexistent/shell").spawn().await;
    assert!(result.is_err(), "expected error for non-existent program");
}

/// SC-07: Ctrl+C (SIGINT via ETX byte) terminates the child.
///
/// Validates that `TIOCSCTTY` was called so the PTY master is the controlling
/// terminal of the child's process group. Without it, `\x03` would be passed
/// as literal data rather than generating SIGINT.
#[tokio::test]
// Ctrl+C → SIGINT relies on controlling-terminal / foreground-process-group
// semantics that differ on macOS/BSD; the child doesn't receive the signal
// there yet. Tracked in khiops/async-xpty#1.
#[cfg_attr(target_os = "macos", ignore = "macOS job-control gap — see #1")]
async fn test_ctrl_c_signal() {
    let mut pty = CommandBuilder::new("/bin/sh")
        .arg("-c")
        .arg("sleep 60")
        .spawn()
        .await
        .unwrap();

    // Wait for the shell (and sleep) to be running
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    let mut writer = pty.writer();
    writer.write_all(b"\x03").await.unwrap();

    let status = tokio::time::timeout(std::time::Duration::from_secs(5), pty.wait())
        .await
        .expect("wait timed out after Ctrl+C")
        .expect("wait error");

    // The shell or sleep should have been interrupted — either a signal or
    // a non-zero exit code is acceptable.
    assert!(
        status.code().is_some() || status.signal().is_some(),
        "expected exit via code or signal after Ctrl+C, got nothing"
    );
}

/// Verify `env_clear` strips inherited environment variables.
#[tokio::test]
async fn test_env_clear() {
    // We know HOME is set in the test process environment. After env_clear it
    // should not be visible in the child (unless we re-export it, which we
    // don't here).
    let pty = CommandBuilder::new("/bin/sh")
        .arg("-c")
        .arg("echo HOME=${HOME}")
        .env_clear()
        .spawn()
        .await
        .unwrap();

    let mut reader = pty.reader();
    let mut output = String::new();

    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        reader.read_to_string(&mut output),
    )
    .await
    .expect("read timed out")
    .expect("read error");

    // HOME should be unset → "HOME=" with empty value
    assert!(
        output.contains("HOME=\r") || output.contains("HOME=\n"),
        "expected empty HOME in {:?}",
        output
    );
}

/// Run a one-shot PTY command to its end: everything it printed, then how it
/// ended.
async fn output_and_status(pty: &mut crate::PtyProcess) -> (String, crate::ExitStatus) {
    let mut output = Vec::new();
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        pty.reader().read_to_end(&mut output),
    )
    .await
    .expect("read timed out")
    .expect("read error");

    let status = tokio::time::timeout(std::time::Duration::from_secs(5), pty.wait())
        .await
        .expect("wait timed out")
        .expect("wait error");

    (String::from_utf8_lossy(&output).into_owned(), status)
}

/// Blocks one signal in the calling thread until dropped.
struct BlockedInThisThread {
    previous: libc::sigset_t,
}

impl BlockedInThisThread {
    fn new(signal: libc::c_int) -> Self {
        // SAFETY: both sets are initialised before use, by `sigemptyset` and
        // by `pthread_sigmask` writing the previous mask.
        unsafe {
            let mut set = MaybeUninit::<libc::sigset_t>::uninit();
            libc::sigemptyset(set.as_mut_ptr());
            libc::sigaddset(set.as_mut_ptr(), signal);
            let mut previous = MaybeUninit::<libc::sigset_t>::uninit();
            let rc = libc::pthread_sigmask(libc::SIG_BLOCK, set.as_ptr(), previous.as_mut_ptr());
            assert_eq!(rc, 0, "pthread_sigmask(SIG_BLOCK) failed");
            Self {
                previous: previous.assume_init(),
            }
        }
    }
}

impl Drop for BlockedInThisThread {
    fn drop(&mut self) {
        // SAFETY: restores the mask saved by `new`. The tests drop the guard
        // on the thread that created it.
        unsafe { libc::pthread_sigmask(libc::SIG_SETMASK, &self.previous, std::ptr::null_mut()) };
    }
}

/// The child does not inherit the parent's ignored `SIGPIPE` (lasterm#597).
///
/// With it, `yes | head -1` in the terminal printed a write error instead of
/// `yes` dying quietly. Here the shell sends itself `SIGPIPE`: it must die of
/// it, not go on to print `alive`.
#[tokio::test]
async fn test_child_does_not_inherit_ignored_sigpipe() {
    // The Rust runtime already ignores SIGPIPE; ignoring it again keeps the
    // test independent of that and changes nothing for this process.
    // SAFETY: SIG_IGN installs no handler.
    let previous = unsafe { libc::signal(libc::SIGPIPE, libc::SIG_IGN) };
    assert_ne!(previous, libc::SIG_ERR, "could not ignore SIGPIPE");

    let mut pty = CommandBuilder::new("/bin/sh")
        .arg("-c")
        .arg("kill -PIPE $$; echo alive")
        .spawn()
        .await
        .unwrap();
    let (output, status) = output_and_status(&mut pty).await;

    assert_eq!(
        status.signal(),
        Some(libc::SIGPIPE),
        "expected the shell to die of SIGPIPE, got {status}, output {output:?}"
    );
    assert!(!output.contains("alive"), "SIGPIPE was ignored: {output:?}");
}

/// The child starts with an empty signal mask, whatever the spawning thread
/// blocks: a shell that sends itself a signal blocked in the parent dies.
// current_thread: the spawn, and so the fork, runs on this thread.
#[tokio::test(flavor = "current_thread")]
async fn test_child_does_not_inherit_blocked_signals() {
    let blocked = BlockedInThisThread::new(libc::SIGUSR1);
    let spawned = CommandBuilder::new("/bin/sh")
        .arg("-c")
        .arg("kill -USR1 $$; echo alive")
        .spawn()
        .await;
    drop(blocked);
    let mut pty = spawned.unwrap();
    let (output, status) = output_and_status(&mut pty).await;

    assert_eq!(
        status.signal(),
        Some(libc::SIGUSR1),
        "expected the shell to die of SIGUSR1, got {status}, output {output:?}"
    );
    assert!(
        !output.contains("alive"),
        "SIGUSR1 stayed blocked: {output:?}"
    );
}

/// Every disposition is reset, not only `SIGPIPE`, and the mask is empty: the
/// program the child runs reports no ignored and no blocked signal.
#[cfg(target_os = "linux")]
// current_thread: the spawn, and so the fork, runs on this thread.
#[tokio::test(flavor = "current_thread")]
async fn test_child_starts_with_no_ignored_or_blocked_signal() {
    // SIGURG's default action is to ignore it, so ignoring it explicitly
    // changes nothing for this process, yet shows in SigIgn if inherited.
    // SAFETY: SIG_IGN installs no handler.
    let previous = unsafe { libc::signal(libc::SIGURG, libc::SIG_IGN) };
    assert_ne!(previous, libc::SIG_ERR, "could not ignore SIGURG");
    let blocked = BlockedInThisThread::new(libc::SIGUSR2);

    let spawned = CommandBuilder::new("cat")
        .arg("/proc/self/status")
        .spawn()
        .await;
    drop(blocked);
    // SAFETY: puts back the disposition read above.
    unsafe { libc::signal(libc::SIGURG, previous) };
    let mut pty = spawned.unwrap();
    let (output, status) = output_and_status(&mut pty).await;

    assert!(status.success(), "cat failed: {status}, output {output:?}");
    for field in ["SigIgn:", "SigBlk:"] {
        let value = output
            .lines()
            .find_map(|line| line.trim().strip_prefix(field))
            .map(str::trim)
            .unwrap_or_else(|| panic!("no {field} line in {output:?}"));
        assert_eq!(
            u64::from_str_radix(value, 16),
            Ok(0),
            "{field} {value} in the child, expected none"
        );
    }
}

/// Verify `ExitStatus` convenience methods.
#[test]
fn test_exit_status_api() {
    let s = crate::ExitStatus::from_code(0);
    assert!(s.success());
    assert_eq!(s.code(), Some(0));
    assert_eq!(s.signal(), None);

    let s = crate::ExitStatus::from_code(1);
    assert!(!s.success());

    let s = crate::ExitStatus::from_signal(9);
    assert_eq!(s.code(), None);
    assert_eq!(s.signal(), Some(9));
    assert!(!s.success());
}

/// Verify `PtySize` default.
#[test]
fn test_pty_size_default() {
    let sz = PtySize::default();
    assert_eq!(sz.cols, 80);
    assert_eq!(sz.rows, 24);
}
