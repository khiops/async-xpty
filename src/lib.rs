//! `async-xpty` — Cross-platform async PTY for tokio.
//!
//! This crate provides an ergonomic, async-native interface for spawning
//! processes inside a pseudo-terminal (PTY). It is built on top of tokio and
//! targets Linux, macOS (Unix family), and Windows (ConPTY, Windows 10 1809+).
//!
//! # Quick start
//!
//! ```no_run
//! use async_xpty::{CommandBuilder, PtySize};
//! use tokio::io::AsyncReadExt;
//!
//! #[tokio::main]
//! async fn main() -> std::io::Result<()> {
//!     let mut pty = CommandBuilder::new("/bin/sh")
//!         .arg("-c")
//!         .arg("echo hello")
//!         .size(80, 24)
//!         .spawn()
//!         .await?;
//!
//!     let mut buf = vec![0u8; 1024];
//!     let n = pty.reader().read(&mut buf).await?;
//!     println!("{}", String::from_utf8_lossy(&buf[..n]));
//!
//!     let status = pty.wait().await?;
//!     println!("exited: {:?}", status.code());
//!     Ok(())
//! }
//! ```

pub mod command;

#[cfg(unix)]
pub mod unix;

#[cfg(windows)]
pub mod windows;

#[cfg(test)]
mod tests;

pub use command::CommandBuilder;

use std::io;

/// Dimensions of a PTY window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PtySize {
    /// Number of columns (width in characters).
    pub cols: u16,
    /// Number of rows (height in characters).
    pub rows: u16,
}

impl Default for PtySize {
    fn default() -> Self {
        Self { cols: 80, rows: 24 }
    }
}

/// The processes that [`PtyProcess::kill_tree`] can reach.
///
/// This enum is exhaustive: each variant describes one of the containment
/// mechanisms used by this crate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KillTreeScope {
    /// The child and its descendants in a Windows job.
    WholeTree,
    /// Members of the child's original Unix process group.
    ///
    /// A descendant that leaves that group, for example with `setsid()`, is
    /// not reached.
    ProcessGroup,
    /// Only the direct child process.
    ///
    /// Windows reports this when job containment was unavailable at spawn.
    DirectProcess,
}

/// The exit status of a PTY child process.
///
/// Exactly one of `code` or `signal` will be `Some` after a normal exit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExitStatus {
    code: Option<i32>,
    signal: Option<i32>,
}

impl ExitStatus {
    /// Create an exit status from an exit code.
    pub fn from_code(code: i32) -> Self {
        Self {
            code: Some(code),
            signal: None,
        }
    }

    /// Create an exit status from a signal number.
    pub fn from_signal(signal: i32) -> Self {
        Self {
            code: None,
            signal: Some(signal),
        }
    }

    /// The exit code, if the process exited normally.
    pub fn code(&self) -> Option<i32> {
        self.code
    }

    /// The signal number that terminated the process, if killed by a signal.
    pub fn signal(&self) -> Option<i32> {
        self.signal
    }

    /// Returns `true` if the process exited successfully (code 0).
    pub fn success(&self) -> bool {
        self.code == Some(0)
    }
}

impl std::fmt::Display for ExitStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if let Some(code) = self.code {
            write!(f, "exit code {}", code)
        } else if let Some(sig) = self.signal {
            write!(f, "signal {}", sig)
        } else {
            write!(f, "unknown exit")
        }
    }
}

/// A running process attached to a PTY master.
///
/// Provides async reader/writer halves and methods to resize, wait for exit,
/// and kill the child process.
///
/// [`Self::kill_tree_scope`] reports whether [`Self::kill_tree`] reaches a
/// Windows job's whole tree, the child's Unix process group, or only a direct
/// Windows child. Dropping this value tears down the whole tree only for
/// [`KillTreeScope::WholeTree`], unless the workload has retained a handle to
/// its own job; it does not signal the other scopes.
///
/// On Unix, [`Self::kill`] and [`Self::kill_tree`] reject signalling after this
/// instance's [`Self::wait`] has observed exit, including `ECHILD`. This catches
/// the local wait-then-signal mistake; it is not a safety mechanism against PID
/// reuse. Callers that reap children by other means own that hazard, just as
/// they would when calling `libc::kill` themselves. On Windows,
/// [`Self::kill_tree`] remains valid after `wait()` when its scope is
/// [`KillTreeScope::WholeTree`], because it targets an owned job handle, but
/// [`Self::kill`] on an exited process fails.
///
/// Obtained by calling [`CommandBuilder::spawn`].
pub struct PtyProcess {
    #[cfg(unix)]
    inner: unix::UnixPtyProcess,
    #[cfg(windows)]
    inner: windows::WinPtyProcess,
}

impl PtyProcess {
    /// Returns an [`AsyncRead`](tokio::io::AsyncRead) half that reads from the
    /// PTY master.
    ///
    /// Multiple calls return independent reader handles backed by the same fd.
    pub fn reader(&self) -> PtyReader {
        PtyReader {
            inner: self.inner.reader(),
        }
    }

    /// Returns an [`AsyncWrite`](tokio::io::AsyncWrite) half that writes to
    /// the PTY master (i.e. the child's stdin).
    pub fn writer(&self) -> PtyWriter {
        PtyWriter {
            inner: self.inner.writer(),
        }
    }

    /// Resize the PTY window. Sends `SIGWINCH` to the process group on Unix so
    /// the running program can adapt its layout. On Windows, calls
    /// `ResizePseudoConsole`.
    pub async fn resize(&self, size: PtySize) -> io::Result<()> {
        self.inner.resize(size).await
    }

    /// Wait for the child process to exit and return its [`ExitStatus`].
    ///
    /// This consumes the mutable reference and should be called at most once.
    pub async fn wait(&mut self) -> io::Result<ExitStatus> {
        self.inner.wait().await
    }

    /// Returns the OS process ID of the child.
    pub fn pid(&self) -> u32 {
        self.inner.pid()
    }

    /// Returns the processes [`Self::kill_tree`] can reach.
    pub fn kill_tree_scope(&self) -> KillTreeScope {
        self.inner.kill_tree_scope()
    }

    /// Send `SIGKILL` to the child process on Unix, or `TerminateProcess` on
    /// Windows.
    ///
    /// On Windows, this targets an owned process handle. It fails for a
    /// process that has already exited, including after [`Self::wait`].
    ///
    /// Prefer [`wait`](Self::wait) after writing an EOF or shell exit command
    /// for a graceful shutdown.
    pub fn kill(&self) -> io::Result<()> {
        self.inner.kill()
    }

    /// Forcefully end the terminal workload.
    ///
    /// The reach is reported by [`Self::kill_tree_scope`]. A successful Unix
    /// call only means a signal was delivered, not that every process in the
    /// group is gone. On Windows, the whole-tree scope remains valid after
    /// [`Self::wait`] because it targets an owned job handle.
    pub fn kill_tree(&self) -> io::Result<()> {
        self.inner.kill_tree()
    }
}

/// Async reader for the PTY master fd.
///
/// Implements [`tokio::io::AsyncRead`].
pub struct PtyReader {
    #[cfg(unix)]
    inner: unix::UnixPtyReader,
    #[cfg(windows)]
    inner: windows::WinPtyReader,
}

impl tokio::io::AsyncRead for PtyReader {
    fn poll_read(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<io::Result<()>> {
        #[cfg(unix)]
        {
            let inner = unsafe { self.map_unchecked_mut(|s| &mut s.inner) };
            inner.poll_read(cx, buf)
        }

        #[cfg(windows)]
        {
            let inner = unsafe { self.map_unchecked_mut(|s| &mut s.inner) };
            inner.poll_read(cx, buf)
        }
    }
}

/// Async writer for the PTY master fd (child stdin).
///
/// Implements [`tokio::io::AsyncWrite`].
pub struct PtyWriter {
    #[cfg(unix)]
    inner: unix::UnixPtyWriter,
    #[cfg(windows)]
    inner: windows::WinPtyWriter,
}

impl tokio::io::AsyncWrite for PtyWriter {
    fn poll_write(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<io::Result<usize>> {
        #[cfg(unix)]
        {
            let inner = unsafe { self.map_unchecked_mut(|s| &mut s.inner) };
            inner.poll_write(cx, buf)
        }

        #[cfg(windows)]
        {
            let inner = unsafe { self.map_unchecked_mut(|s| &mut s.inner) };
            inner.poll_write(cx, buf)
        }
    }

    fn poll_flush(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<io::Result<()>> {
        // PTY master is not buffered at this layer; flush is a no-op.
        std::task::Poll::Ready(Ok(()))
    }

    fn poll_shutdown(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<io::Result<()>> {
        std::task::Poll::Ready(Ok(()))
    }
}
