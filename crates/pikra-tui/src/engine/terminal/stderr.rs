//! Serialized writes to the terminal's stderr stream.

use std::io;
use std::sync::{Condvar, Mutex, PoisonError};
#[cfg(unix)]
use std::time::Duration;

static BUSY: Mutex<bool> = Mutex::new(false);
static AVAILABLE: Condvar = Condvar::new();

struct WriterGuard;

impl Drop for WriterGuard {
    fn drop(&mut self) {
        *BUSY.lock().unwrap_or_else(PoisonError::into_inner) = false;
        AVAILABLE.notify_one();
    }
}

pub(crate) fn with_locked_stderr<T>(
    write: impl FnOnce(&mut std::fs::File) -> io::Result<T>,
) -> io::Result<T> {
    let busy = BUSY.lock().unwrap_or_else(PoisonError::into_inner);
    let mut busy = AVAILABLE
        .wait_while(busy, |busy| *busy)
        .unwrap_or_else(PoisonError::into_inner);
    *busy = true;
    let _guard = WriterGuard;
    drop(busy);
    write(&mut crate::engine::tty::dup_tui_stderr()?)
}

#[cfg(unix)]
pub(super) fn try_with_locked_stderr_for<T>(
    timeout: Duration,
    write: impl FnOnce(&mut std::fs::File) -> T,
) -> Option<T> {
    let busy = BUSY.lock().unwrap_or_else(PoisonError::into_inner);
    let (mut busy, _) = AVAILABLE
        .wait_timeout_while(busy, timeout, |busy| *busy)
        .unwrap_or_else(PoisonError::into_inner);
    if *busy {
        return None;
    }
    *busy = true;
    let _guard = WriterGuard;
    drop(busy);
    Some(write(&mut crate::engine::tty::dup_tui_stderr().ok()?))
}
