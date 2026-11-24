//! Synchronous child-process helpers for applications that own the terminal.

use std::collections::HashMap;
use std::io;
/// Detach from the controlling TTY by starting a new session. `Stdio::null()` only redirects fd 0; programs like `ssh`,
/// `ssh-add`, and interactive shells (`zsh -i`) bypass it by opening `/dev/tty` directly. SAFETY: Must only be called
/// inside a `pre_exec` hook (between `fork` and `exec`). Both `setsid()` and `setpgid()` are async-signal-safe (POSIX).
#[cfg(unix)]
pub fn detach_from_tty() -> io::Result<()> {
    // SAFETY: setsid and setpgid are async-signal-safe and take no pointers.
    if unsafe { libc::setsid() } != -1 {
        return Ok(());
    }
    let error = io::Error::last_os_error();
    if error.raw_os_error() != Some(libc::EPERM) {
        return Err(error);
    }
    if unsafe { libc::setpgid(0, 0) } == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// No-op on non-Unix platforms.
#[cfg(not(unix))]
pub fn detach_from_tty() -> io::Result<()> {
    Ok(())
}

/// The score is inherited across `fork`, so a protected parent would otherwise shield everything it spawns, leaving a
/// runaway command unkillable. Best-effort: failing the spawn is worse than keeping the inherited score. SAFETY: Safe
/// between `fork` and `exec`: raw `open`/`write`/`close` only, which are async-signal-safe and allocation-free.
#[cfg(target_os = "linux")]
pub fn reset_oom_score_adj() -> io::Result<()> {
    const PATH: &[u8] = b"/proc/self/oom_score_adj\0";
    // SAFETY: `PATH` is a NUL-terminated `'static` literal and `value` is passed
    // with its own length; both pointers stay valid for the calls.
    unsafe {
        let fd = libc::open(PATH.as_ptr().cast(), libc::O_WRONLY | libc::O_CLOEXEC);
        if fd < 0 {
            return Ok(());
        }
        let value = b"0\n";
        libc::write(fd, value.as_ptr().cast(), value.len());
        libc::close(fd);
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
pub fn reset_oom_score_adj() -> io::Result<()> {
    Ok(())
}

/// Set when a protected launcher wants children to retain the normal OOM score.
#[cfg(unix)]
pub const RESET_CHILD_OOM_ENV: &str = "RESET_CHILD_OOM_SCORE_ADJ";

/// Lowering the score needs root/`CAP_SYS_RESOURCE`; the error distinguishes a real misconfiguration (`PermissionDenied`)
/// from an expected non-procfs environment (`NotFound`). The score is inherited across `fork`, so a caller that gets `Ok`
/// MUST arm [`RESET_CHILD_OOM_ENV`] or its whole subtree inherits the protection.
#[cfg(target_os = "linux")]
pub fn protect_from_oom_kill() -> io::Result<()> {
    std::fs::write("/proc/self/oom_score_adj", "-900\n")
}

#[cfg(not(target_os = "linux"))]
pub fn protect_from_oom_kill() -> io::Result<()> {
    Err(io::Error::from(io::ErrorKind::Unsupported))
}

#[cfg(unix)]
pub fn detach_from_tty_reset_oom() -> io::Result<()> {
    reset_oom_score_adj()?;
    detach_from_tty()
}

/// Must be called pre-fork, not from inside the hook: reading the environment
/// is not async-signal-safe.
#[cfg(unix)]
pub fn detach_pre_exec_hook() -> fn() -> io::Result<()> {
    if std::env::var_os(RESET_CHILD_OOM_ENV).is_some() {
        detach_from_tty_reset_oom
    } else {
        detach_from_tty
    }
}

/// A null stdio handle for a child, taken from a descriptor opened once rather than from the `/dev/null` path. Use this
/// instead of [`std::process::Stdio::null`] on every spawn path, for stdin and for discarded stdout/stderr alike. A root
/// `go build -o /dev/null` whose build fails is enough to trigger it: Go removes its `-o` target on failure.
#[cfg(unix)]
pub fn null_stdio() -> std::process::Stdio {
    use std::sync::OnceLock;

    static NULL_FD: OnceLock<Option<std::os::fd::OwnedFd>> = OnceLock::new();

    let cached = NULL_FD
        .get_or_init(|| open_null_fd(std::path::Path::new("/dev/null")).or_else(eof_pipe_fd));
    match cached {
        // dup(2) per spawn: `Stdio` takes ownership, the cache keeps the original.
        Some(fd) => fd
            .try_clone()
            .map_or_else(|_| std::process::Stdio::null(), std::process::Stdio::from),
        None => std::process::Stdio::null(),
    }
}

/// Windows has no `/dev/null` path to lose; `Stdio::null()` is already a handle.
#[cfg(windows)]
pub fn null_stdio() -> std::process::Stdio {
    std::process::Stdio::null()
}

/// Read-only descriptor for `path`, or `None` if it cannot be opened. Split out so the "the descriptor outlives the path"
/// property can be tested against an ordinary file, without a mount namespace or root.
#[cfg(unix)]
fn open_null_fd(path: &std::path::Path) -> Option<std::os::fd::OwnedFd> {
    // Read AND write, because one cached descriptor serves both directions: stdin reads EOF from it, stdout/stderr discard
    // into it. This mirrors `Stdio::null()` itself, which opens `/dev/null` readable for stdin and writable for
    // stdout/stderr. `OpenOptions` sets CLOEXEC, so the cached fd is not inherited wholesale.
    std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .ok()
        .map(Into::into)
}

/// That matters more here than for an ordinary pipe: a concurrent `fork`/`exec` landing between `pipe()` and
/// `fcntl(F_SETFD)` could inherit the write end, and a child holding it open means no reader of the cached read end ever
/// sees EOF — a child given it as stdin would block instead of starting cleanly.
#[cfg(unix)]
fn eof_pipe_fd() -> Option<std::os::fd::OwnedFd> {
    use std::os::fd::{FromRawFd, OwnedFd};

    let mut fds = [0 as libc::c_int; 2];

    // SAFETY: `fds` is a valid, writable two-element array for the call's duration.
    #[cfg(target_os = "linux")]
    let created = unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) };
    // SAFETY: same.
    #[cfg(not(target_os = "linux"))]
    let created = unsafe { libc::pipe(fds.as_mut_ptr()) };
    if created != 0 {
        return None;
    }

    // SAFETY: the pipe call reported success, so both descriptors are open and unowned.
    let [read_fd, write_fd] = fds;
    let (read, write) = unsafe {
        (
            OwnedFd::from_raw_fd(read_fd),
            OwnedFd::from_raw_fd(write_fd),
        )
    };

    // Non-Linux has no atomic form: close the window as fast as possible and
    // accept it, as the sibling helper does.
    #[cfg(not(target_os = "linux"))]
    {
        use std::os::fd::AsRawFd;
        for fd in [read.as_raw_fd(), write.as_raw_fd()] {
            // SAFETY: both descriptors are live and owned here.
            unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) };
        }
    }

    // Closing the write end is what turns reads into an immediate EOF.
    drop(write);
    Some(read)
}

/// Detach the child from the controlling terminal. On Windows, keep stdio
/// inheritance while suppressing a new console window.
pub fn detach_command(cmd: &mut std::process::Command) {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // SAFETY: every hook `detach_pre_exec_hook` can return is async-signal-safe,
        // and the env read that selects one happens here, before fork.
        unsafe {
            cmd.pre_exec(detach_pre_exec_hook());
        }
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
}

/// The `pre_exec` body for [`kill_on_parent_death_std`]: arm `PR_SET_PDEATHSIG` and close the classic pdeathsig race
/// (parent died between `fork` and `prctl`, so the signal will never fire) by comparing `getppid()` against the pid
/// captured at spawn time.
#[cfg(target_os = "linux")]
fn bind_to_parent_death(
    parent_pid: u32,
    armed_thread: std::thread::ThreadId,
    signal: libc::c_int,
) -> io::Result<()> {
    // Post-fork, TLS is a copy of the SPAWNING thread's, so this observes
    // which thread called `spawn()`.
    if cfg!(debug_assertions) && std::thread::current().id() != armed_thread {
        return Err(io::Error::from_raw_os_error(libc::EINVAL));
    }
    // SAFETY: prctl(PR_SET_PDEATHSIG, …) only sets the calling process's
    // parent-death signal; it reads/writes no caller memory.
    if unsafe { libc::prctl(libc::PR_SET_PDEATHSIG, signal as libc::c_ulong) } == -1 {
        return Err(io::Error::last_os_error());
    }
    // Parent already gone (pdeathsig can no longer fire): exit instead of orphaning. A reparented child sees a ppid
    // different from the pid the spawn site captured. SAFETY: getppid/_exit are async-signal-safe and take no pointers.
    if unsafe { libc::getppid() } as u32 != parent_pid {
        unsafe { libc::_exit(0) };
    }
    Ok(())
}

/// Never apply it to processes designed to outlive the client — leader daemons, workspace servers, backgrounded user
/// tasks. Composable with [`detach_command`]: `pre_exec` hooks run in registration order, and `setsid`/`setpgid` do
/// not clear the parent-death signal, so this can be applied before or after a `detach_*` helper.
pub fn kill_on_parent_death_std(cmd: &mut std::process::Command) {
    #[cfg(target_os = "linux")]
    kill_on_parent_death_std_with(cmd, libc::SIGTERM);
    #[cfg(not(target_os = "linux"))]
    {
        let _ = cmd;
    }
}

/// SIGTERM (the default) lets the child run its graceful shutdown; pass `libc::SIGKILL` when the child must die even if
/// it is wedged and cannot service a catchable signal — e.g. the PTY e2e pager children that leaked on CI precisely
/// because they were unresponsive to the master-close SIGHUP. All caveats of [`kill_on_parent_death_std`] apply.
#[cfg(target_os = "linux")]
pub fn kill_on_parent_death_std_with(cmd: &mut std::process::Command, signal: libc::c_int) {
    use std::os::unix::process::CommandExt;
    let parent_pid = std::process::id();
    let armed_thread = std::thread::current().id();
    // SAFETY: bind_to_parent_death calls only async-signal-safe libc
    // functions and builds errors without allocating (see its docs for
    // the debug-only TLS read). Satisfies the pre_exec contract.
    unsafe {
        cmd.pre_exec(move || bind_to_parent_death(parent_pid, armed_thread, signal));
    }
}

/// No-op elsewhere. Unlike the spawn-time helper there is no ppid race check: a direct parent at pid 1 is legitimate here
/// (containers where the client is PID 1), so an already-dead parent is indistinguishable from that case. Always `Ok(())`
/// on non-Linux platforms (no-op). Never from daemons designed to outlive their spawner.
pub fn kill_current_process_on_parent_death() -> io::Result<()> {
    #[cfg(target_os = "linux")]
    {
        // SAFETY: prctl(PR_SET_PDEATHSIG, …) only sets the calling process's
        // parent-death signal; it reads/writes no caller memory.
        if unsafe { libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM as libc::c_ulong) } == -1 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

/// Returns environment variables that prevent CLI tools from launching any
/// interactive program that would block waiting for user input — pagers,
/// editors, credential prompts.
pub fn pager_env() -> HashMap<String, String> {
    HashMap::from([
        ("PAGER".to_string(), passthrough_pager().to_string()),
        ("GIT_PAGER".to_string(), passthrough_pager().to_string()),
        ("GH_PAGER".to_string(), passthrough_pager().to_string()),
        ("MANPAGER".to_string(), passthrough_pager().to_string()),
        ("AWS_PAGER".to_string(), String::new()),
        ("SYSTEMD_PAGER".to_string(), passthrough_pager().to_string()),
        ("GIT_EDITOR".to_string(), noop_cmd().to_string()),
        ("GIT_SEQUENCE_EDITOR".to_string(), noop_cmd().to_string()),
        ("GIT_TERMINAL_PROMPT".to_string(), "0".to_string()),
        // Empty (== unset for gpg) so gpg-agent/pinentry can't grab the TUI's tty; with setsid detach, signing fails cleanly instead of corrupting the screen.
        ("GPG_TTY".to_string(), String::new()),
    ])
}

/// Environment variables set on every git command to suppress interactive prompts.
pub const GIT_AUTH_SUPPRESSION_ENVS: [(&str, &str); 4] = [
    ("GIT_TERMINAL_PROMPT", "0"),
    ("GIT_ASKPASS", ""),
    ("GIT_LFS_SKIP_SMUDGE", "1"),
    ("GIT_SSH_COMMAND", "ssh -o BatchMode=yes"),
];

/// Git command with auth/LFS/SSH prompt suppression and `--no-optional-locks`. Respects `GIT_BIN_PATH` for hermetic git
/// in Bazel test sandboxes. `--no-optional-locks` skips *optional* maintenance locks only. Required locks for the
/// requested operation are still taken. Prefer this for readers (`status`, `cat-file`, `rev-parse`).
pub fn git_command() -> std::process::Command {
    let mut cmd = git_command_base();
    cmd.arg("--no-optional-locks");
    cmd
}

/// Like [`git_command`], but omits `--no-optional-locks`.
/// Writers such as `fetch` may take optional maintenance locks besides required ones.
/// Use this for mutating git that must not skip those locks under concurrent restore.
pub fn git_command_locking() -> std::process::Command {
    git_command_base()
}

fn git_command_base() -> std::process::Command {
    let mut hermetic_exec_path: Option<std::path::PathBuf> = None;
    let git = match std::env::var("GIT_BIN_PATH") {
        Ok(p) => {
            let p = std::path::PathBuf::from(p);
            let p = if p.is_relative() {
                std::env::current_dir().unwrap_or_default().join(&p)
            } else {
                p
            };
            // git-minimal spawns subcommands (`git stash` → `git update-index`) through its exec path, which is baked to a
            // build-machine prefix. Helpers live next to the binary, so point the exec path there. Skip the host-fallback wrapper:
            // host git must keep its own exec path.
            if p.file_name().is_some_and(|name| name == "git") {
                hermetic_exec_path = p.parent().map(std::path::Path::to_path_buf);
            }
            p.to_string_lossy().into_owned()
        }
        Err(_) => "git".to_string(),
    };
    let mut cmd = std::process::Command::new(&git);
    detach_command(&mut cmd);
    cmd.stdin(std::process::Stdio::null());
    cmd.envs(pager_env());
    for &(key, val) in &GIT_AUTH_SUPPRESSION_ENVS {
        cmd.env(key, val);
    }
    if let Some(exec_path) = hermetic_exec_path {
        cmd.env("GIT_EXEC_PATH", exec_path);
    }
    cmd
}

fn passthrough_pager() -> &'static str {
    #[cfg(unix)]
    {
        "cat"
    }
    #[cfg(not(unix))]
    {
        ""
    }
}

fn noop_cmd() -> &'static str {
    #[cfg(unix)]
    {
        "true"
    }
    #[cfg(not(unix))]
    {
        r"C:\Windows\System32\cmd.exe /c exit 0"
    }
}

/// The dup'd stderr fd that writes to the real terminal. Set once by [`redirect_native_stderr`]. Stored as
/// [`OwnedFd`](std::os::unix::io::OwnedFd) for type safety; the [`OnceLock`](std::sync::OnceLock) keeps it alive for the
/// entire process lifetime.
#[cfg(unix)]
static TUI_STDERR_FD: std::sync::OnceLock<std::os::unix::io::OwnedFd> = std::sync::OnceLock::new();

/// Redirect native stderr (fd 2) to `/dev/null` so that messages written directly to fd 2 by C libraries do not
/// interleave with TUI escape sequences. After this call, all intentional terminal output should go through the `File`
/// obtained from [`dup_tui_stderr`].
pub fn redirect_native_stderr() {
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::io::{AsRawFd, FromRawFd, OwnedFd};

        // SAFETY: dup(2) is safe on fd 2 (stderr), which is always valid
        // at process start.
        let duped = unsafe { libc::dup(2) };
        if duped < 0 {
            return; // best-effort; don't crash
        }

        // Store the duped fd (as OwnedFd) before redirecting.
        // SAFETY: `duped` is a freshly dup'd, valid file descriptor.
        let owned = unsafe { OwnedFd::from_raw_fd(duped) };
        let _ = TUI_STDERR_FD.set(owned);

        // Flush any buffered stderr data before redirecting fd 2 to
        // /dev/null, so nothing is silently lost.
        let _ = std::io::stderr().flush();

        // Open /dev/null and dup2 it onto fd 2.
        let devnull = match std::fs::OpenOptions::new().write(true).open("/dev/null") {
            Ok(f) => f,
            Err(_) => return,
        };
        // SAFETY: devnull.as_raw_fd() is valid; fd 2 is a valid target.
        unsafe { libc::dup2(devnull.as_raw_fd(), 2) };
        // devnull File is dropped here; fd 2 now points at /dev/null.
    }
}

/// Create a new owned `File` that writes to the real terminal stderr. Each caller gets their own fd that they can wrap in
/// a `BufWriter`, pass to a thread, etc. Dropping the returned `File` closes only that caller's dup'd copy — the
/// underlying terminal fd is never affected. If [`redirect_native_stderr`] was not called, this dups normal fd 2.
pub fn dup_tui_stderr() -> io::Result<std::fs::File> {
    #[cfg(unix)]
    {
        use std::os::unix::io::{AsRawFd, FromRawFd};
        let source_fd = TUI_STDERR_FD.get().map(|fd| fd.as_raw_fd()).unwrap_or(2);
        // SAFETY: source_fd is either the dup'd stderr (valid for the
        // process lifetime via OnceLock<OwnedFd>) or 2 (normal stderr).
        // dup() returns a new independently-owned fd.
        let new_fd = unsafe { libc::dup(source_fd) };
        if new_fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: new_fd is a freshly dup'd, owned file descriptor.
        Ok(unsafe { std::fs::File::from_raw_fd(new_fd) })
    }
    #[cfg(not(unix))]
    {
        use std::os::windows::io::AsHandle;
        std::io::stderr()
            .as_handle()
            .try_clone_to_owned()
            .map(std::fs::File::from)
    }
}

/// Restore fd 2 to point to the real terminal stderr. Call this before exiting so that any final messages (panic output,
/// tracing flushes) reach the user's terminal rather than `/dev/null`.
pub fn restore_native_stderr() {
    #[cfg(unix)]
    {
        use std::os::unix::io::AsRawFd;
        if let Some(fd) = TUI_STDERR_FD.get() {
            // SAFETY: fd is the dup'd stderr (valid for process lifetime
            // via OnceLock<OwnedFd>). dup2 atomically replaces fd 2 with
            // a copy of fd.
            unsafe { libc::dup2(fd.as_raw_fd(), 2) };
        }
    }
}

/// Returns `true` when running inside Windows Subsystem for Linux (WSL1/WSL2). Cached for process lifetime. Detection:
/// the `WSL_DISTRO_NAME` / `WSL_INTEROP` env vars, with a `/proc/sys/kernel/osrelease` `microsoft`/`wsl` substring
/// fallback that also catches WSL1 and env-stripped shells.
pub fn is_wsl() -> bool {
    static CACHE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *CACHE.get_or_init(detect_is_wsl)
}

fn detect_is_wsl() -> bool {
    if !cfg!(target_os = "linux") {
        return false;
    }
    let env: HashMap<String, String> = std::env::vars().collect();
    let osrelease = std::fs::read_to_string("/proc/sys/kernel/osrelease").ok();
    is_wsl_from_inputs(&env, osrelease.as_deref())
}

/// Pure helper so tests can drive the env and `/proc` contents directly.
fn is_wsl_from_inputs(env: &HashMap<String, String>, osrelease: Option<&str>) -> bool {
    if env.contains_key("WSL_DISTRO_NAME") || env.contains_key("WSL_INTEROP") {
        return true;
    }
    osrelease.is_some_and(|s| {
        let lower = s.to_ascii_lowercase();
        lower.contains("microsoft") || lower.contains("wsl")
    })
}

/// True for a character unsafe to render from untrusted or server-supplied text.
/// Controls can inject terminal escapes, invisible format characters (Unicode Cf) hide or reorder text,
/// and line separators break a row. The prepended concatenation marks are Cf too but render visibly, so they stay.
pub fn is_unsafe_display_char(c: char) -> bool {
    c.is_control()
        || matches!(
            c,
            '\u{00AD}'
            | '\u{061C}'
            | '\u{180E}'
            | '\u{200B}'..='\u{200F}'
            | '\u{2028}'..='\u{202E}'
            | '\u{2060}'..='\u{206F}'
            | '\u{FEFF}'
            | '\u{FFF9}'..='\u{FFFB}'
            | '\u{13430}'..='\u{1343F}'
            | '\u{1BCA0}'..='\u{1BCA3}'
            | '\u{1D173}'..='\u{1D17A}'
            | '\u{E0001}'
            | '\u{E0020}'..='\u{E007F}'
        )
}

use std::process::{Child, ExitStatus};
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::time::{Duration, Instant};

// Forty milliseconds balances responsive exit detection against polling load.
const CHILD_EXIT_POLL_QUANTUM: Duration = Duration::from_millis(40);

/// Poll `child` for up to one monotonic `timeout`, reaping it on exit. `Some` means reaped; `None` leaves the child
/// running and caller-owned. Errors perform no cleanup. Unix `ECHILD` makes numeric PID/process-group identity uncertain,
/// so callers must not signal either. A zero timeout still performs initial and final deadline polls.
pub fn wait_child_bounded(child: &mut Child, timeout: Duration) -> io::Result<Option<ExitStatus>> {
    let started = Instant::now();
    // Not thread::sleep: a foreign SIGCHLD handler can overwrite EINTR and trip its errno assert (GB-5008).
    // Not park_timeout: that would consume this thread's park token.
    let mutex = Mutex::new(());
    let condvar = Condvar::new();
    wait_child_bounded_with(
        timeout,
        || child.try_wait(),
        || started.elapsed(),
        |duration| {
            let guard = mutex.lock().unwrap_or_else(PoisonError::into_inner);
            let _guard = condvar
                .wait_timeout_while(guard, duration, |_| true)
                .unwrap_or_else(PoisonError::into_inner);
        },
    )
}

/// Transfer child/group ownership to a named eventual reaper, or return both. Returns thread-spawn failure together with
/// the untransferred owners.
pub fn spawn_child_reaper<G: Send + Sync + 'static>(
    name: &str,
    child: Child,
    group: Option<Arc<G>>,
) -> Result<(), (io::Error, Child, Option<Arc<G>>)> {
    let owners = Arc::new(Mutex::new(Some((child, group))));
    let thread_owners = Arc::clone(&owners);
    match std::thread::Builder::new()
        .name(name.to_owned())
        .spawn(move || {
            let Some((mut child, group)) = thread_owners
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .take()
            else {
                return;
            };
            let _ = child.wait();
            drop(group);
        }) {
        Ok(_) => Ok(()),
        Err(error) => {
            let owners = owners.lock().unwrap_or_else(PoisonError::into_inner).take();
            match owners {
                Some((child, group)) => Err((error, child, group)),
                #[allow(
                    clippy::unreachable,
                    reason = "a failed reaper spawn cannot also hold ownership taken by the thread"
                )]
                None => unreachable!("failed thread cannot take reaper ownership"),
            }
        }
    }
}

/// Whether a wait error means a numeric Unix child/group identity may be stale.
pub fn is_child_wait_identity_uncertain(error: &io::Error) -> bool {
    #[cfg(unix)]
    {
        error.raw_os_error() == Some(libc::ECHILD)
    }
    #[cfg(not(unix))]
    {
        let _ = error;
        false
    }
}

fn wait_child_bounded_with<P, E, W>(
    timeout: Duration,
    mut poll: P,
    mut elapsed: E,
    mut wait: W,
) -> io::Result<Option<ExitStatus>>
where
    P: FnMut() -> io::Result<Option<ExitStatus>>,
    E: FnMut() -> Duration,
    W: FnMut(Duration),
{
    if let Some(status) = poll()? {
        return Ok(Some(status));
    }

    loop {
        let remaining = timeout.saturating_sub(elapsed());
        if remaining.is_zero() {
            return poll();
        }
        wait(remaining.min(CHILD_EXIT_POLL_QUANTUM));
        if timeout.saturating_sub(elapsed()).is_zero() {
            return poll();
        }
        if let Some(status) = poll()? {
            return Ok(Some(status));
        }
    }
}
