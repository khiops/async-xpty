#[cfg(unix)]
mod unix {
    use std::io;
    use std::time::{Duration, Instant};

    use async_xpty::{CommandBuilder, KillTreeScope, PtyProcess};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// Ensures a spawned fixture is signalled even when its test returns early
    /// or panics. Explicit cleanup is still performed so failures are reported.
    struct ProcessCleanup {
        process: Option<PtyProcess>,
    }

    impl ProcessCleanup {
        fn new(process: PtyProcess) -> Self {
            Self {
                process: Some(process),
            }
        }

        fn process(&self) -> &PtyProcess {
            self.process
                .as_ref()
                .expect("process cleanup guard must own the fixture")
        }

        fn process_mut(&mut self) -> &mut PtyProcess {
            self.process
                .as_mut()
                .expect("process cleanup guard must own the fixture")
        }

        fn disarm(&mut self) {
            self.process.take();
        }

        fn cleanup(&mut self) -> io::Result<()> {
            let Some(process) = self.process.as_ref() else {
                return Ok(());
            };
            process.kill_tree()?;
            self.process.take();
            Ok(())
        }
    }

    impl Drop for ProcessCleanup {
        fn drop(&mut self) {
            let _ = self.cleanup();
        }
    }

    fn combine_cleanup(
        result: io::Result<()>,
        cleanup: io::Result<()>,
        cleanup_name: &str,
    ) -> io::Result<()> {
        match (result, cleanup) {
            (Ok(()), Ok(())) => Ok(()),
            (Ok(()), Err(cleanup_error)) => Err(cleanup_error),
            (Err(error), Ok(())) => Err(error),
            (Err(error), Err(cleanup_error)) => Err(io::Error::new(
                error.kind(),
                format!("{error}; {cleanup_name} failed: {cleanup_error}"),
            )),
        }
    }

    async fn read_reported_pid(process: &PtyProcess, marker: &str) -> io::Result<i32> {
        let mut reader = process.reader();
        let mut output = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut buf = [0_u8; 512];

        loop {
            if let Some(pid) = parse_pid(&output, marker) {
                return Ok(pid);
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!("PTY fixture did not report {marker:?}: {output:?}"),
                ));
            }
            let read = tokio::time::timeout(remaining, reader.read(&mut buf))
                .await
                .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "PTY read timed out"))??;
            if read == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    format!("PTY closed before reporting {marker:?}: {output:?}"),
                ));
            }
            output.extend_from_slice(&buf[..read]);
        }
    }

    fn parse_pid(output: &[u8], marker: &str) -> Option<i32> {
        let marker = marker.as_bytes();
        output
            .windows(marker.len())
            .enumerate()
            .filter(|(_, candidate)| *candidate == marker)
            .find_map(|(offset, _)| {
                let start = offset + marker.len();
                let end = start
                    + output[start..]
                        .iter()
                        .position(|byte| !byte.is_ascii_digit())?;
                (end > start)
                    .then(|| std::str::from_utf8(&output[start..end]).ok()?.parse().ok())
                    .flatten()
            })
    }

    #[test]
    fn parse_pid_requires_a_delimiter() {
        assert_eq!(parse_pid(b"PID=123", "PID="), None);
        assert_eq!(parse_pid(b"PID=123\r\n", "PID="), Some(123));
    }

    fn pid_is_alive(pid: i32) -> io::Result<bool> {
        // SAFETY: `kill(pid, 0)` performs no signal delivery; it only checks
        // whether the OS still has a process with this PID.
        let rc = unsafe { libc::kill(pid, 0) };
        if rc == 0 {
            return Ok(true);
        }
        let error = io::Error::last_os_error();
        match error.raw_os_error() {
            Some(libc::ESRCH) => Ok(false),
            Some(libc::EPERM) => Ok(true),
            _ => Err(error),
        }
    }

    async fn wait_until_gone(pid: i32) -> io::Result<()> {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            reap_if_adopted(pid)?;
            if !pid_is_alive(pid)? {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!("PID {pid} is still alive after kill_tree()"),
                ));
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    #[cfg(target_os = "linux")]
    fn enable_subreaper() -> io::Result<()> {
        // Reap the killed background child if the shell dies first. This keeps
        // the liveness assertion honest in containers whose PID 1 does not
        // promptly reap orphaned zombies.
        // SAFETY: PR_SET_CHILD_SUBREAPER affects only this test process and its
        // descendants; all varargs after the value are zero as required.
        let rc = unsafe { libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1, 0, 0, 0) };
        if rc == -1 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }

    #[cfg(not(target_os = "linux"))]
    fn enable_subreaper() -> io::Result<()> {
        Ok(())
    }

    #[cfg(target_os = "linux")]
    fn reap_if_adopted(pid: i32) -> io::Result<()> {
        let mut status = 0;
        // SAFETY: `pid` is the fixture process PID. WNOHANG avoids blocking if
        // it has not yet been reparented to this test process.
        let rc = unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) };
        if rc == -1 {
            let error = io::Error::last_os_error();
            if error.raw_os_error() != Some(libc::ECHILD) {
                return Err(error);
            }
        }
        Ok(())
    }

    #[cfg(not(target_os = "linux"))]
    fn reap_if_adopted(_pid: i32) -> io::Result<()> {
        Ok(())
    }

    // Only the Linux-gated setsid escapee test uses these, so on other Unix
    // targets they would be dead code under `-D warnings`.
    #[cfg(target_os = "linux")]
    fn kill_for_cleanup(pid: i32) -> io::Result<()> {
        // SAFETY: `pid` was reported by the test fixture after a complete,
        // delimiter-terminated PID record and is only used to clean its
        // deliberate setsid escapee. This cleanup holds only a numeric PID, so
        // reuse between the liveness check and kill could target another
        // process; fixtures live for 30 seconds and cleanup is immediate.
        let rc = unsafe { libc::kill(pid, libc::SIGKILL) };
        if rc == 0 {
            return Ok(());
        }
        let error = io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::ESRCH) {
            Ok(())
        } else {
            Err(error)
        }
    }

    #[cfg(target_os = "linux")]
    #[derive(Default)]
    struct EscapedPidCleanup {
        pid: Option<i32>,
    }

    #[cfg(target_os = "linux")]
    impl EscapedPidCleanup {
        fn arm(&mut self, pid: i32) {
            self.pid = Some(pid);
        }

        async fn cleanup(&mut self) -> io::Result<()> {
            let Some(pid) = self.pid else {
                return Ok(());
            };
            kill_for_cleanup(pid)?;
            wait_until_gone(pid).await?;
            self.pid = None;
            Ok(())
        }
    }

    #[cfg(target_os = "linux")]
    impl Drop for EscapedPidCleanup {
        fn drop(&mut self) {
            if let Some(pid) = self.pid {
                let _ = kill_for_cleanup(pid);
            }
        }
    }

    #[tokio::test]
    async fn kill_tree_kills_a_child_in_the_shell_process_group() -> io::Result<()> {
        enable_subreaper()?;
        let process = CommandBuilder::new("/bin/sh").arg("-s").spawn().await?;
        let mut cleanup = ProcessCleanup::new(process);
        let result = async {
            assert_eq!(
                cleanup.process().kill_tree_scope(),
                KillTreeScope::ProcessGroup
            );
            let mut writer = cleanup.process().writer();
            writer
                .write_all(b"trap '' HUP\rset +m\rsleep 30 &\recho PID=$!\r")
                .await?;
            writer.flush().await?;
            drop(writer);
            tokio::time::sleep(Duration::from_millis(100)).await;
            let child_pid = read_reported_pid(cleanup.process(), "PID=").await?;
            // SAFETY: `child_pid` is the complete PID reported by the live
            // fixture, and getpgid only observes its process group.
            let child_group = unsafe { libc::getpgid(child_pid) };
            assert_eq!(
                child_group,
                cleanup.process().pid() as i32,
                "fixture must share the shell group"
            );
            cleanup.process().kill_tree()?;
            cleanup.process_mut().wait().await?;
            wait_until_gone(child_pid).await?;
            cleanup.disarm();
            Ok(())
        }
        .await;
        let cleanup_result = cleanup.cleanup();
        combine_cleanup(result, cleanup_result, "process cleanup")
    }

    // The documented process-group escape bound applies on every Unix target;
    // only Linux asserts it because a default macOS install has no `setsid` binary.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn kill_tree_does_not_kill_a_child_that_calls_setsid() -> io::Result<()> {
        enable_subreaper()?;
        let process = CommandBuilder::new("/bin/sh").arg("-s").spawn().await?;
        let mut process_cleanup = ProcessCleanup::new(process);
        let mut escaped_cleanup = EscapedPidCleanup::default();
        let result = async {
            let mut writer = process_cleanup.process().writer();
            writer
                .write_all(b"set +m\rsetsid sh -c 'echo ESCAPED_PID=$$; exec sleep 30' &\r")
                .await?;
            writer.flush().await?;
            drop(writer);
            tokio::time::sleep(Duration::from_millis(100)).await;
            let escaped_pid = read_reported_pid(process_cleanup.process(), "ESCAPED_PID=").await?;
            escaped_cleanup.arm(escaped_pid);

            process_cleanup.process().kill_tree()?;
            process_cleanup.process_mut().wait().await?;
            if !pid_is_alive(escaped_pid)? {
                return Err(io::Error::other(
                    "setsid fixture was killed instead of escaping the shell process group",
                ));
            }
            escaped_cleanup.cleanup().await?;
            process_cleanup.disarm();
            Ok(())
        }
        .await;
        let result = combine_cleanup(result, process_cleanup.cleanup(), "process cleanup");
        combine_cleanup(
            result,
            escaped_cleanup.cleanup().await,
            "escaped PID cleanup",
        )
    }

    #[tokio::test]
    async fn kill_tree_after_wait_is_rejected() -> io::Result<()> {
        let mut process = CommandBuilder::new("/bin/sh")
            .arg("-c")
            .arg("exit 0")
            .spawn()
            .await?;
        process.wait().await?;

        let error = process
            .kill_tree()
            .expect_err("kill_tree after wait must fail");
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        Ok(())
    }
}

#[cfg(windows)]
mod windows {
    use std::fs::OpenOptions;
    use std::io;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{Duration, Instant};

    use async_xpty::{CommandBuilder, PtyProcess};
    use tokio::io::AsyncWriteExt;
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, STILL_ACTIVE};
    use windows_sys::Win32::System::Threading::{
        GetExitCodeProcess, OpenProcess, TerminateProcess, PROCESS_QUERY_LIMITED_INFORMATION,
        PROCESS_TERMINATE,
    };

    const GRANDCHILD_PID_FILE_ENV: &str = "ASYNC_XPTY_GRANDCHILD_PID_FILE";

    /// Ensures a spawned fixture is terminated even when its test returns early
    /// or panics. Explicit cleanup is still performed so failures are reported.
    struct ProcessCleanup {
        process: Option<PtyProcess>,
    }

    impl ProcessCleanup {
        fn new(process: PtyProcess) -> Self {
            Self {
                process: Some(process),
            }
        }

        fn process(&self) -> &PtyProcess {
            self.process
                .as_ref()
                .expect("process cleanup guard must own the fixture")
        }

        fn process_mut(&mut self) -> &mut PtyProcess {
            self.process
                .as_mut()
                .expect("process cleanup guard must own the fixture")
        }

        fn disarm(&mut self) {
            self.process.take();
        }

        fn cleanup(&mut self) -> io::Result<()> {
            let Some(process) = self.process.as_ref() else {
                return Ok(());
            };
            process.kill_tree()?;
            self.process.take();
            Ok(())
        }
    }

    impl Drop for ProcessCleanup {
        fn drop(&mut self) {
            let _ = self.cleanup();
        }
    }

    fn combine_cleanup(
        result: io::Result<()>,
        cleanup: io::Result<()>,
        cleanup_name: &str,
    ) -> io::Result<()> {
        match (result, cleanup) {
            (Ok(()), Ok(())) => Ok(()),
            (Ok(()), Err(cleanup_error)) => Err(cleanup_error),
            (Err(error), Ok(())) => Err(error),
            (Err(error), Err(cleanup_error)) => Err(io::Error::new(
                error.kind(),
                format!("{error}; {cleanup_name} failed: {cleanup_error}"),
            )),
        }
    }

    fn grandchild_pid_file() -> PathBuf {
        static NEXT_PID_FILE: AtomicU64 = AtomicU64::new(0);

        loop {
            let unique = NEXT_PID_FILE.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "async-xpty-kill-tree-{}-{unique}.pid",
                std::process::id()
            ));
            match OpenOptions::new().write(true).create_new(true).open(&path) {
                Ok(file) => {
                    drop(file);
                    return path;
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => panic!("could not reserve PID file {path:?}: {error}"),
            }
        }
    }

    async fn read_reported_pid(path: &Path) -> io::Result<u32> {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            match std::fs::read_to_string(path) {
                Ok(contents) => match contents.trim().parse() {
                    Ok(pid) => return Ok(pid),
                    Err(_) if Instant::now() < deadline => {}
                    Err(error) => {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            format!("invalid grandchild PID in {path:?}: {error}"),
                        ));
                    }
                },
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
            if Instant::now() >= deadline {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!("cmd fixture did not write a grandchild PID to {path:?}"),
                ));
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    fn open_fixture_process(pid: u32) -> io::Result<HANDLE> {
        // SAFETY: this opens the reported fixture process once, retaining its
        // handle to keep the fixture identity stable for all later operations.
        let handle = unsafe {
            OpenProcess(
                PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_TERMINATE,
                0,
                pid,
            )
        };
        if handle.is_null() {
            let error = io::Error::last_os_error();
            return Err(error);
        }
        Ok(handle)
    }

    fn process_is_alive(handle: HANDLE) -> io::Result<bool> {
        let mut exit_code = 0;
        // SAFETY: `handle` is a valid process handle and `exit_code` is writable
        // for the duration of the query.
        let ok = unsafe { GetExitCodeProcess(handle, &mut exit_code) };
        if ok == 0 {
            Err(io::Error::last_os_error())
        } else {
            // STILL_ACTIVE is also a valid exit code (259). The fixture is a
            // Start-Sleep that never exits with 259; arbitrary processes would
            // need a wait-based liveness check.
            Ok(exit_code == STILL_ACTIVE as u32)
        }
    }

    async fn wait_until_gone(pid: u32, handle: HANDLE) -> io::Result<()> {
        let deadline = Instant::now() + Duration::from_secs(5);
        while process_is_alive(handle)? {
            if Instant::now() >= deadline {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!("grandchild PID {pid} survived kill_tree()"),
                ));
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        Ok(())
    }

    struct GrandchildCleanup {
        pid: Option<u32>,
        process_handle: Option<HANDLE>,
        pid_file: PathBuf,
    }

    impl GrandchildCleanup {
        fn new(pid_file: PathBuf) -> Self {
            Self {
                pid: None,
                process_handle: None,
                pid_file,
            }
        }

        fn arm(&mut self, pid: u32) -> io::Result<()> {
            let process_handle = open_fixture_process(pid)?;
            self.pid = Some(pid);
            self.process_handle = Some(process_handle);
            Ok(())
        }

        fn is_alive(&self) -> io::Result<bool> {
            let handle = self
                .process_handle
                .expect("grandchild cleanup guard must own the fixture handle");
            process_is_alive(handle)
        }

        fn process_handle(&self) -> HANDLE {
            self.process_handle
                .expect("grandchild cleanup guard must own the fixture handle")
        }

        fn cleanup(&mut self) -> io::Result<()> {
            let remove_result = match std::fs::remove_file(&self.pid_file) {
                Ok(()) => Ok(()),
                Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
                Err(error) => Err(error),
            };
            if let (Some(_pid), Some(handle)) = (self.pid, self.process_handle) {
                if process_is_alive(handle)? {
                    // SAFETY: `handle` is the retained handle for this fixture
                    // process and was opened with PROCESS_TERMINATE access.
                    let ok = unsafe { TerminateProcess(handle, 1) };
                    if ok == 0 {
                        return Err(io::Error::last_os_error());
                    }
                }
                // SAFETY: `handle` is the valid retained fixture handle and
                // is no longer needed after termination or confirmed exit.
                if unsafe { CloseHandle(handle) } == 0 {
                    return Err(io::Error::last_os_error());
                }
                self.process_handle = None;
                self.pid = None;
            }
            remove_result
        }
    }

    impl Drop for GrandchildCleanup {
        fn drop(&mut self) {
            let _ = self.cleanup();
        }
    }

    #[tokio::test]
    async fn kill_tree_terminates_a_cmd_started_grandchild() -> io::Result<()> {
        let pid_file = grandchild_pid_file();
        let process = CommandBuilder::new("cmd.exe")
            .arg("/Q")
            .arg("/K")
            .env(GRANDCHILD_PID_FILE_ENV, pid_file.to_string_lossy())
            .spawn()
            .await?;
        let mut cleanup = ProcessCleanup::new(process);
        let mut grandchild_cleanup = GrandchildCleanup::new(pid_file);
        let result = async {
            let mut writer = cleanup.process().writer();
            // Omitting `/b` makes `start` create a separate console. The
            // PowerShell child is therefore not a pseudoconsole client, but it
            // still inherits the cmd process's job.
            writer
                .write_all(
                    b"start \"\" powershell.exe -NoProfile -WindowStyle Hidden -Command \"$PID | Set-Content -NoNewline -Encoding ascii $env:ASYNC_XPTY_GRANDCHILD_PID_FILE; Start-Sleep -Seconds 30\"\r\n",
                )
                .await?;
            writer.flush().await?;
            drop(writer);
            let grandchild_pid = read_reported_pid(&grandchild_cleanup.pid_file).await?;
            grandchild_cleanup.arm(grandchild_pid)?;
            if !grandchild_cleanup.is_alive()? {
                return Err(io::Error::other(
                    "grandchild must be alive immediately before kill_tree()",
                ));
            }

            cleanup.process().kill_tree()?;
            cleanup.process_mut().wait().await?;
            wait_until_gone(grandchild_pid, grandchild_cleanup.process_handle()).await?;
            cleanup.disarm();
            Ok(())
        }
        .await;
        let cleanup_result = combine_cleanup(
            cleanup.cleanup(),
            grandchild_cleanup.cleanup(),
            "grandchild cleanup",
        );
        combine_cleanup(result, cleanup_result, "process cleanup")
    }

    #[tokio::test]
    async fn kill_tree_after_wait_terminates_a_cmd_started_grandchild() -> io::Result<()> {
        let pid_file = grandchild_pid_file();
        let process = CommandBuilder::new("cmd.exe")
            .arg("/Q")
            .arg("/K")
            .env(GRANDCHILD_PID_FILE_ENV, pid_file.to_string_lossy())
            .spawn()
            .await?;
        let mut cleanup = ProcessCleanup::new(process);
        let mut grandchild_cleanup = GrandchildCleanup::new(pid_file);
        let result = async {
            let mut writer = cleanup.process().writer();
            // As above, this child owns a separate console rather than the
            // pseudoconsole, while inheriting cmd's job membership.
            writer
                .write_all(
                    b"start \"\" powershell.exe -NoProfile -WindowStyle Hidden -Command \"$PID | Set-Content -NoNewline -Encoding ascii $env:ASYNC_XPTY_GRANDCHILD_PID_FILE; Start-Sleep -Seconds 30\"\r\nexit\r\n",
                )
                .await?;
            writer.flush().await?;
            drop(writer);
            let grandchild_pid = read_reported_pid(&grandchild_cleanup.pid_file).await?;
            grandchild_cleanup.arm(grandchild_pid)?;

            cleanup.process_mut().wait().await?;
            if !grandchild_cleanup.is_alive()? {
                return Err(io::Error::other(
                    "grandchild must outlive pseudoconsole closure before kill_tree()",
                ));
            }
            cleanup.process().kill_tree()?;
            wait_until_gone(grandchild_pid, grandchild_cleanup.process_handle()).await?;
            cleanup.disarm();
            Ok(())
        }
        .await;
        let cleanup_result = combine_cleanup(
            cleanup.cleanup(),
            grandchild_cleanup.cleanup(),
            "grandchild cleanup",
        );
        combine_cleanup(result, cleanup_result, "process cleanup")
    }
}
