# async-xpty

[![CI](https://github.com/khiops/async-xpty/actions/workflows/ci.yml/badge.svg)](https://github.com/khiops/async-xpty/actions/workflows/ci.yml)
[![crates.io](https://img.shields.io/crates/v/async-xpty.svg)](https://crates.io/crates/async-xpty)
[![docs.rs](https://docs.rs/async-xpty/badge.svg)](https://docs.rs/async-xpty)

Cross-platform async PTY for [tokio](https://tokio.rs).

`async-xpty` provides an ergonomic, async-native interface for spawning processes
inside a pseudo-terminal (PTY). It is built on top of tokio and targets:

- **Linux / macOS** (and other Unix families) via `openpty`
- **Windows** via ConPTY (Windows 10 1809+)

The PTY master is exposed as tokio `AsyncRead` / `AsyncWrite` halves, with async
`resize`, `wait`, `kill`, and a builder for spawning.

## Quick start

```toml
[dependencies]
async-xpty = "0.1"
tokio = { version = "1", features = ["full"] }
```

```rust,no_run
use async_xpty::CommandBuilder;
use tokio::io::AsyncReadExt;

#[tokio::main]
async fn main() -> std::io::Result<()> {
    let mut pty = CommandBuilder::new("/bin/sh")
        .arg("-c")
        .arg("echo hello")
        .size(80, 24)
        .spawn()
        .await?;

    let mut buf = vec![0u8; 1024];
    let n = pty.reader().read(&mut buf).await?;
    println!("{}", String::from_utf8_lossy(&buf[..n]));

    let status = pty.wait().await?;
    println!("exited: {:?}", status.code());
    Ok(())
}
```

## API overview

- [`CommandBuilder`] — configure and `spawn()` a process attached to a PTY.
- [`PtyProcess`] — the running child; `reader()`, `writer()`, `resize()`, `wait()`, `pid()`, `kill()`.
- [`PtyReader`] / [`PtyWriter`] — tokio `AsyncRead` / `AsyncWrite` halves over the PTY master.
- [`PtySize`] — window dimensions (cols × rows).
- [`ExitStatus`] — child exit code or terminating signal.

## Platform notes

- **Resize** sends `SIGWINCH` to the process group on Unix; calls `ResizePseudoConsole` on Windows.
- **Windows** requires the ConPTY API (Windows 10 1809 / build 17763 or newer).

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT license ([LICENSE-MIT](LICENSE-MIT))

at your option.

Unless you explicitly state otherwise, any contribution intentionally submitted
for inclusion in this crate by you, as defined in the Apache-2.0 license, shall
be dual licensed as above, without any additional terms or conditions.

## History

`async-xpty` was originally developed inside the
[termora](https://github.com/khiops/termora) monorepo and extracted to its own
repository (with history) to live as a standalone, independently-versioned library.
