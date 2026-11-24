//! Spawn a child process, optionally feed it stdin, wait up to a wall-clock budget, and reap the whole process group on a breach.
//!
//! Used by both the optional [`super::MmdcEngine`] (which shells out to `mmdc` and headless Chromium) and the pager's out-of-process render child.
//! That child is a short-lived re-exec of the pager that renders one diagram in isolation.
//! The timeout is a *real* process kill, not a soft signal.
//! A panic under `panic = "abort"` or a runaway render in the child is contained because the parent kills and reaps it.
//!
//! The caller builds the [`Command`] (stdio, env, and the TTY/console detach via the terminal host).
//! This module owns spawn, feed stdin, wait, and reap, so neither call site re-implements process-group teardown.

use std::process::{Child, Command};
use std::time::Duration;

use crate::engine::tty::{detach_command, wait_child_bounded};

/// Why a child subprocess run did not complete successfully.
#[derive(thiserror::Error, Debug)]
pub enum SubprocessError {
    /// The child could not be spawned (binary missing, fork failure, ...).
    #[error("could not spawn child process: {0}")]
    Spawn(std::io::Error),
    /// The child exceeded its wall-clock budget and was killed and reaped.
    #[error("child process timed out")]
    Timeout,
    /// The child ran to completion but exited non-zero.
    #[error("child process exited with {0}")]
    NonZeroExit(std::process::ExitStatus),
    /// Waiting on the child itself failed; the child was reaped defensively.
    #[error("waiting on child process failed: {0}")]
    Wait(std::io::Error),
}

/// Spawn `cmd`, optionally write `stdin_payload` to its stdin, wait up to `timeout`, and reap the process group on a breach.
/// The caller supplies the environment and output streams. A payload is sent through a private stdin pipe.
pub fn run_with_timeout(
    mut cmd: Command,
    stdin_payload: Option<&[u8]>,
    timeout: Duration,
) -> Result<(), SubprocessError> {
    if stdin_payload.is_some() {
        cmd.stdin(std::process::Stdio::piped());
    }
    detach_command(&mut cmd);
    let mut child = spawn_with_etxtbsy_retry(&mut cmd).map_err(SubprocessError::Spawn)?;

    // Feed stdin from a scoped thread: a child that stops reading would otherwise wedge a `write_all` of a large payload and deadlock the wait below
    // On timeout we kill the child, so the writer just sees a broken pipe
    let stdin = child.stdin.take();

    std::thread::scope(|scope| {
        if let (Some(mut sink), Some(payload)) = (stdin, stdin_payload) {
            scope.spawn(move || {
                use std::io::Write as _;
                // Errors are expected if the child exits/dies first; ignore them.
                let _ = sink.write_all(payload);
                // Dropping `sink` closes the pipe so the child observes EOF.
            });
        }
        wait_and_reap(&mut child, timeout)
    })
}

/// Spawn `cmd`, retrying briefly on `ETXTBSY` ("Text file busy").
/// The fd is close-on-exec but only closes at that child's own `execve`, so our `execve` of a freshly-written binary can race that window.
#[allow(clippy::disallowed_methods)] // the caller owns the reap
fn spawn_with_etxtbsy_retry(cmd: &mut Command) -> std::io::Result<Child> {
    const MAX_ATTEMPTS: u32 = 5;
    let mut attempt = 0;
    loop {
        match cmd.spawn() {
            Ok(child) => return Ok(child),
            Err(e)
                if e.kind() == std::io::ErrorKind::ExecutableFileBusy
                    && attempt + 1 < MAX_ATTEMPTS =>
            {
                attempt += 1;
                std::thread::sleep(Duration::from_millis(20 * attempt as u64));
            }
            Err(e) => return Err(e),
        }
    }
}

/// Wait for `child` up to `timeout`, tearing down its detached process group on every exit path.
/// Success, non-zero exit, timeout, and wait failure all reap, so a child that spawned grandchildren can't orphan them.
fn wait_and_reap(child: &mut Child, timeout: Duration) -> Result<(), SubprocessError> {
    match wait_child_bounded(child, timeout) {
        // `wait_timeout` already reaped the direct child on these two branches, but it was its own detached group leader
        // For the render child, which has no grandchildren, the leader is already gone and killpg is a harmless no-op (ESRCH)
        // The full `reap()` is unneeded: the direct child is already reaped, so `child.kill()`/`wait()` would be redundant
        Ok(Some(status)) if status.success() => {
            reap_process_group(child);
            Ok(())
        }
        Ok(Some(status)) => {
            reap_process_group(child);
            Err(SubprocessError::NonZeroExit(status))
        }
        Ok(None) => {
            reap(child);
            Err(SubprocessError::Timeout)
        }
        // waitpid failed; the child may still be running, so reap it too, the same teardown as the timeout branch (don't leak the child tree)
        Err(e) if crate::engine::tty::is_child_wait_identity_uncertain(&e) => {
            Err(SubprocessError::Wait(e))
        }
        Err(e) => {
            reap(child);
            Err(SubprocessError::Wait(e))
        }
    }
}

/// Best-effort teardown of a spawned child: SIGKILL its process group (to reach any grandchildren), then kill and reap the child.
fn reap(child: &mut Child) {
    reap_process_group(child);
    let _ = child.kill();
    let _ = child.wait();
}

/// SIGKILL the child's process group so descendants are stopped as well.
#[cfg(unix)]
fn reap_process_group(child: &Child) {
    let pid = child.id() as libc::pid_t;
    // SAFETY: killpg with a valid pid and a standard signal has no memory effects
    unsafe {
        libc::killpg(pid, libc::SIGKILL);
    }
}

#[cfg(not(unix))]
fn reap_process_group(_child: &Child) {
    // Group teardown via Job Objects is tokio-only here; the caller's `child.kill()` still terminates the direct child process
}
